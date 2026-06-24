//! Board **synthesis**: BUILD a `.kicad_pcb` from scratch out of footprint
//! `.kicad_mod` bodies, an engine placement, and a board outline — the
//! agent-flow companion to [`crate::placefp::move_footprints`].
//!
//! Synthesis is shaped as an **engine SDK**: a [`Synthesizer`] consumes ONE
//! self-contained [`BoardModel`] value and RETURNS the emitted text, so the
//! KiCAD-9 emitter ([`KicadV9Synth`]) is one swappable implementation behind a
//! clean seam — a third party targets a different EDA format / KiCAD version /
//! golden-test dumper by implementing [`Synthesizer`] for the same model, with
//! the placement / route / DRC stages unchanged.
//!
//! `move_footprints` re-seats a hand-authored *template* board; synthesis has no
//! template — the agent declares parts (footprint + pad→net) and the engine
//! places them, so we assemble the board text directly. The output is structured
//! to match the checked-in `tests/fixtures/placed_template.kicad_pcb` (the
//! reference KiCAD-9 board): the same version header, `general`/`paper`/`layers`/
//! `setup` skeleton, a `(net …)` table, an `Edge.Cuts` rectangle, and one
//! `(footprint …)` block per part with `(at …)` and per-pad `(net …)` bindings.
//!
//! ## Why text-splice the `.kicad_mod` body (not re-emit from the parsed AST)
//!
//! The slice-4 coherence invariant is that the PlaceProblem and the board derive
//! from ONE description: the pads the router targets MUST be byte-for-byte the
//! pads KiCAD sees, or `move`/synthesis lands copper a pad does not reach and DRC
//! reports it unconnected. The `.kicad_mod` *is* that one description (the same
//! file [`kicad_sexpr::footlib::Footprint::load`] and `part_from_footprint` read), so we
//! transform its raw text rather than round-tripping through a lossy parsed form
//! (kiutils' footprint `ast_mut` does not round-trip through `write()`, the same
//! limitation [`kicad_sexpr::pcb::write_solution`] documents). The transforms are:
//!
//! 1. Rewrite the `(footprint "NAME" …)` header token to the board `lib_id`.
//! 2. Inject `(at x y [rot])` + a deterministic board-instance `(uuid …)` right
//!    after the footprint header (a `.kicad_mod` has neither).
//! 3. Set the `Reference` property value (`REF**` → the real designator) and put
//!    every property on `F.Fab`.
//! 4. **Drop silkscreen graphics** (`fp_line`/`fp_text`/… on `*.SilkS`) — the
//!    slice-4 `silk_over_copper` pitfall on compact boards. Courtyards
//!    (`*.CrtYd`) and fab (`*.Fab`) graphics are kept as the library drew them.
//! 5. Inject `(net N "name")` into each *bound* pad's s-expression (before the
//!    pad's closing paren, minding the nested parens of `(drill …)`/`(options …)`).
//!
//! Net codes are assigned 1-based over the sorted union of every part's pad nets
//! (net 0 is the reserved no-net), so the board's net table and the pad bindings
//! agree by construction — the same coherence guarantee, now writer-enforced.
//!
//! ## Rotation
//!
//! Engine v1 placements are rotation-0 unless a hint/lock set one. KiCAD encodes
//! footprint rotation on the footprint-level `(at x y rot)`; each pad's own
//! stored rotation is *absolute* (footprint angle folded in), so a rotated
//! footprint needs every pad's `(at … rot)` bumped by the footprint angle — the
//! convention `pad_center` reads back. v1 supports 0/90/180/270
//! (the only values the placer emits) by adding the footprint angle to each pad's
//! `(at)` rotation; any other angle is rejected with a clear error rather than
//! emitting wrong geometry (honest rejection beats a silent short).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;

use kicad_sexpr::fmt_num;
use pcb_model::place::Placement;
use pcb_model::{Bounds, Point2};

use crate::ids::synth_uuid;
use crate::sexpr::{
    bump_pad_rotation, cap_font_size, footprint_body, footprint_inner, inject_before_close,
    node_head, on_silk, pad_number, push_reindented, top_level_nodes,
};
use crate::zone::{push_keepout_zone, push_zone};

pub use crate::zone::{plane_fill_rects, KeepoutZone, ZoneSpec};

