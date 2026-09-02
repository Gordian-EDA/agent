//! Drawing connections. The model never supplies wire coordinates: it names
//! two ends and this module routes between them over the live sheet, or — when
//! nothing orthogonal fits — names the net at both ends instead, and says so.

use anyhow::Result;
use geom::{Dir, EPS, Point2, Segment};
use gordian_runtime::AgentRuntime;
use sch_doc::{LabelKind, SchDoc, body_rects, connect};
use sch_floorplan::wire::ElbowRouter;
use sch_model::route::{NetSegment, RouteScene, SchRouter};
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

/// The name a route is drawn under while it is being solved.
///
/// Never the caller's `net`: if the caller says `net: "GND"` the router would
/// treat every scrap of GND copper on the sheet as its own and be free to land
/// on it. Only the two partitions actually being joined get this name.
const ROUTING_NET: &str = "#routing";

/// The obstacle scene for a route between `a` and `b`.
///
/// The partitions those two ends belong to are relabelled [`ROUTING_NET`]: the
/// router must be free to touch what it is about to join, and would otherwise
/// refuse to leave its own start point. Everything else keeps its own name and
/// stays untouchable.
///
/// The two symbols being joined are also lifted out of the solids: a pin tip
/// often falls inside its own body's bounding box — an LED's does — and a
/// router that treats that box as a wall can never reach the pin at all.
fn scene(doc: &SchDoc, a: Point2, b: Point2, own: &[String]) -> RouteScene {
    let live = connect::scene(doc);
    let joined: Vec<String> = live
        .points
        .iter()
        .filter(|(p, _)| p.near_eq(a, EPS) || p.near_eq(b, EPS))
        .map(|(_, name)| name.clone())
        .collect();
    let rename = |name: &String| match joined.iter().any(|j| j == name) {
        true => ROUTING_NET.to_string(),
        false => name.clone(),
    };
    let labels = doc
        .labels()
        .map(|label| {
            let at = label.at.point();
            let half = (1.27 * label.text.chars().count() as f64).max(2.54);
            (
                geom::Rect::from_center_half(at, (half, 1.27)),
                rename(&sch_doc::unescape(&label.text)),
            )
        })
        .collect();
    RouteScene {
        solids: body_rects(doc)
            .into_iter()
            .filter(|(refdes, _)| !own.contains(refdes))
            .map(|(_, r)| r)
            .collect(),
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
        label_solids: labels,
    }
}

/// Draw a wire path, adding a junction wherever it meets existing copper — at
/// its ends and at every corner, since a corner landing mid-span draws a T that
/// KiCAD does not treat as a connection unless a dot says so.
fn draw(doc: &mut SchDoc, path: &[Point2]) -> Vec<String> {
    if path.len() < 2 {
        return Vec::new();
    }
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
    for &vertex in path {
        let interior = existing.iter().any(|(from, to)| {
            Segment::new(*from, *to).contains_point(vertex)
                && !vertex.near_eq(*from, EPS)
                && !vertex.near_eq(*to, EPS)
        });
        let ends = existing
            .iter()
            .filter(|(from, to)| vertex.near_eq(*from, EPS) || vertex.near_eq(*to, EPS))
            .count();
        let already = doc
            .items()
            .iter()
            .any(|item| matches!(item, sch_doc::Item::Junction(j) if j.at.near_eq(vertex, EPS)));
        if !already && (interior || ends >= 2) {
            doc.add_junction(vertex);
        }
    }
    uuids
}

/// Route one connection, or every connection in `pairs`.
pub fn connect_tool(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let Some(pairs) = input.get("pairs").and_then(Value::as_array) else {
        return connect_one(input, ctx);
    };
    // Each pair is its own transaction: a route that cannot be drawn falls
    // back to a label rather than failing, so there is nothing to roll back.
    // Every pair is tried — one bad reference must not silently drop the rest
    // of a block's wiring — and each result says which ends it was about.
    let mut done = Vec::new();
    let mut failures = 0;
    for pair in pairs {
        let mut result = connect_one(pair.clone(), ctx)?;
        failures += usize::from(result.get("error").is_some());
        result["from"] = pair.get("from").cloned().unwrap_or(Value::Null);
        result["to"] = pair.get("to").cloned().unwrap_or(Value::Null);
        done.push(result);
    }
    if failures == done.len() {
        return Ok(json!({ "error": "every connection failed", "connected": done }));
    }
    let revision = done
        .iter()
        .find_map(|result| result.get("revision").cloned());
    let mut output = json!({ "connected": done, "failed": failures });
    if let Some(revision) = revision {
        output["revision"] = revision;
    }
    Ok(output)
}

