//! The PCB placement-engine SDK: the [`PlaceProblem`] an engine reads, the
//! [`PlacementHints`] that steer it, the [`PlaceResult`] it returns, and the
//! [`Placer`] contract it implements — a neutral kernel, so a THIRD-PARTY placer
//! can be written against `place-model` ALONE. It never touches the incumbent
//! engine crate (`pcb-place`) nor any KiCAD CLI. A placer: depends on
//! `place-model`, `impl Placer for MyPlacer`, reads `problem.parts`/`problem.bounds`
//! plus [`derive_nets`], produces [`Placement`]s, self-verifies with the shared
//! [`is_legal`], sets [`PlaceResult::legal`] + `hpwl` honestly, and returns. To pick
//! among placers by routability it drops `Box::new(MyPlacer)` into a
//! [`RoutabilityOracle`] and supplies its own [`RouteRanker`] (its own router).
//!
//! This is the PCB analog of the schematic `PlacementEngine` SDK: the identical
//! silhouette (`name`/`capabilities`/a self-contained result/an injected evaluator),
//! so the two tiers stay symmetric. The evaluator here is the [`RouteRanker`] (the
//! placement oracle's router).

use geom::{Point2, Polygon, Rect};
use pcb_model::{Connection, LayerRef, Obstacle, RoutePoint, RouteProblem};
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
    pub bounds: Rect,
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
    pub outline: Option<Polygon>,
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
    /// Footprint-local line where the finished PCB edge belongs. Present only
    /// when the library footprint explicitly labels a mechanical edge datum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_datum: Option<EdgeDatum>,
    /// If present, the part is pinned at this position/rotation and never moves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<LockedAt>,
}

/// A footprint-local PCB-edge datum carried from the footprint library into
/// placement. The line is expected to lie tangent to the selected board edge.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EdgeDatum {
    pub start: Point2,
    pub end: Point2,
}

impl EdgeDatum {
    pub fn rotated(self, rotation: f64) -> Self {
        Self {
            start: self.start.rotate(rotation),
            end: self.end.rotate(rotation),
        }
    }

    pub fn midpoint(self) -> Point2 {
        Point2::new(
            (self.start.x + self.end.x) / 2.0,
            (self.start.y + self.end.y) / 2.0,
        )
    }

    pub fn is_horizontal(self) -> bool {
        (self.end.x - self.start.x).abs() >= (self.end.y - self.start.y).abs()
    }
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
    /// Rotation in degrees, CCW (0/90/180/270; other values are snapped).
    #[serde(default)]
    pub rotation: f64,
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
    /// Optional quadrant rotation applied to every locked grid member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<f64>,
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
    /// Final rotation (degrees, CCW; 0/90/180/270).
    pub rotation: f64,
}

/// Placement diagnostics: how much legalization happened and the HPWL metric.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

/// Courtyard half-extents after a quadrant rotation (90/270 swap w/h).
pub fn rotated_courtyard_half(part: &Part, rot: f64) -> (f64, f64) {
    let h = Point2::new(part.courtyard_w / 2.0, part.courtyard_h / 2.0).rotated_half_extents(rot);
    (h.x, h.y)
}

/// Half-extents of the part's PAD (copper) bounding box after a quadrant rotation. Bounds ONLY
/// the copper — so the outline check can keep pads inside the board while a part's courtyard
/// (its non-copper margin) is still free to overhang a notch (the mounting-hole allowance).
pub fn rotated_copper_bbox(part: &Part, rot: f64) -> Rect {
    let (mut xmin, mut ymin, mut xmax, mut ymax) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for pad in &part.pads {
        let off = pad.offset.rotate(rot);
        let half = Point2::new(pad.width / 2.0, pad.height / 2.0).rotated_half_extents(rot);
        // TRUE (asymmetric) bbox relative to the part origin — a connector's pads are OFF-CENTRE
        // (origin at pin 1, not the courtyard centre), so a symmetric centre±max|offset| box would
        // be ~2× too large on the empty side and FALSE-REJECT a connector that actually clears the
        // edge. Track real min/max so the outline check is exact.
        xmin = xmin.min(off.x - half.x);
        xmax = xmax.max(off.x + half.x);
        ymin = ymin.min(off.y - half.y);
        ymax = ymax.max(off.y + half.y);
    }
    if xmin > xmax {
        Rect::zero()
    } else {
        Rect::new(xmin, ymin, xmax, ymax)
    }
}

