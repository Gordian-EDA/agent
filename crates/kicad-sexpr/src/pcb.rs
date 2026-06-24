//! Reading a `.kicad_pcb` board into a [`pcb_model::RouteProblem`].
//!
//! Backend: `kiutils_kicad`'s [`kiutils_kicad::PcbFile::read`], whose typed AST
//! exposes `layers / nets / footprints / segments / vias / zones / graphics /
//! setup`. We translate that board into the SimpleRouteJson-shaped routing
//! problem the engine consumes, and record the net-code and layer-name mapping
//! the write-back side (next task) needs to emit copper back onto the board.
//!
//! ## Coordinate & rotation conventions (verified against KiCAD 9.0.9)
//!
//! KiCAD PCB space is **y-down** and file angles are **counter-clockwise
//! positive**. A footprint has a position (`at`) and an optional rotation; its
//! pads carry an `at` offset expressed in the footprint's **unrotated** frame.
//! The pad's own stored `rotation` is the *total* rotation (footprint angle
//! already folded in) and matters only for the pad's shape orientation — to
//! place the pad *center* we rotate the pad offset by the **footprint** angle
//! and translate by the footprint position.
//!
//! Rotating an offset `(dx, dy)` by a CCW angle θ in a y-down world gives
//!
//! ```text
//! x' =  dx·cosθ + dy·sinθ
//! y' = -dx·sinθ + dy·cosθ
//! ```
//!
//! (the y-down sign flip versus the textbook y-up matrix). Sanity check at
//! θ = 90°: `cosθ = 0, sinθ = 1`, so `(dx, dy) → (dy, -dx)`. A pad offset of
//! `(-0.9125, 0)` rotates to `(0, 0.9125)` — i.e. it moves to +y (downward on
//! screen), which matches KiCAD rotating a horizontal 0805 onto its side.
//!
//! ## v1 conservatism (documented simplifications)
//!
//! - Pad obstacles use the **axis-aligned bounding box** of the rotated pad
//!   rectangle, never an oriented rect — always ≥ the true footprint.
//! - Segments inflate to the bounding box of their width-expanded extent.
//! - Vias become a square bbox of side `size`.
//! - Zones are skipped unless they are keepouts; a keepout becomes a full-board
//!   obstacle. (Copper-fill zones do not constrain a fresh route in v1.)
//! - Design rules come from `RouteProblem` defaults: `PcbSetup` in this
//!   kiutils version only surfaces mask/paste clearances, not copper clearance
//!   or track width, so there is nothing board-specific to read.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::Path;

use kiutils_kicad::{PcbAst, PcbFile, PcbFootprint, PcbPad};
use pcb_model::{
    Bounds, Connection, LayerRef, Obstacle, Point2, RoutePoint, RouteProblem, RouteSolution, Trace,
    Via, ViaSpan,
};

/// A board parsed into a routing problem plus the mappings write-back needs.
#[derive(Debug, Clone)]
pub struct BoardProblem {
    /// The routing problem (SimpleRouteJson-shaped) extracted from the board.
    pub problem: RouteProblem,
    /// Connection name (KiCAD net name) → KiCAD net code, for emitting copper
    /// back onto the board with the correct `(net N)` references.
    pub net_codes: BTreeMap<String, i32>,
    /// `RouteProblem` layer order → KiCAD layer name, e.g. `["F.Cu", "B.Cu"]`.
    /// Index 0 is `"top"`, the last index is `"bottom"`.
    pub layer_names: Vec<String>,
}

/// Read a `.kicad_pcb` file into a [`BoardProblem`].
///
/// Returns an [`io::Error`] if the file cannot be read or parsed.
pub fn read_problem(path: &Path) -> io::Result<BoardProblem> {
    let doc = PcbFile::read(path).map_err(map_kiutils_err)?;
    let ast = doc.ast();

    let layer_names = copper_layers(ast);
    let layer_count = layer_names.len().max(1) as u32;

    let net_codes = net_codes(ast);
    let connections = connections(ast, &layer_names);
    // kiutils 0.3 drops custom-pad primitive geometry, so a custom pad would be
    // modelled by its tiny base anchor (under-sizing real copper → the placer /
    // outline crop seats it too close to the edge, a copper_edge_clearance fault).
    // Re-parse each custom pad's primitive bbox from the raw source so its obstacle
    // reflects the true copper. (Same fix as footlib::Footprint::load.)
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let custom_bboxes = crate::footlib::custom_pad_bboxes(&raw);
    let obstacles = obstacles(ast, &layer_names, &custom_bboxes);
    let bounds = board_bounds(ast);

    let problem = RouteProblem {
        layer_count,
        min_trace_width: 0.25,
        obstacles,
        connections,
        bounds,
        // Defaults: kiutils' PcbSetup exposes only mask/paste clearances in
        // this version, so there is no board-specific copper clearance / track
        // width to read — fall back to the RouteProblem defaults.
        clearance: 0.2,
        via_diameter: 0.6,
        via_drill: 0.3,
        net_widths: Default::default(),
        outline: None,
        escape_layers: Default::default(),
    };

    Ok(BoardProblem {
        problem,
        net_codes,
        layer_names,
    })
}

