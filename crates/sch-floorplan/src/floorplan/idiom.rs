//! Idiom detection — recognize circuit idioms (crystal+load-caps, decoupling
//! banks, I2C pull-ups, op-amp feedback) purely from connectivity + pin
//! geometry. The infer stage turns these into IR (frozen cells + reports);
//! read-only over the placed Items.

use std::collections::{BTreeMap, BTreeSet};

use super::infer::{
    best_decoupling_anchor, place_cc_pulldown, place_crystal, place_decoupling, place_i2c_pullup,
};
use super::*;
use sch_place::item::{Incidence, Item};
use sch_place::netclass::{PinSide, is_ground};

/// A circuit idiom recognized purely from connectivity + symbol pin geometry.
/// `infer_ir` turns it into an [`sch_place::result::IdiomReport`] for the LLM. A FROZEN
/// idiom also seeds its cells into `place` and pins its members; a REPORT-ONLY idiom
/// (`!freeze`) lets the members flow through normal placement and is instead tidied by
/// an mm post-pass in `emit` (e.g. a GPIO LED's resistor snapped below it).
pub(super) struct Idiom {
    pub(super) kind: &'static str,
    pub(super) anchor: usize,
    /// (refdes, assigned cell) for every member. Cells seed placement only when frozen;
    /// the refdes list always feeds the report.
    pub(super) cells: Vec<(String, Cell)>,
    /// Pin the members and seed their cells (true), or just recognize them (false).
    pub(super) freeze: bool,
}

/// Project the placed parts into the pure [`circuit_graph::CircuitGraph`] the idiom
/// matcher consumes. Net kinds come from the rail table, with a name-based ground
/// fallback so a lifted netlist that dropped the power-net marks still classifies
/// `GND`/`VSS` correctly (the crystal load caps return to ground by name).
fn build_circuit_graph(
    items: &[Item],
    rails: &BTreeMap<String, Band>,
) -> circuit_graph::CircuitGraph {
    let nodes: Vec<circuit_graph::Node> = items
        .iter()
        .map(|it| circuit_graph::Node {
            refdes: it.refdes.clone(),
            lib_id: it.part.clone(),
            value: it.value.clone(),
            pins: it
                .pins
                .iter()
                .map(|(num, name, net)| circuit_graph::Pin {
                    number: num.clone(),
                    name: name.clone(),
                    net: net.clone(),
                })
                .collect(),
        })
        .collect();
    let rails = rails.clone();
    circuit_graph::CircuitGraph::new(nodes, move |net| {
        if rails.contains_key(net) {
            if is_ground(net) {
                circuit_graph::NetKind::Ground
            } else {
                circuit_graph::NetKind::Power
            }
        } else if is_ground(net) {
            circuit_graph::NetKind::Ground
        } else {
            circuit_graph::NetKind::Signal
        }
    })
}

