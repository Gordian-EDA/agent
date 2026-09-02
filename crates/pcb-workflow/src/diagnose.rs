//! Turning a refused route into something a caller can act on.
//!
//! The strict lint already knows everything about a violation — the rule, the
//! nets, the measured gap against what the board required, and where it is. The
//! refusal used to publish only a count, so a caller could do nothing but retry
//! blind. [`route_refusal`] renders the violations themselves: each one named,
//! attributed to the pads it touches, located in mm, and paired with the fix its
//! class implies.

use std::collections::BTreeSet;

use kicad_board::ImportedPart;
use pcb_drc::connectivity::Violation as ConnViolation;
use pcb_drc::lint::DrcViolation;
use pcb_model::{Point2, RoutingView};
use serde_json::{Value, json};

/// How far from a violation's reported point a pad may sit and still be named as
/// the item involved. Pads are millimetre-scale, so a hit past this is noise.
const PAD_ATTRIBUTION_RADIUS_MM: f64 = 1.5;

/// The most violations spelled out in the headline; the full list stays in the
/// `violations` array.
const HEADLINE_VIOLATIONS: usize = 3;

/// One violation, explained.
struct Explained {
    kind: &'static str,
    nets: Vec<String>,
    items: Vec<String>,
    at: Option<Point2>,
    measured: Option<f64>,
    required: Option<f64>,
    detail: String,
    suggestion: String,
}

impl Explained {
    fn to_json(&self) -> Value {
        json!({
            "rule": self.kind,
            "nets": self.nets,
            "items": self.items,
            "at_mm": self.at.map(|p| json!([round2(p.x), round2(p.y)])),
            "measured_mm": self.measured.map(round3),
            "required_mm": self.required.map(round3),
            "detail": self.detail,
            "suggestion": self.suggestion,
        })
    }