/// A board parsed at the FOOTPRINT level — each part's reference + lib_id +
/// placed position/rotation + pad→net, plus layer count and the Edge.Cuts bbox.
/// The import companion to the DSL exporter: [`read_problem`] is routing-oriented
/// and discards refdes/lib_id, so the round-trip into the board DSL reads here.
#[derive(Debug, Clone)]
pub struct ImportedBoard {
    pub layer_count: u32,
    /// Edge.Cuts bounding box (mm). v1 recovers the bbox; a non-rectangular
    /// outline is not yet reconstructed as a polygon.
    pub bounds: Bounds,
    pub parts: Vec<ImportedPart>,
}

/// One placed part recovered from a `.kicad_pcb`.
#[derive(Debug, Clone)]
pub struct ImportedPart {
    pub reference: String,
    pub lib_id: String,
    pub at: Point2,
    /// Placement angle snapped to the nearest quadrant (0/90/180/270).
    pub rotation: i32,
    /// `(pad number, net name)` in file order; a pad with no net is `None`.
    pub pads: Vec<(String, Option<String>)>,
}

/// Read a `.kicad_pcb` into footprint-level [`ImportedBoard`] data for the DSL
/// importer. Skips items with no `lib_id` or no reference (board-graphic-only
/// footprints). Net names come straight from each pad's `(net code "name")`.
pub fn read_board(path: &Path) -> io::Result<ImportedBoard> {
    let doc = PcbFile::read(path).map_err(map_kiutils_err)?;
    let ast = doc.ast();
    let layer_count = copper_layers(ast).len().max(1) as u32;
    let bounds = board_bounds(ast);

    let mut parts = Vec::new();
    for fp in &ast.footprints {
        let Some(lib_id) = fp.lib_id.clone() else {
            continue;
        };
        let reference = fp
            .reference
            .clone()
            .or_else(|| {
                fp.properties
                    .iter()
                    .find(|p| p.key == "Reference")
                    .map(|p| p.value.clone())
            })
            .unwrap_or_default();
        if reference.is_empty() {
            continue;
        }
        let [x, y] = fp.at.unwrap_or([0.0, 0.0]);
        let rotation = snap_quadrant(fp.rotation.unwrap_or(0.0));
        let pads = fp
            .pads
            .iter()
            .filter_map(|pad| {
                pad.number.clone().map(|num| {
                    let net = pad
                        .net
                        .as_ref()
                        .and_then(|n| n.name.clone())
                        .filter(|n| !n.is_empty());
                    (num, net)
                })
            })
            .collect();
        parts.push(ImportedPart {
            reference,
            lib_id,
            at: Point2 { x, y },
            rotation,
            pads,
        });
    }
    Ok(ImportedBoard {
        layer_count,
        bounds,
        parts,
    })
}

/// Snap a file angle (any degrees, CCW) to the nearest 0/90/180/270 quadrant.
fn snap_quadrant(deg: f64) -> i32 {
    let q = ((deg / 90.0).round() as i32).rem_euclid(4);
    q * 90
}

// ── layers ───────────────────────────────────────────────────────────────────

/// Copper layers (type `signal`/`power`, name ending `.Cu`) ordered
/// `F.Cu, In1.Cu, …, B.Cu` — i.e. top, inners ascending, bottom.
fn copper_layers(ast: &PcbAst) -> Vec<String> {
    let mut copper: Vec<String> = ast
        .layers
        .iter()
        .filter(|l| {
            matches!(l.layer_type.as_deref(), Some("signal") | Some("power"))
                && l.name.as_deref().is_some_and(|n| n.ends_with(".Cu"))
        })
        .filter_map(|l| l.name.clone())
        .collect();
    copper.sort_by_key(|name| copper_order(name));
    copper
}

