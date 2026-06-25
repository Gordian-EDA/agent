//! Specctra `.dsn` export + `.ses` import for the **Freerouting** autorouter.
//!
//! Freerouting (the external Java autorouter vendored at
//! `tools/vendor/freerouting.jar`) speaks the Specctra interchange format: it
//! reads a `.dsn` (the unrouted design — board outline, layers, placement, the
//! per-footprint pad geometry library, and the netlist) and writes a `.ses`
//! (the routed wires + vias). This module is the bridge: [`export_dsn_with_rules`]
//! turns a placed `.kicad_pcb` into a `.dsn`, [`import_ses`] parses the `.ses` back
//! into geometry, and [`freeroute_with_rules`] orchestrates the round-trip through
//! the jar.
//!
//! ## Why a dedicated exporter (vs. reusing [`kicad_sexpr::pcb::read_problem`])
//!
//! `read_problem` *flattens* the board into engine obstacles — it loses the
//! component → pad-offset structure that Specctra's `placement` + `library`
//! sections require (Freerouting routes against pad *images* placed at component
//! origins, not pre-rotated absolute rects). So the exporter walks the kiutils
//! footprint AST directly for placement/library, and reuses `read_problem` only
//! for the netlist, layer order, bounds, and design rules.
//!
//! ## Coordinate conventions (the classic gotcha)
//!
//! - **Units.** KiCAD files are mm; the `.dsn` declares `(resolution um 10)` and
//!   `(unit um)`, so every coordinate is emitted in **micrometres** = mm × 1000.
//! - **Y axis.** Specctra is **y-up**; KiCAD PCB space is **y-down**. We negate
//!   every Y on the way out and negate it back on the way in. Get this wrong and
//!   the whole board mirrors vertically.
//! - **Rotation.** KiCAD file angles are CCW-positive, which matches Specctra's
//!   `(place ... rot)`. Component rotation is emitted as-is; pad offsets in the
//!   `image` library are in the footprint's *unrotated* frame (Freerouting
//!   applies the component rotation), so they are **not** pre-rotated here — only
//!   their Y is negated.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use kiutils_kicad::{PcbAst, PcbFile, PcbFootprint, PcbPad};
use kiutils_sexpr::{parse_one, Atom, Node};
use pcb_model::{Point2, RouteSolution, Trace, Via, ViaSpan};

use kicad_sexpr::pcb::{read_problem, BoardProblem};

/// mm → Specctra um (the `.dsn` unit). KiCAD stores mm; Freerouting wants um.
const UM_PER_MM: f64 = 1000.0;

/// Factor applied to the design clearance when DECLARING it to Freerouting.
/// Freerouting lands traces a hair inside the declared gap, so a small over-declare
/// makes its real output clear KiCAD's true rule without starving routability on a
/// fine-pitch board (a 2× over-declare blocks every between-ball channel and tanks
/// coverage). The oracle / `.kicad_pro` still judge at the true clearance. See the
/// clearance gotcha in [`structure_section`].
const DSN_CLEARANCE_FACTOR: f64 = 1.1;

// ── public geometry types ──────────────────────────────────────────────────────

/// A routed copper polyline recovered from a `.ses`.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutedWire {
    /// KiCAD net name (resolved from the `.ses` `net` token).
    pub net: String,
    /// KiCAD copper layer name (e.g. `F.Cu`, `In1.Cu`, `B.Cu`).
    pub layer: String,
    /// Trace width in **mm**.
    pub width_mm: f64,
    /// Ordered polyline vertices in **mm**, KiCAD y-down space.
    pub path: Vec<Point2>,
}

/// A routed via recovered from a `.ses`.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutedVia {
    /// KiCAD net name.
    pub net: String,
    /// Via center in **mm**, KiCAD y-down space.
    pub at: Point2,
    /// Finished via diameter (mm). Recovered from the padstack the `.ses`
    /// references, falling back to the board default if unknown.
    pub diameter_mm: f64,
    /// Drill diameter (mm), defaulted from the board.
    pub drill_mm: f64,
}

/// The geometry Freerouting produced for a board: routed wires + vias, in mm,
/// KiCAD coordinate space, ready to splice onto the board via [`to_solution`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RoutedGeometry {
    pub wires: Vec<RoutedWire>,
    pub vias: Vec<RoutedVia>,
}

impl RoutedGeometry {
    /// Convert into an engine [`RouteSolution`] (the form [`kicad_sexpr::pcb::write_solution`]
    /// consumes). Wire layers/vias map back through the board's layer order, so
    /// `board` must be the same board the `.dsn` was exported from.
    pub fn to_solution(&self, board: &BoardProblem) -> RouteSolution {
        let traces = self
            .wires
            .iter()
            .filter(|w| w.path.len() >= 2)
            .map(|w| Trace {
                connection: w.net.clone(),
                layer: kicad_sexpr::pcb::layer_ref_for(&w.layer, &board.layer_names),
                width: w.width_mm,
                path: w.path.clone(),
            })
            .collect();
        let vias = self
            .vias
            .iter()
            .map(|v| Via {
                connection: v.net.clone(),
                at: v.at.clone(),
                diameter: v.diameter_mm,
                drill: v.drill_mm,
                span: ViaSpan::Through,
            })
            .collect();
        RouteSolution { traces, vias }
    }
}

// ── error type ─────────────────────────────────────────────────────────────────

/// What can go wrong orchestrating Freerouting.
#[derive(Debug)]
pub enum FreerouteError {
    /// I/O reading the board / writing the `.dsn` / reading the `.ses`.
    Io(io::Error),
    /// The Freerouting jar is missing (run `tools/vendor/fetch_freerouting.sh`).
    JarMissing(String),
    /// `xvfb-run` (or `java`) is not on PATH.
    ToolMissing(String),
    /// Freerouting ran but exited nonzero / produced no `.ses`. Carries stderr.
    RunFailed(String),
    /// The `.ses` could not be parsed.
    Parse(String),
}

impl std::fmt::Display for FreerouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FreerouteError::Io(e) => write!(f, "io error: {e}"),
            FreerouteError::JarMissing(s) => write!(f, "freerouting jar missing: {s}"),
            FreerouteError::ToolMissing(s) => write!(f, "required tool missing: {s}"),
            FreerouteError::RunFailed(s) => write!(f, "freerouting run failed: {s}"),
            FreerouteError::Parse(s) => write!(f, "could not parse .ses: {s}"),
        }
    }
}

impl std::error::Error for FreerouteError {}

