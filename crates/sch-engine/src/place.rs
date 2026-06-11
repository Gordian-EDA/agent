//! Deterministic block placement.
//!
//! Turns a [`circuit_lang::Design`] into per-component sheet positions. This is
//! pure geometry — no I/O, no KiCAD. The MVP bar (spec §8) is *tidy,
//! grid-aligned, ERC-clean* rather than beautiful: each component gets a cell
//! sized to its symbol bounding box plus clearance; clusters (a parent plus its
//! decouple caps as a horizontal bank) are packed into rows that wrap at a
//! ~square width; blocks are arranged into vertical bands by edge hint. Same
//! `Design` -> same `Layout`.

use circuit_lang::Design;
use circuit_lang::model::{Block, Edge, Origin, RefDes};
use indexmap::IndexMap;

use crate::cluster_geom::ClusterGeom;
use crate::emit::Dir;
use crate::grammar::BlockGraph;
use crate::grid::snap_point;

/// Computed positions for every component, keyed by refdes.
///
/// Coordinates are in KiCAD sheet millimetres (x grows right, y grows *down*)
/// and every value lands on the 1.27 mm grid via [`crate::grid::snap_point`].
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub positions: IndexMap<RefDes, [f64; 2]>,
    /// Engine-chosen orientation per component (degrees). Cluster members carry
    /// their geometry angle; anchors are 0 here (reconcile's `initial_angle`
    /// handles non-cluster RailSpan flips).
    pub angles: IndexMap<RefDes, f64>,
    /// Absolute sheet origin of each cluster, keyed by [`cluster_key`].
    pub cluster_origins: IndexMap<String, [f64; 2]>,
    /// Join wires from an anchor pin endpoint to a pin-anchored cluster's tap
    /// point: (pin_endpoint, tap_point, net). Empty unless anchor slotting fired.
    pub joins: Vec<([f64; 2], [f64; 2], String)>,
}

/// Stable cluster identity for layout + reconciliation: block name + index in
/// the block's deterministic cluster order.
pub fn cluster_key(block: &str, idx: usize) -> String {
    format!("{block}\u{0}{idx}")
}

/// Per-refdes approximate body extents `[width, height]` in mm, as produced by
/// [`kicad_bridge::geometry::SymbolGeometry::approx_size`]. A refdes absent from
/// the map falls back to the legacy fixed [`CELL_MM`] cell.
pub type SizeMap = IndexMap<RefDes, [f64; 2]>;

/// Anchor pin geometry: (refdes, pin) -> (offset from the symbol origin at
/// angle 0, outward direction). Caller derives it from `SymbolGeometry`.
pub type AnchorPinEnds = IndexMap<(RefDes, String), ([f64; 2], Dir)>;

/// Gap between an anchor pin and an anchored cluster's tap, mm.
const JOIN_MM: f64 = 5.08;

/// Fixed grid-cell pitch for a component with no known size, in mm. Generous —
/// sheet space is free and a wide pitch keeps symbols, their labels, and
/// decouple caps clear of each other so ERC stays clean.
const CELL_MM: f64 = 25.4;

/// Clearance added around a symbol's bbox for stubs, labels, and fields, mm.
const CLEARANCE_MM: f64 = 15.24;

/// Horizontal gutter between block columns (edge bands), in mm.
const BAND_GAP_MM: f64 = 12.7;

/// Vertical gutter between blocks stacked within the same band, in mm.
const BLOCK_GAP_MM: f64 = 12.7;

/// Sheet margin from the origin where placement begins, in mm.
const MARGIN_MM: f64 = 25.4;

/// Which horizontal band a block lands in, derived from its edge hint. Lower
/// rank == further left on the sheet (smaller x). The ordering also fixes the
/// deterministic block sequence: edge-pinned blocks first (by edge), then the
/// unpinned rest, preserving `Design` block order within each group.
fn band_rank(edge: Option<Edge>) -> u8 {
    match edge {
        Some(Edge::Left) => 0,
        Some(Edge::Top) => 1,
        Some(Edge::Bottom) => 2,
        None => 3,
        Some(Edge::Right) => 4,
    }
}

/// Snap a length up to the 2.54 mm placement grid.
fn snap_up(v: f64) -> f64 {
    (v / 2.54).ceil() * 2.54
}

/// A component's cell extents: bbox + clearance, or the legacy fallback.
fn cell_of(refdes: &RefDes, sizes: &SizeMap) -> [f64; 2] {
    match sizes.get(refdes) {
        Some(s) => [snap_up(s[0] + CLEARANCE_MM), snap_up(s[1] + CLEARANCE_MM)],
        None => [CELL_MM, CELL_MM],
    }
}

/// Which side of its anchor a cluster attaches to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    East,
    West,
    North,
    South,
}

/// Side from the PRIMARY anchor tap (first tap whose pin direction is known —
/// anchor_taps order is deterministic from `analyze`). Falls back to rail
/// polarity (`side_of_tapless`) over the taps' nets when no tap resolves.
pub(crate) fn cluster_side(
    taps: &[(String, String, String)],
    pin_ends: &AnchorPinEnds,
    fallback_nets: &[String],
    is_vplus: &dyn Fn(&str) -> bool,
    is_gnd: &dyn Fn(&str) -> bool,
) -> Side {
    for (_net, aref, apin) in taps {
        if let Some(&(_, dir)) = pin_ends.get(&(aref.clone(), apin.clone())) {
            return match dir {
                Dir::East => Side::East,
                Dir::West => Side::West,
                Dir::North => Side::North,
                Dir::South => Side::South,
            };
        }
    }
    side_of_tapless(fallback_nets, is_vplus, is_gnd)
}

/// Rail-polarity side for a cluster with no resolvable tap: V+ -> North
/// (supply feeds hang from the top), ground-only -> South, neither -> South
/// (decoupling/banks read best at the bottom, matching the references).
pub(crate) fn side_of_tapless(
    nets: &[String],
    is_vplus: &dyn Fn(&str) -> bool,
    is_gnd: &dyn Fn(&str) -> bool,
) -> Side {
    if nets.iter().any(|n| is_vplus(n)) {
        Side::North
    } else if nets.iter().any(|n| is_gnd(n)) {
        Side::South
    } else {
        Side::South
    }
}

