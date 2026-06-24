//! place — the IR → millimetre realization, split into cohesive submodules that share one
//! flat namespace (re-exported below so every `floorplan::place::…` path resolves
//! verbatim):
//!
//! - [`emit`] — gather + grid seed + engine orchestration + `SchematicWriter` assembly.
//! - [`idioms`] — circuit-idiom gather/align + the anchor-block/cohesion helpers.
//! - [`refine`] — the overlap relaxers (`decongest`/`normalize`/keepout passes) the emit
//!   finalize and the engines drive.
//! - [`score`] — the routed `count_*` crossing/merge/short terms + the geometry primitives
//!   the [`measure`] library reads off a built sheet.
//! - [`measure`] — the MEASUREMENT library: [`Realizer`] (build+route+read raw counts) +
//!   [`RawMetrics`] (the weight-free 16 terms) + the [`MeasuringEngine`] dispatch. A
//!   measurement-based engine CALLS this because IT chose measurement; the OBJECTIVE
//!   (the weights) and the SEARCH live in the engine crates, not here.
//! - [`route`] — the orthogonal elbow router + power-rail riser planning.
//!
//! Realizing a sheet is heavy + non-algorithmic (how to draw and measure), so it lives
//! here as a shared library; the cost weights and the search are engine-owned method.

mod emit;
mod idioms;
mod measure;
mod refine;
mod score;
mod route;

pub use emit::*;
pub use idioms::*;
pub use measure::*;
pub use refine::*;
pub use score::*;
// `route` is the orthogonal router — every item is crate-internal (none was `pub`
// pre-split), so re-export it crate-visibly, not publicly.
pub(crate) use route::*;

// The pure placement-problem boundary — `PlaceProblem` (what an engine reads) and the
// `PlacementEngine` trait (what it implements) — lives in `sch_model::place`, so the
// engine crates depend on the shared vocabulary. The measurement-aware `MeasuringEngine`
// dispatch lives in [`measure`] alongside `Realizer`. Re-export the pure boundary here so
// this module's paths resolve unchanged.
pub use sch_model::place::{PlaceProblem, PlacementEngine};

// Re-export the layout vocabulary the submodules and `grid_tests` read, so a
// `use super::*` (whose `super` is now this `place` module) resolves them exactly as
// the pre-split module did through `floorplan`'s re-exports.
pub use sch_model::ir::{Band, Cell, Flow, LayoutIr, Orient, Side};

#[cfg(test)]
mod grid_tests {
    use super::*;
    use circuit_lang::model::{Block, Component, Design, LayoutGrid};
    use indexmap::IndexMap;
    use kicad_sexpr::geometry::SymbolGeometry;
    use sch_model::geom::Dir;
    use sch_model::item::Item;

    fn cells(names: &[&str]) -> Vec<Option<String>> {
        names
            .iter()
            .map(|n| if *n == "~" { None } else { Some((*n).to_string()) })
            .collect()
    }

    fn block(refs: &[&str], layout: LayoutGrid) -> Block {
        let mut components = IndexMap::new();
        for r in refs {
            components.insert((*r).to_string(), Component::default());
        }
        Block { note: None, components, layout }
    }

    #[test]
    fn per_block_grids_compose_into_column_bands_with_spans_and_holes() {
        let mut design = Design::default();
        // block `usb`: J1 | hole | R1  -> width 3
        design.blocks.insert(
            "usb".into(),
            block(&["J1", "R1"], vec![cells(&["J1", "~", "R1"])]),
        );
        // block `mcu`: U1 spans its column across two rows (repeated) -> first occ.
        design.blocks.insert(
            "mcu".into(),
            block(&["U1"], vec![cells(&["U1"]), cells(&["U1"])]),
        );

        let g = grid_from_layout(&design);
        // usb band starts at col 0; the `~` hole reserves col 1. Boxes are
        // [col_min, row_min, col_max, row_max].
        assert_eq!(g["J1"], [0, 0, 0, 0]);
        assert_eq!(g["R1"], [2, 0, 2, 0]);
        // mcu band starts AFTER usb's 3 columns (no overlap); U1 SPANS rows 0..1 in
        // its column (repeated down it), so its box grows in the row axis.
        assert_eq!(g["U1"], [3, 0, 3, 1]);
        assert_eq!(g.len(), 3);
    }

    #[test]
    fn collinear_body_crossing_fires_on_passthrough_not_on_series() {
        // Vertical 2-pin part, pins at (10,0) (top) and (10,10) (bottom).
        let body = vec![([10.0, 0.0], [10.0, 10.0])];
        let w = |a: [f64; 2], b: [f64; 2]| (a, b, None);
        // A wire on the SAME line (x=10) running from above the top pin to below the
        // bottom pin slices straight THROUGH the part — 1 crossing.
        assert_eq!(count_collinear_body_crossings(&body, &[w([10.0, -5.0], [10.0, 15.0])]), 1);
        // A correctly-drawn series part: leads STOP at each pin (two segments, neither
        // spanning beyond both pins) — 0.
        assert_eq!(
            count_collinear_body_crossings(
                &body,
                &[w([10.0, -5.0], [10.0, 0.0]), w([10.0, 10.0], [10.0, 15.0])],
            ),
            0
        );
        // A parallel wire on a DIFFERENT line (x=20) is not collinear — 0.
        assert_eq!(count_collinear_body_crossings(&body, &[w([20.0, -5.0], [20.0, 15.0])]), 0);
        // A PERPENDICULAR wire (handled by count_body_crossings, not this) — 0 here.
        assert_eq!(count_collinear_body_crossings(&body, &[w([0.0, 5.0], [20.0, 5.0])]), 0);
        // A wire reaching one pin from outside but stopping inside the body — 0.
        assert_eq!(count_collinear_body_crossings(&body, &[w([10.0, -5.0], [10.0, 5.0])]), 0);
    }