/// Sort key placing `F.Cu` first, `B.Cu` last, and `In{n}.Cu` in between by `n`.
fn copper_order(name: &str) -> i64 {
    match name {
        "F.Cu" => i64::MIN,
        "B.Cu" => i64::MAX,
        n => n
            .strip_prefix("In")
            .and_then(|rest| rest.strip_suffix(".Cu"))
            .and_then(|n| n.parse::<i64>().ok())
            .unwrap_or(0),
    }
}

/// Map a KiCAD copper layer name to the engine's [`LayerRef`] for this board.
/// `F.Cu` → `top`, `B.Cu` → `bottom`, otherwise the name passes through.
fn layer_ref_for(kicad_layer: &str, layer_names: &[String]) -> LayerRef {
    if kicad_layer == "F.Cu" {
        LayerRef::top()
    } else if kicad_layer == "B.Cu" {
        LayerRef::bottom()
    } else if let Some(idx) = layer_names.iter().position(|n| n == kicad_layer) {
        LayerRef(format!("inner{idx}"))
    } else {
        LayerRef(kicad_layer.to_owned())
    }
}

// ── nets / connections ───────────────────────────────────────────────────────

/// Named nets → their KiCAD codes (skipping code 0 / the empty no-net).
fn net_codes(ast: &PcbAst) -> BTreeMap<String, i32> {
    ast.nets
        .iter()
        .filter_map(|n| {
            let code = n.code?;
            let name = n.name.clone()?;
            (code != 0 && !name.is_empty()).then_some((name, code))
        })
        .collect()
}

/// One [`Connection`] per named net, its `points_to_connect` the absolute
/// centers of every pad on that net.
fn connections(ast: &PcbAst, layer_names: &[String]) -> Vec<Connection> {
    // Net name → accumulated route points, keyed in a BTreeMap for stable order.
    let mut by_net: BTreeMap<String, Vec<RoutePoint>> = BTreeMap::new();

    for fp in &ast.footprints {
        for pad in &fp.pads {
            let Some(net) = &pad.net else { continue };
            let (Some(code), Some(name)) = (net.code, net.name.clone()) else {
                continue;
            };
            if code == 0 || name.is_empty() {
                continue;
            }
            let center = pad_center(fp, pad);
            let layer = pad_layer(pad, layer_names);
            by_net.entry(name).or_default().push(RoutePoint {
                x: center.x,
                y: center.y,
                layer,
            });
        }
    }

    by_net
        .into_iter()
        .map(|(name, points_to_connect)| Connection {
            name,
            points_to_connect,
        })
        .collect()
}

/// The connection layer for a pad: `top` for an F.Cu pad, `bottom` for a B.Cu
/// pad, and `top` for a pad spanning both (e.g. a through-hole) — the obstacle
/// still blocks every copper layer.
fn pad_layer(pad: &PcbPad, layer_names: &[String]) -> LayerRef {
    let on_front = pad.layers.iter().any(|l| l == "F.Cu" || l == "*.Cu");
    let on_back = pad.layers.iter().any(|l| l == "B.Cu" || l == "*.Cu");
    if on_front {
        LayerRef::top()
    } else if on_back {
        LayerRef::bottom()
    } else {
        // Fall back to the first concrete copper layer the pad names.
        pad.layers
            .iter()
            .find(|l| l.ends_with(".Cu"))
            .map(|l| layer_ref_for(l, layer_names))
            .unwrap_or_else(LayerRef::top)
    }
}

// ── pad geometry ─────────────────────────────────────────────────────────────

/// Absolute center of a pad: its offset rotated by the **footprint** angle then
/// translated by the footprint position. See the module docs for the rotation.
fn pad_center(fp: &PcbFootprint, pad: &PcbPad) -> Point2 {
    let [fx, fy] = fp.at.unwrap_or([0.0, 0.0]);
    let [dx, dy] = pad.at.unwrap_or([0.0, 0.0]);
    let (rx, ry) = rotate_offset(dx, dy, fp.rotation.unwrap_or(0.0));
    Point2 {
        x: fx + rx,
        y: fy + ry,
    }
}

/// Rotate offset `(dx, dy)` by a CCW angle `deg` in KiCAD's y-down world.
///
/// `x' = dx·cosθ + dy·sinθ`, `y' = -dx·sinθ + dy·cosθ`.
fn rotate_offset(dx: f64, dy: f64, deg: f64) -> (f64, f64) {
    let theta = deg.to_radians();
    let (s, c) = theta.sin_cos();
    (dx * c + dy * s, -dx * s + dy * c)
}

