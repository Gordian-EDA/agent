//! Measure the tree, then place it: every box's size, every alignment line, every
//! coordinate.
//!
//! A measured node carries two reference points. Its BOX is the room the drawing claims.
//! Its ALIGNMENT LINE is the line a container lines its children up on — for a series part
//! that is its pin axis, for a shunt standing across a row it is the pin that touches the
//! row, and for a part beside an IC it is the pin that reaches the IC. Aligning lines
//! rather than boxes is what makes a wire leave one pin and arrive at the next without a
//! bend.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use geom::{Dir, Point2};
use sch_model::tree::{Align, Axis, Container, DEFAULT_GAP, Tree, UNIT_MM, WRAP_HEIGHT, WRAP_WIDTH};

use crate::SHEET_ASPECT;

use crate::orient::{authored_pose, default_pose};
use crate::part::{Part, Pose};

/// Gap floor (grid units) around a part with many pins: an IC needs a channel its pin
/// text does not spill across.
const IC_GAP: f64 = 6.0;
/// Gap floor (grid units) between siblings that are themselves stacks: the aisle between
/// two GROUPS. Parts that belong together read as a group only when the white space
/// around the group is wider than the white space inside it, so this stays wide while
/// [`sch_model::tree::DEFAULT_GAP`] — the wire between two parts of one group — is tight.
const GROUP_GAP: f64 = 10.0;
/// A part with at least this many pins is an IC for spacing and alignment purposes.
const IC_PINS: usize = 3;
/// Weight of the band-raggedness term in [`ribbon_cost`].
const RAGGED_BAND: f64 = 1.0;

/// A measured node: a box, an alignment line, and how it is built.
pub struct Node {
    pub w: f64,
    pub h: f64,
    /// Alignment line, as an offset from the box's top-left corner.
    pub ax: f64,
    pub ay: f64,
    pub kind: Kind,
}

pub enum Kind {
    Leaf {
        /// Index into the typesetter's parts.
        part: usize,
        pose: Pose,
        /// The instance origin, as an offset from the box's top-left corner.
        anchor: Point2,
    },
    Stack {
        axis: Axis,
        children: Vec<Node>,
        gap: f64,
        align: Align,
        /// Main-axis child positions when `align_col_to_pins` overrode the even spacing.
        offsets: Option<Vec<f64>>,
    },
}

impl Node {
    fn anchor(&self) -> Point2 {
        match &self.kind {
            Kind::Leaf { anchor, .. } => *anchor,
            Kind::Stack { .. } => Point2::new(self.ax, self.ay),
        }
    }

    /// Signal (non-rail) nets somewhere in this subtree.
    fn signal_nets(&self, parts: &[Part]) -> BTreeSet<String> {
        match &self.kind {
            Kind::Leaf { part, .. } => parts[*part]
                .nets()
                .into_iter()
                .filter(|net| !circuit_graph::netclass::is_power_net(net))
                .map(str::to_owned)
                .collect(),
            Kind::Stack { children, .. } => children
                .iter()
                .flat_map(|c| c.signal_nets(parts))
                .collect(),
        }
    }
}

/// Where a leaf resolves to: the part it draws and the pose it takes.
pub struct Placed {
    pub part: usize,
    pub at: Point2,
    pub pose: Pose,
}

