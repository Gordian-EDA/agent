//! The drag primitive: move, rotate or mirror symbols and carry their
//! connections, the way a schematic editor does under the mouse.
//!
//! Three phases. **Retract** peels each attached wire back from the pin to the
//! first point that is holding something else — a junction, a label, another
//! part's pin, a branch — exactly as a rubber-band drag does. **Re-pose** moves
//! the symbols and carries whatever was sitting on a pin, welded power flags
//! included. **Re-draw** routes each pin back to the nearest surviving point of
//! its own net and puts the connection dots where they now belong.
//!
//! Then it checks its work against two invariants, and rolls the document back
//! if either breaks: the net partition must be unchanged, and no pin it moved
//! may be left with nothing drawn on it. The second one is the one that matters
//! — on a sheet wired with labels, a dropped rail wire changes no netlist at
//! all, and the drawing still shows a part connected to thin air.

use std::collections::{HashMap, HashSet};

use geom::Point2;
use sch_doc::{Item, LabelKind, Mirror, Pose, SchDoc};

use crate::route::{self, Obstacles, Path};
use crate::sheet::{NodeKey, Sheet, key, point_of};

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

    /// The placement a symbol currently has. `id` is a UUID, or a reference
    /// designator when only one symbol carries it.
    pub fn of(doc: &SchDoc, id: &str) -> Option<Placement> {
        let s = doc.symbol(id).or_else(|| doc.symbol_by_ref(id))?;
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
    pub moved: Vec<String>,
    pub redrawn_segments: usize,
    /// Connections the router could not draw, left as a matched pair of labels.
    pub labels_added: usize,
    /// New perpendicular wire crossings introduced by the drag.
    pub crossings_added: usize,
    pub junctions_added: usize,
    pub junctions_removed: usize,
}

/// What an in-place turn deliberately reassigned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnReport {
    /// The symbol reference or UUID passed to [`turn_in_place`].
    pub turned: String,
    /// Each pin whose old net is now occupied by another pin.
    pub pins_swapped: Vec<(String, String)>,
}

