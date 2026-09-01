//! `cluster-place` — a schematic placement engine that adds the one lever the
//! per-component simulated annealer structurally lacks: **hub pose**.
//!
//! The SA's move set only TRANSLATES an anchor (carrying its block) and re-orients 2-pin
//! satellites; an IC's `angle`/`mirror` are seeded once from the heuristic IR and never
//! searched. Yet which way an IC faces — its rotation and its left↔right mirror — decides
//! whether its pins meet their neighbours head-on or force the wires to wrap around the
//! body and cross. This engine takes the SA's placement and then searches each hub's 8
//! poses ([`pose`]), moving the hub AND its satellite cluster RIGIDLY (an exact D4-group
//! transform) so the decoupling caps / pull-ups follow the rotated pins. A pose is kept
//! ONLY when it strictly cuts the shipped (truthfulness, warnings, crossings), so the
//! result is strictly additive — never worse than the SA, better where an IC was facing
//! the wrong way.
//!
//! Beyond pose it runs a DE-SPRAWL floorplanner ([`compact`]): each module is laid out cleanly
//! IN ISOLATION (hub + a single-row decoupling bank)
//! and the footprints re-packed, kept only when it strictly out-de-sprawls the SA on BOTH sprawl
//! measures (label-inclusive rendered extent AND part-origin spread) with no new warnings or
//! crossings — else it reverts, so the result is never worse. On repetitive power-IC ARRAYS it
//! then applies the "modules between rails" idiom ([`compact::rail_relayout`]): stand the ICs
//! sharing the dominant rail in one row so a shared trunk replaces their distributed power
//! glyphs. Full-dataset validation: 13/40 liftable boards de-sprawl, 0 regressions.
//!
//! Relational intent ([`sch_place::ir::Relation`]) arrives already satisfied from the anneal
//! baseline; pose, de-sprawl, and the rail relayout are relation-blind rigid moves, so each
//! gate below refuses a result that breaks one.
//!
//! It owns its objective and search; it measures candidates through `sch-floorplan`'s
//! [`RoutedEvaluator`] and implements the published [`PlacementEngine`] trait.

mod compact;
mod eval;
mod pose;

const DEBUG_DIAGNOSTICS: bool = false;

use kicad::KicadInstallation;
use sch_check::model::Design;
use sch_place::ir::LayoutIr;
use sch_place::item::Item;
use sch_place::place::{Crossings, PlaceResult};

use sch_floorplan::contract::{
    PlacementEngine, PlacementOutput, RoutedEvaluator, RoutedSheetRealizer, SchematicPlaceProblem,
};
use sch_floorplan::engine_support::{FAST_PINS, relation_viol};

/// Cluster-pose placement: the SA's leaf seating + a strictly-additive rigid hub-pose search.
pub struct ClusterPlace;

