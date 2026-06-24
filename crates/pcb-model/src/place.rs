//! The PCB placement-engine SDK: the [`PlaceProblem`] an engine reads, the
//! [`PlacementHints`] that steer it, the [`PlaceResult`] it returns, and the
//! [`Placer`] contract it implements — all in the neutral kernel (`pcb-model`) so a
//! THIRD-PARTY placer can be written against `pcb-model` ALONE. It never touches the
//! incumbent engine crate (`pcb-place`) nor any KiCAD CLI. A placer: depends on
//! `pcb-model`, `impl Placer for MyPlacer`, reads `problem.parts`/`problem.bounds`
//! plus [`derive_nets`], produces [`Placement`]s, self-verifies with the shared
//! [`is_legal`], sets [`PlaceResult::legal`] + `hpwl` honestly, and returns. To pick
//! among placers by routability it drops `Box::new(MyPlacer)` into a
//! [`RoutabilityOracle`] and supplies its own [`RouteRanker`] (its own router).
//!
//! This is the PCB analog of the schematic `sch_model::place::PlacementEngine` SDK:
//! the identical silhouette (`name`/`capabilities`/a self-contained result/an
//! injected evaluator), so the two tiers stay symmetric. The evaluator here is the
//! [`RouteRanker`] (the placement oracle's router), carried where the schematic side
//! carries its `PlacementCost`.

use crate::{Bounds, LayerRef, Point2, Rect};
use crate::{Connection, Obstacle, RoutePoint, RouteProblem};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── problem ──────────────────────────────────────────────────────────────────

/// A placement problem: board bounds, design clearance, and the parts to place.
///
/// Logical nets are **not** a field — they are derived from per-pad net names
/// ([`derive_nets`]); pads are the single canonical net source. Unknown JSON
/// fields are rejected (schema drift fails loudly), like `RouteProblem`'s
/// solution types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaceProblem {
    /// Board outline (mm, y-down).
    pub bounds: Bounds,
    /// Copper-to-copper clearance (mm); also floors the courtyard margin.
    #[serde(default = "default_clearance")]
    pub clearance: f64,
    /// Number of copper layers (carried into the emitted [`RouteProblem`]).
    #[serde(default = "default_layer_count")]
    pub layer_count: u32,
    /// Minimum trace width (mm), carried into the emitted [`RouteProblem`].
    #[serde(default = "default_min_trace_width")]
    pub min_trace_width: f64,
    /// The parts to place.
    pub parts: Vec<Part>,
    /// Rectangular keep-out regions on the SIGNAL layers (top/bottom) the placer
    /// must keep parts OUT of — a part dropped inside one would have its pads
    /// trapped (no track can leave without crossing the keep-out). Inner-only
    /// (plane) keep-outs are not included here. Empty for most boards.
    #[serde(default)]
    pub keepouts: Vec<Rect>,
    /// Optional custom board OUTLINE (closed polygon, mm). When set, a part is illegal if
    /// its courtyard falls outside the polygon — so concave shapes (a star) keep parts
    /// inside the TRUE outline, not just its bounding box. Carried into the [`RouteProblem`].
    #[serde(default)]
    pub outline: Option<Vec<Point2>>,
}

fn default_clearance() -> f64 {
    0.2
}
fn default_layer_count() -> u32 {
    2
}
fn default_min_trace_width() -> f64 {
    0.2
}

/// A footprint-shaped part: a reference, a courtyard rectangle (centered on the
/// part origin), its pads, and an optional locked position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Part {
    /// Schematic reference designator ("R1", "U2", "J1"). Must be unique;
    /// determinism sorts parts by this.
    pub reference: String,
    /// Courtyard width (x extent, mm), centered on the part origin at rotation 0.
    pub courtyard_w: f64,
    /// Courtyard height (y extent, mm), centered on the part origin at rotation 0.
    pub courtyard_h: f64,
    /// The part's pads (offsets are relative to the part origin at rotation 0).
    pub pads: Vec<PartPad>,
    /// If present, the part is pinned at this position/rotation and never moves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<LockedAt>,
}

/// One pad: a number, an offset from the part origin (rotation 0), a size, the
/// copper layers it sits on, and the net name it belongs to (the canonical net
/// source).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PartPad {
    /// Pad number/name ("1", "2", "A1").
    pub number: String,
    /// Offset from the part origin at rotation 0 (mm).
    pub offset: Point2,
    /// Pad width (x, mm at rotation 0).
    pub width: f64,
    /// Pad height (y, mm at rotation 0).
    pub height: f64,
    /// Copper layers the pad sits on ("top", "bottom", … — thru-hole lists all).
    pub layers: Vec<LayerRef>,
    /// The net this pad belongs to, if any. `None` = unconnected pad.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<String>,
}

