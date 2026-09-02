//! Turning a board's defects into something a caller can act on.
//!
//! The strict lint already knows everything about a violation — the rule, the
//! nets, the measured gap against what the board required, and where it is.
//! [`violations_json`] renders those violations themselves: each one named,
//! attributed to the pads it touches, located in mm, and paired with the fix its
//! class implies. [`obstruction_between`] answers the other half — what stands
//! between two pads the router could not join — which is what the ratsnest
//! reports as a blocker.

use std::collections::{BTreeMap, BTreeSet};

use kicad_board::ImportedPart;
use pcb_model::Finding as DrcViolation;
use pcb_model::Violation as ConnViolation;
use pcb_model::{Point2, RoutingView};
use serde_json::{Value, json};

/// How far from a violation's reported point a pad may sit and still be named as
/// the item involved. Pads are millimetre-scale, so a hit past this is noise.
const PAD_ATTRIBUTION_RADIUS_MM: f64 = 1.5;

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
        DrcViolation::DanglingEnd { net, at, layer } => Explained {
            kind: "dangling copper",
            nets: vec![net.clone()],
            items: pads_at(parts, *at),
            at: Some(*at),
            measured: None,
            required: None,
            detail: format!("copper on {net} ends without an anchor on {layer}"),
            suggestion: format!(
                "route {net} into a same-net pad, via, track, or zone, or remove the unused spur"
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
pub(crate) struct Terminal {
    /// `U1.3`, or the `[x, y]` literal `route_track` also accepts when no pad
    /// sits on the point.
    pub(crate) pad: String,
    /// The footprint owning the pad, when there is one.
    pub(crate) reference: Option<String>,
    at: Point2,
    layer: String,
}

impl Terminal {
    /// The ratsnest endpoint shape: the part, the pad, where it is, and on what.
    pub(crate) fn to_endpoint_json(&self) -> Value {
        json!({
            "ref": self.reference,
            "pad": self.pad.rsplit_once('.').map_or(self.pad.as_str(), |(_, pad)| pad),
            "x": round2(self.at.x),
            "y": round2(self.at.y),
            "layer": self.layer,
        })
    }
}

pub(crate) fn terminals_of(
    problem: &RoutingView,
    parts: &[ImportedPart],
    net: &str,
) -> Vec<Terminal> {
    problem
        .connections
        .iter()
        .find(|c| c.name == net)
        .map(|c| {
            c.points_to_connect
                .iter()
                .map(|point| {
                    let at = point.point();
                    // No pad on this point means no handle, so hand back the
                    // coordinate form `route_track` also accepts rather than a
                    // label it would reject.
                    let handle = pads_at(parts, at).first().cloned();
                    Terminal {
                        reference: handle
                            .as_ref()
                            .and_then(|pad| pad.split_once('.'))
                            .map(|(reference, _)| reference.to_owned()),
                        pad: handle.unwrap_or_else(|| format!("[{:.3}, {:.3}]", at.x, at.y)),
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
/// The thing standing between two terminals, in the terms a caller acts on.
pub(crate) struct Obstruction {
    /// `pad`, `track`, `via`, `zone`, or `courtyard` for a keep-out.
    pub(crate) kind: &'static str,
    /// The footprint that would have to move, when the blocker belongs to one.
    pub(crate) owner_ref: Option<String>,
    pub(crate) net: Option<String>,
    pub(crate) layer: Option<String>,
    pub(crate) at: Point2,
    pub(crate) gap_mm: f64,
    pub(crate) need_mm: f64,
}

impl Obstruction {
    /// The ratsnest `blocker` shape.
    pub(crate) fn to_blocker_json(&self) -> Value {
        json!({
            "kind": self.kind,
            "owner_ref": self.owner_ref,
            "net": self.net,
            "layer": self.layer,
            "at": [round2(self.at.x), round2(self.at.y)],
            "gap_mm": round3(self.gap_mm),
            "need_mm": round3(self.need_mm),
        })
    }
}

/// Which of the five blocker kinds an obstacle is. Anything the board does not
/// name as copper is a placement keep-out, which on a KiCad board is a
/// courtyard.
fn obstacle_kind(kind: &str) -> &'static str {
    match kind {
        _ if kind.starts_with("pad:") => "pad",
        "track" | "route-trace" => "track",
        "via" | "route-via" => "via",
        "zone" => "zone",
        _ => "courtyard",
    }
}

pub(crate) fn obstruction_between(
    problem: &RoutingView,
    parts: &[ImportedPart],
    net: &str,
    from: &Terminal,
    to: &Terminal,
) -> Option<Obstruction> {
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
    let (_, owner_ref) = obstacle_label(parts, obstacle);
    Some(Obstruction {
        kind: obstacle_kind(&obstacle.kind),
        owner_ref,
        net: obstacle.connected_to.first().cloned(),
        layer: obstacle
            .layers
            .first()
            .map(|layer| layer.0.clone())
            .or_else(|| Some(from.layer.clone())),
        at: obstacle.center,
        gap_mm: gap.max(0.0),
        need_mm: problem.clearance,
    })
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

/// Read a `refdes.pad` handle out of one KiCAD DRC item description.
///
/// KiCAD names the object in prose ("Pad 1 [VBUS] of R1 on F.Cu"). The board's
/// own part list is what turns that back into a handle a caller can pass
/// straight to `route_track`, so only a reference the board really carries and a
/// pad that part really has is ever reported.
pub(crate) fn pad_handle(
    parts: &[ImportedPart],
    description: &str,
) -> Option<(String, String, Point2)> {
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
    Some((
        format!("{}.{}", part.reference, pad.number),
        pad.net.clone()?,
        pad.at,
    ))
}

/// Resolve one KiCad unconnected finding to two verified same-net board pads.
pub(crate) fn unconnected_pair(
    parts: &[ImportedPart],
    violation: &kicad::Violation,
) -> Option<Value> {
    let handles = violation
        .items
        .iter()
        .filter_map(|item| pad_handle(parts, &item.description));
    let mut by_net: BTreeMap<String, Vec<(String, Point2)>> = BTreeMap::new();
    for (pad, net, at) in handles {
        by_net.entry(net).or_default().push((pad, at));
    }
    let (net, handles) = by_net.into_iter().find(|(_, handles)| handles.len() >= 2)?;
    let endpoint = |index: usize| {
        let (pad, at) = &handles[index];
        json!({ "pad": pad, "at": [round2(at.x), round2(at.y)] })
    };
    Some(json!({
        "net": net,
        "from": endpoint(0),
        "to": endpoint(1),
        "description": violation.description,
        "suggestion": format!(
            "join them: route_board {{\"nets\":[\"{net}\"]}}, or route_track \
             {{\"net\":\"{net}\",\"from\":\"{}\",\"to\":\"{}\"}}",
            handles[0].0, handles[1].0,
        ),
    }))
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

/// Every real violation the router left standing, each named, attributed to
/// the pads it touches, located in mm, and paired with the fix its class
/// implies.
pub(crate) fn violations_json(
    violations: &[DrcViolation],
    problem: &RoutingView,
    parts: &[ImportedPart],
) -> Vec<Value> {
    violations
        .iter()
        .map(|violation| explain(violation, problem, parts).to_json())
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use kicad_board::{ImportedPad, ImportedPart};
    use pcb_model::{Connection, LayerRef, Rect, RoutePoint};

    fn part(reference: &str, pads: &[(&str, f64, f64)]) -> ImportedPart {
        part_on_net(reference, pads, None)
    }

    fn part_on_net(reference: &str, pads: &[(&str, f64, f64)], net: Option<&str>) -> ImportedPart {
        ImportedPart {
            reference: reference.to_owned(),
            lib_id: "Package_TO_SOT_SMD:SOT-23-5".to_owned(),
            at: Point2 { x: 0.0, y: 0.0 },
            rotation: 0,
            side: kicad_board::BoardSide::Front,
            locked: false,
            courtyard: None,
            pads: pads
                .iter()
                .map(|(number, x, y)| ImportedPad {
                    number: (*number).to_owned(),
                    net: net.map(str::to_owned),
                    at: Point2 { x: *x, y: *y },
                    layers: vec![LayerRef::top()],
                    shape: "rect".to_owned(),
                    size: Point2::new(0.0, 0.0),
                    drill: None,
                })
                .collect(),
            properties: Default::default(),
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
    fn unconnected_items_come_back_as_the_pad_pair_they_are() {
        let parts = vec![
            part_on_net("U1", &[("3", 10.0, 10.0)], Some("VOUT")),
            part_on_net("J1", &[("1", 20.0, 10.0)], Some("VOUT")),
        ];
        let violation = kicad::Violation {
            severity: "error".to_owned(),
            kind: "unconnected_items".to_owned(),
            description: "Missing connection between items".to_owned(),
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

        let pair = unconnected_pair(&parts, &violation).unwrap();

        assert_eq!(pair["net"], "VOUT");
        assert_eq!(pair["from"]["pad"], "U1.3");
        assert_eq!(pair["to"]["pad"], "J1.1");
        assert_eq!(pair["to"]["at"], json!([20.0, 10.0]));
        let suggestion = pair["suggestion"].as_str().unwrap();
        assert!(
            suggestion.contains("route_board {\"nets\":[\"VOUT\"]}"),
            "{suggestion}"
        );
        assert!(
            suggestion.contains("\"from\":\"U1.3\",\"to\":\"J1.1\""),
            "{suggestion}"
        );
    }

    /// A malformed multi-item report must never turn unrelated nets into a
    /// suggested point-to-point repair.
    #[test]
    fn unconnected_pairs_never_cross_nets() {
        let parts = vec![
            part_on_net("LED1", &[("2", 10.0, 10.0)], Some("/OP2_OUT")),
            part_on_net("U3", &[("1", 20.0, 10.0)], Some("GND")),
        ];
        let violation = kicad::Violation {
            severity: "error".to_owned(),
            kind: "unconnected_items".to_owned(),
            description: "Missing connection between items [/OP2_OUT]".to_owned(),
            items: vec![
                kicad::ViolationItem {
                    description: "Pad 2 [/OP2_OUT] of LED1 on F.Cu".to_owned(),
                    uuid: None,
                    pos: None,
                },
                kicad::ViolationItem {
                    description: "Pad 1 [GND] of U3 on F.Cu".to_owned(),
                    uuid: None,
                    pos: None,
                },
            ],
        };

        assert!(unconnected_pair(&parts, &violation).is_none());
    }

    #[test]
    fn a_clearance_violation_names_pads_its_measurement_and_a_relaxed_rule() {
        let parts = vec![part("U1", &[("3", 10.0, 10.0), ("4", 10.65, 10.0)])];
        let violations = vec![DrcViolation::ClearanceTraceObstacle {
            connection: "VOUT".to_owned(),
            obstacle_owners: vec!["GND".to_owned()],
            layer: "F.Cu".to_owned(),
            gap: 0.15,
            required: 0.2,
            at: Point2 { x: 10.0, y: 10.0 },
        }];

        let reported = violations_json(&violations, &problem(), &parts);

        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0]["rule"], "clearance (track to pad)");
        assert_eq!(reported[0]["items"][0], "U1.3");
        assert_eq!(reported[0]["measured_mm"], 0.15);
        assert_eq!(reported[0]["required_mm"], 0.2);
        assert_eq!(reported[0]["nets"], json!(["GND", "VOUT"]));
        assert!(
            reported[0]["suggestion"]
                .as_str()
                .unwrap()
                .contains("\"clearance\":0.15"),
            "{}",
            reported[0]["suggestion"]
        );
    }

    #[test]
    fn an_unreachable_pad_and_a_short_are_both_named() {
        let parts = vec![part("J1", &[("1", 20.0, 10.0)])];
        let unreachable = violations_json(
            &[DrcViolation::Connectivity {
                violation: ConnViolation::Unconnected {
                    connection: "VOUT".to_owned(),
                    point_index: 1,
                },
            }],
            &problem(),
            &parts,
        );
        assert_eq!(unreachable[0]["items"][0], "J1.1");
        assert!(
            unreachable[0]["detail"]
                .as_str()
                .unwrap()
                .contains("net VOUT never reaches J1.1")
        );

        let short = violations_json(
            &[DrcViolation::Connectivity {
                violation: ConnViolation::CrossNetMerge {
                    a: "NETA".to_owned(),
                    b: "NETB".to_owned(),
                },
            }],
            &problem(),
            &[],
        );
        assert_eq!(short[0]["rule"], "short");
        assert_eq!(short[0]["nets"], json!(["NETA", "NETB"]));
    }
}