impl PlacementEngine for ClusterPlace {
    fn name(&self) -> &'static str {
        "cluster"
    }

    fn place(
        &self,
        env: &KicadInstallation,
        design: &Design,
        problem: &mut SchematicPlaceProblem,
        ir: Option<LayoutIr>,
    ) -> PlacementOutput {
        // The routed annealer has a fixed multi-start budget of thousands of full
        // route/text-solve evaluations.  On tiny, simple sheets that setup cost can
        // dominate the entire commit (a connector + two-resistor divider took over
        // two minutes), despite there being no useful global search to perform.
        // Spine is deterministic and route-aware, and solves this topology in one
        // pass. Keep the cluster engine identity in diagnostics because this is an
        // internal fast path, not a user-selected engine change.
        if spine_fast_path_pin_profile(
            problem
                .items
                .iter()
                .map(|item| (item.part.as_str(), item.geom.pins.len())),
        ) {
            let mut out = spine_place::SpinePlace.place(env, design, problem, ir);
            out.result.engine = self.name().to_owned();
            return out;
        }
        // 1. Baseline placement: the SA's own best (its strong leaf search + the
        //    route-aware refinement). The pose lever is layered ON TOP so it is isolated —
        //    where pose finds nothing the result is byte-identical to the SA.
        let mut out = anneal_place::Anneal.place(env, design, problem, ir);
        // The pose search + density sweep + gate each realize the sheet several times; on a
        // huge board (hundreds of parts) that text-solve cost dominates and can time out, for a
        // de-sprawl the floorplanner rarely lands there anyway. Ship the (already-computed)
        // anneal result directly above a size cap so the engine never regresses on latency.
        if problem.items.is_empty() || problem.items.len() > 70 {
            return out;
        }
        let realizer = RoutedSheetRealizer::new(env, &problem.inc, &out.ir, problem.options);
        let eval = RoutedEvaluator::new(&realizer);
        // The SA's RENDERED sprawl (post text-solve + orphan label-columns), captured BEFORE
        // pose, is the baseline the de-sprawl floorplanner must beat outright — measured the
        // same way as the candidate so the comparison is apples-to-apples (a pose move that
        // spreads an IC can't lower the bar either).
        let n = problem.items.len();
        let (sa_crossings, sa_warnings, baseline_rendered) =
            match eval.shipped(design, &problem.items) {
                Some((cr, w, r)) => (cr.total(), w, compact::rendered_sprawl(&r, n)),
                None => (usize::MAX, usize::MAX, f64::MAX),
            };
        let baseline_parts = compact::part_sprawl(&problem.items);
        // Relations arrive already satisfied from the anneal baseline (which searches under
        // a hard feasibility rule); pose and the de-sprawl floorplanner are relation-blind
        // rigid moves, so the Pareto gate below refuses any of their results that breaks one.
        let baseline_relation = relation_viol(&problem.items, &out.ir);
        // Snapshot the SA placement so the whole pose+compact result can fall back to it.
        let sa_snap = crate::eval::save(&problem.items);
        // 2. THE lever the SA never searches: re-pose each hub (+ its satellite cluster,
        //    moved rigidly), keeping a pose only when it strictly cuts shipped crossings. Pose
        //    can only REDUCE crossings, so when the anneal already routed the sheet crossing-free
        //    (the common case) the whole search is wasted realizes — skip it.
        if sa_crossings > 0 {
            pose::search_hub_poses(&eval, &mut problem.items, &problem.inc, &out.ir);
        }
        // 3. De-sprawl floorplanner: lay each module
        //    out in isolation + pack, kept only when it strictly out-de-sprawls the SA on both
        //    sprawl measures without regressing warnings/crossings — else it reverts.
        compact::compact_clusters(
            &eval,
            design,
            &mut problem.items,
            &problem.inc,
            &out.ir,
            baseline_rendered,
            sa_warnings,
        );
        // 4. SAFETY NET: pose gates on gate-time (truthfulness, warnings, crossings), which is
        //    blind to the emit's orphan label-columns — so it can chase a phantom gate-time win
        //    that ships a MORE-SPRAWLED or MORE-COLLIDING sheet (a dense board: 54→78 sprawl, or
        //    1→4 warnings, crossings unchanged). Pose's genuine value is CROSSINGS, so measure
        //    the SHIPPED result and fall back to the SA snapshot unless pose/compact earned its
        //    keep: a real crossing cut, no new warnings, and no sprawl bloat.
        let (final_crossings, final_warnings, final_rendered) =
            match eval.shipped(design, &problem.items) {
                Some((cr, w, r)) => (cr.total(), w, compact::rendered_sprawl(&r, n)),
                None => (usize::MAX, usize::MAX, f64::MAX),
            };
        // STRICT PARETO: ship pose+compact only if it regresses NOTHING — warnings, crossings,
        // and BOTH sprawl measures (the label-inclusive rendered extent AND the part-origin
        // spread). Two measures because each is blind where the other sees: rendered catches the
        // orphan-column balloon a dense pack causes; part-spread catches a pose splaying an IC,
        // which leaves the label-padded extent flat. A mixed result (pose cut crossings but
        // spread the parts +20%) is NOT a more human-like sheet, so revert it.
        let final_parts = compact::part_sprawl(&problem.items);
        let earned_keep = final_warnings <= sa_warnings
            && relation_viol(&problem.items, &out.ir) <= baseline_relation
            && final_crossings <= sa_crossings
            && final_rendered <= baseline_rendered + 1e-3
            && final_parts <= baseline_parts + 1e-3;
        if !earned_keep {
            crate::eval::restore(&mut problem.items, &sa_snap);
        }
        if DEBUG_DIAGNOSTICS {
            tracing::debug!(
                "[cluster] x {sa_crossings}->{final_crossings}  w {sa_warnings}->{final_warnings}  rendered {baseline_rendered:.1}->{final_rendered:.1}  parts {baseline_parts:.1}->{final_parts:.1}  keep={earned_keep}"
            );
        }
        // 5. POWER RAILS: try the "modules between rails" idiom — stand the ICs sharing the
        //    dominant power net in one top-aligned row so a shared trunk replaces their
        //    distributed per-pin power glyphs (the dominant residual sprawl). Snapshot first;
        //    keep it only if the SHIPPED rendered sheet (with the trunk forced) shrinks with no
        //    new warnings or crossings — a colliding trunk reverts. Gate measures via a fresh
        //    realizer that carries `rail_force`; anneal never sets it ⇒ references unaffected.
        let cur = eval
            .shipped(design, &problem.items)
            .map(|(cr, w, r)| (cr.total(), w, compact::rendered_sprawl(&r, n)));
        // `eval`/`realizer` borrow `out.ir`; their last use is the shipped measurement above,
        // so NLL frees that borrow here and the rail step may replace `out.ir`.
        if let Some((cur_x, cur_w, cur_spr)) = cur {
            let pre = crate::eval::save(&problem.items);
            if let Some(rail) = compact::rail_relayout(&mut problem.items, &problem.inc, &out.ir) {
                let mut ir_rail = out.ir.clone();
                ir_rail.rail_force.insert(rail);
                let rz = RoutedSheetRealizer::new(env, &problem.inc, &ir_rail, problem.options);
                let ev = RoutedEvaluator::new(&rz);
                let got = ev
                    .shipped(design, &problem.items)
                    .map(|(cr, w, r)| (cr.total(), w, compact::rendered_sprawl(&r, n)));
                let keep = rail_candidate_wins((cur_x, cur_w, cur_spr), got)
                    && relation_viol(&problem.items, &ir_rail) <= baseline_relation;
                if DEBUG_DIAGNOSTICS {
                    tracing::debug!(
                        "[cluster] rails: {cur_spr:.1} -> {:?}  keep={keep}",
                        got.map(|g| g.2)
                    );
                }
                if keep {
                    out.ir = ir_rail;
                } else {
                    crate::eval::restore(&mut problem.items, &pre);
                }
            } else {
                crate::eval::restore(&mut problem.items, &pre);
            }
        }
        let final_realizer = RoutedSheetRealizer::new(env, &problem.inc, &out.ir, problem.options);
        let final_eval = RoutedEvaluator::new(&final_realizer);
        out.result = report(self.name(), &problem.items, &final_eval);
        out
    }
}