/// A locked (pinned) placement: an absolute position and a rotation in degrees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LockedAt {
    /// Absolute part-origin position (mm).
    pub at: Point2,
    /// Rotation in degrees (0/90/180/270; other values are snapped).
    #[serde(default)]
    pub rotation: i32,
}

// ── hints ──────────────────────────────────────────────────────────────────────

/// LLM-authored placement hints. **Empty hints are valid** and must produce a
/// legal placement (hints improve, never gate). Data only — slice 5's LLM
/// integration is a serde/prompt problem, not an engine change.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlacementHints {
    /// Grouping/region/edge hints.
    #[serde(default)]
    pub groups: Vec<GroupHint>,
    /// References that should be pulled to their NEAREST board edge (connectors,
    /// headers, mounting holes — parts a cable or the enclosure reaches from
    /// outside). Unlike a group `edge` hint, the engine picks each part's nearest
    /// edge automatically, so the caller need not know the final layout. A
    /// professional board puts these at the perimeter, not stranded in the
    /// interior with copper wrapping around them.
    #[serde(default)]
    pub edge_seek: Vec<String>,
    /// References pulled to their NEAREST board CORNER (mounting holes — mechanical
    /// fixings belong at the corners, where screws clear the components). Stronger
    /// and more specific than [`Self::edge_seek`] (a corner, not anywhere along an
    /// edge), so a board's 2–4 mounting holes settle one per corner instead of
    /// stranding in the interior or bunching mid-edge.
    #[serde(default)]
    pub corner_seek: Vec<String>,
}

/// A group of parts that should cohere, optionally pulled into a region and/or
/// toward a board edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupHint {
    /// Human label (for provenance/debug; not load-bearing).
    pub name: String,
    /// References of the parts in this group.
    pub members: Vec<String>,
    /// Optional rectangle the members should land inside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Rect>,
    /// Optional board edge the group should hug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<Edge>,
    /// Tile the members in a regular GRID filling [`Self::region`] (row-major, in
    /// member order), locking each at its cell. For repetitive arrays the agent
    /// wants laid out tidily (LED matrices, resistor networks) rather than the
    /// general annealer's scatter. Requires `region`; ignored without it.
    #[serde(default)]
    pub grid: bool,
    /// Ring the members tightly around the perimeter of this target part (by
    /// reference) — the decoupling-cap pattern: caps hug their IC instead of
    /// scattering. The target must be LOCKED (the agent fixes the IC first) so its
    /// position is known when the ring is laid out. Ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surround: Option<String>,
}

/// A board edge for edge-affinity hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Edge {
    N,
    S,
    E,
    W,
}

// ── result ───────────────────────────────────────────────────────────────────

/// The result of [`Placer::place`]: per-part placements, a legality verdict
/// (verified by exact geometry), and a quality/diagnostic report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaceResult {
    /// Placed parts (one per input part, in input order).
    pub placements: Vec<Placement>,
    /// True iff no courtyard overlap (with margin) and all parts in bounds —
    /// verified by exact geometry at the end, not trusted from the algorithm.
    pub legal: bool,
    /// Diagnostics + the HPWL quality number.
    pub report: PlaceReport,
}

/// One part's final placement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Placement {
    /// The part this places.
    pub reference: String,
    /// Final part-origin position (mm).
    pub at: Point2,
    /// Final rotation (degrees; 0/90/180/270).
    pub rotation: i32,
}

/// Placement diagnostics: how much legalization happened and the HPWL metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaceReport {
    /// How many parts the spiral legalizer had to move off their snapped cell.
    pub overlaps_resolved: usize,
    /// How many parts were clamped because the force layout pushed them out of
    /// bounds.
    pub out_of_bounds_clamps: usize,
    /// Half-perimeter wirelength over net bounding boxes (mm) — the cheap
    /// placement-quality number (lower is tighter).
    pub hpwl: f64,
    /// The full layout cost of the final placement (overlap + wirelength +
    /// compaction + decoupling cohesion + silk gap). The [`RoutabilityOracle`]
    /// selects the variant with the lowest layout_cost among those that route as
    /// cleanly, so a search engine's layout-quality gains are actually chosen.
    pub layout_cost: f64,
}

// ── derived nets ─────────────────────────────────────────────────────────────

