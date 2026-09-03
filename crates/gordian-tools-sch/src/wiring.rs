//! Drawing connections. The model never supplies wire coordinates: it names
//! two ends and this module routes between them over the live sheet, or — when
//! nothing orthogonal fits — names the net at both ends instead, and says so.

use anyhow::Result;
use geom::{EPS, Point2, Rect, Segment};
use gordian_runtime::AgentRuntime;
use sch_doc::{LabelKind, SchDoc, connect};
use serde_json::{Value, json};

use crate::place::{Occupancy, snap_point};
use crate::refs::{self, Target};
use crate::session::{Allow, Edit, is_auto, symbol_source};

/// The name a route is drawn under while it is being solved.
///
/// Never the caller's `net`: if the caller says `net: "GND"` the router would
/// treat every scrap of GND copper on the sheet as its own and be free to land
/// on it. Only the two partitions actually being joined get this name.
const ROUTING_NET: &str = "#routing";

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
    Ok(json!({ "connected": done, "failed": failures }))
}

/// Route a connection between two ends of the sheet.
fn connect_one(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    // One end and a net is not a malformed `connect`; it is "put this pin on that
    // net", which is what `label` does. Answering it with an argument complaint cost
    // the campaign runs a request every time they asked.
    if input.get("to").is_none()
        && let Some(pin) = input.get("from").and_then(Value::as_str)
        && let Some(net) = input.get("net").and_then(Value::as_str)
    {
        return label_tool(json!({ "pin": pin, "net": net }), ctx);
    }
    let mut edit = Edit::open(ctx)?;
    let (from, to) = match (input.get("from"), input.get("to")) {
        (Some(from), Some(to)) => (from, to),
        _ => {
            return Ok(json!({
                "error": "connect needs `from` and `to` (two pins), or `from` and `net` to \
                          put one pin on a named net"
            }));
        }
    };
    if let Some(result) = connect_net_endpoint(&edit.doc, edit.before(), from, to, ctx)? {
        return Ok(result);
    }
    let from = match refs::target(&edit.doc, from) {
        Ok(target) => target,
        Err(error) => return Ok(reference_error(&edit.doc, &input, error, ctx)),
    };
    let to = match refs::target(&edit.doc, to) {
        Ok(target) => target,
        Err(error) => return Ok(reference_error(&edit.doc, &input, error, ctx)),
    };
    let requested_net = match input.get("net").and_then(Value::as_str) {
        Some(net) => match resolve_tool_net(&mut edit, net) {
            Ok(resolved) => Some(resolved),
            Err(error) => return Ok(json!({"error": error})),
        },
        None => None,
    };
    let (a, b) = (from.at(), to.at());
    if a.near_eq(b, EPS) {
        return Ok(json!({ "error": "both ends are the same point; they already touch" }));
    }
    // Clearing comes first, before the router looks at the sheet and before the
    // ends' nets are read: a marker severs its point, so a wire drawn to a still
    // marked pin joins nothing and the pin reads as belonging to no net at all.
    let cleared = clear_no_connects(&mut edit.doc, &[(a, from.describe()), (b, to.describe())]);
    let live = connect::extract(&edit.doc);
    let existing = |target: &Target| match target {
        Target::Pin(pin) => refs::net_of(&live, &pin.refdes, &pin.number).map(str::to_string),
        Target::Point(_) => None,
    };
    let (from_net, to_net) = (existing(&from), existing(&to));
    let net = requested_net
        .as_ref()
        .map(|resolved| resolved.name.clone())
        .or_else(|| from_net.clone())
        .or_else(|| to_net.clone());
    let out_a = match &from {
        Target::Pin(pin) => pin.out,
        Target::Point(_) if (b.x - a.x).abs() >= (b.y - a.y).abs() => {
            Point2::new((b.x - a.x).signum(), 0.0)
        }
        Target::Point(_) => Point2::new(0.0, (b.y - a.y).signum()),
    };

    let allow = Allow::nothing()
        .joining_nets(net.clone())
        .joining_endpoints(
            [from_net.as_ref(), to_net.as_ref()]
                .into_iter()
                .flatten()
                .cloned(),
        )
        .parts(from.owner().map(str::to_string))
        .parts(to.owner().map(str::to_string))
        .creating();

    let own: Vec<String> = [from.owner(), to.owner()]
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect();
    let drawn = sch_drag::redraw_wire(&mut edit.doc, a, out_a, b, ROUTING_NET, &own)
        // Drawing a path is not the same as making a connection: if the two
        // ends did not end up on one partition, the wire is decoration.
        .filter(|_| joined(&edit.doc, a, b));
    match drawn {
        Some(redraw) => {
            // An explicitly asked-for name is part of the request, not just a
            // routing hint: give the wire that name unless it already has it.
            if let Some(wanted) = requested_net
                .as_ref()
                .map(|resolved| resolved.name.as_str())
                && from_net.as_deref() != Some(wanted)
                && to_net.as_deref() != Some(wanted)
            {
                let scope = sheet_scope(&edit.doc, wanted).unwrap_or(LabelKind::Local);
                edit.doc.add_label(scope, wanted, pose(a));
            }
            let changed = format!(
                "wired {} to {} with {} segment(s)",
                from.describe(),
                to.describe(),
                redraw.redrawn_segments
            );
            let result = edit.commit(with_cleared(changed, cleared), allow)?;
            Ok(with_resolved_net(
                with_joined(result, from_net, to_net),
                requested_net.as_ref(),
            ))
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
            let scope = sheet_scope(&edit.doc, &net).unwrap_or(LabelKind::Local);
            for target in [&from, &to] {
                edit.doc.add_label(scope, &net, pose(target.at()));
            }
            edit.warn(format!(
                "no clear wire path; {} and {} were joined by a `{net}` label at each end",
                from.describe(),
                to.describe()
            ));
            let result = edit.commit(
                with_cleared(
                    format!("labelled both ends `{net}` — no clear wire path"),
                    cleared,
                ),
                allow,
            )?;
            Ok(with_resolved_net(
                with_joined(result, from_net, to_net),
                requested_net.as_ref(),
            ))
        }
    }
}