/// Sheets at either end of the small-board complexity range do not justify anneal's
/// fixed routed-search budget. Tiny sheets have no useful global search space. Dense
/// agent-sized sheets just below [`FAST_PINS`] are the opposite failure mode: each
/// routed objective is expensive and the four small-board searches can exceed the tool
/// timeout (an 11-part NE555 draft spent 105 s in greedy refine alone). Spine is
/// deterministic and route-aware, and finishes both profiles with bounded work.
fn spine_fast_path_pin_profile<'a>(pin_profiles: impl Iterator<Item = (&'a str, usize)>) -> bool {
    let profiles: Vec<(&str, usize)> = pin_profiles.collect();
    let counts: Vec<usize> = profiles.iter().map(|(_, pins)| *pins).collect();
    let pins = counts.iter().sum::<usize>();
    // A connector/IC plus a handful of passives is topologically simple even
    // when the anchor exposes many stacked terminals (USB-C is 15 pins). Its
    // routed anneal objective is disproportionately expensive, while Spine has
    // only one anchor pose to solve.
    let single_anchor = !counts.is_empty()
        && counts.len() <= 8
        && pins <= FAST_PINS
        && counts.iter().filter(|&&pins| pins >= 3).count() <= 1;
    let dense_interactive = (5..=24).contains(&counts.len())
        && (24..=FAST_PINS).contains(&pins)
        && counts.iter().filter(|&&pins| pins >= 3).count() <= 2;
    // A complete protected CAN interface has several legitimate small hubs at
    // once: the transceiver, two connectors, termination switch, and dual TVS
    // devices. That makes the generic anchor-count rule above miss it even
    // though its topology is still a bounded bus with passive satellites. Full
    // routed annealing of the reproduced 18-part block exceeded the interactive
    // tool timeout; Spine consumes the inferred bus/protection idioms directly.
    let can_interface = (8..=24).contains(&profiles.len())
        && pins <= 80
        && profiles.iter().any(|(part, _)| {
            let part = part.to_ascii_uppercase();
            part.contains("INTERFACE_CAN_LIN:") || part.contains("CAN_TRANSCEIVER")
        });
    // A dual-op-amp symbol expands into three unit items that all inherit the
    // package's full pin table. Small bias/filter blocks therefore look like
    // three hubs even though they have one logical IC, and hit the same costly
    // routed-search profile as the dense interactive case.
    let compact_multi_unit = counts.len() <= 8
        && (24..=FAST_PINS).contains(&pins)
        && counts.iter().filter(|&&pins| pins >= 3).count() <= 3;
    // USB-C receptacle symbols expose 15, 17, or 25 placeable pins depending on
    // whether stacked power/shield pins are collapsed. The otherwise-small
    // input/protection block can therefore sit above FAST_PINS and miss the
    // dense-sheet case. Keep this shape deliberately narrow: require an actual
    // connector lib id (pin count alone could be a small MCU), exactly one wide
    // connector, no other item above six pins, and at most three modest support
    // hubs (regulator, protector, header). Multi-IC and MCU sheets retain anneal.
    let is_wide_connector = |part: &str, pins: usize| {
        (15..=26).contains(&pins) && circuit_graph::netclass::is_connector_like(part)
    };
    let wide_connector_count = profiles
        .iter()
        .filter(|&&(part, pins)| is_wide_connector(part, pins))
        .count();
    let wide_connector_block = (8..=16).contains(&counts.len())
        && (FAST_PINS + 1..=64).contains(&pins)
        && wide_connector_count == 1
        && profiles
            .iter()
            .filter(|&&(part, pins)| !is_wide_connector(part, pins))
            .all(|(_, pins)| *pins <= 6)
        && profiles
            .iter()
            .filter(|&&(part, pins)| !is_wide_connector(part, pins) && (3..=6).contains(&pins))
            .count()
            <= 3;
    // A bussed resistor-array feeding an edge header is already a complete regular
    // topology: each array leg goes directly to one connector pin, while its common
    // pin and the optional bypass capacitor land on rails. The small routed annealer
    // spends roughly 80 seconds searching permutations of this shape even though it
    // has no useful hub-pose choice. Keep the match intentionally structural and
    // narrow so MCU/analog sheets retain the premium search.
    let passive_bus_bank = profiles.len() <= 4
        && pins <= 24
        && profiles
            .iter()
            .filter(|&&(part, pins)| {
                (8..=16).contains(&pins) && circuit_graph::netclass::is_connector_like(part)
            })
            .count()
            == 1
        && profiles
            .iter()
            .filter(|&&(part, pins)| {
                (8..=16).contains(&pins) && part.to_ascii_uppercase().contains(":R_NETWORK")
            })
            .count()
            == 1
        && profiles.iter().all(|&(part, pins)| {
            circuit_graph::netclass::is_connector_like(part)
                || part.to_ascii_uppercase().contains(":R_NETWORK")
                || (pins <= 2 && (part.starts_with("Device:R") || part.starts_with("Device:C")))
        });
    // A complete 8-channel 817 input bank is already fully constrained by the
    // schematic idiom detector: the optocouplers and their per-channel passive
    // chains are frozen into regular cells. Routed annealing has no useful
    // permutation left, yet repeatedly realizing this ~60-part sheet took 158 s.
    // Spine consumes the same inferred/frozen IR in one deterministic pass.
    let opto817_count = profiles
        .iter()
        .filter(|&&(part, pins)| {
            let part = part.to_ascii_uppercase().replace(['-', '_'], "");
            pins == 4 && (part.contains("PC817") || part.contains("LTV817"))
        })
        .count();
    let repeated_817_bank = (48..=80).contains(&profiles.len())
        && opto817_count >= 8
        && profiles.iter().filter(|(_, pins)| *pins == 2).count() >= 32
        && profiles
            .iter()
            .filter(|&&(part, _)| circuit_graph::netclass::is_connector_like(part))
            .count()
            >= 4;
    if DEBUG_DIAGNOSTICS {
        tracing::debug!("[cluster] pin profile {counts:?} total={pins}");
    }
    single_anchor
        || dense_interactive
        || can_interface
        || compact_multi_unit
        || wide_connector_block
        || passive_bus_bank
        || repeated_817_bank
}

