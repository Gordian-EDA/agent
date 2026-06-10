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

/// One cluster laid out: per-refdes offsets (cell CENTERS) relative to the
/// cluster's top-left corner, plus the cluster envelope [w, h]. A multi-cell
/// cluster (parent + decouple caps) places the parent's cell, then a single
/// row of cap cells to its right, top-aligned (the decoupling bank).
fn layout_cluster(cluster: &Cluster, sizes: &SizeMap) -> (Vec<(RefDes, [f64; 2])>, [f64; 2]) {
    let parent_cell = cell_of(&cluster[0], sizes);
    let mut offsets = vec![(
        cluster[0].clone(),
        [parent_cell[0] / 2.0, parent_cell[1] / 2.0],
    )];
    let mut x = parent_cell[0];
    let mut h = parent_cell[1];
    for child in &cluster[1..] {
        let c = cell_of(child, sizes);
        offsets.push((child.clone(), [x + c[0] / 2.0, c[1] / 2.0]));
        x += c[0];
        h = h.max(c[1]);
    }
    (offsets, [x, h])
}

/// Lay a block's clusters into rows of envelopes, wrapping at a ~square target
/// width. Returns per-refdes positions relative to the block origin plus the
/// block envelope [w, h].
fn layout_block(block: &Block, sizes: &SizeMap) -> (Vec<(RefDes, [f64; 2])>, [f64; 2]) {
    let clusters = block_clusters(block);
    let laid: Vec<_> = clusters.iter().map(|c| layout_cluster(c, sizes)).collect();
    let total_area: f64 = laid.iter().map(|(_, e)| e[0] * e[1]).sum();
    let target_w = total_area
        .sqrt()
        .max(laid.iter().map(|(_, e)| e[0]).fold(0.0, f64::max));

    let mut out = Vec::new();
    let (mut x, mut y, mut row_h, mut max_w) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for (offsets, env) in &laid {
        if x > 0.0 && x + env[0] > target_w {
            y += row_h;
            x = 0.0;
            row_h = 0.0;
        }
        for (refdes, off) in offsets {
            out.push((refdes.clone(), [x + off[0], y + off[1]]));
        }
        x += env[0];
        row_h = row_h.max(env[1]);
        max_w = max_w.max(x);
    }
    (out, [max_w, y + row_h])
}