    /// The one-line form used in the refusal headline.
    fn headline(&self) -> String {
        let where_at = self
            .at
            .map(|p| format!(" at ({:.2}, {:.2}) mm", p.x, p.y))
            .unwrap_or_default();
        format!("{}{where_at} — {}", self.detail, self.suggestion)
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// `U1.3`-style labels for the pads sitting on `at`, nearest first.
fn pads_at(parts: &[ImportedPart], at: Point2) -> Vec<String> {
    let mut hits: Vec<(f64, String)> = parts
        .iter()
        .flat_map(|part| {
            part.pads.iter().map(move |pad| {
                let d = (pad.at.x - at.x).hypot(pad.at.y - at.y);
                (d, format!("{}.{}", part.reference, pad.number))
            })
        })
        .filter(|(d, _)| *d <= PAD_ATTRIBUTION_RADIUS_MM)
        .collect();
    hits.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    hits.truncate(2);
    hits.into_iter().map(|(_, label)| label).collect()
}

/// The board position of a connection's `point_index`-th terminal.
fn connection_point(problem: &RoutingView, connection: &str, point_index: usize) -> Option<Point2> {
    problem
        .connections
        .iter()
        .find(|c| c.name == connection)
        .and_then(|c| c.points_to_connect.get(point_index))
        .map(pcb_model::RoutePoint::point)
}

/// A clearance rule the board cannot honour is a rule problem, not a router
/// problem: say which relaxed value would admit the geometry that exists.
fn clearance_suggestion(gap: f64, required: f64, subject: &str) -> String {
    let relaxed = (gap * 100.0).floor() / 100.0;
    if relaxed > 0.0 && relaxed < required {
        format!(
            "{subject} only leaves {gap:.3} mm here, below the board's {required:.2} mm clearance. \
             Either move the parts apart (move_parts / place_board), or if this pitch is inherent \
             to the footprint regenerate_board with {{\"rules\":{{\"clearance\":{relaxed:.2}}}}}"
        )
    } else {
        format!("{subject} overlaps foreign copper; move the parts apart and route again")
    }
}

fn explain(violation: &DrcViolation, problem: &RoutingView, parts: &[ImportedPart]) -> Explained {
    match violation {
        DrcViolation::ClearanceTraceTrace {
            a,
            b,
            layer,
            gap,
            required,
            at,
        } => Explained {
            kind: "clearance (track to track)",
            nets: vec![a.clone(), b.clone()],
            items: pads_at(parts, *at),
            at: Some(*at),
            measured: Some(*gap),
            required: Some(*required),
            detail: format!("tracks {a} and {b} on {layer} are {gap:.3} mm apart"),
            suggestion: clearance_suggestion(*gap, *required, "this track pair"),
        },
        DrcViolation::ClearanceTraceObstacle {
            connection,
            obstacle_owners,
            layer,
            gap,
            required,
            at,
        } => {
            let items = pads_at(parts, *at);
            let obstacle = if items.is_empty() {
                owners_label(obstacle_owners)
            } else {
                items.join(" / ")
            };
            Explained {
                kind: "clearance (track to pad)",
                nets: net_list([connection.as_str()], obstacle_owners),
                items: items.clone(),
                at: Some(*at),
                measured: Some(*gap),
                required: Some(*required),
                detail: format!("track {connection} on {layer} passes {gap:.3} mm from {obstacle}"),
                suggestion: clearance_suggestion(*gap, *required, "the pad escape"),
            }
        }
        DrcViolation::ClearanceViaAny {
            connection,
            other_owners,
            gap,
            required,
            at,
        } => {
            let items = pads_at(parts, *at);
            let other = if items.is_empty() {
                owners_label(other_owners)
            } else {
                items.join(" / ")
            };
            Explained {
                kind: "clearance (via)",
                nets: net_list([connection.as_str()], other_owners),
                items,
                at: Some(*at),
                measured: Some(*gap),
                required: Some(*required),
                detail: format!("via on {connection} sits {gap:.3} mm from {other}"),
                suggestion: format!(
                    "{} — or shrink the via with regenerate_board \
                     {{\"rules\":{{\"via_diameter\":…, \"via_drill\":…}}}}",
                    clearance_suggestion(*gap, *required, "this via")
                ),
            }
        }
        DrcViolation::TraceWidthBelowMin {
            connection,
            layer,
            width,
            required,
        } => Explained {
            kind: "track width",
            nets: vec![connection.clone()],
            items: Vec::new(),
            at: None,
            measured: Some(*width),
            required: Some(*required),
            detail: format!(
                "track {connection} on {layer} is {width:.3} mm wide, under the board's \
                 {required:.3} mm minimum"
            ),
            suggestion: format!(
                "lower the rule to match the copper: regenerate_board \
                 {{\"rules\":{{\"min_trace_width\":{:.2}}}}}",
                (width * 100.0).floor() / 100.0
            ),
        },
        DrcViolation::OutOfBounds {
            connection,
            overshoot,
            at,
        } => Explained {
            kind: "copper outside board",
            nets: vec![connection.clone()],
            items: pads_at(parts, *at),
            at: Some(*at),
            measured: Some(*overshoot),
            required: Some(0.0),
            detail: format!("copper on {connection} runs {overshoot:.3} mm past the board edge"),
            suggestion: format!(
                "a part sits on or over the outline — move it inward with move_parts, or grow the \
                 board with regenerate_board bounds (currently {:.1} x {:.1} mm)",
                problem.bounds.max_x - problem.bounds.min_x,
                problem.bounds.max_y - problem.bounds.min_y
            ),
        },
        DrcViolation::InvalidLayer {
            connection,
            layer,
            layer_count,
        } => Explained {
            kind: "invalid layer",
            nets: vec![connection.clone()],
            items: Vec::new(),
            at: None,
            measured: None,
            required: None,
            detail: format!(
                "copper on {connection} references layer {layer}, which does not exist on this \
                 {layer_count}-layer board"
            ),
            suggestion: format!(
                "give the board the layers the route needs: regenerate_board \
                 {{\"rules\":{{\"layer_count\":{}}}}}",
                (layer_count + 2).min(8)
            ),
        },
        DrcViolation::ViaDiameterBelowMin {
            connection,
            diameter,
            required,
            at,
        } => Explained {
            kind: "via diameter",
            nets: vec![connection.clone()],
            items: pads_at(parts, *at),
            at: Some(*at),
            measured: Some(*diameter),
            required: Some(*required),
            detail: format!(
                "via on {connection} is {diameter:.3} mm, under the board's {required:.3} mm minimum"
            ),
            suggestion: format!(
                "regenerate_board {{\"rules\":{{\"via_diameter\":{diameter:.2}, \
                 \"via_drill\":{:.2}}}}} if the fabricator allows it",
                (diameter * 0.5 * 100.0).floor() / 100.0
            ),
        },
        DrcViolation::Connectivity {
            violation:
                ConnViolation::Unconnected {
                    connection,
                    point_index,
                },
        } => {
            let at = connection_point(problem, connection, *point_index);
            let items = at.map(|p| pads_at(parts, p)).unwrap_or_default();
            let where_pad = items
                .first()
                .map_or_else(|| format!("terminal #{point_index}"), Clone::clone);
            Explained {
                kind: "unrouted",
                nets: vec![connection.clone()],
                items,
                at,
                measured: None,
                required: None,
                detail: format!("net {connection} never reaches {where_pad}"),
                suggestion: format!(
                    "no legal escape was found for {where_pad}: move that part away from its \
                     neighbours or the board edge with move_parts, or give the router room with \
                     regenerate_board bounds"
                ),
            }
        }
        DrcViolation::Connectivity {
            violation: ConnViolation::CrossNetMerge { a, b },
        } => Explained {
            kind: "short",
            nets: vec![a.clone(), b.clone()],
            items: Vec::new(),
            at: None,
            measured: None,
            required: None,
            detail: format!("copper shorts {a} to {b}"),
            suggestion: format!(
                "the two nets' copper touches — separate the parts carrying {a} and {b} with \
                 move_parts and route again"
            ),
        },
    }
}

fn owners_label(owners: &[String]) -> String {
    if owners.is_empty() {
        "unowned copper".to_owned()
    } else {
        owners.join("/")
    }
}

fn net_list<'a>(first: impl IntoIterator<Item = &'a str>, rest: &[String]) -> Vec<String> {
    let mut nets: BTreeSet<String> = first.into_iter().map(str::to_owned).collect();
    nets.extend(rest.iter().cloned());
    nets.into_iter().collect()
}

/// The refusal payload for a route whose copper failed the strict lint.
///
/// Names every violation (rule, nets, pads, location, measured against
/// required) with the fix its class implies, and reports how much of the board
/// did route so partial progress is visible.
pub(crate) fn route_refusal(
    violations: &[DrcViolation],
    problem: &RoutingView,
    parts: &[ImportedPart],
    failed_connections: &[String],
) -> Value {
    let explained: Vec<Explained> = violations
        .iter()
        .map(|v| explain(v, problem, parts))
        .collect();
    let total = problem.connections.len();
    let failed_set: BTreeSet<&str> = failed_connections.iter().map(String::as_str).collect();
    let routed = total.saturating_sub(failed_set.len());
    let violating_nets: Vec<String> = explained
        .iter()
        .flat_map(|e| e.nets.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let headline = explained
        .iter()
        .take(HEADLINE_VIOLATIONS)
        .map(Explained::headline)
        .collect::<Vec<_>>()
        .join("; ");
    let more = explained.len().saturating_sub(HEADLINE_VIOLATIONS);
    let tail = if more > 0 {
        format!(" (+{more} more in `violations`)")
    } else {
        String::new()
    };
    let error = format!(
        "route not written: {} DRC violation(s) in the copper the router produced, so the live \
         KiCAD board was left untouched. {routed} of {total} net(s) routed. {headline}{tail}",
        explained.len()
    );

    json!({
        "error": error,
        "violation_count": explained.len(),
        "violations": explained.iter().map(Explained::to_json).collect::<Vec<_>>(),
        "violating_nets": violating_nets,
        "routed_connection_count": routed,
        "total_connection_count": total,
        "failed_connections": failed_connections,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kicad_board::{ImportedPad, ImportedPart};
    use pcb_model::{Connection, LayerRef, Rect, RoutePoint};

    fn part(reference: &str, pads: &[(&str, f64, f64)]) -> ImportedPart {
        ImportedPart {
            reference: reference.to_owned(),
            lib_id: "Package_TO_SOT_SMD:SOT-23-5".to_owned(),
            at: Point2 { x: 0.0, y: 0.0 },
            rotation: 0,
            locked: false,
            pads: pads
                .iter()
                .map(|(number, x, y)| ImportedPad {
                    number: (*number).to_owned(),
                    net: None,
                    at: Point2 { x: *x, y: *y },
                    layers: vec![LayerRef::top()],
                })
                .collect(),
        }
    }

    fn problem() -> RoutingView {
        RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: Vec::new(),
            connections: vec![Connection {
                name: "VOUT".to_owned(),
                points_to_connect: vec![
                    RoutePoint {
                        x: 10.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                    RoutePoint {
                        x: 20.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                ],
            }],
            bounds: Rect::new(0.0, 0.0, 80.0, 60.0),
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
    fn clearance_refusal_names_pads_measurement_and_a_relaxed_rule() {
        let parts = vec![part("U1", &[("3", 10.0, 10.0), ("4", 10.65, 10.0)])];
        let violations = vec![DrcViolation::ClearanceTraceObstacle {
            connection: "VOUT".to_owned(),
            obstacle_owners: vec!["GND".to_owned()],
            layer: "F.Cu".to_owned(),
            gap: 0.15,
            required: 0.2,
            at: Point2 { x: 10.0, y: 10.0 },
        }];
        let out = route_refusal(&violations, &problem(), &parts, &["VOUT".to_owned()]);
        let error = out["error"].as_str().unwrap();
        assert!(error.contains("0.150 mm"), "{error}");
        assert!(error.contains("(10.00, 10.00) mm"), "{error}");
        assert!(error.contains("\"clearance\":0.15"), "{error}");
        assert!(error.contains("0 of 1 net(s) routed"), "{error}");
        let v = &out["violations"][0];
        assert_eq!(v["rule"], "clearance (track to pad)");
        assert_eq!(v["items"][0], "U1.3");
        assert_eq!(v["measured_mm"], 0.15);
        assert_eq!(v["required_mm"], 0.2);
        assert_eq!(out["violating_nets"], json!(["GND", "VOUT"]));
    }

    #[test]
    fn unrouted_refusal_points_at_the_pad_that_could_not_escape() {
        let parts = vec![part("J1", &[("1", 20.0, 10.0)])];
        let violations = vec![DrcViolation::Connectivity {
            violation: ConnViolation::Unconnected {
                connection: "VOUT".to_owned(),
                point_index: 1,
            },
        }];
        let out = route_refusal(&violations, &problem(), &parts, &[]);
        let error = out["error"].as_str().unwrap();
        assert!(error.contains("net VOUT never reaches J1.1"), "{error}");
        assert!(error.contains("1 of 1 net(s) routed"), "{error}");
        assert_eq!(out["violations"][0]["items"][0], "J1.1");
    }

    #[test]
    fn short_refusal_names_both_nets() {
        let violations = vec![DrcViolation::Connectivity {
            violation: ConnViolation::CrossNetMerge {
                a: "NETA".to_owned(),
                b: "NETB".to_owned(),
            },
        }];
        let out = route_refusal(&violations, &problem(), &[], &[]);
        let error = out["error"].as_str().unwrap();
        assert!(error.contains("copper shorts NETA to NETB"), "{error}");
        assert_eq!(out["violations"][0]["rule"], "short");
    }

    #[test]
    fn headline_truncates_but_the_array_keeps_every_violation() {
        let violations: Vec<DrcViolation> = (0..5)
            .map(|i| DrcViolation::TraceWidthBelowMin {
                connection: format!("N{i}"),
                layer: "F.Cu".to_owned(),
                width: 0.1,
                required: 0.2,
            })
            .collect();
        let out = route_refusal(&violations, &problem(), &[], &[]);
        assert!(
            out["error"].as_str().unwrap().contains("(+2 more"),
            "{}",
            out["error"]
        );
        assert_eq!(out["violations"].as_array().unwrap().len(), 5);
        assert_eq!(out["violation_count"], 5);
    }
}