/// One part to synthesize onto the board: its board identity, the source
/// `.kicad_mod` text, its pad→net wiring, and where the engine placed it.
#[derive(Debug, Clone)]
pub struct SynthPart {
    /// Schematic reference designator ("R1", "U1", "J1").
    pub reference: String,
    /// Fully-qualified footprint id for the board header
    /// (`"Resistor_SMD:R_0603_1608Metric"`).
    pub lib_id: String,
    /// The raw `.kicad_mod` source text whose footprint body we transform.
    pub source: String,
    /// Pad number → net name. A pad absent from the map is left unconnected.
    pub pad_nets: BTreeMap<String, String>,
    /// Where the engine placed this part (origin + rotation).
    pub placement: Placement,
}

/// A KiCAD net class: a named group of nets sharing one set of design rules
/// (clearance / trace width / via geometry). Emitting these makes the board's
/// per-net intent — fat power vs thin signal — LEGIBLE and editable once the
/// `.kicad_pcb` is opened in KiCAD (board setup → net classes), instead of being
/// baked invisibly into trace widths only. The class is derived from the per-net
/// widths the router already carries (see the export's `net_classes_from_rules`);
/// `members` are the net names assigned to the class via `(add_net …)`.
#[derive(Debug, Clone)]
pub struct NetClass {
    /// Class name as shown in KiCAD (e.g. `"Power"`, `"Signal"`, `"Default"`).
    pub name: String,
    /// Human-readable description string.
    pub description: String,
    /// Copper-to-copper clearance (mm) for nets in this class.
    pub clearance: f64,
    /// Trace width (mm) for nets in this class.
    pub trace_width: f64,
    /// Via copper diameter (mm).
    pub via_diameter: f64,
    /// Via drill diameter (mm).
    pub via_drill: f64,
    /// Net names assigned to this class (emitted as `(add_net …)`), sorted.
    pub members: Vec<String>,
}

/// A fully materialized board, ready for ANY [`Synthesizer`] — the single value
/// the engine consumes. It carries the parts (footprint + placement + pad→net),
/// the board geometry (`bounds`, `layer_count`, optional custom `outline`), the
/// copper `zones` (planes + pours) and routing `keepouts`, and the `net_classes`
/// (the fat-power/thin-signal design-rule groups). A third party targeting a
/// different EDA format implements [`Synthesizer`] against THIS value, so the
/// upstream placement / route / DRC stages need no change.
#[derive(Debug, Clone)]
pub struct BoardModel {
    /// The placed parts, in emit order.
    pub parts: Vec<SynthPart>,
    /// Board extents (also the default rectangular `Edge.Cuts`).
    pub bounds: Bounds,
    /// Copper layer count (2 or 4 — the engine's supported stackups).
    pub layer_count: u32,
    /// Copper-plane / signal-pour zones, emitted before the board close.
    pub zones: Vec<ZoneSpec>,
    /// Routing keep-outs exported as KiCAD rule areas.
    pub keepouts: Vec<KeepoutZone>,
    /// A custom closed-polygon `Edge.Cuts` outline; `None` ⇒ the `bounds` rect.
    pub outline: Option<Vec<Point2>>,
    /// Per-net design-rule groups, emitted as `(net_class …)` blocks.
    pub net_classes: Vec<NetClass>,
}

impl BoardModel {
    /// A board of `parts` on `bounds` with `layer_count` copper layers and no
    /// zones / keep-outs / custom outline / net classes. Set those fields after
    /// construction for a fuller board.
    pub fn new(parts: Vec<SynthPart>, bounds: Bounds, layer_count: u32) -> Self {
        Self {
            parts,
            bounds,
            layer_count,
            zones: Vec::new(),
            keepouts: Vec::new(),
            outline: None,
            net_classes: Vec::new(),
        }
    }
}

/// Build a `.kicad_pcb` (or any target format) from a [`BoardModel`].
///
/// The single seam between the design (a materialized [`BoardModel`]) and its
/// serialized form: the model is engine-agnostic, so a third party emits a
/// different KiCAD version, a JSON dump, or a golden-test format by implementing
/// this trait — the placement / route / DRC stages are unchanged.
///
/// **Determinism contract.** An implementation MUST be deterministic given the
/// `BoardModel` (a model re-emits byte-identically), MUST report every failure
/// it cannot serialize as an [`io::Error`] with a human-readable reason (rather
/// than silently dropping it), MUST emit no artifact when it returns `Err`, and
/// MUST NOT panic.
pub trait Synthesizer {
    /// Open string provenance — the emitter's name (e.g. `"kicad-v9"`).
    fn name(&self) -> &'static str;

    /// Serialize `board` to the target format, or return an [`io::Error`].
    fn emit(&self, board: &BoardModel) -> io::Result<String>;
}

