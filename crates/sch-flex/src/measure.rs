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

use geom::{Dir, Point2, Rect};
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
/// Clearance (mm) a piece of text keeps from whatever it is drawn beside — half a
/// character. Unlike the gap between two bodies it holds no wire, only white space.
const TEXT_GAP: f64 = 1.27;

/// One rectangle a node draws, in its own box's coordinates.
#[derive(Clone, Copy)]
struct Ink {
    r: Rect,
    /// Text or a glyph hanging off a pin rather than a symbol body: no wire runs through
    /// it, so it needs white space beside it, not a channel.
    text: bool,
}

impl Ink {
    fn shifted(self, dx: f64, dy: f64) -> Ink {
        Ink {
            r: Rect::new(
                self.r.min_x + dx,
                self.r.min_y + dy,
                self.r.max_x + dx,
                self.r.max_y + dy,
            ),
            text: self.text,
        }
    }
}

/// A measured node: a box, an alignment line, and how it is built.
///
/// The BOX is connection geometry only — bodies and pin stubs — because that is what a
/// container packs its children into and lines them up on. What the node actually DRAWS,
/// labels and all, is `ink`: kept clear of its neighbours, never summed into a row's
/// width, so a label hangs into the gap beside a shorter part instead of widening every
/// part that shares its column.
pub struct Node {
    pub w: f64,
    pub h: f64,
    /// Alignment line, as an offset from the box's top-left corner.
    pub ax: f64,
    pub ay: f64,
    ink: Vec<Ink>,
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
        self.nets_of(parts, |_| true)
    }

    /// The subset carried by parts a chain runs THROUGH, as opposed to one that ends at
    /// an IC pin.
    fn chain_nets(&self, parts: &[Part]) -> BTreeSet<String> {
        self.nets_of(parts, |p| p.links_a_chain())
    }

    fn nets_of(&self, parts: &[Part], keep: fn(&Part) -> bool) -> BTreeSet<String> {
        match &self.kind {
            Kind::Leaf { part, .. } => parts[*part]
                .nets()
                .into_iter()
                .filter(|net| !circuit_graph::netclass::is_power_net(net))
                .filter(|_| keep(&parts[*part]))
                .map(str::to_owned)
                .collect(),
            Kind::Stack { children, .. } => children
                .iter()
                .flat_map(|c| c.nets_of(parts, keep))
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
                        ..default_pose(&parts[i], axis, false)
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
        ink: Vec::new(),
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
    let r = parts[part].body(pose);
    let anchor = Point2::new(-r.min_x, -r.min_y);
    let mut ink = vec![Ink {
        r: Rect::new(0.0, 0.0, r.width(), r.height()),
        text: false,
    }];
    ink.extend(parts[part].overhang(pose).into_iter().map(|o| Ink {
        r: Rect::new(
            o.min_x + anchor.x,
            o.min_y + anchor.y,
            o.max_x + anchor.x,
            o.max_y + anchor.y,
        ),
        text: true,
    }));
    let mut node = Node {
        w: r.width(),
        h: r.height(),
        ax: anchor.x,
        ay: anchor.y,
        ink,
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

/// Re-pose the container's own two-pin leaves now that their siblings are known.
///
/// A resistor off a rail is a LEG — it stands — only when no other part the signal passes
/// THROUGH reaches the net on its far pin. When one does, the two are links of one chain
/// drawn inside this container, and standing a link bends its wire around its own body: a
/// divider's top resistor, a 555's timing resistor, an LED's series resistor. A net that
/// instead ends at an IC pin is a leg's far end, however many parts hang off it there.
///
/// Only the container knows its siblings, and only once they are measured, so the leaves
/// are measured first and the ones this verdict changes are measured again.
fn stand_the_legs(children: &mut [Node], trees: &[Tree], parts: &[Part], axis: Axis) {
    let reach: Vec<BTreeSet<String>> = children.iter().map(|k| k.chain_nets(parts)).collect();
    for (i, (node, tree)) in children.iter_mut().zip(trees).enumerate() {
        let Tree::Leaf(leaf) = tree else { continue };
        if leaf.rot.is_some() {
            continue;
        }
        let Kind::Leaf { part, .. } = node.kind else {
            continue;
        };
        let chained = parts[part].nets().into_iter().any(|net| {
            !circuit_graph::netclass::is_power_net(net)
                && reach
                    .iter()
                    .enumerate()
                    .any(|(j, nets)| j != i && nets.contains(net))
        });
        let pose = Pose {
            mirror: leaf.mirror,
            ..default_pose(&parts[part], axis, chained)
        };
        *node = leaf_node(part, parts, pose, axis);
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
    stand_the_legs(&mut children, &c.children, parts, c.axis);
    if let Some(wrapped) = wrap(c, &children, parts) {
        let mut node = container_node(&wrapped, parts, index, facing);
        share_tracks(&mut node, wrap_limit(c));
        return node;
    }
    face_neighbours(&mut children, &c.children, parts, c.axis, facing);
    if c.axis == Axis::Row {
        align_columns_to_ic_pins(&mut children, parts);
    }
    let gap = spacing(c, &children, parts);
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
    let crosses: Vec<f64> = children
        .iter()
        .map(|k| cross_offset(k, c.axis, c.align, thickness, at_line))
        .collect();
    let offsets = sweep(&children, &crosses, c.axis, gap);
    let span = children
        .iter()
        .zip(&offsets)
        .map(|(k, at)| at + main(k, c.axis))
        .fold(0.0, f64::max);
    let head = children.first().map_or(span / 2.0, |k| line(k, flip(c.axis)));
    let ink = stack_ink(&children, &offsets, &crosses, c.axis);
    let (w, h, ax, ay) = match c.axis {
        Axis::Row => (span, thickness, head, at_line),
        Axis::Col => (thickness, span, at_line, head),
    };
    Node {
        w,
        h,
        ax,
        ay,
        ink,
        kind: Kind::Stack {
            axis: c.axis,
            children,
            gap,
            align: c.align,
            offsets: Some(offsets),
        },
    }
}

/// Where a child sits ACROSS its container's axis — the same arithmetic [`place`] does,
/// needed at measure time because where a label collides depends on it.
fn cross_offset(child: &Node, axis: Axis, align: Align, thickness: f64, at_line: f64) -> f64 {
    match align {
        Align::Center => at_line - line(child, axis),
        Align::End => thickness - cross(child, axis),
        Align::Start => 0.0,
    }
}

/// Main-axis offsets for a container's children.
///
/// Seating each child as far back as its own text allows packs tightest, but it gives a
/// row of identical capacitors a different pitch at every step, and ragged pitch is the
/// defect the eye reads first — worse than the millimetres it saves. So the tight seating
/// is measured and then EVENED OUT over each run of same-size siblings: a bank of like
/// parts comes out on one pitch, whatever is written beside any one of them, while the
/// step to a part of another size stays as tight as it can be.
fn sweep(children: &[Node], crosses: &[f64], axis: Axis, gap: f64) -> Vec<f64> {
    let mut tight: Vec<f64> = Vec::with_capacity(children.len());
    for (i, child) in children.iter().enumerate() {
        let placed: Vec<(f64, f64, &Node)> = tight
            .iter()
            .enumerate()
            .map(|(j, at)| (*at, crosses[j], &children[j]))
            .collect();
        tight.push(seat_next(&placed, child, crosses[i], axis, gap));
    }
    let mut gaps: Vec<f64> = (1..children.len())
        .map(|i| tight[i] - tight[i - 1] - main(&children[i - 1], axis))
        .collect();
    for run in runs(children, axis) {
        let widest = run.clone().map(|i| gaps[i]).fold(gap, f64::max);
        for i in run {
            gaps[i] = widest;
        }
    }
    let mut offsets = vec![0.0];
    for i in 1..children.len() {
        offsets.push(offsets[i - 1] + main(&children[i - 1], axis) + gaps[i - 1]);
    }
    offsets
}

/// The gap indices inside each run of three or more siblings that measure the same along
/// `axis` — the banks of like parts whose pitch the eye checks.
fn runs(children: &[Node], axis: Axis) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut from = 0;
    for i in 1..=children.len() {
        let same = i < children.len()
            && (main(&children[i], axis) - main(&children[from], axis)).abs() < geom::EPS;
        if !same {
            if i - from > 2 {
                out.push(from..i - 1);
            }
            from = i;
        }
    }
    out
}

/// The main-axis offset `next` needs to clear everything in `placed`.
///
/// Two BODIES keep the container's whole `gap` between them whether or not they line up
/// across the axis: that gap is the channel the wire between them runs in. Two pieces of
/// TEXT only have to miss each other where they land.
///
/// Text against a BODY asks for nothing. A body box already carries its own pad of white
/// space on every side, so holding text off the box means holding it three millimetres off
/// the nearest ink — clearance paid for twice. Measured over the corpus the whole
/// text-against-body rule bought 1.6% of area and 4% of the writer's overlap warnings, and
/// it cost the even pitch of every bank it touched: one label under one capacitor set the
/// spacing for the whole row.
fn seat_next(
    placed: &[(f64, f64, &Node)],
    next: &Node,
    next_cross: f64,
    axis: Axis,
    gap: f64,
) -> f64 {
    let mut at = 0.0f64;
    for (main, cross, node) in placed {
        for a in &node.ink {
            let a_end = span(&a.r, axis).1;
            let (a_lo, a_hi) = cross_span(&a.r, axis);
            for b in &next.ink {
                if a.text != b.text {
                    continue;
                }
                let b_start = span(&b.r, axis).0;
                let clear = if a.text {
                    let (b_lo, b_hi) = cross_span(&b.r, axis);
                    if a_hi + cross <= b_lo + next_cross + TEXT_GAP
                        || b_hi + next_cross <= a_lo + cross + TEXT_GAP
                    {
                        continue;
                    }
                    TEXT_GAP
                } else {
                    gap
                };
                at = at.max(main + a_end + clear - b_start);
            }
        }
    }
    at
}

/// A rectangle's reach along `axis`.
fn span(r: &Rect, axis: Axis) -> (f64, f64) {
    match axis {
        Axis::Row => (r.min_x, r.max_x),
        Axis::Col => (r.min_y, r.max_y),
    }
}

/// A rectangle's reach across `axis`.
fn cross_span(r: &Rect, axis: Axis) -> (f64, f64) {
    span(r, flip(axis))
}

/// Everything a stack's children draw, in the stack's own coordinates.
fn stack_ink(children: &[Node], offsets: &[f64], crosses: &[f64], axis: Axis) -> Vec<Ink> {
    children
        .iter()
        .zip(offsets)
        .zip(crosses)
        .flat_map(|((child, main), cross)| {
            let (dx, dy) = match axis {
                Axis::Row => (*main, *cross),
                Axis::Col => (*cross, *main),
            };
            child.ink.iter().map(move |k| k.shifted(dx, dy))
        })
        .collect()
}

/// Recompute a stack's ink after its children moved along its axis.
fn rebuild_ink(node: &mut Node) {
    let Kind::Stack {
        axis,
        children,
        gap,
        align,
        offsets,
    } = &node.kind
    else {
        return;
    };
    let (thickness, at_line) = match axis {
        Axis::Row => (node.h, node.ay),
        Axis::Col => (node.w, node.ax),
    };
    let crosses: Vec<f64> = children
        .iter()
        .map(|k| cross_offset(k, *axis, *align, thickness, at_line))
        .collect();
    let mains: Vec<f64> = match offsets {
        Some(offsets) => offsets.clone(),
        None => children
            .iter()
            .scan(0.0, |cursor, k| {
                let at = *cursor;
                *cursor += main(k, *axis) + gap;
                Some(at)
            })
            .collect(),
    };
    node.ink = stack_ink(children, &mains, &crosses, *axis);
}

/// The children of a stack; nothing, for a leaf.
fn kids(node: &Node) -> &[Node] {
    match &node.kind {
        Kind::Stack { children, .. } => children,
        Kind::Leaf { .. } => &[],
    }
}

/// Put wrapped bands on SHARED COLUMN TRACKS: the k-th part of every band starts on one
/// line, so the grid reads down its columns as well as across its rows — which is how a
/// person draws repeated channels, and the thing our sheets most visibly get wrong.
///
/// The tracks are a GRID, not a per-column squeeze: column `k` is as wide as the widest
/// part any band puts there, and one pitch separates every pair. Fitting each column to
/// the band that happens to need least would line the bands up and leave the pitch as
/// ragged as it started, which is half the defect this is here to fix.
///
/// The budget is the WRAP LIMIT the bands were folded to fit, not the width they happened
/// to come out at. Holding a grid to "no wider than the ribbon already was" rejects it for
/// a millimetre and buys nothing back: the bands were chosen against `limit`, so a grid
/// inside `limit` costs no page. Measuring a part's box without its label overhang is what
/// leaves room in that budget — same-kind parts then measure the same, whatever is written
/// beside them.
fn share_tracks(node: &mut Node, limit: f64) {
    let Kind::Stack {
        axis: outer,
        children: bands,
        align,
        ..
    } = &node.kind
    else {
        return;
    };
    let (outer, align, inner) = (*outer, *align, flip(*outer));
    if bands.len() < 2 {
        return;
    }
    let mut bandwise: Vec<(Vec<f64>, f64)> = Vec::new();
    for band in bands {
        let Kind::Stack {
            axis,
            children,
            gap,
            align,
            ..
        } = &band.kind
        else {
            return;
        };
        if *axis != inner || children.is_empty() {
            return;
        }
        let (thickness, at_line) = match axis {
            Axis::Row => (band.h, band.ay),
            Axis::Col => (band.w, band.ax),
        };
        bandwise.push((
            children
                .iter()
                .map(|k| cross_offset(k, *axis, *align, thickness, at_line))
                .collect(),
            *gap,
        ));
    }
    let width = bands.iter().map(|b| kids(b).len()).max().unwrap_or(0);
    let reach = |k: usize, past: bool| {
        bands
            .iter()
            .filter_map(|band| kids(band).get(k))
            .map(|child| match past {
                true => main(child, inner) - line(child, flip(inner)),
                false => line(child, flip(inner)),
            })
            .fold(0.0, f64::max)
    };
    let columns: Vec<(f64, f64)> = (0..width).map(|k| (reach(k, false), reach(k, true))).collect();
    let Some(tracks) = grid_pitch(bands, &bandwise, &columns, inner) else {
        return;
    };
    let seats: Vec<Vec<f64>> = bands.iter().map(|band| seats_on(band, &tracks, inner)).collect();
    let widest = bands.iter().map(|b| main(b, inner)).fold(0.0, f64::max);
    let lengths: Vec<f64> = bands
        .iter()
        .zip(&seats)
        .map(|(band, seats)| {
            kids(band)
                .iter()
                .zip(seats)
                .map(|(child, at)| at + main(child, inner))
                .fold(0.0, f64::max)
        })
        .collect();
    let longest = lengths.iter().copied().fold(0.0, f64::max);
    // A track that starts before the block's own left edge would put the band's first
    // part outside the frame everything else is measured against.
    let head = seats
        .iter()
        .filter_map(|s| s.first().copied())
        .fold(0.0, f64::min);
    if longest > widest.max(limit) + geom::EPS || head < -geom::EPS {
        return;
    }
    let Kind::Stack {
        children: bands, ..
    } = &mut node.kind
    else {
        return;
    };
    for ((band, length), seats) in bands.iter_mut().zip(&lengths).zip(seats) {
        match inner {
            Axis::Row => band.w = *length,
            Axis::Col => band.h = *length,
        }
        if let Kind::Stack { offsets, .. } = &mut band.kind {
            *offsets = Some(seats);
        }
        rebuild_ink(band);
    }
    match outer {
        Axis::Row => node.h = longest,
        Axis::Col => node.w = longest,
    }
    if align != Align::Center {
        match outer {
            Axis::Row => node.ay = longest / 2.0,
            Axis::Col => node.ax = longest / 2.0,
        }
    }
    rebuild_ink(node);
}

/// Where a band's children sit so each one's ALIGNMENT LINE lands on its track.
///
/// A track is a line, not a box edge. Seating boxes would line up the left sides of parts
/// of different widths and leave their pin axes — the coordinate a wire and the eye both
/// read a column by — as scattered as before.
fn seats_on(band: &Node, tracks: &[f64], inner: Axis) -> Vec<f64> {
    kids(band)
        .iter()
        .zip(tracks)
        .map(|(child, track)| track - line(child, flip(inner)))
        .collect()
}

/// Track lines for a grid whose columns reach `columns.0` before and `columns.1` past
/// each one, under ONE pitch: the smallest pitch at which no band's text runs into what
/// that band already drew to its left.
///
/// What a collision asks for depends on where the tracks are, and the tracks depend on the
/// pitch, so it is solved by widening — start at the bands' own gap and open it until
/// nothing is short. `None` if it does not settle, which leaves the bands as they were
/// rather than shipping a grid whose text overlaps.
fn grid_pitch(
    bands: &[Node],
    bandwise: &[(Vec<f64>, f64)],
    columns: &[(f64, f64)],
    inner: Axis,
) -> Option<Vec<f64>> {
    let lay = |pitch: f64| -> Vec<f64> {
        columns
            .iter()
            .scan(None, |past: &mut Option<f64>, (before, after)| {
                let track = match *past {
                    None => *before,
                    Some(end) => end + pitch + before,
                };
                *past = Some(track + after);
                Some(track)
            })
            .collect()
    };
    let mut pitch = bandwise.iter().map(|(_, gap)| *gap).fold(0.0, f64::max);
    // Each pass opens the pitch by the worst shortfall spread over the tracks before it,
    // so the widening converges from below: a grid of a dozen columns needs a dozen passes,
    // not a handful. At eight, half of them were being thrown away unsettled.
    for _ in 0..64 {
        let tracks = lay(pitch);
        let mut want = pitch;
        for ((band, (crosses, gap)), seats) in bands
            .iter()
            .zip(bandwise)
            .zip(bands.iter().map(|b| seats_on(b, &tracks, inner)))
        {
            let members = kids(band);
            for k in 1..members.len() {
                let placed: Vec<(f64, f64, &Node)> = (0..k)
                    .map(|j| (seats[j], crosses[j], &members[j]))
                    .collect();
                let need = seat_next(&placed, &members[k], crosses[k], inner, *gap);
                if need > seats[k] + geom::EPS {
                    want = want.max(pitch + (need - seats[k]) / k as f64);
                }
            }
        }
        if want <= pitch + geom::EPS {
            return Some(tracks);
        }
        pitch = want;
    }
    None
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
    let limit = wrap_limit(c);
    let gap = spacing(c, children, parts);
    let sizes: Vec<f64> = children.iter().map(|k| drawn(k, c.axis)).collect();
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

/// How far a container may run along its own axis before it has to fold.
fn wrap_limit(c: &Container) -> f64 {
    c.wrap.unwrap_or(match c.axis {
        Axis::Row => WRAP_WIDTH,
        Axis::Col => WRAP_HEIGHT,
    }) * UNIT_MM
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
            band.iter().map(|k| drawn(k, axis)).sum::<f64>() + gap * (len - 1) as f64,
        );
        thick += band.iter().map(|k| drawn(k, flip(axis))).fold(0.0, f64::max)
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

/// How far the node's DRAWING reaches along `axis` — its box plus every label that hangs
/// off it. What a page has to hold, as opposed to [`main`], which is what a sibling has to
/// make room for. Folding a ribbon on the packing width instead would leave a row of
/// labelled parts unwrapped and run its text off the paper.
fn drawn(node: &Node, axis: Axis) -> f64 {
    let (lo, hi) = node.ink.iter().fold((0.0f64, main(node, axis)), |(lo, hi), k| {
        let (a, b) = span(&k.r, axis);
        (lo.min(a), hi.max(b))
    });
    hi - lo
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
            // Turn the device to face the row it stands in, then line it up on the pin
            // that reaches its neighbour.
            let (left, right) = side_nets(children, parts, i);
            let pose = match axis == Axis::Row
                && turning_faces_more(part_ref, pose, &left, &right)
            {
                true => {
                    let turned = Pose { mirror: !pose.mirror, ..pose };
                    children[i] = leaf_node(part, parts, turned, axis);
                    turned
                }
                false => pose,
            };
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
///
/// A neighbour that is a GROUP counts for every net it carries: the column of pull-ups
/// beside a translator is what the translator has to face, and reading only leaf siblings
/// left every device with a composed neighbour facing whichever way its symbol was drawn.
fn neighbour_nets(
    children: &[Node],
    parts: &[Part],
    i: usize,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let of = |j: Option<usize>| {
        j.and_then(|j| children.get(j))
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

/// The signal nets of everything before and after child `i` in its container.
///
/// Which way a device should FACE is decided against the whole row, not the part next to
/// it: a device whose output net is read three seats to its right still belongs with that
/// pin on the right, however its immediate neighbour is wired.
fn side_nets(children: &[Node], parts: &[Part], i: usize) -> (BTreeSet<String>, BTreeSet<String>) {
    let of = |range: &[Node]| -> BTreeSet<String> {
        range.iter().flat_map(|k| k.signal_nets(parts)).collect()
    };
    (of(&children[..i]), of(&children[i + 1..]))
}

/// Whether flipping a device left-for-right puts MORE of its pins on the side the parts
/// sharing their net are on.
///
/// The same convention as the one that turns a connector at the end of a row, applied to
/// the device in the middle of it: a level translator whose B pins are drawn on the right
/// but whose B-side parts the author composed on the left is drawn backwards, and every
/// net across it becomes a label and a detour. Nothing else about the symbol changes —
/// mirroring leaves a pin's rail direction alone, so a supply still leaves at the top.
fn turning_faces_more(
    part: &Part,
    pose: Pose,
    before: &BTreeSet<String>,
    after: &BTreeSet<String>,
) -> bool {
    let facing = |pose: Pose| {
        part.pins
            .iter()
            .filter(|pin| {
                let dir = part.pin_dir(pin, pose);
                part.net(pin).is_some_and(|net| match dir {
                    Dir::West => before.contains(net),
                    Dir::East => after.contains(net),
                    _ => false,
                })
            })
            .count()
    };
    let turned = Pose {
        mirror: !pose.mirror,
        ..pose
    };
    // Only a device drawn ENTIRELY backwards is turned. Trading one wired side for the
    // other reverses the convention the symbol was drawn with — a 555 whose timing parts
    // happen to sit on its right ends up with its output pointing back into the block —
    // and buys nothing the alignment line does not already give.
    facing(pose) == 0 && facing(turned) > 0
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
    rebuild_ink(col);
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
