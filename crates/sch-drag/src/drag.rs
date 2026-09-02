//! The drag primitive: move, rotate or mirror one symbol and carry its
//! connections, the way a schematic editor does under the mouse.
//!
//! Three phases. **Retract** peels each attached wire back from the pin to the
//! first point that is holding something else — a junction, a label, another
//! part's pin, a branch — exactly as a rubber-band drag does. **Re-pose** moves
//! the symbol and carries whatever was sitting on a pin. **Re-draw** routes
//! each pin back to the nearest surviving point of its own net.
//!
//! Then it checks its work: the extracted net partition after the drag must
//! equal the one before, or the document is rolled back and the drag reported
//! as impossible. Nothing here can silently unwire a board.

use std::collections::{HashMap, HashSet};

use geom::Point2;
use sch_doc::{LabelKind, Mirror, Pose, SchDoc, connect};

use crate::eval;
use crate::route::{self, Obstacles, Path};
use crate::sheet::{NodeKey, Sheet, key};

/// Where a symbol sits and how it is turned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub at: Point2,
    pub rot: f64,
    pub mirror: Mirror,
}

impl Placement {
    pub fn new(at: Point2, rot: f64, mirror: Mirror) -> Placement {
        Placement { at, rot, mirror }
    }

    /// The placement a symbol currently has.
    pub fn of(doc: &SchDoc, id: &str) -> Option<Placement> {
        let s = doc.symbol_by_ref(id).or_else(|| doc.symbol(id))?;
        Some(Placement {
            at: s.at.point(),
            rot: s.at.rot,
            mirror: s.mirror,
        })
    }
}

/// What one drag did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DragReport {
    pub moved: String,
    pub redrawn_segments: usize,
    pub labels_added: usize,
    /// Change in wire–wire crossings across the whole sheet, which is how a
    /// locally tidy drag betrays that it made a mess somewhere else.
    pub crossings_added: i64,
}

/// Why a drag could not be made.
#[derive(Debug, Clone, PartialEq)]
pub enum DragError {
    /// No symbol carries this reference or UUID.
    Unknown(String),
    /// The move would have changed the netlist; the document was rolled back.
    /// Carries the nets that differed, for the report.
    Truthfulness(Vec<String>),
    /// The document rejected the edit.
    Doc(String),
}