/// A pin site: which part, which pad.
#[derive(Debug, Clone, PartialEq)]
pub struct Pin {
    /// Index into `problem.parts`.
    pub part: usize,
    /// Index into that part's `pads`.
    pub pad: usize,
}

/// A derived logical net: a name and the pin sites that share it.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalNet {
    pub name: String,
    pub pins: Vec<Pin>,
}

/// Derive logical nets from per-pad net names — the canonical source. Pins are
/// grouped by net name in deterministic (name, part, pad) order. Single-pin
/// nets are kept (callers filter: a 1-pin net has nothing to connect).
pub fn derive_nets(problem: &PlaceProblem) -> Vec<LogicalNet> {
    let mut by_name: BTreeMap<String, Vec<Pin>> = BTreeMap::new();
    for (pi, part) in problem.parts.iter().enumerate() {
        for (di, pad) in part.pads.iter().enumerate() {
            if let Some(net) = &pad.net {
                by_name
                    .entry(net.clone())
                    .or_default()
                    .push(Pin { part: pi, pad: di });
            }
        }
    }
    by_name
        .into_iter()
        .map(|(name, pins)| LogicalNet { name, pins })
        .collect()
}

// ── shared geometry ────────────────────────────────────────────────────────────
//
// The PURE placement geometry the SDK functions ([`is_legal`], [`compute_hpwl`],
// [`to_route_problem`]) and a third-party placer both need. Quadrant rotation,
// centered-rect overlap, bounds fit — all deterministic, no problem-mutating state.
// (The engine's own search-only scaffold — grid snap, spiral, edge pulls — stays
// private to `pcb-place`.)

/// Minimum courtyard-to-courtyard gap (mm). The effective margin is
/// `max(clearance, COURTYARD_MARGIN_MIN)`.
pub const COURTYARD_MARGIN_MIN: f64 = 0.25;

/// KiCAD's copper-to-board-edge clearance (its default). A part's PADS must clear the board
/// outline by this — otherwise an edge-seeking connector lands a pad on the Edge.Cuts and trips
/// `copper_edge_clearance`. (The COURTYARD may still overhang — only copper is constrained.)
pub const EDGE_CLEAR_PLACE_MM: f64 = 0.5;

/// The effective courtyard margin: `max(clearance, COURTYARD_MARGIN_MIN)`.
pub fn courtyard_margin(clearance: f64) -> f64 {
    clearance.max(COURTYARD_MARGIN_MIN)
}

/// Snap an arbitrary rotation (degrees) to the nearest quadrant in 0/90/180/270.
pub fn snap_rotation(deg: i32) -> i32 {
    let r = deg.rem_euclid(360);
    (((r + 45) / 90) * 90) % 360
}

/// Courtyard half-extents after a quadrant rotation (90/270 swap w/h).
pub fn rotated_courtyard_half(part: &Part, rot: i32) -> (f64, f64) {
    let (w, h) = (part.courtyard_w / 2.0, part.courtyard_h / 2.0);
    match rot {
        90 | 270 => (h, w),
        _ => (w, h),
    }
}