/// Why a requested in-place turn could not be made.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnError {
    /// No symbol carries this reference or UUID.
    Unknown(String),
    /// The requested pose moves the symbol anchor by this offset.
    AnchorOffset(Point2),
    /// This pin misses every old pin position by the reported nearest offset.
    PinOffset { pin: String, offset: Point2 },
    /// The document rejected the orientation.
    Doc(String),
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TurnError::Unknown(id) => write!(f, "no symbol {id}"),
            TurnError::AnchorOffset(offset) => write!(
                f,
                "the symbol anchor would move by [{:.2}, {:.2}] mm",
                offset.x, offset.y
            ),
            TurnError::PinOffset { pin, offset } => write!(
                f,
                "pin {pin} would miss every old pin position by offset [{:.2}, {:.2}] mm",
                offset.x, offset.y
            ),
            TurnError::Doc(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for TurnError {}

/// Why a drag could not be made.
#[derive(Debug, Clone, PartialEq)]
pub enum DragError {
    /// No symbol carries this reference or UUID.
    Unknown(String),
    /// The move would have changed the netlist; the document was rolled back.
    /// Carries the nets that differed, for the report.
    Truthfulness(Vec<String>),
    /// The move would have left a pin with nothing drawn on it and no name to
    /// carry the connection — the netlist would still read correctly and the
    /// drawing would be a lie.
    Disconnection(usize),
    /// The move would have left more wire ends hanging in space, or more names
    /// speaking for nothing, than it found.
    Litter(usize),
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
            DragError::Disconnection(n) => write!(f, "drag would break {n} drawn connections"),
            DragError::Litter(n) => write!(f, "drag would leave {n} loose ends behind"),
            DragError::Doc(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DragError {}

/// A pin of a symbol being dragged.
type PinId = (String, String);

/// One old pin identity and the pin that replaces it after a symbol edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinReSeat {
    /// UUID of the symbol that owned the old pin.
    pub old_owner: String,
    /// Physical number of the old pin.
    pub old_number: String,
    /// UUID of the symbol that owns the replacement pin.
    pub new_owner: String,
    /// Physical number of the replacement pin.
    pub new_number: String,
}

impl PinReSeat {
    /// Describe a pin replacement by owner UUID and physical pin number.
    pub fn new(
        old_owner: impl Into<String>,
        old_number: impl Into<String>,
        new_owner: impl Into<String>,
        new_number: impl Into<String>,
    ) -> PinReSeat {
        PinReSeat {
            old_owner: old_owner.into(),
            old_number: old_number.into(),
            new_owner: new_owner.into(),
            new_number: new_number.into(),
        }
    }
}

/// Where a retracted stub has to reconnect.
#[derive(Debug, Clone)]
enum Anchor {
    /// A point that is not moving.
    Fixed(Point2),
    /// A pin of a symbol this same drag is moving.
    OwnPin(PinId),
    /// The stub was dangling; nothing has to be reconnected.
    Dangling,
}

#[derive(Debug, Clone)]
struct Stub {
    pin: PinId,
    /// The net the pin was on before the drag. The anchor's own name is gone
    /// once the retraction has erased it, and this is what says what to re-draw.
    net: String,
    anchor: Anchor,
}

/// What a retraction leaves behind: the wires to delete, the pieces of a cut
/// wire to keep, and where each pin now has to reconnect.
struct Retraction {
    removed: Vec<String>,
    kept: Vec<(Point2, Point2)>,
    stubs: Vec<Stub>,
}

/// Peel every wire chain attached to these symbols' pins back to the first
/// point that holds something else.
fn retract(sheet: &Sheet, moving: &HashSet<PinId>) -> Retraction {
    let own: HashMap<NodeKey, PinId> = sheet
        .pins
        .iter()
        .filter(|p| moving.contains(&(p.owner.clone(), p.number.clone())))
        .map(|p| (key(p.at), (p.owner.clone(), p.number.clone())))
        .collect();
    let mut removed: HashSet<String> = HashSet::new();
    let mut taken: HashSet<usize> = HashSet::new();
    let mut kept: Vec<(Point2, Point2)> = Vec::new();
    let mut stubs = Vec::new();
    // Anything attached part-way along a wire — a dot, a sheet pin, a label, a
    // no-connect — is a connection the wire is holding up, so the rubber band
    // stops at the nearest one and the far half stays drawn.
    let tapped = |seg: &crate::sheet::WireSeg, from: Point2| {
        let mut taps: Vec<Point2> = sheet
            .fixtures
            .iter()
            .map(point_of)
            .filter(|p| {
                let inside =
                    |v: f64, a: f64, b: f64| v > a.min(b) + geom::EPS && v < a.max(b) - geom::EPS;
                if seg.horizontal() {
                    (seg.a.y - p.y).abs() < geom::EPS && inside(p.x, seg.a.x, seg.b.x)
                } else {
                    (seg.a.x - p.x).abs() < geom::EPS && inside(p.y, seg.a.y, seg.b.y)
                }
            })
            .collect();
        taps.sort_by(|a, b| {
            from.manhattan(*a)
                .partial_cmp(&from.manhattan(*b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        taps.first().copied()
    };

    for pin in sheet
        .pins
        .iter()
        .filter(|p| moving.contains(&(p.owner.clone(), p.number.clone())))
    {
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
                taken.insert(edge);
                removed.insert(seg.uuid.clone());
                // A wire KiCAD wrote with more than two points goes as one item,
                // so the parts of it this drag is not cutting are drawn again.
                if seg.polyline {
                    kept.extend(
                        sheet
                            .wires
                            .iter()
                            .enumerate()
                            .filter(|(other, w)| w.uuid == seg.uuid && *other != edge)
                            .map(|(_, w)| (w.a, w.b)),
                    );
                }
                let far = if key(seg.a) == node { seg.b } else { seg.a };
                if let Some(tap) = tapped(seg, point_of(&node)) {
                    kept.push((tap, far));
                    break Anchor::Fixed(tap);
                }
                let next = key(far);
                if let Some(id) = own.get(&next) {
                    break Anchor::OwnPin(id.clone());
                }
                let degree = sheet.incident.get(&next).map_or(0, Vec::len);
                let held = sheet.fixtures.contains(&next) || sheet.pins_at.contains_key(&next);
                if held || degree > 2 {
                    break Anchor::Fixed(point_of(&next));
                }
                if degree == 1 {
                    break Anchor::Dangling;
                }
                let Some(onward) = sheet.incident[&next].iter().copied().find(|w| *w != edge)
                else {
                    break Anchor::Dangling;
                };
                if taken.contains(&onward) {
                    break Anchor::Fixed(point_of(&next));
                }
                node = next;
                edge = onward;
            };
            stubs.push(Stub {
                pin: (pin.owner.clone(), pin.number.clone()),
                net: sheet.net_at(pin.at).unwrap_or_default().to_string(),
                anchor,
            });
        }
    }
    Retraction {
        removed: removed.into_iter().collect(),
        kept,
        stubs,
    }
}

/// Single-pin symbols welded straight onto a moving pin — power flags, rail
/// markers, test points. They have no wire to retract, so a drag that left them
/// behind would drop the pin off its rail.
///
/// Returned as `(symbol uuid, the pin it rides on)`.
fn glued_symbols(sheet: &Sheet, moving: &HashSet<PinId>) -> Vec<(String, PinId)> {
    let own: HashMap<NodeKey, PinId> = sheet
        .pins
        .iter()
        .filter(|p| moving.contains(&(p.owner.clone(), p.number.clone())))
        .map(|p| (key(p.at), (p.owner.clone(), p.number.clone())))
        .collect();
    let mut pin_count: HashMap<&str, usize> = HashMap::new();
    for pin in &sheet.pins {
        *pin_count.entry(pin.owner.as_str()).or_default() += 1;
    }
    let mut glued: Vec<(String, PinId)> = sheet
        .pins
        .iter()
        .filter(|p| {
            !moving.contains(&(p.owner.clone(), p.number.clone()))
                && pin_count[p.owner.as_str()] == 1
        })
        .filter_map(|p| Some((p.owner.clone(), own.get(&key(p.at))?.clone())))
        .collect();
    glued.sort();
    glued.dedup();
    glued
}

/// Move one symbol, carrying its connections.
pub fn drag(doc: &mut SchDoc, id: &str, to: Placement) -> Result<DragReport, DragError> {
    let before = Sheet::of(doc);
    drag_many(doc, &[(id.to_string(), to)], &before).map(|(report, _)| report)
}

/// Turn a symbol while leaving every wire and other drawing object fixed.
///
/// This is intentionally not a drag: it permits the symbol's pins to exchange
/// the nets already present at their positions. The operation is accepted only
/// when the requested pose leaves the anchor fixed and permutes the complete
/// set of pin positions exactly.
pub fn turn_in_place(doc: &mut SchDoc, id: &str, to: Placement) -> Result<TurnReport, TurnError> {
    let symbol = doc
        .symbol(id)
        .or_else(|| doc.symbol_by_ref(id))
        .ok_or_else(|| TurnError::Unknown(id.to_string()))?;
    let uuid = symbol.uuid.clone();
    let anchor = symbol.at.point();
    if !anchor.near_eq(to.at, geom::EPS) {
        return Err(TurnError::AnchorOffset(Point2::new(
            to.at.x - anchor.x,
            to.at.y - anchor.y,
        )));
    }

    let before = Sheet::of(doc);
    let old_pins: Vec<_> = before.pins_of(&uuid).cloned().collect();
    let mut turned = doc.clone();
    turned
        .set_symbol_orientation(&uuid, to.rot, to.mirror)
        .map_err(|error| TurnError::Doc(error.to_string()))?;
    let after = Sheet::of(&turned);
    let new_pins: Vec<_> = after.pins_of(&uuid).cloned().collect();

    let mut remaining: HashMap<NodeKey, usize> = HashMap::new();
    for pin in &old_pins {
        *remaining.entry(key(pin.at)).or_default() += 1;
    }
    for pin in &new_pins {
        let at = key(pin.at);
        if let Some(count) = remaining.get_mut(&at)
            && *count > 0
        {
            *count -= 1;
            continue;
        }
        let nearest = old_pins.iter().min_by(|left, right| {
            pin.at
                .manhattan(left.at)
                .partial_cmp(&pin.at.manhattan(right.at))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let offset = nearest.map_or(pin.at, |old| {
            Point2::new(pin.at.x - old.at.x, pin.at.y - old.at.y)
        });
        return Err(TurnError::PinOffset {
            pin: pin.number.clone(),
            offset,
        });
    }
    if new_pins.len() != old_pins.len() || remaining.values().any(|count| *count != 0) {
        return Err(TurnError::Doc(
            "the orientation changed the symbol's pin count".to_string(),
        ));
    }

    let mut pins_swapped = Vec::new();
    for old in &old_pins {
        let Some(old_net) = before.net_at(old.at) else {
            continue;
        };
        let changed = new_pins
            .iter()
            .find(|new| new.number == old.number)
            .and_then(|new| after.net_at(new.at))
            != Some(old_net);
        if changed {
            pins_swapped.push((old.number.clone(), old_net.to_string()));
        }
    }
    pins_swapped.sort();
    *doc = turned;
    Ok(TurnReport {
        turned: id.to_string(),
        pins_swapped,
    })
}

/// Move several symbols at once, carrying their connections.
///
/// Moving a block in one step is not the same as moving its parts one by one:
/// the wires *inside* the block are retracted and re-drawn once, against a
/// sheet where every member has already arrived, so an IC and its decoupling
/// caps are re-wired to each other rather than to wherever they used to be.
pub fn drag_many(
    doc: &mut SchDoc,
    moves: &[(String, Placement)],
    before: &Sheet,
) -> Result<(DragReport, Sheet), DragError> {
    let mut targets: Vec<(String, Placement)> = Vec::with_capacity(moves.len());
    for (id, to) in moves {
        let uuid = doc
            .symbol(id)
            .or_else(|| doc.symbol_by_ref(id))
            .map(|s| s.uuid.clone())
            .ok_or_else(|| DragError::Unknown(id.clone()))?;
        targets.push((uuid, *to));
    }
    let owners: HashSet<&str> = targets.iter().map(|(uuid, _)| uuid.as_str()).collect();
    let seats = before
        .pins
        .iter()
        .filter(|pin| owners.contains(pin.owner.as_str()))
        .map(|pin| PinReSeat::new(&pin.owner, &pin.number, &pin.owner, &pin.number))
        .collect::<Vec<_>>();
    reseat_impl(doc, before, &seats, &targets, moves)
}

/// Reconnect pins whose identities or positions changed during a symbol edit.
///
/// The caller supplies the sheet from before the edit and the old-to-new pin
/// mapping. Attached runs are retracted from the old pins and routed back to
/// the nearest surviving point of their own nets. A route that cannot be drawn
/// cleanly becomes a matched label pair and is counted in [`DragReport`].
pub fn reseat_many(
    doc: &mut SchDoc,
    before: &Sheet,
    seats: &[PinReSeat],
) -> Result<(DragReport, Sheet), DragError> {
    reseat_impl(doc, before, seats, &[], &[])
}

/// Draw one obstacle-aware orthogonal connection between two fixed points.
///
/// `ignored_symbols` names endpoint symbols by UUID or reference so their own
/// bodies do not block the route leaving their pins. `None` means every clean
/// straight, L, Z and lattice route was blocked; callers can then report and
/// apply their appropriate label fallback.
pub fn redraw_wire(
    doc: &mut SchDoc,
    from: Point2,
    out: Point2,
    to: Point2,
    net: &str,
    ignored_symbols: &[String],
) -> Option<DragReport> {
    let sheet = Sheet::of(doc);
    let obstacles = Obstacles::new_ignoring(&sheet, ignored_symbols);
    let path = route::route(&obstacles, from, out, to, net)
        .or_else(|| route::maze(&obstacles, from, out, to, net))?;
    let mut touched = HashSet::new();
    let mut redrawn_segments = 0;
    for pair in path.windows(2) {
        if pair[0].near_eq(pair[1], geom::EPS) {
            continue;
        }
        doc.add_wire(pair[0], pair[1]);
        touched.insert(key(pair[0]));
        touched.insert(key(pair[1]));
        redrawn_segments += 1;
    }
    let after = Sheet::of(doc);
    let (junctions_added, junctions_removed) = settle_junctions(doc, &after, &touched);
    Some(DragReport {
        redrawn_segments,
        junctions_added,
        junctions_removed,
        ..DragReport::default()
    })
}

fn reseat_impl(
    doc: &mut SchDoc,
    before: &Sheet,
    seats: &[PinReSeat],
    targets: &[(String, Placement)],
    reported_moves: &[(String, Placement)],
) -> Result<(DragReport, Sheet), DragError> {
    let strict_partition = !targets.is_empty();
    let mapping: HashMap<PinId, PinId> = seats
        .iter()
        .map(|seat| {
            (
                (seat.old_owner.clone(), seat.old_number.clone()),
                (seat.new_owner.clone(), seat.new_number.clone()),
            )
        })
        .collect();
    let moving: HashSet<PinId> = mapping.keys().cloned().collect();

    let backup = doc.clone();
    let was = remap_partition(partition(before), &mapping);

    let old_pins: HashMap<PinId, Point2> = before
        .pins
        .iter()
        .filter(|p| moving.contains(&(p.owner.clone(), p.number.clone())))
        .map(|p| ((p.owner.clone(), p.number.clone()), p.at))
        .collect();
    let Retraction {
        removed,
        kept,
        stubs,
    } = retract(before, &moving);
    let glued = glued_symbols(before, &moving);

    doc.remove_drawing(&removed);
    for (a, b) in &kept {
        doc.add_wire(*a, *b);
    }
    for (uuid, to) in targets {
        doc.move_symbol(uuid, to.at.x, to.at.y)
            .and_then(|()| doc.set_symbol_orientation(uuid, to.rot, to.mirror))
            .map_err(|e| DragError::Doc(e.to_string()))?;
    }

    let placed: HashMap<PinId, (Point2, Point2)> = sch_doc::placed_pins(doc)
        .into_iter()
        .map(|p| ((p.owner, p.number), (p.at, p.out)))
        .collect();
    let new_pins: HashMap<PinId, (Point2, Point2)> = mapping
        .iter()
        .filter_map(|(old, new)| Some((old.clone(), *placed.get(new)?)))
        .collect();
    if new_pins.len() != mapping.len() {
        *doc = backup;
        return Err(DragError::Doc(
            "a re-seated pin does not exist after the symbol edit".to_string(),
        ));
    }

    // Whatever was sitting on a pin — a label, a junction, a no-connect — rides
    // along, or the symbol would leave its own connection behind.
    let carried: Vec<(Point2, Point2)> = old_pins
        .iter()
        .filter_map(|(id, old)| Some((*old, new_pins.get(id)?.0)))
        .collect();
    doc.move_attached_many(&carried);

    for (glued_uuid, pin) in &glued {
        let (Some((new_at, _)), Some(old_at)) = (new_pins.get(pin), old_pins.get(pin)) else {
            continue;
        };
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

    let (mut report, mut touched) = redraw(doc, &stubs, &new_pins);
    report.moved = reported_moves.iter().map(|(id, _)| id.clone()).collect();
    let cut: Vec<&crate::sheet::WireSeg> = before
        .wires
        .iter()
        .filter(|w| removed.contains(&w.uuid))
        .collect();
    touched.extend(cut.iter().flat_map(|w| [key(w.a), key(w.b)]));
    touched.extend(old_pins.values().map(|at| key(*at)));
    // A dot the retraction stranded part-way along a wire it deleted has to be
    // reconsidered too.
    touched.extend(
        before
            .junctions
            .iter()
            .filter(|node| {
                let p = point_of(node);
                cut.iter().any(|w| {
                    let on = |v: f64, a: f64, b: f64| {
                        v >= a.min(b) - geom::EPS && v <= a.max(b) + geom::EPS
                    };
                    if w.horizontal() {
                        (w.a.y - p.y).abs() < geom::EPS && on(p.x, w.a.x, w.b.x)
                    } else {
                        (w.a.x - p.x).abs() < geom::EPS && on(p.y, w.a.y, w.b.y)
                    }
                })
            })
            .copied(),
    );

    let mut after = Sheet::of(doc);
    let (added, dropped) = settle_junctions(doc, &after, &touched);
    report.junctions_added = added;
    report.junctions_removed = dropped;
    let stranded = strip_stranded_labels(doc, &after, &touched);
    if added + dropped + stranded > 0 {
        after = Sheet::of(doc);
    }

    if strict_partition && canonical_partition(partition(&after)) != was {
        let broken = broken_nets(before, &after);
        *doc = backup;
        return Err(DragError::Truthfulness(broken));
    }
    let lost = orphaned_pins(before, &after, &moving, &mapping);
    if lost > 0 {
        *doc = backup;
        return Err(DragError::Disconnection(lost));
    }
    // Nothing about a drag justifies leaving the sheet with more wire ends in
    // mid-air, or more names attached to nothing, than it started with.
    let litter = (after.dangling_ends() + after.stranded_labels())
        .saturating_sub(before.dangling_ends() + before.stranded_labels());
    if litter > 0 {
        *doc = backup;
        return Err(DragError::Litter(litter));
    }
    report.crossings_added = crate::eval::measure(&after)
        .crossings
        .saturating_sub(crate::eval::measure(before).crossings);
    Ok((report, after))
}

fn remap_partition(
    mut groups: Vec<Vec<String>>,
    mapping: &HashMap<PinId, PinId>,
) -> Vec<Vec<String>> {
    let names: HashMap<String, String> = mapping
        .iter()
        .map(|((old_owner, old_number), (new_owner, new_number))| {
            (
                format!("{old_owner}.{old_number}"),
                format!("{new_owner}.{new_number}"),
            )
        })
        .collect();
    for group in &mut groups {
        for pin in group.iter_mut() {
            if let Some(new) = names.get(pin) {
                *pin = new.clone();
            }
        }
        group.sort();
        group.dedup();
    }
    groups.sort();
    groups.dedup();
    groups
}

fn canonical_partition(mut groups: Vec<Vec<String>>) -> Vec<Vec<String>> {
    for group in &mut groups {
        group.sort();
        group.dedup();
    }
    groups.sort();
    groups.dedup();
    groups
}

/// Put a connection dot where the drag left one needed, and take away the ones
/// it left connecting nothing. Only points the drag touched are reconsidered —
/// a dot the author put somewhere else is their decision, not this crate's.
///
/// Returns how many were added and removed.
pub(crate) fn settle_junctions(
    doc: &mut SchDoc,
    sheet: &Sheet,
    touched: &HashSet<NodeKey>,
) -> (usize, usize) {
    let stray: Vec<String> = doc
        .items()
        .iter()
        .filter_map(|item| match item {
            Item::Junction(j) if touched.contains(&key(j.at)) && sheet.junction_inert(j.at) => {
                Some(j.uuid.clone())
            }
            _ => None,
        })
        .collect();
    let missing: Vec<Point2> = touched
        .iter()
        .filter(|k| !sheet.junctions.contains(*k))
        .map(point_of)
        .filter(|p| sheet.junction_needed(*p))
        .collect();
    let dropped = doc.remove_drawing(&stray);
    for at in &missing {
        doc.add_junction(*at);
    }
    (missing.len(), dropped)
}

/// Take away a label the drag left speaking for nothing.
///
/// A pin re-routed to a different point of its own net leaves the label that
/// used to hold it naming an empty spot. The netlist does not notice — the name
/// still merges — but KiCAD reports it and a reader sees a name in mid-air. The
/// partition gate has the last word within the sheet, and only *local* labels
/// are ever dropped: a global or hierarchical name may be holding a connection
/// to a sheet this crate cannot see.
fn strip_stranded_labels(doc: &mut SchDoc, sheet: &Sheet, touched: &HashSet<NodeKey>) -> usize {
    let stranded: Vec<String> = doc
        .labels()
        .filter(|l| l.kind == LabelKind::Local)
        .filter(|l| touched.contains(&key(l.at.point())) && sheet.label_stranded(l.at.point()))
        .map(|l| l.uuid.clone())
        .collect();
    doc.remove_drawing(&stranded)
}

/// The net partition as the drag gate compares it: which pins share a net.
///
/// Names are deliberately absent from the key, so re-drawing a connection may
/// rename an auto-generated net but may never move a pin between nets. A pin
/// alone on an unnamed node is not a net, which is the line the extractor draws
/// too; a pin alone on a *named* one is, so losing its label is caught.
pub fn partition(sheet: &Sheet) -> Vec<Vec<String>> {
    let mut groups: HashMap<&str, Vec<String>> = HashMap::new();
    for pin in &sheet.pins {
        if let Some(net) = sheet.net_at(pin.at) {
            // Keyed by the owning symbol, not by a reference designator: an
            // unannotated sheet repeats `R?`, and a gate that cannot tell two
            // pins apart is not a gate.
            groups
                .entry(net)
                .or_default()
                .push(format!("{}.{}", pin.owner, pin.number));
        }
    }
    let mut out: Vec<Vec<String>> = groups
        .into_iter()
        .filter(|(net, pins)| pins.len() > 1 || !net.starts_with('#'))
        .map(|(net, mut pins)| {
            pins.sort();
            pins.dedup();
            if pins.len() == 1 {
                pins.insert(0, net.to_string());
            }
            pins
        })
        .collect();
    out.sort();
    out
}

/// Moved pins that had something drawn on them and now have nothing — not a
/// wire, not a label, not a power flag — while the netlist still claims they
/// are connected.
///
/// This is the failure the pin partition cannot see: on a sheet wired with
/// labels, dropping a pin's only wire leaves the netlist word-perfect and the
/// drawing showing a part connected to thin air.
fn orphaned_pins(
    before: &Sheet,
    after: &Sheet,
    moving: &HashSet<PinId>,
    mapping: &HashMap<PinId, PinId>,
) -> usize {
    let held = |sheet: &Sheet, at: Point2| {
        let k = key(at);
        sheet.incident.contains_key(&k)
            || sheet.fixtures.contains(&k)
            || sheet.pins_at.get(&k).is_some_and(|pins| pins.len() > 1)
    };
    let was_held: HashSet<(&str, &str)> = before
        .pins
        .iter()
        .filter(|p| moving.contains(&(p.owner.clone(), p.number.clone())) && held(before, p.at))
        .map(|p| (p.owner.as_str(), p.number.as_str()))
        .collect();
    mapping
        .iter()
        .filter(|((owner, number), _)| was_held.contains(&(owner.as_str(), number.as_str())))
        .filter(|(_, (owner, number))| {
            after
                .pins
                .iter()
                .find(|pin| pin.owner == *owner && pin.number == *number)
                .is_none_or(|pin| !held(after, pin.at))
        })
        .count()
}

/// Nets whose pin membership differs between two sheet states.
fn broken_nets(before: &Sheet, after: &Sheet) -> Vec<String> {
    let (was, now) = (partition(before), partition(after));
    let old: HashSet<&Vec<String>> = was.iter().collect();
    let new: HashSet<&Vec<String>> = now.iter().collect();
    let name_of = |group: &Vec<String>, sheet: &Sheet| {
        group
            .iter()
            .find_map(|pin| {
                let (owner, number) = pin.split_once('.')?;
                let at = sheet
                    .pins
                    .iter()
                    .find(|p| p.owner == owner && p.number == number)?
                    .at;
                sheet.net_at(at).map(str::to_string)
            })
            .unwrap_or_else(|| group.join("+"))
    };
    let mut broken: Vec<String> = was
        .iter()
        .filter(|g| !new.contains(g))
        .map(|g| name_of(g, before))
        .chain(
            now.iter()
                .filter(|g| !old.contains(g))
                .map(|g| name_of(g, after)),
        )
        .collect();
    broken.sort();
    broken.dedup();
    broken.truncate(8);
    broken
}

/// Points on `net` a re-drawn wire may land on, nearest first.
///
/// Existing connection points come first; a perpendicular tap onto a same-net
/// wire is offered too, because that is how a rail actually gets used.
fn targets(sheet: &Sheet, from: Point2, net: &str) -> Vec<(Point2, bool)> {
    let mut out: Vec<(Point2, bool)> = sheet
        .node_net
        .iter()
        .filter(|(_, name)| *name == net)
        .map(|(node, _)| (point_of(node), false))
        .collect();
    for wire in sheet.wires.iter().filter(|w| w.net == net) {
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

/// A net name nothing on the sheet is using — `N$1`, never a reference
/// designator, which a reader would take for a part.
fn fresh_name(taken: &HashSet<String>) -> String {
    (1..)
        .map(|n| format!("N${n}"))
        .find(|c| !taken.contains(c))
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

/// Re-draw every retracted stub, falling back to a matched pair of labels where
/// no clean route exists.
fn redraw(
    doc: &mut SchDoc,
    stubs: &[Stub],
    new_pins: &HashMap<PinId, (Point2, Point2)>,
) -> (DragReport, HashSet<NodeKey>) {
    let sheet = Sheet::of(doc);
    let mut obstacles = Obstacles::new(&sheet);
    let mut drawn: Vec<Path> = Vec::new();
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
            // The anchor may itself have been erased by this retraction, when
            // every wire holding it belonged to a pin that moved. Routing to it
            // brings it back, so the pin's own net is what says where to go.
            Anchor::Fixed(p) => (*p, sheet.net_at(*p).unwrap_or(stub.net.as_str())),
        };
        if from.near_eq(target, geom::EPS) {
            continue;
        }
        match best_route(&obstacles, &sheet, from, out, target, net, &stub.anchor) {
            Some(path) => {
                obstacles.add_path(&path, net);
                drawn.push(path);
            }
            None => fallbacks.push((from, out, target)),
        }
    }

    let mut redrawn_segments = 0;
    let mut touched: HashSet<NodeKey> = new_pins.values().map(|(at, _)| key(*at)).collect();
    touched.extend(stubs.iter().filter_map(|s| match &s.anchor {
        Anchor::Fixed(p) => Some(key(*p)),
        _ => None,
    }));
    for path in &drawn {
        for pair in path.windows(2) {
            doc.add_wire(pair[0], pair[1]);
            redrawn_segments += 1;
            // A same-net point the new wire runs straight through needs the dot
            // that says so, and would otherwise never be reconsidered.
            let span = geom::Segment::new(pair[0], pair[1]);
            touched.extend(
                sheet
                    .node_net
                    .keys()
                    .filter(|node| span.contains_point(point_of(node)))
                    .copied(),
            );
        }
        touched.extend(path.iter().map(|p| key(*p)));
    }

    let mut labels_added = 0;
    let mut named: HashMap<NodeKey, String> = HashMap::new();
    let mut taken: HashSet<String> = sheet
        .label_texts()
        .into_iter()
        .map(str::to_string)
        .chain(sheet.pins.iter().map(|p| p.refdes.clone()))
        .chain(sheet.node_net.values().cloned())
        .collect();
    for (from, out, target) in fallbacks {
        let at = key(target);
        let name = match sheet.label_names.get(&at).or_else(|| named.get(&at)) {
            Some(existing) => existing.clone(),
            None => {
                let name = fresh_name(&taken);
                taken.insert(name.clone());
                named.insert(at, name.clone());
                doc.add_label(LabelKind::Local, &name, Pose::new(target.x, target.y, 0.0));
                labels_added += 1;
                name
            }
        };
        // The stub a label sits on is a wire like any other and has to clear the
        // same obstacles; a label straight on the pin is the last resort.
        let stub_end = [2.54_f64, 5.08, 1.27]
            .into_iter()
            .map(|reach| {
                route::snap_point(Point2::new(from.x + out.x * reach, from.y + out.y * reach))
            })
            .find(|end| obstacles.path_ok(&[from, *end], &name));
        let anchor = match stub_end {
            Some(end) => {
                doc.add_wire(from, end);
                obstacles.add_path(&[from, end], "");
                redrawn_segments += 1;
                end
            }
            None => from,
        };
        doc.add_label(
            LabelKind::Local,
            &name,
            Pose::new(anchor.x, anchor.y, label_rot(out)),
        );
        labels_added += 1;
    }

    (
        DragReport {
            redrawn_segments,
            labels_added,
            ..Default::default()
        },
        touched,
    )
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
) -> Option<Path> {
    let mut candidates: Vec<(Point2, bool)> = vec![(anchor, false)];
    if !matches!(kind, Anchor::OwnPin(_)) {
        candidates.extend(targets(sheet, from, net));
    }
    let mut best: Option<(Path, f64)> = None;
    for (target, tap) in &candidates {
        let Some(path) = route::route(obstacles, from, out, *target, net) else {
            continue;
        };
        let cost = route::path_cost(&path, out) + if *tap { 3.0 } else { 0.0 };
        if best.as_ref().is_none_or(|(_, c)| cost < *c) {
            best = Some((path, cost));
        }
    }
    // Only when every straight, L and Z shape is blocked is the lattice search
    // worth its cost — that is exactly the case a human solves with a detour and
    // this crate used to solve with a label.
    if best.is_none() {
        for (target, _) in candidates.iter().take(2) {
            let Some(path) = route::maze(obstacles, from, out, *target, net) else {
                continue;
            };
            let cost = route::path_cost(&path, out);
            if best.as_ref().is_none_or(|(_, c)| cost < *c) {
                best = Some((path, cost));
            }
        }
    }
    best.map(|(path, _)| path)
}
