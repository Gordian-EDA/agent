//! Reading a `.kicad_pcb` board into a [`pcb_engine::problem::RouteProblem`].
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
use std::io;
use std::path::Path;

use kiutils_kicad::{PcbAst, PcbFile, PcbFootprint, PcbPad};
use pcb_engine::problem::{
    Bounds, Connection, LayerRef, Obstacle, Point2, RoutePoint, RouteProblem,
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
    let obstacles = obstacles(ast, &layer_names);
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
    };

    Ok(BoardProblem {
        problem,
        net_codes,
        layer_names,
    })
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

fn obstacles(ast: &PcbAst, layer_names: &[String]) -> Vec<Obstacle> {
    let mut out = Vec::new();

    // Every pad becomes a rect obstacle on the copper layers it occupies,
    // tagged with its net name (empty for a no-net pad).
    for fp in &ast.footprints {
        for pad in &fp.pads {
            out.push(pad_obstacle(fp, pad, layer_names));
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
fn pad_obstacle(fp: &PcbFootprint, pad: &PcbPad, layer_names: &[String]) -> Obstacle {
    let center = pad_center(fp, pad);
    let [w, h] = pad.size.unwrap_or([0.0, 0.0]);
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

// ── error mapping ────────────────────────────────────────────────────────────

fn map_kiutils_err(e: kiutils_kicad::Error) -> io::Error {
    match e {
        kiutils_kicad::Error::Io(io) => io,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}