impl From<io::Error> for FreerouteError {
    fn from(e: io::Error) -> Self {
        FreerouteError::Io(e)
    }
}

// ── DSN export ─────────────────────────────────────────────────────────────────

/// One unique footprint *image* (Specctra `library` entry): all parts with the
/// same lib_id share one image whose pads define their padstacks.
struct Image {
    /// Sanitized image id (lib_id with Specctra-unsafe chars replaced).
    id: String,
    /// One entry per pad: `(pad number, padstack id, dx_um, dy_um)`. dx/dy are in
    /// the footprint's unrotated frame, Y already negated.
    pins: Vec<(String, String, f64, f64)>,
}

/// A padstack: a pad shape on copper layers. Keyed by its id so identical shapes
/// across footprints share one definition.
struct Padstack {
    id: String,
    /// Rendered `(shape ...)` body lines (per layer), already in um, y-negated.
    shape_lines: Vec<String>,
}

/// Design rules to route against, overriding the [`BoardProblem`] defaults.
///
/// `read_problem` can only return engine defaults for copper rules (this kiutils
/// version doesn't surface board clearance/width — see [`kicad_sexpr::pcb`]), so a
/// caller that knows the board's true fine rules (e.g. a 0.1mm-clearance BGA
/// escape) passes them here. The same rules are emitted to Freerouting AND should
/// be written into the board's `net_settings` (via [`write_net_settings`]) so the
/// KiCAD DRC oracle judges the result against the design intent, not its 0.2mm
/// built-in default.
#[derive(Debug, Clone, Copy)]
pub struct RouteRules {
    /// Minimum trace width (mm).
    pub trace_width: f64,
    /// Copper clearance (mm).
    pub clearance: f64,
    /// Finished via diameter (mm).
    pub via_diameter: f64,
    /// Via drill diameter (mm).
    pub via_drill: f64,
}

impl RouteRules {
    /// Pull the current (engine-default) rules out of a parsed board.
    pub fn from_board(board: &BoardProblem) -> Self {
        RouteRules {
            trace_width: board.problem.min_trace_width,
            clearance: board.problem.clearance,
            via_diameter: board.problem.via_diameter,
            via_drill: board.problem.via_drill,
        }
    }

    /// Apply these rules onto a [`BoardProblem`] in place.
    fn apply(&self, board: &mut BoardProblem) {
        board.problem.min_trace_width = self.trace_width;
        board.problem.clearance = self.clearance;
        board.problem.via_diameter = self.via_diameter;
        board.problem.via_drill = self.via_drill;
    }
}

/// Write a KiCAD project file (`<board-stem>.kicad_pro`) beside `board_path` whose
/// **Default** netclass carries these rules.
///
/// `kicad-cli pcb drc` reads the sibling `.kicad_pro` for its netclass, so a board
/// designed for a fine clearance (e.g. a 0.1mm BGA escape) is judged against that
/// intent instead of KiCAD's 0.2mm built-in default. Without it, fine copper that
/// is perfectly legal for the design reads as a wall of clearance violations.
/// This is the DRC analog of [`export_dsn_with_rules`]: route AND judge at the
/// same rules.
pub fn write_net_settings(board_path: &Path, rules: RouteRules) -> io::Result<()> {
    let stem = board_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("board");
    let pro_path = board_path.with_file_name(format!("{stem}.kicad_pro"));

    // Minimal `.kicad_pro` with one Default netclass at the routed rules. Fields
    // mirror KiCAD 9's schema (extra fields are tolerated / defaulted by KiCAD).
    let pro = format!(
        r#"{{
  "board": {{
    "design_settings": {{
      "rules": {{
        "min_clearance": 0.0,
        "min_track_width": 0.0,
        "min_via_diameter": 0.0,
        "min_hole_clearance": {hole},
        "min_hole_to_hole": {hole}
      }}
    }}
  }},
  "net_settings": {{
    "classes": [
      {{
        "name": "Default",
        "clearance": {clr},
        "track_width": {tw},
        "via_diameter": {vd},
        "via_drill": {vdr},
        "microvia_diameter": 0.3,
        "microvia_drill": 0.1,
        "diff_pair_gap": 0.25,
        "diff_pair_width": 0.2,
        "priority": 2147483647
      }}
    ],
    "meta": {{ "version": 3 }}
  }},
  "meta": {{ "filename": "{stem}.kicad_pro", "version": 1 }}
}}
"#,
        hole = rules.clearance,
        clr = rules.clearance,
        tw = rules.trace_width,
        vd = rules.via_diameter,
        vdr = rules.via_drill,
    );
    std::fs::write(pro_path, pro)
}

/// Read a placed `.kicad_pcb` at `board_path` and write a Specctra `.dsn` to
/// `dsn_path` for Freerouting, with explicit [`RouteRules`] overriding the board
/// defaults. The `.dsn` contains resolution/unit, the layer stack, the board
/// boundary, width/clearance rules, component placement, the footprint pad-image
/// library, and the netlist. **No existing wiring is emitted** — Freerouting
/// routes from scratch.
pub fn export_dsn_with_rules(
    board_path: &Path,
    dsn_path: &Path,
    rules: RouteRules,
) -> io::Result<()> {
    let mut board = read_problem(board_path)?;
    rules.apply(&mut board);
    export_dsn_inner(board_path, dsn_path, &board)
}

fn export_dsn_inner(board_path: &Path, dsn_path: &Path, board: &BoardProblem) -> io::Result<()> {
    let doc = PcbFile::read(board_path).map_err(map_kiutils_err)?;
    let ast = doc.ast();
    let layer_names = &board.layer_names;
    let dsn = build_dsn(ast, board, layer_names);
    std::fs::write(dsn_path, dsn)
}

/// The high-fanout power nets poured as planes for `board_path` (net name → KiCAD
/// copper layer), exactly as [`export_dsn_with_rules`] decides them. A caller writing the
/// routed board back uses this to lay down the matching plane zones (see
/// [`write_plane_zones`]) so the poured power nets are actually connected in KiCAD.
pub fn plane_nets(board_path: &Path) -> io::Result<BTreeMap<String, String>> {
    let board = read_problem(board_path)?;
    let doc = PcbFile::read(board_path).map_err(map_kiutils_err)?;
    Ok(plane_assignment(doc.ast(), &board.layer_names))
}


/// Net name → the copper layer it is poured as a PLANE on (instead of routed as
/// discrete traces). The escape for high-fanout power nets — see [`plane_assignment`].
type PlaneAssignment = BTreeMap<String, String>;