/// Which connector kinds a block turns.
///
/// The facing is decided once for the whole block and then applied, because the rule
/// that decides it is positional (see [`face_connectors`]) and a bank of identical
/// headers spread over several rows would otherwise disagree row by row.
enum Facing<'a> {
    /// Measuring to learn: each container decides for itself and records the kind.
    Probe(&'a RefCell<BTreeSet<String>>),
    /// Measuring to draw: exactly these kinds are turned, wherever they sit.
    Fixed(&'a BTreeSet<String>),
}

/// Measure `tree` and lay it out with its top-left at the origin.
pub fn typeset_block(tree: &Tree, parts: &[Part], index: &dyn Fn(&str, u8) -> Option<usize>) -> Vec<Placed> {
    let probe = RefCell::new(BTreeSet::new());
    measure(tree, parts, index, Axis::Row, &Facing::Probe(&probe));
    let node = measure(tree, parts, index, Axis::Row, &Facing::Fixed(&probe.into_inner()));
    let mut out = Vec::new();
    place(&node, 0.0, 0.0, &mut out);
    out
}

fn measure(
    tree: &Tree,
    parts: &[Part],
    index: &dyn Fn(&str, u8) -> Option<usize>,
    axis: Axis,
    facing: &Facing,
) -> Node {
    match tree {
        Tree::Leaf(leaf) => {
            let Some(i) = index(&leaf.part, leaf.unit.unwrap_or(1)) else {
                return empty();
            };
            leaf_node(
                i,
                parts,
                match leaf.rot {
                    Some(rot) => authored_pose(&parts[i], rot, leaf.mirror),
                    None => Pose {
                        mirror: leaf.mirror,
                        ..default_pose(&parts[i], axis)
                    },
                },
                axis,
            )
        }
        Tree::Container(c) => container_node(c, parts, index, facing),
    }
}

fn empty() -> Node {
    Node {
        w: 0.0,
        h: 0.0,
        ax: 0.0,
        ay: 0.0,
        kind: Kind::Stack {
            axis: Axis::Row,
            children: Vec::new(),
            gap: 0.0,
            align: Align::Center,
            offsets: None,
        },
    }
}

fn leaf_node(part: usize, parts: &[Part], pose: Pose, axis: Axis) -> Node {
    let r = parts[part].extent(pose);
    let anchor = Point2::new(-r.min_x, -r.min_y);
    let mut node = Node {
        w: r.width(),
        h: r.height(),
        ax: anchor.x,
        ay: anchor.y,
        kind: Kind::Leaf { part, pose, anchor },
    };
    align_line(&mut node, parts, axis);
    node
}

/// A 2-pin part standing ACROSS its container's axis — a shunt in a row, a series part in
/// a column — is aligned on the pin that touches the container's line, so the through-wire
/// meets its tip instead of its middle. Everything else aligns on its anchor.
fn align_line(node: &mut Node, parts: &[Part], axis: Axis) {
    let Kind::Leaf { part, pose, anchor } = &node.kind else {
        return;
    };
    let part = &parts[*part];
    if !part.two_pin() {
        return;
    }
    let flat = part.lies_flat(*pose);
    let offsets: Vec<Point2> = part
        .pins
        .iter()
        .map(|pin| part.pin_offset(pin, *pose))
        .collect();
    match axis {
        Axis::Row if !flat => node.ay = anchor.y + offsets[0].y.min(offsets[1].y),
        Axis::Col if flat => node.ax = anchor.x + offsets[0].x.min(offsets[1].x),
        _ => {}
    }
}

fn container_node(
    c: &Container,
    parts: &[Part],
    index: &dyn Fn(&str, u8) -> Option<usize>,
    facing: &Facing,
) -> Node {
    // A leaf naming a part this call is not placing — one already on the sheet, or one the
    // payload could not resolve — is dropped outright rather than measured as an empty
    // box, which would leave a gap where nothing is drawn.
    let kept: Vec<&Tree> = c
        .children
        .iter()
        .filter(|child| !matches!(child, Tree::Leaf(l) if index(&l.part, l.unit.unwrap_or(1)).is_none()))
        .collect();
    let c = &Container {
        children: kept.into_iter().cloned().collect(),
        ..c.clone()
    };
    let mut children: Vec<Node> = c
        .children
        .iter()
        .map(|child| measure(child, parts, index, c.axis, facing))
        .collect();
    if children.is_empty() {
        return empty();
    }
    if let Some(wrapped) = wrap(c, &children, parts) {
        return container_node(&wrapped, parts, index, facing);
    }
    face_neighbours(&mut children, &c.children, parts, c.axis, facing);
    if c.axis == Axis::Row {
        align_columns_to_ic_pins(&mut children, parts);
    }
    let gap = spacing(c, &children, parts);
    let span: f64 = children.iter().map(|k| main(k, c.axis)).sum::<f64>()
        + gap * children.len().saturating_sub(1) as f64;
    let (before, after) = (
        children.iter().map(|k| line(k, c.axis)).fold(0.0, f64::max),
        children
            .iter()
            .map(|k| cross(k, c.axis) - line(k, c.axis))
            .fold(0.0, f64::max),
    );
    let (thickness, at_line) = if c.align == Align::Center {
        (before + after, before)
    } else {
        let t = children.iter().map(|k| cross(k, c.axis)).fold(0.0, f64::max);
        (t, t / 2.0)
    };
    let head = children.first().map_or(span / 2.0, |k| line(k, flip(c.axis)));
    let (w, h, ax, ay) = match c.axis {
        Axis::Row => (span, thickness, head, at_line),
        Axis::Col => (thickness, span, at_line, head),
    };
    Node {
        w,
        h,
        ax,
        ay,
        kind: Kind::Stack {
            axis: c.axis,
            children,
            gap,
            align: c.align,
            offsets: None,
        },
    }
}

/// The spacing a container is laid out with, which is what a band has to be measured
/// against. Siblings that are drawings sit a wire apart; one of them an IC opens the row
/// to [`IC_GAP`], and siblings that are themselves stacks are groups and keep the wider
/// [`GROUP_GAP`] aisle between them.
fn spacing(c: &Container, children: &[Node], parts: &[Part]) -> f64 {
    let big = children.iter().any(|k| ic_leaf(k, parts));
    let grouped = children.iter().any(|k| matches!(k.kind, Kind::Stack { .. }));
    let floor = if big {
        IC_GAP
    } else if grouped {
        GROUP_GAP
    } else {
        0.0
    };
    c.gap.unwrap_or(DEFAULT_GAP).max(floor) * UNIT_MM
}

/// Break a container that has outgrown its page into bands of the same children, in
/// order, stacked across its own axis — a row into stacked rows, a column into
/// side-by-side columns. `None` when it already fits.
///
/// WHICH split is taken is decided by the shape it leaves behind, not by filling each
/// band to the limit. A greedy first fit optimises one band at a time: it leaves the last
/// band short, and the stack it hands back overflows the OTHER axis, wraps again, and
/// flips axis on every pass until the block is a ribbon several sheets wide (a 53-part
/// block measured 918 mm across). Scoring the whole grid instead — proportions first,
/// with a band longer than the page penalised — settles it in one pass, and the bands are
/// marked as final so nothing re-wraps them.
fn wrap(c: &Container, children: &[Node], parts: &[Part]) -> Option<Container> {
    if c.children.len() < 2 {
        return None;
    }
    let limit = c.wrap.unwrap_or(match c.axis {
        Axis::Row => WRAP_WIDTH,
        Axis::Col => WRAP_HEIGHT,
    }) * UNIT_MM;
    let gap = spacing(c, children, parts);
    let sizes: Vec<f64> = children.iter().map(|k| main(k, c.axis)).collect();
    let span: f64 = sizes.iter().sum::<f64>() + gap * (children.len() - 1) as f64;
    if span <= limit {
        return None;
    }
    let cost = |b: &[usize]| ribbon_cost(b, children, c.axis, gap, limit);
    let bands = (2..=children.len())
        .flat_map(|count| {
            [
                split(&sizes, gap, span / count as f64),
                even_bands(children.len(), count),
            ]
        })
        .min_by(|a, b| cost(a).total_cmp(&cost(b)))?;
    (bands.len() > 1).then(|| Container {
        axis: flip(c.axis),
        children: bands
            .iter()
            .scan(0, |from, len| {
                let band = c.children[*from..*from + len].to_vec();
                *from += len;
                Some(Tree::Container(Container {
                    axis: c.axis,
                    children: band,
                    gap: c.gap,
                    align: c.align,
                    wrap: Some(f64::INFINITY),
                }))
            })
            .collect(),
        gap: Some(DEFAULT_GAP),
        align: Align::Start,
        // The bands were chosen against the finished grid's proportions; measuring that
        // grid again on the flipped axis is what started the ribbon.
        wrap: Some(f64::INFINITY),
    })
}

/// `n` children dealt into `count` bands of as near the same size as they divide.
fn even_bands(n: usize, count: usize) -> Vec<usize> {
    let (each, extra) = (n / count, n % count);
    (0..count)
        .map(|i| each + usize::from(i < extra))
        .filter(|len| *len > 0)
        .collect()
}

/// Child counts of the bands `sizes` falls into when each is filled up to `target` — a
/// child longer than `target` takes a band of its own rather than being dropped.
fn split(sizes: &[f64], gap: f64, target: f64) -> Vec<usize> {
    let mut bands = vec![0usize];
    let mut used = 0.0;
    for size in sizes {
        let band = bands.last_mut().expect("one band exists");
        if *band > 0 && used + gap + size > target {
            bands.push(1);
            used = *size;
        } else {
            used += if *band == 0 { *size } else { gap + size };
            *band += 1;
        }
    }
    bands
}

/// How badly a banding reads: how far the finished grid is from a page's proportions,
/// how far its longest band overruns the page it has to fit across, and how ragged the
/// bands are against each other. Logarithmic on all three, so twice as wide and half as
/// wide are the same defect.
///
/// The raggedness term is what makes eight identical channels fold 4 and 4 rather than
/// 6 and 2: a short trailing row reads as an unfinished drawing, and on a repeated
/// structure it throws away the symmetry the sheet is read by.
fn ribbon_cost(bands: &[usize], children: &[Node], axis: Axis, gap: f64, limit: f64) -> f64 {
    let (mut long, mut thick, mut from) = (0.0f64, 0.0f64, 0usize);
    for (i, len) in bands.iter().enumerate() {
        let band = &children[from..from + len];
        long = long.max(
            band.iter().map(|k| main(k, axis)).sum::<f64>() + gap * (len - 1) as f64,
        );
        thick += band.iter().map(|k| cross(k, axis)).fold(0.0, f64::max)
            + if i > 0 { DEFAULT_GAP * UNIT_MM } else { 0.0 };
        from += len;
    }
    let (w, h) = match axis {
        Axis::Row => (long, thick),
        Axis::Col => (thick, long),
    };
    let ragged = match (bands.iter().min(), bands.iter().max()) {
        (Some(few), Some(many)) => (*many as f64 / *few as f64).ln(),
        _ => 0.0,
    };
    ((w / h.max(1.0)) / SHEET_ASPECT).ln().abs()
        + 2.0 * (long / limit.max(1.0)).max(1.0).ln()
        + RAGGED_BAND * ragged
}

fn flip(axis: Axis) -> Axis {
    match axis {
        Axis::Row => Axis::Col,
        Axis::Col => Axis::Row,
    }
}

/// Size along `axis`.
fn main(node: &Node, axis: Axis) -> f64 {
    match axis {
        Axis::Row => node.w,
        Axis::Col => node.h,
    }
}

/// Size across `axis`.
fn cross(node: &Node, axis: Axis) -> f64 {
    main(node, flip(axis))
}

/// Alignment line measured across `axis`.
fn line(node: &Node, axis: Axis) -> f64 {
    match axis {
        Axis::Row => node.ay,
        Axis::Col => node.ax,
    }
}

fn ic_leaf(node: &Node, parts: &[Part]) -> bool {
    match &node.kind {
        Kind::Leaf { part, .. } => {
            parts[*part].pins.len() > 8 && !parts[*part].is_connector()
        }
        Kind::Stack { children, .. } => children.iter().any(|c| ic_leaf(c, parts)),
    }
}

/// Turn each child to face the sibling it shares a net with.
///
/// Three conventions, all of them things a human does without thinking:
/// a connector at the end of a row is mirrored so its pins point INTO the circuit; a
/// multi-pin part is aligned on the pin its neighbour connects to (a series resistor feeds
/// straight into an op-amp input, not into the symbol's centre line); and a 2-pin part is
/// flipped end-for-end when the pin sharing the neighbour's net is on the far side.
fn face_neighbours(
    children: &mut [Node],
    authored: &[Tree],
    parts: &[Part],
    axis: Axis,
    facing: &Facing,
) {
    face_connectors(children, authored, parts, axis, facing);
    for i in 0..children.len() {
        let Kind::Leaf { part, pose, .. } = children[i].kind else {
            continue;
        };
        if matches!(&authored[i], Tree::Leaf(l) if l.rot.is_some() || l.mirror) {
            continue;
        }
        let (before, after) = neighbour_nets(children, parts, i);
        let part_ref = &parts[part];
        if part_ref.is_connector() {
            continue;
        }
        if !part_ref.two_pin() {
            if let Some(shift) = anchor_pin_shift(part_ref, pose, axis, &before, &after) {
                match axis {
                    Axis::Row => children[i].ay = children[i].anchor().y + shift.y,
                    Axis::Col => children[i].ax = children[i].anchor().x + shift.x,
                }
            }
            continue;
        }
        if should_flip(part_ref, pose, axis, &before, &after) {
            let flipped = Pose {
                angle: (pose.angle + 180.0) % 360.0,
                ..pose
            };
            if !upside_down(part_ref, flipped) {
                children[i] = leaf_node(part, parts, flipped, axis);
            }
        }
    }
}

/// One facing for every connector of the same kind in a container.
///
/// The rule for one connector is that the end of a row points its pins INTO the circuit
/// rather than off the sheet. Applied per child it splits a bank of identical headers —
/// only the one at the end is at an end — and J3's labels come out on the right while
/// J4's come out on the left, which no drawn-by-hand sheet does. So the decision is taken
/// once per kind: if any sibling of that kind faces away, they all turn.
fn face_connectors(
    children: &mut [Node],
    authored: &[Tree],
    parts: &[Part],
    axis: Axis,
    facing: &Facing,
) {
    let len = children.len();
    let mut siblings: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, child) in children.iter().enumerate() {
        let Kind::Leaf { part, .. } = child.kind else {
            continue;
        };
        if !parts[part].is_connector() {
            continue;
        }
        if matches!(&authored[i], Tree::Leaf(l) if l.rot.is_some() || l.mirror) {
            continue;
        }
        siblings
            .entry(parts[part].item.part.as_str())
            .or_default()
            .push(i);
    }
    for (name, kind) in siblings {
        let faces_away = |&i: &usize| match children[i].kind {
            Kind::Leaf { part, pose, .. } => connector_faces_away(&parts[part], pose, axis, i, len),
            Kind::Stack { .. } => false,
        };
        match facing {
            Facing::Probe(seen) => {
                if kind.iter().any(faces_away) {
                    seen.borrow_mut().insert(name.to_string());
                }
                continue;
            }
            Facing::Fixed(turn) if !turn.contains(name) => continue,
            Facing::Fixed(_) => {}
        }
        for i in kind {
            let Kind::Leaf { part, pose, .. } = children[i].kind else {
                continue;
            };
            // A column flips the symbol top-to-bottom, which KiCAD has no flag for:
            // mirror-x is a half turn composed with the mirror-y it does have.
            let turned = match axis {
                Axis::Row => Pose {
                    mirror: !pose.mirror,
                    ..pose
                },
                Axis::Col => Pose {
                    angle: (pose.angle + 180.0) % 360.0,
                    mirror: !pose.mirror,
                },
            };
            children[i] = leaf_node(part, parts, turned, axis);
        }
    }
}

