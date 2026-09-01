//! Drawing connections. The model never supplies wire coordinates: it names
//! two ends and this module routes between them over the live sheet, or — when
//! nothing orthogonal fits — names the net at both ends instead, and says so.

use anyhow::Result;
use geom::{Dir, EPS, Point2, Segment};
use gordian_runtime::AgentRuntime;
use sch_doc::{LabelKind, SchDoc, body_rects, connect};
use sch_io::wire::{NetSegment, RouteScene, route_edge};
use serde_json::{Value, json};

use crate::place::{Occupancy, snap_point};
use crate::refs::{self, Target};
use crate::session::{Allow, Edit, symbol_source};

/// The direction a wire leaves a pin, snapped to the nearest axis.
pub(crate) fn dir_of(out: Point2) -> Dir {
    if out.x.abs() >= out.y.abs() {
        if out.x < 0.0 { Dir::West } else { Dir::East }
    } else if out.y < 0.0 {
        Dir::North
    } else {
        Dir::South
    }
}

/// The obstacle scene for a route of `net` between `a` and `b`.
///
/// Everything already on the partitions those two ends belong to is relabelled
/// `net`: the router must be free to touch what it is about to join, and would
/// otherwise refuse to leave its own start point.
fn scene(doc: &SchDoc, a: Point2, b: Point2, net: &str) -> RouteScene {
    let live = connect::scene(doc);
    let joined: Vec<String> = live
        .points
        .iter()
        .filter(|(p, _)| p.near_eq(a, EPS) || p.near_eq(b, EPS))
        .map(|(_, name)| name.clone())
        .collect();
    let rename = |name: &String| match joined.iter().any(|j| j == name) {
        true => net.to_string(),
        false => name.clone(),
    };
    RouteScene {
        solids: body_rects(doc).into_iter().map(|(_, r)| r).collect(),
        points: live
            .points
            .iter()
            .map(|(p, name)| (*p, rename(name)))
            .collect(),
        segments: live
            .segments
            .iter()
            .map(|(from, to, name)| NetSegment::new(*from, *to, rename(name)))
            .collect(),
        label_solids: Vec::new(),
    }
}