/// Route a connection between two ends of the sheet.
fn connect_one(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let mut edit = Edit::open(ctx)?;
    let (from, to) = match (input.get("from"), input.get("to")) {
        (Some(from), Some(to)) => (from, to),
        _ => return Ok(json!({ "error": "connect needs `from` and `to`" })),
    };
    let from = match refs::target(&edit.doc, from) {
        Ok(target) => target,
        Err(error) => return Ok(reference_error(&edit.doc, &input, error)),
    };
    let to = match refs::target(&edit.doc, to) {
        Ok(target) => target,
        Err(error) => return Ok(reference_error(&edit.doc, &input, error)),
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
    let (a, b) = (from.at(), to.at());
    if a.near_eq(b, EPS) {
        return Ok(json!({ "error": "both ends are the same point; they already touch" }));
    }
    let dir_a = match &from {
        Target::Pin(pin) => dir_of(pin.out),
        Target::Point(_) => dir_of(Point2::new(b.x - a.x, b.y - a.y)),
    };

    let allow = Allow::nothing()
        .joining_nets(net.clone())
        .joining_nets(from_net.clone())
        .joining_nets(to_net.clone())
        .parts(from.owner().map(str::to_string))
        .parts(to.owner().map(str::to_string))
        .creating();

    let own: Vec<String> = [from.owner(), to.owner()]
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect();
    let scene = scene(&edit.doc, a, b, &own);
    let drawn = ElbowRouter
        .route_edge(a, dir_a, b, ROUTING_NET, &scene)
        .map(|path| draw(&mut edit.doc, &path))
        // Drawing a path is not the same as making a connection: if the two
        // ends did not end up on one partition, the wire is decoration.
        .filter(|_| joined(&edit.doc, a, b));
    match drawn {
        Some(wires) => {
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
            edit.commit(
                ctx,
                "connect",
                "Connect schematic pins",
                json!(changed),
                allow,
            )
        }
        None => {
            // Nothing orthogonal fits, so join the ends by name instead — the
            // same move a person makes when a wire would be spaghetti. A bare
            // point cannot carry a label, so there is nothing to fall back to.
            if matches!(from, Target::Point(_)) || matches!(to, Target::Point(_)) {
                return Ok(json!({
                    "error": format!(
                        "no clear wire path between {} and {}, and a bare point cannot be \
                         joined by name — connect pins, or make room first",
                        from.describe(),
                        to.describe()
                    ),
                }));
            }
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
                ctx,
                "connect",
                "Connect schematic pins",
                json!(format!("labelled both ends `{net}` — no clear wire path")),
                allow,
            )
        }
    }
}