/// The signal nets of the siblings on either side of child `i`.
fn neighbour_nets(
    children: &[Node],
    parts: &[Part],
    i: usize,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let of = |j: Option<usize>| {
        j.and_then(|j| children.get(j))
            .filter(|n| matches!(n.kind, Kind::Leaf { .. }))
            .map(|n| n.signal_nets(parts))
            .unwrap_or_default()
    };
    (of(i.checked_sub(1)), of(Some(i + 1)))
}

/// Whether a connector at one end of a container has every pin pointing away from its
/// only neighbour — the case a human fixes by mirroring the symbol rather than drawing
/// wires around it.
fn connector_faces_away(part: &Part, pose: Pose, axis: Axis, i: usize, len: usize) -> bool {
    let (first, last) = (i == 0, i + 1 == len);
    if first == last {
        return false;
    }
    let mut dirs = part.pins.iter().map(|pin| part.pin_dir(pin, pose));
    let Some(d) = dirs.next() else { return false };
    if !dirs.all(|other| other == d) {
        return false;
    }
    match axis {
        Axis::Row => (first && d == Dir::West) || (last && d == Dir::East),
        Axis::Col => (first && d == Dir::North) || (last && d == Dir::South),
    }
}

/// Where a multi-pin part's alignment line goes: onto the pin that faces the neighbour it
/// shares a net with — the previous one by preference, so a chain reads forwards.
fn anchor_pin_shift(
    part: &Part,
    pose: Pose,
    axis: Axis,
    before: &BTreeSet<String>,
    after: &BTreeSet<String>,
) -> Option<Point2> {
    let toward_previous = match axis {
        Axis::Row => Dir::West,
        Axis::Col => Dir::North,
    };
    let toward_next = match axis {
        Axis::Row => Dir::East,
        Axis::Col => Dir::South,
    };
    for (nets, want) in [(before, toward_previous), (after, toward_next)] {
        let hit = part.pins.iter().find(|pin| {
            part.net(pin).is_some_and(|net| nets.contains(net)) && part.pin_dir(pin, pose) == want
        });
        if let Some(pin) = hit {
            return Some(part.pin_offset(pin, pose));
        }
    }
    None
}