/// Decide which high-fanout nets become copper PLANES, and on which layers.
///
/// REFRAME (the structural lever): a power net like GND with 85 pins routed as 85+
/// point-to-point traces saturates a signal layer and forces crossings/shorts — the
/// exact failure we saw. Real boards pour such nets as a PLANE on a dedicated inner
/// layer; pins drop a short via to the plane, and the signal layers stay open. So we
/// pull the highest-fanout nets onto the inner layers (one net per inner layer) as
/// planes, leaving F.Cu / B.Cu (and any remaining inners) for signal routing.
///
/// A net qualifies if it has `>= POWER_FANOUT_MIN` pins (i.e. it is power/ground, not
/// a bus). On a 2-layer board nothing is pulled (no inner layer to spare).
fn plane_assignment(ast: &PcbAst, layer_names: &[String]) -> PlaneAssignment {
    // The board ALREADY carries its power/ground planes as filled copper `(zone)`s
    // (the engine pours GND/VCC on dedicated inner layers — verified on
    // bga-escape-fineclear: GND on In1.Cu, VCC on In2.Cu). So we DETECT those, not
    // invent our own: a net poured on a copper layer is a plane Freerouting must NOT
    // route point-to-point — it just drops a via from each plane-net pin to the
    // existing pour. (Inventing planes would duplicate the pour and double-route the
    // net.) Only NON-keepout zones with a named net on a copper layer count.
    let copper: std::collections::BTreeSet<&str> =
        layer_names.iter().map(String::as_str).collect();

    let mut assignment = PlaneAssignment::new();
    for zone in &ast.zones {
        if zone.has_keepout {
            continue;
        }
        let Some(net) = zone.net_name.clone().filter(|n| !n.is_empty()) else {
            continue;
        };
        // Zone layer is in `layer` (single) or the first copper entry of `layers`.
        let layer = zone
            .layer
            .clone()
            .filter(|l| copper.contains(l.as_str()))
            .or_else(|| {
                zone.layers
                    .iter()
                    .find(|l| copper.contains(l.as_str()))
                    .cloned()
            });
        if let Some(layer) = layer {
            assignment.entry(net).or_insert(layer);
        }
    }
    assignment
}

/// Render the whole `.dsn` text.
fn build_dsn(ast: &PcbAst, board: &BoardProblem, layer_names: &[String]) -> String {
    let mut out = String::new();

    let planes = plane_assignment(ast, layer_names);

    // Header / parser.
    out.push_str("(pcb gordian\n");
    out.push_str("  (parser\n");
    out.push_str("    (string_quote \")\n");
    out.push_str("    (space_in_quoted_tokens on)\n");
    out.push_str("    (host_cad \"KiCad's Pcbnew\")\n");
    out.push_str("    (host_version \"gordian\")\n");
    out.push_str("  )\n");
    out.push_str("  (resolution um 10)\n");
    out.push_str("  (unit um)\n");

    // Structure: layers, boundary, default rule.
    structure_section(&mut out, board, layer_names, &planes);

    // Placement + library are built together (placement references image ids,
    // the library defines those images and their padstacks).
    let (images, padstacks) = collect_images(ast, layer_names, board);
    placement_section(&mut out, ast, &images);
    library_section(&mut out, &images, &padstacks);

    // Network + classes (plane nets are poured, not routed → excluded here).
    network_section(&mut out, ast, board, &planes);

    // Empty wiring — Freerouting routes from scratch.
    out.push_str("  (wiring\n");
    out.push_str("  )\n");

    out.push_str(")\n");
    out
}

/// `(structure ...)`: signal/power layers, board boundary polygon, default rule.
fn structure_section(
    out: &mut String,
    board: &BoardProblem,
    layer_names: &[String],
    planes: &PlaneAssignment,
) {
    out.push_str("  (structure\n");
    // A layer carrying a plane net is declared `(type power)`; the rest are signal.
    let plane_layers: std::collections::BTreeSet<&String> = planes.values().collect();
    for (idx, name) in layer_names.iter().enumerate() {
        let ty = if plane_layers.contains(name) {
            "power"
        } else {
            "signal"
        };
        let _ = writeln!(
            out,
            "    (layer \"{name}\"\n      (type {ty})\n      (property\n        (index {idx})\n      )\n    )"
        );
    }

    // Boundary: the Edge.Cuts bbox, in um, y-negated. A rectangle path is enough
    // for the rectangular outlines these boards use.
    let b = &board.problem.bounds;
    let (x0, x1) = (b.min_x * UM_PER_MM, b.max_x * UM_PER_MM);
    // y-down → y-up: negate, so min_y(top) becomes the larger (positive) value.
    let (y0, y1) = (-b.min_y * UM_PER_MM, -b.max_y * UM_PER_MM);
    let _ = writeln!(
        out,
        "    (boundary\n      (path pcb 0 {} {} {} {} {} {} {} {} {} {})\n    )",
        fmt(x0),
        fmt(y0),
        fmt(x1),
        fmt(y0),
        fmt(x1),
        fmt(y1),
        fmt(x0),
        fmt(y1),
        fmt(x0),
        fmt(y0),
    );

    // Plane polygons: one full-board rectangle per power net on its layer. Pins on
    // a plane net connect to it via a via; signal layers stay free of those traces.
    let mut plane_nets: Vec<(&String, &String)> = planes.iter().collect();
    plane_nets.sort();
    for (net, layer) in plane_nets {
        let _ = writeln!(
            out,
            "    (plane \"{net}\"\n      (polygon \"{layer}\" 0 {} {} {} {} {} {} {} {})\n    )",
            fmt(x0),
            fmt(y0),
            fmt(x1),
            fmt(y0),
            fmt(x1),
            fmt(y1),
            fmt(x0),
            fmt(y1),
        );
    }

    // Default rule: width + clearance in um.
    //
    // CLEARANCE GOTCHA #1: a bare `(clearance N)` in Freerouting applies only to the
    // *default* (wire↔wire) class. Pad↔wire (`smd_to_turn_gap`), via↔wire, and
    // SMD-neighbour gaps use SEPARATE clearance entries that default to a SMALLER
    // value if unspecified. KiCAD's own `.dsn` exporter emits one `(clearance N
    // (type ...))` per class; we mirror that so every adjacency honours clearance.
    //
    // GOTCHA #2 (the real killer on a dense board): Freerouting routes to its own
    // grid and consistently lands traces a hair INSIDE the declared gap — KiCAD then
    // reads ~half the intended clearance (e.g. 0.105 for a 0.2 target) and the dense
    // BGA fanout fills with clearance/short faults. We over-declare the export
    // clearance by [`DSN_CLEARANCE_FACTOR`] so Freerouting's actual output clears
    // KiCAD's REAL rule. The oracle and `.kicad_pro` still judge at the true value;
    // this only makes Freerouting route more conservatively (a clean route beats a
    // dense dirty one — the project's connectivity-honest principle).
    let width_um = board.problem.min_trace_width * UM_PER_MM;
    let clear_um = (board.problem.clearance * UM_PER_MM * DSN_CLEARANCE_FACTOR).ceil();
    let _ = writeln!(out, "    (rule");
    let _ = writeln!(out, "      (width {})", fmt(width_um));
    let _ = writeln!(out, "      (clearance {})", fmt(clear_um));
    // Per-type clearances (KiCAD DSN exporter set). `(clearance N (type a_b))`.
    for ty in [
        "default_smd",
        "smd_smd",
        "smd_to_turn_gap",
        "smd_via",
        "via_via",
        "via_wire",
        "wire_wire",
        "wire_via",
        "via_smd",
    ] {
        let _ = writeln!(out, "      (clearance {} (type {ty}))", fmt(clear_um));
    }
    out.push_str("    )\n");

    // Allow vias to land directly on SMD pads (via-in-pad). Landlocked inner BGA
    // balls have no in-plane channel out, so their only escape is a via dropped
    // ON the ball pad to an inner/back layer. Without this, Freerouting leaves
    // every inner ball unrouted.
    out.push_str("    (control\n      (via_at_smd on)\n    )\n");

    out.push_str("  )\n");
}

