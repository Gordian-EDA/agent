//! Deterministic block placement.
//!
//! Turns a [`circuit_lang::Design`] into per-component sheet positions. This is
//! pure geometry — no I/O, no KiCAD. The MVP bar (spec §8) is *tidy,
//! grid-aligned, ERC-clean* rather than beautiful: each block occupies a
//! rectangular region, components fall into a fixed-cell grid inside it, and
//! edge hints push regions toward sheet sides. Same `Design` -> same `Layout`.

use circuit_lang::Design;
use circuit_lang::model::{Block, Edge, Origin, RefDes};
use indexmap::IndexMap;

use crate::grid::snap_point;

/// Computed positions for every component, keyed by refdes.
///
/// Coordinates are in KiCAD sheet millimetres (x grows right, y grows *down*)
/// and every value lands on the 1.27 mm grid via [`crate::grid::snap_point`].
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub positions: IndexMap<RefDes, [f64; 2]>,
}

/// Fixed grid-cell pitch for a component, in mm. Generous — sheet space is free
/// and a wide pitch keeps symbols, their labels, and decouple caps clear of each
/// other so ERC stays clean.
const CELL_MM: f64 = 25.4;

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

/// Grid dimensions (cols, rows) for `n` cells: ~square, columns first.
fn grid_dims(n: usize) -> (usize, usize) {
    if n == 0 {
        return (1, 1);
    }
    let cols = (n as f64).sqrt().ceil() as usize;
    let cols = cols.max(1);
    let rows = n.div_ceil(cols);
    (cols, rows)
}

/// A physically-grouped set of cells laid out together: an authored component
/// (the parent, always first) followed by its synthesized children (decouple
/// caps). Every other component is a singleton cluster of length 1. Keeping a
/// parent and its caps in one cluster lets the layout place them in a compact
/// sub-grid so caps never wrap onto a far row.
type Cluster = Vec<RefDes>;

/// Deterministic clusters for a block's components.
///
/// Each authored (non-synthesized) component becomes a cluster led by that
/// component, followed by its decouple caps in `(role, index)` order. Clusters
/// are emitted in natural refdes order of their parent, so the same `Design`
/// always yields the same cluster sequence. Synthesized parts whose parent
/// lives in another block (rare) trail at the end as singletons.
fn block_clusters(block: &Block) -> Vec<Cluster> {
    // Authored (and any non-synthesized) refdes in natural order.
    let mut roots: Vec<RefDes> = block
        .components
        .iter()
        .filter(|(_, c)| !matches!(c.origin, Origin::Synthesized { .. }))
        .map(|(r, _)| r.clone())
        .collect();
    roots.sort_by(|a, b| {
        if circuit_lang::canon::natural_lt(a, b) {
            std::cmp::Ordering::Less
        } else if circuit_lang::canon::natural_lt(b, a) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });

    // Children grouped by parent, ordered by (role, index) for determinism.
    let mut children: IndexMap<RefDes, Vec<(String, u32, RefDes)>> = IndexMap::new();
    let mut orphans: Vec<(String, u32, RefDes)> = Vec::new();
    for (refdes, comp) in &block.components {
        if let Origin::Synthesized {
            parent,
            role,
            index,
        } = &comp.origin
        {
            let entry = (role.clone(), *index, refdes.clone());
            if block.components.contains_key(parent) {
                children.entry(parent.clone()).or_default().push(entry);
            } else {
                orphans.push(entry);
            }
        }
    }
    for v in children.values_mut() {
        v.sort();
    }
    orphans.sort();

    let mut clusters: Vec<Cluster> = Vec::with_capacity(roots.len());
    for root in &roots {
        let mut cluster: Cluster = vec![root.clone()];
        if let Some(kids) = children.get(root) {
            cluster.extend(kids.iter().map(|(_, _, r)| r.clone()));
        }
        clusters.push(cluster);
    }
    // Synthesized parts whose parent lives in another block trail as singletons.
    clusters.extend(orphans.into_iter().map(|(_, _, r)| vec![r]));
    clusters
}

