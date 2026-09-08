//! Specctra DSN writer: the board as the design file Freerouting reads.
//!
//! Frames: KiCad is mm with y down, DSN is um with y up, so a point maps to
//! `(x * 1000, -y * 1000)`. Images follow KiCad's own exporter (what Freerouting expects):
//! front pads translate and rotate, back pads mirror x first and then add 180 degrees.
//!
//! Ported from `pcbagent.route.dsn`, cut down to what this pipeline routes: a whole,
//! freshly placed two-layer board with no routing window, no kept nets and no blind vias.

use std::collections::{HashMap, HashSet};

use kicad::KicadInstallation;

use crate::geom::{cross, point_in_polygon, seg_hits_box, BBox, Point};
use crate::model::{expand_layers, Board, Footprint, Pad, Rules};
use crate::rules::{fine_escape, signal_track_width};

/// mm -> um.
pub const SCALE: f64 = 1000.0;

/// Freerouting's own defaults make a layer hop nearly free, so it answers a congested board
/// with a via instead of routing around. These state the cost model in `(autoroute_settings)`.
const DEFAULT_VIA_COSTS: i64 = 30;
const PREFERRED_TRACE_COSTS: f64 = 1.0;
const AGAINST_PREFERRED_TRACE_COSTS: f64 = 2.5;

/// A filled pour can carry thousands of arc-facet points; this many is all the router needs.
const MAX_PLANE_POINTS: usize = 200;

/// Quote a DSN token when it holds characters the parser would split on.
pub fn q(name: &str) -> String {
    let bare = !name.is_empty()
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || "_+-./:#$%&*<>=?@^|~[]".contains(c)
        });
    if bare {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "'"))
    }
}