/// Half-extents of the part's PAD (copper) bounding box after a quadrant rotation. Bounds ONLY
/// the copper — so the outline check can keep pads inside the board while a part's courtyard
/// (its non-copper margin) is still free to overhang a notch (the mounting-hole allowance).
pub fn rotated_copper_bbox(part: &Part, rot: i32) -> (f64, f64, f64, f64) {
    let (mut xmin, mut ymin, mut xmax, mut ymax) =
        (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for pad in &part.pads {
        let off = rotate_offset(&pad.offset, rot);
        let (pw, ph) = match rot.rem_euclid(360) {
            90 | 270 => (pad.height / 2.0, pad.width / 2.0),
            _ => (pad.width / 2.0, pad.height / 2.0),
        };
        // TRUE (asymmetric) bbox relative to the part origin — a connector's pads are OFF-CENTRE
        // (origin at pin 1, not the courtyard centre), so a symmetric centre±max|offset| box would
        // be ~2× too large on the empty side and FALSE-REJECT a connector that actually clears the
        // edge. Track real min/max so the outline check is exact.
        xmin = xmin.min(off.x - pw);
        xmax = xmax.max(off.x + pw);
        ymin = ymin.min(off.y - ph);
        ymax = ymax.max(off.y + ph);
    }
    if xmin > xmax {
        (0.0, 0.0, 0.0, 0.0) // no pads
    } else {
        (xmin, ymin, xmax, ymax)
    }
}

/// A pad offset rotated by a quadrant (degrees), y-down.
pub fn rotate_offset(off: &Point2, rot: i32) -> Point2 {
    // KiCAD footprint-rotation convention (y-down board coords): a pad's local
    // offset under a footprint rotated by `rot` lands at these world offsets.
    // Verified against kicad-cli: a 270° footprint maps local (x,y) → (-y, x).
    // (The 90 and 270 cases were previously swapped, which placed the engine's
    // routing targets on the WRONG physical pad for any rotated part → shorts.)
    match rot.rem_euclid(360) {
        90 => Point2 { x: off.y, y: -off.x },
        180 => Point2 { x: -off.x, y: -off.y },
        270 => Point2 { x: -off.y, y: off.x },
        _ => off.clone(),
    }
}

/// World position of a pin's pad center given current part positions.
pub fn pad_world(problem: &PlaceProblem, pos: &[Point2], pin: &Pin) -> Point2 {
    let part = &problem.parts[pin.part];
    let rot = part.locked.as_ref().map(|l| snap_rotation(l.rotation)).unwrap_or(0);
    let off = rotate_offset(&part.pads[pin.pad].offset, rot);
    Point2 {
        x: pos[pin.part].x + off.x,
        y: pos[pin.part].y + off.y,
    }
}

/// Margin-inflated axis overlaps of two centered rects. `> 0` on BOTH axes ⇒
/// overlapping; each rect is inflated by `margin/2` per side so the required *gap*
/// between courtyards is `margin`.
pub fn rect_overlap(
    ci: &Point2,
    hi: (f64, f64),
    cj: &Point2,
    hj: (f64, f64),
    margin: f64,
) -> (f64, f64) {
    let m = margin / 2.0;
    let ox = (hi.0 + m + hj.0 + m) - (ci.x - cj.x).abs();
    let oy = (hi.1 + m + hj.1 + m) - (ci.y - cj.y).abs();
    (ox, oy)
}

/// Margin-inflated overlap of two parts' courtyards, per axis (mm; >0 on both
/// axes ⇒ overlapping).
pub fn courtyard_overlap(
    pos: &[Point2],
    half: &[(f64, f64)],
    margin: f64,
    i: usize,
    j: usize,
) -> (f64, f64) {
    rect_overlap(&pos[i], half[i], &pos[j], half[j], margin)
}

/// Does a part's courtyard fit fully within `bounds`?
pub fn fits_in_bounds(p: &Point2, b: &Bounds, h: (f64, f64)) -> bool {
    p.x - h.0 >= b.min_x - 1e-9
        && p.x + h.0 <= b.max_x + 1e-9
        && p.y - h.1 >= b.min_y - 1e-9
        && p.y + h.1 <= b.max_y + 1e-9
}

/// Overlap `(ox, oy)` of a part's courtyard (centre `p`, half-extents `h`) with a
/// keep-out rect; both strictly positive means the part intrudes into the keep-out.
pub fn part_keepout_overlap(p: &Point2, h: (f64, f64), k: &Rect) -> (f64, f64) {
    let ox = (p.x + h.0).min(k.max_x) - (p.x - h.0).max(k.min_x);
    let oy = (p.y + h.1).min(k.max_y) - (p.y - h.1).max(k.min_y);
    (ox, oy)
}

/// The placement analog of the lint: re-verify in exact geometry that no two
/// courtyards overlap (with margin) and every part is in bounds. Never trusts
/// the algorithm — a placer calls this to set [`PlaceResult::legal`] HONESTLY.
pub fn is_legal(
    problem: &PlaceProblem,
    half: &[(f64, f64)],
    copper_bbox: &[(f64, f64, f64, f64)],
    margin: f64,
    pos: &[Point2],
) -> bool {
    let n = problem.parts.len();
    for i in 0..n {
        if !fits_in_bounds(&pos[i], &problem.bounds, half[i]) {
            return false;
        }
        // On a custom outline, a part's CENTRE must be inside the true polygon (keeps parts out
        // of a star's concave notches the bbox alone allows), AND its PAD (copper) bounding box,
        // grown by the edge clearance, must be inside too — a part's placed pads are copper the
        // router never relocates, so an edge-seeking connector whose centre is inside but whose
        // far pad overhangs the edge would otherwise ship a copper_edge_clearance fault. The
        // COURTYARD may still overhang (only copper is constrained), preserving the mounting-hole
        // -in-a-notch allowance.
        if let Some(poly) = &problem.outline {
            if !crate::point_in_polygon(&pos[i], poly) {
                return false;
            }
            let (xmin, ymin, xmax, ymax) = copper_bbox[i];
            let ec = EDGE_CLEAR_PLACE_MM;
            for (dx, dy) in [
                (xmin - ec, ymin - ec),
                (xmax + ec, ymin - ec),
                (xmax + ec, ymax + ec),
                (xmin - ec, ymax + ec),
            ] {
                let c = Point2 { x: pos[i].x + dx, y: pos[i].y + dy };
                if !crate::point_in_polygon(&c, poly) {
                    return false;
                }
            }
        }
        // A part overlapping a signal-layer keep-out is illegal (its pads can't route).
        for k in &problem.keepouts {
            let (ox, oy) = part_keepout_overlap(&pos[i], half[i], k);
            if ox > 1e-9 && oy > 1e-9 {
                return false;
            }
        }
        for j in (i + 1)..n {
            let (ox, oy) = courtyard_overlap(pos, half, margin, i, j);
            // Strictly-positive overlap on BOTH axes is a real courtyard
            // collision. Touching exactly at the margin (overlap == 0) is legal.
            if ox > 1e-9 && oy > 1e-9 {
                return false;
            }
        }
    }
    true
}

/// Half-perimeter wirelength over net bounding boxes (mm): for each multi-pin
/// net, `(maxX-minX) + (maxY-minY)` of its pad world positions, summed. The cheap
/// placement-quality number a placer reports in [`PlaceReport::hpwl`].
pub fn compute_hpwl(problem: &PlaceProblem, nets: &[LogicalNet], pos: &[Point2]) -> f64 {
    let mut total = 0.0;
    for net in nets {
        if net.pins.len() < 2 {
            continue;
        }
        let mut min_x = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for pin in &net.pins {
            let w = pad_world(problem, pos, pin);
            min_x = min_x.min(w.x);
            max_x = max_x.max(w.x);
            min_y = min_y.min(w.y);
            max_y = max_y.max(w.y);
        }
        total += (max_x - min_x) + (max_y - min_y);
    }
    total
}

// ── co-placement pairs ─────────────────────────────────────────────────────────

/// Series co-placement earns its keep only where escape congestion is real: a DENSE
/// package (QFP/BGA/QFN — many pads on tight pitch) whose signal pads must thread
/// limited channels to break out. A small anchor (SOIC-8, SOT-223) has trivial escape,
/// so pulling a series part to it just perturbs an already-clean layout. Gate on pad count.
const SERIES_ANCHOR_MIN_PADS: usize = 16;

/// Detect decoupling co-placement pairs `(cap_idx, ic_idx)`: a 2-pad part whose
/// BOTH pad nets also appear on a larger (≥3-pad) part is its decoupling cap and
/// should hug that IC/regulator. The smaller-index qualifying anchor wins
/// (deterministic). A part wired to two unrelated nets (e.g. a divider resistor)
/// finds no single anchor with both nets, so this fires only for real bypass caps.
/// Public so the agent surface can suggest a `surround` hint for a decoupling-heavy IC.
pub fn decoupling_pairs(problem: &PlaceProblem) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    for (si, small) in problem.parts.iter().enumerate() {
        if small.pads.len() != 2 {
            continue;
        }
        let nets: Vec<&str> = small.pads.iter().filter_map(|p| p.net.as_deref()).collect();
        if nets.len() != 2 || nets[0] == nets[1] {
            continue;
        }
        for (ai, anc) in problem.parts.iter().enumerate() {
            if ai == si || anc.pads.len() < 3 {
                continue;
            }
            let anc_nets: std::collections::BTreeSet<&str> =
                anc.pads.iter().filter_map(|p| p.net.as_deref()).collect();
            if anc_nets.contains(nets[0]) && anc_nets.contains(nets[1]) {
                pairs.push((si, ai));
                break;
            }
        }
    }
    pairs
}