/// Compute deterministic positions for every component in `design`.
///
/// Blocks are grouped into vertical bands by edge hint (Left band leftmost,
/// Right band rightmost); within a band each block stacks downward and occupies
/// a rectangular region; within a region components fill a ~square grid of
/// fixed-pitch, grid-snapped cells. Same `Design` -> same `Layout`.
pub fn place(design: &Design, sizes: &SizeMap) -> Layout {
    // Stable block order: by band rank, then original `Design` order.
    let mut blocks: Vec<(&str, &Block)> =
        design.blocks.iter().map(|(n, b)| (n.as_str(), b)).collect();
    blocks.sort_by_key(|(_, b)| band_rank(b.layout.edge));
    // `sort_by_key` is stable, so equal-rank blocks keep `Design` order.

    // Pre-compute each block's relative layout + envelope once; reuse below for
    // both band-width accumulation and final placement.
    let laid: Vec<(Vec<(RefDes, [f64; 2])>, [f64; 2])> = blocks
        .iter()
        .map(|(_, b)| layout_block(b, sizes))
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
            .zip(laid.iter())
            .filter(|((_, b), _)| band_rank(b.layout.edge) == *rank)
            .map(|(_, (_, env))| env[0])
            .fold(0.0_f64, f64::max);
        band_x.insert(*rank, cursor_x);
        cursor_x += max_w + BAND_GAP_MM;
    }

    let mut positions: IndexMap<RefDes, [f64; 2]> = IndexMap::new();
    // Independent vertical cursor per band so stacked blocks don't collide.
    let mut band_y: IndexMap<u8, f64> = IndexMap::new();

    for ((_name, block), (rels, env)) in blocks.iter().zip(laid.iter()) {
        let rank = band_rank(block.layout.edge);
        let x0 = band_x[&rank];
        let y0 = *band_y.entry(rank).or_insert(MARGIN_MM);

        for (refdes, rel) in rels {
            let p = snap_point([x0 + rel[0], y0 + rel[1]]);
            positions.insert(refdes.clone(), p);
        }

        // Advance this band's cursor past the block's envelope plus a gutter.
        band_y.insert(rank, y0 + env[1] + BLOCK_GAP_MM);
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
        let layout = place(&design, &SizeMap::new());

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
        assert_eq!(layout, place(&design, &SizeMap::new()));
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

        let layout = place(&design, &SizeMap::new());
        let u1 = layout.positions["U1"];
        // With the decoupling-bank layout the caps form a single row to the RIGHT
        // of their parent: every cap sits at a larger x and within a reasonable
        // horizontal span of the parent (no wrapping onto a far row).
        let span = (1 + cap_keys.len()) as f64 * CELL_MM;
        for cap in &cap_keys {
            let p = layout.positions[*cap];
            assert!(
                p[0] > u1[0],
                "decouple cap {cap} at {p:?} must sit right of U1 at {u1:?}"
            );
            assert!(
                (p[0] - u1[0]) <= span,
                "decouple cap {cap} at {p:?} too far right of U1 at {u1:?} (span {span})"
            );
        }
        // All caps share one row (same y as each other).
        let cap_ys: Vec<f64> = cap_keys.iter().map(|c| layout.positions[*c][1]).collect();
        assert!(
            cap_ys.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-6),
            "decouple caps must share one row: {cap_ys:?}"
        );
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

        // Cluster = U6 + its 3 caps. The decoupling bank places caps in one row
        // immediately right of U6, so the farthest cap is at most the bank's
        // total cell width away — never wrapped onto a distant row.
        let bank_span = (1 + cap_keys.len()) as f64 * CELL_MM;

        let layout = place(&design, &SizeMap::new());
        let u6 = layout.positions["U6"];
        let cap_ys: Vec<f64> = cap_keys.iter().map(|c| layout.positions[*c][1]).collect();
        for cap in &cap_keys {
            let p = layout.positions[*cap];
            assert!(
                p[0] > u6[0],
                "decouple cap {cap} at {p:?} must sit right of U6 at {u6:?}"
            );
            assert!(
                (p[0] - u6[0]) <= bank_span,
                "decouple cap {cap} at {p:?} is {:.1} mm right of U6 at {u6:?}, \
                 exceeds bank span {bank_span:.1} mm",
                p[0] - u6[0]
            );
        }
        // Caps share one row (no row-wrap). The bank span is far tighter than the
        // old row-major layout, where a wrapped cap landed 50+ mm away.
        assert!(
            cap_ys.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-6),
            "decouple caps must share one bank row: {cap_ys:?}"
        );
        assert!(
            bank_span < 5.0 * CELL_MM,
            "bank span {bank_span:.1} mm must stay compact"
        );

        // No overlaps and determinism still hold for the bank layout.
        assert!(no_overlaps(&layout), "components overlap: {layout:?}");
        assert_eq!(layout, place(&design, &SizeMap::new()));
    }

    /// The decoupling-bank invariant in isolation: a part's synthesized caps form
    /// a single horizontal row immediately to the right of their parent.
    #[test]
    fn decouple_caps_form_a_row_beside_parent() {
        use circuit_lang::{PinType, SymbolProvider};

        let mut provider = circuit_lang::MockSymbolProvider::new();
        provider
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

        let cap_keys: Vec<&str> = design.blocks["mcu"]
            .components
            .iter()
            .filter(|(_, c)| matches!(c.origin, circuit_lang::model::Origin::Synthesized { .. }))
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(cap_keys.len(), 2, "expected 2 decouple caps");

        let layout = place(&design, &SizeMap::new());
        let u1 = layout.positions["U1"];
        let caps: Vec<[f64; 2]> = cap_keys.iter().map(|c| layout.positions[*c]).collect();
        for p in &caps {
            assert!(
                p[0] > u1[0],
                "caps sit to the right of the parent: {p:?} vs {u1:?}"
            );
        }
        // All caps share one row (same y).
        assert!(caps.windows(2).all(|w| (w[0][1] - w[1][1]).abs() < 1e-6));
    }

    #[test]
    fn edge_hints_push_blocks_to_sheet_sides() {
        let design = design_with_edge_hints();
        let layout = place(&design, &SizeMap::new());
        let ax = block_centroid_x(&layout, &design, "a");
        let bx = block_centroid_x(&layout, &design, "b");
        assert!(
            ax < bx,
            "left-edge block (x={ax}) must be left of right-edge block (x={bx})"
        );
    }
}