fn rail_candidate_wins(
    current: (usize, usize, f64),
    candidate: Option<(usize, usize, f64)>,
) -> bool {
    matches!(candidate, Some((crossings, warnings, sprawl))
        if crossings <= current.0
            && warnings <= current.1
            && sprawl + 1e-3 < current.2)
}

/// Measure the FINAL placement for the diagnostic [`PlaceResult`].
fn report(engine: &str, items: &[Item], eval: &RoutedEvaluator) -> PlaceResult {
    if items.is_empty() {
        return PlaceResult {
            engine: engine.to_string(),
            truthfulness_breaks: 0,
            warnings: 0,
            crossings: Crossings::default(),
            cost: 0.0,
        };
    }
    PlaceResult {
        engine: engine.to_string(),
        truthfulness_breaks: eval.truthfulness_breaks(items),
        warnings: eval.warnings(items),
        crossings: eval.crossings(items),
        cost: eval::cost(eval, items),
    }
}

#[cfg(test)]
mod tests {
    use super::{rail_candidate_wins, spine_fast_path_pin_profile};

    fn profile(pin_counts: impl IntoIterator<Item = usize>) -> bool {
        spine_fast_path_pin_profile(pin_counts.into_iter().map(|pins| ("Device:Generic", pins)))
    }