/// The KiCAD-9 `.kicad_pcb` emitter: assembles the board text by string-splicing
/// each part's `.kicad_mod` body (see the module docs) onto the KiCAD-9 board
/// skeleton, then the net table, net classes, edge cuts, zones and keep-outs.
#[derive(Debug, Clone, Copy, Default)]
pub struct KicadV9Synth;

impl Synthesizer for KicadV9Synth {
    fn name(&self) -> &'static str {
        "kicad-v9"
    }

    fn emit(&self, board: &BoardModel) -> io::Result<String> {
        emit_kicad_v9(board)
    }
}

/// Synthesize a complete 2-layer `.kicad_pcb` from `parts` on a board of
/// `bounds`. Convenience over [`KicadV9Synth`] + [`BoardModel`].
pub fn synthesize_board(parts: &[SynthPart], bounds: &Bounds) -> io::Result<String> {
    synthesize_board_layers(parts, bounds, 2)
}

/// Synthesize a complete `.kicad_pcb` from `parts` on a board of `bounds` with
/// `layer_count` copper layers (2 or 4 — the engine's supported stackups).
///
/// The result parses with [`kicad_sexpr::pcb::read_problem`] and is structurally a
/// KiCAD-9 board (see the module docs). Net codes are 1-based over the sorted
/// union of every part's pad nets. Returns an [`io::Error`] if a part's source
/// has no parseable footprint block, a placement is missing for a part, or a
/// non-axis-aligned rotation is requested.
pub fn synthesize_board_layers(
    parts: &[SynthPart],
    bounds: &Bounds,
    layer_count: u32,
) -> io::Result<String> {
    KicadV9Synth.emit(&BoardModel::new(parts.to_vec(), bounds.clone(), layer_count))
}

/// [`synthesize_board_layers`] plus copper-plane `zones` (power pours), routing
/// `keepouts` (rule areas), and `net_classes` (the per-net design-rule groups that
/// make fat-power/thin-signal intent legible in KiCAD), all emitted before the
/// board close. Each zone's net must be one of the parts' nets.
pub fn synthesize_board_full(
    parts: &[SynthPart],
    bounds: &Bounds,
    layer_count: u32,
    zones: &[ZoneSpec],
    keepouts: &[KeepoutZone],
    outline: Option<&[Point2]>,
    net_classes: &[NetClass],
) -> io::Result<String> {
    KicadV9Synth.emit(&BoardModel {
        zones: zones.to_vec(),
        keepouts: keepouts.to_vec(),
        outline: outline.map(<[Point2]>::to_vec),
        net_classes: net_classes.to_vec(),
        ..BoardModel::new(parts.to_vec(), bounds.clone(), layer_count)
    })
}

/// The KiCAD-9 emit kernel: assemble the whole board text from a [`BoardModel`].
fn emit_kicad_v9(board: &BoardModel) -> io::Result<String> {
    let BoardModel { parts, bounds, layer_count, zones, keepouts, outline, net_classes } = board;
    let layer_count = *layer_count;
    let outline = outline.as_deref();

    // Net code table: 1-based over the sorted union of every bound pad's net.
    let net_codes = net_codes(parts);

    let mut out = String::with_capacity(4096 + parts.len() * 1024);
    out.push_str("(kicad_pcb\n");
    out.push_str("\t(version 20241229)\n");
    out.push_str("\t(generator \"autopcb\")\n");
    out.push_str("\t(generator_version \"9.0\")\n");
    out.push_str("\t(general\n\t\t(thickness 1.6)\n\t\t(legacy_teardrops no)\n\t)\n");
    out.push_str("\t(paper \"A4\")\n");
    push_layers(&mut out, layer_count);
    out.push_str(
        "\t(setup\n\t\t(pad_to_mask_clearance 0)\n\
         \t\t(allow_soldermask_bridges_in_footprints no)\n\
         \t\t(aux_axis_origin 0 0)\n\t\t(grid_origin 0 0)\n\t)\n",
    );
    push_nets(&mut out, &net_codes);
    push_net_classes(&mut out, net_classes, &net_codes);
    push_edge_cuts(&mut out, bounds, outline);

    for part in parts {
        let block = synth_footprint(part, &net_codes)?;
        out.push_str(&block);
    }

    for (i, z) in zones.iter().enumerate() {
        let code = net_codes.get(&z.net_name).copied().unwrap_or(0);
        push_zone(&mut out, code, z, i);
    }

    for (i, k) in keepouts.iter().enumerate() {
        push_keepout_zone(&mut out, k, i);
    }

    out.push_str(")\n");
    Ok(out)
}