/// `(placement ...)`: one `(component IMAGE (place REF x y side rot))` per part,
/// grouped by image so each image lists its instances.
fn placement_section(out: &mut String, ast: &PcbAst, images: &BTreeMap<String, Image>) {
    out.push_str("  (placement\n");

    // Group placements under their image id.
    let mut by_image: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for fp in &ast.footprints {
        let Some(lib_id) = &fp.lib_id else { continue };
        let reference = footprint_ref(fp);
        if reference.is_empty() {
            continue;
        }
        let image_id = sanitize(lib_id);
        if !images.contains_key(&image_id) {
            continue;
        }
        let [x, y] = fp.at.unwrap_or([0.0, 0.0]);
        let rot = fp.rotation.unwrap_or(0.0);
        // F.Cu component → front, B.Cu → back.
        let side = match fp.layer.as_deref() {
            Some("B.Cu") => "back",
            _ => "front",
        };
        let line = format!(
            "    (place \"{}\" {} {} {} {})",
            reference,
            fmt(x * UM_PER_MM),
            fmt(-y * UM_PER_MM),
            side,
            fmt(norm_rot(rot)),
        );
        by_image.entry(image_id).or_default().push(line);
    }

    for (image_id, places) in by_image {
        let _ = writeln!(out, "    (component \"{image_id}\"");
        for p in places {
            let _ = writeln!(out, "  {p}");
        }
        out.push_str("    )\n");
    }
    out.push_str("  )\n");
}

/// `(library ...)`: one `(image ...)` per distinct footprint and every distinct
/// `(padstack ...)` referenced.
fn library_section(out: &mut String, images: &BTreeMap<String, Image>, padstacks: &[Padstack]) {
    out.push_str("  (library\n");
    for image in images.values() {
        let _ = writeln!(out, "    (image \"{}\"", image.id);
        for (num, padstack, dx, dy) in &image.pins {
            let _ = writeln!(out, "      (pin \"{padstack}\" \"{num}\" {} {})", fmt(*dx), fmt(*dy));
        }
        out.push_str("    )\n");
    }
    for ps in padstacks {
        let _ = writeln!(out, "    (padstack \"{}\"", ps.id);
        for line in &ps.shape_lines {
            let _ = writeln!(out, "      {line}");
        }
        // `attach on` lets Freerouting land this stack ON an SMD pad (via-in-pad),
        // the only escape for landlocked inner BGA balls. Pad padstacks keep
        // `attach off` (a via must not sit on a *different* component's pad).
        if ps.id == "via_default" {
            out.push_str("      (attach on)\n");
        } else {
            out.push_str("      (attach off)\n");
        }
        out.push_str("    )\n");
    }
    out.push_str("  )\n");
}

/// `(network ...)` + `(class ...)`: each named net lists its `component-pin`
/// members. Two classes: `default` routes the signal nets; each plane net gets a
/// class assigned to its plane layer so its pins connect to the pour (not traces).
fn network_section(out: &mut String, ast: &PcbAst, board: &BoardProblem, planes: &PlaneAssignment) {
    // Net name → list of "REF-PADNUM" pins.
    let mut by_net: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for fp in &ast.footprints {
        let reference = footprint_ref(fp);
        if reference.is_empty() {
            continue;
        }
        for pad in &fp.pads {
            let (Some(num), Some(net)) = (&pad.number, &pad.net) else {
                continue;
            };
            let (Some(code), Some(name)) = (net.code, net.name.clone()) else {
                continue;
            };
            if code == 0 || name.is_empty() {
                continue;
            }
            by_net
                .entry(name)
                .or_default()
                .push(format!("\"{reference}\"-\"{num}\""));
        }
    }

    out.push_str("  (network\n");
    let mut net_names: Vec<&String> = by_net.keys().collect();
    net_names.sort();
    for name in &net_names {
        // A net needs >= 2 pins to route; singletons are skipped (nothing to join).
        let pins = &by_net[*name];
        if pins.len() < 2 {
            continue;
        }
        let _ = writeln!(out, "    (net \"{name}\"");
        let _ = writeln!(out, "      (pins {})", pins.join(" "));
        out.push_str("    )\n");
    }

    let width_um = board.problem.min_trace_width * UM_PER_MM;
    // Over-declare clearance to Freerouting (see DSN_CLEARANCE_FACTOR); the per-class
    // rule must agree with the structure rule or Freerouting uses the looser one.
    let clear_um = (board.problem.clearance * UM_PER_MM * DSN_CLEARANCE_FACTOR).ceil();

    // Default class: every SIGNAL net (>= 2 pins, not a plane net) + the rule.
    out.push_str("    (class default");
    for name in &net_names {
        if by_net[*name].len() >= 2 && !planes.contains_key(*name) {
            let _ = write!(out, " \"{name}\"");
        }
    }
    out.push('\n');
    let _ = writeln!(out, "      (circuit\n        (use_via via_default)\n      )");
    let _ = writeln!(
        out,
        "      (rule\n        (width {})\n        (clearance {})\n      )",
        fmt(width_um),
        fmt(clear_um),
    );
    out.push_str("    )\n");

    // One class per plane net, bound to its plane layer. Freerouting connects each
    // plane-net pin to the pour with a via instead of routing point-to-point traces.
    let mut plane_nets: Vec<(&String, &String)> = planes.iter().collect();
    plane_nets.sort();
    for (net, layer) in plane_nets {
        // Only emit if the plane net actually has pins on the board.
        if by_net.get(net.as_str()).map(|p| p.len()).unwrap_or(0) < 1 {
            continue;
        }
        let class = format!("plane_{net}");
        let _ = writeln!(out, "    (class \"{class}\" \"{net}\"");
        let _ = writeln!(out, "      (circuit\n        (use_via via_default)\n      )");
        let _ = writeln!(out, "      (use_layer \"{layer}\")");
        let _ = writeln!(
            out,
            "      (rule\n        (width {})\n        (clearance {})\n      )",
            fmt(width_um),
            fmt(clear_um),
        );
        out.push_str("    )\n");
    }

    out.push_str("  )\n");
}