/// Detect series co-placement pairs `(part_idx, anchor_idx)`: a 2-pad part with a
/// pad on a **2-pin net** whose other pin belongs to a dense (≥[`SERIES_ANCHOR_MIN_PADS`]
/// -pad) anchor — a series element hanging directly off one anchor pin (the classic
/// BGA/IC signal breakout: ball → series R → header). Co-placing it next to that anchor
/// pad keeps the congested escape hop short, so the breakout actually routes. The
/// 2-pin-net test is what makes this safe: a divider resistor's nets are high-fanout power
/// rails (≥3 pins), so it never matches — this fires only for true series taps.
/// When both pads qualify (R between two ICs), the LARGER anchor wins (the dense
/// package whose escape congestion matters most). Disjoint from [`decoupling_pairs`]
/// (whose caps share BOTH nets with one anchor, i.e. high-fanout power).
pub fn series_pairs(problem: &PlaceProblem) -> Vec<(usize, usize)> {
    // net name → the part indices with a pad on it (one entry per pad).
    let mut net_pins: std::collections::HashMap<&str, Vec<usize>> =
        std::collections::HashMap::new();
    for (pi, part) in problem.parts.iter().enumerate() {
        for pad in &part.pads {
            if let Some(n) = pad.net.as_deref() {
                net_pins.entry(n).or_default().push(pi);
            }
        }
    }
    let mut pairs = Vec::new();
    for (si, small) in problem.parts.iter().enumerate() {
        if small.pads.len() != 2 {
            continue;
        }
        let mut best: Option<usize> = None;
        let mut best_pads = 0usize;
        for pad in &small.pads {
            let Some(net) = pad.net.as_deref() else { continue };
            let pins = &net_pins[net];
            // 2-pin net: exactly this part's pad + one other pin.
            if pins.len() != 2 {
                continue;
            }
            if let Some(&anchor) = pins.iter().find(|&&p| p != si) {
                let np = problem.parts[anchor].pads.len();
                if np >= SERIES_ANCHOR_MIN_PADS && np > best_pads {
                    best_pads = np;
                    best = Some(anchor);
                }
            }
        }
        if let Some(anchor) = best {
            pairs.push((si, anchor));
        }
    }
    pairs
}

