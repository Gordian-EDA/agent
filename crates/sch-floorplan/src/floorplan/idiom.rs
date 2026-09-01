//! Idiom detection — recognize circuit idioms (crystal+load-caps, decoupling
//! banks, I2C pull-ups, op-amp feedback) purely from connectivity + pin
//! geometry. The infer stage turns these into IR (frozen cells + reports);
//! read-only over the placed Items.

use std::collections::{BTreeMap, BTreeSet};

use super::infer::{
    best_decoupling_anchor, place_cc_pulldown, place_crystal, place_decoupling, place_i2c_pullup,
};
use super::*;
use circuit_graph::netclass::{is_connector_like, is_ground};
use sch_place::item::PinSide;
use sch_place::item::{Incidence, Item};

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
    let lib = circuit_graph::library::active_library();
    let matches = circuit_graph::find_all(&graph, &lib);
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
                if let Some(cells) = place_decoupling(
                    items, inc, anchors, rails, anchor_col, anchor_row, ai, &caps, &out,
                ) {
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
    let other_net = |i: usize, known: &str| nets(i).into_iter().find(|net| *net != known);
    let is_positive_rail = |net: &str| rails.contains_key(net) && !is_ground(net);
    let is_resistor = |i: usize| items[i].part == "Device:R";
    let is_led =
        |i: usize| items[i].part == "Device:LED" || items[i].part.starts_with("Device:LED_");

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
    channels.sort_by(|a, b| refdes_cmp(&items[a.opto].refdes, &items[b.opto].refdes));
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

    // A repeated channel is much wider than it is tall, so an unbounded vertical
    // strip wastes most of the page once a bank grows past a few channels.  Fold
    // the bank into a near-page-shaped lattice while preserving natural channel
    // order top-to-bottom within each column.  The aspect correction (n / 2)
    // accounts for the roughly 2:1 width:height of the five-part channel cell.
    let bank_cols = ((channels.len() as f64 / 2.0).sqrt().ceil() as usize).max(1);
    let rows_per_bank = channels.len().div_ceil(bank_cols);

    let channel_members: BTreeSet<usize> = channels
        .iter()
        .flat_map(|c| [c.opto, c.rin, c.rpu, c.rled, c.led])
        .collect();
    let input_nets: BTreeSet<&str> = channels
        .iter()
        .filter_map(|c| {
            let opto_input = pin_net(c.opto, "1")?;
            other_net(c.rin, opto_input)
        })
        .collect();
    let output_nets: BTreeSet<&str> = channels
        .iter()
        .filter_map(|c| pin_net(c.opto, "4"))
        .collect();
    let bank_anchor = channels[0].opto;

    let mut idioms: Vec<Idiom> = channels
        .into_iter()
        .enumerate()
        .map(|(index, channel)| {
            let bank_col = index / rows_per_bank;
            let bank_row = index % rows_per_bank;
            // Five occupied channel columns plus one empty gutter keep adjacent
            // banks visually distinct and leave a routing lane between them.
            let channel_col = base_col + bank_col as i32 * 6;
            let supply_row = base_row + bank_row as i32 * 2;
            let signal_row = supply_row + 1;
            Idiom {
                kind: "pc817_channel",
                anchor: channel.opto,
                cells: vec![
                    (
                        items[channel.rin].refdes.clone(),
                        Cell {
                            col: channel_col,
                            row: signal_row,
                            orient: Orient::Right,
                        },
                    ),
                    (
                        items[channel.opto].refdes.clone(),
                        Cell {
                            col: channel_col + 1,
                            row: signal_row,
                            orient: Orient::Right,
                        },
                    ),
                    (
                        items[channel.rpu].refdes.clone(),
                        Cell {
                            col: channel_col + 2,
                            row: supply_row,
                            orient: Orient::Down,
                        },
                    ),
                    (
                        items[channel.rled].refdes.clone(),
                        Cell {
                            col: channel_col + 3,
                            row: supply_row,
                            orient: Orient::Down,
                        },
                    ),
                    (
                        items[channel.led].refdes.clone(),
                        Cell {
                            // Keep the LED one track to the right of its
                            // vertical supply resistor. A shared column leaves
                            // KiCad's generated rail value directly on top of
                            // the next channel's LED body.
                            col: channel_col + 4,
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
        .collect();

    // Gather the shared connectors and rail-only support parts into the open
    // flank immediately beside the bank.  Without this bounded support island,
    // generic anchor shelving and spare-column placement strand the connectors,
    // bypass parts, and mechanical symbols at unrelated page edges.  Detection
    // remains topology-based: the signal connectors must carry only bank input
    // or output nets, while support parts must carry only rails (or no pins).
    let connector_on = |wanted: &BTreeSet<&str>, i: usize| {
        is_connector_like(&items[i].part)
            && !nets(i).is_empty()
            && nets(i).iter().all(|net| wanted.contains(net))
    };
    let input_connector = (0..items.len()).find(|&i| connector_on(&input_nets, i));
    let output_connector = (0..items.len()).find(|&i| connector_on(&output_nets, i));
    let max_channel_col = base_col + (bank_cols.saturating_sub(1) as i32) * 6 + 4;
    let mid_signal_row = base_row + (rows_per_bank.saturating_sub(1) as i32);
    let mut support_cells = Vec::new();
    if let Some(i) = input_connector {
        support_cells.push((
            items[i].refdes.clone(),
            Cell {
                col: base_col - 2,
                row: mid_signal_row,
                orient: Orient::Right,
            },
        ));
    }
    if let Some(i) = output_connector {
        support_cells.push((
            items[i].refdes.clone(),
            Cell {
                col: max_channel_col + 1,
                row: mid_signal_row,
                orient: Orient::Right,
            },
        ));
    }

    let claimed_connectors: BTreeSet<usize> = input_connector
        .into_iter()
        .chain(output_connector)
        .collect();
    let mut shared: Vec<usize> = (0..items.len())
        .filter(|i| !channel_members.contains(i) && !claimed_connectors.contains(i))
        .filter(|&i| {
            if items[i].part.starts_with("power:") {
                return false;
            }
            let ns = nets(i);
            ns.is_empty() || ns.iter().all(|net| rails.contains_key(*net))
        })
        .collect();
    shared.sort_by(|&a, &b| refdes_cmp(&items[a].refdes, &items[b].refdes));
    // Four columns make the usual 8-channel support set (power connectors,
    // bypass bank, and mounting holes) roughly as tall as the four channel rows.
    const SUPPORT_COLS: usize = 4;
    let support_col = max_channel_col + 3;
    for (index, i) in shared.into_iter().enumerate() {
        support_cells.push((
            items[i].refdes.clone(),
            Cell {
                col: support_col + (index % SUPPORT_COLS) as i32,
                row: base_row + (index / SUPPORT_COLS) as i32 * 2,
                orient: Orient::Down,
            },
        ));
    }
    if !support_cells.is_empty() {
        idioms.push(Idiom {
            kind: "pc817_bank_support",
            anchor: bank_anchor,
            cells: support_cells,
            freeze: true,
        });
    }

    idioms
}

/// Natural refdes order via the canonical key, with the full string as tiebreak.
fn refdes_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use sch_check::model::refdes_key;
    refdes_key(a).cmp(&refdes_key(b)).then_with(|| a.cmp(b))
}