/// Bounding box, relative to the part origin, that must remain inside the
/// rectangular board bounds. It combines the courtyard and the pad-copper
/// envelope required by KiCad's board-edge clearance.
pub fn placement_bounds_envelope(half: (f64, f64), copper_bbox: Rect) -> Rect {
    Rect::new(
        (-half.0).min(copper_bbox.min_x - EDGE_CLEAR_PLACE_MM),
        (-half.1).min(copper_bbox.min_y - EDGE_CLEAR_PLACE_MM),
        half.0.max(copper_bbox.max_x + EDGE_CLEAR_PLACE_MM),
        half.1.max(copper_bbox.max_y + EDGE_CLEAR_PLACE_MM),
    )
}

/// Bounds envelope for a concrete part. A labelled PCB-edge datum explicitly
/// authorizes its mechanical body/courtyard to cross the outline; pad copper
/// and the footprint origin must still remain inside with edge clearance.
pub fn part_placement_bounds_envelope(part: &Part, half: (f64, f64), copper_bbox: Rect) -> Rect {
    if part.edge_datum.is_none() {
        return placement_bounds_envelope(half, copper_bbox);
    }
    Rect::new(
        0.0_f64.min(copper_bbox.min_x - EDGE_CLEAR_PLACE_MM),
        0.0_f64.min(copper_bbox.min_y - EDGE_CLEAR_PLACE_MM),
        0.0_f64.max(copper_bbox.max_x + EDGE_CLEAR_PLACE_MM),
        0.0_f64.max(copper_bbox.max_y + EDGE_CLEAR_PLACE_MM),
    )
}

/// Distance from an explicit physical edge datum (or, for ordinary parts, its
/// courtyard) to the nearest compatible rectangular board edge.
pub fn part_edge_distance(
    part: &Part,
    rotation: f64,
    at: Point2,
    bounds: &Rect,
    half: (f64, f64),
) -> f64 {
    if let Some(datum) = part.edge_datum.map(|datum| datum.rotated(rotation)) {
        let midpoint = datum.midpoint();
        let dx = (datum.end.x - datum.start.x).abs();
        let dy = (datum.end.y - datum.start.y).abs();
        let mut best = f64::INFINITY;
        if dy <= geom::EPS {
            let world_y = at.y + midpoint.y;
            best = best.min((world_y - bounds.min_y).abs());
            best = best.min((world_y - bounds.max_y).abs());
        }
        if dx <= geom::EPS {
            let world_x = at.x + midpoint.x;
            best = best.min((world_x - bounds.min_x).abs());
            best = best.min((world_x - bounds.max_x).abs());
        }
        if best.is_finite() {
            return best;
        }
    }

    let dl = (at.x - half.0) - bounds.min_x;
    let dr = bounds.max_x - (at.x + half.0);
    let dt = (at.y - half.1) - bounds.min_y;
    let db = bounds.max_y - (at.y + half.1);
    dl.min(dr).min(dt).min(db).max(0.0)
}

/// Translate a part-relative placement envelope to world coordinates.
pub fn placement_envelope_at(center: Point2, envelope: Rect) -> Rect {
    Rect::new(
        center.x + envelope.min_x,
        center.y + envelope.min_y,
        center.x + envelope.max_x,
        center.y + envelope.max_y,
    )
}

/// Clamp a part origin so an asymmetric placement envelope fits in `bounds`.
/// Oversize envelopes are centered on the affected axis.
pub fn clamp_center_for_envelope(bounds: &Rect, center: Point2, envelope: Rect) -> Point2 {
    let (lo_x, hi_x) = (bounds.min_x - envelope.min_x, bounds.max_x - envelope.max_x);
    let (lo_y, hi_y) = (bounds.min_y - envelope.min_y, bounds.max_y - envelope.max_y);
    Point2::new(
        if lo_x <= hi_x {
            center.x.clamp(lo_x, hi_x)
        } else {
            bounds.center().x - envelope.center().x
        },
        if lo_y <= hi_y {
            center.y.clamp(lo_y, hi_y)
        } else {
            bounds.center().y - envelope.center().y
        },
    )
}