/// Interpret one bare string endpoint as the net to put the other pin on.
fn connect_net_endpoint(
    doc: &SchDoc,
    netlist: &sch_doc::Netlist,
    from: &Value,
    to: &Value,
    ctx: &AgentRuntime,
) -> Result<Option<Value>> {
    fn bare(value: &Value) -> Option<&str> {
        value.as_str().filter(|text| !text.contains('.'))
    }
    let (pin, net) = match (bare(from), bare(to)) {
        (None, Some(net)) => (from.as_str(), Some(net)),
        (Some(net), None) => (to.as_str(), Some(net)),
        (Some(_), Some(_)) => {
            return Ok(Some(json!({
                "error": "connect needs at least one pin endpoint; two net names do not identify a connection"
            })));
        }
        (None, None) => return Ok(None),
    };
    let Some(pin) = pin else {
        return Ok(None);
    };
    let net = net.expect("a bare endpoint was matched");
    let existed = refs::net_exists(doc, netlist, net);
    let mut result = label_tool(json!({"pin": pin, "net": net}), ctx)?;
    if result.get("error").is_none() {
        result["net_endpoint"] = json!({
            "net": net,
            "existed": existed,
            "created": !existed,
        });
    }
    Ok(Some(result))
}

/// Add the endpoint partitions and the surviving net to a successful join.
fn with_joined(mut result: Value, from_net: Option<String>, to_net: Option<String>) -> Value {
    if result.get("error").is_some() || from_net == to_net {
        return result;
    }
    let Some((from_net, to_net)) = from_net.zip(to_net) else {
        return result;
    };
    let survivor = result
        .pointer("/net_delta/merged/0/1")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(survivor) = survivor {
        result["joined"] = json!({
            "from_net": from_net,
            "to_net": to_net,
            "survivor": survivor,
        });
    }
    result
}

struct ResolvedToolNet {
    name: String,
    report: Option<(String, String)>,
}