/// A millimetre value as DSN micrometres, trailing `.0` dropped.
fn um(v: f64) -> String {
    let s = format!("{:.1}", v * SCALE);
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// A KiCad-frame point as a DSN coordinate pair.
fn pt(p: Point) -> String {
    format!("{} {}", um(p.0), um(-p.1))
}

/// Rotate in the y-up DSN frame (mathematically positive = counter-clockwise on screen).
fn rot_up(p: Point, deg: f64) -> Point {
    let a = deg.to_radians();
    let (c, s) = (a.cos(), a.sin());
    (p.0 * c - p.1 * s, p.0 * s + p.1 * c)
}

/// Rotate in the KiCad board frame (y down), matching `geom::rotate`.
fn rot_down(p: Point, deg: f64) -> Point {
    crate::geom::rotate(p, deg)
}

/// A track inside another pad's solder-mask aperture is a mask-bridge DRC error, so the router
/// has to stay outside the aperture as well as outside the copper.
fn mask_aperture_clearance(board: &Board) -> f64 {
    let Some(setup) = board.tree.find("setup") else {
        return 0.0;
    };
    let get = |k: &str| setup.find(k).and_then(|n| n.arg_f64(0)).unwrap_or(0.0);
    get("pad_to_mask_clearance") + get("solder_mask_min_width") + 0.01
}

/// F.Cu, In1.Cu .. InN.Cu, B.Cu.
pub fn copper_layer_order(board: &Board) -> Vec<String> {
    let mut names = board.copper_layers();
    let key = |n: &String| -> (u8, i64) {
        if n == "F.Cu" {
            (0, 0)
        } else if n == "B.Cu" {
            (2, 0)
        } else {
            let inner = n
                .strip_prefix("In")
                .and_then(|s| s.strip_suffix(".Cu"))
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            (1, inner)
        }
    };
    names.sort_by_key(key);
    names
}

/// KiCad's own padstack name for a via: `Via[i-j]_size:drill_um` (i/j = copper layer indices).
pub fn via_name(size: f64, drill: f64, span: (usize, usize)) -> String {
    format!(
        "Via[{}-{}]_{:.0}:{:.0}_um",
        span.0,
        span.1,
        size * SCALE,
        drill * SCALE
    )
}

/// A via's `(layers A B)` as a pair of copper-layer indices, low first.
fn via_span(v: &crate::model::Via, copper: &[String]) -> (usize, usize) {
    let idx = |l: &String, default: usize| copper.iter().position(|c| c == l).unwrap_or(default);
    let a = idx(&v.layers.0, 0);
    let b = idx(&v.layers.1, copper.len().saturating_sub(1));
    if a <= b { (a, b) } else { (b, a) }
}

/// A polygon with at most `max_points` vertices, dropping the least significant first.
pub fn simplify_polygon(pts: &[Point], max_points: usize) -> Vec<Point> {
    if pts.len() <= max_points {
        return pts.to_vec();
    }
    let keep = |tol: f64| -> Vec<Point> {
        let mut out: Vec<Point> = vec![pts[0]];
        for &p in &pts[1..] {
            let a = *out.last().expect("seeded above");
            if (p.0 - a.0).hypot(p.1 - a.1) >= tol {
                out.push(p);
            }
        }
        // a vertex that sits on the line between its neighbours carries no shape
        let n = out.len();
        let mut res = Vec::with_capacity(n);
        for (i, &p) in out.iter().enumerate() {
            let a = out[(i + n - 1) % n];
            let b = out[(i + 1) % n];
            let (ux, uy) = (b.0 - a.0, b.1 - a.1);
            let len = ux.hypot(uy);
            let d = if len > 1e-9 {
                ((p.0 - a.0) * uy - (p.1 - a.1) * ux).abs() / len
            } else {
                0.0
            };
            if d >= tol / 2.0 {
                res.push(p);
            }
        }
        if res.len() >= 3 { res } else { out }
    };
    let mut tol = 0.02;
    let mut best = pts.to_vec();
    for _ in 0..12 {
        let out = keep(tol);
        if out.len() >= 3 {
            best = out;
        }
        if best.len() <= max_points {
            break;
        }
        tol *= 1.7;
    }
    if best.len() >= 3 { best } else { pts.to_vec() }
}

/// The `(autoroute_settings ...)` scope for a stackup: alternating preferred directions
/// (F.Cu horizontal, the next layer vertical, ...) and a via that has to earn itself.
///
/// Position matters. Freerouting's `AutorouteSettings.readScope` swallows the token that
/// follows the scope, so a settings block written last inside `(structure)` eats the
/// structure's own closing bracket and everything after it is silently skipped.
fn autoroute_settings(copper: &[String]) -> String {
    let mut lines = vec![
        "    (autoroute_settings".to_string(),
        "      (fanout off)".to_string(),
        "      (autoroute on)".to_string(),
        "      (postroute on)".to_string(),
        "      (vias on)".to_string(),
        format!("      (via_costs {DEFAULT_VIA_COSTS})"),
    ];
    for (i, lay) in copper.iter().enumerate() {
        // alternate on the stackup index, not on the count of signal layers
        let direction = if i % 2 == 0 { "horizontal" } else { "vertical" };
        lines.push(format!(
            "      (layer_rule {}\n        (active on)\n        (preferred_direction {})\n        \
             (preferred_direction_trace_costs {})\n        \
             (against_preferred_direction_trace_costs {})\n      )",
            q(lay), direction, py_float(PREFERRED_TRACE_COSTS), py_float(AGAINST_PREFERRED_TRACE_COSTS)
        ));
    }
    lines.push("    )".to_string());
    lines.join("\n")
}

/// A cost as Python's `repr` writes it, so the settings block is byte-identical: a whole
/// number keeps its `.0`.
fn py_float(v: f64) -> String {
    if v == v.trunc() { format!("{v:.1}") } else { format!("{v}") }
}

/// One coordinate as a dedup key. Rounded to the tenth of a micrometre the file is written
/// at, with the sign of zero dropped -- Python keys on floats, where `-0.0 == 0.0`, so two
/// otherwise identical pads must not become two padstacks over a signed zero.
fn key_coord(v: f64) -> String {
    let s = format!("{v:.1}");
    if s == "-0.0" { "0.0".to_string() } else { s }
}

/// The transform between the DSN board frame and an image's local frame.
struct Placement {
    x: f64,
    y: f64,
    side: &'static str,
    rot: f64,
}

impl Placement {
    fn of(f: &Footprint) -> Placement {
        let front = f.side() == "front";
        Placement {
            x: f.pos.0 * SCALE,
            y: -f.pos.1 * SCALE,
            side: if front { "front" } else { "back" },
            rot: (if front { f.rot } else { f.rot + 180.0 }).rem_euclid(360.0),
        }
    }

    /// KiCad-frame mm point -> image-local DSN um point.
    fn to_local(&self, p: Point) -> Point {
        let d = (p.0 * SCALE - self.x, -p.1 * SCALE - self.y);
        let l = rot_up(d, -self.rot);
        if self.side == "back" { (-l.0, l.1) } else { l }
    }
}


/// The pad's copper box.
pub(crate) fn pad_bbox(_f: &Footprint, p: &Pad) -> BBox {
    let (w, h) = p.size;
    let rot = p.rot;
    BBox::of_points([(-w / 2.0, -h / 2.0), (w / 2.0, -h / 2.0), (w / 2.0, h / 2.0), (-w / 2.0, h / 2.0)]
        .into_iter()
        .map(|c| {
            let r = rot_down(c, rot);
            (p.pos.0 + r.0, p.pos.1 + r.1)
        }))
}

/// Pad geometry in the KiCad board frame: `(kind, points, width)` where kind is
/// `circle` (points = [centre], width = diameter), `path` (two ends, width) or
/// `polygon` (closed outline, width 0). A plain hole grows by `hole_margin` mm.
fn pad_outline_board(_f: &Footprint, p: &Pad, hole_margin: f64) -> (&'static str, Vec<Point>, f64) {
    let (w, h) = p.size;
    if p.kind == "np_thru_hole" {
        let bare = if w > 0.0 && h > 0.0 { w.min(h) } else { 0.0 };
        let d = p.drill.unwrap_or(0.0).max(bare) + 2.0 * hole_margin;
        return ("circle", vec![p.pos], d);
    }
    if p.shape == "circle" || (p.shape == "oval" && (w - h).abs() < 1e-6) {
        return ("circle", vec![p.pos], w.max(h));
    }
    let prot = p.rot;
    if p.shape == "oval" {
        let (ends, width) = if w > h {
            ([(-(w - h) / 2.0, 0.0), ((w - h) / 2.0, 0.0)], h)
        } else {
            ([(0.0, -(h - w) / 2.0), (0.0, (h - w) / 2.0)], w)
        };
        let pts = ends
            .iter()
            .map(|&e| {
                let r = rot_down(e, prot);
                (p.pos.0 + r.0, p.pos.1 + r.1)
            })
            .collect();
        return ("path", pts, width);
    }
    let local: Vec<Point> = if p.shape == "roundrect" && p.roundrect_ratio.unwrap_or(0.0) > 0.0 {
        let r = (p.roundrect_ratio.unwrap_or(0.0) * w.min(h)).min(w / 2.0).min(h / 2.0);
        let (cx, cy) = (w / 2.0 - r, h / 2.0 - r);
        let corners = [
            ((cx, cy), 0.0),
            ((-cx, cy), 90.0),
            ((-cx, -cy), 180.0),
            ((cx, -cy), 270.0),
        ];
        let mut out = Vec::with_capacity(24);
        for ((ox, oy), a0) in corners {
            for k in 0..6 {
                let a: f64 = (a0 + 90.0 * k as f64 / 5.0).to_radians();
                out.push((ox + r * a.cos(), oy + r * a.sin()));
            }
        }
        out
    } else if p.shape == "custom" {
        let mut pts = vec![
            (-w / 2.0, -h / 2.0),
            (w / 2.0, -h / 2.0),
            (w / 2.0, h / 2.0),
            (-w / 2.0, h / 2.0),
        ];
        pts.extend(p.custom_points.iter().copied());
        convex_hull(&pts)
    } else {
        // rect, trapezoid, anything else: the pad's size box
        vec![
            (-w / 2.0, -h / 2.0),
            (w / 2.0, -h / 2.0),
            (w / 2.0, h / 2.0),
            (-w / 2.0, h / 2.0),
        ]
    };
    let pts = local
        .into_iter()
        .map(|l| {
            let r = rot_down(l, prot);
            (p.pos.0 + r.0, p.pos.1 + r.1)
        })
        .collect();
    ("polygon", pts, 0.0)
}

/// Monotone-chain hull, matching the Python writer's tie-breaking (`cross <= 0` pops).
fn convex_hull(pts: &[Point]) -> Vec<Point> {
    let mut sorted: Vec<Point> = pts.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    sorted.dedup();
    if sorted.len() < 3 {
        return sorted;
    }
    let chain = |src: &mut dyn Iterator<Item = Point>| -> Vec<Point> {
        let mut out: Vec<Point> = Vec::new();
        for p in src {
            while out.len() >= 2 && cross(out[out.len() - 2], out[out.len() - 1], p) <= 0.0 {
                out.pop();
            }
            out.push(p);
        }
        out.pop();
        out
    };
    let mut lower = chain(&mut sorted.iter().copied());
    let upper = chain(&mut sorted.iter().rev().copied());
    lower.extend(upper);
    lower
}

/// Layers a pad occupies in the image frame, where F.Cu is the image's own side.
fn image_layers(f: &Footprint, p: &Pad, copper: &[String]) -> Vec<String> {
    if p.is_through() {
        return copper.to_vec();
    }
    let lays: HashSet<&String> = p.layers.iter().collect();
    let front = f.side() == "front";
    let own = if front { "F.Cu" } else { "B.Cu" };
    let other = if front { "B.Cu" } else { "F.Cu" };
    let mut out = Vec::new();
    if lays.contains(&own.to_string()) {
        out.push("F.Cu".to_string());
    }
    if lays.contains(&other.to_string()) {
        out.push("B.Cu".to_string());
    }
    for l in copper {
        if l.starts_with("In") && lays.contains(l) {
            out.push(l.clone());
        }
    }
    if out.is_empty() {
        vec!["F.Cu".to_string()]
    } else {
        out
    }
}

// ---- pour ---------------------------------------------------------------------------------

/// A pad as the pour analysis sees it: net, centre, copper box, layers, identity.
type PadEntry = (String, Point, BBox, Vec<String>, (usize, usize));
/// A zone outline and the copper layers it covers.
type ZonePlane = (Vec<Point>, Vec<String>);
/// A via padstack name and the span and barrel it stands for.
type ViaSpec = (String, ((usize, usize), f64));

/// One filled piece of one pour.
#[derive(Clone)]
struct Island {
    net: String,
    layer: String,
    poly: Vec<Point>,
    bbox: BBox,
}

/// Does a pad at `pos` with copper box `bbox` sit on this piece of fill?
///
/// True if the pad centre is inside the fill, or the fill's boundary runs through the pad's
/// box — a thermal-relief pad sits in a *hole* in the fill, not inside it.
fn pad_on_island(pos: Point, bbox: &BBox, island: &Island) -> bool {
    if point_in_polygon(pos, &island.poly) {
        return true;
    }
    if island.bbox.x0 > bbox.x1
        || island.bbox.x1 < bbox.x0
        || island.bbox.y0 > bbox.y1
        || island.bbox.y1 < bbox.y0
    {
        return false;
    }
    let n = island.poly.len();
    (0..n).any(|i| seg_hits_box(island.poly[i], island.poly[(i + 1) % n], bbox))
}

/// `(layer, polygon)` for every filled piece of a `(zone ...)` node.
fn zone_fill_polys(node: &crate::sexp::SList, fallback_layer: &str) -> Vec<(String, Vec<Point>)> {
    let mut out = Vec::new();
    for fp in node.lists(Some("filled_polygon")) {
        let layer = fp
            .find("layer")
            .and_then(|l| l.arg_text(0))
            .unwrap_or(fallback_layer)
            .to_string();
        let Some(pts) = fp.find("pts") else { continue };
        let poly: Vec<Point> = pts
            .lists(Some("xy"))
            .iter()
            .map(|p| (p.arg_f64(0).unwrap_or(0.0), p.arg_f64(1).unwrap_or(0.0)))
            .collect();
        if poly.len() >= 3 {
            out.push((layer, poly));
        }
    }
    out
}

/// Every filled island of every copper pour, refilled through `kicad-cli` on a scratch copy.
///
/// Simplified from the Python writer: islands are never joined through tracks, vias or
/// through-hole pads, so one island is one connected group. That is exact for the board this
/// pipeline routes — freshly placed, no copper yet, one pour per net and layer.
fn filled_islands(board: &Board, kicad: Option<&KicadInstallation>) -> Vec<Island> {
    let Some(kicad) = kicad else { return vec![] };
    let has_pour = board
        .zones()
        .iter()
        .any(|z| z.keepout.is_none() && !z.polygon.is_empty() && (z.net_id != 0 || !z.net_name.is_empty()));
    if !has_pour {
        return vec![];
    }
    let Ok(dir) = tempfile::Builder::new().prefix("pcb-auto-fill-").tempdir() else {
        return vec![];
    };
    let path = dir.path().join("fill.kicad_pcb");
    if std::fs::write(&path, board.dumps()).is_err() {
        return vec![];
    }
    if kicad.refill_zones(&path, true).is_err() {
        return vec![];
    }
    let Ok(filled) = Board::load(&path) else {
        return vec![];
    };
    let copper = copper_layer_order(&filled);
    let zones = filled.zones();
    let nets: HashMap<i64, String> = filled.nets().into_iter().map(|n| (n.id, n.name)).collect();
    let mut out = Vec::new();
    for (z, node) in zones.iter().zip(filled.tree.lists(Some("zone"))) {
        if z.keepout.is_some() {
            continue;
        }
        let name = if z.net_name.is_empty() {
            nets.get(&z.net_id).cloned().unwrap_or_default()
        } else {
            z.net_name.clone()
        };
        let own = expand_layers(&z.layers, &copper);
        let fallback = own.first().cloned().unwrap_or_default();
        for (layer, poly) in zone_fill_polys(node, &fallback) {
            if name.is_empty() || !copper.contains(&layer) {
                continue;
            }
            let bbox = BBox::of_points(poly.iter().copied());
            out.push(Island { net: name.clone(), layer, poly, bbox });
        }
    }
    out
}

/// Pour analysis: which island is a net's main copper on a layer, and which pins the fill
/// already connects (net name -> pad keys held out of `(network)`).
struct Pour {
    /// (net, layer) -> the polygons that are the net's main copper there.
    mains: HashMap<(String, String), Vec<Vec<Point>>>,
    /// Islands that are neither main copper nor hold a pin: obstacles, connecting nothing.
    obstacles: Vec<(String, Vec<Point>)>,
    covered: HashMap<String, HashSet<(usize, usize)>>,
    any_fill: bool,
}

fn analyse_pour(board: &Board, copper: &[String], islands: &[Island]) -> Pour {
    let nets: HashMap<i64, String> = board.nets().into_iter().map(|n| (n.id, n.name)).collect();
    let mut pour = Pour {
        mains: HashMap::new(),
        obstacles: Vec::new(),
        covered: HashMap::new(),
        any_fill: !islands.is_empty(),
    };

    // every net pad, with the copper layers it reaches
    let mut pads: Vec<PadEntry> = Vec::new();
    for f in board.footprints() {
        for p in &f.pads {
            if p.net_id == 0 || p.number.is_empty() {
                continue;
            }
            let name = nets.get(&p.net_id).cloned().unwrap_or_else(|| p.net_name.clone());
            if name.is_empty() {
                continue;
            }
            let lays: Vec<String> = if p.is_through() {
                copper.to_vec()
            } else {
                p.copper_layers().into_iter().filter(|l| copper.contains(l)).collect()
            };
            pads.push((name, p.pos, pad_bbox(&f, p), lays, p.node_key));
        }
    }

    if islands.is_empty() {
        // No fill to read: fall back to the zone outlines, which over-state what the copper
        // joins but are all an export without KiCad has.
        let mut planes: HashMap<String, Vec<ZonePlane>> = HashMap::new();
        for z in board.zones() {
            if z.keepout.is_some() || z.polygon.is_empty() {
                continue;
            }
            let name = if z.net_name.is_empty() {
                nets.get(&z.net_id).cloned().unwrap_or_default()
            } else {
                z.net_name.clone()
            };
            let lays: Vec<String> =
                expand_layers(&z.layers, copper).into_iter().filter(|l| copper.contains(l)).collect();
            if !name.is_empty() && !lays.is_empty() {
                planes.entry(name).or_default().push((z.polygon.clone(), lays));
            }
        }
        for (name, pos, _bb, lays, key) in &pads {
            for (poly, zl) in planes.get(name).into_iter().flatten() {
                if zl.iter().any(|l| lays.contains(l)) && point_in_polygon(*pos, poly) {
                    pour.covered.entry(name.clone()).or_default().insert(*key);
                    break;
                }
            }
        }
        return pour;
    }

    // pins per island, so the best island of a net can be told from its crumbs
    let mut on: Vec<Vec<usize>> = vec![Vec::new(); islands.len()];
    for (i, island) in islands.iter().enumerate() {
        for (j, (name, pos, bb, lays, _)) in pads.iter().enumerate() {
            if *name == island.net && lays.contains(&island.layer) && pad_on_island(*pos, bb, island) {
                on[i].push(j);
            }
        }
    }

    // The net's main copper is the island that feeds the most pins, largest first on a tie;
    // every other island is a crumb. Per NET, not per layer: Freerouting reads two planes of one
    // net as one piece of copper, so naming the pour on both sides lets it "connect" a stranded
    // pin by touching the other side's plane, and the board comes back nine connections worse.
    let mut by_net: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, island) in islands.iter().enumerate() {
        by_net.entry(island.net.as_str()).or_default().push(i);
    }
    let mut main_of: HashSet<usize> = HashSet::new();
    for ids in by_net.values() {
        let best = ids.iter().copied().max_by(|&a, &b| {
            let key = |i: usize| {
                (
                    on[i].len(),
                    crate::geom::polygon_area(&islands[i].poly).abs(),
                )
            };
            let (ka, kb) = (key(a), key(b));
            ka.0.cmp(&kb.0)
                .then(ka.1.partial_cmp(&kb.1).unwrap_or(std::cmp::Ordering::Equal))
        });
        if let Some(b) = best {
            main_of.insert(b);
        }
    }
    for (i, island) in islands.iter().enumerate() {
        if main_of.contains(&i) {
            pour.mains
                .entry((island.net.clone(), island.layer.clone()))
                .or_default()
                .push(island.poly.clone());
        } else if on[i].is_empty() {
            // a crumb connects nothing, but a foreign track driven through it still severs
            // the pour it was cut from, so it goes out as an anonymous obstacle
            pour.obstacles.push((island.layer.clone(), island.poly.clone()));
        }
    }

    // A pin is only held out of `(network)` when it sits on the net's MAIN plane, and then only
    // after the first: the plane is what joins those. A pin on a crumb is NOT connected by the
    // pour -- the crumb may be adrift -- so it stays in the network and the router has to reach
    // it. Treating a crumb like the plane is what left ground pads stranded after the fill
    // fragmented around the routing.
    let mut seen: HashMap<&str, HashSet<usize>> = HashMap::new();
    for (j, (name, _, _, _, key)) in pads.iter().enumerate() {
        let Some(i) = (0..islands.len())
            .find(|&i| islands[i].net == *name && on[i].contains(&j))
        else {
            continue;
        };
        if !main_of.contains(&i) {
            continue;
        }
        if !seen.entry(name.as_str()).or_default().insert(i) {
            pour.covered.entry(name.clone()).or_default().insert(*key);
        }
    }
    pour
}