/// Axis-aligned bounding half-extents of a `w × h` rectangle rotated by `deg`.
/// Returns `(half_width, half_height)` of the enclosing AABB.
fn rotated_aabb_half(w: f64, h: f64, deg: f64) -> (f64, f64) {
    let theta = deg.to_radians();
    let (s, c) = theta.sin_cos();
    let hw = (w / 2.0 * c).abs() + (h / 2.0 * s).abs();
    let hh = (w / 2.0 * s).abs() + (h / 2.0 * c).abs();
    (hw, hh)
}

// ── obstacles ────────────────────────────────────────────────────────────────

fn obstacles(ast: &PcbAst, layer_names: &[String], custom_bboxes: &[(f64, f64)]) -> Vec<Obstacle> {
    let mut out = Vec::new();

    // Every pad becomes a rect obstacle on the copper layers it occupies,
    // tagged with its net name (empty for a no-net pad). A custom pad is grown to
    // its primitive bbox (kiutils gives only the base anchor) — `custom_bboxes`
    // lists those half-extents in pad order.
    let mut ci = 0;
    for fp in &ast.footprints {
        for pad in &fp.pads {
            let custom_half = if pad.shape.as_deref() == Some("custom") {
                let b = custom_bboxes.get(ci).copied();
                ci += 1;
                b
            } else {
                None
            };
            out.push(pad_obstacle(fp, pad, layer_names, custom_half));
        }
    }

    // Existing routed copper: segments → width-inflated bbox along the segment.
    for seg in &ast.segments {
        let (Some([sx, sy]), Some([ex, ey])) = (seg.start, seg.end) else {
            continue;
        };
        let width = seg.width.unwrap_or(0.0);
        let min_x = sx.min(ex) - width / 2.0;
        let max_x = sx.max(ex) + width / 2.0;
        let min_y = sy.min(ey) - width / 2.0;
        let max_y = sy.max(ey) + width / 2.0;
        let layers = seg
            .layer
            .as_deref()
            .map(|l| vec![layer_ref_for(l, layer_names)])
            .unwrap_or_default();
        out.push(Obstacle {
            kind: "rect".to_owned(),
            layers,
            center: Point2 {
                x: (min_x + max_x) / 2.0,
                y: (min_y + max_y) / 2.0,
            },
            width: max_x - min_x,
            height: max_y - min_y,
            connected_to: net_name(ast, seg.net).into_iter().collect(),
        });
    }

    // Vias: a square bbox spanning all copper layers.
    for via in &ast.vias {
        let Some([vx, vy]) = via.at else { continue };
        let size = via.size.unwrap_or(0.0);
        out.push(Obstacle {
            kind: "rect".to_owned(),
            layers: all_layer_refs(layer_names),
            center: Point2 { x: vx, y: vy },
            width: size,
            height: size,
            connected_to: net_name(ast, via.net).into_iter().collect(),
        });
    }

    // Zones: skipped for bounds; recorded as a full-board obstacle only when a
    // keepout (v1 simplification — see module docs).
    for zone in &ast.zones {
        if zone.has_keepout {
            let b = board_bounds(ast);
            out.push(Obstacle {
                kind: "rect".to_owned(),
                layers: all_layer_refs(layer_names),
                center: Point2 {
                    x: (b.min_x + b.max_x) / 2.0,
                    y: (b.min_y + b.max_y) / 2.0,
                },
                width: b.max_x - b.min_x,
                height: b.max_y - b.min_y,
                connected_to: Vec::new(),
            });
        }
    }

    out
}

/// A single pad as a rect obstacle (rotated-rect AABB, v1 conservatism).
fn pad_obstacle(
    fp: &PcbFootprint,
    pad: &PcbPad,
    layer_names: &[String],
    custom_half: Option<(f64, f64)>,
) -> Obstacle {
    let center = pad_center(fp, pad);
    let [mut w, mut h] = pad.size.unwrap_or([0.0, 0.0]);
    // Grow a custom pad to its primitive bbox (the base anchor under-sizes it).
    if let Some((hx, hy)) = custom_half {
        w = w.max(2.0 * hx);
        h = h.max(2.0 * hy);
    }
    // The pad's stored rotation is its total rotation in board space.
    let rot = pad.rotation.unwrap_or_else(|| fp.rotation.unwrap_or(0.0));
    let (hw, hh) = rotated_aabb_half(w, h, rot);

    let layers = pad_obstacle_layers(pad, layer_names);
    let connected_to = pad
        .net
        .as_ref()
        .and_then(|n| n.name.clone())
        .filter(|n| !n.is_empty())
        .into_iter()
        .collect();

    Obstacle {
        kind: "rect".to_owned(),
        layers,
        center,
        width: hw * 2.0,
        height: hh * 2.0,
        connected_to,
    }
}