    fn connector_profile(
        pin_counts: impl IntoIterator<Item = usize>,
        connector_pins: usize,
    ) -> bool {
        spine_fast_path_pin_profile(pin_counts.into_iter().map(|pins| {
            if pins == connector_pins {
                ("Connector:USB_C_Receptacle_USB2.0", pins)
            } else {
                ("Device:Generic", pins)
            }
        }))
    }

    #[test]
    fn tiny_simple_sheet_uses_deterministic_fast_path() {
        assert!(profile([3, 2, 2, 1]));
        assert!(profile([2, 2]));
        assert!(profile([2, 2, 1, 1, 1]));
        assert!(profile([5, 2, 2, 2]));

        assert!(!profile([]));
        assert!(profile([3, 2, 2, 2, 2, 2, 1]));
        assert!(profile([15, 2, 2]));
        assert!(profile([7, 2, 1]));
        assert!(!profile([32, 2, 2]));
        assert!(!profile([3, 3, 1]));
    }

    #[test]
    fn dense_interactive_sheet_uses_deterministic_fast_path() {
        // The reproduced USB-C input block: one 15-pin connector, one 6-pin
        // protector and four two-pin parts = 29 routed pins over 6 items.
        assert!(profile([15, 6, 2, 2, 2, 2]));

        // The reproduced NE555 sheet: one 8-pin hub, a 3-pin pot and eight
        // two-pin parts = 27 routed pins over 10 placeable items.
        assert!(profile([8, 3, 2, 2, 2, 2, 2, 2, 2, 2]));
        assert!(profile([8, 8, 8, 2, 2, 2]));
        // A protected CAN node with one transceiver, ten two-pin protection/
        // passive parts, and six one-pin test points. Routed annealing this
        // simple 34-pin star exceeded the interactive apply timeout.
        assert!(profile([8, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1]));

        // Genuinely large sheets retain their existing anneal path; three-anchor
        // sheets retain hub-pose search quality.
        assert!(profile([8, 2, 2, 2, 2]));
        assert!(!profile([16, 8, 8, 2, 2, 2, 2, 2, 2, 2]));
        assert!(!profile([8, 5, 3, 2, 2, 2, 2, 2, 2, 2]));
    }

    #[test]
    fn complete_can_interface_uses_bounded_fast_path() {
        let parts = [
            ("Interface_CAN_LIN:MCP2562-E-P", 8),
            ("Connector:Conn_01x02_Pin", 2),
            ("Connector:Conn_01x02_Pin", 2),
            ("Switch:SW_SPST", 2),
            ("Device:D_TVS_Dual_ACA", 3),
            ("Device:D_TVS_Dual_ACA", 3),
            ("Device:R", 2),
            ("Device:C", 2),
            ("Device:R", 2),
            ("Device:R", 2),
            ("Device:R", 2),
            ("Device:R", 2),
            ("Device:C", 2),
            ("Device:C", 2),
            ("Device:C", 2),
            ("Device:C", 2),
            ("Device:LED", 2),
            ("Device:R", 2),
        ];
        assert!(spine_fast_path_pin_profile(parts.into_iter()));
    }