// ---- document -----------------------------------------------------------------------------

/// The Specctra design file, plus what the writer decided while building it.
pub struct DsnDocument {
    pub text: String,
    /// Nets written into `(network)`, in board order.
    pub nets: Vec<String>,
    /// Every via padstack in the library, the default one first.
    pub via_names: Vec<String>,
    /// Copper layers, F.Cu first.
    pub layers: Vec<String>,
    /// Pins left out of `(network)` because a pour already connects them.
    pub pins_dropped: usize,
    /// Net name -> the number of pins actually offered to the router.
    pub pins_offered: HashMap<String, usize>,
}

/// Insertion-ordered map: the Python writer's output order is dict order, and the DSN text
/// has to be reproducible.
struct Ordered<V> {
    idx: HashMap<String, usize>,
    items: Vec<(String, V)>,
}

impl<V> Ordered<V> {
    fn new() -> Self {
        Ordered { idx: HashMap::new(), items: Vec::new() }
    }
    fn entry(&mut self, key: &str, make: impl FnOnce() -> V) -> &mut V {
        let i = match self.idx.get(key) {
            Some(&i) => i,
            None => {
                let i = self.items.len();
                self.items.push((key.to_string(), make()));
                self.idx.insert(key.to_string(), i);
                i
            }
        };
        &mut self.items[i].1
    }
    fn get(&self, key: &str) -> Option<&V> {
        self.idx.get(key).map(|&i| &self.items[i].1)
    }
    fn len(&self) -> usize {
        self.items.len()
    }
}