/// World position of a pin's pad center given current part positions.
pub fn pad_world(problem: &PlaceProblem, pos: &[Point2], pin: &Pin) -> Point2 {
    let part = &problem.parts[pin.part];
    let rot = part
        .locked
        .as_ref()
        .map(|l| geom::snap_quadrant(l.rotation))
        .unwrap_or(0.0);
    let off = part.pads[pin.pad].offset.rotate(rot);
    Point2 {
        x: pos[pin.part].x + off.x,
        y: pos[pin.part].y + off.y,
    }
}

/// World position of a pin's pad center with explicit per-part rotations. This is
/// the version a placer should use after it has polished rotations but has not
/// mutated `Part::locked`; [`pad_world`] remains the compatibility helper for
/// locked-input geometry.
pub fn pad_world_with_rotation(
    problem: &PlaceProblem,
    pos: &[Point2],
    rotations: &[f64],
    pin: &Pin,
) -> Point2 {
    let part = &problem.parts[pin.part];
    let rot = geom::snap_quadrant(
        rotations
            .get(pin.part)
            .copied()
            .unwrap_or_else(|| part.locked.as_ref().map_or(0.0, |l| l.rotation)),
    );
    let off = part.pads[pin.pad].offset.rotate(rot);
    Point2 {
        x: pos[pin.part].x + off.x,
        y: pos[pin.part].y + off.y,
    }
}

