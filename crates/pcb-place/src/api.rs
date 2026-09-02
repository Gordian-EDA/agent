//! Placement geometry and derived-net helpers used by the concrete placer.

use geom::{Point2, Rect};
use pcb_model::{Connection, Obstacle, RoutePoint, RoutingView};
pub use pcb_model::{
    Edge, EdgeDatum, GroupHint, LayerRef, LockedAt, Part, PartPad, PlaceReport, PlaceResult,
    Placement, PlacementHints, PlacementView,
};
use std::collections::BTreeMap;

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
pub fn derive_nets(problem: &PlacementView) -> Vec<LogicalNet> {
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
pub fn pad_world(problem: &PlacementView, pos: &[Point2], pin: &Pin) -> Point2 {
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
    problem: &PlacementView,
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
    problem: &PlacementView,
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
pub fn compute_hpwl(problem: &PlacementView, nets: &[LogicalNet], pos: &[Point2]) -> f64 {
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
    problem: &PlacementView,
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
pub fn decoupling_pairs(problem: &PlacementView) -> Vec<(usize, usize)> {
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
pub fn series_pairs(problem: &PlacementView) -> Vec<(usize, usize)> {
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
pub fn series_fanout_order(problem: &PlacementView, ic: usize, parts: &[usize]) -> Vec<String> {
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

// ── routing_view ───────────────────────────────────────────────────────────

/// Via geometry carried into the emitted [`RoutingView`] (mirrors the route
/// model's defaults — the value the existing fixtures and oracle expect).
const DEFAULT_VIA_DIAMETER: f64 = 0.6;
const DEFAULT_VIA_DRILL: f64 = 0.3;

/// Build a [`RoutingView`] from a placement: every pad becomes a net-attributed
/// obstacle (at its placed+rotated world position), and every multi-pin net
/// becomes a [`Connection`] whose `points_to_connect` are the pad centers on the
/// pad's layer. Board bounds and design rules are carried from the problem.
///
/// The emitted view round-trips serde and is accepted by the tuned routing phase
/// and the connectivity oracle unchanged (pads on nets, points on pads).
pub fn routing_view(problem: &PlacementView, placements: &[Placement]) -> RoutingView {
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

    RoutingView {
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
        fixed_copper: Default::default(),
        nets: None,
    }
}