/// A block laid out anchor-centrically, in BLOCK-LOCAL coordinates (the anchor
/// row sits on local y = 0; north clusters go negative). The caller translates
/// by a grid-snapped shift, so all the local relationships (tap alignment,
/// join straightness) survive verbatim.
struct LocalPlan {
    /// Anchor centers (local, grid-aligned).
    anchors: Vec<(RefDes, [f64; 2])>,
    /// Cluster members: (refdes, local position, angle).
    members: Vec<(RefDes, [f64; 2], f64)>,
    /// (cluster index, local origin).
    cluster_origins: Vec<(usize, [f64; 2])>,
    /// (pin endpoint, tap point, net), local.
    joins: Vec<([f64; 2], [f64; 2], String)>,
    /// Local bbox [min_x, min_y, max_x, max_y] over every placed rect.
    bbox: [f64; 4],
}

/// Gap between an anchor row and the north/south cluster rows, mm.
const SIDE_ROW_GAP_MM: f64 = 5.08;

/// Horizontal gap between clusters sharing a north/south row, mm.
const SIDE_ROW_PITCH_MM: f64 = 5.08;

/// Outward-push step when a slotted cluster overlaps, mm (2 grid units).
const PUSH_MM: f64 = 2.54;

/// Maximum outward-push steps before a cluster falls back to the leftover row.
const PUSH_CAP: usize = 40;

