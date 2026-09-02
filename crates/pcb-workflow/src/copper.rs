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

/// Drop every trace that touches one of `pads` or carries one of `nets`.
///
/// Vias are retained: they are net-local stitches, not pad attachments, and a
/// re-route of the named nets replaces them where needed.
pub(crate) fn retract(
    copper: &RouteSolution,
    pads: &[Rect],
    nets: &BTreeSet<String>,
) -> RetractedCopper {
    let invalidated = |trace: &Trace| {
        nets.contains(&trace.connection)
            || trace
                .path
                .iter()
                .any(|point| pads.iter().any(|pad| pad.contains(*point)))
    };
    let mut retract = RetractedCopper::default();
    for trace in &copper.traces {
        if invalidated(trace) {
            retract.nets.insert(trace.connection.clone());
            retract.count += 1;
        } else {
            retract.retained.traces.push(trace.clone());
        }
    }
    retract.retained.vias = copper.vias.clone();
    retract
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
    std::fs::write(&path, replacement).map_err(|err| format!("could not write the board: {err}"))
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