/// Whether a 2-pin part's near pin is the wrong one: the pin its neighbours' net lands on
/// is at the far end, so the wire would have to run back past the body.
fn should_flip(
    part: &Part,
    pose: Pose,
    axis: Axis,
    before: &BTreeSet<String>,
    after: &BTreeSet<String>,
) -> bool {
    let offsets: Vec<Point2> = part
        .pins
        .iter()
        .map(|pin| part.pin_offset(pin, pose))
        .collect();
    let flat = part.lies_flat(pose);
    let across = (axis == Axis::Row && !flat) || (axis == Axis::Col && flat);
    let near = if across {
        // Standing across the axis: the pin on the container's line is the near one.
        let (a, b) = match axis {
            Axis::Row => (offsets[0].y, offsets[1].y),
            Axis::Col => (offsets[0].x, offsets[1].x),
        };
        usize::from(a >= b)
    } else {
        let (a, b) = match axis {
            Axis::Row => (offsets[0].x, offsets[1].x),
            Axis::Col => (offsets[0].y, offsets[1].y),
        };
        usize::from(a >= b)
    };
    let net = |i: usize| part.net(part.pins[i]).unwrap_or_default().to_owned();
    let (near_net, far_net) = (net(near), net(1 - near));
    if across {
        let touching: BTreeSet<&String> = before.union(after).collect();
        return touching.contains(&far_net) && !touching.contains(&near_net);
    }
    (before.contains(&far_net) && !before.contains(&near_net))
        || (after.contains(&near_net) && !after.contains(&far_net))
}

