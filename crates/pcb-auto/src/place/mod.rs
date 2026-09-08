//! Connectivity-aware placement: anchors by intent, satellites by nets, legal by construction.
//!
//! [`plan_placement`] seeds, clusters, legalises and locally searches; best of [`restarts`] seeded
//! runs, every loop bound by a count so a board and a seed give the same plan. Units are mm.

pub mod cluster;
pub mod edges;

use std::collections::{BTreeMap, BTreeSet};

use crate::geom::{BBox, Point, rotate, seg_hits_box};
use crate::model::{Board, Footprint};

pub use edges::{is_connector, seat_on_edges};

pub const BIG_NET_PADS: usize = 8;
/// Extra courtyard margin around mounting holes (each side).
pub const HOLE_GAP: f64 = 0.5;
/// Copper ring around a drill that has to be on the board; the washer may overhang.
pub const HOLE_ANNULUS: f64 = 0.25;
/// Median courtyard area / (outline inset 1 mm) area over human boards in the dataset.
pub const HUMAN_DENSITY: f64 = 0.6;
/// How much of the uniform-density push applies inside one cluster.
pub const SAME_GROUP_SPREAD: f64 = 0.5;
/// Parts per functional cluster the seeder aims for.
pub const CLUSTER_SIZE: f64 = 12.0;
/// Pull along a power rail, split over the rail's members.
pub const RAIL_ATTRACT: f64 = 1.0;
/// A part this far off another part's row or column is snapped onto it.
pub const TIDY_TOL: f64 = 1.5;
/// Pull along a net with exactly two pads, against 1.0 for any other signal.
pub const TWO_PAD_ATTRACT: f64 = 2.0;
/// A rail of no more pads than this is a block's supply, not a board-wide bus ...
pub const SMALL_RAIL_PADS: usize = 16;
/// ... so it groups its members like a net rather than a rail (`RAIL_ATTRACT`) ...
pub const SMALL_RAIL_ATTRACT: f64 = 3.0;
/// ... and a pose is charged for the length of its hop, not the 0.3 of a bus.
pub const SMALL_RAIL_HOP: f64 = 0.7;
/// Seeded placement runs per call at most; the best by wirelength is kept.
pub const RESTARTS: usize = 12;
/// Restarts step by this so seed and seed+1 do not plan the same boards.
pub const SEED_STRIDE: u64 = 1009;
/// A run costs about `n_parts^2`, so a board gets `RESTART_WORK / n_parts^2` runs -- counts, not
/// seconds, so a seed plans the same boards anywhere.
pub const RESTART_WORK: usize = 42_000;
/// Descent sweeps of the HPWL local search (relocate/swap families) per run.
pub const REFINE_SWEEPS: usize = 20;
/// ... and `REFINE_PROBES / n_parts` move probes, whichever comes first.
pub const REFINE_PROBES: usize = 240_000;
/// Safety stop only: a call past this finishes the run it is in and notes it.
pub const PLACE_CEILING_S: f64 = 90.0;
/// ... same for one local search.
pub const REFINE_CEILING_S: f64 = 20.0;
/// mm the local search will look away from a wanted spot for a free one.
pub const REFINE_REACH: f64 = 14.0;
/// Pull a `near` group's member towards its anchor (outvotes any net).
pub const NEAR_ATTRACT: f64 = 8.0;
/// Default mm a `near` group's members must stay within of the anchor.
pub const NEAR_RADIUS: f64 = 12.0;
/// Floor for the copper-to-edge margin around a mounting hole's pad.
pub const HOLE_EDGE_GAP: f64 = 0.1;
/// ... over the board's own edge clearance, so a seat sits INSIDE the rule.
pub const EDGE_SEAT_SLACK: f64 = 0.05;
/// How far in from the outline a courtyard has to stay.
pub const OUTLINE_INSET: f64 = 1.0;

/// One footprint's new pose.
#[derive(Debug, Clone, PartialEq)]
pub struct Move {
    pub ref_: String,
    pub x: f64,
    pub y: f64,
    pub rot: f64,
    pub side: String,
}

/// What edge a connector actually got, and how far its courtyard stands off it.
#[derive(Debug, Clone, PartialEq)]
pub struct SeatReport {
    pub side_requested: String,
    pub side_used: String,
    pub gap_mm: f64,
}