/// Build the per-lib_id image library and the deduplicated padstack table, plus
/// inject a `via_default` padstack (round, board via diameter) on all layers.
fn collect_images(
    ast: &PcbAst,
    layer_names: &[String],
    board: &BoardProblem,
) -> (BTreeMap<String, Image>, Vec<Padstack>) {
    let mut images: BTreeMap<String, Image> = BTreeMap::new();
    // padstack id → shape lines, deduped.
    let mut padstacks: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for fp in &ast.footprints {
        let Some(lib_id) = &fp.lib_id else { continue };
        if footprint_ref(fp).is_empty() {
            continue;
        }
        let image_id = sanitize(lib_id);
        if images.contains_key(&image_id) {
            continue; // first instance defines the image
        }

        let mut pins = Vec::new();
        for pad in &fp.pads {
            let Some(num) = &pad.number else { continue };
            let [dx, dy] = pad.at.unwrap_or([0.0, 0.0]);
            // Pad offset is in the footprint's unrotated frame (Freerouting
            // applies the component rotation). Y negated for y-up.
            let (px, py) = (dx * UM_PER_MM, -dy * UM_PER_MM);

            let (ps_id, ps_lines) = padstack_for(pad, layer_names);
            padstacks.entry(ps_id.clone()).or_insert(ps_lines);
            pins.push((num.clone(), ps_id, px, py));
        }

        images.insert(
            image_id.clone(),
            Image {
                id: image_id,
                pins,
            },
        );
    }

    // The default via padstack — a round pad of the board via diameter on every
    // copper layer.
    let via_d_um = board.problem.via_diameter * UM_PER_MM;
    let mut via_lines = Vec::new();
    for name in layer_names {
        via_lines.push(format!("(shape\n        (circle \"{name}\" {})\n      )", fmt(via_d_um)));
    }
    padstacks
        .entry("via_default".to_owned())
        .or_insert(via_lines);

    let padstacks: Vec<Padstack> = padstacks
        .into_iter()
        .map(|(id, shape_lines)| Padstack { id, shape_lines })
        .collect();

    (images, padstacks)
}

/// Build the padstack id + shape lines for a pad. Identical shapes (same kind /
/// size / layer-set) share one id so the library stays small.
fn padstack_for(pad: &PcbPad, layer_names: &[String]) -> (String, Vec<String>) {
    let [w, h] = pad.size.unwrap_or([0.0, 0.0]);
    let (wu, hu) = (w * UM_PER_MM, h * UM_PER_MM);
    let shape = pad.shape.as_deref().unwrap_or("circle");

    // Which copper layers this pad occupies.
    let layers = pad_copper_layers(pad, layer_names);

    // Distinct shape body per layer (Freerouting wants one shape line per layer
    // the pad lives on). The id encodes kind+size+layer-count so dedup is sound.
    let (kind, shape_for) : (&str, Box<dyn Fn(&str) -> String>) = match shape {
        "circle" => (
            "circ",
            Box::new(move |layer: &str| format!("(shape\n        (circle \"{layer}\" {})\n      )", fmt(wu))),
        ),
        "oval" => {
            // Approximate an oval as a stadium via a path of its long axis with
            // the short-axis width — Freerouting accepts a 2-point path shape.
            // Simpler + robust: emit a rectangle bbox (slightly conservative).
            (
                "oval",
                Box::new(move |layer: &str| {
                    rect_shape(layer, wu, hu)
                }),
            )
        }
        "roundrect" | "rect" | _ => (
            if shape == "roundrect" { "rrect" } else { "rect" },
            Box::new(move |layer: &str| rect_shape(layer, wu, hu)),
        ),
    };

    let id = format!("{kind}_{}x{}_{}", fmt(wu), fmt(hu), layers.len());
    let lines: Vec<String> = layers.iter().map(|l| shape_for(l)).collect();
    (id, lines)
}

/// A Specctra rectangle shape on `layer`, centered at origin, `wu`×`hu` um.
fn rect_shape(layer: &str, wu: f64, hu: f64) -> String {
    let (hx, hy) = (wu / 2.0, hu / 2.0);
    format!(
        "(shape\n        (rect \"{layer}\" {} {} {} {})\n      )",
        fmt(-hx),
        fmt(-hy),
        fmt(hx),
        fmt(hy),
    )
}

/// Copper layers a pad occupies, as KiCAD names. `*.Cu` / through-hole → all.
fn pad_copper_layers(pad: &PcbPad, layer_names: &[String]) -> Vec<String> {
    if pad.layers.iter().any(|l| l == "*.Cu") {
        return layer_names.to_vec();
    }
    let mut out: Vec<String> = pad
        .layers
        .iter()
        .filter(|l| l.ends_with(".Cu"))
        .cloned()
        .collect();
    if out.is_empty() {
        // SMD pad with no concrete copper named — assume front.
        out.push(
            layer_names
                .first()
                .cloned()
                .unwrap_or_else(|| "F.Cu".to_owned()),
        );
    }
    out
}

// ── SES import ─────────────────────────────────────────────────────────────────