/// Copper layers a pad obstacle blocks. `*.Cu` (and through-hole pads naming
/// both faces) blocks every copper layer; otherwise just the named copper.
fn pad_obstacle_layers(pad: &PcbPad, layer_names: &[String]) -> Vec<LayerRef> {
    if pad.layers.iter().any(|l| l == "*.Cu") {
        return all_layer_refs(layer_names);
    }
    let refs: Vec<LayerRef> = pad
        .layers
        .iter()
        .filter(|l| l.ends_with(".Cu"))
        .map(|l| layer_ref_for(l, layer_names))
        .collect();
    if refs.is_empty() {
        all_layer_refs(layer_names)
    } else {
        refs
    }
}

/// Every copper layer as a [`LayerRef`], in board order.
fn all_layer_refs(layer_names: &[String]) -> Vec<LayerRef> {
    layer_names
        .iter()
        .map(|l| layer_ref_for(l, layer_names))
        .collect()
}

/// Look up the name of a net code in the board's net table (skipping code 0).
fn net_name(ast: &PcbAst, code: Option<i32>) -> Option<String> {
    let code = code?;
    if code == 0 {
        return None;
    }
    ast.nets
        .iter()
        .find(|n| n.code == Some(code))
        .and_then(|n| n.name.clone())
        .filter(|n| !n.is_empty())
}

// ── bounds ───────────────────────────────────────────────────────────────────

/// Board outline bounding box: the bbox over every `Edge.Cuts` graphic's
/// `start`/`end`/`center` points. Falls back to a zero box if absent.
fn board_bounds(ast: &PcbAst) -> Bounds {
    let mut pts: Vec<[f64; 2]> = Vec::new();
    for g in &ast.graphics {
        if g.layer.as_deref() != Some("Edge.Cuts") {
            continue;
        }
        for p in [g.start, g.end, g.center, g.at].into_iter().flatten() {
            pts.push(p);
        }
    }

    if pts.is_empty() {
        return Bounds {
            min_x: 0.0,
            max_x: 0.0,
            min_y: 0.0,
            max_y: 0.0,
        };
    }
    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for [x, y] in pts {
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }
    Bounds {
        min_x,
        max_x,
        min_y,
        max_y,
    }
}

// ── write-back: emit traces & vias ───────────────────────────────────────────
//
// `kiutils_kicad`'s `PcbDocument` cannot append segments/vias (its `ast_mut`
// edits are rejected at `write()`; only title-block/property setters round-trip).
// So write-back follows the house "render text, validate by re-parse" precedent
// (`sch-io/src/write.rs`): we render the `(segment …)` / `(via …)`
// s-expressions ourselves, splice them in before the file's final closing paren
// — preserving every original byte outside the insertion point — then re-read
// with `PcbFile::read` and assert the counts grew by exactly what we emitted with
// no new diagnostics. The render is fully deterministic (content-derived v5
// UUIDs, minimal number formatting), so identical input yields byte-identical
// output.

/// Fixed namespace UUID for auto-pcb **board** copper identifiers
/// (`5c1a7d4e-3f62-5b89-a0d1-2e3f4a5b6c7d`). Distinct from the schematic
/// namespace in `sch-model/src/ids.rs` so a segment and a symbol never collide.
/// Do not change: doing so would alter every emitted segment/via UUID.
const PCB_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x5c1a_7d4e_3f62_5b89_a0d1_2e3f_4a5b_6c7d);

/// Content-derived UUID for emitted copper. The same `key` always yields the
/// same canonical hyphenated UUID (byte-identical re-emit).
fn copper_uuid(key: &str) -> String {
    uuid::Uuid::new_v5(&PCB_NAMESPACE, key.as_bytes())
        .as_hyphenated()
        .to_string()
}

/// Format an `f64` the way KiCAD writes coordinates: a bare minimal decimal with
/// no trailing zeros (`10`, `8.9125`), and `-0.0` collapsed to `0`. Mirrors
/// `sch_io::write::fmt_coord`'s negative-zero canonicalization.
///
/// The single owner of KiCAD coordinate formatting; `pcb-synth` re-uses it via
/// the [`crate::fmt_num`] re-export so synthesized boards stay byte-identical.
pub fn fmt_num(v: f64) -> String {
    let v = if v == 0.0 { 0.0 } else { v };
    // Rust's `{}` for f64 already prints the shortest round-tripping decimal
    // with no trailing zeros (e.g. `10`, `8.9125`), which matches KiCAD.
    format!("{v}")
}