/// Anchor-centric layout of one block, in local coordinates.
///
/// Anchors line up left-to-right on local y=0; each cluster slots beside the
/// anchor pin it taps (East/West, tap-aligned so the join wire is straight) or
/// stacks in a row above (V+ feeds) / below (ground/banks). Overlaps resolve
/// by pushing outward along the assigned side; a cluster that cannot find a
/// free spot within [`PUSH_CAP`] steps joins the leftover row at the bottom.
fn layout_block_anchor_centric(
    block: &Block,
    graph: &BlockGraph,
    geoms: &[ClusterGeom],
    sizes: &SizeMap,
    pin_ends: &AnchorPinEnds,
    is_vplus: &dyn Fn(&str) -> bool,
) -> LocalPlan {
    let mut plan = LocalPlan {
        anchors: Vec::new(),
        members: Vec::new(),
        cluster_origins: Vec::new(),
        joins: Vec::new(),
        bbox: [f64::MAX, f64::MAX, f64::MIN, f64::MIN],
    };
    // Occupied rects: (min, max). Anchor rects use the BODY size plus a small
    // pad (not the label-clearance cell) so slotted clusters sit snug; the
    // push loop resolves any genuine collision.
    let mut occupied: Vec<([f64; 2], [f64; 2])> = Vec::new();
    fn grow(b: &mut [f64; 4], mn: [f64; 2], mx: [f64; 2]) {
        b[0] = b[0].min(mn[0]);
        b[1] = b[1].min(mn[1]);
        b[2] = b[2].max(mx[0]);
        b[3] = b[3].max(mx[1]);
    }

    // 1. Anchor row on y = 0.
    let mut anchor_pos: IndexMap<&str, [f64; 2]> = IndexMap::new();
    let mut x = 0.0;
    for anchor in &graph.anchors {
        let cell = cell_of(anchor, sizes);
        let body = sizes
            .get(anchor)
            .map(|s| [s[0] + 5.08, s[1] + 5.08])
            .unwrap_or(cell);
        let center = snap_point([x + cell[0] / 2.0, 0.0]);
        anchor_pos.insert(anchor.as_str(), center);
        plan.anchors.push((anchor.clone(), center));
        let (mn, mx) = (
            [center[0] - body[0] / 2.0, center[1] - body[1] / 2.0],
            [center[0] + body[0] / 2.0, center[1] + body[1] / 2.0],
        );
        occupied.push((mn, mx));
        grow(&mut plan.bbox, mn, mx);
        x += cell[0] + BAND_GAP_MM;
    }
    let row_top = occupied.iter().map(|(mn, _)| mn[1]).fold(0.0, f64::min);
    let row_bottom = occupied.iter().map(|(_, mx)| mx[1]).fold(0.0, f64::max);

    // 2./3. Clusters by side.
    let mut north_cursor = 0.0;
    let mut south_cursor = 0.0;
    let mut leftovers: Vec<usize> = Vec::new();
    for (ci, cluster) in graph.clusters.iter().enumerate() {
        let Some(geom) = geoms.get(ci) else {
            leftovers.push(ci);
            continue;
        };
        let env = geom.envelope;
        // The cluster's own nets, for rail-polarity fallback when no tap
        // resolves (e.g. a rail-rail decouple bank has no anchor taps at all).
        let mut cluster_nets: Vec<String> = Vec::new();
        for chain in &cluster.chains {
            for l in &chain.links {
                cluster_nets.push(l.a_net.clone());
                cluster_nets.push(l.b_net.clone());
            }
        }
        for bank in &cluster.banks {
            cluster_nets.push(bank.a_net.clone());
            cluster_nets.push(bank.b_net.clone());
        }
        let side = if cluster.chains.is_empty() && !cluster.banks.is_empty() {
            // A pure bank (parallel decouple caps) always reads best as the
            // bottom row, reference-style — even though it touches a V+ rail.
            Side::South
        } else {
            cluster_side(
                &cluster.anchor_taps,
                pin_ends,
                &cluster_nets,
                is_vplus,
                &crate::grammar::is_ground,
            )
        };
        // Primary tap: first tap with a known pin end on a placed anchor AND a
        // tap point in the geometry. The pin endpoint is snapped here so the
        // recorded join is dust-free (anchor centers and pin offsets are both
        // grid values in exact arithmetic).
        let primary = cluster.anchor_taps.iter().find_map(|(net, aref, apin)| {
            let &(off, dir) = pin_ends.get(&(aref.clone(), apin.clone()))?;
            let aidx = anchor_pos.get_index_of(aref.as_str())?;
            let &apos = anchor_pos.get(aref.as_str())?;
            let &tap = geom.tap_points.get(net)?;
            Some((
                net.clone(),
                snap_point([apos[0] + off[0], apos[1] + off[1]]),
                dir,
                tap,
                aidx,
            ))
        });

        let commit = |o: [f64; 2],
                          join: Option<([f64; 2], [f64; 2], String)>,
                          plan: &mut LocalPlan,
                          occupied: &mut Vec<([f64; 2], [f64; 2])>| {
            plan.cluster_origins.push((ci, o));
            for (refdes, local, angle) in &geom.placements {
                plan.members
                    .push((refdes.clone(), [o[0] + local[0], o[1] + local[1]], *angle));
            }
            if let Some(j) = join {
                plan.joins.push(j);
            }
            let (mn, mx) = (o, [o[0] + env[0], o[1] + env[1]]);
            occupied.push((mn, mx));
            grow(&mut plan.bbox, mn, mx);
        };

        // Whether the horizontal join segment at `y`, spanning `xa..xb`,
        // crosses any occupied rect. A join that crosses a rect would run a
        // wire THROUGH another unit's body — an electrical short waiting to
        // happen — so such a slot is rejected outright (the cluster falls back
        // to a row, keeping label connectivity).
        // `skip` is the tapped anchor's own rect index (anchors occupy the
        // first `graph.anchors.len()` slots): the pin endpoint legitimately
        // sits inside its own anchor's padded rect.
        let seg_clear = |y: f64, xa: f64, xb: f64, skip: usize, occ: &[([f64; 2], [f64; 2])]| {
            let (lo, hi) = (xa.min(xb), xa.max(xb));
            occ.iter().enumerate().all(|(k, (mn, mx))| {
                k == skip || !(mn[1] < y && y < mx[1] && mn[0] < hi && lo < mx[0])
            })
        };

        // East/West: tap-aligned slot with a straight join. Snap x ONCE up
        // front; keep y EXACT (pin.y - tap.y) so the join stays horizontal.
        // Returns false when no SAFE slot exists (overlap cap, or the join
        // would cross an earlier unit) — the cluster then takes row placement.
        let slotted = match (side, &primary) {
            (Side::East, Some((net, pin, Dir::East, tap, aidx))) => {
                let mut o = [snap_point([pin[0] + JOIN_MM, 0.0])[0], pin[1] - tap[1]];
                let mut tries = 0;
                while overlaps_any(o, env, &occupied) && tries < PUSH_CAP {
                    o[0] += PUSH_MM;
                    tries += 1;
                }
                let tap_sheet = [o[0] + tap[0], o[1] + tap[1]];
                if tries < PUSH_CAP && seg_clear(pin[1], pin[0], tap_sheet[0], *aidx, &occupied) {
                    commit(o, Some((*pin, tap_sheet, net.clone())), &mut plan, &mut occupied);
                    true
                } else {
                    false
                }
            }
            (Side::West, Some((net, pin, Dir::West, tap, aidx))) => {
                let mut o = [
                    snap_point([pin[0] - JOIN_MM - env[0], 0.0])[0],
                    pin[1] - tap[1],
                ];
                let mut tries = 0;
                while overlaps_any(o, env, &occupied) && tries < PUSH_CAP {
                    o[0] -= PUSH_MM;
                    tries += 1;
                }
                let tap_sheet = [o[0] + tap[0], o[1] + tap[1]];
                if tries < PUSH_CAP && seg_clear(pin[1], pin[0], tap_sheet[0], *aidx, &occupied) {
                    commit(o, Some((*pin, tap_sheet, net.clone())), &mut plan, &mut occupied);
                    true
                } else {
                    false
                }
            }
            _ => false,
        };

        // North/South rows: x near the tap pin when known, else a running
        // cursor; push outward (away from the anchor row) on overlap. No
        // join wires — label connectivity until the router slice. Also the
        // fallback for E/W clusters whose slot was unsafe.
        if !slotted {
            let ns = match side {
                Side::North => Side::North,
                Side::South => Side::South,
                // E/W cluster that couldn't slot: rail polarity decides.
                _ => side_of_tapless(&cluster_nets, is_vplus, &crate::grammar::is_ground),
            };
            let (start_y, dir_y, cursor) = match ns {
                Side::North => (
                    row_top - SIDE_ROW_GAP_MM - env[1],
                    -PUSH_MM,
                    &mut north_cursor,
                ),
                _ => (row_bottom + SIDE_ROW_GAP_MM, PUSH_MM, &mut south_cursor),
            };
            let x0 = match &primary {
                Some((_, pin, _, _, _)) => pin[0] - env[0] / 2.0,
                None => *cursor,
            };
            let mut o = [snap_point([x0, 0.0])[0], start_y];
            let mut tries = 0;
            while overlaps_any(o, env, &occupied) && tries < PUSH_CAP {
                o[1] += dir_y;
                tries += 1;
            }
            if tries == PUSH_CAP {
                leftovers.push(ci);
                continue;
            }
            if primary.is_none() {
                *cursor = o[0] + env[0] + SIDE_ROW_PITCH_MM;
            }
            commit(o, None, &mut plan, &mut occupied);
        }
    }

    // 5. Leftovers: packed in a row below everything placed so far.
    if !leftovers.is_empty() {
        let extents: Vec<[f64; 2]> = leftovers
            .iter()
            .map(|&ci| geoms.get(ci).map(|g| g.envelope).unwrap_or([CELL_MM, CELL_MM]))
            .collect();
        let (origins, _) = pack_units(&extents);
        let y0 = plan.bbox[3] + BLOCK_GAP_MM;
        for (k, &ci) in leftovers.iter().enumerate() {
            let o = [origins[k][0], y0 + origins[k][1]];
            let env = extents[k];
            plan.cluster_origins.push((ci, o));
            if let Some(geom) = geoms.get(ci) {
                for (refdes, local, angle) in &geom.placements {
                    plan.members
                        .push((refdes.clone(), [o[0] + local[0], o[1] + local[1]], *angle));
                }
            }
            let (mn, mx) = (o, [o[0] + env[0], o[1] + env[1]]);
            occupied.push((mn, mx));
            grow(&mut plan.bbox, mn, mx);
        }
    }

    let _ = block;
    plan
}

/// One packable unit inside a block: a standalone anchor, or a grammar cluster
/// referenced by its index in the block's deterministic cluster order.
enum Unit {
    Anchor(RefDes),
    Cluster(usize),
}

