//! Spatial selection: a board window, lowered to the subset the tools already
//! understand.
//!
//! `place_board` and `route_board` are LOCAL operations. A caller names what to
//! work on either by reference (`refs`) / by net (`nets`), or by drawing a box
//! on the board (`bbox`). A box is only a *selector*: it resolves to the same
//! reference or net subset the tools have always taken, so one code path does
//! the work and one guard protects it. Omitting both selects the whole board.

use geom::Rect;
use kicad_board::IpcBoardSnapshot;
use pcb_model::RouteSolution;
use serde_json::Value;
use std::collections::BTreeSet;

/// Read the optional `bbox` field. An absent box is a whole-board call.
pub(crate) fn parse_bbox(input: &Value) -> std::result::Result<Option<Rect>, String> {
    let Some(value) = input.get("bbox") else {
        return Ok(None);
    };
    let object = value
        .as_object()
        .ok_or_else(|| "bbox is an object with min_x, min_y, max_x and max_y in mm".to_owned())?;
    let read = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("bbox.{key} must be a number in mm"))
    };
    let (min_x, min_y, max_x, max_y) = (
        read("min_x")?,
        read("min_y")?,
        read("max_x")?,
        read("max_y")?,
    );
    if max_x <= min_x || max_y <= min_y {
        return Err(
            "bbox must have max_x > min_x and max_y > min_y; it is a window, not a point"
                .to_owned(),
        );
    }
    Ok(Some(Rect::new(min_x, min_y, max_x, max_y)))
}

/// The footprints a box selects: those whose courtyard CENTRE is inside it.
///
/// The centre, not any overlap: a large part clipping the corner of the window
/// belongs to whatever is outside it, and moving it would not be a local edit.
pub(crate) fn parts_in_bbox(board: &IpcBoardSnapshot, bbox: &Rect) -> Vec<String> {
    board
        .imported
        .parts
        .iter()
        .filter(|part| bbox.contains(part.at))
        .map(|part| part.reference.clone())
        .collect()
}

/// The nets a box selects: every net with a pad inside the box, and every net
/// whose copper enters it.
///
/// A net crossing the boundary is selected whole. Half a net of copper is a
/// dangling stub rather than a partial route (see [`crate::copper`]), so the
/// unit of rip-up stays the net; what makes the operation local is that every
/// net that does *not* reach into the window keeps its copper untouched and
/// becomes fixed obstacle.
pub(crate) fn nets_in_bbox(
    board: &IpcBoardSnapshot,
    copper: &RouteSolution,
    bbox: &Rect,
) -> BTreeSet<String> {
    let mut nets: BTreeSet<String> = board
        .imported
        .parts
        .iter()
        .flat_map(|part| part.pads.iter())
        .filter(|pad| bbox.contains(pad.at))
        .filter_map(|pad| pad.net.clone())
        .collect();
    for trace in &copper.traces {
        if trace
            .path
            .windows(2)
            .any(|pair| segment_meets_rect(pair[0], pair[1], bbox))
        {
            nets.insert(trace.connection.clone());
        }
    }
    for via in &copper.vias {
        if bbox.contains(via.at) {
            nets.insert(via.connection.clone());
        }
    }
    nets
}