/// Whether two points sit on the same partition of the sheet.
fn joined(doc: &SchDoc, a: Point2, b: Point2) -> bool {
    let live = connect::scene(doc);
    let name_at = |p: Point2| {
        live.points
            .iter()
            .find(|(q, _)| q.near_eq(p, EPS))
            .map(|(_, name)| name.clone())
    };
    match (name_at(a), name_at(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
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

pub(crate) fn pose(at: Point2) -> sch_doc::Pose {
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
    // `@R1.2` names whatever net that pin is on — the only way to join a net whose
    // own name KiCAD generated.
    let net = &match refs::net_of_pin_name(&edit.doc, edit.before(), net) {
        Ok(resolved) => resolved,
        Err(error) => return Ok(json!({ "error": error })),
    };
    if let Some(error) = refs::derived_name_refusal(edit.before(), net) {
        return Ok(json!({ "error": error }));
    }
    let pin = match refs::pin(&edit.doc, spec) {
        Ok(pin) => pin,
        Err(error) => return Ok(reference_error(&edit.doc, &input, error)),
    };
    let was = refs::net_of(edit.before(), &pin.refdes, &pin.number).map(str::to_string);
    edit.doc.add_label(kind, net, pose(pin.at));
    edit.commit(
        ctx,
        "label",
        "Label a schematic net",
        json!(format!("named {spec} `{net}`")),
        Allow::nothing()
            .joining_nets([net.to_string()])
            .joining_nets(was)
            .part(&pin.refdes)
            .creating(),
    )
}

fn reference_error(doc: &SchDoc, input: &Value, error: String) -> Value {
    let mut response = json!({ "error": error });
    let Some(net) = input.get("net").and_then(Value::as_str) else {
        return response;
    };
    let live = connect::extract(doc);
    if live.nets.iter().any(|candidate| candidate.name == net) {
        return response;
    }
    if let Some(candidate) = sch_check::place_parts::closest_net_name(
        net,
        live.nets.iter().map(|candidate| candidate.name.as_str()),
    ) {
        response["did_you_mean"] = json!({ net: candidate });
    }
    response
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
    if let Some(net) = refs::net_of(edit.before(), &pin.refdes, &pin.number) {
        return Ok(json!({
            "error": format!(
                "{spec} is connected to `{net}`; a no-connect marker on a wired pin is ignored. \
                 Disconnect it first if that is what you meant."
            ),
        }));
    }
    if edit
        .before()
        .no_connect
        .iter()
        .any(|p| p.refdes == pin.refdes && p.pin == pin.number)
    {
        return Ok(json!({ "changed": format!("{spec} was already marked no-connect") }));
    }
    edit.doc.add_no_connect(pin.at);
    edit.commit(
        ctx,
        "no_connect",
        "Mark a schematic pin unconnected",
        json!(format!("marked {spec} no-connect")),
        Allow::nothing().part(&pin.refdes),
    )
}

/// The `power:` symbols that could carry a rail called `net`, best first.
///
/// KiCAD spells a fractional rail two ways — `+3V3` and `+3.3V` — and a caller
/// may write either, with or without the leading `+`. Offer all of them.
fn power_candidates(net: &str) -> Vec<String> {
    let bare = net.trim_start_matches('+');
    let mut names = vec![net.to_string(), bare.to_string(), format!("+{bare}")];
    if let Some((whole, frac)) = bare.trim_end_matches('V').split_once('V') {
        names.push(format!("+{whole}.{frac}V"));
    }
    if let Some((whole, frac)) = bare.trim_end_matches('V').split_once('.') {
        names.push(format!("+{whole}V{frac}"));
    }
    names.dedup();
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
    if was.as_deref() == Some(net) {
        return Ok(json!({
            "changed": format!("{spec} is already on `{net}`; nothing to add"),
            "net_delta": "connectivity unchanged",
        }));
    }
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
    let stub = stand_off(&mut edit.doc, &refdes, &pin);
    edit.commit(
        ctx,
        "add_power",
        "Add a schematic power symbol",
        json!(format!(
            "attached {lib_id} `{net}` to {spec}{}",
            if stub { " through a short wire" } else { "" }
        )),
        Allow::nothing()
            .joining_nets([net.to_string()])
            .joining_nets(was)
            .part(&refdes)
            .part(&pin.refdes)
            .creating(),
    )
}

/// Seat a rail symbol on its pin, backing it off along the pin until its body
/// clears the part it feeds and drawing a stub to bridge the gap.
///
/// A rail dropped straight onto the pin of a diode lands inside the diode's
/// own outline — the pin tip is inside that body — and the two print on top of
/// each other. Returns whether a stub wire was needed.
fn stand_off(doc: &mut SchDoc, refdes: &str, pin: &sch_doc::PlacedPin) -> bool {
    let owner = doc
        .symbol(&pin.owner)
        .and_then(|inst| crate::place::extent(doc, inst));
    let mut at = pin.at;
    for _ in 0..6 {
        align_onto_pin(doc, refdes, at, pin.out);
        let rail = doc
            .symbol_by_ref(refdes)
            .and_then(|inst| crate::place::extent(doc, inst));
        let clashes = match (owner, rail) {
            (Some(owner), Some(rail)) => owner.overlaps(&rail),
            _ => false,
        };
        if !clashes {
            break;
        }
        at = Point2::new(at.x + pin.out.x * 1.27, at.y + pin.out.y * 1.27);
    }
    if at.near_eq(pin.at, EPS) {
        return false;
    }
    doc.add_wire(pin.at, at);
    true
}

/// Rotate and shift a just-placed one-pin symbol so its pin sits exactly on
/// `at`, facing back along `out`.
fn align_onto_pin(doc: &mut SchDoc, refdes: &str, at: Point2, out: Point2) {
    // The rail's pin must point back the way the target pin points out; keep
    // the orientation that does that best rather than the last one tried.
    let mut best = (f64::MAX, 0.0);
    for rot in [0.0, 90.0, 180.0, 270.0] {
        let _ = doc.set_symbol_orientation(refdes, rot, sch_doc::Mirror::None);
        let Some(own) = sch_doc::placed_pins(doc)
            .into_iter()
            .find(|p| p.refdes == refdes)
        else {
            return;
        };
        let alignment = own.out.x * out.x + own.out.y * out.y;
        if alignment < best.0 {
            best = (alignment, rot);
        }
    }
    let _ = doc.set_symbol_orientation(refdes, best.1, sch_doc::Mirror::None);
    let Some(own) = sch_doc::placed_pins(doc)
        .into_iter()
        .find(|p| p.refdes == refdes)
    else {
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

/// Redraw the wires a move left slanting.
///
/// Dragging a symbol carries its wires' endpoints with it, which turns a
/// right-angled route into a diagonal one — the thing that makes a moved part
/// look wrong and sends the model off deleting and re-wiring by hand. Each
/// slanted wire is re-routed; one the router cannot redraw goes back exactly
/// as it was, because a slanted wire still connects.
pub(crate) fn straighten(doc: &mut SchDoc, moved: &[Point2]) -> usize {
    let touches = |p: Point2| moved.iter().any(|q| q.near_eq(p, EPS));
    let slanted: Vec<(String, Point2, Point2)> = doc
        .wires()
        .filter_map(|wire| Some((wire.uuid.clone(), refs::ends(wire)?)))
        .filter(|(_, (a, b))| (a.x - b.x).abs() > EPS && (a.y - b.y).abs() > EPS)
        .filter(|(_, (a, b))| touches(*a) || touches(*b))
        .map(|(uuid, (a, b))| (uuid, a, b))
        .collect();
    let mut redrawn = 0;
    for (uuid, a, b) in slanted {
        let (a, b) = if touches(a) { (a, b) } else { (b, a) };
        let pin = sch_doc::placed_pins(doc).into_iter().find(|p| p.at == a);
        let dir = pin.as_ref().map_or_else(
            || dir_of(Point2::new(b.x - a.x, b.y - a.y)),
            |p| dir_of(p.out),
        );
        let own: Vec<String> = pin.iter().map(|p| p.refdes.clone()).collect();
        doc.remove_drawing(&[uuid]);
        let scene = scene(doc, a, b, &own);
        match ElbowRouter.route_edge(a, dir, b, ROUTING_NET, &scene) {
            Some(path) if joined_after(doc, &path, a, b) => redrawn += 1,
            _ => {
                doc.add_wire(a, b);
            }
        }
    }
    redrawn
}

/// Draw `path` and keep it only if it really joined `a` to `b`.
fn joined_after(doc: &mut SchDoc, path: &[Point2], a: Point2, b: Point2) -> bool {
    let uuids = draw(doc, path);
    if joined(doc, a, b) {
        return true;
    }
    doc.remove_drawing(&uuids);
    false
}

/// Every wire belonging to a run that no longer reaches a pin, a label or a
/// no-connect marker.
///
/// Cutting a net at one pin leaves the rest of that pin's route behind: an
/// L-bend whose far half still sits on the sheet, joined to nothing. KiCAD
/// calls that a dangling-wire *error*, and it carries no connection, so it
/// goes with the cut.
pub(crate) fn floating_wires(doc: &SchDoc) -> Vec<String> {
    let runs: Vec<(String, Segment)> = doc
        .wires()
        .filter_map(|wire| Some((wire.uuid.clone(), refs::ends(wire)?)))
        .map(|(uuid, (a, b))| (uuid, Segment::new(a, b)))
        .collect();
    let mut groups = geom::UnionFind::new(runs.len());
    for (i, (_, one)) in runs.iter().enumerate() {
        for (j, (_, other)) in runs.iter().enumerate().skip(i + 1) {
            let meets = [one.a, one.b].iter().any(|p| other.contains_point(*p))
                || [other.a, other.b].iter().any(|p| one.contains_point(*p));
            if meets {
                groups.union(i, j);
            }
        }
    }
    let mut anchors: Vec<Point2> = sch_doc::placed_pins(doc).iter().map(|p| p.at).collect();
    anchors.extend(doc.labels().map(|l| l.at.point()));
    anchors.extend(doc.items().iter().filter_map(|item| match item {
        sch_doc::Item::NoConnect(marker) => Some(marker.at),
        _ => None,
    }));
    let roots: Vec<usize> = (0..runs.len()).map(|i| groups.find(i)).collect();
    let held: Vec<usize> = runs
        .iter()
        .enumerate()
        .filter(|(_, (_, run))| anchors.iter().any(|p| run.contains_point(*p)))
        .map(|(i, _)| roots[i])
        .collect();
    runs.iter()
        .zip(&roots)
        .filter(|(_, root)| !held.contains(root))
        .map(|((uuid, _), _)| uuid.clone())
        .collect()
}

/// Remove drawn wires by pin, by net, by the parts they touch, or by UUID.
pub fn delete_wires(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let mut edit = Edit::open(ctx)?;
    let live = connect::scene(&edit.doc);
    let wanted_net = input.get("net").and_then(Value::as_str);
    let wanted_refs: Vec<String> = input
        .get("refs")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let wanted_uuids: Vec<String> = input
        .get("uuids")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut wanted_pins: Vec<Point2> = Vec::new();
    let mut pin_owners: Vec<String> = Vec::new();
    for spec in input
        .get("pins")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(Value::as_str)
    {
        match refs::pin(&edit.doc, spec) {
            Ok(pin) => {
                wanted_pins.push(pin.at);
                pin_owners.push(pin.refdes);
            }
            Err(error) => return Ok(json!({ "error": error })),
        }
    }
    if wanted_net.is_none()
        && wanted_refs.is_empty()
        && wanted_uuids.is_empty()
        && wanted_pins.is_empty()
    {
        return Ok(json!({
            "error": "delete_wires needs one of `pins`, `net`, `refs` or `uuids`",
        }));
    }
    let pins = sch_doc::placed_pins(&edit.doc);
    let touches_pin = |a: Point2, b: Point2| {
        wanted_pins
            .iter()
            .any(|p| p.near_eq(a, EPS) || p.near_eq(b, EPS))
    };
    let touches_ref = |a: Point2, b: Point2| {
        pins.iter().any(|p| {
            wanted_refs.contains(&p.refdes) && (p.at.near_eq(a, EPS) || p.at.near_eq(b, EPS))
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
            let Some((a, b)) = refs::ends(wire) else {
                return false;
            };
            wanted_uuids.contains(&wire.uuid)
                || touches_pin(a, b)
                || touches_ref(a, b)
                || wanted_net.is_some_and(|net| net_of_segment(a, b).as_deref() == Some(net))
        })
        .map(|wire| wire.uuid.clone())
        .collect();
    if doomed.is_empty() {
        return Ok(json!({ "changed": "no wire matched", "net_delta": "connectivity unchanged" }));
    }
    let mut removed = edit.doc.remove_drawing(&doomed);
    removed += edit.doc.remove_drawing(&floating_wires(&edit.doc));
    // Deleting copper loosens the pins that shared it, which is the point of
    // the call: every net the request named, and every pin on one, is fair game.
    let mut named: Vec<String> = wanted_refs.clone();
    named.extend(pin_owners);
    let mut nets = refs::nets_touching(edit.before(), &named);
    nets.extend(wanted_net.map(str::to_string));
    let loosened: Vec<String> = edit
        .before()
        .nets
        .iter()
        .filter(|net| nets.contains(&net.name))
        .flat_map(|net| net.pins.iter().map(|p| p.refdes.clone()))
        .collect();
    let allow = Allow::nothing()
        .nets(nets)
        .parts(named)
        .parts(loosened)
        .creating();
    let loose = refs::newly_loose(edit.before(), &connect::extract(&edit.doc));
    let changed = match loose.is_empty() {
        true => format!("deleted {removed} wire(s)"),
        false => format!(
            "deleted {removed} wire(s); these pins are now loose and need reconnecting: {}",
            loose.join(", ")
        ),
    };
    edit.commit(
        ctx,
        "delete_wires",
        "Delete schematic wiring",
        json!(changed),
        allow,
    )
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
) -> Option<(Point2, bool)> {
    let symbol = doc.symbol_by_ref(anchor)?;
    let body = crate::place::extent(doc, symbol)?;
    Occupancy::skipping(doc, skip).beside(body, side, w, h)
}

/// A free spot anywhere, preferring near `from`.
pub(crate) fn spot_near(
    doc: &SchDoc,
    from: Point2,
    w: f64,
    h: f64,
    skip: &[String],
) -> Option<Point2> {
    Occupancy::skipping(doc, skip).nearest_free(snap_point(from), w, h)
}
