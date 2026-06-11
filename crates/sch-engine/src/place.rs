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
    // Stable block order: by band rank, then original `Design` order.
    let mut blocks: Vec<(&str, &Block)> =
        design.blocks.iter().map(|(n, b)| (n.as_str(), b)).collect();
    blocks.sort_by_key(|(_, b)| band_rank(b.layout.edge));
    // `sort_by_key` is stable, so equal-rank blocks keep `Design` order.

    let empty_graph = BlockGraph::default();

    // Pre-compute each block's unit order, packed unit origins, and envelope
    // once; reuse below for both band-width accumulation and final placement.
    struct Packed<'a> {
        units: Vec<Unit>,
        extents: Vec<[f64; 2]>,
        origins: Vec<[f64; 2]>,
        env: [f64; 2],
        block_name: &'a str,
    }
    let packed: Vec<Packed> = blocks
        .iter()
        .map(|(name, block)| {
            let graph = graphs.get(*name).unwrap_or(&empty_graph);
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
            Packed { units, extents, origins, env, block_name: name }
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
            .map(|(_, p)| p.env[0])
            .fold(0.0_f64, f64::max);
        band_x.insert(*rank, cursor_x);
        cursor_x += max_w + BAND_GAP_MM;
    }

    let mut positions: IndexMap<RefDes, [f64; 2]> = IndexMap::new();
    let mut angles: IndexMap<RefDes, f64> = IndexMap::new();
    let mut cluster_origins: IndexMap<String, [f64; 2]> = IndexMap::new();
    // Independent vertical cursor per band so stacked blocks don't collide.
    let mut band_y: IndexMap<u8, f64> = IndexMap::new();

    for ((_name, block), p) in blocks.iter().zip(packed.iter()) {
        let rank = band_rank(block.layout.edge);
        let block_x0 = band_x[&rank];
        let block_y0 = *band_y.entry(rank).or_insert(MARGIN_MM);

        for (i, unit) in p.units.iter().enumerate() {
            let uo = p.origins[i];
            match unit {
                Unit::Anchor(refdes) => {
                    let ext = p.extents[i];
                    let center = [
                        block_x0 + uo[0] + ext[0] / 2.0,
                        block_y0 + uo[1] + ext[1] / 2.0,
                    ];
                    positions.insert(refdes.clone(), snap_point(center));
                    angles.insert(refdes.clone(), 0.0);
                }
                Unit::Cluster(ci) => {
                    let o = snap_point([block_x0 + uo[0], block_y0 + uo[1]]);
                    cluster_origins.insert(cluster_key(p.block_name, *ci), o);
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
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster_geom::ClusterGeom;
    use crate::grammar::BlockGraph;
    use circuit_lang::PinType;
    use circuit_lang::model::Edge;

    /// Provider with the basics plus two many-pin "ICs" used as anchors.
    fn provider() -> circuit_lang::MockSymbolProvider {
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

    fn compile(src: &str) -> Design {
        let result = circuit_lang::compile(src, &provider());
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
        let provider = provider();
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
        let geoms: IndexMap<String, Vec<ClusterGeom>> = graphs
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
                            &mock_pins,
                            &|_| [5.08, 10.16],
                        )
                    })
                    .collect();
                (n.clone(), gs)
            })
            .collect();

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
            .map(|n| (n.clone(), crate::grammar::analyze(&design, n, &provider())))
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
}