/// 1-based net codes over the sorted union of every part's pad net names.
fn net_codes(parts: &[SynthPart]) -> BTreeMap<String, i32> {
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for p in parts {
        for net in p.pad_nets.values() {
            if !net.is_empty() {
                names.insert(net.clone());
            }
        }
    }
    names
        .into_iter()
        .enumerate()
        .map(|(i, name)| (name, i as i32 + 1))
        .collect()
}

/// Emit the `(layers …)` declaration: a 2-layer board with the silk/mask/edge
/// technical layers KiCAD 9 expects (matches `placed_template.kicad_pcb`).
fn push_layers(out: &mut String, layer_count: u32) {
    out.push_str("\t(layers\n");
    out.push_str("\t\t(0 \"F.Cu\" signal)\n");
    // Inner copper layers (4-layer stackup): In1.Cu=1, In2.Cu=2, … sequential,
    // with B.Cu following. KiCAD 9 accepts this sequential numbering (verified by
    // load + DRC); the 2-layer board keeps the canonical (0 F.Cu)(2 B.Cu).
    if layer_count >= 4 {
        for i in 1..=(layer_count - 2) {
            let _ = writeln!(out, "\t\t({i} \"In{i}.Cu\" signal)");
        }
        let b = layer_count - 1;
        let _ = writeln!(out, "\t\t({b} \"B.Cu\" signal)");
    } else {
        out.push_str("\t\t(2 \"B.Cu\" signal)\n");
    }
    out.push_str("\t\t(36 \"B.SilkS\" user \"B.Silkscreen\")\n");
    out.push_str("\t\t(37 \"F.SilkS\" user \"F.Silkscreen\")\n");
    out.push_str("\t\t(38 \"B.Mask\" user)\n");
    out.push_str("\t\t(39 \"F.Mask\" user)\n");
    out.push_str("\t\t(44 \"Edge.Cuts\" user)\n");
    out.push_str("\t)\n");
}

/// Emit the `(net 0 "")` reserved no-net plus one `(net code "name")` per named
/// net, in code order.
fn push_nets(out: &mut String, net_codes: &BTreeMap<String, i32>) {
    out.push_str("\t(net 0 \"\")\n");
    let mut by_code: Vec<(&i32, &String)> = net_codes.iter().map(|(n, c)| (c, n)).collect();
    by_code.sort();
    for (code, name) in by_code {
        let _ = writeln!(out, "\t(net {code} \"{name}\")");
    }
}

/// Emit one `(net_class …)` block per class, each carrying its design rules
/// (clearance / trace width / via geometry) and its member nets via `(add_net …)`.
/// Only members that are real nets on this board (present in `net_codes`) are
/// emitted, so a class never references a net the board does not have. KiCAD reads
/// these from the `.kicad_pcb` directly (board setup → net classes) and `kicad-cli
/// pcb drc` honours their clearance/width — making the per-net intent both visible
/// and an independent DRC check, additive to the routed copper.
fn push_net_classes(out: &mut String, classes: &[NetClass], net_codes: &BTreeMap<String, i32>) {
    for c in classes {
        let members: Vec<&String> = c.members.iter().filter(|m| net_codes.contains_key(*m)).collect();
        if members.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\t(net_class \"{}\" \"{}\"", c.name, c.description);
        let _ = writeln!(out, "\t\t(clearance {})", fmt_num(c.clearance));
        let _ = writeln!(out, "\t\t(trace_width {})", fmt_num(c.trace_width));
        let _ = writeln!(out, "\t\t(via_dia {})", fmt_num(c.via_diameter));
        let _ = writeln!(out, "\t\t(via_drill {})", fmt_num(c.via_drill));
        for m in members {
            let _ = writeln!(out, "\t\t(add_net \"{m}\")");
        }
        out.push_str("\t)\n");
    }
}