/// Deterministic unit order: anchors in natural order, each immediately
/// followed by the clusters whose members are its synthesized children
/// (decouple banks stay beside their parent), then remaining clusters in
/// cluster order.
fn block_units(block: &Block, graph: &BlockGraph) -> Vec<Unit> {
    let mut units = Vec::new();
    let mut used: Vec<bool> = vec![false; graph.clusters.len()];
    for anchor in &graph.anchors {
        units.push(Unit::Anchor(anchor.clone()));
        for (ci, cluster) in graph.clusters.iter().enumerate() {
            if used[ci] {
                continue;
            }
            let child_of_anchor = cluster.members().iter().any(|m| {
                matches!(
                    &block.components[m.as_str()].origin,
                    Origin::Synthesized { parent, .. } if parent == anchor
                )
            });
            if child_of_anchor {
                used[ci] = true;
                units.push(Unit::Cluster(ci));
            }
        }
    }
    for (ci, _) in graph.clusters.iter().enumerate() {
        if !used[ci] {
            units.push(Unit::Cluster(ci));
        }
    }
    units
}

/// Pack a block's units into rows wrapping at a ~square width; rows center
/// their units vertically. Returns unit top-left origins + block envelope.
fn pack_units(extents: &[[f64; 2]]) -> (Vec<[f64; 2]>, [f64; 2]) {
    let total: f64 = extents.iter().map(|e| e[0] * e[1]).sum();
    let target_w = total
        .sqrt()
        .max(extents.iter().map(|e| e[0]).fold(0.0, f64::max));
    // First pass: assign rows.
    let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
    let mut x = 0.0;
    for (i, e) in extents.iter().enumerate() {
        if x > 0.0 && x + e[0] > target_w {
            rows.push(Vec::new());
            x = 0.0;
        }
        rows.last_mut().unwrap().push(i);
        x += e[0];
    }
    // Second pass: place with vertical centering per row.
    let mut origins = vec![[0.0, 0.0]; extents.len()];
    let mut y = 0.0;
    let mut max_w = 0.0_f64;
    for row in &rows {
        let row_h = row.iter().map(|&i| extents[i][1]).fold(0.0, f64::max);
        let mut x = 0.0;
        for &i in row {
            origins[i] = [x, y + (row_h - extents[i][1]) / 2.0];
            x += extents[i][0];
        }
        max_w = max_w.max(x);
        y += row_h;
    }
    (origins, [max_w, y])
}

/// Compute deterministic positions for every component in `design`.
///
/// Blocks are grouped into vertical bands by edge hint (Left band leftmost,
/// Right band rightmost); within a band each block stacks downward and occupies
/// a rectangular region; within a region components fill bbox-sized cells packed
/// into rows (clusters kept atomic), grid-snapped. Same `Design` -> same `Layout`.
pub fn place(
    design: &Design,
    sizes: &SizeMap,
    graphs: &IndexMap<String, BlockGraph>,
    geoms: &IndexMap<String, Vec<ClusterGeom>>,
) -> Layout {
    place_with_anchor_pins(design, sizes, graphs, geoms, &AnchorPinEnds::new())
}

/// Whether two axis-aligned rects `[a_min, a_max]` and `[b_min, b_max]` overlap.
/// A tiny epsilon keeps edge-touching rects from counting as an overlap.
fn overlaps_any(o: [f64; 2], env: [f64; 2], rects: &[([f64; 2], [f64; 2])]) -> bool {
    const EPS: f64 = 1e-6;
    let a_min = o;
    let a_max = [o[0] + env[0], o[1] + env[1]];
    rects.iter().any(|(b_min, b_max)| {
        a_min[0] + EPS < b_max[0]
            && b_min[0] + EPS < a_max[0]
            && a_min[1] + EPS < b_max[1]
            && b_min[1] + EPS < a_max[1]
    })
}