/// Resolve a caller-facing pin-net alias and establish a stable local identity.
fn resolve_tool_net(
    edit: &mut Edit,
    original: &str,
) -> std::result::Result<ResolvedToolNet, String> {
    let Some(derived) = refs::derived_net_ref(&edit.doc, original)? else {
        return refs::net_of_pin_name(&edit.doc, edit.before(), original)
            .map(|name| ResolvedToolNet { name, report: None });
    };
    let found = refs::net_of_pin(&edit.doc, edit.before(), &derived.spec)?;
    if let refs::PinNet::Mint {
        refdes,
        number,
        net,
    } = &found
    {
        let pin = refs::pin(&edit.doc, &format!("{refdes}.{number}"))?;
        edit.doc.add_label(LabelKind::Local, net, pose(pin.at));
        let mut allowed = vec![net.clone()];
        if let Some(was) = refs::net_of(edit.before(), refdes, number) {
            allowed.push(was.to_string());
        }
        edit.joined_nets(allowed);
    }
    Ok(ResolvedToolNet {
        name: found.net().to_string(),
        report: Some((original.to_string(), derived.reported)),
    })
}

fn with_resolved_net(mut result: Value, resolved: Option<&ResolvedToolNet>) -> Value {
    if result.get("error").is_some() {
        return result;
    }
    if let Some((original, pin)) = resolved.and_then(|resolved| resolved.report.as_ref()) {
        result["resolved_nets"] = json!({original: pin});
    }
    result
}

/// The scope the sheet already draws `net` in, if it draws it at all.
///
/// A net has ONE scope on a sheet: adding a plain label to a net the sheet names
/// with a pennant is KiCAD's `same_local_global_label`, and the two do not merge,
/// so the new label would name a different net that happens to read the same.
pub(crate) fn sheet_scope(doc: &SchDoc, net: &str) -> Option<LabelKind> {
    doc.labels()
        .find(|label| sch_doc::unescape(&label.text) == net)
        .map(|label| label.kind)
}

/// Drop the no-connect markers sitting on points a connection just reached,
/// returning the pins that were cleared.
///
/// A marker states that the pin is deliberately left alone. Wiring or naming it
/// makes that statement false, and KiCAD reports the pair as
/// `no_connect_connected` — so whichever call makes the connection is the call
/// that has to take the marker away.
fn clear_no_connects(doc: &mut SchDoc, reached: &[(Point2, String)]) -> Vec<String> {
    let mut doomed = Vec::new();
    let mut cleared = Vec::new();
    for (at, name) in reached {
        let hits: Vec<String> = doc
            .items()
            .iter()
            .filter_map(|item| match item {
                sch_doc::Item::NoConnect(marker) if marker.at.near_eq(*at, EPS) => {
                    Some(marker.uuid.clone())
                }
                _ => None,
            })
            .collect();
        if !hits.is_empty() && !cleared.contains(name) {
            cleared.push(name.clone());
        }
        doomed.extend(hits);
    }
    doc.remove_drawing(&doomed);
    cleared
}