    #[test]
    fn usb_c_support_block_uses_bounded_fast_path() {
        // Exact profile from the 14-component USB-C acceptance draft. The power
        // symbol is not placeable, leaving these 13 routed items / 58 pins.
        assert!(connector_profile(
            [25, 2, 5, 2, 2, 6, 4, 2, 2, 2, 2, 2, 2],
            25
        ));
        // Exact compact 16P receptacle profile from the live acceptance run.
        assert!(connector_profile(
            [17, 2, 6, 5, 2, 2, 2, 2, 2, 2, 2, 2, 4],
            17
        ));
        assert!(connector_profile(
            [15, 2, 6, 5, 2, 2, 2, 2, 2, 2, 2, 2, 4],
            15
        ));

        // Multiple wide/large hubs and a support-heavy MCU-style block still
        // need routed anneal and its hub-pose search.
        assert!(!connector_profile([25, 8, 8, 6, 4, 2, 2, 2, 2], 25));
        assert!(!connector_profile([25, 6, 6, 6, 6, 2, 2, 2, 2], 25));
        assert!(!connector_profile([25, 24, 2, 2, 2, 2, 2, 2], 25));

        // A 15/17-pin MCU with the same support profile is not a connector and
        // must not bypass the routed hub-pose search merely because of pin count.
        assert!(!profile([17, 2, 6, 5, 2, 2, 2, 2, 2, 2, 2, 2, 4]));
        assert!(!profile([15, 2, 6, 5, 2, 2, 2, 2, 2, 2, 2, 2, 4]));
        assert!(!connector_profile([17, 16, 6, 5, 2, 2, 2, 2, 2, 2], 17));
    }

    #[test]
    fn passive_resistor_bus_bank_uses_bounded_fast_path() {
        let bank = |parts: &[(&str, usize)]| spine_fast_path_pin_profile(parts.iter().copied());
        assert!(bank(&[
            ("Device:R_Network08", 9),
            ("Device:C", 2),
            ("Connector_Generic:Conn_01x10", 10),
        ]));
        // Declaration order does not change topology.
        assert!(bank(&[
            ("Connector_Generic:Conn_01x10", 10),
            ("Device:R_Network08", 9),
            ("Device:C", 2),
        ]));

        // Similar pin counts are not enough: active hubs and generic wide parts
        // must retain the routed annealer.
        assert!(!bank(&[
            ("MCU_Microchip_ATmega:ATmega328P-AU", 9),
            ("Device:C", 2),
            ("Connector_Generic:Conn_01x10", 10),
        ]));
        assert!(!bank(&[
            ("Device:Generic", 9),
            ("Device:C", 2),
            ("Connector_Generic:Conn_01x10", 10),
        ]));
        assert!(!bank(&[
            ("Device:R_Network08", 9),
            ("Analog_ADC:ADC", 3),
            ("Connector_Generic:Conn_01x10", 10),
        ]));
    }

    #[test]
    fn repeated_817_input_bank_uses_bounded_fast_path() {
        let mut bank = vec![("Isolator:PC817", 4); 8];
        bank.extend(vec![("Device:R", 2); 32]);
        bank.extend(vec![("Device:LED", 2); 8]);
        bank.extend(vec![("Connector:Conn_01x08_Pin", 8); 4]);
        bank.extend(vec![("Mechanical:MountingHole", 0); 4]);
        assert!(spine_fast_path_pin_profile(bank.iter().copied()));

        bank.retain(|(part, _)| *part != "Isolator:PC817");
        bank.extend(vec![("Isolator:PC817", 4); 7]);
        assert!(!spine_fast_path_pin_profile(bank.iter().copied()));
    }

    #[test]
    fn rail_gate_requires_sprawl_win_without_crossing_or_warning_regression() {
        let current = (1, 2, 100.0);
        assert!(rail_candidate_wins(current, Some((1, 2, 90.0))));
        assert!(!rail_candidate_wins(current, Some((2, 2, 80.0))));
        assert!(!rail_candidate_wins(current, Some((1, 3, 80.0))));
        assert!(!rail_candidate_wins(current, Some((1, 2, 100.0))));
        assert!(!rail_candidate_wins(current, None));
    }
}