/// Lay a block's clusters into grid cells.
///
/// Returns the `(refdes, col, row)` cell for every component plus the block's
/// total `(cols, rows)` footprint. Singletons pack left-to-right into rows of
/// width `block_width`. A multi-cell cluster never straddles that packing: it
/// starts at column 0 of a fresh row and fills its own compact
/// `ceil(sqrt(len))`-wide sub-grid anchored at the parent, so each decouple cap
/// stays within a few cells of its parent. Deterministic: cluster order and
/// cell assignment depend only on the block.
fn layout_block_cells(block: &Block) -> (Vec<(RefDes, usize, usize)>, usize, usize) {
    let clusters = block_clusters(block);
    let total: usize = clusters.iter().map(|c| c.len()).sum();
    // Overall block packing width: ~square in the total cell count, and at least
    // wide enough to hold the widest cluster's sub-grid so nothing overflows.
    let widest_cluster = clusters
        .iter()
        .map(|c| sub_cols(c.len()))
        .max()
        .unwrap_or(1);
    let block_width = grid_dims(total).0.max(widest_cluster).max(1);

    let mut cells: Vec<(RefDes, usize, usize)> = Vec::with_capacity(total);
    let mut row = 0usize; // current packing row
    let mut col = 0usize; // next free column in the current packing row
    let mut max_rows = 0usize;

    for cluster in &clusters {
        if cluster.len() == 1 {
            // Singleton: pack into the current row, wrapping at block_width.
            if col >= block_width {
                row += 1;
                col = 0;
            }
            cells.push((cluster[0].clone(), col, row));
            max_rows = max_rows.max(row + 1);
            col += 1;
        } else {
            // Multi-cell cluster: drop to a fresh row so the parent leads its own
            // compact sub-grid and its caps cannot wrap away across the block.
            if col != 0 {
                row += 1;
                col = 0;
            }
            let cw = sub_cols(cluster.len());
            for (k, refdes) in cluster.iter().enumerate() {
                let sub_col = k % cw;
                let sub_row = k / cw;
                cells.push((refdes.clone(), sub_col, row + sub_row));
            }
            let ch = cluster.len().div_ceil(cw);
            max_rows = max_rows.max(row + ch);
            row += ch; // next cluster starts below this sub-grid
            // `col` is already 0 here (reset above on entry / fresh row).
        }
    }

    (cells, block_width.max(1), max_rows)
}

/// Sub-grid column count for a cluster of `len` cells: ~square, columns first.
/// A cluster laid out `ceil(sqrt(len))` columns wide keeps its farthest cap
/// within `ceil(sqrt(len)) - 1` cells of the parent on each axis.
fn sub_cols(len: usize) -> usize {
    if len <= 1 {
        return 1;
    }
    ((len as f64).sqrt().ceil() as usize).max(1)
}

/// Compute deterministic positions for every component in `design`.
///
/// Blocks are grouped into vertical bands by edge hint (Left band leftmost,
/// Right band rightmost); within a band each block stacks downward and occupies
/// a rectangular region; within a region components fill a ~square grid of
/// fixed-pitch, grid-snapped cells. Same `Design` -> same `Layout`.
pub fn place(design: &Design) -> Layout {
    // Stable block order: by band rank, then original `Design` order.
    let mut blocks: Vec<(&str, &Block)> =
        design.blocks.iter().map(|(n, b)| (n.as_str(), b)).collect();
    blocks.sort_by_key(|(_, b)| band_rank(b.layout.edge));
    // `sort_by_key` is stable, so equal-rank blocks keep `Design` order.

    // x-base for each band: bands laid left-to-right in ascending rank. Each
    // band is as wide as its widest block's grid; we accumulate widths so bands
    // never overlap horizontally.
    let mut band_x: IndexMap<u8, f64> = IndexMap::new();
    let mut cursor_x = MARGIN_MM;
    let mut ranks: Vec<u8> = blocks
        .iter()
        .map(|(_, b)| band_rank(b.layout.edge))
        .collect();
    ranks.sort_unstable();
    ranks.dedup();
    for rank in &ranks {
        // Widest block in this band determines the band's column count. Use the
        // cluster layout's footprint so band widths match actual placement.
        let max_cols = blocks
            .iter()
            .filter(|(_, b)| band_rank(b.layout.edge) == *rank)
            .map(|(_, b)| layout_block_cells(b).1)
            .max()
            .unwrap_or(1);
        band_x.insert(*rank, cursor_x);
        cursor_x += max_cols as f64 * CELL_MM + BAND_GAP_MM;
    }

    let mut positions: IndexMap<RefDes, [f64; 2]> = IndexMap::new();
    // Independent vertical cursor per band so stacked blocks don't collide.
    let mut band_y: IndexMap<u8, f64> = IndexMap::new();

    for (_name, block) in &blocks {
        let rank = band_rank(block.layout.edge);
        let x0 = band_x[&rank];
        let y0 = *band_y.entry(rank).or_insert(MARGIN_MM);

        let (cells, _cols, rows) = layout_block_cells(block);
        for (refdes, col, row) in &cells {
            let p = snap_point([x0 + *col as f64 * CELL_MM, y0 + *row as f64 * CELL_MM]);
            positions.insert(refdes.clone(), p);
        }

        // Advance this band's cursor past the block's region plus a gutter.
        let used_rows = if cells.is_empty() { 0 } else { rows };
        band_y.insert(rank, y0 + used_rows as f64 * CELL_MM + BLOCK_GAP_MM);
    }

    Layout { positions }
}