/// Map an engine [`LayerRef`] to this board's KiCAD copper layer name via the
/// index mapping `read_problem` established (`LayerRef::index → layer_names[i]`).
fn kicad_layer(layer: &LayerRef, board: &BoardProblem) -> io::Result<String> {
    let layer_count = board.layer_names.len() as u32;
    let idx = layer.index(layer_count).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "layer {:?} is out of range for a {layer_count}-layer board",
                layer.0
            ),
        )
    })?;
    board.layer_names.get(idx as usize).cloned().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "layer {:?} maps to index {idx} with no board layer",
                layer.0
            ),
        )
    })
}

/// The KiCAD net code for a connection, or an `InvalidData` error naming it.
fn net_code_for(connection: &str, board: &BoardProblem) -> io::Result<i32> {
    board.net_codes.get(connection).copied().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("connection {connection:?} has no net code on this board"),
        )
    })
}

/// Render one trace polyline into `(segment …)` lines (one per consecutive,
/// non-degenerate point pair) appended to `out`. Returns the number emitted.
fn render_trace(out: &mut String, trace: &Trace, board: &BoardProblem) -> io::Result<usize> {
    let layer = kicad_layer(&trace.layer, board)?;
    let net = net_code_for(&trace.connection, board)?;
    let w = fmt_num(trace.width);
    let mut count = 0;
    for pair in trace.path.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if a.x == b.x && a.y == b.y {
            continue; // skip zero-length pairs
        }
        let (x1, y1, x2, y2) = (fmt_num(a.x), fmt_num(a.y), fmt_num(b.x), fmt_num(b.y));
        let uuid = copper_uuid(&format!("segment:{net}:{x1}:{y1}:{x2}:{y2}:{layer}"));
        let _ = writeln!(
            out,
            "\t(segment (start {x1} {y1}) (end {x2} {y2}) (width {w}) (layer \"{layer}\") (net {net}) (uuid \"{uuid}\"))"
        );
        count += 1;
    }
    Ok(count)
}

/// Render one via into a `(via …)` line appended to `out`. A `Through` via spans the
/// full copper stack and emits no type keyword (byte-identical to the historical output);
/// a `Partial` span is an HDI via and emits KiCAD's BARE `micro`/`blind` keyword right
/// after `via` — NOT a `(type …)` sub-node, which would silently break the JSON DRC report
/// writer (see docs/specs/hdi-microvia-feasibility.md).
fn render_via(out: &mut String, via: &Via, board: &BoardProblem) -> io::Result<()> {
    let net = net_code_for(&via.connection, board)?;
    let (x, y) = (fmt_num(via.at.x), fmt_num(via.at.y));
    let (size, drill) = (fmt_num(via.diameter), fmt_num(via.drill));
    let layer_at = |idx: usize, fallback: &str| -> String {
        board
            .layer_names
            .get(idx)
            .map(String::as_str)
            .unwrap_or(fallback)
            .to_owned()
    };
    let last = board
        .layer_names
        .last()
        .map(String::as_str)
        .unwrap_or("B.Cu")
        .to_owned();
    let (kind, top, bottom) = match &via.span {
        ViaSpan::Through => ("", layer_at(0, "F.Cu"), last),
        ViaSpan::Partial { from, to, micro } => (
            if *micro { "micro " } else { "blind " },
            layer_at(*from as usize, "F.Cu"),
            layer_at(*to as usize, "B.Cu"),
        ),
    };
    // Keep the Through uuid seed exactly as before so existing boards stay byte-identical;
    // a Partial via folds its span into the seed (two spans at one xy must not collide).
    let uuid = match &via.span {
        ViaSpan::Through => copper_uuid(&format!("via:{net}:{x}:{y}:{size}:{drill}")),
        ViaSpan::Partial { .. } => {
            copper_uuid(&format!("via:{net}:{x}:{y}:{size}:{drill}:{top}:{bottom}"))
        }
    };
    let _ = writeln!(
        out,
        "\t(via {kind}(at {x} {y}) (size {size}) (drill {drill}) (layers \"{top}\" \"{bottom}\") (net {net}) (uuid \"{uuid}\"))"
    );
    Ok(())
}

