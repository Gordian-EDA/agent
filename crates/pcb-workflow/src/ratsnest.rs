//! The one shape a caller reads a partial board's connectivity through.
//!
//! `get_board{net}` and `route_board` answer the same question — which nets have
//! copper, which do not, and what is standing in the way — so they answer it
//! with the same entries: two endpoints, a status, the blocker when there is
//! one, and the calls that would free it.

use std::collections::{BTreeMap, BTreeSet};

use kicad_board::BoardSnapshot;
use pcb_model::{Finding, RoutingView, Violation};
use serde_json::{Value, json};

use crate::diagnose::{Obstruction, Terminal, obstruction_between, terminals_of};

/// Whether a net's copper joins its pads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// No copper yet, and nothing identifiable in the way.
    Open,
    /// The net's pads are all joined.
    Routed,
    /// A route was tried and something concrete stopped it.
    Blocked,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::Routed => "routed",
            Status::Blocked => "blocked",
        }
    }
}

/// The board's connectivity as a ratsnest: one entry per net, plus the counts
/// `check_board` and `route_board` report progress with.
pub(crate) struct Ratsnest {
    pub(crate) entries: Vec<Value>,
    pub(crate) routed: usize,
    pub(crate) total: usize,
}

impl Ratsnest {
    /// The blocked entries on their own, for a progress report.
    pub(crate) fn blocked(&self) -> Vec<Value> {
        self.entries
            .iter()
            .filter(|entry| entry.get("status").and_then(Value::as_str) == Some("blocked"))
            .cloned()
            .collect()
    }
}

/// The connections whose copper does not reach all their pads, straight from
/// the same lint the guard trusts.
pub(crate) fn unrouted_connections(board: &BoardSnapshot) -> BTreeSet<String> {
    let mut problem = board.problem.clone();
    crate::route::remove_existing_copper_obstacles(&mut problem);
    pcb_engine::check(&problem, &board.copper)
        .into_iter()
        .filter_map(|finding| match finding {
            Finding::Connectivity {
                violation: Violation::Unconnected { connection, .. },
            } => Some(connection),
            _ => None,
        })
        .collect()
}

/// What would free one entry, in the calls that would do it.
fn escapes(
    net: &str,
    from: &Terminal,
    to: &Terminal,
    staged: &[&str],
    obstruction: Option<&Obstruction>,
) -> Vec<String> {
    if !staged.is_empty() {
        return vec![format!(
            "place_board {{\"refs\": {}}} — this net reaches a part still in the staging row",
            serde_json::to_string(staged).unwrap_or_else(|_| "[]".to_owned())
        )];
    }
    // A pad handle is a JSON string and a bare coordinate a JSON array, so the
    // suggestion is valid `route_track` input either way.
    let hand_route = format!(
        "route_track {{\"net\":\"{net}\",\"from\":{},\"to\":{}}}",
        from.to_literal(),
        to.to_literal()
    );
    match obstruction.and_then(|obstruction| obstruction.owner_ref.as_deref()) {
        Some(blocker) => vec![
            format!(
                "move_parts to shift {blocker} off the line, then route_board {{\"nets\":[\"{net}\"]}}"
            ),
            hand_route,
        ],
        None => vec![
            "move_parts to bring the two parts closer".to_owned(),
            "sync_board {\"rules\":{\"layer_count\":4}} to give the router another layer"
                .to_owned(),
            hand_route,
        ],
    }
}

/// Build the ratsnest for a board.
///
/// `problem` is the routing view the obstacles should be read from — for
/// `route_board` that is the view the router actually faced, so the copper this
/// call kept can be named as the thing in the way. `failed` names the nets a
/// router attempt gave up on; a net with no attempt behind it and no copper is
/// simply `open`. `scope`, when given, limits the entries to the nets a call was
/// asked about.
pub(crate) fn build(
    board: &BoardSnapshot,
    problem: &RoutingView,
    failed: &[pcb_model::FailedNet],
    scope: Option<&BTreeSet<String>>,
) -> Ratsnest {
    let unrouted = unrouted_connections(board);
    let staged = crate::staging::staged_references(board);
    let mut reasons: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for record in failed.iter().filter(|record| !record.connection.is_empty()) {
        reasons
            .entry(record.connection.as_str())
            .or_default()
            .push(record.reason.as_str());
    }

    let mut entries = Vec::new();
    let mut routed = 0usize;
    let mut total = 0usize;
    for connection in &board.problem.connections {
        let net = connection.name.as_str();
        if scope.is_some_and(|scope| !scope.contains(net)) {
            continue;
        }
        total += 1;
        let terminals = terminals_of(problem, &board.imported.parts, net);
        let (Some(from), Some(to)) = (terminals.first(), terminals.get(1)) else {
            entries.push(json!({
                "net": net,
                "from": terminals.first().map(Terminal::to_endpoint_json),
                "to": Value::Null,
                "status": Status::Open.as_str(),
                "escapes": [format!("net {net} has fewer than two terminals on this board")],
            }));
            continue;
        };
        let on_staged: Vec<&str> = terminals
            .iter()
            .filter_map(|terminal| terminal.reference.as_deref())
            .filter(|reference| staged.contains(*reference))
            .collect();
        // Status is read off the board, not off whether some call happened to
        // try this net: copper that joins the pads is routed, a net whose
        // direct path is crossed by foreign copper is blocked, and everything
        // else is simply open. That way `get_board`, `check_board` and
        // `route_board` answer the same question the same way.
        let status = if !unrouted.contains(net) && on_staged.is_empty() {
            routed += 1;
            Status::Routed
        } else {
            Status::Open
        };
        let obstruction = (status == Status::Open && on_staged.is_empty())
            .then(|| obstruction_between(problem, &board.imported.parts, net, from, to))
            .flatten();
        let plane_partial = reasons.get(net).is_some_and(|records| {
            records
                .iter()
                .any(|reason| reason.contains("copper pour reaches only part"))
        });
        let status = match (status, &obstruction, plane_partial) {
            (Status::Open, Some(_), _) | (Status::Open, None, true) => Status::Blocked,
            (status, _, _) => status,
        };
        let mut entry = json!({
            "net": net,
            "from": from.to_endpoint_json(),
            "to": to.to_endpoint_json(),
            "status": status.as_str(),
        });
        if let Some(object) = entry.as_object_mut() {
            if let Some(obstruction) = &obstruction {
                object.insert("blocker".to_owned(), obstruction.to_blocker_json());
            } else if plane_partial {
                object.insert(
                    "blocker".to_owned(),
                    json!({
                        "kind": "zone",
                        "owner_ref": Value::Null,
                        "net": net,
                        "layer": to.layer,
                        "at": [to.at.x, to.at.y],
                        "gap_mm": 0.0,
                        "need_mm": problem.clearance,
                    }),
                );
            }
            if status != Status::Routed {
                object.insert(
                    "escapes".to_owned(),
                    json!(escapes(net, from, to, &on_staged, obstruction.as_ref())),
                );
                if let Some(reason) = reasons.get(net) {
                    object.insert("reason".to_owned(), json!(reason.join("; ")));
                }
            }
        }
        entries.push(entry);
    }
    Ratsnest {
        entries,
        routed,
        total,
    }
}