struct Padstack {
    name: String,
    layers: Vec<String>,
    kind: &'static str,
    /// local um relative to the pin, DSN frame
    pts: Vec<Point>,
    /// um
    width: f64,
}

impl Padstack {
    fn text(&self) -> String {
        let mut shapes = Vec::new();
        for lay in &self.layers {
            match self.kind {
                "circle" => shapes.push(format!("      (shape (circle {lay} {:.1}))", self.width)),
                "path" => {
                    let (a, b) = (self.pts[0], self.pts[1]);
                    shapes.push(format!(
                        "      (shape (path {lay} {:.1}  {:.1} {:.1}  {:.1} {:.1}))",
                        self.width, a.0, a.1, b.0, b.1
                    ));
                }
                _ => {
                    let coords = self
                        .pts
                        .iter()
                        .chain(self.pts.first())
                        .map(|p| format!("{:.1} {:.1}", p.0, p.1))
                        .collect::<Vec<_>>()
                        .join("  ");
                    shapes.push(format!("      (shape (polygon {lay} 0  {coords}))"));
                }
            }
        }
        format!(
            "    (padstack {}\n{}\n      (attach off)\n    )",
            q(&self.name),
            shapes.join("\n")
        )
    }
}

fn closed_coords(poly: &[Point]) -> String {
    poly.iter()
        .chain(poly.first())
        .map(|&p| pt(p))
        .collect::<Vec<_>>()
        .join("  ")
}