/// Render the full copper block (all traces' segments, then all vias). Returns
/// the rendered text and `(segment_count, via_count)`.
fn render_solution(
    solution: &RouteSolution,
    board: &BoardProblem,
) -> io::Result<(String, usize, usize)> {
    let mut block = String::new();
    let mut segments = 0;
    for trace in &solution.traces {
        segments += render_trace(&mut block, trace, board)?;
    }
    let vias = solution.vias.len();
    for via in &solution.vias {
        render_via(&mut block, via, board)?;
    }
    Ok((block, segments, vias))
}

/// Splice `block` into `source` immediately before the file's final `)` (the
/// root `(kicad_pcb …)` closer), preserving every original byte. Returns the new
/// text and the byte offset of the splice point (the length of the unchanged
/// prefix), so callers can assert prefix bytes are untouched.
fn splice_before_root_close(source: &str, block: &str) -> io::Result<(String, usize)> {
    let close = source.rfind(')').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "board has no closing paren to splice before",
        )
    })?;
    let mut out = String::with_capacity(source.len() + block.len());
    out.push_str(&source[..close]);
    out.push_str(block);
    out.push_str(&source[close..]);
    Ok((out, close))
}

/// Emit `solution`'s traces and vias into the `.kicad_pcb` at `path`.
///
/// Each trace polyline becomes one `(segment …)` per consecutive non-zero-length
/// point pair on `(layer "<mapped>")`; each via becomes one `(via …)` spanning
/// the copper stack. Layer names come from `board.layer_names` (via
/// `LayerRef::index`) and net codes from `board.net_codes` — a trace whose
/// connection has no net code is an `InvalidData` error naming it. UUIDs are
/// deterministic (content-derived v5), so identical input yields byte-identical
/// output.
///
/// The rendered block is spliced before the file's final closing paren, leaving
/// every original byte intact. The result is staged to a temp file, re-read with
/// [`PcbFile::read`], and validated (zero diagnostics beyond the pre-write
/// baseline; segment/via counts grew by exactly the emitted numbers) **before**
/// it atomically replaces `path` — a board that fails validation is never left
/// behind.
pub fn write_solution(
    path: &Path,
    solution: &RouteSolution,
    board: &BoardProblem,
) -> io::Result<()> {
    // Baseline: the file must already parse. Capture pre-write counts and the
    // diagnostic count so post-write validation compares against the real prior
    // state rather than assuming zero.
    let baseline = PcbFile::read(path).map_err(map_kiutils_err)?;
    let base_segments = baseline.ast().segments.len();
    let base_vias = baseline.ast().vias.len();
    let base_diags = baseline.diagnostics().len();
    drop(baseline);

    let source = std::fs::read_to_string(path)?;

    let (block, n_segments, n_vias) = render_solution(solution, board)?;
    let (spliced, splice_at) = splice_before_root_close(&source, &block)?;

    // Stage to a temp file in the SAME directory (so the final rename is atomic
    // on the same filesystem), validate, then promote. This mirrors the
    // staging-then-rename idiom used elsewhere in the bridge.
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = tempfile::Builder::new()
        .prefix("autopcb-pcb-")
        .suffix(".kicad_pcb")
        .tempfile_in(dir)?;
    std::fs::write(tmp.path(), spliced.as_bytes())?;

    // Re-read & validate before promoting; the temp file is dropped (deleted) on
    // any early return, so a bad board never replaces the original.
    let reread = PcbFile::read(tmp.path()).map_err(map_kiutils_err)?;
    let new_diags = reread.diagnostics().len();
    if new_diags > base_diags {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "write_solution produced {} new diagnostic(s): {:?}",
                new_diags - base_diags,
                reread.diagnostics()
            ),
        ));
    }
    let got_segments = reread.ast().segments.len();
    let got_vias = reread.ast().vias.len();
    if got_segments != base_segments + n_segments {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "segment count mismatch after write: expected {}, got {got_segments}",
                base_segments + n_segments
            ),
        ));
    }
    if got_vias != base_vias + n_vias {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "via count mismatch after write: expected {}, got {got_vias}",
                base_vias + n_vias
            ),
        ));
    }
    // Lossless prefix: the bytes before the splice point are unchanged.
    debug_assert_eq!(
        spliced.as_bytes()[..splice_at],
        source.as_bytes()[..splice_at]
    );
    drop(reread);

    // Validated: atomically replace the original.
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Extract the copper already routed on a board back into a [`RouteSolution`].
///
/// The inverse of [`write_solution`]: each `(segment …)` becomes a 2-point
/// [`Trace`] (its `connection` resolved from the net code, its `layer` mapped
/// back to a [`LayerRef`]) and each `(via …)` becomes a [`Via`]. Copper on the
/// no-net code (0) or an unnamed net is skipped — it carries no connection
/// identity to attribute. Reused by the round-trip oracle test and slice 1's
/// e2e to read a board's existing routing as a solution.
pub fn extract_copper(path: &Path) -> io::Result<RouteSolution> {
    let doc = PcbFile::read(path).map_err(map_kiutils_err)?;
    let ast = doc.ast();
    let layer_names = copper_layers(ast);

    let mut traces = Vec::new();
    for seg in &ast.segments {
        let (Some([sx, sy]), Some([ex, ey])) = (seg.start, seg.end) else {
            continue;
        };
        let Some(connection) = net_name(ast, seg.net) else {
            continue;
        };
        let layer = seg
            .layer
            .as_deref()
            .map(|l| layer_ref_for(l, &layer_names))
            .unwrap_or_else(LayerRef::top);
        traces.push(Trace {
            connection,
            layer,
            width: seg.width.unwrap_or(0.0),
            path: vec![Point2 { x: sx, y: sy }, Point2 { x: ex, y: ey }],
        });
    }

    let mut vias = Vec::new();
    for via in &ast.vias {
        let Some([vx, vy]) = via.at else { continue };
        let Some(connection) = net_name(ast, via.net) else {
            continue;
        };
        vias.push(Via {
            connection,
            at: Point2 { x: vx, y: vy },
            diameter: via.size.unwrap_or(0.0),
            drill: via.drill.unwrap_or(0.0),
            span: ViaSpan::Through,
        });
    }

    Ok(RouteSolution { traces, vias })
}