/// The outcome of one [`plan_placement`] call.
#[derive(Debug, Clone, Default)]
pub struct PlacementPlan {
    pub moves: Vec<Move>,
    /// `(ref, reason, least-illegal pose)` for a part that could not be seated.
    pub unplaced: Vec<(String, String, Option<(f64, f64, f64)>)>,
    pub wirelength_before: f64,
    pub wirelength_after: f64,
    pub overlaps_after: usize,
    pub notes: Vec<String>,
    pub seated: BTreeMap<String, SeatReport>,
}

/// Placement inputs. Every map is ordered, so a seed gives one plan.
#[derive(Debug, Clone)]
pub struct PlanOptions {
    pub anchors: BTreeMap<String, (f64, f64)>,
    pub fixed: BTreeSet<String>,
    /// ref -> `left|right|top|bottom|any`.
    pub edge_for: BTreeMap<String, String>,
    /// Seat every auto-detected connector as well as the `edge_for` refs.
    pub seat_connectors: bool,
    /// `(anchor, members, radius_mm)`.
    pub near: Vec<(String, Vec<String>, f64)>,
    pub spacing: f64,
    pub grid: f64,
    pub spread: f64,
    pub seed: u64,
    pub restarts: Option<usize>,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            anchors: BTreeMap::new(),
            fixed: BTreeSet::new(),
            edge_for: BTreeMap::new(),
            seat_connectors: false,
            near: Vec::new(),
            spacing: 0.5,
            grid: 0.5,
            spread: HUMAN_DENSITY,
            seed: 1,
            restarts: None,
        }
    }
}

/// What a part is doing in the plan: `Free` parts are seeded and searched, `Satellite`s follow a
/// parent's courtyard, `Anchor`s start where the caller asked and `Fixed` parts do not move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Fixed,
    Anchor,
    Satellite,
    Free,
}

/// A footprint reduced to what placement needs: pad offsets and a keep-out box at rotation 0.
#[derive(Debug, Clone)]
pub struct Part {
    pub ref_: String,
    pub fp: Footprint,
    /// `(pad number, net id, offset at rotation 0 on this side)`.
    pub pads: Vec<(String, i64, Point)>,
    /// Local keep-out box at rotation 0, relative to `pos`.
    pub crt: (f64, f64, f64, f64),
    pub x: f64,
    pub y: f64,
    pub rot: f64,
    pub side: String,
    pub movable: bool,
    pub role: Role,
    /// Functional cluster id; a satellite inherits its parent's.
    pub group: i64,
    pub seated: bool,
    pub parent: Option<String>,
    pub target: Option<Point>,
    pub seed: Option<Point>,
}

impl Part {
    pub fn pad_positions_at(&self, x: f64, y: f64, rot: f64) -> Vec<(i64, Point)> {
        self.pads
            .iter()
            .map(|(_, net, off)| {
                let d = rotate(*off, rot);
                (*net, (x + d.0, y + d.1))
            })
            .collect()
    }

    pub fn pad_positions(&self) -> Vec<(i64, Point)> {
        self.pad_positions_at(self.x, self.y, self.rot)
    }