/// Order series parts by the ANGLE of their connected `ic` pad around the IC
/// centre. Ringing them in this order makes each IC→part escape route radially
/// (short, parallel, NON-crossing) instead of spaghetti — the key to neat fan-out
/// on a board whose signal pins each tap a series element (the routing-neatness
/// lever). `parts` are part indices (e.g. from [`series_pairs`] anchored at `ic`).
pub fn series_fanout_order(problem: &PlaceProblem, ic: usize, parts: &[usize]) -> Vec<String> {
    let mut with_angle: Vec<(f64, String)> = parts
        .iter()
        .filter_map(|&p| {
            let p_nets: std::collections::BTreeSet<&str> =
                problem.parts[p].pads.iter().filter_map(|pp| pp.net.as_deref()).collect();
            // The IC pad sharing this part's 2-pin net → its angle around the IC.
            problem.parts[ic].pads.iter().find_map(|pad| {
                let n = pad.net.as_deref()?;
                p_nets.contains(n).then(|| {
                    (pad.offset.y.atan2(pad.offset.x), problem.parts[p].reference.clone())
                })
            })
        })
        .collect();
    with_angle.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    with_angle.into_iter().map(|(_, r)| r).collect()
}

// ── to_route_problem ───────────────────────────────────────────────────────────

/// Via geometry carried into the emitted [`RouteProblem`] (mirrors the route
/// model's defaults — the value the existing fixtures and oracle expect).
const DEFAULT_VIA_DIAMETER: f64 = 0.6;
const DEFAULT_VIA_DRILL: f64 = 0.3;

