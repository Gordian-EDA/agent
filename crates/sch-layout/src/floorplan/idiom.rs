//! Idiom detection — recognize circuit idioms (crystal+load-caps, decoupling
//! banks, I2C pull-ups, op-amp feedback) purely from connectivity + pin
//! geometry. The infer stage turns these into IR (frozen cells + reports);
//! read-only over the placed Items.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use super::place::{Incidence, Item};
use super::infer::is_ground;
use super::infer::{best_decoupling_anchor, place_cc_pulldown, place_crystal, place_decoupling, place_i2c_pullup, PinSide};

/// A circuit idiom recognized purely from connectivity + symbol pin geometry.
/// `infer_ir` turns it into an [`sch_model::result::IdiomReport`] for the LLM. A FROZEN
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
fn build_circuit_graph(items: &[Item], rails: &BTreeMap<String, Band>) -> circuit_graph::CircuitGraph {
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
            if is_ground(net) { circuit_graph::NetKind::Ground } else { circuit_graph::NetKind::Power }
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
            eprintln!("AUDIT-MATCH {} anchor={} score={:.2} bindings={:?}", m.pattern, m.anchor, m.score, m.bindings);
        }
    }
    let idx: BTreeMap<&str, usize> =
        items.iter().enumerate().map(|(i, it)| (it.refdes.as_str(), i)).collect();
    let get = |rd: &str| idx.get(rd).copied();

    let mut out: Vec<Idiom> = Vec::new();
    let mut claimed: BTreeSet<usize> = BTreeSet::new();
    for m in &matches {
        let Some(ai) = get(&m.anchor) else { continue };
        match m.pattern {
            "crystal" => {
                let (Some(yi), Some(caps)) = (
                    m.bindings.get("crystal").and_then(|v| v.first()).and_then(|r| get(r)),
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
                if caps.len() != 2 || caps.iter().chain(std::iter::once(&yi)).any(|c| claimed.contains(c)) {
                    continue;
                }
                if let Some(cells) =
                    place_crystal(items, inc, anchors, anchor_col, anchor_row, ai, yi, &caps)
                {
                    claimed.insert(yi);
                    claimed.extend(&caps);
                    out.push(Idiom { kind: "crystal", anchor: ai, cells, freeze: true });
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
                if let Some(mut cells) =
                    place_decoupling(items, inc, anchors, rails, anchor_col, anchor_row, ai, &caps, &out)
                {
                    // A SMALL decoupling anchor (a 3-4 pin LDO/regulator) connects only through
                    // power rails — weak cohesion, so the SA drifts it off its own FROZEN bank
                    // (the "LDO isolated far from the caps it serves" power-entry defect). Pin it
                    // WITH the bank so the power-conversion block stays together. Multi-pin ⇒
                    // `orient_angle` returns 0 (no rotation). Gated to multi-sheet so single-sheet
                    // reference snapshots stay byte-identical.
                    if std::env::var_os("MULTISHEET_REFINE").is_some()
                        && (3..=4).contains(&items[ai].geom.pins.len())
                        && !claimed.contains(&ai)
                    {
                        if let (Some(&acol), Some(&arow)) =
                            (anchor_col.get(&ai), anchor_row.get(&ai))
                        {
                            cells.push((
                                items[ai].refdes.clone(),
                                Cell { col: acol, row: arow, orient: Orient::Right },
                            ));
                        }
                    }
                    claimed.extend(cells.iter().filter_map(|(rd, _)| get(rd)));
                    out.push(Idiom { kind: "decoupling", anchor: ai, cells, freeze: true });
                }
            }
            "led_indicator" => {
                // Report-only: a GPIO LED taps its IC pin so normal placement seats it
                // well; we just recognize the pair so the mm post-pass (`align_led_chain`)
                // can snap the series resistor directly below the LED, clear of the body,
                // rather than letting it drift to a spare column.
                let Some(ri) = m.bindings.get("res").and_then(|v| v.first()).and_then(|r| get(r))
                else {
                    continue;
                };
                if claimed.contains(&ai) || claimed.contains(&ri) {
                    continue;
                }
                out.push(Idiom {
                    kind: "led_indicator",
                    anchor: ai,
                    cells: vec![(items[ri].refdes.clone(), Cell { col: 0, row: 0, orient: Orient::Down })],
                    freeze: false,
                });
            }
            "cc_pulldown" => {
                // Freeze the two CC resistors as a reserved pair beside the connector.
                let (Some(ra), Some(rb)) = (
                    m.bindings.get("res_a").and_then(|v| v.first()).and_then(|r| get(r)),
                    m.bindings.get("res_b").and_then(|v| v.first()).and_then(|r| get(r)),
                ) else {
                    continue;
                };
                if [ai, ra, rb].iter().any(|c| claimed.contains(c)) {
                    continue;
                }
                if let Some(cells) =
                    place_cc_pulldown(anchor_col, anchor_row, ai, &items[ra].refdes, &items[rb].refdes)
                {
                    claimed.insert(ra);
                    claimed.insert(rb);
                    out.push(Idiom { kind: "cc_pulldown", anchor: ai, cells, freeze: true });
                }
            }
            "i2c_pullup" => {
                // Freeze the two I2C pull-ups as a reserved pair beside the IC (tap UP to power).
                let (Some(ra), Some(rb)) = (
                    m.bindings.get("res_a").and_then(|v| v.first()).and_then(|r| get(r)),
                    m.bindings.get("res_b").and_then(|v| v.first()).and_then(|r| get(r)),
                ) else {
                    continue;
                };
                if [ai, ra, rb].iter().any(|c| claimed.contains(c)) {
                    continue;
                }
                if let Some(cells) =
                    place_i2c_pullup(anchor_col, anchor_row, ai, &items[ra].refdes, &items[rb].refdes)
                {
                    claimed.insert(ra);
                    claimed.insert(rb);
                    out.push(Idiom { kind: "i2c_pullup", anchor: ai, cells, freeze: true });
                }
            }
            _ => {}
        }
    }
    out
}