    pub fn bbox_at(&self, x: f64, y: f64, rot: f64) -> BBox {
        let (x0, y0, x1, y1) = self.crt;
        let r = rot.rem_euclid(360.0);
        if r == 0.0 {
            BBox::new(x + x0, y + y0, x + x1, y + y1)
        } else if r == 180.0 {
            BBox::new(x - x1, y - y1, x - x0, y - y0)
        } else if r == 90.0 {
            // (px, py) -> (py, -px)
            BBox::new(x + y0, y - x1, x + y1, y - x0)
        } else if r == 270.0 {
            // (px, py) -> (-py, px)
            BBox::new(x - y1, y + x0, x - y0, y + x1)
        } else {
            BBox::of_points(
                [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
                    .into_iter()
                    .map(|p| {
                        let q = rotate(p, rot);
                        (x + q.0, y + q.1)
                    }),
            )
        }
    }

    pub fn bbox(&self) -> BBox {
        self.bbox_at(self.x, self.y, self.rot)
    }

    pub fn size(&self) -> (f64, f64) {
        (self.crt.2 - self.crt.0, self.crt.3 - self.crt.1)
    }

    pub fn area(&self) -> f64 {
        let (w, h) = self.size();
        w * h
    }
}

/// The part table, kept in board order (the order the springs and every sweep walk it in) with a
/// reference index beside it.
#[derive(Debug, Clone, Default)]
pub struct Parts {
    pub list: Vec<Part>,
    pub idx: BTreeMap<String, usize>,
}

impl Parts {
    pub fn push(&mut self, p: Part) {
        self.idx.insert(p.ref_.clone(), self.list.len());
        self.list.push(p);
    }
    pub fn index(&self, ref_: &str) -> Option<usize> {
        self.idx.get(ref_).copied()
    }
    pub fn get(&self, ref_: &str) -> Option<&Part> {
        self.index(ref_).map(|i| &self.list[i])
    }
    pub fn get_mut(&mut self, ref_: &str) -> Option<&mut Part> {
        self.index(ref_).map(|i| &mut self.list[i])
    }
    pub fn iter(&self) -> std::slice::Iter<'_, Part> {
        self.list.iter()
    }
    pub fn len(&self) -> usize {
        self.list.len()
    }
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
    pub fn boxes(&self) -> Vec<(String, BBox)> {
        self.list.iter().map(|p| (p.ref_.clone(), p.bbox())).collect()
    }
}

/// Pad offsets and the keep-out box of a part at rotation 0: courtyard UNIONED with pad copper
/// (many footprints draw no F.CrtYd), that copper grown by half of `clearance`.
pub fn local_geometry(
    fp: &Footprint,
    flip: bool,
    clearance: f64,
) -> (Vec<(String, i64, Point)>, (f64, f64, f64, f64)) {
    let mut pads = Vec::new();
    for p in &fp.pads {
        if p.net_id == 0 {
            continue;
        }
        let mut off = rotate((p.pos.0 - fp.pos.0, p.pos.1 - fp.pos.1), -fp.rot);
        if flip {
            off = (off.0, -off.1);
        }
        pads.push((p.number.clone(), p.net_id, off));
    }
    let b = fp.courtyard_bbox();
    let mut corners: Vec<Point> = if b.valid() {
        b.corners()
            .iter()
            .map(|&(cx, cy)| rotate((cx - fp.pos.0, cy - fp.pos.1), -fp.rot))
            .collect()
    } else {
        Vec::new()
    };
    // every pad, not only the netted ones: bare copper still clashes
    let mut copper = BBox::empty();
    for p in &fp.pads {
        copper.add_bbox(&p.bbox());
    }
    if copper.valid() {
        corners.extend(
            copper
                .inflate(clearance / 2.0)
                .corners()
                .iter()
                .map(|&(cx, cy)| rotate((cx - fp.pos.0, cy - fp.pos.1), -fp.rot)),
        );
    }
    if flip {
        for c in corners.iter_mut() {
            c.1 = -c.1;
        }
    }
    let lb = BBox::of_points(corners);
    (pads, (lb.x0, lb.y0, lb.x1, lb.y1))
}

/// The gap the rules demand between two pieces of copper: design clearance or pad override.
pub fn copper_clearance(board: &Board) -> f64 {
    let r = board.design_rules();
    r.clearance.max(r.pad_clearance.unwrap_or(0.0))
}

/// The board's own `edge_clearance` rule, never below [`HOLE_EDGE_GAP`].
pub fn board_edge_clearance(board: &Board) -> f64 {
    board.design_rules().edge_clearance.max(HOLE_EDGE_GAP)
}

/// The board-frame twin of [`local_geometry`]'s box: courtyard union pad copper + clearance/2.
pub fn keepout_bbox(fp: &Footprint, clearance: f64) -> BBox {
    let c = fp.courtyard_bbox();
    let mut box_ = if c.valid() { c } else { BBox::empty() };
    if clearance > 0.0 {
        for p in &fp.pads {
            box_.add_bbox(&p.bbox().inflate(clearance / 2.0));
        }
    }
    box_
}

/// The boxes another part must stay out of. (The Python carrier branch -- one box per pad for a
/// module whose courtyard IS the board -- is dropped: no carrier on a 2-layer board.)
pub fn obstacle_boxes(fp: &Footprint, clearance: f64) -> Vec<BBox> {
    let b = keepout_bbox(fp, clearance);
    if b.valid() { vec![b] } else { vec![] }
}