#[cfg(test)]
mod tests {
    use super::*;
    use circuit_lang::model::Edge;

    fn compile(src: &str) -> Design {
        let provider = circuit_lang::MockSymbolProvider::with_basics();
        let result = circuit_lang::compile(src, &provider);
        assert!(
            !result.diagnostics.has_errors(),
            "compile errors: {:?}",
            result.diagnostics
        );
        result.design.expect("design compiles")
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
        let layout = place(&design);

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
        assert_eq!(layout, place(&design));
    }

    #[test]
    fn synthesized_decouple_caps_sit_adjacent_to_parent() {
        use circuit_lang::{PinType, SymbolProvider};

        // A part with explicit power pins so `decouple:` desugars into caps.
        let mut provider = circuit_lang::MockSymbolProvider::new();
        provider
            .add(
                "Device:R",
                vec![
                    ("1", "~", PinType::Passive, 1),
                    ("2", "~", PinType::Passive, 1),
                ],
            )
            .add(
                "Device:C",
                vec![
                    ("1", "~", PinType::Passive, 1),
                    ("2", "~", PinType::Passive, 1),
                ],
            )
            .add(
                "MCU:M",
                vec![
                    ("1", "VDD", PinType::PowerInput, 1),
                    ("2", "VSS", PinType::PowerInput, 1),
                    ("3", "IO", PinType::Passive, 1),
                ],
            );
        assert!(provider.symbol("MCU:M").is_some());

        let src = "
version: 1
name: t
rails: [3V3, GND]
blocks:
  mcu:
    components:
      U1: {part: MCU:M, decouple: {100nF: 2}, pins: {VDD: 3V3, VSS: GND, IO: SIG}}
";
        let result = circuit_lang::compile(src, &provider);
        assert!(!result.diagnostics.has_errors(), "{:?}", result.diagnostics);
        let design = result.design.expect("compiles");

        // Two synthesized decouple caps exist alongside U1.
        let mcu = &design.blocks["mcu"];
        let cap_keys: Vec<&str> = mcu
            .components
            .iter()
            .filter(|(_, c)| matches!(c.origin, circuit_lang::model::Origin::Synthesized { .. }))
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(cap_keys.len(), 2, "expected 2 decouple caps");

        let layout = place(&design);
        let u1 = layout.positions["U1"];
        // Each cap must sit within the compact-cluster bound of its parent.
        let bound = cluster_bound(1 + cap_keys.len());
        for cap in cap_keys {
            let p = layout.positions[cap];
            let d = ((p[0] - u1[0]).powi(2) + (p[1] - u1[1]).powi(2)).sqrt();
            assert!(
                d <= bound,
                "decouple cap {cap} at {p:?} not adjacent to U1 at {u1:?} (dist {d})"
            );
        }
    }

    /// Compact-cluster adjacency bound for a cluster of `cluster_len` cells
    /// (parent + caps). The cluster is laid out in a `ceil(sqrt(cluster_len))`
    /// wide sub-grid anchored at the parent, so the farthest cap is at most
    /// `cw - 1` cells away on each axis; its Euclidean distance from the parent
    /// is therefore at most `CELL_MM * sqrt(2) * (cw - 1)`. A small grid-snap
    /// tolerance covers the 1.27 mm rounding. This is far tighter than a full
    /// wide row (where a wrapped cap lands 50+ mm away).
    fn cluster_bound(cluster_len: usize) -> f64 {
        let cw = (cluster_len as f64).sqrt().ceil();
        CELL_MM * std::f64::consts::SQRT_2 * (cw - 1.0) + 2.0 * crate::grid::GRID_MM
    }