/// Parse the Specctra `.ses` Freerouting writes at `ses_path` into geometry in
/// **mm**, KiCAD y-down space. Width/diameter come from the `.ses` itself;
/// `board` supplies the net-name set (the `.ses` net tokens are KiCAD names) and
/// a via-drill fallback.
pub fn import_ses(ses_path: &Path, board: &BoardProblem) -> Result<RoutedGeometry, FreerouteError> {
    let text = std::fs::read_to_string(ses_path)?;
    let doc = parse_one(&text).map_err(|e| FreerouteError::Parse(e.to_string()))?;
    let root = doc
        .nodes
        .first()
        .ok_or_else(|| FreerouteError::Parse("empty .ses".to_owned()))?;

    // The `.ses` declares its own scale, e.g. `(resolution um 10)` means 1
    // file-unit = um/10 = 0.1um (the classic Specctra gotcha — output is at 10×
    // the um input). Read it so coordinate scaling is exact, not hard-coded.
    let mm_div = ses_mm_divisor(root);

    // Via padstack id → diameter (mm), so a `via PADSTACK x y` resolves its size.
    let via_diam = via_padstack_diameters(root, mm_div);
    let default_drill = board.problem.via_drill;
    let default_via_d = board.problem.via_diameter;

    let mut geo = RoutedGeometry::default();

    // Walk to (routes (network_out (net NAME (wire ...)(via ...)) ...)).
    if let Some(routes) = find_child(root, "routes")
        && let Some(network_out) = find_child(routes, "network_out") {
            for net_node in children_named(network_out, "net") {
                let net_name = match first_atom_string(net_node) {
                    Some(n) => n,
                    None => continue,
                };
                // Each net carries wires + vias.
                for wire in children_named(net_node, "wire") {
                    if let Some(w) = parse_wire(wire, &net_name, mm_div) {
                        geo.wires.push(w);
                    }
                }
                for via in children_named(net_node, "via") {
                    if let Some(v) = parse_via(
                        via,
                        &net_name,
                        &via_diam,
                        default_via_d,
                        default_drill,
                        mm_div,
                    ) {
                        geo.vias.push(v);
                    }
                }
            }
        }

    Ok(geo)
}

/// The divisor from `.ses` file-units to **mm**, read from `(resolution UNIT N)`.
/// For `(resolution um 10)` a file-unit is um/10, so mm = value / (10 × 1000) =
/// value / 10000. Defaults to 10000 (Freerouting's usual `um 10`) if absent.
fn ses_mm_divisor(root: &Node) -> f64 {
    // The resolution can sit at the top level or inside the placement section.
    let res = find_child(root, "resolution")
        .or_else(|| find_child(root, "placement").and_then(|p| find_child(p, "resolution")));
    let Some(res) = res else { return 10_000.0 };
    let Some(items) = list_items(res) else {
        return 10_000.0;
    };
    // (resolution UNIT N): items = ["resolution", unit, n]
    let unit = items.get(1).and_then(atom_str).unwrap_or_default();
    let n = items.get(2).and_then(node_f64).unwrap_or(10.0);
    let um_per_unit = match unit.as_str() {
        "um" => 1.0,
        "mm" => 1000.0,
        "inch" => 25_400.0,
        "mil" => 25.4,
        _ => 1.0,
    };
    // file-unit = um_per_unit / n micrometres; mm = value × (um_per_unit / n) / 1000.
    (n / um_per_unit) * 1000.0
}

/// `(via PADSTACK x y)` → a `RoutedVia`. Diameter resolves from the padstack
/// table, falling back to the board via diameter.
fn parse_via(
    node: &Node,
    net: &str,
    via_diam: &BTreeMap<String, f64>,
    default_via_d: f64,
    default_drill: f64,
    mm_div: f64,
) -> Option<RoutedVia> {
    let items = list_items(node)?;
    // items[0] = "via", items[1] = padstack id, items[2..] = x y [...]
    let padstack = atom_str(items.get(1)?)?;
    let nums: Vec<f64> = items[2..].iter().filter_map(node_f64).collect();
    let (x, y) = (*nums.first()?, *nums.get(1)?);
    let diameter_mm = via_diam.get(&padstack).copied().unwrap_or(default_via_d);
    Some(RoutedVia {
        net: net.to_owned(),
        at: Point2 {
            x: x / mm_div,
            y: -y / mm_div,
        },
        diameter_mm,
        drill_mm: default_drill,
    })
}

/// `(wire (path LAYER WIDTH x1 y1 x2 y2 ...) (net NAME))` → a `RoutedWire`.
fn parse_wire(node: &Node, net: &str, mm_div: f64) -> Option<RoutedWire> {
    let path = find_child(node, "path")?;
    let items = list_items(path)?;
    // items: "path", layer, width, then coordinate pairs.
    let layer = atom_str(items.get(1)?)?;
    let width = node_f64(items.get(2)?)?;
    let coords: Vec<f64> = items[3..].iter().filter_map(node_f64).collect();
    if coords.len() < 4 {
        return None;
    }
    let mut pts = Vec::new();
    let mut i = 0;
    while i + 1 < coords.len() {
        pts.push(Point2 {
            x: coords[i] / mm_div,
            y: -coords[i + 1] / mm_div,
        });
        i += 2;
    }
    Some(RoutedWire {
        net: net.to_owned(),
        layer,
        width_mm: width / mm_div,
        path: pts,
    })
}

/// Scan the `.ses` library for via padstacks → finished diameter (mm). The `.ses`
/// echoes back the padstack defs; a via padstack's diameter is the circle/rect
/// extent of its shape.
fn via_padstack_diameters(root: &Node, mm_div: f64) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    let Some(library) = find_child(root, "library_out").or_else(|| find_child(root, "library"))
    else {
        return out;
    };
    for ps in children_named(library, "padstack") {
        let Some(id) = first_atom_string(ps) else {
            continue;
        };
        // Diameter = max extent of any shape in the padstack.
        let mut d = 0.0_f64;
        for shape in children_named(ps, "shape") {
            if let Some(circle) = find_child(shape, "circle")
                && let Some(items) = list_items(circle) {
                    // (circle LAYER DIAMETER [x y])
                    if let Some(dia) = items.get(2).and_then(node_f64) {
                        d = d.max(dia / mm_div);
                    }
                }
        }
        if d > 0.0 {
            out.insert(id, d);
        }
    }
    out
}

// ── orchestration ──────────────────────────────────────────────────────────────