// ── error mapping ────────────────────────────────────────────────────────────

fn map_kiutils_err(e: kiutils_kicad::Error) -> io::Error {
    match e {
        kiutils_kicad::Error::Io(io) => io,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

#[cfg(test)]
mod via_render_tests {
    use super::*;
    use pcb_model::{Bounds, Point2, RouteProblem, Via, ViaSpan};
    use std::collections::BTreeMap;

    fn board_4layer() -> BoardProblem {
        let problem = RouteProblem {
            layer_count: 4,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![],
            bounds: Bounds { min_x: 0.0, max_x: 10.0, min_y: 0.0, max_y: 10.0 },
            clearance: 0.15,
            via_diameter: 0.5,
            via_drill: 0.3,
            net_widths: BTreeMap::new(),
            outline: None,
            escape_layers: Default::default(),
        };
        let mut net_codes = BTreeMap::new();
        net_codes.insert("GND".to_owned(), 1);
        BoardProblem {
            problem,
            net_codes,
            layer_names: vec!["F.Cu".into(), "In1.Cu".into(), "In2.Cu".into(), "B.Cu".into()],
        }
    }

    fn render(span: ViaSpan) -> String {
        let via = Via {
            connection: "GND".to_owned(),
            at: Point2 { x: 8.4, y: 8.4 },
            diameter: 0.5,
            drill: 0.3,
            span,
        };
        let mut out = String::new();
        render_via(&mut out, &via, &board_4layer()).unwrap();
        out
    }

    #[test]
    fn through_via_emits_no_keyword_full_stack() {
        let s = render(ViaSpan::Through);
        assert!(s.contains("(via (at 8.4 8.4)"), "through via has no type keyword: {s}");
        assert!(s.contains("(layers \"F.Cu\" \"B.Cu\")"), "through spans the full stack: {s}");
    }

    #[test]
    fn micro_via_emits_bare_micro_keyword_and_span() {
        // F.Cu -> In1.Cu microvia (the de-risked HDI inner-ball escape form).
        let s = render(ViaSpan::Partial { from: 0, to: 1, micro: true });
        assert!(s.contains("(via micro (at 8.4 8.4)"), "micro keyword must be BARE after via: {s}");
        assert!(s.contains("(layers \"F.Cu\" \"In1.Cu\")"), "micro spans its own layers: {s}");
        // The wrong `(type micro)` form silently breaks the json DRC writer — must never appear.
        assert!(!s.contains("(type"), "no (type ...) sub-node allowed: {s}");
    }

    #[test]
    fn blind_via_emits_bare_blind_keyword_and_span() {
        let s = render(ViaSpan::Partial { from: 0, to: 2, micro: false });
        assert!(s.contains("(via blind (at 8.4 8.4)"), "blind keyword must be BARE after via: {s}");
        assert!(s.contains("(layers \"F.Cu\" \"In2.Cu\")"), "blind spans its own layers: {s}");
    }
}
