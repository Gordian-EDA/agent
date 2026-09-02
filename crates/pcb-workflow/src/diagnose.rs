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
use pcb_model::Violation as ConnViolation;
use pcb_model::Finding as DrcViolation;
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
             to the footprint sync_board with {{\"rules\":{{\"clearance\":{relaxed:.2}}}}}"
        )
    } else {
        format!("{subject} overlaps foreign copper; move the parts apart and route again")
    }
}

/// A violation's identity across an edit: the rule it broke and the nets it
/// involves. Copper moves, so a coordinate is not identity; the fault is.
pub(crate) type FaultKey = (&'static str, Vec<String>);

/// One explained violation, keyed so a guard can tell a fault the board already
/// had from one an edit introduced.
#[derive(Debug, Clone)]
pub(crate) struct Fault {
    pub(crate) key: FaultKey,
    pub(crate) json: Value,
}

/// Explain every violation and key it.
pub(crate) fn faults(
    violations: &[DrcViolation],
    problem: &RoutingView,
    parts: &[ImportedPart],
) -> Vec<Fault> {
    violations
        .iter()
        .map(|violation| {
            let explained = explain(violation, problem, parts);
            let mut nets = explained.nets.clone();
            nets.sort();
            Fault {
                key: (explained.kind, nets),
                json: explained.to_json(),
            }
        })
        .collect()
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
            // Two TRACKS too close is the router's own doing — never a
            // footprint's pitch — so the fix is placement, not a looser rule.
            suggestion: format!(
                "the board needs {required:.2} mm between them: move the parts these nets run \
                 between apart with move_parts, then route_board \
                 {{\"nets\":[\"{a}\", \"{b}\"]}}"
            ),
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
                    "{} — or shrink the via with sync_board \
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
                "lower the rule to match the copper: sync_board \
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
                 board with update_board_outline (currently {:.1} x {:.1} mm)",
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
            suggestion: if *layer_count >= 8 {
                "the board already has every copper layer KiCAD allows, so this is a router \
                 defect, not a rule problem — route_board again and report it if it recurs"
                    .to_owned()
            } else {
                format!(
                    "give the board the layers the route needs: sync_board \
                     {{\"rules\":{{\"layer_count\":{}}}}}",
                    (layer_count + 2).min(8)
                )
            },
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
                "sync_board {{\"rules\":{{\"via_diameter\":{diameter:.2}, \
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
                     update_board_outline bounds"
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

// ── unrouted nets ────────────────────────────────────────────────────────────

/// A terminal of a net, named the way a caller can act on it.
struct Terminal {
    pad: String,
    at: Point2,
    layer: String,
}

impl Terminal {
    fn to_json(&self) -> Value {
        json!({ "pad": self.pad, "at": [round2(self.at.x), round2(self.at.y)], "layer": self.layer })
    }
}

fn terminals_of(problem: &RoutingView, parts: &[ImportedPart], net: &str) -> Vec<Terminal> {
    problem
        .connections
        .iter()
        .find(|c| c.name == net)
        .map(|c| {
            c.points_to_connect
                .iter()
                .map(|point| {
                    let at = point.point();
                    Terminal {
                        // No pad on this point means no handle, so hand back the
                        // coordinate form `route_track` also accepts rather than
                        // a label it would reject.
                        pad: pads_at(parts, at)
                            .first()
                            .cloned()
                            .unwrap_or_else(|| format!("[{:.3}, {:.3}]", at.x, at.y)),
                        at,
                        layer: point.layer.0.clone(),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The nearest piece of foreign copper standing on the straight line between two
/// terminals — the thing the router had to get around and could not.
///
/// This is board state, not a guess at the router's search: if a pad or keepout
/// sits on the direct path, that is what a caller must move.
fn obstruction_between(
    problem: &RoutingView,
    parts: &[ImportedPart],
    net: &str,
    from: &Terminal,
    to: &Terminal,
) -> Option<Value> {
    let path = geom::Segment::new(from.at, to.at);
    // The two ends sit ON their own pads, whose neighbours are inches from the
    // line by construction. Naming one of those would tell the caller to move
    // the very part it is trying to reach, so the parts the route starts and
    // ends on are not candidates.
    let endpoints: BTreeSet<&str> = [from, to]
        .iter()
        .filter_map(|t| t.pad.split('.').next())
        .collect();
    let on_route_layer = |obstacle: &pcb_model::Obstacle| {
        obstacle.layers.is_empty()
            || obstacle
                .layers
                .iter()
                .any(|layer| layer.0 == from.layer || layer.0 == to.layer)
    };
    let mut best: Option<(f64, &pcb_model::Obstacle)> = None;
    for obstacle in &problem.obstacles {
        if obstacle.connected_to.iter().any(|n| n == net) || !on_route_layer(obstacle) {
            continue;
        }
        if obstacle
            .kind
            .strip_prefix("pad:")
            .is_some_and(|reference| endpoints.contains(reference))
        {
            continue;
        }
        let gap = path.dist_to_rect(&geom::Rect::new(
            obstacle.center.x - obstacle.width / 2.0,
            obstacle.center.y - obstacle.height / 2.0,
            obstacle.center.x + obstacle.width / 2.0,
            obstacle.center.y + obstacle.height / 2.0,
        ));
        if gap < problem.clearance && best.is_none_or(|(d, _)| gap < d) {
            best = Some((gap, obstacle));
        }
    }
    let (gap, obstacle) = best?;
    let (what, blocker) = obstacle_label(parts, obstacle);
    Some(json!({
        "what": what,
        "blocker": blocker,
        "at": [round2(obstacle.center.x), round2(obstacle.center.y)],
        "gap_mm": round3(gap.max(0.0)),
        "detail": format!(
            "the direct path is crossed by {what} at ({:.2}, {:.2}) mm, {:.3} mm from the line \
             (the board needs {:.2} mm)",
            obstacle.center.x, obstacle.center.y, gap.max(0.0), problem.clearance
        ),
    }))
}

/// Name an obstacle the way a caller can act on it, and the part (if any) that
/// would have to move.
///
/// A pad obstacle carries its owning reference in its kind (`"pad:U3"`) all the
/// way from the KiCAD snapshot, so a blocking part can be named exactly rather
/// than guessed at from coordinates.
fn obstacle_label(
    parts: &[ImportedPart],
    obstacle: &pcb_model::Obstacle,
) -> (String, Option<String>) {
    let net = obstacle.connected_to.first();
    if let Some(reference) = obstacle.kind.strip_prefix("pad:") {
        let pad = pads_at(parts, obstacle.center)
            .into_iter()
            .find(|handle| handle.starts_with(&format!("{reference}.")))
            .unwrap_or_else(|| reference.to_owned());
        return (
            match net {
                Some(net) => format!("{pad} (net {net})"),
                None => pad.clone(),
            },
            Some(reference.to_owned()),
        );
    }
    let what = match (obstacle.kind.as_str(), net) {
        ("track" | "route-trace", Some(net)) => format!("a track on net {net}"),
        ("via" | "route-via", Some(net)) => format!("a via on net {net}"),
        ("zone", Some(net)) => format!("the {net} copper zone"),
        (kind, Some(net)) => format!("{kind} copper on net {net}"),
        (kind, None) => format!("an unowned {kind} keepout"),
    };
    (what, None)
}

/// What a caller should do about one unrouted net.
/// A pad handle is a JSON string; a bare coordinate is a JSON array. Quote only
/// the former, so the suggestion is valid `route_track` input either way.
fn endpoint_literal(terminal: &Terminal) -> String {
    if terminal.pad.starts_with('[') {
        terminal.pad.clone()
    } else {
        format!("\"{}\"", terminal.pad)
    }
}

fn unrouted_suggestion(
    net: &str,
    from: &Terminal,
    to: &Terminal,
    obstruction: &Option<Value>,
) -> String {
    match obstruction
        .as_ref()
        .and_then(|o| o.get("blocker"))
        .and_then(Value::as_str)
    {
        Some(blocker) => format!(
            "move_parts to shift {blocker} off the line between {} and {}, then \
             route_board {{\"nets\":[\"{net}\"]}} — or lay it by hand with \
             route_track {{\"net\":\"{net}\",\"from\":\"{}\",\"to\":\"{}\"}}",
            from.pad, to.pad, from.pad, to.pad
        ),
        None => format!(
            "no channel was found between {} and {}: move those two parts closer with move_parts, \
             give the router another copper layer with sync_board \
             {{\"rules\":{{\"layer_count\":4}}}}, or lay it by hand with route_track \
             {{\"net\":\"{net}\",\"from\":{},\"to\":{}}}",
            from.pad,
            to.pad,
            endpoint_literal(from),
            endpoint_literal(to)
        ),
    }
}

/// Every net this route left without copper, said in pads, coordinates and the
/// obstacle that stopped it.
/// `scope` is the net subset this `route_board` call was asked to route, if it
/// was given one: a net outside it lost its copper before this call, and saying
/// so keeps the caller from chasing an obstacle that is not the reason.
pub(crate) fn unrouted_report(
    problem: &RoutingView,
    parts: &[ImportedPart],
    failed: &[pcb_model::FailedNet],
    layer_names: &[String],
    scope: Option<&BTreeSet<String>>,
) -> Vec<Value> {
    let mut reasons: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for record in failed {
        if record.connection.is_empty() {
            continue;
        }
        reasons
            .entry(record.connection.as_str())
            .or_default()
            .push(record.reason.as_str());
    }
    reasons
        .into_iter()
        .map(|(net, reasons)| {
            let terminals = terminals_of(problem, parts, net);
            let from = terminals.first();
            let to = terminals.get(1).or(from);
            let obstruction = match (from, to) {
                (Some(a), Some(b)) => obstruction_between(problem, parts, net, a, b),
                _ => None,
            };
            let in_scope = scope.is_none_or(|scope| scope.contains(net));
            json!({
                "net": net,
                "in_scope": in_scope,
                "from": from.map(Terminal::to_json),
                "to": to.map(Terminal::to_json),
                "terminals": terminals.iter().map(Terminal::to_json).collect::<Vec<_>>(),
                "layer_attempts": layer_names,
                "reason": reasons.join("; "),
                "obstruction": obstruction,
                "suggestion": match (from, to) {
                    (Some(a), Some(b)) if in_scope => unrouted_suggestion(net, a, b, &obstruction),
                    (Some(_), Some(_)) => format!(
                        "this call did not route {net}, and it had no complete copper to keep — \
                         add it to route_board {{\"nets\":[…]}} or route the whole board"
                    ),
                    _ => format!("net {net} has fewer than two terminals on this board"),
                },
            })
        })
        .collect()
}

/// Read a `refdes.pad` handle out of one KiCAD DRC item description.
///
/// KiCAD names the object in prose ("Pad 1 [VBUS] of R1 on F.Cu"). The board's
/// own part list is what turns that back into a handle a caller can pass
/// straight to `route_track`, so only a reference the board really carries and a
/// pad that part really has is ever reported.
fn pad_handle(parts: &[ImportedPart], description: &str) -> Option<(String, Point2)> {
    let words: Vec<&str> = description
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        .filter(|w| !w.is_empty())
        .collect();
    let pad_number = words
        .iter()
        .position(|w| w.eq_ignore_ascii_case("pad"))
        .and_then(|i| words.get(i + 1))?;
    let part = parts
        .iter()
        .find(|part| words.iter().any(|w| *w == part.reference))?;
    let pad = part.pads.iter().find(|p| p.number == *pad_number)?;
    Some((format!("{}.{}", part.reference, pad.number), pad.at))
}

/// KiCAD's unconnected findings as the pad pairs they are.
///
/// A bare count tells a caller nothing; the two pads that should be joined tell
/// it exactly which `route_track` or `route_board{nets}` call to make.
pub(crate) fn unconnected_pairs<'a>(
    parts: &[ImportedPart],
    violations: impl IntoIterator<Item = &'a kicad::Violation>,
) -> Vec<Value> {
    violations
        .into_iter()
        .map(|violation| {
            let handles: Vec<(String, Point2)> = violation
                .items
                .iter()
                .filter_map(|item| pad_handle(parts, &item.description))
                .collect();
            let endpoint = |index: usize| {
                handles
                    .get(index)
                    .map(|(pad, at)| json!({ "pad": pad, "at": [round2(at.x), round2(at.y)] }))
            };
            let net = violation
                .description
                .split(['[', ']'])
                .nth(1)
                .map(str::to_owned);
            json!({
                "net": net,
                "from": endpoint(0),
                "to": endpoint(1),
                "description": violation.description,
                "suggestion": match (handles.first(), handles.get(1)) {
                    (Some((from, _)), Some((to, _))) => format!(
                        "join them: route_board {{\"nets\":[{}]}}, or route_track \
                         {{\"net\":…,\"from\":\"{from}\",\"to\":\"{to}\"}}",
                        net.as_deref().map_or("…".to_owned(), |n| format!("\"{n}\"")),
                    ),
                    _ => "run route_board again, or inspect the board render".to_owned(),
                },
            })
        })
        .collect()
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
    let violating: BTreeSet<String> = explained
        .iter()
        .flat_map(|e| e.nets.iter().cloned())
        .collect();
    // A net named in a violation has no clean copper either, so it may not be
    // counted as routed just because the router did not report it failed.
    let unclean: BTreeSet<&str> = failed_connections
        .iter()
        .map(String::as_str)
        .chain(violating.iter().map(String::as_str))
        .filter(|net| problem.connections.iter().any(|c| c.name == *net))
        .collect();
    let routed = total.saturating_sub(unclean.len());
    let violating_nets: Vec<String> = violating.into_iter().collect();

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
         KiCAD board was left untouched. {routed} of {total} net(s) had clean copper. \
         {headline}{tail}",
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
            fixed_copper: pcb_model::RouteSolution::default(),
            nets: None,
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

    /// A net the router could not finish must come back as two pads, the thing
    /// standing between them, and a repair the caller can paste back.
    #[test]
    fn an_unrouted_net_names_its_pads_the_blocking_part_and_the_repair() {
        let parts = vec![
            part("U1", &[("3", 10.0, 10.0)]),
            part("J1", &[("1", 20.0, 10.0)]),
            part("C3", &[("1", 15.0, 10.0)]),
        ];
        let mut problem = problem();
        // C3's pad sits squarely on the line between U1.3 and J1.1.
        problem.obstacles.push(pcb_model::Obstacle {
            kind: "pad:C3".to_owned(),
            layers: vec![LayerRef::top()],
            center: Point2 { x: 15.0, y: 10.0 },
            width: 0.9,
            height: 1.0,
            connected_to: vec!["GND".to_owned()],
        });
        let failed = vec![pcb_model::FailedNet {
            connection: "VOUT".to_owned(),
            reason: "no grid path from point 1 to the routed tree (congestion or enclosure)"
                .to_owned(),
        }];

        let report = unrouted_report(
            &problem,
            &parts,
            &failed,
            &["F.Cu".to_owned(), "B.Cu".to_owned()],
            None,
        );

        assert_eq!(report.len(), 1);
        let net = &report[0];
        assert_eq!(net["net"], "VOUT");
        assert_eq!(net["from"]["pad"], "U1.3");
        assert_eq!(net["from"]["at"], json!([10.0, 10.0]));
        assert_eq!(net["to"]["pad"], "J1.1");
        assert_eq!(net["layer_attempts"], json!(["F.Cu", "B.Cu"]));
        assert!(
            net["reason"].as_str().unwrap().contains("no grid path"),
            "{net}"
        );
        assert_eq!(net["obstruction"]["blocker"], "C3");
        assert_eq!(net["obstruction"]["what"], "C3.1 (net GND)");
        let suggestion = net["suggestion"].as_str().unwrap();
        assert!(suggestion.contains("shift C3 off the line"), "{suggestion}");
        assert!(
            suggestion.contains("route_board {\"nets\":[\"VOUT\"]}"),
            "{suggestion}"
        );
        assert!(
            suggestion.contains("\"from\":\"U1.3\",\"to\":\"J1.1\""),
            "{suggestion}"
        );
    }

    /// With nothing on the direct line, the advice is about channels and layers,
    /// not about moving an innocent part.
    #[test]
    fn an_unrouted_net_with_a_clear_line_advises_channels_not_a_blocker() {
        let parts = vec![
            part("U1", &[("3", 10.0, 10.0)]),
            part("J1", &[("1", 20.0, 10.0)]),
        ];
        let failed = vec![pcb_model::FailedNet {
            connection: "VOUT".to_owned(),
            reason: "channel router found no clean preferred-direction route".to_owned(),
        }];

        let report = unrouted_report(&problem(), &parts, &failed, &["F.Cu".to_owned()], None);

        assert_eq!(report[0]["obstruction"], Value::Null);
        let suggestion = report[0]["suggestion"].as_str().unwrap();
        assert!(suggestion.contains("no channel was found"), "{suggestion}");
        assert!(suggestion.contains("layer_count"), "{suggestion}");
    }

    /// KiCAD reports unconnected items as prose. A caller needs the pad pair.
    #[test]
    fn unconnected_items_come_back_as_the_pad_pair_they_are() {
        let parts = vec![
            part("U1", &[("3", 10.0, 10.0)]),
            part("J1", &[("1", 20.0, 10.0)]),
        ];
        let violation = kicad::Violation {
            severity: "error".to_owned(),
            kind: "unconnected_items".to_owned(),
            description: "Missing connection between items [VOUT]".to_owned(),
            items: vec![
                kicad::ViolationItem {
                    description: "Pad 3 of U1 on F.Cu".to_owned(),
                    uuid: None,
                    pos: None,
                },
                kicad::ViolationItem {
                    description: "Pad 1 of J1 on F.Cu".to_owned(),
                    uuid: None,
                    pos: None,
                },
            ],
        };

        let pairs = unconnected_pairs(&parts, &[violation]);

        assert_eq!(pairs[0]["net"], "VOUT");
        assert_eq!(pairs[0]["from"]["pad"], "U1.3");
        assert_eq!(pairs[0]["to"]["pad"], "J1.1");
        assert_eq!(pairs[0]["to"]["at"], json!([20.0, 10.0]));
        let suggestion = pairs[0]["suggestion"].as_str().unwrap();
        assert!(
            suggestion.contains("route_board {\"nets\":[\"VOUT\"]}"),
            "{suggestion}"
        );
        assert!(
            suggestion.contains("\"from\":\"U1.3\",\"to\":\"J1.1\""),
            "{suggestion}"
        );
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
        assert!(error.contains("0 of 1 net(s) had clean copper"), "{error}");
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
        assert!(error.contains("0 of 1 net(s) had clean copper"), "{error}");
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