/// Path to the vendored Freerouting jar, relative to the workspace root.
fn jar_path() -> std::path::PathBuf {
    if let Ok(j) = std::env::var("FREEROUTING_JAR") {
        return std::path::PathBuf::from(j);
    }
    // CARGO_MANIFEST_DIR for specctra is <root>/crates/specctra.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(manifest)
        .join("tools/vendor/freerouting.jar")
}

/// Route a placed board end-to-end through Freerouting against explicit
/// [`RouteRules`] (the board's true fine rules), with default passes/timeout.
///
/// Routing coverage is fixed in the first pass; further passes only *optimize*
/// (shorten) the wiring, with diminishing returns. We cap at 10 passes — enough to
/// tidy the route without the 100-pass optimization blowing the timeout on a dense
/// BGA — and allow 10 minutes (a 100-ball via-in-pad escape is slow).
pub fn freeroute_with_rules(
    board_path: &Path,
    rules: RouteRules,
) -> Result<RoutedGeometry, FreerouteError> {
    freeroute_with(board_path, rules, 10, Duration::from_secs(600))
}

/// [`freeroute_with_rules`] with explicit max-passes and timeout (used by tests/tools).
pub fn freeroute_with(
    board_path: &Path,
    rules: RouteRules,
    max_passes: u32,
    timeout: Duration,
) -> Result<RoutedGeometry, FreerouteError> {
    let mut board = read_problem(board_path)?;
    rules.apply(&mut board);

    let jar = jar_path();
    if !jar.exists() {
        return Err(FreerouteError::JarMissing(format!(
            "{} (run tools/vendor/fetch_freerouting.sh)",
            jar.display()
        )));
    }
    if which("xvfb-run").is_none() {
        return Err(FreerouteError::ToolMissing(
            "xvfb-run not on PATH (install xvfb)".to_owned(),
        ));
    }
    if which("java").is_none() {
        return Err(FreerouteError::ToolMissing("java not on PATH".to_owned()));
    }

    // Work in a temp dir alongside the board so paths are simple.
    let dir = tempfile::Builder::new()
        .prefix("gordian-freeroute-")
        .tempdir()?;
    let dsn = dir.path().join("board.dsn");
    let ses = dir.path().join("board.ses");

    export_dsn_with_rules(board_path, &dsn, rules)?;

    let output = run_freerouting(&jar, &dsn, &ses, max_passes, timeout)?;

    if !ses.exists() {
        return Err(FreerouteError::RunFailed(format!(
            "freerouting produced no .ses. stderr:\n{}",
            output
        )));
    }

    import_ses(&ses, &board)
}

/// Invoke `xvfb-run -a java -jar JAR -de DSN -do SES -mp PASSES`. Returns the
/// combined stdout+stderr (for diagnostics). A nonzero exit is tolerated as long
/// as a `.ses` was written (Freerouting sometimes exits oddly under xvfb but
/// still produces output); the caller checks for the `.ses`.
fn run_freerouting(
    jar: &Path,
    dsn: &Path,
    ses: &Path,
    max_passes: u32,
    timeout: Duration,
) -> Result<String, FreerouteError> {
    let java = std::env::var("FREEROUTING_JAVA").unwrap_or_else(|_| "java".to_string());
    let mut child = Command::new("xvfb-run")
        .arg("-a")
        .arg(java)
        .arg("-jar")
        .arg(jar)
        .arg("-de")
        .arg(dsn)
        .arg("-do")
        .arg(ses)
        .arg("-mp")
        .arg(max_passes.to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| FreerouteError::RunFailed(format!("spawn xvfb-run: {e}")))?;

    // Poll for completion up to the timeout.
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    // A .ses may already exist from earlier passes; let the caller decide.
                    return Ok(format!("freerouting timed out after {:?}", timeout));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(FreerouteError::RunFailed(format!("wait: {e}"))),
        }
    }

    let out = child
        .wait_with_output()
        .map_err(|e| FreerouteError::RunFailed(format!("collect output: {e}")))?;
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(combined)
}

/// Find an executable on PATH (lightweight `which`).
fn which(prog: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(prog);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ── s-expr helpers ─────────────────────────────────────────────────────────────

fn list_items(node: &Node) -> Option<&[Node]> {
    match node {
        Node::List { items, .. } => Some(items),
        _ => None,
    }
}

/// The head symbol of a list node, e.g. `"net"` for `(net ...)`.
fn head(node: &Node) -> Option<String> {
    let items = list_items(node)?;
    atom_str(items.first()?)
}

fn atom_str(node: &Node) -> Option<String> {
    match node {
        Node::Atom { atom, .. } => Some(match atom {
            Atom::Symbol(s) | Atom::Quoted(s) => s.clone(),
        }),
        _ => None,
    }
}

/// The first atom *after* the head (e.g. the net name in `(net "GND" ...)`).
fn first_atom_string(node: &Node) -> Option<String> {
    let items = list_items(node)?;
    atom_str(items.get(1)?)
}

fn node_f64(node: &Node) -> Option<f64> {
    match node {
        Node::Atom {
            atom: Atom::Symbol(s),
            ..
        } => s.parse().ok(),
        _ => None,
    }
}

/// First direct child list whose head is `name`.
fn find_child<'a>(node: &'a Node, name: &str) -> Option<&'a Node> {
    let items = list_items(node)?;
    items.iter().find(|c| head(c).as_deref() == Some(name))
}

/// All direct child lists whose head is `name`.
fn children_named<'a>(node: &'a Node, name: &str) -> Vec<&'a Node> {
    list_items(node)
        .map(|items| {
            items
                .iter()
                .filter(|c| head(c).as_deref() == Some(name))
                .collect()
        })
        .unwrap_or_default()
}

// ── formatting helpers ─────────────────────────────────────────────────────────

/// Format a um coordinate: round to the grid (resolution um 10 → 0.1 um), drop a
/// trailing `.0`, and collapse `-0` to `0`.
fn fmt(v: f64) -> String {
    let r = (v * 10.0).round() / 10.0;
    let r = if r == 0.0 { 0.0 } else { r };
    if r.fract() == 0.0 {
        format!("{}", r as i64)
    } else {
        format!("{r}")
    }
}

/// Normalize a rotation into `[0, 360)`.
fn norm_rot(deg: f64) -> f64 {
    let r = deg % 360.0;
    if r < 0.0 {
        r + 360.0
    } else {
        r
    }
}