/// Fold the no-connect markers a connection cleared into its `changed` report.
fn with_cleared(changed: String, cleared: Vec<String>) -> Value {
    match cleared.is_empty() {
        true => json!(changed),
        false => json!({ "changed": changed, "removed_no_connects": cleared }),
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
    let (Some(spec), Some(original_net)) = (
        input.get("pin").and_then(Value::as_str),
        input.get("net").and_then(Value::as_str),
    ) else {
        return Ok(json!({ "error": "label needs `pin` and `net`" }));
    };
    let asked = match input.get("kind").and_then(Value::as_str) {
        None => None,
        Some("local") => Some(LabelKind::Local),
        Some("global") => Some(LabelKind::Global),
        Some("hierarchical") => Some(LabelKind::Hier),
        Some(other) => return Ok(json!({ "error": format!("unknown label kind `{other}`") })),
    };
    let mut edit = Edit::open(ctx)?;
    let resolved_net = match resolve_tool_net(&mut edit, original_net) {
        Ok(resolved) => resolved,
        Err(error) => return Ok(json!({ "error": error })),
    };
    let net = &resolved_net.name;
    let pin = match refs::pin(&edit.doc, spec) {
        Ok(pin) => pin,
        Err(error) => return Ok(reference_error(&edit.doc, &input, error, ctx)),
    };
    // Naming a pin connects it, so its marker goes — and it goes BEFORE the pin's
    // net is read, because a marker severs its point and a still-marked pin reads
    // as belonging to nothing. That is how a name landed on a pin that already had
    // one and merged two nets in silence.
    let cleared = clear_no_connects(&mut edit.doc, &[(pin.at, spec.to_string())]);
    let live = connect::extract(&edit.doc);
    let was = refs::net_of(&live, &pin.refdes, &pin.number).map(str::to_string);
    // A label never REPLACES the name a pin already has — it stacks a second one on
    // the same point, which merges the two partitions under whichever name wins. So
    // a pin that already carries an authored name is refused, whether or not the new
    // name is in use. Naming a partition KiCAD named for itself is still fine: that
    // one has no authored name to lose.
    if let Some(was) = was.as_deref()
        && was != net
        && !is_auto(was)
    {
        return Ok(json!({
            "error": format!(
                "{spec} is already on net `{was}`; a label does not replace that name, it \
                 merges `{was}` and `{net}` into one net. Use delete_wires to take {spec} off \
                 `{was}` first, or name a pin that is loose."
            ),
            "fix": {
                "tool": "delete_wires",
                "args": {"pins": [spec]},
            },
        }));
    }
    // A net has one scope on the sheet: a plain label on a net the sheet names with
    // a pennant does not join it, it shadows it. The caller may still say which.
    let kind = asked
        .or_else(|| sheet_scope(&edit.doc, net))
        .unwrap_or(LabelKind::Local);
    edit.doc.add_label(kind, net, pose(pin.at));
    let result = edit.commit(
        with_cleared(format!("named {spec} `{net}`"), cleared),
        Allow::nothing()
            .joining_nets([net.to_string()])
            .joining_nets(was)
            .part(&pin.refdes)
            .creating(),
    )?;
    Ok(with_resolved_net(result, Some(&resolved_net)))
}

fn string_list(input: &Value, key: &str) -> Vec<String> {
    input
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn input_bbox(input: &Value) -> std::result::Result<Option<Rect>, String> {
    let Some(values) = input.get("bbox") else {
        return Ok(None);
    };
    let Some(values) = values.as_array() else {
        return Err("`bbox` must be [x1, y1, x2, y2] in mm".to_string());
    };
    let coordinates = values.iter().filter_map(Value::as_f64).collect::<Vec<_>>();
    if coordinates.len() != 4 {
        return Err("`bbox` must be [x1, y1, x2, y2] in mm".to_string());
    }
    Ok(Some(Rect::from_points(
        Point2::new(coordinates[0], coordinates[1]),
        Point2::new(coordinates[2], coordinates[3]),
    )))
}

/// Remove labels selected by name, UUID, region or the net they name.
pub fn delete_labels(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let names = string_list(&input, "names");
    let uuids = string_list(&input, "uuids");
    let bbox = match input_bbox(&input) {
        Ok(bbox) => bbox,
        Err(error) => return Ok(json!({ "error": error })),
    };
    let wanted_net = input.get("net").and_then(Value::as_str);
    if names.is_empty() && uuids.is_empty() && bbox.is_none() && wanted_net.is_none() {
        return Ok(json!({
            "error": "delete_labels needs one of `names`, `uuids`, `bbox` or `net`",
        }));
    }
    let mut edit = Edit::open(ctx)?;
    let scene = connect::scene(&edit.doc);
    let on_net = |at: Point2, net: &str| {
        scene
            .points
            .iter()
            .any(|(point, name)| name == net && point.near_eq(at, EPS))
    };
    let doomed = edit
        .doc
        .labels()
        .filter(|label| {
            let text = sch_doc::unescape(&label.text);
            names.contains(&text)
                || uuids.contains(&label.uuid)
                || bbox.is_some_and(|bounds| bounds.contains(label.at.point()))
                || wanted_net.is_some_and(|net| text == net || on_net(label.at.point(), net))
        })
        .map(|label| (label.uuid.clone(), sch_doc::unescape(&label.text)))
        .collect::<Vec<_>>();
    if doomed.is_empty() {
        return Ok(json!({
            "changed": {"removed": 0, "labels": []},
            "net_delta": "connectivity unchanged",
        }));
    }
    edit.doc.remove_drawing(
        &doomed
            .iter()
            .map(|(uuid, _)| uuid.clone())
            .collect::<Vec<_>>(),
    );
    let after = connect::extract(&edit.doc);
    let delta = sch_doc::Netlist::diff(edit.before(), &after);
    if let Some((sources, target)) = delta.merged.first() {
        let mut nets = sources.clone();
        nets.push(target.clone());
        nets.sort();
        nets.dedup();
        return Ok(json!({
            "error": format!(
                "refused: deleting those labels would silently merge nets {}; nothing was written",
                nets.join(" and ")
            ),
        }));
    }
    let all_nets = edit
        .before()
        .nets
        .iter()
        .chain(&after.nets)
        .map(|net| net.name.clone())
        .collect::<Vec<_>>();
    let all_refs = edit
        .doc
        .symbols()
        .map(|symbol| symbol.refdes().to_string())
        .collect::<Vec<_>>();
    let now_unconnected = delta
        .pins_now_unconnected
        .iter()
        .map(refs::label)
        .collect::<Vec<_>>();
    let changed = json!({
        "removed": doomed.len(),
        "labels": doomed.iter().map(|(_, name)| name).collect::<Vec<_>>(),
        "nets_renamed": delta.renamed,
        "now_unconnected": now_unconnected,
    });
    edit.commit(
        changed,
        Allow::nothing()
            .joining_nets(all_nets)
            .parts(all_refs)
            .creating(),
    )
}

fn reference_error(doc: &SchDoc, input: &Value, error: String, ctx: &AgentRuntime) -> Value {
    let mut response = json!({ "error": error });
    let suggestions = ["from", "to", "pin"]
        .into_iter()
        .filter_map(|key| input.get(key).and_then(Value::as_str))
        .chain(
            input
                .get("pins")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        )
        .filter_map(|spec| {
            let ranked = refs::pin_suggestions(doc, spec, ctx.env().symbol_dir().to_path_buf());
            (!ranked.is_empty()).then(|| (spec.to_string(), json!(ranked)))
        })
        .collect::<serde_json::Map<String, Value>>();
    if !suggestions.is_empty() {
        response["did_you_mean"] = Value::Object(suggestions);
    }
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
    let mut specs = input
        .get("pins")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if let Some(spec) = input.get("pin").and_then(Value::as_str) {
        specs.push(spec.to_owned());
    }
    specs.sort();
    specs.dedup();
    if specs.is_empty() {
        return Ok(json!({ "error": "no_connect needs `pin` or a non-empty `pins` list" }));
    }
    let mut edit = Edit::open(ctx)?;
    let mut pins = Vec::with_capacity(specs.len());
    let mut nets = Vec::new();
    for spec in &specs {
        let pin = match refs::pin(&edit.doc, spec) {
            Ok(pin) => pin,
            Err(error) => {
                return Ok(reference_error(
                    &edit.doc,
                    &json!({"pin": spec}),
                    error,
                    ctx,
                ));
            }
        };
        if let Some(net) = edit.before().nets.iter().find(|net| {
            net.pins
                .iter()
                .any(|member| member.refdes == pin.refdes && member.pin == pin.number)
        }) {
            if let Some(other) = net
                .pins
                .iter()
                .find(|member| member.refdes != pin.refdes || member.pin != pin.number)
            {
                return Ok(json!({
                    "error": format!(
                        "{spec} is connected to `{}` with {}.{}; a no-connect marker would sever a real net. Disconnect it first if that is what you meant.",
                        net.name, other.refdes, other.pin,
                    ),
                }));
            }
            nets.push(net.name.clone());
        }
        pins.push(pin);
    }

    let mut retracted = NetDrawing::default();
    for net in &nets {
        retracted.extend(drawing_on_net(&edit.doc, net));
    }
    edit.doc.remove_drawing(&retracted.uuids);

    // "Is this pin already marked?" is a question about the drawing, and the
    // drawing is what answers it: a pin whose partition a label happens to name
    // is absent from the extracted `no_connect` list while plainly carrying a
    // marker, and asking the netlist instead put a second marker on top of it.
    let mut marked = Vec::new();
    for pin in &pins {
        let already =
            edit.doc.items().iter().any(
                |item| matches!(item, sch_doc::Item::NoConnect(m) if m.at.near_eq(pin.at, EPS)),
            );
        if already {
            continue;
        }
        edit.doc.add_no_connect(pin.at);
        marked.push(format!("{}.{}", pin.refdes, pin.number));
    }
    let refs = pins
        .iter()
        .map(|pin| pin.refdes.clone())
        .collect::<Vec<_>>();
    edit.commit(
        json!({
            "pins": marked,
            "retracted": {"labels": retracted.labels, "wires": retracted.wires},
        }),
        Allow::nothing().parts(refs).nets(nets).creating(),
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
    let mut candidates = names
        .into_iter()
        .map(|name| format!("power:{name}"))
        .collect::<Vec<_>>();
    candidates.push("power:VDC".to_string());
    candidates
}

/// Drop a rail symbol onto a loose pin, or a PWR_FLAG onto an existing rail.
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
        Err(error) => return Ok(reference_error(&edit.doc, &input, error, ctx)),
    };
    // Seating a rail on a pin connects it, so the pin's marker goes with the same
    // rule `connect` and `label` follow — and goes before its net is read.
    let cleared = clear_no_connects(&mut edit.doc, &[(pin.at, spec.to_string())]);
    let live = match cleared.is_empty() {
        true => edit.before().clone(),
        false => connect::extract(&edit.doc),
    };
    let was = refs::net_of(&live, &pin.refdes, &pin.number).map(str::to_string);
    let source = symbol_source(ctx);
    let candidates: Vec<String> = match input.get("lib_id").and_then(Value::as_str) {
        Some(lib_id) => vec![lib_id.to_string()],
        None if was.as_deref() == Some(net) => vec!["power:PWR_FLAG".to_string()],
        None => power_candidates(net),
    };
    let prefix = if candidates.first().is_some_and(|id| id == "power:PWR_FLAG") {
        "#FLG"
    } else {
        "#PWR"
    };
    let refdes = crate::edit::next_refdes(&edit, prefix);
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
    let redraw = stand_off(&mut edit.doc, &refdes, &pin, net);
    if redraw.labels_added > 0 {
        edit.warn(format!(
            "pin re-seat debit: {} labels added because no clean orthogonal power stub fit",
            redraw.labels_added
        ));
    }
    let mut result = edit.commit(
        with_cleared(
            format!(
                "attached {lib_id} `{net}` to {spec}{}",
                if redraw.labels_added > 0 {
                    " by matched labels"
                } else if redraw.redrawn_segments > 0 {
                    " through an orthogonal wire"
                } else {
                    ""
                }
            ),
            cleared,
        ),
        Allow::nothing()
            .joining_nets([net.to_string()])
            .joining_nets(was)
            .part(&refdes)
            .part(&pin.refdes)
            .creating(),
    )?;
    if result.get("error").is_none() {
        result["power_symbol_used"] = json!(lib_id);
    }
    Ok(result)
}

/// Seat a rail symbol on its pin, backing it off along the pin until its body
/// clears the part it feeds and drawing a stub to bridge the gap.
///
/// A rail dropped straight onto the pin of a diode lands inside the diode's
/// own outline — the pin tip is inside that body — and the two print on top of
/// each other. The report records either the orthogonal route or its label
/// fallback.
fn stand_off(
    doc: &mut SchDoc,
    refdes: &str,
    pin: &sch_doc::PlacedPin,
    net: &str,
) -> sch_drag::DragReport {
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
        return sch_drag::DragReport::default();
    }
    let Some(rail) = sch_doc::placed_pins(doc)
        .into_iter()
        .find(|placed| placed.refdes == refdes)
    else {
        return sch_drag::DragReport::default();
    };
    if let Some(report) = sch_drag::redraw_wire(
        doc,
        pin.at,
        pin.out,
        rail.at,
        ROUTING_NET,
        &[pin.refdes.clone(), refdes.to_string()],
    ) {
        return report;
    }
    let kind = sheet_scope(doc, net).unwrap_or(LabelKind::Global);
    doc.add_label(kind, net, pose(pin.at));
    doc.add_label(kind, net, pose(rail.at));
    sch_drag::DragReport {
        labels_added: 2,
        ..sch_drag::DragReport::default()
    }
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
    anchors.extend(doc.items().iter().flat_map(|item| match item {
        sch_doc::Item::NoConnect(marker) => vec![marker.at],
        // A sheet pin is a connection point no symbol owns; a run that reaches one
        // is held by it, not floating.
        sch_doc::Item::Sheet(sheet) => sheet.pins.iter().map(|pin| pin.at.point()).collect(),
        _ => Vec::new(),
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

#[derive(Default)]
struct NetDrawing {
    uuids: Vec<String>,
    labels: usize,
    wires: usize,
    junctions: usize,
}

impl NetDrawing {
    fn extend(&mut self, other: NetDrawing) {
        for uuid in other.uuids {
            if !self.uuids.contains(&uuid) {
                self.uuids.push(uuid);
            }
        }
        self.labels += other.labels;
        self.wires += other.wires;
        self.junctions += other.junctions;
    }
}

/// Every wire, label and junction whose extracted partition carries `net`.
fn drawing_on_net(doc: &SchDoc, net: &str) -> NetDrawing {
    let live = connect::scene(doc);
    let point_is_on_net = |at: Point2| {
        live.points
            .iter()
            .any(|(point, name)| name == net && point.near_eq(at, EPS))
    };
    let segment_is_on_net = |a: Point2, b: Point2| {
        live.segments.iter().any(|(from, to, name)| {
            name == net
                && ((from.near_eq(a, EPS) && to.near_eq(b, EPS))
                    || (from.near_eq(b, EPS) && to.near_eq(a, EPS)))
        })
    };
    let mut drawing = NetDrawing::default();
    for item in doc.items() {
        let uuid = match item {
            sch_doc::Item::Wire(wire)
                if wire
                    .points
                    .windows(2)
                    .any(|pair| segment_is_on_net(pair[0], pair[1])) =>
            {
                drawing.wires += 1;
                Some(&wire.uuid)
            }
            sch_doc::Item::Label(label)
                if sch_doc::unescape(&label.text) == net || point_is_on_net(label.at.point()) =>
            {
                drawing.labels += 1;
                Some(&label.uuid)
            }
            sch_doc::Item::Junction(junction) if point_is_on_net(junction.at) => {
                drawing.junctions += 1;
                Some(&junction.uuid)
            }
            _ => None,
        };
        if let Some(uuid) = uuid {
            drawing.uuids.push(uuid.clone());
        }
    }
    drawing
}

/// Remove drawn wires by pin, by net, by the parts they touch, or by UUID.
pub fn delete_wires(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let mut edit = Edit::open(ctx)?;
    let live = connect::scene(&edit.doc);
    let wanted_bbox = match input_bbox(&input) {
        Ok(bbox) => bbox,
        Err(error) => return Ok(json!({"error": error})),
    };
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
        && wanted_bbox.is_none()
    {
        return Ok(json!({
            "error": "delete_wires needs one of `pins`, `net`, `refs`, `uuids` or `bbox`",
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
            .find(|(from, to, _)| {
                from.near_eq(a, EPS) && to.near_eq(b, EPS)
                    || from.near_eq(b, EPS) && to.near_eq(a, EPS)
            })
            .map(|(_, _, name)| name.clone())
    };
    let mut doomed: Vec<String> = edit
        .doc
        .wires()
        .filter(|wire| {
            let Some((a, b)) = refs::ends(wire) else {
                return false;
            };
            wanted_uuids.contains(&wire.uuid)
                || touches_pin(a, b)
                || touches_ref(a, b)
                || wanted_bbox.is_some_and(|bounds| Segment::new(a, b).dist_to_rect(&bounds) <= EPS)
                || wanted_net.is_some_and(|net| net_of_segment(a, b).as_deref() == Some(net))
        })
        .map(|wire| wire.uuid.clone())
        .collect();
    if let Some(net) = wanted_net {
        doomed.extend(drawing_on_net(&edit.doc, net).uuids);
    }
    doomed.extend(
        edit.doc
            .labels()
            .filter(|label| {
                wanted_pins
                    .iter()
                    .any(|pin| pin.near_eq(label.at.point(), EPS))
            })
            .map(|label| label.uuid.clone()),
    );
    doomed.sort();
    doomed.dedup();
    if doomed.is_empty() {
        return Ok(json!({ "changed": "no wire matched", "net_delta": "connectivity unchanged" }));
    }
    let selected_segments = edit
        .doc
        .wires()
        .filter(|wire| doomed.contains(&wire.uuid))
        .flat_map(|wire| wire.points.windows(2).map(|pair| (pair[0], pair[1])))
        .collect::<Vec<_>>();
    let mut selected_nets = live
        .segments
        .iter()
        .filter(|(a, b, _)| {
            selected_segments.iter().any(|(from, to)| {
                from.near_eq(*a, EPS) && to.near_eq(*b, EPS)
                    || from.near_eq(*b, EPS) && to.near_eq(*a, EPS)
            })
        })
        .map(|(_, _, net)| net.clone())
        .collect::<std::collections::BTreeSet<_>>();
    for item in edit.doc.items() {
        let point = match item {
            sch_doc::Item::Label(label) if doomed.contains(&label.uuid) => Some(label.at.point()),
            sch_doc::Item::Junction(junction) if doomed.contains(&junction.uuid) => {
                Some(junction.at)
            }
            _ => None,
        };
        if let Some(point) = point {
            selected_nets.extend(
                live.points
                    .iter()
                    .filter(|(at, _)| at.near_eq(point, EPS))
                    .map(|(_, net)| net.clone()),
            );
        }
    }
    let affected_pins = edit
        .before()
        .nets
        .iter()
        .filter(|net| selected_nets.contains(&net.name))
        .flat_map(|net| net.pins.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>();
    // The endpoints the cut is about to leave in the air. Deleting the middle of a
    // run leaves the surviving half anchored at one end and dangling at the other,
    // which is the same `unconnected_wire_endpoint` a removed pin leaves behind.
    let cut: Vec<Point2> = edit
        .doc
        .wires()
        .filter(|wire| doomed.contains(&wire.uuid))
        .filter_map(refs::ends)
        .flat_map(|(a, b)| [a, b])
        .collect();
    let mut removed = edit.doc.remove_drawing(&doomed);
    removed += crate::edit::retract_stubs(&mut edit.doc, &cut);
    removed += edit.doc.remove_drawing(&floating_wires(&edit.doc));
    let after = connect::extract(&edit.doc);
    selected_nets.extend(
        after
            .nets
            .iter()
            .filter(|net| net.pins.iter().any(|pin| affected_pins.contains(pin)))
            .map(|net| net.name.clone()),
    );
    // Deleting copper loosens the pins that shared it, which is the point of
    // the call: every net the request named, and every pin on one, is fair game.
    let mut named: Vec<String> = wanted_refs.clone();
    named.extend(pin_owners);
    named.extend(affected_pins.iter().map(|pin| pin.refdes.clone()));
    let mut nets = refs::nets_touching(edit.before(), &named);
    nets.extend(wanted_net.map(str::to_string));
    nets.extend(selected_nets);
    let loosened: Vec<String> = edit
        .before()
        .nets
        .iter()
        .filter(|net| nets.contains(&net.name))
        .flat_map(|net| net.pins.iter().map(|p| p.refdes.clone()))
        .collect();
    let allow = Allow::nothing()
        .unname_nets(nets)
        .parts(named)
        .parts(loosened)
        .creating();
    let loose = refs::newly_loose(edit.before(), &after);
    let changed = match loose.is_empty() {
        true => format!("deleted {removed} wiring item(s)"),
        false => format!(
            "deleted {removed} wiring item(s); these pins are now loose and need reconnecting: {}",
            loose.join(", ")
        ),
    };
    edit.commit(json!(changed), allow)
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