/// Draw a wire path, adding a junction wherever it lands on existing copper.
fn draw(doc: &mut SchDoc, path: &[Point2]) -> Vec<String> {
    let existing: Vec<(Point2, Point2)> = doc
        .wires()
        .flat_map(|w| {
            w.points
                .windows(2)
                .map(|p| (p[0], p[1]))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut uuids = Vec::new();
    for pair in path.windows(2) {
        uuids.push(doc.add_wire(pair[0], pair[1]));
    }
    for vertex in [path[0], path[path.len() - 1]] {
        let interior = existing.iter().any(|(from, to)| {
            Segment::new(*from, *to).contains_point(vertex)
                && !vertex.near_eq(*from, EPS)
                && !vertex.near_eq(*to, EPS)
        });
        let ends = existing
            .iter()
            .filter(|(from, to)| vertex.near_eq(*from, EPS) || vertex.near_eq(*to, EPS))
            .count();
        let already = doc.items().iter().any(|item| {
            matches!(item, sch_doc::Item::Junction(j) if j.at.near_eq(vertex, EPS))
        });
        if !already && (interior || ends >= 2) {
            doc.add_junction(vertex);
        }
    }
    uuids
}

/// Route a connection between two ends of the sheet.
pub fn connect_tool(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let mut edit = Edit::open(ctx)?;
    let (from, to) = match (input.get("from"), input.get("to")) {
        (Some(from), Some(to)) => (from, to),
        _ => return Ok(json!({ "error": "connect needs `from` and `to`" })),
    };
    let from = match refs::target(&edit.doc, from) {
        Ok(target) => target,
        Err(error) => return Ok(json!({ "error": error })),
    };
    let to = match refs::target(&edit.doc, to) {
        Ok(target) => target,
        Err(error) => return Ok(json!({ "error": error })),
    };
    let existing = |target: &Target| match target {
        Target::Pin(pin) => {
            refs::net_of(edit.before(), &pin.refdes, &pin.number).map(str::to_string)
        }
        Target::Point(_) => None,
    };
    let (from_net, to_net) = (existing(&from), existing(&to));
    let net = input
        .get("net")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| from_net.clone())
        .or_else(|| to_net.clone());
    let route_net = net.clone().unwrap_or_else(|| "#new".to_string());

    let (a, b) = (from.at(), to.at());
    if a.near_eq(b, EPS) {
        return Ok(json!({ "error": "both ends are the same point; they already touch" }));
    }
    let dir_a = match &from {
        Target::Pin(pin) => dir_of(pin.out),
        Target::Point(_) => dir_of(Point2::new(b.x - a.x, b.y - a.y)),
    };

    let allow = Allow::nothing()
        .nets(net.clone())
        .nets(from_net.clone())
        .nets(to_net.clone())
        .parts(from.owner().map(str::to_string))
        .parts(to.owner().map(str::to_string))
        .creating();

    let scene = scene(&edit.doc, a, b, &route_net);
    match route_edge(a, dir_a, b, &route_net, &scene) {
        Some(path) => {
            let wires = draw(&mut edit.doc, &path);
            // An explicitly asked-for name is part of the request, not just a
            // routing hint: give the wire that name unless it already has it.
            if let Some(wanted) = input.get("net").and_then(Value::as_str)
                && from_net.as_deref() != Some(wanted)
                && to_net.as_deref() != Some(wanted)
            {
                edit.doc.add_label(LabelKind::Local, wanted, pose(a));
            }
            let changed = format!(
                "wired {} to {} with {} segment(s)",
                from.describe(),
                to.describe(),
                wires.len()
            );
            edit.commit(json!(changed), allow)
        }
        None => {
            // Nothing orthogonal fits, so join the ends by name instead — the
            // same move a person makes when a wire would be spaghetti.
            let net = net.unwrap_or_else(|| fallback_name(&from, &to));
            for target in [&from, &to] {
                edit.doc
                    .add_label(LabelKind::Local, &net, pose(target.at()));
            }
            edit.warn(format!(
                "no clear wire path; {} and {} were joined by a `{net}` label at each end",
                from.describe(),
                to.describe()
            ));
            edit.commit(
                json!(format!("labelled both ends `{net}` — no clear wire path")),
                allow,
            )
        }
    }
}

/// A net name for a labelled connection the caller did not name, built from
/// the two ends so it reads as what it is: `N_R5_1_U1_VDD`.
fn fallback_name(from: &Target, to: &Target) -> String {
    let part = |target: &Target| {
        target
            .describe()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
    };
    format!("N_{}_{}", part(from), part(to)).to_uppercase()
}

fn pose(at: Point2) -> sch_doc::Pose {
    sch_doc::Pose::new(at.x, at.y, 0.0)
}

/// Name the net at one pin.
pub fn label_tool(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let (Some(spec), Some(net)) = (
        input.get("pin").and_then(Value::as_str),
        input.get("net").and_then(Value::as_str),
    ) else {
        return Ok(json!({ "error": "label needs `pin` and `net`" }));
    };
    let kind = match input.get("kind").and_then(Value::as_str).unwrap_or("local") {
        "local" => LabelKind::Local,
        "global" => LabelKind::Global,
        "hierarchical" => LabelKind::Hier,
        other => return Ok(json!({ "error": format!("unknown label kind `{other}`") })),
    };
    let mut edit = Edit::open(ctx)?;
    let pin = match refs::pin(&edit.doc, spec) {
        Ok(pin) => pin,
        Err(error) => return Ok(json!({ "error": error })),
    };
    let was = refs::net_of(edit.before(), &pin.refdes, &pin.number).map(str::to_string);
    edit.doc.add_label(kind, net, pose(pin.at));
    edit.commit(
        json!(format!("named {spec} `{net}`")),
        Allow::nothing()
            .net(net)
            .nets(was)
            .part(&pin.refdes)
            .creating(),
    )
}

/// Mark a pin deliberately unconnected.
pub fn no_connect(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let Some(spec) = input.get("pin").and_then(Value::as_str) else {
        return Ok(json!({ "error": "no_connect needs `pin`" }));
    };
    let mut edit = Edit::open(ctx)?;
    let pin = match refs::pin(&edit.doc, spec) {
        Ok(pin) => pin,
        Err(error) => return Ok(json!({ "error": error })),
    };
    if edit.before().no_connect.iter().any(|p| p.refdes == pin.refdes && p.pin == pin.number) {
        return Ok(json!({ "changed": format!("{spec} was already marked no-connect") }));
    }
    edit.doc.add_no_connect(pin.at);
    edit.commit(
        json!(format!("marked {spec} no-connect")),
        Allow::nothing().part(&pin.refdes),
    )
}

/// The `power:` symbols that could carry a rail called `net`, best first.
fn power_candidates(net: &str) -> Vec<String> {
    let bare = net.trim_start_matches('+');
    let mut names = vec![net.to_string(), format!("+{bare}")];
    if let Some(rest) = bare.strip_suffix('V').or(Some(bare)) {
        // `3V3` and `3.3V` are the two spellings KiCAD ships.
        if let Some((whole, frac)) = rest.split_once('V') {
            names.push(format!("+{whole}.{frac}V"));
        }
    }
    names.push(bare.to_string());
    names
        .into_iter()
        .map(|name| format!("power:{name}"))
        .collect()
}

/// Drop a power symbol straight onto a pin.
pub fn add_power(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let (Some(net), Some(spec)) = (
        input.get("net").and_then(Value::as_str),
        input.get("pin").and_then(Value::as_str),
    ) else {
        return Ok(json!({ "error": "add_power needs `net` and `pin`" }));
    };
    let mut edit = Edit::open(ctx)?;
    let pin = match refs::pin(&edit.doc, spec) {
        Ok(pin) => pin,
        Err(error) => return Ok(json!({ "error": error })),
    };
    let was = refs::net_of(edit.before(), &pin.refdes, &pin.number).map(str::to_string);
    let source = symbol_source(ctx);
    let candidates: Vec<String> = match input.get("lib_id").and_then(Value::as_str) {
        Some(lib_id) => vec![lib_id.to_string()],
        None => power_candidates(net),
    };
    let refdes = crate::edit::next_refdes(&edit.doc, "#PWR");
    let mut placed = None;
    for lib_id in &candidates {
        if edit
            .doc
            .add_symbol(lib_id, &refdes, net, pose(pin.at), &source)
            .is_ok()
        {
            placed = Some(lib_id.clone());
            break;
        }
    }
    let Some(lib_id) = placed else {
        return Ok(json!({
            "error": format!(
                "no power symbol for `{net}` (tried {}); pass `lib_id` explicitly",
                candidates.join(", ")
            ),
        }));
    };
    align_onto_pin(&mut edit.doc, &refdes, pin.at, pin.out);
    edit.commit(
        json!(format!("attached {lib_id} `{net}` to {spec}")),
        Allow::nothing()
            .net(net)
            .nets(was)
            .part(&refdes)
            .part(&pin.refdes)
            .creating(),
    )
}

/// Rotate and shift a just-placed one-pin symbol so its pin sits exactly on
/// `at`, facing back along `out`.
fn align_onto_pin(doc: &mut SchDoc, refdes: &str, at: Point2, out: Point2) {
    for rot in [0.0, 90.0, 180.0, 270.0] {
        let _ = doc.set_symbol_orientation(refdes, rot, sch_doc::Mirror::None);
        let Some(own) = sch_doc::placed_pins(doc).into_iter().find(|p| p.refdes == refdes) else {
            return;
        };
        // The rail's pin must point back the way the target pin points out.
        if own.out.x * out.x + own.out.y * out.y < -0.5 {
            break;
        }
    }
    let Some(own) = sch_doc::placed_pins(doc).into_iter().find(|p| p.refdes == refdes) else {
        return;
    };
    let symbol_at = match doc.symbol_by_ref(refdes) {
        Some(symbol) => symbol.at,
        None => return,
    };
    let _ = doc.move_symbol(
        refdes,
        symbol_at.x + (at.x - own.at.x),
        symbol_at.y + (at.y - own.at.y),
    );
}

/// Remove drawn wires by net, by the parts they touch, or by UUID.
pub fn delete_wires(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let mut edit = Edit::open(ctx)?;
    let live = connect::scene(&edit.doc);
    let wanted_net = input.get("net").and_then(Value::as_str);
    let wanted_refs: Vec<String> = input
        .get("refs")
        .and_then(Value::as_array)
        .map(|v| v.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let wanted_uuids: Vec<String> = input
        .get("uuids")
        .and_then(Value::as_array)
        .map(|v| v.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if wanted_net.is_none() && wanted_refs.is_empty() && wanted_uuids.is_empty() {
        return Ok(json!({ "error": "delete_wires needs one of `net`, `refs` or `uuids`" }));
    }
    let pins = sch_doc::placed_pins(&edit.doc);
    let touches_ref = |a: Point2, b: Point2| {
        pins.iter().any(|p| {
            wanted_refs.contains(&p.refdes)
                && (p.at.near_eq(a, EPS) || p.at.near_eq(b, EPS))
        })
    };
    let net_of_segment = |a: Point2, b: Point2| {
        live.segments
            .iter()
            .find(|(from, to, _)| from.near_eq(a, EPS) && to.near_eq(b, EPS))
            .map(|(_, _, name)| name.clone())
    };
    let doomed: Vec<String> = edit
        .doc
        .wires()
        .filter(|wire| {
            let (a, b) = (wire.points[0], wire.points[wire.points.len() - 1]);
            wanted_uuids.contains(&wire.uuid)
                || touches_ref(a, b)
                || wanted_net.is_some_and(|net| net_of_segment(a, b).as_deref() == Some(net))
        })
        .map(|wire| wire.uuid.clone())
        .collect();
    if doomed.is_empty() {
        return Ok(json!({ "changed": "no wire matched", "net_delta": "connectivity unchanged" }));
    }
    let removed = edit.doc.remove_drawing(&doomed);
    // Deleting a net's wires loosens its pins, which is the whole point.
    let loosened: Vec<String> = edit
        .before()
        .nets
        .iter()
        .filter(|net| wanted_net == Some(net.name.as_str()))
        .flat_map(|net| net.pins.iter().map(|p| p.refdes.clone()))
        .collect();
    let allow = Allow::nothing()
        .nets(wanted_net.map(str::to_string))
        .nets(refs::nets_touching(edit.before(), &wanted_refs))
        .parts(wanted_refs.clone())
        .parts(loosened)
        .creating();
    edit.commit(json!(format!("deleted {removed} wire(s)")), allow)
}

/// A free spot near a symbol, used by the tools that place something beside an
/// anchor. Exposed here so `edit` and `wiring` agree on the rule.
pub(crate) fn spot_beside(
    doc: &SchDoc,
    anchor: &str,
    side: crate::place::Side,
    w: f64,
    h: f64,
    skip: &[String],
) -> Option<Point2> {
    let symbol = doc.symbol_by_ref(anchor)?;
    let body = crate::place::extent(doc, symbol)?;
    Occupancy::skipping(doc, skip).beside(body, side, w, h)
}

/// A free spot anywhere, preferring near `from`.
pub(crate) fn spot_near(doc: &SchDoc, from: Point2, w: f64, h: f64, skip: &[String]) -> Option<Point2> {
    Occupancy::skipping(doc, skip).nearest_free(snap_point(from), w, h)
}