/// Reference designator of a footprint (handles the `reference` field or a
/// `Reference` property).
fn footprint_ref(fp: &PcbFootprint) -> String {
    fp.reference
        .clone()
        .or_else(|| {
            fp.properties
                .iter()
                .find(|p| p.key == "Reference")
                .map(|p| p.value.clone())
        })
        .unwrap_or_default()
}

/// Replace Specctra-unsafe characters in an id (spaces are allowed because we
/// quote, but keep ids simple). lib_ids contain `:` which is fine quoted.
fn sanitize(s: &str) -> String {
    s.to_owned()
}

fn map_kiutils_err(e: kiutils_kicad::Error) -> io::Error {
    match e {
        kiutils_kicad::Error::Io(io) => io,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_rounds_to_grid_and_collapses_neg_zero() {
        assert_eq!(fmt(0.0), "0");
        assert_eq!(fmt(-0.0), "0");
        assert_eq!(fmt(1000.0), "1000");
        assert_eq!(fmt(1234.56), "1234.6");
    }

    #[test]
    fn norm_rot_wraps() {
        assert_eq!(norm_rot(0.0), 0.0);
        assert_eq!(norm_rot(90.0), 90.0);
        assert_eq!(norm_rot(-90.0), 270.0);
        assert_eq!(norm_rot(450.0), 90.0);
    }

    #[test]
    fn parse_wire_negates_y_and_scales() {
        // `.ses` units at `um 10` = 0.1um, so mm_div = 10000.
        let text = r#"(wire (path F.Cu 2500 10000 -20000 30000 -40000) (net GND))"#;
        let doc = parse_one(text).unwrap();
        let node = doc.nodes.first().unwrap();
        let w = parse_wire(node, "GND", 10_000.0).unwrap();
        assert_eq!(w.layer, "F.Cu");
        assert_eq!(w.width_mm, 0.25);
        // y is negated back to KiCAD y-down: -(-20000)/10000 = 2.0
        assert_eq!(w.path[0], Point2 { x: 1.0, y: 2.0 });
        assert_eq!(w.path[1], Point2 { x: 3.0, y: 4.0 });
    }

    #[test]
    fn ses_resolution_um10_is_10000_per_mm() {
        let text = "(session s (resolution um 10))";
        let doc = parse_one(text).unwrap();
        let root = doc.nodes.first().unwrap();
        assert_eq!(ses_mm_divisor(root), 10_000.0);
    }

    #[test]
    fn ses_resolution_defaults_when_absent() {
        let text = "(session s)";
        let doc = parse_one(text).unwrap();
        let root = doc.nodes.first().unwrap();
        assert_eq!(ses_mm_divisor(root), 10_000.0);
    }

    #[test]
    fn parse_via_negates_y_and_resolves_diameter() {
        let text = "(via via_default 487819 -513009)";
        let doc = parse_one(text).unwrap();
        let node = doc.nodes.first().unwrap();
        let mut diam = BTreeMap::new();
        diam.insert("via_default".to_owned(), 0.5);
        let v = parse_via(node, "GND", &diam, 0.6, 0.3, 10_000.0).unwrap();
        assert_eq!(v.net, "GND");
        // 487819 / 10000 = 48.7819 ; y negated: -(-513009)/10000 = 51.3009
        assert!((v.at.x - 48.7819).abs() < 1e-6, "{:?}", v.at);
        assert!((v.at.y - 51.3009).abs() < 1e-6, "{:?}", v.at);
        assert_eq!(v.diameter_mm, 0.5); // resolved from padstack table
        assert_eq!(v.drill_mm, 0.3); // board default
    }

    /// The full `.ses` network walk: a `routes/network_out/net` tree yields wires
    /// (with negated Y) attributed to the right net.
    #[test]
    fn import_ses_walks_network_out() {
        let ses = r#"(session s
  (routes
    (resolution um 10)
    (network_out
      (net SA3
        (wire (path F.Cu 1000 100000 -200000 100000 -300000) (net SA3))
      )
      (net GND
        (via via_default 150000 -250000)
      )
    )
  )
)"#;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.ses");
        std::fs::write(&p, ses).unwrap();
        // Minimal board (only the fields import_ses reads: via defaults).
        let board = BoardProblem {
            problem: pcb_model::RouteProblem {
                layer_count: 2,
                min_trace_width: 0.1,
                obstacles: vec![],
                connections: vec![],
                bounds: pcb_model::Bounds {
                    min_x: 0.0,
                    max_x: 10.0,
                    min_y: 0.0,
                    max_y: 10.0,
                },
                clearance: 0.1,
                via_diameter: 0.6,
                via_drill: 0.3,
                net_widths: BTreeMap::new(),
                outline: None,
                escape_layers: Default::default(),
            },
            net_codes: BTreeMap::new(),
            layer_names: vec!["F.Cu".into(), "B.Cu".into()],
        };
        let geo = import_ses(&p, &board).unwrap();
        assert_eq!(geo.wires.len(), 1);
        assert_eq!(geo.wires[0].net, "SA3");
        assert_eq!(geo.wires[0].width_mm, 0.1);
        // y negated: -(-200000)/10000 = 20.0
        assert_eq!(geo.wires[0].path[0], Point2 { x: 10.0, y: 20.0 });
        assert_eq!(geo.vias.len(), 1);
        assert_eq!(geo.vias[0].net, "GND");
        assert_eq!(geo.vias[0].at, Point2 { x: 15.0, y: 25.0 });
    }

    /// If a placed harness board is present, `export_dsn_with_rules` produces a structurally
    /// valid `.dsn` (header, layers, boundary, placement, library, network). Skipped
    /// when the board is absent (CI without the harness).
    #[test]
    fn export_dsn_structure_smoke() {
        let board = Path::new("/tmp/pcb-harness/bga-escape-fineclear/board.kicad_pcb");
        if !board.exists() {
            eprintln!("skip: harness board absent");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let dsn = dir.path().join("b.dsn");
        let rules = RouteRules::from_board(&read_problem(board).unwrap());
        export_dsn_with_rules(board, &dsn, rules).unwrap();
        let text = std::fs::read_to_string(&dsn).unwrap();
        for needle in [
            "(pcb gordian",
            "(resolution um 10)",
            "(structure",
            "(boundary",
            "(placement",
            "(library",
            "(network",
            "(wiring",
        ] {
            assert!(text.contains(needle), "DSN missing {needle}");
        }
        // The DSN must be re-parseable as one s-expr.
        assert!(parse_one(&text).is_ok(), "DSN is not valid s-expression");
    }
}