fn upside_down(part: &Part, pose: Pose) -> bool {
    part.pins.iter().any(|pin| {
        part.net(pin).is_some_and(|net| {
            circuit_graph::netclass::is_power_net(net)
                && part.pin_dir(pin, pose)
                    == if circuit_graph::netclass::is_ground(net) {
                        Dir::North
                    } else {
                        Dir::South
                    }
        })
    })
}

/// A column standing beside an IC is put on the IC's own grid: each child slides onto the
/// line of the pin it connects to, so its wire is a straight run rather than a dog-leg.
fn align_columns_to_ic_pins(children: &mut [Node], parts: &[Part]) {
    for i in 0..children.len() {
        if !matches!(children[i].kind, Kind::Stack { axis: Axis::Col, .. }) {
            continue;
        }
        for (j, toward) in [(i.checked_sub(1), Dir::East), (Some(i + 1), Dir::West)] {
            let Some(j) = j else { continue };
            let Some(ic) = children.get(j) else { continue };
            let Kind::Leaf { part, pose, anchor } = ic.kind else {
                continue;
            };
            if parts[part].pins.len() < IC_PINS || parts[part].is_connector() {
                continue;
            }
            let lines = pin_lines(&parts[part], pose, anchor.y, ic.ay, toward);
            if !lines.is_empty() && seat_column(&mut children[i], parts, &lines) {
                break;
            }
        }
    }
}