pub fn is_hole(fp: &Footprint) -> bool {
    fp.lib_id.contains("MountingHole") || fp.ref_.starts_with('H')
}

/// A [`Part`] at the footprint's own pose, flipped when `side` differs; `hole_gap = false`
/// measures a mounting hole on its true geometry instead of padding it.
pub fn part_from(
    fp: &Footprint,
    movable: bool,
    side: Option<&str>,
    clearance: f64,
    hole_gap: bool,
) -> Part {
    let side = side.unwrap_or(fp.side()).to_string();
    let flip = side != fp.side();
    let (pads, mut crt) = local_geometry(fp, flip, clearance);
    if hole_gap && is_hole(fp) {
        crt = (
            crt.0 - HOLE_GAP,
            crt.1 - HOLE_GAP,
            crt.2 + HOLE_GAP,
            crt.3 + HOLE_GAP,
        );
    }
    Part {
        ref_: fp.ref_.clone(),
        fp: fp.clone(),
        pads,
        crt,
        x: fp.pos.0,
        y: fp.pos.1,
        rot: if flip { -fp.rot } else { fp.rot },
        side,
        movable,
        role: Role::Free,
        group: -1,
        seated: false,
        parent: None,
        target: None,
        seed: None,
    }
}

/// Bboxes of the rule areas that forbid footprints. (Footprint-local keepout zones are dropped:
/// the model exposes only board-level zones.)
pub fn keepout_boxes(board: &Board) -> Vec<BBox> {
    let mut out = Vec::new();
    for z in board.zones() {
        let Some(flags) = &z.keepout else { continue };
        if flags.get("footprints").copied().unwrap_or(true) {
            continue;
        }
        if z.polygon.len() >= 3 {
            out.push(BBox::of_points(z.polygon.iter().copied()));
        }
    }
    out
}

/// A four-point axis-aligned polygon: its bbox says everything, so skip the polygon maths.
pub fn is_rect(poly: &[Point]) -> bool {
    if poly.len() != 4 {
        return false;
    }
    let bb = BBox::of_points(poly.iter().copied());
    poly.iter()
        .all(|p| (p.0 - bb.x0).abs() < 1e-6 || (p.0 - bb.x1).abs() < 1e-6)
        && poly
            .iter()
            .all(|p| (p.1 - bb.y0).abs() < 1e-6 || (p.1 - bb.y1).abs() < 1e-6)
}

/// The area a courtyard may occupy: outline bbox (fast reject), outline polygon when it is not a
/// rectangle, and the keepout rule areas. [`Region::accepts`] is the predicate every pass asks.
#[derive(Debug, Clone)]
pub struct Region {
    pub bbox: BBox,
    pub poly: Option<Vec<Point>>,
    pub edges: Vec<(Point, Point)>,
    pub no_go: Vec<BBox>,
    pub inset: f64,
    /// Anything beyond the bbox test to do?
    pub strict: bool,
}