/// Like [`place`], but given anchor pin endpoints, attempts to slot any cluster
/// with exactly one in-block anchor tap (East/West pin) beside that pin,
/// recording a join wire. Clusters that can't be slotted keep their packed
/// position. Production callers use [`place`] (empty `pin_ends`), so layout is
/// unchanged unless `pin_ends` is supplied.
pub fn place_with_anchor_pins(
    design: &Design,
    sizes: &SizeMap,
    graphs: &IndexMap<String, BlockGraph>,
    geoms: &IndexMap<String, Vec<ClusterGeom>>,
    pin_ends: &AnchorPinEnds,
) -> Layout {
    // Stable block order: by band rank, then original `Design` order.
    let mut blocks: Vec<(&str, &Block)> =
        design.blocks.iter().map(|(n, b)| (n.as_str(), b)).collect();
    blocks.sort_by_key(|(_, b)| band_rank(b.layout.edge));
    // `sort_by_key` is stable, so equal-rank blocks keep `Design` order.

    let empty_graph = BlockGraph::default();

    // Whether a net counts as a positive supply for side classification: a
    // declared power net that is not a ground.
    let is_vplus = |n: &str| {
        design
            .nets
            .get(n)
            .map(|a| a.power)
            .unwrap_or(false)
            && !crate::grammar::is_ground(n)
    };

    // Pre-compute each block's layout plan once; reuse below for both
    // band-width accumulation and final placement. Blocks WITH anchors get the
    // anchor-centric local plan; anchor-less blocks keep row packing.
    struct Packed<'a> {
        units: Vec<Unit>,
        extents: Vec<[f64; 2]>,
        origins: Vec<[f64; 2]>,
        env: [f64; 2],
        block_name: &'a str,
    }
    enum BlockPlan<'a> {
        Packed(Packed<'a>),
        Centric { plan: LocalPlan, block_name: &'a str },
    }
    impl BlockPlan<'_> {
        fn env(&self) -> [f64; 2] {
            match self {
                BlockPlan::Packed(p) => p.env,
                BlockPlan::Centric { plan, .. } => [
                    (plan.bbox[2] - plan.bbox[0]).max(0.0),
                    (plan.bbox[3] - plan.bbox[1]).max(0.0),
                ],
            }
        }
    }
    let packed: Vec<BlockPlan> = blocks
        .iter()
        .map(|(name, block)| {
            let graph = graphs.get(*name).unwrap_or(&empty_graph);
            if !graph.anchors.is_empty() {
                let empty_geoms: Vec<ClusterGeom> = Vec::new();
                let gs = geoms.get(*name).unwrap_or(&empty_geoms);
                let plan = layout_block_anchor_centric(
                    block, graph, gs, sizes, pin_ends, &is_vplus,
                );
                return BlockPlan::Centric { plan, block_name: name };
            }
            let units = block_units(block, graph);
            let extents: Vec<[f64; 2]> = units
                .iter()
                .map(|u| match u {
                    Unit::Anchor(r) => cell_of(r, sizes),
                    Unit::Cluster(ci) => geoms
                        .get(*name)
                        .and_then(|gs| gs.get(*ci))
                        .map(|g| g.envelope)
                        .unwrap_or([CELL_MM, CELL_MM]),
                })
                .collect();
            let (origins, env) = pack_units(&extents);
            BlockPlan::Packed(Packed { units, extents, origins, env, block_name: name })
        })
        .collect();

    // x-base for each band: bands laid left-to-right in ascending rank. Each
    // band is as wide as its widest block's envelope; we accumulate widths in mm
    // so bands never overlap horizontally.
    let mut band_x: IndexMap<u8, f64> = IndexMap::new();
    let mut cursor_x = MARGIN_MM;
    let mut ranks: Vec<u8> = blocks
        .iter()
        .map(|(_, b)| band_rank(b.layout.edge))
        .collect();
    ranks.sort_unstable();
    ranks.dedup();
    for rank in &ranks {
        // Widest block envelope in this band determines the band's width.
        let max_w = blocks
            .iter()
            .zip(packed.iter())
            .filter(|((_, b), _)| band_rank(b.layout.edge) == *rank)
            .map(|(_, p)| p.env()[0])
            .fold(0.0_f64, f64::max);
        band_x.insert(*rank, cursor_x);
        cursor_x += max_w + BAND_GAP_MM;
    }

    let mut positions: IndexMap<RefDes, [f64; 2]> = IndexMap::new();
    let mut angles: IndexMap<RefDes, f64> = IndexMap::new();
    let mut cluster_origins: IndexMap<String, [f64; 2]> = IndexMap::new();
    let mut joins: Vec<([f64; 2], [f64; 2], String)> = Vec::new();
    // Independent vertical cursor per band so stacked blocks don't collide.
    let mut band_y: IndexMap<u8, f64> = IndexMap::new();

    for ((_name, block), bp) in blocks.iter().zip(packed.iter()) {
        let rank = band_rank(block.layout.edge);
        let block_x0 = band_x[&rank];
        let block_y0 = *band_y.entry(rank).or_insert(MARGIN_MM);

        // Anchor-centric block: translate the local plan by a grid-snapped
        // shift so every local relationship (tap alignment, straight joins)
        // survives verbatim, then advance the band cursor.
        let p = match bp {
            BlockPlan::Centric { plan, block_name } => {
                let shift = snap_point([block_x0 - plan.bbox[0], block_y0 - plan.bbox[1]]);
                for (refdes, at) in &plan.anchors {
                    positions.insert(refdes.clone(), snap_point([at[0] + shift[0], at[1] + shift[1]]));
                    angles.insert(refdes.clone(), 0.0);
                }
                for (refdes, at, angle) in &plan.members {
                    positions.insert(refdes.clone(), snap_point([at[0] + shift[0], at[1] + shift[1]]));
                    angles.insert(refdes.clone(), *angle);
                }
                for (ci, o) in &plan.cluster_origins {
                    cluster_origins.insert(
                        cluster_key(block_name, *ci),
                        [o[0] + shift[0], o[1] + shift[1]],
                    );
                }
                for (pin, tap, net) in &plan.joins {
                    joins.push((
                        [pin[0] + shift[0], pin[1] + shift[1]],
                        [tap[0] + shift[0], tap[1] + shift[1]],
                        net.clone(),
                    ));
                }
                let env = bp.env();
                band_y.insert(rank, block_y0 + env[1] + BLOCK_GAP_MM);
                // Fallback guard for components the plan missed.
                let mut fallback_y = block_y0 + env[1] + BLOCK_GAP_MM;
                for refdes in block.components.keys() {
                    if positions.contains_key(refdes) {
                        continue;
                    }
                    let cell = cell_of(refdes, sizes);
                    let center = [block_x0 + cell[0] / 2.0, fallback_y + cell[1] / 2.0];
                    positions.insert(refdes.clone(), snap_point(center));
                    angles.insert(refdes.clone(), 0.0);
                    fallback_y += cell[1] + BLOCK_GAP_MM;
                }
                continue;
            }
            BlockPlan::Packed(p) => p,
        };

        // Each placed unit's rect [min, max] in sheet mm, tagged with the cluster
        // index it belongs to (None for anchors) — used for anchor-slot overlap.
        let mut unit_rects: Vec<(Option<usize>, [f64; 2], [f64; 2])> = Vec::new();

        for (i, unit) in p.units.iter().enumerate() {
            let uo = p.origins[i];
            match unit {
                Unit::Anchor(refdes) => {
                    let ext = p.extents[i];
                    let center = [
                        block_x0 + uo[0] + ext[0] / 2.0,
                        block_y0 + uo[1] + ext[1] / 2.0,
                    ];
                    let pos = snap_point(center);
                    positions.insert(refdes.clone(), pos);
                    angles.insert(refdes.clone(), 0.0);
                    unit_rects.push((
                        None,
                        [center[0] - ext[0] / 2.0, center[1] - ext[1] / 2.0],
                        [center[0] + ext[0] / 2.0, center[1] + ext[1] / 2.0],
                    ));
                }
                Unit::Cluster(ci) => {
                    let o = snap_point([block_x0 + uo[0], block_y0 + uo[1]]);
                    cluster_origins.insert(cluster_key(p.block_name, *ci), o);
                    let env = geoms
                        .get(p.block_name)
                        .and_then(|gs| gs.get(*ci))
                        .map(|g| g.envelope)
                        .unwrap_or([0.0, 0.0]);
                    unit_rects.push((Some(*ci), o, [o[0] + env[0], o[1] + env[1]]));
                    if let Some(geom) = geoms.get(p.block_name).and_then(|gs| gs.get(*ci)) {
                        for (refdes, local, angle) in &geom.placements {
                            positions.insert(
                                refdes.clone(),
                                snap_point([o[0] + local[0], o[1] + local[1]]),
                            );
                            angles.insert(refdes.clone(), *angle);
                        }
                    }
                }
            }
        }

        // Anchor-slot post-pass: now that every unit in this block is placed and
        // its rect known, try to relocate each single-tap cluster beside its
        // in-block anchor pin. Clusters processed in index order; first-fit; a
        // relocated cluster's new rect is visible to later clusters.
        if let Some(graph) = graphs.get(p.block_name) {
            for (ci, cluster) in graph.clusters.iter().enumerate() {
                if cluster.anchor_taps.len() != 1 {
                    continue;
                }
                let (net, aref, apin) = &cluster.anchor_taps[0];
                let Some(geom) = geoms.get(p.block_name).and_then(|gs| gs.get(ci)) else {
                    continue;
                };
                let Some(&tap_local) = geom.tap_points.get(net) else {
                    continue;
                };
                let Some(&(off, dir)) = pin_ends.get(&(aref.clone(), apin.clone())) else {
                    continue;
                };
                if dir != Dir::East && dir != Dir::West {
                    continue;
                }
                let Some(&anchor_pos) = positions.get(aref) else {
                    continue;
                };
                // Anchors are emitted at angle 0; pin end is anchor origin + off.
                let pin = [anchor_pos[0] + off[0], anchor_pos[1] + off[1]];
                let raw_o = match dir {
                    Dir::East => [pin[0] + JOIN_MM, pin[1] - tap_local[1]],
                    Dir::West => [pin[0] - JOIN_MM - geom.envelope[0], pin[1] - tap_local[1]],
                    _ => unreachable!(),
                };
                // Snap x to the grid ONCE, up front, so the rect we overlap-check
                // is exactly the rect we commit (no post-check drift). Keep y EXACT
                // (`pin.y - tap_local.y`) so `tap_sheet.y == pin.y` and the join
                // wire stays perfectly horizontal — snapping y could break that.
                let new_o = [snap_point([raw_o[0], 0.0])[0], raw_o[1]];
                // Overlap against every OTHER unit's rect in this block.
                let others: Vec<([f64; 2], [f64; 2])> = unit_rects
                    .iter()
                    .filter(|(tag, _, _)| *tag != Some(ci))
                    .map(|(_, mn, mx)| (*mn, *mx))
                    .collect();
                if overlaps_any(new_o, geom.envelope, &others) {
                    continue;
                }
                // Commit: relocate members + cluster origin, record the join.
                cluster_origins.insert(cluster_key(p.block_name, ci), new_o);
                for (refdes, local, angle) in &geom.placements {
                    positions.insert(
                        refdes.clone(),
                        snap_point([new_o[0] + local[0], new_o[1] + local[1]]),
                    );
                    angles.insert(refdes.clone(), *angle);
                }
                let tap_sheet = [new_o[0] + tap_local[0], new_o[1] + tap_local[1]];
                joins.push((pin, tap_sheet, net.clone()));
                // Update this cluster's rect so later clusters see the new spot.
                if let Some(slot) = unit_rects.iter_mut().find(|(tag, _, _)| *tag == Some(ci)) {
                    slot.1 = new_o;
                    slot.2 = [new_o[0] + geom.envelope[0], new_o[1] + geom.envelope[1]];
                }
            }
        }

        // Advance this band's cursor past the block's envelope plus a gutter.
        band_y.insert(rank, block_y0 + p.env[1] + BLOCK_GAP_MM);

        // Guard: `analyze` classifies every component as an anchor or a cluster
        // member, so this should be empty — but never crash if a component slips
        // through. Drop it into a fallback cell below the block's envelope.
        let mut fallback_y = block_y0 + p.env[1] + BLOCK_GAP_MM;
        for refdes in block.components.keys() {
            if positions.contains_key(refdes) {
                continue;
            }
            let cell = cell_of(refdes, sizes);
            let center = [block_x0 + cell[0] / 2.0, fallback_y + cell[1] / 2.0];
            positions.insert(refdes.clone(), snap_point(center));
            angles.insert(refdes.clone(), 0.0);
            fallback_y += cell[1] + BLOCK_GAP_MM;
        }
    }

    Layout {
        positions,
        angles,
        cluster_origins,
        joins,
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::grammar::BlockGraph;
    use circuit_lang::PinType;
    use circuit_lang::model::Edge;

    /// Provider with the basics plus two many-pin "ICs" used as anchors.
    fn place_test_provider() -> circuit_lang::MockSymbolProvider {
        let mut p = circuit_lang::MockSymbolProvider::with_basics();
        p.add(
            "Mock:BIG",
            vec![
                ("A", "A", PinType::Other, 1),
                ("B", "B", PinType::Other, 1),
                ("C", "C", PinType::Other, 1),
                ("D", "D", PinType::Other, 1),
            ],
        )
        .add(
            "Mock:SMALL",
            vec![
                ("A", "A", PinType::Other, 1),
                ("B", "B", PinType::Other, 1),
                ("C", "C", PinType::Other, 1),
            ],
        );
        p
    }

    /// Build per-block cluster geometries from a design + its graphs, using the
    /// given pin-end function (shared by cluster-placement tests).
    fn build_test_geoms(
        design: &Design,
        graphs: &IndexMap<String, crate::grammar::BlockGraph>,
        pins: &dyn Fn(&str, &str) -> Option<[f64; 2]>,
    ) -> IndexMap<String, Vec<crate::cluster_geom::ClusterGeom>> {
        graphs
            .iter()
            .map(|(n, g)| {
                let gs = g
                    .clusters
                    .iter()
                    .map(|c| {
                        crate::cluster_geom::layout_cluster(
                            c,
                            &design.blocks[n],
                            &std::collections::BTreeSet::new(),
                            &pins,
                            &|_| [5.08, 10.16],
                        )
                    })
                    .collect();
                (n.clone(), gs)
            })
            .collect()
    }

    fn compile(src: &str) -> Design {
        let result = circuit_lang::compile(src, &place_test_provider());
        assert!(
            !result.diagnostics.has_errors(),
            "compile errors: {:?}",
            result.diagnostics
        );
        result.design.expect("design compiles")
    }

    /// Empty grammar (no clusters) for blocks that should pack as plain anchors.
    fn no_grammar(design: &Design) -> IndexMap<String, BlockGraph> {
        design
            .blocks
            .keys()
            .map(|n| (n.clone(), BlockGraph::default()))
            .collect()
    }

    /// Two blocks, a handful of `Device:R`/`Device:C` components each.
    fn small_two_block_design() -> Design {
        compile(
            "
version: 1
name: t
rails: [3V3, GND]
blocks:
  a:
    components:
      R1: {part: R, value: 1k, between: [N1, GND]}
      R2: {part: R, value: 2k, between: [N2, GND]}
      C1: {part: C, value: 1uF, between: [N1, GND]}
  b:
    components:
      R3: {part: R, value: 3k, between: [N3, GND]}
      R4: {part: R, value: 4k, between: [N4, GND]}
",
        )
    }

    /// Block `a` pinned left, block `b` pinned right.
    fn design_with_edge_hints() -> Design {
        let d = compile(
            "
version: 1
name: t
rails: [3V3, GND]
blocks:
  a:
    layout: {edge: left}
    components:
      R1: {part: R, value: 1k, between: [N1, GND]}
      R2: {part: R, value: 2k, between: [N2, GND]}
  b:
    layout: {edge: right}
    components:
      R3: {part: R, value: 3k, between: [N3, GND]}
      R4: {part: R, value: 4k, between: [N4, GND]}
",
        );
        assert_eq!(d.blocks["a"].layout.edge, Some(Edge::Left));
        assert_eq!(d.blocks["b"].layout.edge, Some(Edge::Right));
        d
    }

    /// Per-component bounding-box half-extent for overlap checks, in mm.
    /// Components are placed on a fixed cell grid; this is smaller than the
    /// cell pitch so that adjacent cells in a block do not "overlap".
    const COMP_HALF: f64 = 5.0;

    fn overlaps(a: [f64; 2], b: [f64; 2]) -> bool {
        (a[0] - b[0]).abs() < 2.0 * COMP_HALF && (a[1] - b[1]).abs() < 2.0 * COMP_HALF
    }

    fn no_overlaps(layout: &Layout) -> bool {
        let pts: Vec<[f64; 2]> = layout.positions.values().copied().collect();
        for i in 0..pts.len() {
            for j in (i + 1)..pts.len() {
                if overlaps(pts[i], pts[j]) {
                    return false;
                }
            }
        }
        true
    }

    fn block_centroid_x(layout: &Layout, design: &Design, block: &str) -> f64 {
        let refs: Vec<&str> = design.blocks[block]
            .components
            .keys()
            .map(|s| s.as_str())
            .collect();
        let xs: Vec<f64> = refs.iter().map(|r| layout.positions[*r][0]).collect();
        xs.iter().sum::<f64>() / xs.len() as f64
    }

    #[test]
    fn places_blocks_into_non_overlapping_regions_on_grid() {
        let design = small_two_block_design();
        let layout = place(&design, &SizeMap::new(), &no_grammar(&design), &IndexMap::new());

        // Every component has a position.
        let total: usize = design.blocks.values().map(|b| b.components.len()).sum();
        assert_eq!(layout.positions.len(), total);

        // All positions sit on the 1.27 mm grid.
        for pos in layout.positions.values() {
            assert_eq!(*pos, crate::grid::snap_point(*pos), "off-grid: {pos:?}");
        }

        // No two components overlap (bbox check with margins).
        assert!(no_overlaps(&layout), "components overlap: {:?}", layout);

        // Determinism: same design -> same layout.
        assert_eq!(
            layout,
            place(&design, &SizeMap::new(), &no_grammar(&design), &IndexMap::new())
        );
    }

    /// A cluster member lands at its cluster origin plus its local geometry
    /// offset, and carries its geometry angle.
    #[test]
    fn cluster_members_place_at_origin_plus_local_offset() {
        let design = compile(
            "
version: 1
name: t
rails: [3V3, GND]
blocks:
  a:
    components:
      C1: {part: Device:C, value: 100n, between: [3V3, GND]}
      C2: {part: Device:C, value: 100n, between: [3V3, GND]}
",
        );
        let provider = place_test_provider();
        let graphs: IndexMap<String, BlockGraph> = design
            .blocks
            .keys()
            .map(|n| (n.clone(), crate::grammar::analyze(&design, n, &provider)))
            .collect();
        let mock_pins = |_: &str, pin: &str| match pin {
            "1" => Some([0.0, -3.81]),
            "2" => Some([0.0, 3.81]),
            _ => None,
        };
        let geoms = build_test_geoms(&design, &graphs, &mock_pins);

        let layout = place(&design, &SizeMap::new(), &graphs, &geoms);
        let key = cluster_key("a", 0);
        let origin = layout.cluster_origins[&key];
        let local_c1 = geoms["a"][0]
            .placements
            .iter()
            .find(|(r, _, _)| r == "C1")
            .unwrap()
            .1;
        assert_eq!(
            layout.positions["C1"],
            crate::grid::snap_point([origin[0] + local_c1[0], origin[1] + local_c1[1]])
        );
        assert!(layout.angles.contains_key("C1"));
    }

    /// Within a packed row, units of differing height center vertically so
    /// large and small anchors share a band center.
    #[test]
    fn rows_center_units_vertically() {
        let design = compile(
            "
version: 1
name: t
rails: []
blocks:
  a:
    components:
      U1: {part: Mock:BIG, pins: {A: N1, B: N2, C: N3, D: N4}}
      U2: {part: Mock:SMALL, pins: {A: N5, B: N6, C: N7}}
",
        );
        // Tall-and-narrow cells so the ~square pack width (sqrt of total area)
        // exceeds the two widths combined and both land in one row, where the
        // shorter unit must center against the taller one.
        let mut sizes = SizeMap::new();
        sizes.insert("U1".into(), [10.0, 120.0]);
        sizes.insert("U2".into(), [10.0, 10.0]);
        // Both are many-pin anchors; analyze lists them so they pack into a row
        // (an empty grammar would route them through the fallback stack instead).
        let graphs: IndexMap<String, BlockGraph> = design
            .blocks
            .keys()
            .map(|n| (n.clone(), crate::grammar::analyze(&design, n, &place_test_provider())))
            .collect();
        let layout = place(&design, &sizes, &graphs, &IndexMap::new());
        let (u1, u2) = (layout.positions["U1"], layout.positions["U2"]);
        assert!((u1[1] - u2[1]).abs() < 5.1, "u1={u1:?} u2={u2:?}");
    }

    #[test]
    fn edge_hints_push_blocks_to_sheet_sides() {
        let design = design_with_edge_hints();
        let layout = place(&design, &SizeMap::new(), &no_grammar(&design), &IndexMap::new());
        let ax = block_centroid_x(&layout, &design, "a");
        let bx = block_centroid_x(&layout, &design, "b");
        assert!(
            ax < bx,
            "left-edge block (x={ax}) must be left of right-edge block (x={bx})"
        );
    }

    /// East-tap cluster lands right of the IC with a straight join; the 9V
    /// decouple bank (tap-less, V+ touching) lands above the anchor row.
    #[test]
    fn anchor_centric_block_slots_clusters_by_side() {
        let design = compile(
            "
version: 1
name: t
rails: [9V, GND]
blocks:
  a:
    components:
      U1: {part: Mock:BIG, pins: {A: CC1, B: N2, C: N3, D: N4}}
      R1: {part: Device:R, value: 5k1, between: [CC1, GND]}
      C5: {part: Device:C, value: 100n, between: [9V, GND]}
      C6: {part: Device:C, value: 100n, between: [9V, GND]}
",
        );
        let provider = place_test_provider();
        let graphs: IndexMap<String, BlockGraph> = design
            .blocks
            .keys()
            .map(|n| (n.clone(), crate::grammar::analyze(&design, n, &provider)))
            .collect();
        let mock_pins = |_: &str, pin: &str| match pin {
            "1" => Some([0.0, -3.81]),
            "2" => Some([0.0, 3.81]),
            _ => None,
        };
        let geoms = build_test_geoms(&design, &graphs, &mock_pins);
        let mut pin_ends = AnchorPinEnds::new();
        pin_ends.insert(("U1".into(), "A".into()), ([10.16, 0.0], Dir::East));

        let layout = place_with_anchor_pins(&design, &SizeMap::new(), &graphs, &geoms, &pin_ends);
        let u1 = layout.positions["U1"];
        // R-divider cluster East of U1 with a straight join on CC1.
        let join = layout
            .joins
            .iter()
            .find(|(_, _, n)| n == "CC1")
            .expect("join recorded");
        assert_eq!(join.0[1], join.1[1], "straight horizontal join");
        assert!(layout.positions["R1"][0] > u1[0], "R1 east of U1");
        // A pure decouple bank always takes the bottom row (reference style),
        // even though it touches the 9V rail.
        assert!(
            layout.positions["C5"][1] > u1[1],
            "bank below the anchor: C5={:?} U1={:?}",
            layout.positions["C5"],
            u1
        );
        // Bank members pack intentionally tight (BANK_PITCH), so the blanket
        // no_overlaps helper does not apply; assert the ANCHOR keeps clear of
        // every cluster member instead.
        for (r, pos) in &layout.positions {
            if r == "U1" {
                continue;
            }
            assert!(
                (pos[0] - u1[0]).abs() >= 10.0 || (pos[1] - u1[1]).abs() >= 10.0,
                "{r} overlaps U1: {pos:?} vs {u1:?}"
            );
        }
    }

    #[test]
    fn cluster_side_follows_primary_tap_direction() {
        let mut pin_ends = AnchorPinEnds::new();
        pin_ends.insert(("U1".into(), "A".into()), ([10.16, 0.0], Dir::East));
        pin_ends.insert(("U1".into(), "B".into()), ([-10.16, 0.0], Dir::West));
        let taps_e = vec![("N1".to_string(), "U1".to_string(), "A".to_string())];
        let taps_w = vec![("N2".to_string(), "U1".to_string(), "B".to_string())];
        assert_eq!(
            cluster_side(&taps_e, &pin_ends, &[], &|_| false, &|_| false),
            Side::East
        );
        assert_eq!(
            cluster_side(&taps_w, &pin_ends, &[], &|_| false, &|_| false),
            Side::West
        );
    }

    #[test]
    fn tapless_cluster_side_follows_rail_polarity() {
        // V+ wins when both rails are present: the cluster hangs from the
        // supply; its ground end points down anyway.
        let is_vplus = |n: &str| n == "9V";
        let is_gnd = |n: &str| n == "GND";
        assert_eq!(
            side_of_tapless(&["9V".into(), "N1".into()], &is_vplus, &is_gnd),
            Side::North
        );
        assert_eq!(
            side_of_tapless(&["N1".into(), "GND".into()], &is_vplus, &is_gnd),
            Side::South
        );
    }

    #[test]
    fn single_tap_cluster_slots_beside_its_anchor_pin() {
        let design = compile(
            "
version: 1
name: t
rails: [VCC, GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 5k1, between: [CC1, GND]}
      U1: {part: Mock:BIG, pins: {A: CC1, B: N2, C: N3, D: N4}}
",
        );
        let provider = place_test_provider();
        let graphs: IndexMap<String, crate::grammar::BlockGraph> = design
            .blocks
            .keys()
            .map(|n| (n.clone(), crate::grammar::analyze(&design, n, &provider)))
            .collect();
        assert_eq!(graphs["a"].clusters[0].anchor_taps.len(), 1);

        let mock_pins = |_: &str, pin: &str| match pin {
            "1" => Some([0.0, -3.81]),
            "2" => Some([0.0, 3.81]),
            _ => None,
        };
        let geoms = build_test_geoms(&design, &graphs, &mock_pins);
        // U1's pin A points East from its right side, 10mm from origin.
        let mut pin_ends = AnchorPinEnds::new();
        pin_ends.insert(
            ("U1".to_string(), "A".to_string()),
            ([10.16, 0.0], crate::emit::Dir::East),
        );

        let layout = place_with_anchor_pins(&design, &SizeMap::new(), &graphs, &geoms, &pin_ends);
        let u1 = layout.positions["U1"];
        let pin = [u1[0] + 10.16, u1[1]];
        let join = layout.joins.iter().find(|(_, _, n)| n == "CC1").expect("join wire recorded");
        // Compare with tolerance: the engine snaps the recorded endpoint, the
        // test's `u1 + 10.16` carries float dust.
        assert!(
            (join.0[0] - pin[0]).abs() < 1e-6 && (join.0[1] - pin[1]).abs() < 1e-6,
            "join starts at the pin endpoint: {:?} vs {pin:?}",
            join.0
        );
        assert_eq!(join.0[1], join.1[1], "straight horizontal join");
        assert!(join.1[0] > pin[0], "cluster sits East of the pin");
    }
}