/// Build a [`RouteProblem`] from a placement: every pad becomes a net-attributed
/// obstacle (at its placed+rotated world position), and every multi-pin net
/// becomes a [`Connection`] whose `points_to_connect` are the pad centers on the
/// pad's layer. Board bounds and design rules are carried from the problem.
///
/// The emitted problem round-trips serde and is accepted by `route_auto` and the
/// connectivity oracle unchanged (pads on nets, points on pads). A [`RouteRanker`]
/// (the placement oracle's router) consumes exactly this.
pub fn to_route_problem(problem: &PlaceProblem, placements: &[Placement]) -> RouteProblem {
    // Index placements by reference so we tolerate any order.
    let place_by_ref: BTreeMap<&str, &Placement> =
        placements.iter().map(|p| (p.reference.as_str(), p)).collect();

    let mut obstacles: Vec<Obstacle> = Vec::new();
    // Net → its pad world positions + layer (for connections). Deterministic order.
    let mut net_points: BTreeMap<String, Vec<RoutePoint>> = BTreeMap::new();

    for part in &problem.parts {
        let Some(pl) = place_by_ref.get(part.reference.as_str()) else {
            continue;
        };
        let rot = snap_rotation(pl.rotation);
        for pad in &part.pads {
            let off = rotate_offset(&pad.offset, rot);
            let center = Point2 {
                x: pl.at.x + off.x,
                y: pl.at.y + off.y,
            };
            // Rotation swaps pad w/h for the quadrant cases.
            let (w, h) = match rot {
                90 | 270 => (pad.height, pad.width),
                _ => (pad.width, pad.height),
            };
            let connected_to = pad.net.clone().into_iter().collect::<Vec<_>>();
            obstacles.push(Obstacle {
                kind: "rect".to_owned(),
                layers: pad.layers.clone(),
                center: center.clone(),
                width: w,
                height: h,
                connected_to,
            });
            if let Some(net) = &pad.net {
                // The connection point sits at the pad center on the pad's first
                // copper layer (a thru-hole pad lists several; the route point
                // anchors one — the via/oracle stitch the rest).
                let layer = pad.layers.first().cloned().unwrap_or_else(LayerRef::top);
                net_points
                    .entry(net.clone())
                    .or_default()
                    .push(RoutePoint {
                        x: center.x,
                        y: center.y,
                        layer,
                    });
            }
        }
    }

    // Multi-pin nets → connections (single-pin nets have nothing to connect).
    let connections: Vec<Connection> = net_points
        .into_iter()
        .filter(|(_, pts)| pts.len() >= 2)
        .map(|(name, points_to_connect)| Connection {
            name,
            points_to_connect,
        })
        .collect();

    RouteProblem {
        layer_count: problem.layer_count,
        min_trace_width: problem.min_trace_width,
        obstacles,
        connections,
        bounds: problem.bounds.clone(),
        clearance: problem.clearance,
        // Via geometry: the defaults the existing fixtures use. v1 placement does
        // not model via sizing, so it carries these constants.
        via_diameter: DEFAULT_VIA_DIAMETER,
        via_drill: DEFAULT_VIA_DRILL,
        // Per-net widths are applied by the agent layer (route_board) after this, from
        // the board's design rules; placement itself is width-agnostic.
        net_widths: std::collections::BTreeMap::new(),
        // Carry the custom outline so the router keeps copper inside the true shape.
        outline: problem.outline.clone(),
        escape_layers: Default::default(),
    }
}

// ── engine-SDK trait seam ────────────────────────────────────────────────────

/// What a [`Placer`] OFFERS — the capability descriptor a selector queries before
/// dispatch (never hardcoded out-of-band knowledge at the call site). Defaulted, so
/// a minimal placer need not implement [`Placer::capabilities`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// The largest board (part count) the placer is willing to attempt. A selector
    /// skips a placer whose limit a problem exceeds. `usize::MAX` = no limit (the
    /// default — the built-in placers scale to any board they are handed).
    pub max_parts: usize,
    /// The placer honours [`PlacementHints`] (groups/regions/edges). A placer that
    /// ignores hints declares `false`, and a selector can prefer a hint-aware one
    /// when hints are present.
    pub honors_hints: bool,
    /// The placer keeps `locked` parts pinned at their [`LockedAt`] position. A
    /// placer that cannot honour locks declares `false` (a selector then never hands
    /// it a problem with pinned parts).
    pub honors_locked: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self { max_parts: usize::MAX, honors_hints: true, honors_locked: true }
    }
}

/// A PCB placement ENGINE: given a [`PlaceProblem`] and [`PlacementHints`], produce
/// a [`PlaceResult`] (per-part placements + an honest legality verdict + a report).
/// The only contract is "produce a placement"; *how* (force/anneal/fan-out, learned,
/// constraint, template, portfolio) is the engine's own business. Lives in the kernel
/// (`pcb-model`) so a third party can implement it against `pcb-model` ALONE.
///
/// ## Contract
/// - **Deterministic given the [`PlaceProblem`] + [`PlacementHints`].** No clock, no
///   I/O; a fixed seed reproduces. Two runs on equal inputs serialize byte-equal.
/// - **Reports failures, never silently drops them.** A board it cannot seat returns
///   [`PlaceResult`] with `legal: false` (verified by [`is_legal`]) and a report —
///   the caller sees the verdict.
/// - **Emits nothing for a failed unit.** A `legal: false` result is not a placement
///   to ship; a [`RoutabilityOracle`] discards it (saturated rank) and keeps a legal
///   candidate.
/// - **Never panics.** An impossible board returns `legal: false`, never unwinds.
///
/// A faithful placer self-verifies with the shared [`is_legal`] and sets
/// [`PlaceResult::legal`] + [`PlaceReport::hpwl`] HONESTLY (via [`compute_hpwl`]) so
/// a selector can trust the verdict without re-checking.
pub trait Placer {
    /// Open provenance: the placer's stable name (e.g. `"legalizing"`, `"anneal"`,
    /// `"fanout"`, `"oracle"`).
    fn name(&self) -> &'static str;