impl std::fmt::Display for DragError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DragError::Unknown(id) => write!(f, "no symbol {id}"),
            DragError::Truthfulness(nets) => {
                write!(f, "drag would change the netlist: {}", nets.join(", "))
            }
            DragError::Doc(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DragError {}

/// Where a retracted stub has to reconnect.
#[derive(Debug, Clone)]
enum Anchor {
    /// A point that is not moving.
    Fixed(Point2),
    /// Another pin of the symbol being dragged, by pin number.
    OwnPin(String),
    /// The stub was dangling; nothing has to be reconnected.
    Dangling,
}

#[derive(Debug, Clone)]
struct Stub {
    pin: String,
    anchor: Anchor,
}

/// Peel every wire chain attached to this symbol's pins back to the first point
/// that holds something else.
fn retract(sheet: &Sheet, uuid: &str) -> (Vec<String>, Vec<Stub>) {
    let own: HashMap<NodeKey, String> = sheet
        .pins_of(uuid)
        .map(|p| (key(p.at), p.number.clone()))
        .collect();
    let mut removed: HashSet<String> = HashSet::new();
    let mut taken: HashSet<usize> = HashSet::new();
    let mut stubs = Vec::new();

    for pin in sheet.pins_of(uuid) {
        let start = key(pin.at);
        let Some(incident) = sheet.incident.get(&start) else {
            continue;
        };
        for &first in incident {
            if taken.contains(&first) {
                continue;
            }
            let mut node = start;
            let mut edge = first;
            let anchor = loop {
                let seg = &sheet.wires[edge];
                if seg.polyline {
                    break Anchor::Fixed(point_of(node));
                }
                taken.insert(edge);
                removed.insert(seg.uuid.clone());
                let next = if key(seg.a) == node {
                    key(seg.b)
                } else {
                    key(seg.a)
                };
                if let Some(number) = own.get(&next) {
                    break Anchor::OwnPin(number.clone());
                }
                let degree = sheet.incident.get(&next).map_or(0, Vec::len);
                let held = sheet.fixtures.contains(&next) || sheet.pins_at.contains_key(&next);
                if held {
                    break Anchor::Fixed(point_of(next));
                }
                if degree == 1 {
                    break Anchor::Dangling;
                }
                if degree != 2 {
                    break Anchor::Fixed(point_of(next));
                }
                let Some(onward) = sheet.incident[&next].iter().copied().find(|w| *w != edge)
                else {
                    break Anchor::Dangling;
                };
                if taken.contains(&onward) {
                    break Anchor::Fixed(point_of(next));
                }
                node = next;
                edge = onward;
            };
            stubs.push(Stub {
                pin: pin.number.clone(),
                anchor,
            });
        }
    }
    (removed.into_iter().collect(), stubs)
}

/// Single-pin symbols welded straight onto one of this symbol's pins — power
/// flags, rail markers, test points. They have no wire to retract, so a drag
/// that left them behind would drop the pin off its rail.
///
/// Returned as `(symbol uuid, the moved symbol's pin number it rides on)`.
fn glued_symbols(sheet: &Sheet, uuid: &str) -> Vec<(String, String)> {
    let own: HashMap<NodeKey, String> = sheet
        .pins_of(uuid)
        .map(|p| (key(p.at), p.number.clone()))
        .collect();
    let mut pin_count: HashMap<&str, usize> = HashMap::new();
    for pin in &sheet.pins {
        *pin_count.entry(pin.owner.as_str()).or_default() += 1;
    }
    let mut glued: Vec<(String, String)> = sheet
        .pins
        .iter()
        .filter(|p| p.owner != uuid && pin_count[p.owner.as_str()] == 1)
        .filter_map(|p| Some((p.owner.clone(), own.get(&key(p.at))?.clone())))
        .collect();
    glued.sort();
    glued.dedup();
    glued
}

fn point_of(k: NodeKey) -> Point2 {
    Point2::new(k.0 as f64 / 1000.0, k.1 as f64 / 1000.0)
}

/// Points on `net` a re-drawn wire may land on, nearest first.
///
/// Existing connection points come first; a perpendicular tap onto a same-net
/// wire is offered too, because that is how a rail actually gets used.
fn targets(sheet: &Sheet, from: Point2, net: &str) -> Vec<(Point2, bool)> {
    let mut out: Vec<(Point2, bool)> = Vec::new();
    for (node, name) in &sheet.node_net {
        if name == net {
            out.push((point_of(*node), false));
        }
    }
    for wire in &sheet.wires {
        if wire.net != net {
            continue;
        }
        let foot = if wire.horizontal() {
            Point2::new(
                route::snap(from.x.clamp(wire.a.x.min(wire.b.x), wire.a.x.max(wire.b.x))),
                wire.a.y,
            )
        } else {
            Point2::new(
                wire.a.x,
                route::snap(from.y.clamp(wire.a.y.min(wire.b.y), wire.a.y.max(wire.b.y))),
            )
        };
        if !sheet.node_net.contains_key(&key(foot)) {
            out.push((foot, true));
        }
    }
    out.sort_by(|a, b| {
        from.manhattan(a.0)
            .partial_cmp(&from.manhattan(b.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(24);
    out
}

/// A name no label on the sheet is using.
fn fresh_name(sheet: &Sheet) -> String {
    let taken = sheet.label_texts();
    (1..)
        .map(|n| format!("D{n}"))
        .find(|c| !taken.contains(c.as_str()))
        .expect("unbounded")
}

fn label_rot(out: Point2) -> f64 {
    if out.x.abs() > out.y.abs() {
        if out.x > 0.0 { 0.0 } else { 180.0 }
    } else if out.y < 0.0 {
        90.0
    } else {
        270.0
    }
}

/// Move a symbol to `to`, carrying its connections.
///
/// The netlist is re-extracted afterwards and compared with the one before; a
/// drag that would change it is rolled back and returned as
/// [`DragError::Truthfulness`].
pub fn drag(doc: &mut SchDoc, id: &str, to: Placement) -> Result<DragReport, DragError> {
    let uuid = doc
        .symbol_by_ref(id)
        .or_else(|| doc.symbol(id))
        .map(|s| s.uuid.clone())
        .ok_or_else(|| DragError::Unknown(id.to_string()))?;

    let backup = doc.clone();
    let before_list = connect::extract(doc);
    let before = before_list.partition();
    let before_sheet = Sheet::of(doc);
    let crossings_before = eval::crossings(&before_sheet) as i64;

    let old_pins: HashMap<String, Point2> = before_sheet
        .pins_of(&uuid)
        .map(|p| (p.number.clone(), p.at))
        .collect();
    let (removed, stubs) = retract(&before_sheet, &uuid);
    let glued = glued_symbols(&before_sheet, &uuid);

    doc.remove_drawing(&removed);
    doc.move_symbol(&uuid, to.at.x, to.at.y)
        .and_then(|()| doc.set_symbol_orientation(&uuid, to.rot, to.mirror))
        .map_err(|e| DragError::Doc(e.to_string()))?;

    let after_move = Sheet::of(doc);
    let new_pins: HashMap<String, (Point2, Point2)> = after_move
        .pins_of(&uuid)
        .map(|p| (p.number.clone(), (p.at, p.out)))
        .collect();

    // Whatever was sitting on a pin — a label, a junction, a no-connect — rides
    // along, or the symbol would leave its own connection behind.
    let carried: Vec<(Point2, Point2)> = old_pins
        .iter()
        .filter(|(number, _)| new_pins.contains_key(*number))
        .map(|(number, old)| (*old, new_pins[number].0))
        .collect();
    doc.move_attached_many(&carried);

    // A power flag welded straight onto a pin has no wire to retract, so it has
    // to travel with the pin or the rail is left behind.
    for (glued_uuid, pin) in &glued {
        let Some((new_at, _)) = new_pins.get(pin) else {
            continue;
        };
        let old_at = old_pins[pin];
        let Some(symbol) = doc.symbol(glued_uuid) else {
            continue;
        };
        let (x, y) = (
            symbol.at.x + new_at.x - old_at.x,
            symbol.at.y + new_at.y - old_at.y,
        );
        doc.move_symbol(glued_uuid, x, y)
            .map_err(|e| DragError::Doc(e.to_string()))?;
    }

    let report = redraw(doc, &stubs, &new_pins)?;

    let after = connect::extract(doc);
    if after.partition() != before {
        let broken = broken_nets(&before_list, &after);
        *doc = backup;
        return Err(DragError::Truthfulness(broken));
    }

    let crossings_after = eval::crossings(&Sheet::of(doc)) as i64;
    Ok(DragReport {
        moved: id.to_string(),
        crossings_added: crossings_after - crossings_before,
        ..report
    })
}

/// Names of the nets whose pin membership differs between two extractions.
fn broken_nets(before: &connect::Netlist, after: &connect::Netlist) -> Vec<String> {
    let key_of = |net: &connect::Net| {
        let mut pins: Vec<String> = net
            .pins
            .iter()
            .map(|p| format!("{}.{}", p.refdes, p.pin))
            .collect();
        pins.sort();
        pins
    };
    let old: HashSet<Vec<String>> = before.nets.iter().map(key_of).collect();
    let mut broken: Vec<String> = before
        .nets
        .iter()
        .filter(|n| !after.nets.iter().any(|m| key_of(m) == key_of(n)))
        .map(|n| n.name.clone())
        .chain(
            after
                .nets
                .iter()
                .filter(|n| !old.contains(&key_of(n)))
                .map(|n| n.name.clone()),
        )
        .collect();
    broken.sort();
    broken.dedup();
    broken.truncate(8);
    broken
}

/// Re-draw every retracted stub, falling back to a matched pair of labels where
/// no clean route exists.
fn redraw(
    doc: &mut SchDoc,
    stubs: &[Stub],
    new_pins: &HashMap<String, (Point2, Point2)>,
) -> Result<DragReport, DragError> {
    let sheet = Sheet::of(doc);
    let mut obstacles = Obstacles::new(&sheet, &[]);
    let mut drawn: Vec<(Path, Point2, bool)> = Vec::new();
    let mut fallbacks: Vec<(Point2, Point2, Point2)> = Vec::new();

    for stub in stubs {
        let Some((from, out)) = new_pins.get(&stub.pin).copied() else {
            continue;
        };
        let (target, net) = match &stub.anchor {
            Anchor::Dangling => continue,
            Anchor::OwnPin(other) => {
                let Some((p, _)) = new_pins.get(other).copied() else {
                    continue;
                };
                (p, sheet.net_at(from).unwrap_or_default())
            }
            Anchor::Fixed(p) => {
                let Some(net) = sheet.net_at(*p) else {
                    continue;
                };
                (*p, net)
            }
        };
        if from.near_eq(target, geom::EPS) {
            continue;
        }
        let best = best_route(&obstacles, &sheet, from, out, target, net, &stub.anchor);
        match best {
            Some((path, tap)) => {
                obstacles.add_path(&path, net);
                drawn.push((path, target, tap));
            }
            None => fallbacks.push((from, out, target)),
        }
    }

    let mut redrawn_segments = 0;
    let mut ends: Vec<Point2> = Vec::new();
    for (path, _, tap) in &drawn {
        for pair in path.windows(2) {
            doc.add_wire(pair[0], pair[1]);
            redrawn_segments += 1;
        }
        let end = *path.last().expect("route has an end");
        if *tap {
            doc.add_junction(end);
        } else {
            ends.push(end);
        }
    }
    // Three wire ends meeting need the dot KiCAD draws for them.
    let after = Sheet::of(doc);
    for end in ends {
        let k = key(end);
        if after.incident.get(&k).map_or(0, Vec::len) >= 3 && !after.fixtures.contains(&k) {
            doc.add_junction(end);
        }
    }

    let mut labels_added = 0;
    for (from, out, target) in fallbacks {
        let sheet = Sheet::of(doc);
        let name = match sheet.label_names.get(&key(target)) {
            Some(existing) => existing.clone(),
            None => {
                let name = fresh_name(&sheet);
                doc.add_label(LabelKind::Local, &name, Pose::new(target.x, target.y, 0.0));
                labels_added += 1;
                name
            }
        };
        let stub_end = route::snap_point(Point2::new(from.x + out.x * 2.54, from.y + out.y * 2.54));
        doc.add_wire(from, stub_end);
        redrawn_segments += 1;
        doc.add_label(
            LabelKind::Local,
            &name,
            Pose::new(stub_end.x, stub_end.y, label_rot(out)),
        );
        labels_added += 1;
    }

    Ok(DragReport {
        redrawn_segments,
        labels_added,
        ..Default::default()
    })
}

/// The cheapest legal route from a pin, over the anchor and every other point
/// of the same net.
fn best_route(
    obstacles: &Obstacles,
    sheet: &Sheet,
    from: Point2,
    out: Point2,
    anchor: Point2,
    net: &str,
    kind: &Anchor,
) -> Option<(Path, bool)> {
    let mut candidates: Vec<(Point2, bool)> = vec![(anchor, false)];
    if !matches!(kind, Anchor::OwnPin(_)) {
        candidates.extend(targets(sheet, from, net));
    }
    let mut best: Option<(Path, bool, f64)> = None;
    for (target, tap) in candidates {
        let Some(path) = route::route(obstacles, from, out, target, net) else {
            continue;
        };
        let cost = route::path_cost(&path, out) + if tap { 3.0 } else { 0.0 };
        if best.as_ref().is_none_or(|(_, _, c)| cost < *c) {
            best = Some((path, tap, cost));
        }
    }
    best.map(|(path, tap, _)| (path, tap))
}