/// The board as a Specctra design file.
///
/// `net_widths` gives per-net track widths (each distinct width becomes its own class);
/// `rules` are the routing rules (see [`crate::rules::infer_rules`]); `kicad` is used to
/// refill the pours on a scratch copy so the fill can be written as `(plane ...)` and the
/// pins it already connects held out of `(network)`.
pub fn write_dsn(
    board: &Board,
    net_widths: &std::collections::BTreeMap<String, f64>,
    rules: &Rules,
    kicad: Option<&KicadInstallation>,
) -> anyhow::Result<DsnDocument> {
    write_dsn_scoped(board, net_widths, rules, kicad, &HashSet::new())
}

/// [`write_dsn`], with routing scoped to `only_nets` when it is non-empty: every other net
/// goes out with a single pin, so the router is asked for nothing on it, while its copper is
/// still written as protected wiring for the router to keep clear of.
pub fn write_dsn_scoped(
    board: &Board,
    net_widths: &std::collections::BTreeMap<String, f64>,
    rules: &Rules,
    kicad: Option<&KicadInstallation>,
    only_nets: &HashSet<String>,
) -> anyhow::Result<DsnDocument> {
    let copper = copper_layer_order(board);
    anyhow::ensure!(!copper.is_empty(), "board has no copper layers");
    let outline = board
        .outline_polygon()
        .ok_or_else(|| anyhow::anyhow!("board has no closed outline on Edge.Cuts; routing needs one"))?;

    let mut width = signal_track_width(rules, Some(board));
    let mut via_size = rules.via_size;
    let mut via_drill = rules.via_drill;
    let hole_margin = (rules.hole_clearance - rules.clearance).max(0.0);
    // The router works on 0.1 um integers and polygonised pads; a hair of margin keeps KiCad's
    // exact check from reporting 0.1993 mm against a 0.2 mm rule.
    let mut clearance = (rules.clearance + 0.01).max(mask_aperture_clearance(board));

    // A pin field tighter than that lane gets a Specctra class of its own, at the narrowest
    // width the board's own copper uses on such a part; without it a rail cannot leave its pad.
    let fine = fine_escape(board, rules);
    let fine_nets: HashSet<String> = fine
        .as_ref()
        .map(|f| f.nets.iter().cloned().collect())
        .unwrap_or_default();
    let fine_width = fine.as_ref().map(|f| f.width).unwrap_or(width);
    let fine_clearance = match &fine {
        Some(f) => (f.clearance + 0.01).max(mask_aperture_clearance(board)),
        None => clearance,
    };

    let nets_by_id: HashMap<i64, String> =
        board.nets().into_iter().map(|n| (n.id, n.name)).collect();
    let footprints = board.footprints();
    let tracks = board.tracks();
    let vias = board.vias();

    let islands = filled_islands(board, kicad);
    let pour = analyse_pour(board, &copper, &islands);

    let mut out: Vec<String> = Vec::new();
    out.push(format!("(pcb {}", q("pcbagent")));
    out.push("  (parser\n    (string_quote \")\n    (space_in_quoted_tokens on)\n    (host_cad \"pcbagent\")\n    (host_version \"0.1\")\n  )".to_string());
    out.push("  (resolution um 10)\n  (unit um)".to_string());

    // ---- structure
    out.push("  (structure".to_string());
    for (i, lay) in copper.iter().enumerate() {
        out.push(format!(
            "    (layer {lay}\n      (type signal)\n      (property\n        (index {i})\n      )\n    )"
        ));
    }
    out.push(autoroute_settings(&copper));
    out.push(format!(
        "    (boundary\n      (path pcb 0  {})\n    )",
        closed_coords(&outline)
    ));

    let mut plane_i = 0usize;
    let mut written: HashSet<(String, String)> = HashSet::new();
    for z in board.zones() {
        if z.keepout.is_some() || z.polygon.is_empty() {
            continue;
        }
        for lay in expand_layers(&z.layers, &copper) {
            if !copper.contains(&lay) {
                continue;
            }
            let mut name = if z.net_name.is_empty() {
                nets_by_id.get(&z.net_id).cloned().unwrap_or_default()
            } else {
                z.net_name.clone()
            };
            let named = !name.is_empty();
            if !named {
                name = format!("@:no_net_{plane_i}");
            }
            plane_i += 1;
            let polys = if named {
                pour.mains.get(&(name.clone(), lay.clone()))
            } else {
                None
            };
            match polys {
                Some(polys) => {
                    if !written.insert((name.clone(), lay.clone())) {
                        continue;
                    }
                    for poly in polys {
                        let poly = simplify_polygon(poly, MAX_PLANE_POINTS);
                        if poly.len() < 3 {
                            continue;
                        }
                        out.push(format!(
                            "    (plane {} (polygon {lay} 0  {}))",
                            q(&name),
                            closed_coords(&poly)
                        ));
                    }
                }
                None => {
                    // A refill that said nothing about this net fills to nothing, so it
                    // connects nothing and the zone outline is all there is to draw.
                    if pour.any_fill && named {
                        continue;
                    }
                    out.push(format!(
                        "    (plane {} (polygon {lay} 0  {}))",
                        q(&name),
                        closed_coords(&z.polygon)
                    ));
                }
            }
        }
    }
    for (lay, poly) in &pour.obstacles {
        let poly = simplify_polygon(poly, MAX_PLANE_POINTS);
        if poly.len() < 3 {
            continue;
        }
        out.push(format!(
            "    (plane {} (polygon {lay} 0  {}))",
            q(&format!("@:no_net_{plane_i}")),
            closed_coords(&poly)
        ));
        plane_i += 1;
    }
    for (k, z) in board.zones().iter().enumerate() {
        let Some(ko) = &z.keepout else { continue };
        if z.polygon.is_empty() {
            continue;
        }
        let kind = if !ko.get("tracks").copied().unwrap_or(true) {
            "keepout"
        } else if !ko.get("vias").copied().unwrap_or(true) {
            "via_keepout"
        } else {
            continue;
        };
        let mut layers: Vec<String> = expand_layers(&z.layers, &copper)
            .into_iter()
            .filter(|l| copper.contains(l))
            .collect();
        if layers.is_empty() {
            layers = copper.clone();
        }
        let coords = closed_coords(&z.polygon);
        let name = if z.name.is_empty() { format!("keepout_{k}") } else { z.name.clone() };
        for lay in layers {
            out.push(format!("    ({kind} {} (polygon {lay} 0  {coords}))", q(&name)));
        }
    }

    // The through via a new hop is drawn at is the board's own, when the board has one and it
    // is no bigger than the rule's: a via the board carries is one its fab already accepted.
    let through_span = (0usize, copper.len() - 1);
    for v in &vias {
        if via_span(v, &copper) == through_span && v.size > 0.0 && v.size < via_size {
            via_size = v.size;
            via_drill = v.drill;
        }
    }
    let default_via = via_name(via_size, via_drill, through_span);
    // One padstack per distinct via span and size, named the way KiCad's exporter names them.
    // Only the through padstack is ever *offered* to new routing: this pipeline routes
    // two-layer boards, where every span is the through span.
    let mut via_specs: Vec<ViaSpec> = vec![(default_via.clone(), (through_span, via_size))];
    for v in &vias {
        let span = via_span(v, &copper);
        let name = via_name(v.size, v.drill, span);
        if !via_specs.iter().any(|(n, _)| *n == name) {
            via_specs.push((name, (span, v.size)));
        }
    }
    via_specs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut via_names = vec![default_via.clone()];
    via_names.extend(via_specs.iter().map(|(n, _)| n.clone()).filter(|n| *n != default_via));
    let use_via_str = q(&default_via);
    out.push(format!(
        "    (via {})",
        via_names.iter().map(|n| q(n)).collect::<Vec<_>>().join(" ")
    ));
    out.push(format!(
        "    (rule\n      (width {})\n      (clearance {})\n      (clearance {} (type smd_smd))\n    )",
        um(width),
        um(clearance),
        um(clearance.min(0.05))
    ));
    out.push("  )".to_string());

    // ---- placement + library
    let mut padstacks: Ordered<Padstack> = Ordered::new();
    let mut images: Ordered<(String, Vec<String>)> = Ordered::new();
    let mut image_names: HashMap<String, usize> = HashMap::new();
    let mut placements: Ordered<Vec<String>> = Ordered::new();
    let mut pins_by_net: Ordered<Vec<(String, (usize, usize))>> = Ordered::new();

    for f in &footprints {
        let pl = Placement::of(f);
        let mut pin_lines = Vec::new();
        let mut sig = format!("{}|{}", f.lib_id, pl.side);
        for p in &f.pads {
            let (kind, pts_b, wd) = pad_outline_board(f, p, hole_margin);
            let center = pl.to_local(p.pos);
            let layers = image_layers(f, p, &copper);
            let rel: Vec<Point> = if kind == "circle" {
                vec![]
            } else {
                pts_b
                    .iter()
                    .map(|&b| {
                        let l = pl.to_local(b);
                        (l.0 - center.0, l.1 - center.1)
                    })
                    .collect()
            };
            let ps_width = wd * SCALE;
            let key = format!(
                "{kind}|{}|{}|{:.1}",
                layers.join(","),
                rel.iter()
                    .map(|p| format!("{},{}", key_coord(p.0), key_coord(p.1)))
                    .collect::<Vec<_>>()
                    .join(";"),
                ps_width
            );
            let next = padstacks.len() + 1;
            let ps_name = {
                let ps = padstacks.entry(&key, || Padstack {
                    name: format!("Pad{next}_{kind}"),
                    layers: layers.clone(),
                    kind,
                    pts: rel.clone(),
                    width: ps_width,
                });
                ps.name.clone()
            };
            pin_lines.push(format!(
                "      (pin {} {} {:.1} {:.1})",
                q(&ps_name),
                q(&p.number),
                center.0,
                center.1
            ));
            sig.push_str(&format!(
                "|{ps_name}~{}~{}~{}",
                p.number,
                key_coord(center.0),
                key_coord(center.1)
            ));
            if p.net_id != 0 && !p.number.is_empty() {
                let name = nets_by_id
                    .get(&p.net_id)
                    .cloned()
                    .unwrap_or_else(|| p.net_name.clone());
                pins_by_net
                    .entry(&name, Vec::new)
                    .push((q(&format!("{}-{}", f.ref_, p.number)), p.node_key));
            }
        }
        let n = *image_names.get(&f.lib_id).unwrap_or(&0);
        let fresh = if n == 0 { f.lib_id.clone() } else { format!("{}::{}", f.lib_id, n) };
        let existed = images.get(&sig).is_some();
        let img_name = images.entry(&sig, || (fresh, pin_lines)).0.clone();
        if !existed {
            image_names.insert(f.lib_id.clone(), n + 1);
        }
        let value = if f.value.is_empty() { "~" } else { f.value.as_str() };
        placements.entry(&img_name, Vec::new).push(format!(
            "      (place {} {:.1} {:.1} {} {:.4} (PN {}))",
            q(&f.ref_),
            pl.x,
            pl.y,
            pl.side,
            pl.rot,
            q(value)
        ));
    }

    out.push("  (placement".to_string());
    for (img_name, places) in &placements.items {
        out.push(format!("    (component {}", q(img_name)));
        out.extend(places.iter().cloned());
        out.push("    )".to_string());
    }
    out.push("  )".to_string());
    out.push("  (library".to_string());
    for (_, (img_name, pin_lines)) in &images.items {
        out.push(format!("    (image {}", q(img_name)));
        out.extend(pin_lines.iter().cloned());
        out.push("    )".to_string());
    }
    for (_, ps) in &padstacks.items {
        out.push(ps.text());
    }
    for (name, (span, size)) in &via_specs {
        // copper only on the layers the via actually spans -- that is what makes it blind
        let shapes = copper[span.0..=span.1]
            .iter()
            .map(|lay| format!("      (shape (circle {lay} {:.1}))", size * SCALE))
            .collect::<Vec<_>>()
            .join("\n");
        out.push(format!(
            "    (padstack {}\n{shapes}\n      (attach off)\n    )",
            q(name)
        ));
    }
    out.push("  )".to_string());

    // ---- network
    out.push("  (network".to_string());
    let mut net_names: Vec<String> = Vec::new();
    let mut pins_out: HashMap<String, Vec<String>> = HashMap::new();
    let mut dropped = 0usize;
    for (name, entries) in &pins_by_net.items {
        if name.is_empty() || entries.len() < 2 {
            continue;
        }
        let empty = HashSet::new();
        let skip = pour.covered.get(name).unwrap_or(&empty);
        let pins: Vec<String> = if !only_nets.is_empty() && !only_nets.contains(name) {
            // Not up for routing: the name is declared so the wiring and planes have
            // something to refer to, and one pin leaves the router nothing to connect.
            vec![entries[0].0.clone()]
        } else {
            entries
                .iter()
                .filter(|(_, key)| !skip.contains(key))
                .map(|(tok, _)| tok.clone())
                .collect()
        };
        dropped += entries.len() - pins.len();
        // The name still has to exist -- the plane refers to it -- so a net the pour connects
        // end to end keeps one pin, which asks the router for nothing.
        let pins = if pins.is_empty() { vec![entries[0].0.clone()] } else { pins };
        pins_out.insert(name.clone(), pins);
        net_names.push(name.clone());
    }
    for name in &net_names {
        out.push(format!(
            "    (net {}\n      (pins {})\n    )",
            q(name),
            pins_out[name].join(" ")
        ));
    }
    // A net is in exactly one class. When taking the fine nets out would leave `kicad_default`
    // with no nets at all, the fine gauge simply BECOMES the default gauge.
    let mut in_fine: Vec<String> =
        net_names.iter().filter(|n| fine_nets.contains(*n)).cloned().collect();
    let mut default_nets: Vec<String> = net_names
        .iter()
        .filter(|n| !net_widths.contains_key(*n) && !fine_nets.contains(*n))
        .cloned()
        .collect();
    if !in_fine.is_empty() && default_nets.is_empty() {
        default_nets = std::mem::take(&mut in_fine);
        width = fine_width;
        clearance = fine_clearance;
    }
    let fine_set: HashSet<&String> = in_fine.iter().collect();
    let default_set: HashSet<&String> = default_nets.iter().collect();
    let class = |head: &str, nets: &[String], w: f64, c: f64| {
        format!(
            "    (class {head}\n      {}\n      (circuit\n        (use_via {use_via_str})\n      )\n      (rule\n        (width {})\n        (clearance {})\n      )\n    )",
            nets.iter().map(|n| q(n)).collect::<Vec<_>>().join(" "),
            um(w),
            um(c)
        )
    };
    out.push(class("kicad_default", &default_nets, width, clearance));
    if !in_fine.is_empty() {
        out.push(class("fine", &in_fine, fine_width, fine_clearance));
    }
    let mut by_width: Vec<(f64, Vec<String>)> = Vec::new();
    for n in &net_names {
        if fine_set.contains(n) || default_set.contains(n) {
            continue;
        }
        let Some(&wd) = net_widths.get(n) else { continue };
        match by_width.iter_mut().find(|(w, _)| *w == wd) {
            Some((_, v)) => v.push(n.clone()),
            None => by_width.push((wd, vec![n.clone()])),
        }
    }
    by_width.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    for (i, (wd, ns)) in by_width.iter().enumerate() {
        out.push(class(&format!("width_{i}"), ns, *wd, clearance));
    }
    out.push("  )".to_string());

    // ---- wiring (normally empty: this pipeline routes a freshly placed board)
    out.push("  (wiring".to_string());
    for t in &tracks {
        let Some(name) = nets_by_id.get(&t.net_id) else { continue };
        if name.is_empty() || !copper.contains(&t.layer) {
            continue;
        }
        out.push(format!(
            "    (wire (path {} {}  {}  {})(net {})(type protect))",
            t.layer,
            um(t.width),
            pt(t.start),
            pt(t.end),
            q(name)
        ));
    }
    for v in &vias {
        let Some(name) = nets_by_id.get(&v.net_id) else { continue };
        if name.is_empty() {
            continue;
        }
        out.push(format!(
            "    (via {}  {} (net {})(type protect))",
            q(&via_name(v.size, v.drill, via_span(v, &copper))),
            pt(v.pos),
            q(name)
        ));
    }
    out.push("  )".to_string());
    out.push(")".to_string());

    let pins_offered = net_names
        .iter()
        .map(|n| (n.clone(), pins_out[n].len()))
        .collect();
    Ok(DsnDocument {
        text: out.join("\n") + "\n",
        nets: net_names,
        via_names,
        layers: copper,
        pins_dropped: dropped,
        pins_offered,
    })
}