    /// Reviewer's pathology: a block of singleton components (R1..R5) followed
    /// by a decoupled part (U6 with three 100 nF caps). Under the old row-major
    /// layout U6 lands at the end of a row and its caps wrap onto the next row,
    /// landing 50+ mm away. The cluster layout must keep every cap inside U6's
    /// compact sub-grid, within the cluster adjacency bound.
    #[test]
    fn decouple_caps_stay_adjacent_when_parent_at_row_boundary() {
        use circuit_lang::{PinType, SymbolProvider};

        let mut provider = circuit_lang::MockSymbolProvider::new();
        provider
            .add(
                "Device:R",
                vec![
                    ("1", "~", PinType::Passive, 1),
                    ("2", "~", PinType::Passive, 1),
                ],
            )
            .add(
                "Device:C",
                vec![
                    ("1", "~", PinType::Passive, 1),
                    ("2", "~", PinType::Passive, 1),
                ],
            )
            .add(
                "MCU:M",
                vec![
                    ("1", "VDD", PinType::PowerInput, 1),
                    ("2", "VSS", PinType::PowerInput, 1),
                    ("3", "IO", PinType::Passive, 1),
                ],
            );
        assert!(provider.symbol("MCU:M").is_some());

        // Five singletons then a decoupled part: 5 + 1 + 3 = 9 cells. Under the
        // old grid (cols = ceil(sqrt(9)) = 3), U6 would land at col 2 (row end)
        // and its caps wrap to the next row.
        let src = "
version: 1
name: t
rails: [3V3, GND]
blocks:
  blk:
    components:
      R1: {part: R, value: 1k, between: [N1, GND]}
      R2: {part: R, value: 2k, between: [N2, GND]}
      R3: {part: R, value: 3k, between: [N3, GND]}
      R4: {part: R, value: 4k, between: [N4, GND]}
      R5: {part: R, value: 5k, between: [N5, GND]}
      U6: {part: MCU:M, decouple: {100nF: 3}, pins: {VDD: 3V3, VSS: GND, IO: SIG}}
";
        let result = circuit_lang::compile(src, &provider);
        assert!(!result.diagnostics.has_errors(), "{:?}", result.diagnostics);
        let design = result.design.expect("compiles");

        let blk = &design.blocks["blk"];
        let cap_keys: Vec<&str> = blk
            .components
            .iter()
            .filter(|(_, c)| {
                matches!(
                    &c.origin,
                    circuit_lang::model::Origin::Synthesized { role, parent, .. }
                        if role == "decouple" && parent == "U6"
                )
            })
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(cap_keys.len(), 3, "expected 3 decouple caps for U6");

        // Cluster = U6 + its 3 caps.
        let cluster_len = 1 + cap_keys.len();
        let bound = cluster_bound(cluster_len);

        let layout = place(&design);
        let u6 = layout.positions["U6"];
        for cap in &cap_keys {
            let p = layout.positions[*cap];
            let d = ((p[0] - u6[0]).powi(2) + (p[1] - u6[1]).powi(2)).sqrt();
            assert!(
                d <= bound,
                "decouple cap {cap} at {p:?} is {d:.1} mm from U6 at {u6:?}, \
                 exceeds compact-cluster bound {bound:.1} mm"
            );
        }

        // Sanity: the bound genuinely excludes the old row-wrap distance, which
        // separated a cap from its parent by well over 50 mm.
        assert!(
            bound < 50.0,
            "adjacency bound {bound:.1} mm must be tight enough to catch row-wrap"
        );

        // No overlaps and determinism still hold for clustered layout.
        assert!(no_overlaps(&layout), "components overlap: {layout:?}");
        assert_eq!(layout, place(&design));
    }

    #[test]
    fn edge_hints_push_blocks_to_sheet_sides() {
        let design = design_with_edge_hints();
        let layout = place(&design);
        let ax = block_centroid_x(&layout, &design, "a");
        let bx = block_centroid_x(&layout, &design, "b");
        assert!(
            ax < bx,
            "left-edge block (x={ax}) must be left of right-edge block (x={bx})"
        );
    }
}
