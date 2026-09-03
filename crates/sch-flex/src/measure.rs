//! Measure the tree, then place it: every box's size, every alignment line, every
//! coordinate.
//!
//! A measured node carries two reference points. Its BOX is the room the drawing claims.
//! Its ALIGNMENT LINE is the line a container lines its children up on — for a series part
//! that is its pin axis, for a shunt standing across a row it is the pin that touches the
//! row, and for a part beside an IC it is the pin that reaches the IC. Aligning lines
//! rather than boxes is what makes a wire leave one pin and arrive at the next without a
//! bend.

use std::collections::BTreeSet;

use geom::{Dir, Point2};
use sch_model::tree::{Align, Axis, Container, DEFAULT_GAP, Tree, UNIT_MM, WRAP};

use crate::orient::{authored_pose, default_pose};
use crate::part::{Part, Pose};

/// Gap floor (grid units) around a part with many pins: an IC needs a channel its pin
/// text does not spill across.
const IC_GAP: f64 = 10.0;
/// A part with at least this many pins is an IC for spacing and alignment purposes.
const IC_PINS: usize = 3;

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

/// Measure `tree` and lay it out with its top-left at the origin.
pub fn typeset_block(tree: &Tree, parts: &[Part], index: &dyn Fn(&str, u8) -> Option<usize>) -> Vec<Placed> {
    let node = measure(tree, parts, index, Axis::Row);
    let mut out = Vec::new();
    place(&node, 0.0, 0.0, parts, &mut out);
    out
}

fn measure(
    tree: &Tree,
    parts: &[Part],
    index: &dyn Fn(&str, u8) -> Option<usize>,
    axis: Axis,
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
        Tree::Container(c) => container_node(c, parts, index),
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

fn container_node(c: &Container, parts: &[Part], index: &dyn Fn(&str, u8) -> Option<usize>) -> Node {
    let mut children: Vec<Node> = c
        .children
        .iter()
        .map(|child| measure(child, parts, index, c.axis))
        .collect();
    if let Some(wrapped) = wrap_row(c, &children) {
        return container_node(&wrapped, parts, index);
    }
    face_neighbours(&mut children, &c.children, parts, c.axis);
    if c.axis == Axis::Row {
        align_columns_to_ic_pins(&mut children, parts);
    }
    let big = children.iter().any(|k| ic_leaf(k, parts));
    let gap = c.gap.unwrap_or(DEFAULT_GAP).max(if big { IC_GAP } else { 0.0 }) * UNIT_MM;
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

/// A row wider than a sheet column is not a path a reader can follow: break it into
/// stacked rows of the same children, in order. `None` when it already fits.
fn wrap_row(c: &Container, children: &[Node]) -> Option<Container> {
    if c.axis != Axis::Row || c.children.len() < 2 {
        return None;
    }
    let limit = c.wrap.unwrap_or(WRAP) * UNIT_MM;
    let gap = c.gap.unwrap_or(DEFAULT_GAP) * UNIT_MM;
    let width: f64 =
        children.iter().map(|k| k.w).sum::<f64>() + gap * (children.len() - 1) as f64;
    if width <= limit {
        return None;
    }
    let mut rows: Vec<Vec<Tree>> = vec![Vec::new()];
    let mut used = 0.0;
    for (child, node) in c.children.iter().zip(children) {
        let last = rows.last_mut().expect("one row exists");
        if !last.is_empty() && used + gap + node.w > limit {
            rows.push(vec![child.clone()]);
            used = node.w;
        } else {
            used += if last.is_empty() { node.w } else { gap + node.w };
            last.push(child.clone());
        }
    }
    if rows.len() < 2 {
        return None;
    }
    Some(Container {
        axis: Axis::Col,
        children: rows
            .into_iter()
            .map(|row| {
                Tree::Container(Container {
                    axis: Axis::Row,
                    children: row,
                    gap: c.gap,
                    align: c.align,
                    // Already sized to the limit: a second pass must not split it again.
                    wrap: Some(f64::INFINITY),
                })
            })
            .collect(),
        gap: Some(DEFAULT_GAP),
        align: Align::Start,
        wrap: None,
    })
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
fn face_neighbours(children: &mut [Node], authored: &[Tree], parts: &[Part], axis: Axis) {
    for i in 0..children.len() {
        let Kind::Leaf { part, pose, .. } = children[i].kind else {
            continue;
        };
        if matches!(&authored[i], Tree::Leaf(l) if l.rot.is_some()) {
            continue;
        }
        let (before, after) = neighbour_nets(children, parts, i);
        let part_ref = &parts[part];
        if part_ref.is_connector() {
            if connector_faces_away(part_ref, pose, axis, i, children.len()) {
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
fn seat_column(col: &mut Node, parts: &[Part], lines: &[(String, f64)]) -> bool {
    let Kind::Stack { children, gap, .. } = &col.kind else {
        return false;
    };
    let gap = *gap;
    let targets: Vec<Option<f64>> = children
        .iter()
        .map(|child| {
            let nets = child.signal_nets(parts);
            lines
                .iter()
                .filter(|(net, _)| nets.contains(net))
                .map(|(_, y)| *y)
                .min_by(f64::total_cmp)
        })
        .collect();
    if targets.iter().all(Option::is_none) {
        return false;
    }
    let (mut cursor, mut anchor_line, mut offsets) = (0.0f64, None, Vec::new());
    for (child, target) in children.iter().zip(&targets) {
        let mut y = cursor;
        if let Some(t) = target {
            match anchor_line {
                None => anchor_line = Some(y + child.ay - t),
                Some(l) => y = cursor.max(l + t - child.ay),
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
fn place(node: &Node, x: f64, y: f64, parts: &[Part], out: &mut Vec<Placed>) {
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
                place(child, cx, cy, parts, out);
                cursor = along + main(child, *axis) + gap;
            }
        }
    }
}

/// Schematic parts sit on the 100-mil lattice: half the pitch of the sheet grid, and the
/// spacing every KiCAD symbol's pins are drawn on.
fn snap(v: f64) -> f64 {
    (v / (2.0 * UNIT_MM)).round() * 2.0 * UNIT_MM
}