/// Recognize circuit idioms with the graph-similarity matcher (`circuit-graph`)
/// and co-place each as a cohesive cluster BEFORE the generic satellite loop. The
/// matcher decides *what* is an idiom (declarative, extensible); the per-kind
/// placement helpers decide *where* the cluster lands using grid/pin geometry the
/// pure crate cannot see. A board with no idiom is untouched. A match whose
/// geometry can't be realized (e.g. an osc pin not on the IC) is silently dropped,
/// so the generic rules place it instead — the engine never reports an idiom it
/// did not actually freeze.
#[allow(clippy::too_many_arguments)]
pub(super) fn detect_idioms(
    items: &[Item],
    inc: &Incidence,
    anchors: &[usize],
    sats: &[usize],
    rails: &BTreeMap<String, Band>,
    pin_meta: &BTreeMap<(usize, String), (PinSide, i32)>,
    anchor_col: &BTreeMap<usize, i32>,
    anchor_row: &BTreeMap<usize, i32>,
) -> Vec<Idiom> {
    let _ = (sats, pin_meta);
    let graph = build_circuit_graph(items, rails);
    // I2C_PULLUP is gated to the multi-sheet refine path so single-sheet reference snapshots stay
    // byte-identical (it would otherwise re-bind pull-up pairs on IC reference sheets).
    let mut lib = circuit_graph::library::active_library();
    if std::env::var_os("MULTISHEET_REFINE").is_some() {
        lib.push(circuit_graph::library::I2C_PULLUP.clone());
    }
    let matches = circuit_graph::find_all(&graph, &lib);
    if std::env::var("IDIOM_AUDIT").is_ok() {
        for m in &matches {
            eprintln!(
                "AUDIT-MATCH {} anchor={} score={:.2} bindings={:?}",
                m.pattern, m.anchor, m.score, m.bindings
            );
        }
    }
    let idx: BTreeMap<&str, usize> = items
        .iter()
        .enumerate()
        .map(|(i, it)| (it.refdes.as_str(), i))
        .collect();
    let get = |rd: &str| idx.get(rd).copied();

    // A repeated PC817 bank is one visual idiom per channel, not a shelf of ICs
    // followed by separate resistor/LED banks.  Recognize the topology (rather
    // than relying on generated refdes prefixes) and reserve a compact,
    // left-to-right channel row:
    //
    //   input -- RIN -- PC817 -- OUT --+-- RPU -- V+
    //                                  +-- LED -- RLED -- V+
    //
    // The passive supply legs occupy the row immediately above their channel.
    // Requiring at least two complete channels keeps this deliberately bounded:
    // a lone optocoupler continues through the generic placement path.
    let mut out = detect_pc817_channel_bank(items, rails, anchor_col, anchor_row);
    let mut claimed: BTreeSet<usize> = out
        .iter()
        .flat_map(|idiom| idiom.cells.iter())
        .filter_map(|(rd, _)| get(rd))
        .collect();
    for m in &matches {
        let Some(ai) = get(&m.anchor) else { continue };
        match m.pattern {
            "crystal" => {
                let (Some(yi), Some(caps)) = (
                    m.bindings
                        .get("crystal")
                        .and_then(|v| v.first())
                        .and_then(|r| get(r)),
                    Some(
                        ["cap_a", "cap_b"]
                            .iter()
                            .filter_map(|k| m.bindings.get(*k))
                            .flatten()
                            .filter_map(|r| get(r))
                            .collect::<Vec<_>>(),
                    ),
                ) else {
                    continue;
                };
                if caps.len() != 2
                    || caps
                        .iter()
                        .chain(std::iter::once(&yi))
                        .any(|c| claimed.contains(c))
                {
                    continue;
                }
                if let Some(cells) =
                    place_crystal(items, inc, anchors, anchor_col, anchor_row, ai, yi, &caps)
                {
                    claimed.insert(yi);
                    claimed.extend(&caps);
                    out.push(Idiom {
                        kind: "crystal",
                        anchor: ai,
                        cells,
                        freeze: true,
                    });
                }
            }
            "decoupling" => {
                let caps: Vec<usize> = m
                    .bindings
                    .get("cap")
                    .into_iter()
                    .flatten()
                    .filter_map(|r| get(r))
                    .filter(|c| !claimed.contains(c))
                    .collect();
                // The graph matcher's anchor is ANY part bridging V+ and GND — often a
                // jumper or power connector that merely touches the rails, not the IC
                // the caps actually bypass. Re-select the real load: the anchor with the
                // most pins on the bank's V+ rail, preferring a true IC over a
                // connector/jumper. Without this the whole bank freezes beside a stray
                // 3-pin part and floats far from the MCU (the #1 "decoupling bank in the
                // far corner" critic defect).
                let ai = best_decoupling_anchor(items, anchors, rails, &caps).unwrap_or(ai);
                if let Some(mut cells) = place_decoupling(
                    items, inc, anchors, rails, anchor_col, anchor_row, ai, &caps, &out,
                ) {
                    // A SMALL decoupling anchor (a 3-4 pin LDO/regulator) connects only through
                    // power rails — weak cohesion, so the SA drifts it off its own FROZEN bank
                    // (the "LDO isolated far from the caps it serves" power-entry defect). Pin it
                    // WITH the bank so the power-conversion block stays together. Multi-pin ⇒
                    // `orient_angle` returns 0 (no rotation). Gated to multi-sheet so single-sheet
                    // reference snapshots stay byte-identical.
                    if std::env::var_os("MULTISHEET_REFINE").is_some()
                        && (3..=4).contains(&items[ai].geom.pins.len())
                        && !claimed.contains(&ai)
                        && let (Some(&acol), Some(&arow)) =
                            (anchor_col.get(&ai), anchor_row.get(&ai))
                    {
                        cells.push((
                            items[ai].refdes.clone(),
                            Cell {
                                col: acol,
                                row: arow,
                                orient: Orient::Right,
                            },
                        ));
                    }
                    claimed.extend(cells.iter().filter_map(|(rd, _)| get(rd)));
                    out.push(Idiom {
                        kind: "decoupling",
                        anchor: ai,
                        cells,
                        freeze: true,
                    });
                }
            }
            "led_indicator" => {
                // Report-only: a GPIO LED taps its IC pin so normal placement seats it
                // well; we just recognize the pair so the mm post-pass (`align_led_chain`)
                // can snap the series resistor directly below the LED, clear of the body,
                // rather than letting it drift to a spare column.
                let Some(ri) = m
                    .bindings
                    .get("res")
                    .and_then(|v| v.first())
                    .and_then(|r| get(r))
                else {
                    continue;
                };
                if claimed.contains(&ai) || claimed.contains(&ri) {
                    continue;
                }
                out.push(Idiom {
                    kind: "led_indicator",
                    anchor: ai,
                    cells: vec![(
                        items[ri].refdes.clone(),
                        Cell {
                            col: 0,
                            row: 0,
                            orient: Orient::Down,
                        },
                    )],
                    freeze: false,
                });
            }
            "cc_pulldown" => {
                // Freeze the two CC resistors as a reserved pair beside the connector.
                let (Some(ra), Some(rb)) = (
                    m.bindings
                        .get("res_a")
                        .and_then(|v| v.first())
                        .and_then(|r| get(r)),
                    m.bindings
                        .get("res_b")
                        .and_then(|v| v.first())
                        .and_then(|r| get(r)),
                ) else {
                    continue;
                };
                if [ai, ra, rb].iter().any(|c| claimed.contains(c)) {
                    continue;
                }
                if let Some(cells) = place_cc_pulldown(
                    anchor_col,
                    anchor_row,
                    ai,
                    &items[ra].refdes,
                    &items[rb].refdes,
                ) {
                    claimed.insert(ra);
                    claimed.insert(rb);
                    out.push(Idiom {
                        kind: "cc_pulldown",
                        anchor: ai,
                        cells,
                        freeze: true,
                    });
                }
            }
            "i2c_pullup" => {
                // Freeze the two I2C pull-ups as a reserved pair beside the IC (tap UP to power).
                let (Some(ra), Some(rb)) = (
                    m.bindings
                        .get("res_a")
                        .and_then(|v| v.first())
                        .and_then(|r| get(r)),
                    m.bindings
                        .get("res_b")
                        .and_then(|v| v.first())
                        .and_then(|r| get(r)),
                ) else {
                    continue;
                };
                if [ai, ra, rb].iter().any(|c| claimed.contains(c)) {
                    continue;
                }
                if let Some(cells) = place_i2c_pullup(
                    anchor_col,
                    anchor_row,
                    ai,
                    &items[ra].refdes,
                    &items[rb].refdes,
                ) {
                    claimed.insert(ra);
                    claimed.insert(rb);
                    out.push(Idiom {
                        kind: "i2c_pullup",
                        anchor: ai,
                        cells,
                        freeze: true,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

#[derive(Debug)]
struct Pc817Channel {
    opto: usize,
    rin: usize,
    rpu: usize,
    rled: usize,
    led: usize,
}

fn detect_pc817_channel_bank(
    items: &[Item],
    rails: &BTreeMap<String, Band>,
    anchor_col: &BTreeMap<usize, i32>,
    anchor_row: &BTreeMap<usize, i32>,
) -> Vec<Idiom> {
    let pin_net = |i: usize, number: &str| {
        items[i]
            .pins
            .iter()
            .find(|(n, _, _)| n == number)
            .and_then(|(_, _, net)| net.as_deref())
    };
    let nets = |i: usize| {
        items[i]
            .pins
            .iter()
            .filter_map(|(_, _, net)| net.as_deref())
            .collect::<Vec<_>>()
    };
    let has_net = |i: usize, wanted: &str| nets(i).contains(&wanted);
    let other_net = |i: usize, known: &str| {
        nets(i).into_iter().find(|net| *net != known)
    };
    let is_positive_rail = |net: &str| rails.contains_key(net) && !is_ground(net);
    let is_resistor = |i: usize| items[i].part == "Device:R";
    let is_led = |i: usize| {
        items[i].part == "Device:LED" || items[i].part.starts_with("Device:LED_")
    };

    let mut channels = Vec::new();
    for (opto, item) in items.iter().enumerate() {
        let compact = item.part.to_ascii_uppercase().replace(['-', '_'], "");
        if !(compact.contains("PC817") || compact.contains("LTV817")) {
            continue;
        }
        let (Some(input), Some(output)) = (pin_net(opto, "1"), pin_net(opto, "4")) else {
            continue;
        };

        let rin = (0..items.len()).find(|&i| {
            i != opto
                && is_resistor(i)
                && has_net(i, input)
                && other_net(i, input).is_some_and(|net| !rails.contains_key(net))
        });
        let rpu = (0..items.len()).find(|&i| {
            is_resistor(i)
                && has_net(i, output)
                && other_net(i, output).is_some_and(is_positive_rail)
        });
        let led = (0..items.len()).find(|&i| is_led(i) && has_net(i, output));
        let Some(led) = led else { continue };
        let Some(led_anode) = other_net(led, output) else {
            continue;
        };
        let rled = (0..items.len()).find(|&i| {
            is_resistor(i)
                && has_net(i, led_anode)
                && other_net(i, led_anode).is_some_and(is_positive_rail)
        });
        let (Some(rin), Some(rpu), Some(rled)) = (rin, rpu, rled) else {
            continue;
        };
        channels.push(Pc817Channel {
            opto,
            rin,
            rpu,
            rled,
            led,
        });
    }

    if channels.len() < 2 {
        return Vec::new();
    }
    channels.sort_by(|a, b| natural_refdes_cmp(&items[a.opto].refdes, &items[b.opto].refdes));
    let base_col = channels
        .iter()
        .filter_map(|c| anchor_col.get(&c.opto))
        .copied()
        .min()
        .unwrap_or(0);
    let base_row = channels
        .iter()
        .filter_map(|c| anchor_row.get(&c.opto))
        .copied()
        .min()
        .unwrap_or(0);

    channels
        .into_iter()
        .enumerate()
        .map(|(index, channel)| {
            let supply_row = base_row + index as i32 * 2;
            let signal_row = supply_row + 1;
            Idiom {
                kind: "pc817_channel",
                anchor: channel.opto,
                cells: vec![
                    (
                        items[channel.rin].refdes.clone(),
                        Cell {
                            col: base_col,
                            row: signal_row,
                            orient: Orient::Right,
                        },
                    ),
                    (
                        items[channel.opto].refdes.clone(),
                        Cell {
                            col: base_col + 1,
                            row: signal_row,
                            orient: Orient::Right,
                        },
                    ),
                    (
                        items[channel.rpu].refdes.clone(),
                        Cell {
                            col: base_col + 2,
                            row: supply_row,
                            orient: Orient::Down,
                        },
                    ),
                    (
                        items[channel.rled].refdes.clone(),
                        Cell {
                            col: base_col + 3,
                            row: supply_row,
                            orient: Orient::Down,
                        },
                    ),
                    (
                        items[channel.led].refdes.clone(),
                        Cell {
                            col: base_col + 3,
                            row: signal_row,
                            // KiCad's LED pin 1 is K and pin 2 is A.  Pointing
                            // pin 1 down puts A beneath RLED and K on OUT.
                            orient: Orient::Up,
                        },
                    ),
                ],
                freeze: true,
            }
        })
        .collect()
}

fn natural_refdes_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn split(s: &str) -> (&str, u32) {
        let cut = s.find(|c: char| c.is_ascii_digit()).unwrap_or(s.len());
        (&s[..cut], s[cut..].parse::<u32>().unwrap_or(u32::MAX))
    }
    split(a).cmp(&split(b)).then_with(|| a.cmp(b))
}