/// Net → the y of the IC pin carrying it, measured from the IC's alignment line. Pins on
/// the top and bottom edges count too, just outside the body, so a part hanging off them
/// hooks around the corner instead of looping over the whole symbol.
fn pin_lines(
    part: &Part,
    pose: Pose,
    anchor_y: f64,
    line_y: f64,
    toward: Dir,
) -> Vec<(String, f64)> {
    let shift = line_y - anchor_y;
    let mut out: Vec<(String, f64)> = Vec::new();
    for edge in [false, true] {
        for pin in &part.pins {
            let Some(net) = part.net(pin) else { continue };
            if circuit_graph::netclass::is_power_net(net) || out.iter().any(|(n, _)| n == net) {
                continue;
            }
            let dir = part.pin_dir(pin, pose);
            let y = part.pin_offset(pin, pose).y - shift;
            match (edge, dir) {
                (false, d) if d == toward => out.push((net.to_owned(), y)),
                (true, Dir::North) => out.push((net.to_owned(), y - 4.0 * UNIT_MM)),
                (true, Dir::South) => out.push((net.to_owned(), y + 4.0 * UNIT_MM)),
                _ => {}
            }
        }
    }
    out
}

/// Slide a column's children onto `lines`, keeping their order and never overlapping.
/// Returns whether any child found a line to sit on.
/// Where in a node's own box the pin carrying `net` sits — what has to land on the IC's
/// pin line. Falls back to the node's alignment line when it has no such pin of its own.
fn seat_of(node: &Node, parts: &[Part], net: &str) -> f64 {
    let Kind::Leaf { part, pose, anchor } = &node.kind else {
        return node.ay;
    };
    let part = &parts[*part];
    part.pins
        .iter()
        .find(|pin| part.net(pin) == Some(net))
        .map(|pin| anchor.y + part.pin_offset(pin, *pose).y)
        .unwrap_or(node.ay)
}