impl Region {
    pub fn new(bbox: BBox, poly: Option<Vec<Point>>, no_go: Vec<BBox>, inset: f64) -> Self {
        let poly = poly.filter(|p| !p.is_empty() && !is_rect(p));
        let edges = poly
            .as_ref()
            .map(|p| {
                (0..p.len())
                    .map(|i| (p[i], p[(i + 1) % p.len()]))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let strict = poly.is_some() || !no_go.is_empty();
        Self { bbox, poly, edges, no_go, inset, strict }
    }

    pub fn x0(&self) -> f64 {
        self.bbox.x0
    }
    pub fn y0(&self) -> f64 {
        self.bbox.y0
    }
    pub fn x1(&self) -> f64 {
        self.bbox.x1
    }
    pub fn y1(&self) -> f64 {
        self.bbox.y1
    }
    pub fn w(&self) -> f64 {
        self.bbox.w()
    }
    pub fn h(&self) -> f64 {
        self.bbox.h()
    }
    pub fn center(&self) -> Point {
        self.bbox.center()
    }

    pub fn inflate(&self, d: f64) -> Region {
        Region {
            bbox: self.bbox.inflate(d),
            poly: self.poly.clone(),
            edges: self.edges.clone(),
            no_go: self.no_go.clone(),
            inset: (self.inset - d).max(0.0),
            strict: self.strict,
        }
    }

    pub fn accepts(&self, bb: &BBox) -> bool {
        if bb.x0 < self.bbox.x0 - 1e-6
            || bb.y0 < self.bbox.y0 - 1e-6
            || bb.x1 > self.bbox.x1 + 1e-6
            || bb.y1 > self.bbox.y1 + 1e-6
        {
            return false;
        }
        if self.no_go.iter().any(|k| bb.overlaps(k)) {
            return false;
        }
        let Some(poly) = &self.poly else { return true };
        // inside the polygon AND `inset` clear of every edge: either test alone lets one through
        if !crate::geom::box_in_polygon(bb, poly) {
            return false;
        }
        let grown = bb.inflate((self.inset - 1e-6).max(0.0));
        !self.edges.iter().any(|(a, b)| seg_hits_box(*a, *b, &grown))
    }
}

/// The region without its inset -- the outline itself, where a hole or edge connector belongs.
pub fn outline_region(region: &Region) -> Region {
    Region {
        bbox: region.bbox.inflate(region.inset),
        poly: region.poly.clone(),
        edges: region.edges.clone(),
        no_go: region.no_go.clone(),
        inset: 0.0,
        strict: region.strict,
    }
}

/// `GND*`, `V*`, `+3V3`, `AGND` ... -- a supply rail by name alone.
fn is_power_name(name: &str) -> bool {
    let s = name.trim();
    let s = s.strip_prefix('+').unwrap_or(s);
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    let up = s.to_ascii_uppercase();
    if up.starts_with('V') || up.starts_with("GND") || matches!(up.as_str(), "AGND" | "DGND" | "PGND") {
        return true;
    }
    // `\d+V\d*`
    let digits = up.trim_start_matches(|c: char| c.is_ascii_digit());
    digits.len() < up.len()
        && digits.starts_with('V')
        && digits[1..].chars().all(|c| c.is_ascii_digit())
}

/// Net names, the power net ids and the ground net ids.
pub fn net_classes(board: &Board) -> (BTreeMap<i64, String>, BTreeSet<i64>, BTreeSet<i64>) {
    let names: BTreeMap<i64, String> = board.nets().into_iter().map(|n| (n.id, n.name)).collect();
    let counts: BTreeMap<i64, usize> = board
        .pads_by_net()
        .into_iter()
        .map(|(k, v)| (k, v.len()))
        .collect();
    let (mut power, mut ground) = (BTreeSet::new(), BTreeSet::new());
    for (nid, name) in &names {
        if *nid == 0 {
            continue;
        }
        let pads = counts.get(nid).copied().unwrap_or(0);
        if crate::rules::is_ground_name(name) {
            ground.insert(*nid);
            power.insert(*nid);
        } else if (is_power_name(name) || pads > BIG_NET_PADS) && pads != 2 {
            power.insert(*nid);
        }
    }
    (names, power, ground)
}

/// Half-perimeter wirelength over all nets, in mm, from pad positions.
pub fn wirelength(board: &Board) -> f64 {
    let mut total = 0.0;
    for pads in board.pads_by_net().values() {
        if pads.len() >= 2 {
            let b = BBox::of_points(pads.iter().map(|(_, p)| p.pos));
            total += b.w() + b.h();
        }
    }
    total
}

/// Half-perimeter wirelength of the part table at its current poses.
pub fn hpwl(parts: &Parts) -> f64 {
    let mut nets: BTreeMap<i64, BBox> = BTreeMap::new();
    for p in parts.iter() {
        for (net, pos) in p.pad_positions() {
            nets.entry(net).or_insert_with(BBox::empty).add_point(pos);
        }
    }
    nets.values().filter(|b| b.valid()).map(|b| b.w() + b.h()).sum()
}

/// Penetration depth along x and y of two boxes grown by `gap`/2 each; `(0, 0)` if apart.
pub fn overlap(a: &BBox, b: &BBox, gap: f64) -> (f64, f64) {
    let ox = a.x1.min(b.x1) - a.x0.max(b.x0) + gap;
    let oy = a.y1.min(b.y1) - a.y0.max(b.y0) + gap;
    if ox <= 0.0 || oy <= 0.0 {
        (0.0, 0.0)
    } else {
        (ox, oy)
    }
}

pub fn hits(a: &BBox, b: &BBox, gap: f64) -> bool {
    overlap(a, b, gap) != (0.0, 0.0)
}

/// How many seeded runs a board of `n_parts` gets: from [`RESTART_WORK`], never from the clock.
pub fn restarts(n_parts: usize) -> usize {
    RESTARTS.min(RESTART_WORK / n_parts.max(1).pow(2)).max(1)
}

pub fn snap(v: f64, grid: f64) -> f64 {
    if grid > 0.0 { (v / grid).round() * grid } else { v }
}

/// Normalise `near` to `(anchor, refs, radius)`, dropping self-references and empty groups.
pub fn near_groups(near: &[(String, Vec<String>, f64)]) -> Vec<(String, Vec<String>, f64)> {
    let mut out = Vec::new();
    for (anchor, refs, rad) in near {
        let mut seen = BTreeSet::new();
        let refs: Vec<String> = refs
            .iter()
            .filter(|r| !r.is_empty() && *r != anchor && seen.insert((*r).clone()))
            .cloned()
            .collect();
        if anchor.is_empty() || refs.is_empty() {
            continue;
        }
        out.push((
            anchor.clone(),
            refs,
            if *rad > 0.0 { *rad } else { NEAR_RADIUS },
        ));
    }
    out
}

/// See the module docstring. Runs `restarts` seeds (`seed`, `seed + SEED_STRIDE`, ...), keeping the
/// best by unplaced / overlaps / wirelength.
pub fn plan_placement(board: &Board, opts: &PlanOptions) -> PlacementPlan {
    let fps = board.footprints();
    let tries = opts.restarts.unwrap_or_else(|| restarts(fps.len())).max(1);
    let key = |p: &PlacementPlan| (p.unplaced.len(), p.overlaps_after, p.wirelength_after);
    let t0 = std::time::Instant::now();
    let mut best: Option<PlacementPlan> = None;
    let mut stopped = false;
    let mut done = 0usize;
    for k in 0..tries {
        let plan = cluster::plan_once(board, &fps, opts, opts.seed + k as u64 * SEED_STRIDE);
        done = k + 1;
        if best.as_ref().map_or(true, |b| {
            let (a, c) = (key(&plan), key(b));
            a.0 < c.0 || (a.0 == c.0 && (a.1 < c.1 || (a.1 == c.1 && a.2 < c.2)))
        }) {
            best = Some(plan);
        }
        // the stride: re-planning with seed + 1 must give a different set of runs, not the same plan
        if k + 1 < tries && t0.elapsed().as_secs_f64() > PLACE_CEILING_S {
            stopped = true;
            break;
        }
    }
    let mut best = best.unwrap_or_default();
    best.notes
        .push(format!("best of {done} placement run(s) from seed {}", opts.seed));
    if stopped {
        best.notes.push(format!(
            "stopped short of {tries} runs: past the {PLACE_CEILING_S:.0}s safety ceiling, \
             so this plan is NOT reproducible from its seed"
        ));
    }
    best
}

/// Write the plan's moves into the board.
pub fn apply(board: &mut Board, plan: &PlacementPlan) {
    for m in &plan.moves {
        let Some(fp) = board.footprint(&m.ref_) else { continue };
        if fp.side() != m.side {
            board.flip_footprint(&m.ref_);
        }
        board.set_footprint_pose(&m.ref_, Some((m.x, m.y)), Some(m.rot));
    }
}

/// Board size that fits the footprints at human-like density: `(w_mm, h_mm)` with `aspect` w/h.
/// (The Python `shape="star"` branch is dropped.)
pub fn suggest_outline(board: &Board, aspect: f64, density: f64, margin: f64) -> (f64, f64) {
    let fps = board.footprints();
    let area: f64 = fps
        .iter()
        .map(|f| {
            let b = f.courtyard_bbox();
            b.w().max(0.0) * b.h().max(0.0)
        })
        .sum();
    let inner = area / density.max(0.05);
    let h = (inner / aspect).sqrt();
    let w = inner / h;
    let big = fps
        .iter()
        .map(|f| {
            let b = f.courtyard_bbox();
            b.w().max(b.h())
        })
        .fold(0.0f64, f64::max);
    let w = w.max(big + 2.0 * margin);
    let h = h.max(big + 2.0 * margin);
    let round1 = |v: f64| (v * 10.0).round() / 10.0;
    (round1(w + 2.0 * margin), round1(h + 2.0 * margin))
}