    #[test]
    fn block_without_layout_contributes_nothing() {
        let mut design = Design::default();
        design.blocks.insert("main".into(), block(&["U1", "R1"], Vec::new()));
        assert!(grid_from_layout(&design).is_empty());
    }

    #[test]
    fn dir_to_side_inverts_side_dir() {
        for s in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
            assert_eq!(dir_to_side(side_dir(s)), s);
        }
    }

    /// Minimal `Item` for the decoupling-bank deferral tests: `geom.pins` is padded to the right COUNT
    /// (the assignment only reads `geom.pins.len()`), while `pins` carries the (number,name,net) the
    /// classifier inspects. `at` drives the nearest-anchor tie-break.
    fn item(refdes: &str, part: &str, at: [f64; 2], nets: &[&str]) -> Item {
        let pins: Vec<(String, String, Option<String>)> = nets
            .iter()
            .enumerate()
            .map(|(k, n)| ((k + 1).to_string(), n.to_string(), Some(n.to_string())))
            .collect();
        let geom_pins = (0..nets.len())
            .map(|k| kicad_sexpr::geometry::PinGeom {
                number: (k + 1).to_string(),
                name: nets[k].to_string(),
                at: [0.0, 0.0],
                angle: 0.0,
                length: 2.54,
                unit: 1,
            })
            .collect();
        Item {
            refdes: refdes.into(),
            part: part.into(),
            value: String::new(),
            footprint: None,
            geom: SymbolGeometry { lib_id: part.into(), pins: geom_pins, raw_definition: String::new() },
            pins,
            at,
            angle: 0.0,
            unit: 1,
            mirror: false,
            frozen: false,
        }
    }

    /// A 3-pin regulator with only TWO bulk caps on its V+ rail forms NO decoupling bank (gather's ≥3
    /// floor), so those caps must NOT be deferred — `align_rail_cap_rows` keeps them in the bulk row
    /// instead of stranding them. The regression for align_rail_cap_rows defer-vs-gather drift.
    #[test]
    fn three_pin_anchor_with_two_caps_is_not_a_deferred_bank() {
        let ir = LayoutIr::default();
        let items = vec![
            item("VR1", "Regulator_Linear:AMS1117", [0.0, 0.0], &["VIN", "GND", "VCC"]),
            item("C1", "Device:C", [10.0, 0.0], &["VCC", "GND"]),
            item("C2", "Device:C", [20.0, 0.0], &["VCC", "GND"]),
        ];
        // Two caps < 3 ⇒ no bank ⇒ nothing deferred.
        assert!(decoupling_bank_caps(&items, &ir).is_empty());
    }

    /// Three+ bypass caps on a 3-pin anchor's V+ rail DO form a bank gather will re-seat, so every member
    /// is deferred (and `align_rail_cap_rows` leaves them for the gather rather than rowing them).
    #[test]
    fn three_pin_anchor_with_three_caps_is_a_deferred_bank() {
        let ir = LayoutIr::default();
        let items = vec![
            item("U1", "MCU_ST:STM32", [0.0, 0.0], &["VIN", "GND", "VCC"]),
            item("C1", "Device:C", [10.0, 0.0], &["VCC", "GND"]),
            item("C2", "Device:C", [20.0, 0.0], &["VCC", "GND"]),
            item("C3", "Device:C", [30.0, 0.0], &["VCC", "GND"]),
        ];
        let deferred = decoupling_bank_caps(&items, &ir);
        assert_eq!(deferred.len(), 3);
        assert_eq!(deferred, [1usize, 2, 3].into_iter().collect());
    }

    /// A connector touching the rail is the supply ENTRY, not a decoupling target: its caps never form a
    /// deferred bank even at ≥3 (mirrors gather's `Not(Connector)` exclusion).
    #[test]
    fn connector_anchor_never_forms_a_deferred_bank() {
        let ir = LayoutIr::default();
        let items = vec![
            item("J1", "Connector:Conn_01x03", [0.0, 0.0], &["VIN", "GND", "VCC"]),
            item("C1", "Device:C", [10.0, 0.0], &["VCC", "GND"]),
            item("C2", "Device:C", [20.0, 0.0], &["VCC", "GND"]),
            item("C3", "Device:C", [30.0, 0.0], &["VCC", "GND"]),
        ];
        assert!(decoupling_bank_caps(&items, &ir).is_empty());
    }

    #[test]
    fn single_pin_port_exit_follows_pin_with_short_reach() {
        // The h-bridge regression: a lone WEST-facing gate pin must seat its port
        // pennant on a SHORT stub to its own side (clear of its body and of the
        // next symbol in a packed row), NOT the long multi-pin reach.
        let west = [([20.0, 0.0], Dir::West)];
        assert_eq!(port_exit_point(&west, Side::Left), [crate::grid::snap(20.0 - 2.54), 0.0]);
        let east = [([20.0, 0.0], Dir::East)];
        assert_eq!(port_exit_point(&east, Side::Right), [crate::grid::snap(20.0 + 2.54), 0.0]);
        // A multi-pin port keeps the longer reach so it clears the last pin.
        let two = [([20.0, 0.0], Dir::East), ([24.0, 0.0], Dir::East)];
        assert_eq!(port_exit_point(&two, Side::Right), [crate::grid::snap(24.0 + 7.62), 0.0]);
    }
}

