//! Copper retraction: the copper a board edit invalidates.
//!
//! Moving a part, deleting one, or changing which net a pad belongs to leaves
//! traces that no longer describe the board. Retraction drops exactly those and
//! rewrites the file with what still stands, so the agent can re-route the named
//! nets instead of re-routing everything.

use std::collections::BTreeSet;

use geom::Rect;
use gordian_runtime::AgentRuntime;
use pcb_model::{RouteSolution, RoutingView, Trace};

/// The copper an edit invalidates, and everything it leaves standing.
#[derive(Debug, Default, Clone)]
pub(crate) struct RetractedCopper {
    pub(crate) retained: RouteSolution,
    /// Nets that lost copper and therefore need re-routing.
    pub(crate) nets: BTreeSet<String>,
    pub(crate) count: usize,
}

/// Board-space pad extents of `references`, from the problem's `pad:<ref>`
/// obstacles.
pub(crate) fn pad_extents<'a>(
    problem: &RoutingView,
    references: impl IntoIterator<Item = &'a str>,
) -> Vec<Rect> {
    let wanted: BTreeSet<&str> = references.into_iter().collect();
    problem
        .obstacles
        .iter()
        .filter(|obstacle| {
            obstacle
                .kind
                .strip_prefix("pad:")
                .is_some_and(|reference| wanted.contains(reference))
        })
        .map(|obstacle| {
            Rect::new(
                obstacle.center.x - obstacle.width / 2.0,
                obstacle.center.y - obstacle.height / 2.0,
                obstacle.center.x + obstacle.width / 2.0,
                obstacle.center.y + obstacle.height / 2.0,
            )
        })
        .collect()
}

/// Retract every net with a trace touching one of `pads`, plus every net in
/// `nets`.
///
/// The unit of retraction is the NET, not the trace. A net whose copper is only
/// partly removed is left with a stub hanging off nothing — KiCAD reports it as
/// `track_dangling`, and the board fails DRC with no unrouted net to explain it.
/// So once an edit invalidates any of a net's copper, all of it goes and the net
/// is named for re-routing.
pub(crate) fn retract(
    copper: &RouteSolution,
    pads: &[Rect],
    nets: &BTreeSet<String>,
) -> RetractedCopper {
    let touches_pad = |trace: &Trace| {
        trace
            .path
            .iter()
            .any(|point| pads.iter().any(|pad| pad.contains(*point)))
    };
    let dropped: BTreeSet<String> = copper
        .traces
        .iter()
        .filter(|trace| nets.contains(&trace.connection) || touches_pad(trace))
        .map(|trace| trace.connection.clone())
        .collect();
    let (out, kept): (Vec<_>, Vec<_>) = copper
        .traces
        .iter()
        .cloned()
        .partition(|trace| dropped.contains(&trace.connection));
    RetractedCopper {
        count: out.len(),
        retained: RouteSolution {
            traces: kept,
            vias: copper
                .vias
                .iter()
                .filter(|via| !dropped.contains(&via.connection))
                .cloned()
                .collect(),
        },
        nets: dropped,
    }
}

/// Rewrite the board file so it carries only the retained copper. A retraction
/// that dropped nothing writes nothing.
pub(crate) fn write_retained(
    ctx: &AgentRuntime,
    layer_count: u32,
    layer_names: &[String],
    retract: &RetractedCopper,
) -> std::result::Result<(), String> {
    if retract.count == 0 {
        return Ok(());
    }
    ctx.close_kicad_session();
    let path = ctx.pcb_path();
    let text =
        std::fs::read_to_string(&path).map_err(|err| format!("could not read the board: {err}"))?;
    let (stripped, _, _) = kicad_board::strip_copper(&text)?;
    let replacement =
        kicad_board::append_copper(&stripped, &retract.retained, layer_count, layer_names)?;
    // The same atomic replace the route path uses: an interrupted edit must not
    // leave a half-written board.
    crate::route::write_board_atomically(&path, replacement.as_bytes())
        .map_err(|err| format!("could not write the board: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_model::{LayerRef, Obstacle, Point2};

    fn trace(connection: &str, path: &[(f64, f64)]) -> Trace {
        Trace {
            connection: connection.into(),
            layer: LayerRef::top(),
            width: 0.2,
            path: path.iter().map(|&(x, y)| Point2::new(x, y)).collect(),
        }
    }

    fn problem() -> RoutingView {
        RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![Obstacle {
                kind: "pad:R1".into(),
                layers: vec![LayerRef::top()],
                center: Point2::new(10.0, 10.0),
                width: 1.0,
                height: 1.0,
                connected_to: vec!["VIN".into()],
            }],
            connections: Vec::new(),
            bounds: Rect::new(0.0, 0.0, 50.0, 40.0),
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            plane_nets: Default::default(),
            escape_layers: Default::default(),
            nets: Default::default(),
            fixed_copper: Default::default(),
        }
    }

    #[test]
    fn traces_on_a_named_pad_or_a_named_net_come_out() {
        let copper = RouteSolution {
            traces: vec![
                trace("VIN", &[(10.0, 10.0), (20.0, 10.0)]),
                trace("GND", &[(0.0, 0.0), (5.0, 0.0)]),
                trace("SENSE", &[(30.0, 30.0), (31.0, 30.0)]),
            ],
            vias: Vec::new(),
        };
        let pads = pad_extents(&problem(), ["R1"]);
        let out = retract(&copper, &pads, &BTreeSet::from(["SENSE".to_string()]));
        assert_eq!(out.count, 2);
        assert_eq!(
            out.nets,
            BTreeSet::from(["VIN".to_string(), "SENSE".to_string()])
        );
        assert_eq!(out.retained.traces.len(), 1);
        assert_eq!(out.retained.traces[0].connection, "GND");
    }

    #[test]
    fn a_nets_copper_comes_out_whole() {
        // The far trace never touches R1; it goes anyway, because half a net of
        // copper is a dangling stub, not a partial route.
        let copper = RouteSolution {
            traces: vec![
                trace("VIN", &[(10.0, 10.0), (20.0, 10.0)]),
                trace("VIN", &[(30.0, 30.0), (40.0, 30.0)]),
            ],
            vias: vec![pcb_model::Via {
                connection: "VIN".into(),
                at: Point2::new(30.0, 30.0),
                diameter: 0.6,
                drill: 0.3,
                span: pcb_model::ViaSpan::Through,
            }],
        };
        let out = retract(&copper, &pad_extents(&problem(), ["R1"]), &BTreeSet::new());
        assert_eq!(out.count, 2);
        assert!(out.retained.traces.is_empty());
        assert!(out.retained.vias.is_empty(), "the net's vias go with it");
    }

    #[test]
    fn an_untouched_board_retracts_nothing() {
        let copper = RouteSolution {
            traces: vec![trace("GND", &[(0.0, 0.0), (5.0, 0.0)])],
            vias: Vec::new(),
        };
        let out = retract(&copper, &pad_extents(&problem(), ["R9"]), &BTreeSet::new());
        assert_eq!(out.count, 0);
        assert_eq!(out.retained.traces.len(), 1);
    }
}