/// Does the segment `a`→`b` enter `rect`? Endpoints inside count.
fn segment_meets_rect(a: geom::Point2, b: geom::Point2, rect: &Rect) -> bool {
    if rect.contains(a) || rect.contains(b) {
        return true;
    }
    // Liang–Barsky: clip the parametric segment against each slab.
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let (mut t0, mut t1) = (0.0_f64, 1.0_f64);
    for (p, q) in [
        (-dx, a.x - rect.min_x),
        (dx, rect.max_x - a.x),
        (-dy, a.y - rect.min_y),
        (dy, rect.max_y - a.y),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return false;
            }
            continue;
        }
        let r = q / p;
        if p < 0.0 {
            t0 = t0.max(r);
        } else {
            t1 = t1.min(r);
        }
        if t0 > t1 {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::Point2;
    use serde_json::json;

    #[test]
    fn an_absent_bbox_is_a_whole_board_call() {
        assert_eq!(parse_bbox(&json!({})).unwrap(), None);
    }

    #[test]
    fn a_degenerate_bbox_is_refused_by_name() {
        let error = parse_bbox(&json!({
            "bbox": { "min_x": 5.0, "min_y": 0.0, "max_x": 5.0, "max_y": 10.0 }
        }))
        .unwrap_err();
        assert!(error.contains("window, not a point"), "{error}");
    }

    #[test]
    fn a_missing_corner_names_the_field() {
        let error = parse_bbox(&json!({ "bbox": { "min_x": 0.0, "min_y": 0.0, "max_x": 1.0 } }))
            .unwrap_err();
        assert!(error.contains("bbox.max_y"), "{error}");
    }

    fn board() -> IpcBoardSnapshot {
        use kicad_board::{ImportedBoard, ImportedPad, ImportedPart};
        let pad = |net: &str, x: f64, y: f64| ImportedPad {
            number: "1".to_owned(),
            net: Some(net.to_owned()),
            at: Point2::new(x, y),
            layers: vec![pcb_model::LayerRef::top()],
        };
        let bounds = Rect::new(0.0, 0.0, 40.0, 40.0);
        IpcBoardSnapshot {
            imported: ImportedBoard {
                layer_count: 2,
                bounds,
                parts: vec![
                    ImportedPart {
                        reference: "R1".to_owned(),
                        lib_id: "Resistor_SMD:R_0603".to_owned(),
                        at: Point2::new(15.0, 15.0),
                        rotation: 0,
                        locked: false,
                        pads: vec![pad("INSIDE", 15.0, 15.0)],
                    },
                    ImportedPart {
                        reference: "R2".to_owned(),
                        lib_id: "Resistor_SMD:R_0603".to_owned(),
                        at: Point2::new(35.0, 35.0),
                        rotation: 0,
                        locked: false,
                        pads: vec![pad("OUTSIDE", 35.0, 35.0)],
                    },
                ],
                placement_keepouts: vec![],
                keepout_count: 0,
            },
            problem: pcb_model::RoutingView {
                layer_count: 2,
                min_trace_width: 0.2,
                obstacles: vec![],
                connections: vec![],
                bounds,
                clearance: 0.2,
                via_diameter: 0.6,
                via_drill: 0.3,
                net_widths: Default::default(),
                outline: None,
                escape_layers: Default::default(),
                plane_nets: Default::default(),
                fixed_copper: Default::default(),
                nets: None,
            },
            copper: RouteSolution::default(),
            layer_names: vec!["F.Cu".to_owned(), "B.Cu".to_owned()],
        }
    }

    fn trace(connection: &str, path: &[(f64, f64)]) -> pcb_model::Trace {
        pcb_model::Trace {
            connection: connection.to_owned(),
            layer: pcb_model::LayerRef::top(),
            width: 0.2,
            path: path.iter().map(|&(x, y)| Point2::new(x, y)).collect(),
        }
    }

    #[test]
    fn a_window_selects_pads_inside_it_and_every_net_whose_copper_enters() {
        let bbox = Rect::new(10.0, 10.0, 20.0, 20.0);
        let copper = RouteSolution {
            traces: vec![
                trace("CROSSING", &[(0.0, 15.0), (30.0, 15.0)]),
                trace("FAR", &[(30.0, 30.0), (35.0, 30.0)]),
            ],
            vias: Vec::new(),
        };
        let nets = nets_in_bbox(&board(), &copper, &bbox);
        assert!(
            nets.contains("INSIDE"),
            "a pad in the window selects its net"
        );
        assert!(
            nets.contains("CROSSING"),
            "copper entering the window counts"
        );
        assert!(!nets.contains("FAR"));
        assert!(!nets.contains("OUTSIDE"));
    }

    #[test]
    fn a_segment_crossing_a_box_without_an_endpoint_inside_is_found() {
        let rect = Rect::new(10.0, 10.0, 20.0, 20.0);
        assert!(segment_meets_rect(
            Point2::new(0.0, 15.0),
            Point2::new(30.0, 15.0),
            &rect
        ));
        assert!(!segment_meets_rect(
            Point2::new(0.0, 5.0),
            Point2::new(30.0, 5.0),
            &rect
        ));
        assert!(segment_meets_rect(
            Point2::new(15.0, 15.0),
            Point2::new(30.0, 30.0),
            &rect
        ));
    }

    #[test]
    fn an_axis_aligned_segment_outside_the_box_is_rejected_by_the_degenerate_slab() {
        let rect = Rect::new(10.0, 10.0, 20.0, 20.0);
        // dx == 0 exactly: the vertical slab test divides by zero unless the
        // p == 0 case is handled.
        assert!(!segment_meets_rect(
            Point2::new(5.0, 0.0),
            Point2::new(5.0, 40.0),
            &rect
        ));
        assert!(segment_meets_rect(
            Point2::new(15.0, 0.0),
            Point2::new(15.0, 40.0),
            &rect
        ));
    }

    #[test]
    fn a_via_inside_the_box_selects_its_net() {
        let copper = RouteSolution {
            traces: Vec::new(),
            vias: vec![pcb_model::Via {
                connection: "STITCH".to_owned(),
                at: Point2::new(15.0, 15.0),
                diameter: 0.6,
                drill: 0.3,
                span: pcb_model::ViaSpan::Through,
            }],
        };
        let nets = nets_in_bbox(&board(), &copper, &Rect::new(10.0, 10.0, 20.0, 20.0));
        assert!(nets.contains("STITCH"));
    }

    #[test]
    fn a_diagonal_past_a_corner_does_not_select_the_box() {
        let rect = Rect::new(10.0, 10.0, 20.0, 20.0);
        assert!(!segment_meets_rect(
            Point2::new(0.0, 25.0),
            Point2::new(5.0, 30.0),
            &rect
        ));
    }
}