/// Emit the board outline on `Edge.Cuts`. With `outline = Some(pts)` (≥3 points) the
/// outline is that closed polygon (one `gr_line` per edge) — circle (many points),
/// square, star, any custom shape. Otherwise the `bounds` rectangle (the default).
fn push_edge_cuts(out: &mut String, bounds: &Bounds, outline: Option<&[Point2]>) {
    if let Some(pts) = outline
        && pts.len() >= 3 {
            for i in 0..pts.len() {
                let a = &pts[i];
                let b = &pts[(i + 1) % pts.len()];
                let (x0, y0) = (fmt_num(a.x), fmt_num(a.y));
                let (x1, y1) = (fmt_num(b.x), fmt_num(b.y));
                let uuid = synth_uuid(&format!("edge:{x0}:{y0}:{x1}:{y1}"));
                let _ = write!(
                    out,
                    "\t(gr_line\n\t\t(start {x0} {y0})\n\t\t(end {x1} {y1})\n\
                     \t\t(stroke\n\t\t\t(width 0.1)\n\t\t\t(type default)\n\t\t)\n\
                     \t\t(layer \"Edge.Cuts\")\n\t\t(uuid \"{uuid}\")\n\t)\n"
                );
            }
            return;
        }
    let (x0, y0) = (fmt_num(bounds.min_x), fmt_num(bounds.min_y));
    let (x1, y1) = (fmt_num(bounds.max_x), fmt_num(bounds.max_y));
    let uuid = synth_uuid(&format!("edge:{x0}:{y0}:{x1}:{y1}"));
    let _ = write!(
        out,
        "\t(gr_rect\n\t\t(start {x0} {y0})\n\t\t(end {x1} {y1})\n\
         \t\t(stroke\n\t\t\t(width 0.1)\n\t\t\t(type default)\n\t\t)\n\
         \t\t(fill no)\n\t\t(layer \"Edge.Cuts\")\n\t\t(uuid \"{uuid}\")\n\t)\n"
    );
}

// ── per-footprint synthesis ──────────────────────────────────────────────────

/// Transform one part's `.kicad_mod` source into a board `(footprint …)` block,
/// indented one tab to sit at the board's top level.
fn synth_footprint(part: &SynthPart, net_codes: &BTreeMap<String, i32>) -> io::Result<String> {
    let body = footprint_body(&part.source).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("part {}: source has no (footprint …) block", part.reference),
        )
    })?;

    // Footprint rotation: KiCAD CCW degrees, normalized to [0,360). v1 placer
    // emits only axis-aligned angles; reject anything else rather than emit
    // wrong pad geometry.
    let rot = part.placement.rotation.rem_euclid(360);
    if !matches!(rot, 0 | 90 | 180 | 270) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "part {}: rotation {rot}° is not supported — synthesis handles \
                 0/90/180/270 only (the engine emits axis-aligned placements)",
                part.reference
            ),
        ));
    }

    // Strip the `(footprint "NAME"` opener and the body's final closing paren so
    // we can re-wrap the inner children with our injected header + per-pad nets.
    let inner = footprint_inner(body).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("part {}: malformed (footprint …) block", part.reference),
        )
    })?;

    let mut out = String::with_capacity(body.len() + 256);
    let _ = writeln!(out, "\t(footprint \"{}\"", part.lib_id);
    // Injected header: layer, board-instance uuid, position+rotation.
    out.push_str("\t\t(layer \"F.Cu\")\n");
    let fp_uuid = synth_uuid(&format!("fp:{}:{}", part.reference, part.lib_id));
    let _ = writeln!(out, "\t\t(uuid \"{fp_uuid}\")");
    if rot == 0 {
        let _ = writeln!(
            out,
            "\t\t(at {} {})",
            fmt_num(part.placement.at.x),
            fmt_num(part.placement.at.y)
        );
    } else {
        let _ = writeln!(
            out,
            "\t\t(at {} {} {})",
            fmt_num(part.placement.at.x),
            fmt_num(part.placement.at.y),
            rot
        );
    }

    // Re-indent and transform every top-level child node of the footprint body.
    for node in top_level_nodes(inner) {
        if let Some(transformed) = transform_node(node, part, net_codes, rot)? {
            // The node text is at `.kicad_mod` indentation (one tab); board
            // footprints sit one level deeper, so add one tab to every line.
            push_reindented(&mut out, &transformed);
        }
    }

    out.push_str("\t)\n");
    Ok(out)
}