/// The placement analog of the lint: re-verify in exact geometry that no two
/// courtyards overlap (with margin) and every part is in bounds. Never trusts
/// the algorithm — a placer calls this to set [`PlaceResult::legal`] HONESTLY.
pub fn is_legal(
    problem: &PlaceProblem,
    half: &[(f64, f64)],
    copper_bbox: &[Rect],
    margin: f64,
    pos: &[Point2],
) -> bool {
    let n = problem.parts.len();
    for i in 0..n {
        let courtyard = Rect::from_center_half(pos[i], half[i]);
        let copper = copper_bbox[i];
        let envelope = part_placement_bounds_envelope(&problem.parts[i], half[i], copper);
        if !problem
            .bounds
            .contains_rect_eps(&placement_envelope_at(pos[i], envelope), 1e-9)
        {
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
            if !poly.contains_point(pos[i]) {
                return false;
            }
            let ec = EDGE_CLEAR_PLACE_MM;
            for (dx, dy) in [
                (copper.min_x - ec, copper.min_y - ec),
                (copper.max_x + ec, copper.min_y - ec),
                (copper.max_x + ec, copper.max_y + ec),
                (copper.min_x - ec, copper.max_y + ec),
            ] {
                let c = Point2 {
                    x: pos[i].x + dx,
                    y: pos[i].y + dy,
                };
                if !poly.contains_point(c) {
                    return false;
                }
            }
        }
        // A part overlapping a signal-layer keep-out is illegal (its pads can't route).
        for k in &problem.keepouts {
            let (ox, oy) = courtyard.axis_penetration(k);
            if ox > 1e-9 && oy > 1e-9 {
                return false;
            }
        }
        for j in (i + 1)..n {
            let other = Rect::from_center_half(pos[j], half[j]);
            let (ox, oy) = courtyard
                .inflate(margin / 2.0)
                .axis_penetration(&other.inflate(margin / 2.0));
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
    let rotations: Vec<f64> = problem
        .parts
        .iter()
        .map(|part| part.locked.as_ref().map_or(0.0, |l| l.rotation))
        .collect();
    compute_hpwl_with_rotations(problem, nets, pos, &rotations)
}

/// Half-perimeter wirelength over net bounding boxes with explicit final
/// rotations. This is the honest report metric for placements whose unlocked
/// parts were rotated during polish.
pub fn compute_hpwl_with_rotations(
    problem: &PlaceProblem,
    nets: &[LogicalNet],
    pos: &[Point2],
    rotations: &[f64],
) -> f64 {
    let mut total = 0.0;
    for net in nets {
        if net.pins.len() < 2 {
            continue;
        }
        let pts: Vec<Point2> = net
            .pins
            .iter()
            .map(|pin| pad_world_with_rotation(problem, pos, rotations, pin))
            .collect();
        total += Rect::bounding(&pts).map_or(0.0, |r| r.half_perimeter());
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
            let Some(net) = pad.net.as_deref() else {
                continue;
            };
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
            let p_nets: std::collections::BTreeSet<&str> = problem.parts[p]
                .pads
                .iter()
                .filter_map(|pp| pp.net.as_deref())
                .collect();
            // The IC pad sharing this part's 2-pin net → its angle around the IC.
            problem.parts[ic].pads.iter().find_map(|pad| {
                let n = pad.net.as_deref()?;
                p_nets.contains(n).then(|| {
                    (
                        pad.offset.y.atan2(pad.offset.x),
                        problem.parts[p].reference.clone(),
                    )
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
    let place_by_ref: BTreeMap<&str, &Placement> = placements
        .iter()
        .map(|p| (p.reference.as_str(), p))
        .collect();

    let mut obstacles: Vec<Obstacle> = Vec::new();
    // Net → its pad world positions + layer (for connections). Deterministic order.
    let mut net_points: BTreeMap<String, Vec<RoutePoint>> = BTreeMap::new();

    for part in &problem.parts {
        let Some(pl) = place_by_ref.get(part.reference.as_str()) else {
            continue;
        };
        let rot = geom::snap_quadrant(pl.rotation) as i32;
        for pad in &part.pads {
            let off = pad.offset.rotate(rot as f64);
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
                center,
                width: w,
                height: h,
                connected_to,
            });
            if let Some(net) = &pad.net {
                // The connection point sits at the pad center on the pad's first
                // copper layer (a thru-hole pad lists several; the route point
                // anchors one — the via/oracle stitch the rest).
                let layer = pad.layers.first().cloned().unwrap_or_else(LayerRef::top);
                net_points.entry(net.clone()).or_default().push(RoutePoint {
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
        bounds: problem.bounds,
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
        plane_nets: Default::default(),
    }
}

// ── engine-SDK trait seam ────────────────────────────────────────────────────

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
/// - **Deterministic given the [`RouteProblem`]**: equal input ⇒ equal `faults`.
/// - **Never panics.** An un-routable problem returns a high fault count, never
///   unwinds — so a bad candidate simply loses the ranking.
pub trait RouteRanker {
    /// The routability of `rp`: the fault count (unrouted nets + geometry DRC
    /// violations). A worse-routed layout is never chosen; lower is better.
    fn faults(&self, rp: &RouteProblem) -> usize;

    /// Richer route-quality key for equally-faulty placements. Existing rankers
    /// may rely on the default, which preserves the old fault-only behaviour.
    /// Built-in rankers can override this to expose geometry DRC count, failed-net
    /// count, via count, and routed wirelength without changing the oracle's
    /// injected-router shape.
    fn rank_key(&self, rp: &RouteProblem) -> RouteRankKey {
        (self.faults(rp), 0, 0, 0, 0)
    }
}

pub type RouteRankKey = (usize, usize, usize, usize, u64);
type PlacementRouteCacheKey = Vec<(String, i64, i64, i64)>;
type PlacedCandidate = (usize, Option<PlacementRouteCacheKey>, PlaceResult);

/// Full placement selection key: route quality first, then hint adherence, then
/// layout cost, then HPWL.
pub type PlacementRankKey = (RouteRankKey, u64, u64, u64);

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

    /// Run every inner placer, route each unique LEGAL placement via the injected ranker,
    /// and return the lowest-ranked one. The rank key is `(route_rank,
    /// hint_penalty, layout_cost, hpwl)`: routing faults dominate (a worse-routed
    /// layout is never chosen), richer route quality may decide among equally-
    /// routable layouts, explicit placement hints decide next, then layout
    /// cost/HPWL break final ties. Duplicate placements from different placers
    /// share the route-ranker result, avoiding repeated routing work while still
    /// keeping each candidate's own layout-cost tie-breaks. An illegal candidate
    /// ranks saturated (it can never win). `placers[0]` wins exact ties.
    pub fn place_with_rank_key(
        &self,
        problem: &PlaceProblem,
        hints: &PlacementHints,
    ) -> (PlaceResult, PlacementRankKey) {
        // Evaluate every placer IN PARALLEL — each is an independent, pure
        // placement. Ranking then reuses route results for duplicate placement
        // geometries before the deterministic winner sort.
        use rayon::prelude::*;
        let placed: Vec<PlacedCandidate> = self
            .placers
            .par_iter()
            .enumerate()
            .map(|(i, p)| {
                let r = p.place(problem, hints);
                let placement_key = r.legal.then(|| placement_route_cache_key(&r.placements));
                (i, placement_key, r)
            })
            .collect();

        let mut unique_routes: BTreeMap<PlacementRouteCacheKey, RouteProblem> = BTreeMap::new();
        for (_, placement_key, result) in &placed {
            if let Some(placement_key) = placement_key {
                unique_routes
                    .entry(placement_key.clone())
                    .or_insert_with(|| to_route_problem(problem, &result.placements));
            }
        }
        let route_rank_cache: BTreeMap<PlacementRouteCacheKey, RouteRankKey> = unique_routes
            .par_iter()
            .map(|(placement_key, rp)| (placement_key.clone(), self.ranker.rank_key(rp)))
            .collect();

        let mut scored: Vec<(usize, PlacementRankKey, PlaceResult)> =
            Vec::with_capacity(placed.len());
        for (i, placement_key, r) in placed {
            let key = if let Some(placement_key) = placement_key {
                let route_key = route_rank_cache
                    .get(&placement_key)
                    .copied()
                    .expect("legal placement was pre-ranked");
                (
                    route_key,
                    placement_hint_penalty_um(problem, hints, &r),
                    (r.report.layout_cost * 1000.0) as u64,
                    (r.report.hpwl * 1000.0) as u64,
                )
            } else {
                (
                    (usize::MAX, usize::MAX, usize::MAX, usize::MAX, u64::MAX),
                    u64::MAX,
                    u64::MAX,
                    u64::MAX,
                )
            };
            scored.push((i, key, r));
        }
        scored.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        if scored.is_empty() {
            return (
                PlaceResult {
                    placements: Vec::new(),
                    legal: false,
                    report: PlaceReport::default(),
                },
                (
                    (usize::MAX, usize::MAX, usize::MAX, usize::MAX, u64::MAX),
                    u64::MAX,
                    u64::MAX,
                    u64::MAX,
                ),
            );
        }
        let (_, key, result) = scored.swap_remove(0);
        (result, key)
    }
}

fn placement_hint_penalty_um(
    problem: &PlaceProblem,
    hints: &PlacementHints,
    result: &PlaceResult,
) -> u64 {
    if hints.groups.is_empty() && hints.edge_seek.is_empty() && hints.corner_seek.is_empty() {
        return 0;
    }
    let placements: BTreeMap<&str, &Placement> = result
        .placements
        .iter()
        .map(|placement| (placement.reference.as_str(), placement))
        .collect();
    let mut penalty = 0.0_f64;
    for reference in &hints.edge_seek {
        let Some(part_idx) = problem
            .parts
            .iter()
            .position(|part| part.reference == *reference)
        else {
            continue;
        };
        let Some(placement) = placements.get(reference.as_str()) else {
            continue;
        };
        let half = rotated_courtyard_half(&problem.parts[part_idx], placement.rotation);
        penalty += part_edge_distance(
            &problem.parts[part_idx],
            placement.rotation,
            placement.at,
            &problem.bounds,
            half,
        );
    }
    for reference in &hints.corner_seek {
        let Some(part_idx) = problem
            .parts
            .iter()
            .position(|part| part.reference == *reference)
        else {
            continue;
        };
        let Some(placement) = placements.get(reference.as_str()) else {
            continue;
        };
        let (hw, hh) = rotated_courtyard_half(&problem.parts[part_idx], placement.rotation);
        let dx = (placement.at.x - hw - problem.bounds.min_x)
            .min(problem.bounds.max_x - (placement.at.x + hw));
        let dy = (placement.at.y - hh - problem.bounds.min_y)
            .min(problem.bounds.max_y - (placement.at.y + hh));
        penalty += dx.max(0.0) + dy.max(0.0);
    }
    for group in &hints.groups {
        for reference in &group.members {
            let Some(part_idx) = problem
                .parts
                .iter()
                .position(|part| part.reference == *reference)
            else {
                continue;
            };
            let Some(placement) = placements.get(reference.as_str()) else {
                continue;
            };
            if let Some(region) = group.region {
                let dx = if placement.at.x < region.min_x {
                    region.min_x - placement.at.x
                } else if placement.at.x > region.max_x {
                    placement.at.x - region.max_x
                } else {
                    0.0
                };
                let dy = if placement.at.y < region.min_y {
                    region.min_y - placement.at.y
                } else if placement.at.y > region.max_y {
                    placement.at.y - region.max_y
                } else {
                    0.0
                };
                penalty += dx + dy;
            }
            if let Some(edge) = group.edge {
                let (hw, hh) = rotated_courtyard_half(&problem.parts[part_idx], placement.rotation);
                penalty += match edge {
                    Edge::N => placement.at.y - hh - problem.bounds.min_y,
                    Edge::S => problem.bounds.max_y - (placement.at.y + hh),
                    Edge::W => placement.at.x - hw - problem.bounds.min_x,
                    Edge::E => problem.bounds.max_x - (placement.at.x + hw),
                }
                .max(0.0);
            }
        }
    }
    (penalty * 1000.0).round() as u64
}

impl Placer for RoutabilityOracle {
    fn name(&self) -> &'static str {
        "oracle"
    }

    fn place(&self, problem: &PlaceProblem, hints: &PlacementHints) -> PlaceResult {
        self.place_with_rank_key(problem, hints).0
    }
}

fn placement_route_cache_key(placements: &[Placement]) -> PlacementRouteCacheKey {
    let mut key: Vec<_> = placements
        .iter()
        .map(|p| {
            (
                p.reference.clone(),
                quantize_place_mm(p.at.x),
                quantize_place_mm(p.at.y),
                quantize_place_mm(p.rotation),
            )
        })
        .collect();
    key.sort();
    key
}

fn quantize_place_mm(v: f64) -> i64 {
    (v * 1000.0).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct FixedPlacer {
        x: f64,
        layout_cost: f64,
    }

    impl Placer for FixedPlacer {
        fn name(&self) -> &'static str {
            "fixed"
        }

        fn place(&self, _problem: &PlaceProblem, _hints: &PlacementHints) -> PlaceResult {
            PlaceResult {
                placements: vec![Placement {
                    reference: "P1".to_owned(),
                    at: Point2 { x: self.x, y: 5.0 },
                    rotation: 0.0,
                }],
                legal: true,
                report: PlaceReport {
                    overlaps_resolved: 0,
                    out_of_bounds_clamps: 0,
                    hpwl: self.layout_cost,
                    layout_cost: self.layout_cost,
                },
            }
        }
    }

    struct RichRanker;

    impl RouteRanker for RichRanker {
        fn faults(&self, _rp: &RouteProblem) -> usize {
            0
        }

        fn rank_key(&self, rp: &RouteProblem) -> (usize, usize, usize, usize, u64) {
            let x = rp.obstacles.first().map_or(0.0, |o| o.center.x);
            // Pretend left placement needs one via and right placement needs none.
            (0, 0, 0, if x < 5.0 { 1 } else { 0 }, 0)
        }
    }

    #[test]
    fn routability_oracle_uses_rich_rank_key_before_layout_cost() {
        let problem = PlaceProblem {
            bounds: Rect {
                min_x: 0.0,
                max_x: 10.0,
                min_y: 0.0,
                max_y: 10.0,
            },
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts: vec![Part {
                reference: "P1".to_owned(),
                courtyard_w: 1.0,
                courtyard_h: 1.0,
                pads: vec![PartPad {
                    number: "1".to_owned(),
                    offset: Point2 { x: 0.0, y: 0.0 },
                    width: 0.4,
                    height: 0.4,
                    layers: vec![LayerRef::top()],
                    net: Some("N".to_owned()),
                }],
                edge_datum: None,
                locked: None,
            }],
            keepouts: vec![],
            outline: None,
        };
        let oracle = RoutabilityOracle::new(
            vec![
                Box::new(FixedPlacer {
                    x: 2.0,
                    layout_cost: 1.0,
                }),
                Box::new(FixedPlacer {
                    x: 8.0,
                    layout_cost: 100.0,
                }),
            ],
            Box::new(RichRanker),
        );

        let result = oracle.place(&problem, &PlacementHints::default());

        assert_eq!(
            result.placements[0].at.x, 8.0,
            "richer route quality should beat lower layout cost at equal faults"
        );
    }

    struct EqualRanker;

    impl RouteRanker for EqualRanker {
        fn faults(&self, _rp: &RouteProblem) -> usize {
            0
        }

        fn rank_key(&self, _rp: &RouteProblem) -> (usize, usize, usize, usize, u64) {
            (0, 0, 0, 0, 0)
        }
    }

    struct FixedXyPlacer {
        x: f64,
        y: f64,
        layout_cost: f64,
    }

    impl Placer for FixedXyPlacer {
        fn name(&self) -> &'static str {
            "fixed-xy"
        }

        fn place(&self, _problem: &PlaceProblem, _hints: &PlacementHints) -> PlaceResult {
            PlaceResult {
                placements: vec![Placement {
                    reference: "P1".to_owned(),
                    at: Point2 {
                        x: self.x,
                        y: self.y,
                    },
                    rotation: 0.0,
                }],
                legal: true,
                report: PlaceReport {
                    overlaps_resolved: 0,
                    out_of_bounds_clamps: 0,
                    hpwl: self.layout_cost,
                    layout_cost: self.layout_cost,
                },
            }
        }
    }

    #[test]
    fn routability_oracle_honors_edge_seek_before_layout_cost() {
        let problem = PlaceProblem {
            bounds: Rect {
                min_x: 0.0,
                max_x: 10.0,
                min_y: 0.0,
                max_y: 10.0,
            },
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts: vec![Part {
                reference: "P1".to_owned(),
                courtyard_w: 1.0,
                courtyard_h: 1.0,
                pads: vec![PartPad {
                    number: "1".to_owned(),
                    offset: Point2 { x: 0.0, y: 0.0 },
                    width: 0.4,
                    height: 0.4,
                    layers: vec![LayerRef::top()],
                    net: Some("N".to_owned()),
                }],
                edge_datum: None,
                locked: None,
            }],
            keepouts: vec![],
            outline: None,
        };
        let oracle = RoutabilityOracle::new(
            vec![
                Box::new(FixedPlacer {
                    x: 5.0,
                    layout_cost: 1.0,
                }),
                Box::new(FixedPlacer {
                    x: 0.5,
                    layout_cost: 100.0,
                }),
            ],
            Box::new(EqualRanker),
        );

        let result = oracle.place(
            &problem,
            &PlacementHints {
                edge_seek: vec!["P1".to_owned()],
                ..Default::default()
            },
        );

        assert_eq!(
            result.placements[0].at.x, 0.5,
            "an equally routed edge-seek part should stay on the perimeter before layout-cost tie-breaks"
        );
    }

    #[test]
    fn routability_oracle_honors_corner_seek_before_layout_cost() {
        let problem = PlaceProblem {
            bounds: Rect {
                min_x: 0.0,
                max_x: 10.0,
                min_y: 0.0,
                max_y: 10.0,
            },
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts: vec![Part {
                reference: "P1".to_owned(),
                courtyard_w: 1.0,
                courtyard_h: 1.0,
                pads: vec![PartPad {
                    number: "1".to_owned(),
                    offset: Point2 { x: 0.0, y: 0.0 },
                    width: 0.4,
                    height: 0.4,
                    layers: vec![LayerRef::top()],
                    net: Some("N".to_owned()),
                }],
                edge_datum: None,
                locked: None,
            }],
            keepouts: vec![],
            outline: None,
        };
        let oracle = RoutabilityOracle::new(
            vec![
                Box::new(FixedXyPlacer {
                    x: 0.5,
                    y: 5.0,
                    layout_cost: 1.0,
                }),
                Box::new(FixedXyPlacer {
                    x: 0.5,
                    y: 0.5,
                    layout_cost: 100.0,
                }),
            ],
            Box::new(EqualRanker),
        );

        let result = oracle.place(
            &problem,
            &PlacementHints {
                corner_seek: vec!["P1".to_owned()],
                ..Default::default()
            },
        );

        assert_eq!(
            result.placements[0].at,
            Point2 { x: 0.5, y: 0.5 },
            "an equally routed corner-seek part should prefer a board corner before layout-cost tie-breaks"
        );
    }

    #[test]
    fn routability_oracle_honors_group_region_before_layout_cost() {
        let problem = PlaceProblem {
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts: vec![Part {
                reference: "P1".to_owned(),
                courtyard_w: 1.0,
                courtyard_h: 1.0,
                pads: vec![],
                edge_datum: None,
                locked: None,
            }],
            keepouts: vec![],
            outline: None,
        };
        let oracle = RoutabilityOracle::new(
            vec![
                Box::new(FixedXyPlacer {
                    x: 8.0,
                    y: 5.0,
                    layout_cost: 1.0,
                }),
                Box::new(FixedXyPlacer {
                    x: 1.0,
                    y: 1.0,
                    layout_cost: 100.0,
                }),
            ],
            Box::new(EqualRanker),
        );
        let hints = PlacementHints {
            groups: vec![GroupHint {
                name: "authored".to_owned(),
                members: vec!["P1".to_owned()],
                region: Some(Rect::new(0.5, 0.5, 2.0, 2.0)),
                edge: Some(Edge::N),
                grid: false,
                rotation: None,
                surround: None,
            }],
            ..PlacementHints::default()
        };

        let result = oracle.place(&problem, &hints);

        assert_eq!(
            result.placements[0].at,
            Point2::new(1.0, 1.0),
            "an equally routed placement inside its authored region and near its authored edge must win"
        );
    }

    struct CountingRanker {
        calls: Arc<AtomicUsize>,
    }

    impl RouteRanker for CountingRanker {
        fn faults(&self, _rp: &RouteProblem) -> usize {
            0
        }

        fn rank_key(&self, _rp: &RouteProblem) -> (usize, usize, usize, usize, u64) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (0, 0, 0, 0, 0)
        }
    }

    #[test]
    fn routability_oracle_caches_duplicate_placement_route_ranks() {
        let problem = PlaceProblem {
            bounds: Rect {
                min_x: 0.0,
                max_x: 10.0,
                min_y: 0.0,
                max_y: 10.0,
            },
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts: vec![Part {
                reference: "P1".to_owned(),
                courtyard_w: 1.0,
                courtyard_h: 1.0,
                pads: vec![PartPad {
                    number: "1".to_owned(),
                    offset: Point2 { x: 0.0, y: 0.0 },
                    width: 0.4,
                    height: 0.4,
                    layers: vec![LayerRef::top()],
                    net: Some("N".to_owned()),
                }],
                edge_datum: None,
                locked: None,
            }],
            keepouts: vec![],
            outline: None,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let oracle = RoutabilityOracle::new(
            vec![
                Box::new(FixedPlacer {
                    x: 2.0,
                    layout_cost: 10.0,
                }),
                Box::new(FixedPlacer {
                    x: 2.0,
                    layout_cost: 5.0,
                }),
            ],
            Box::new(CountingRanker {
                calls: calls.clone(),
            }),
        );

        let (result, key) = oracle.place_with_rank_key(&problem, &PlacementHints::default());

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "identical placement geometry should be routed only once"
        );
        assert_eq!(key.0, (0, 0, 0, 0, 0));
        assert_eq!(
            result.report.layout_cost, 5.0,
            "duplicate route rank must still leave layout cost as the tie-break"
        );
    }
}