    /// What this placer offers, for the selector. Defaults to the unrestricted,
    /// fully hint/lock-honouring descriptor.
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    /// Place `problem` under `hints` and return the result.
    fn place(&self, problem: &PlaceProblem, hints: &PlacementHints) -> PlaceResult;
}

/// The routability EVALUATOR a [`RoutabilityOracle`] ranks candidate placements
/// against — the boundary that keeps the router INJECTABLE, not hardwired into the
/// placer. An implementation wraps a router (`pcb-place` ships a grid-astar-backed
/// default); a third party supplies its own to score with its own router. The
/// oracle calls it on each candidate's [`to_route_problem`] and keeps the candidate
/// with the fewest faults.
///
/// ## Contract
/// - **Deterministic given the [`RouteProblem`]**: equal input ⇒ equal `(faults,
///   wirelength)`.
/// - **Never panics.** An un-routable problem returns a high fault count, never
///   unwinds — so a bad candidate simply loses the ranking.
pub trait RouteRanker {
    /// The routability of `rp`: `(faults, routed_wirelength)`. `faults` is the
    /// PRIMARY key (unrouted nets + geometry DRC violations — a worse-routed layout
    /// is never chosen); the wirelength (mm, ×1000 as a `u64` for a total order)
    /// breaks ties among equally-routable candidates. Lower is better on both.
    fn faults(&self, rp: &RouteProblem) -> (usize, u64);
}

/// The routability oracle: a [`Placer`] that runs a portfolio of inner [`Placer`]s,
/// routes each candidate with the injected [`RouteRanker`], and KEEPS the one that
/// routes cleanest (fewest faults, then lowest layout cost, then HPWL). It is itself
/// a `Placer`, so it composes (an oracle can be an inner placer of another).
///
/// This is the placement analog of the router's `route_auto` selector: the built-in
/// placers are candidates, the baseline always among them, so an aggressive variant
/// can never regress a board it does not improve — the oracle decides per board.
/// Because the router is injected (not hardwired), a third party drops their own
/// [`Placer`]s into `placers` and their own [`RouteRanker`] into `ranker`.
pub struct RoutabilityOracle {
    /// The candidate placers, tried in order. `placers[0]` is the tie-break winner
    /// (an exact tie keeps the earliest), so put the baseline first. `Send + Sync`
    /// so the oracle can evaluate candidates in parallel.
    pub placers: Vec<Box<dyn Placer + Send + Sync>>,
    /// The router the oracle ranks routability with — injected, not hardwired.
    pub ranker: Box<dyn RouteRanker + Send + Sync>,
}

impl RoutabilityOracle {
    /// Build an oracle from a placer portfolio and a route ranker.
    pub fn new(
        placers: Vec<Box<dyn Placer + Send + Sync>>,
        ranker: Box<dyn RouteRanker + Send + Sync>,
    ) -> Self {
        Self { placers, ranker }
    }
}

impl Placer for RoutabilityOracle {
    fn name(&self) -> &'static str {
        "oracle"
    }

    /// Run every inner placer, route each LEGAL candidate via the injected ranker,
    /// and return the lowest-ranked one. The rank key is `(faults, layout_cost,
    /// hpwl)`: routing faults dominate (a worse-routed layout is never chosen), the
    /// layout cost decides among equally-routable layouts (so a search engine's
    /// compaction/cohesion gains are chosen), and HPWL breaks final ties. An illegal
    /// candidate ranks saturated (it can never win). `placers[0]` wins exact ties.
    fn place(&self, problem: &PlaceProblem, hints: &PlacementHints) -> PlaceResult {
        let rank = |r: &PlaceResult| -> (usize, u64, u64) {
            if !r.legal {
                return (usize::MAX, u64::MAX, u64::MAX);
            }
            let rp = to_route_problem(problem, &r.placements);
            let (faults, _wl) = self.ranker.faults(&rp);
            (
                faults,
                (r.report.layout_cost * 1000.0) as u64,
                (r.report.hpwl * 1000.0) as u64,
            )
        };
        // Evaluate every candidate IN PARALLEL — each is an independent, pure
        // place+rank (deterministic regardless of thread/order). We then pick the
        // lowest-ranked; `placers[0]` wins exact ties via the index tie-break, so the
        // selection is byte-identical to a sequential evaluation.
        use rayon::prelude::*;
        let mut scored: Vec<(usize, (usize, u64, u64), PlaceResult)> = self
            .placers
            .par_iter()
            .enumerate()
            .map(|(i, p)| {
                let r = p.place(problem, hints);
                let key = rank(&r);
                (i, key, r)
            })
            .collect();
        scored.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        scored.swap_remove(0).2
    }
}