/// Decide what to do with one top-level child node of a footprint body. Returns
/// `Ok(None)` to drop the node (silkscreen graphics), `Ok(Some(text))` with the
/// (possibly rewritten) node otherwise.
fn transform_node(
    node: &str,
    part: &SynthPart,
    net_codes: &BTreeMap<String, i32>,
    fp_rot: i32,
) -> io::Result<Option<String>> {
    let head = node_head(node);
    match head {
        // Footprint-level position/uuid/layer are injected fresh in the header;
        // drop any the source carried so they are not duplicated. (`.kicad_mod`
        // has none, but be robust to sources that do.)
        "at" | "uuid" | "layer" => Ok(None),
        // Silkscreen *text* is dropped — the only library silk text is a value/
        // ref placeholder that would duplicate the reference designator (which we
        // keep, see `property` below) and clutter the board. Silk *graphics* (the
        // component outline lines/arcs) are KEPT: they are what makes the render
        // read as a real board, they sit outside the part's own pads, and any
        // silk-over-neighbour-copper is a tolerated DRC warning, not an error.
        "fp_text" if on_silk(node) => Ok(None),
        // Library cruft that does not belong on a board footprint instance.
        "version" | "generator" | "generator_version" | "embedded_fonts" | "model"
        | "tags" | "descr" => Ok(None),
        // Version-specific footprint-authoring hints that postdate the minimum
        // KiCAD we target: KiCAD 9.0.2's board loader rejects the whole file on
        // an unknown footprint token (silent "Failed to load board"). These carry
        // no copper/courtyard/routing meaning, so drop them rather than gate the
        // board on the writer's KiCAD version. `duplicate_pad_numbers_are_jumpers`
        // appears in library footprints saved by KiCAD ≥ 9.0.3; none of the
        // 9.0.2-era system libraries emit it.
        "duplicate_pad_numbers_are_jumpers" => Ok(None),
        // Reference property: set the designator, leave it on its library layer
        // (F.SilkS, positioned above the part). Value property: hide it so the
        // long footprint-name string never clutters the board render.
        "property" => Ok(Some(transform_property(node, &part.reference))),
        // Pads: inject the (net …) binding for bound pads, and bump pad rotation
        // by the footprint angle when the footprint is rotated.
        "pad" => Ok(Some(transform_pad(node, part, net_codes, fp_rot)?)),
        // Everything else (fp_rect/fp_line on F.CrtYd or F.Fab, attr, …) passes
        // through unchanged.
        _ => Ok(Some(node.to_owned())),
    }
}

/// Reference-designator text height cap (mm). KiCAD library defaults are 1.0mm,
/// which crowd dense boards; 0.8mm stays legible and reduces silk collisions.
const REF_TEXT_SIZE_MM: f64 = 0.8;

/// Rewrite a `(property …)` node: when it is the `Reference`, replace the value
/// with `reference`; force the property's `(layer …)` to `F.Fab` either way.
fn transform_property(node: &str, reference: &str) -> String {
    // Reference: set the designator and keep it on its library layer (F.SilkS,
    // positioned above the part) — that is where it belongs on a fabricated
    // board and what a professional render shows.
    if let Some(rest) = node.strip_prefix("(property \"Reference\" \"")
        && let Some(close) = rest.find('"')
    {
        // Cap the refdes text height: a 1.0mm library default crowds a dense
        // board, and a smaller refdes only ever REDUCES silk overlap (it never
        // moves a ref into a collision), so this is a safe legibility win.
        let body = cap_font_size(&rest[close + 1..], REF_TEXT_SIZE_MM);
        return format!("(property \"Reference\" \"{reference}\"{body}");
    }
    // Value: keep the property (KiCAD expects it to exist) but hide it. Its text
    // is the full footprint library name, which on a small board dominates the
    // render and overlaps neighbouring parts; a hidden value is conventional.
    if node.starts_with("(property \"Value\"") && !node.contains("(hide yes)")
        && let Some(hidden) = inject_before_close(node, "(hide yes)") {
            return hidden;
        }
    node.to_owned()
}