fn seat_column(col: &mut Node, parts: &[Part], lines: &[(String, f64)]) -> bool {
    let Kind::Stack { children, gap, .. } = &col.kind else {
        return false;
    };
    let gap = *gap;
    // What has to land on the IC's pin line is the child's OWN pin carrying that net, not
    // its origin: a standing 2-pin part's origin is midway between its pins, so seating
    // the origin puts the connection half a body off the line.
    // Per child: the IC pin line to sit on, and where in the child's own box the pin that
    // reaches it is. Seating the child's ORIGIN instead would put a standing passive's
    // connection half a body off the line.
    let targets: Vec<Option<(f64, f64)>> = children
        .iter()
        .map(|child| {
            let nets = child.signal_nets(parts);
            let (net, line) = lines
                .iter()
                .filter(|(net, _)| nets.contains(net))
                .min_by(|a, b| a.1.total_cmp(&b.1))?;
            Some((*line, seat_of(child, parts, net)))
        })
        .collect();
    if targets.iter().all(Option::is_none) {
        return false;
    }
    let (mut cursor, mut anchor_line, mut offsets) = (0.0f64, None, Vec::new());
    for (child, target) in children.iter().zip(&targets) {
        let mut y = cursor;
        if let Some((line, seat)) = target {
            match anchor_line {
                None => anchor_line = Some(y + seat - line),
                Some(l) => y = cursor.max(l + line - seat),
            }
        }
        offsets.push(y);
        cursor = y + child.h + gap;
    }
    col.h = cursor - gap;
    col.ay = anchor_line.unwrap_or(col.ay);
    let Kind::Stack { offsets: slot, .. } = &mut col.kind else {
        return false;
    };
    *slot = Some(offsets);
    true
}

/// Walk the measured tree, writing out one position per leaf. `(x, y)` is the node box's
/// top-left corner.
fn place(node: &Node, x: f64, y: f64, out: &mut Vec<Placed>) {
    match &node.kind {
        Kind::Leaf { part, pose, anchor } => {
            // Snap the ALIGNMENT LINE to the grid and derive the instance origin from it,
            // so two parts meant to share a pin line still share it after snapping.
            let snapped = Point2::new(snap(x + node.ax), snap(y + node.ay));
            out.push(Placed {
                part: *part,
                at: Point2::new(
                    snapped.x - (node.ax - anchor.x),
                    snapped.y - (node.ay - anchor.y),
                ),
                pose: *pose,
            });
        }
        Kind::Stack {
            axis,
            children,
            gap,
            align,
            offsets,
        } => {
            let mut cursor = 0.0;
            for (i, child) in children.iter().enumerate() {
                let along = offsets.as_ref().map_or(cursor, |o| o[i]);
                let across = match align {
                    Align::Center => line(node, *axis) - line(child, *axis),
                    Align::End => cross(node, *axis) - cross(child, *axis),
                    Align::Start => 0.0,
                };
                let (cx, cy) = match axis {
                    Axis::Row => (x + along, y + across),
                    Axis::Col => (x + across, y + along),
                };
                place(child, cx, cy, out);
                cursor = along + main(child, *axis) + gap;
            }
        }
    }
}

/// The lattice a pin connects on: the 50-mil grid every KiCAD symbol's pins are drawn on,
/// and the one `pin_endpoint` snaps a wire's terminal to. Rounding an alignment line any
/// coarser than this collapses two pin lines a single grid step apart onto one.
fn snap(v: f64) -> f64 {
    geom::GRID_50_MIL.snap(v)
}