/// Inject `(net N "name")` into a pad node for a bound pad, and add the
/// footprint rotation to the pad's own `(at … rot)` when the footprint is
/// rotated. An unbound pad (no entry in `pad_nets`) is returned with only its
/// rotation adjusted.
fn transform_pad(
    node: &str,
    part: &SynthPart,
    net_codes: &BTreeMap<String, i32>,
    fp_rot: i32,
) -> io::Result<String> {
    let number = pad_number(node);
    let rotated = if fp_rot != 0 {
        bump_pad_rotation(node, fp_rot)
    } else {
        node.to_owned()
    };

    let Some(number) = number else {
        return Ok(rotated);
    };
    let Some(net) = part.pad_nets.get(&number).filter(|n| !n.is_empty()) else {
        return Ok(rotated);
    };
    let code = net_codes.get(net).copied().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "part {}: pad {number} net {net:?} has no code (internal: net table out of sync)",
                part.reference
            ),
        )
    })?;

    // Inject `(net N "name")` immediately before the pad's final closing paren,
    // on its own indented line. The pad body is a balanced s-expr; the last
    // top-level `)` closes it.
    inject_before_close(&rotated, &format!("(net {code} \"{net}\")")).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("part {}: pad {number} has no closing paren", part.reference),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_model::Point2;
    use std::path::PathBuf;

    fn fixture(name: &str) -> String {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../kicad-sexpr/tests/fixtures/footprints")
            .join(name);
        std::fs::read_to_string(p).unwrap()
    }

    fn place(reference: &str, x: f64, y: f64, rot: i32) -> Placement {
        Placement {
            reference: reference.to_owned(),
            at: Point2 { x, y },
            rotation: rot,
        }
    }

    fn nets(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn synthesize_two_part_board_is_render_ready() {
        let parts = vec![
            SynthPart {
                reference: "R1".into(),
                lib_id: "Resistor_SMD:R_0603_1608Metric".into(),
                source: fixture("R_0603_1608Metric.kicad_mod"),
                pad_nets: nets(&[("1", "VOUT"), ("2", "GND")]),
                placement: place("R1", 10.0, 10.0, 0),
            },
            SynthPart {
                reference: "U1".into(),
                lib_id: "Package_TO_SOT_SMD:SOT-23".into(),
                source: fixture("SOT-23.kicad_mod"),
                pad_nets: nets(&[("1", "VIN"), ("2", "GND"), ("3", "VOUT")]),
                placement: place("U1", 20.0, 10.0, 0),
            },
        ];
        let bounds = Bounds { min_x: 0.0, max_x: 30.0, min_y: 0.0, max_y: 20.0 };
        let board = synthesize_board(&parts, &bounds).unwrap();

        // Silkscreen survives so the board renders like a real PCB: the layer
        // table declares F.SilkS and the footprint outline graphics sit on it.
        assert!(
            board.contains("(layer \"F.SilkS\")"),
            "silk graphics (component outline + reference) must be kept:\n{board}"
        );
        // The reference designator is kept on silk; the long Value name is hidden.
        assert!(board.contains("(property \"Reference\" \"R1\""), "ref on board: {board}");
        assert!(
            board.contains("(property \"Value\"") && board.contains("(hide yes)"),
            "Value property must be present but hidden:\n{board}"
        );
        // Reference value was set; REF** placeholder is gone.
        assert!(board.contains("\"R1\""));
        assert!(board.contains("\"U1\""));
        assert!(!board.contains("REF**"));
        // Net table carries the three nets.
        assert!(board.contains("(net 0 \"\")"));
        for n in ["GND", "VIN", "VOUT"] {
            assert!(board.contains(&format!("\"{n}\"")), "missing net {n}");
        }
        // Courtyards survive (kept from the library).
        assert!(board.contains("F.CrtYd"));
        // Version-specific authoring tokens that KiCAD 9.0.2's loader rejects must
        // not leak through from the source library (silent "Failed to load board").
        assert!(
            !board.contains("duplicate_pad_numbers_are_jumpers"),
            "loader-breaking footprint token leaked into the board:\n{board}"
        );
    }

    /// A board with two distinct net widths emits ≥2 `(net_class …)` blocks, each
    /// carrying its trace width and the right member nets (`add_net`). This is the
    /// fat-power vs thin-signal intent made legible in the `.kicad_pcb`.
    #[test]
    fn distinct_net_widths_emit_net_class_blocks() {
        let parts = vec![SynthPart {
            reference: "R1".into(),
            lib_id: "Resistor_SMD:R_0603_1608Metric".into(),
            source: fixture("R_0603_1608Metric.kicad_mod"),
            pad_nets: nets(&[("1", "VOUT"), ("2", "GND")]),
            placement: place("R1", 10.0, 10.0, 0),
        }];
        let bounds = Bounds { min_x: 0.0, max_x: 30.0, min_y: 0.0, max_y: 20.0 };
        let classes = vec![
            NetClass {
                name: "Power".into(),
                description: "fat power nets".into(),
                clearance: 0.2,
                trace_width: 0.8,
                via_diameter: 0.6,
                via_drill: 0.3,
                members: vec!["VOUT".into()],
            },
            NetClass {
                name: "Default".into(),
                description: "board default".into(),
                clearance: 0.2,
                trace_width: 0.2,
                via_diameter: 0.6,
                via_drill: 0.3,
                members: vec!["GND".into()],
            },
        ];
        let board =
            synthesize_board_full(&parts, &bounds, 2, &[], &[], None, &classes).unwrap();

        // Two distinct widths → two classes.
        assert_eq!(board.matches("(net_class ").count(), 2, "two classes:\n{board}");
        // Power class: fat width + the VOUT member.
        assert!(board.contains("(net_class \"Power\" \"fat power nets\""), "{board}");
        assert!(board.contains("(trace_width 0.8)"), "fat trace width:\n{board}");
        assert!(board.contains("(add_net \"VOUT\")"), "VOUT in a class:\n{board}");
        // Default class: thin width + the GND member.
        assert!(board.contains("(net_class \"Default\""), "{board}");
        assert!(board.contains("(trace_width 0.2)"), "thin trace width:\n{board}");
        assert!(board.contains("(add_net \"GND\")"), "GND in a class:\n{board}");
        // A class member that is not a real board net is dropped, never emitted.
        let phantom = vec![NetClass {
            name: "Ghost".into(),
            description: "no real nets".into(),
            clearance: 0.2,
            trace_width: 0.5,
            via_diameter: 0.6,
            via_drill: 0.3,
            members: vec!["NOPE".into()],
        }];
        let board2 =
            synthesize_board_full(&parts, &bounds, 2, &[], &[], None, &phantom).unwrap();
        assert!(!board2.contains("net_class"), "empty class dropped:\n{board2}");
    }

    /// The synthesized board parses with `read_problem`, pads land at
    /// placement+offset, nets bind, and `write_solution` then round-trips copper.
    #[test]
    fn synthesized_board_round_trips_through_read_problem_and_write_solution() {
        use kicad_sexpr::pcb::{extract_copper, read_problem, write_solution};
        use pcb_model::{LayerRef, RouteSolution, Trace};

        let parts = vec![
            SynthPart {
                reference: "R1".into(),
                lib_id: "Resistor_SMD:R_0603_1608Metric".into(),
                source: fixture("R_0603_1608Metric.kicad_mod"),
                pad_nets: nets(&[("1", "VOUT"), ("2", "GND")]),
                placement: place("R1", 10.0, 10.0, 0),
            },
            SynthPart {
                reference: "U1".into(),
                lib_id: "Package_TO_SOT_SMD:SOT-23".into(),
                source: fixture("SOT-23.kicad_mod"),
                pad_nets: nets(&[("1", "VIN"), ("2", "GND"), ("3", "VOUT")]),
                placement: place("U1", 20.0, 10.0, 0),
            },
        ];
        let bounds = Bounds { min_x: 0.0, max_x: 30.0, min_y: 0.0, max_y: 20.0 };
        let board = synthesize_board(&parts, &bounds).unwrap();

        // Write the synthesized board to a temp file and parse it back.
        let tmp = tempfile::Builder::new()
            .prefix("autopcb-synth-")
            .suffix(".kicad_pcb")
            .tempfile()
            .unwrap();
        std::fs::write(tmp.path(), board.as_bytes()).unwrap();
        let bp = read_problem(tmp.path()).expect("read_problem on synthesized board");

        // Bounds came through.
        assert_eq!(bp.problem.bounds, bounds);
        // Net codes exist for every named net.
        for n in ["GND", "VIN", "VOUT"] {
            assert!(bp.net_codes.contains_key(n), "net {n} missing: {:?}", bp.net_codes);
        }

        // R1 pad "1" world center = placement (10,10) + offset (-0.825,0).
        let vout = bp
            .problem
            .connections
            .iter()
            .find(|c| c.name == "VOUT")
            .expect("VOUT connection");
        // VOUT binds R1.1 (10-0.825,10) and U1.3 (20+0.9375,10).
        let has_r1_pad1 = vout
            .points_to_connect
            .iter()
            .any(|p| (p.x - 9.175).abs() < 1e-6 && (p.y - 10.0).abs() < 1e-6);
        assert!(has_r1_pad1, "R1.1 not at (9.175,10): {:?}", vout.points_to_connect);
        let has_u1_pad3 = vout
            .points_to_connect
            .iter()
            .any(|p| (p.x - 20.9375).abs() < 1e-6 && (p.y - 10.0).abs() < 1e-6);
        assert!(has_u1_pad3, "U1.3 not at (20.9375,10): {:?}", vout.points_to_connect);

        // Now splice a trace onto VOUT and round-trip it back out.
        let solution = RouteSolution {
            traces: vec![Trace {
                connection: "VOUT".into(),
                layer: LayerRef::top(),
                width: 0.25,
                path: vec![
                    pcb_model::Point2 { x: 9.175, y: 10.0 },
                    pcb_model::Point2 { x: 20.9375, y: 10.0 },
                ],
            }],
            vias: Vec::new(),
        };
        write_solution(tmp.path(), &solution, &bp).expect("write_solution onto synthesized board");
        let back = extract_copper(tmp.path()).expect("extract_copper");
        assert_eq!(back.traces.len(), 1, "one trace round-tripped: {back:?}");
        assert_eq!(back.traces[0].connection, "VOUT");
    }
}
