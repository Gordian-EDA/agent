//! Board **synthesis**: BUILD a `.kicad_pcb` from scratch out of footprint
//! `.kicad_mod` bodies, an engine placement, and a board outline — the
//! agent-flow companion to [`crate::placefp::move_footprints`].
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
//! file [`crate::footlib::Footprint::load`] and `part_from_footprint` read), so we
//! transform its raw text rather than round-tripping through a lossy parsed form
//! (kiutils' footprint `ast_mut` does not round-trip through `write()`, the same
//! limitation [`crate::pcb::write_solution`] documents). The transforms are:
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
//! convention [`crate::pcb::pad_center`] reads back. v1 supports 0/90/180/270
//! (the only values the placer emits) by adding the footprint angle to each pad's
//! `(at)` rotation; any other angle is rejected with a clear error rather than
//! emitting wrong geometry (honest rejection beats a silent short).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;

use pcb_engine::placement::Placement;
use pcb_engine::problem::{Bounds, Point2};

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

/// Synthesize a complete 2-layer `.kicad_pcb` from `parts` on a board of
/// `bounds`. Convenience wrapper over [`synthesize_board_layers`].
pub fn synthesize_board(parts: &[SynthPart], bounds: &Bounds) -> io::Result<String> {
    synthesize_board_layers(parts, bounds, 2)
}

/// Synthesize a complete `.kicad_pcb` from `parts` on a board of `bounds` with
/// `layer_count` copper layers (2 or 4 — the engine's supported stackups).
///
/// The result parses with [`crate::pcb::read_problem`] and is structurally a
/// KiCAD-9 board (see the module docs). Net codes are 1-based over the sorted
/// union of every part's pad nets. Returns an [`io::Error`] if a part's source
/// has no parseable footprint block, a placement is missing for a part, or a
/// non-axis-aligned rotation is requested.
pub fn synthesize_board_layers(
    parts: &[SynthPart],
    bounds: &Bounds,
    layer_count: u32,
) -> io::Result<String> {
    synthesize_board_full(parts, bounds, layer_count, &[], None)
}

/// A copper-plane zone to emit: the net it belongs to, the copper layer name
/// (e.g. `"In1.Cu"`), and the precomputed fill rectangles ([`plane_fill_rects`]).
#[derive(Debug, Clone)]
pub struct ZoneSpec {
    pub net_name: String,
    pub layer_name: String,
    pub fill_rects: Vec<[f64; 4]>,
    /// Zone copper-to-foreign clearance (mm) — the board's design clearance, so the
    /// zone is checked against the SAME rule the router used (not KiCAD's 0.2 default,
    /// which false-flags a finer-pitch board's plane).
    pub clearance: f64,
    /// Minimum zone copper width (mm) — the board's min trace width.
    pub min_thickness: f64,
}

/// [`synthesize_board_layers`] plus copper-plane `zones` (power pours) emitted
/// before the board close. Each zone's net must be one of the parts' nets.
pub fn synthesize_board_full(
    parts: &[SynthPart],
    bounds: &Bounds,
    layer_count: u32,
    zones: &[ZoneSpec],
    outline: Option<&[Point2]>,
) -> io::Result<String> {
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
    push_edge_cuts(&mut out, bounds, outline);

    for part in parts {
        let block = synth_footprint(part, &net_codes)?;
        out.push_str(&block);
    }

    for (i, z) in zones.iter().enumerate() {
        let code = net_codes.get(&z.net_name).copied().unwrap_or(0);
        push_zone(&mut out, code, z, i);
    }

    out.push_str(")\n");
    Ok(out)
}

/// Emit one copper-plane `(zone …)` with the precomputed fill rectangles as
/// edge-sharing `filled_polygon` islands (KiCAD treats them as one connected
/// pour — see [`plane_fill_rects`]).
fn push_zone(out: &mut String, net_code: i32, z: &ZoneSpec, idx: usize) {
    let uuid = synth_uuid(&format!("zone:{}:{}", z.net_name, z.layer_name));
    let _ = idx;
    let _ = writeln!(
        out,
        // SOLID pad connection (`connect_pads yes`): a power/ground plane should
        // tie to its same-net pads with full copper, not thermal-relief spokes —
        // the spokes starve on large through-hole pads (mounting holes, TH power),
        // which KiCAD flags as `starved_thermal`. Solid is the standard plane
        // connection and is low-impedance. Foreign pads are still carved out by the
        // anti-pad keepouts baked into `fill_rects`, so this only ties same-net copper.
        "\t(zone\n\t\t(net {net_code})\n\t\t(net_name \"{}\")\n\t\t(layer \"{}\")\n\
         \t\t(uuid \"{uuid}\")\n\t\t(hatch edge 0.5)\n\t\t(connect_pads yes (clearance {clr}))\n\
         \t\t(min_thickness {mt})\n\t\t(fill yes (thermal_gap 0.3) (thermal_bridge_width 0.5))",
        z.net_name, z.layer_name, clr = fmt_num(z.clearance), mt = fmt_num(z.min_thickness)
    );
    // Zone outline = the board's fill bounding box (KiCAD requires a polygon; the
    // filled_polygon islands below are the authoritative copper).
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for r in &z.fill_rects {
        x0 = x0.min(r[0]);
        y0 = y0.min(r[1]);
        x1 = x1.max(r[2]);
        y1 = y1.max(r[3]);
    }
    if x0 <= x1 {
        let _ = writeln!(
            out,
            "\t\t(polygon (pts (xy {} {}) (xy {} {}) (xy {} {}) (xy {} {})))",
            fmt_num(x0), fmt_num(y0), fmt_num(x1), fmt_num(y0),
            fmt_num(x1), fmt_num(y1), fmt_num(x0), fmt_num(y1)
        );
    }
    for r in &z.fill_rects {
        let _ = writeln!(
            out,
            "\t\t(filled_polygon (layer \"{}\") (pts (xy {} {}) (xy {} {}) (xy {} {}) (xy {} {})))",
            z.layer_name,
            fmt_num(r[0]), fmt_num(r[1]), fmt_num(r[2]), fmt_num(r[1]),
            fmt_num(r[2]), fmt_num(r[3]), fmt_num(r[0]), fmt_num(r[3])
        );
    }
    out.push_str("\t)\n");
}

/// 1-based net codes over the sorted union of every part's pad net names.
/// A copper-plane (power-pour) fill as axis-aligned rectangles tiling `bounds`
/// inset by `edge_margin`, MINUS a rectangular keep-out around each item in
/// `keepouts` (`(center, half_x, half_y)` — the halves already include the
/// required clearance; a via/pad passes equal halves, a keep-out region its rect
/// halves). Returns rects as `[min_x, min_y, max_x, max_y]`.
///
/// KiCAD treats edge-sharing `filled_polygon` islands as one connected plane
/// (verified against kicad-cli), so a horizontal-band sweep produces a valid,
/// DRC-clean fill with NO clipping / keyhole geometry: cut the board into y-bands
/// at every keep-out edge, and in each band emit the x-segments left free by the
/// keep-outs active there. This is how a power net's many pins are joined without
/// routing each one — the lever a BGA's power balls need.
pub fn plane_fill_rects(
    bounds: &Bounds,
    edge_margin: f64,
    keepouts: &[(Point2, f64, f64)],
    outline: Option<&[Point2]>,
) -> Vec<[f64; 4]> {
    let (bx0, bx1) = (bounds.min_x + edge_margin, bounds.max_x - edge_margin);
    let (by0, by1) = (bounds.min_y + edge_margin, bounds.max_y - edge_margin);
    if bx1 <= bx0 || by1 <= by0 {
        return Vec::new();
    }
    // y-band boundaries: the board edges plus each keep-out's top/bottom (clamped).
    let mut ycuts: Vec<f64> = vec![by0, by1];
    for (c, _hx, hy) in keepouts {
        ycuts.push((c.y - hy).clamp(by0, by1));
        ycuts.push((c.y + hy).clamp(by0, by1));
    }
    // For a custom OUTLINE, add fine y-bands so the per-band polygon scanline clip
    // (taken at the band mid-y) follows the true edge smoothly instead of overhanging.
    if outline.is_some() {
        let mut y = by0;
        while y < by1 {
            ycuts.push(y);
            y += 0.5;
        }
    }
    ycuts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    ycuts.dedup_by(|a, b| (*a - *b).abs() < 1e-6);

    let mut rects = Vec::new();
    for w in ycuts.windows(2) {
        let (y0, y1) = (w[0], w[1]);
        if y1 - y0 < 1e-6 {
            continue;
        }
        let ymid = (y0 + y1) / 2.0;
        // x-intervals blocked by keep-outs straddling this band, merged.
        let mut blocked: Vec<(f64, f64)> = keepouts
            .iter()
            .filter(|(c, _hx, hy)| c.y - hy < ymid && ymid < c.y + hy)
            .map(|(c, hx, _hy)| ((c.x - hx).max(bx0), (c.x + hx).min(bx1)))
            .filter(|(a, b)| b > a)
            .collect();
        blocked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut merged: Vec<(f64, f64)> = Vec::new();
        for (a, b) in blocked {
            match merged.last_mut() {
                Some(last) if a <= last.1 + 1e-9 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        // Free x-segments = [bx0, bx1] minus the merged blocked intervals.
        let mut free: Vec<(f64, f64)> = Vec::new();
        let mut x = bx0;
        for (a, b) in &merged {
            if a - x > 1e-6 {
                free.push((x, *a));
            }
            x = x.max(*b);
        }
        if bx1 - x > 1e-6 {
            free.push((x, bx1));
        }
        // Custom outline: clip each free segment to the polygon's interior at this band
        // (scanline x-spans at mid-y, inset by the edge margin), so copper never reaches
        // past the true edge — a pour/plane on a non-rectangular board.
        if let Some(poly) = outline {
            let spans: Vec<(f64, f64)> = polygon_x_spans(poly, ymid)
                .into_iter()
                .map(|(a, b)| (a + edge_margin, b - edge_margin))
                .filter(|(a, b)| b - a > 1e-6)
                .collect();
            let mut clipped = Vec::new();
            for (fa, fb) in &free {
                for (pa, pb) in &spans {
                    let (lo, hi) = (fa.max(*pa), fb.min(*pb));
                    if hi - lo > 1e-6 {
                        clipped.push((lo, hi));
                    }
                }
            }
            free = clipped;
        }
        for (a, b) in free {
            rects.push([a, y0, b, y1]);
        }
    }
    rects
}

/// The x-intervals where the horizontal line `y` is INSIDE polygon `poly` (scanline,
/// even-odd): the sorted edge crossings paired up. A convex shape gives one interval; a
/// concave one (a star) gives several. Used to clip a plane/pour fill to a custom outline.
fn polygon_x_spans(poly: &[Point2], y: f64) -> Vec<(f64, f64)> {
    let n = poly.len();
    if n < 3 {
        return Vec::new();
    }
    let mut xs: Vec<f64> = Vec::new();
    let mut j = n - 1;
    for i in 0..n {
        let (a, b) = (&poly[j], &poly[i]);
        if (a.y > y) != (b.y > y) {
            xs.push(a.x + (y - a.y) / (b.y - a.y) * (b.x - a.x));
        }
        j = i;
    }
    xs.sort_by(|p, q| p.partial_cmp(q).unwrap_or(std::cmp::Ordering::Equal));
    xs.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0], c[1])).collect()
}

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
            let _ = write!(out, "\t\t({i} \"In{i}.Cu\" signal)\n");
        }
        let b = layer_count - 1;
        let _ = write!(out, "\t\t({b} \"B.Cu\" signal)\n");
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

/// Emit the board outline on `Edge.Cuts`. With `outline = Some(pts)` (≥3 points) the
/// outline is that closed polygon (one `gr_line` per edge) — circle (many points),
/// square, star, any custom shape. Otherwise the `bounds` rectangle (the default).
fn push_edge_cuts(out: &mut String, bounds: &Bounds, outline: Option<&[Point2]>) {
    if let Some(pts) = outline {
        if pts.len() >= 3 {
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

/// Rewrite a `(property …)` node: when it is the `Reference`, replace the value
/// with `reference`; force the property's `(layer …)` to `F.Fab` either way.
/// Reference-designator text height cap (mm). KiCAD library defaults are 1.0mm,
/// which crowd dense boards; 0.8mm stays legible and reduces silk collisions.
const REF_TEXT_SIZE_MM: f64 = 0.8;

/// Cap the first `(size W H)` in `body` to `max` mm on each axis (shrink only —
/// a smaller library value is left alone). Used to keep refdes text compact.
fn cap_font_size(body: &str, max: f64) -> String {
    let Some(start) = body.find("(size ") else { return body.to_owned() };
    let open = start + "(size ".len();
    let Some(rel_close) = body[open..].find(')') else { return body.to_owned() };
    let inner = &body[open..open + rel_close];
    let nums: Vec<f64> = inner.split_whitespace().filter_map(|t| t.parse().ok()).collect();
    if nums.len() != 2 {
        return body.to_owned();
    }
    let (w, h) = (nums[0].min(max), nums[1].min(max));
    format!("{}(size {} {}){}", &body[..start], fmt_num(w), fmt_num(h), &body[open + rel_close + 1..])
}

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
    if node.starts_with("(property \"Value\"") && !node.contains("(hide yes)") {
        if let Some(hidden) = inject_before_close(node, "(hide yes)") {
            return hidden;
        }
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

/// Add `fp_rot` (CCW degrees) to a pad's stored `(at x y [rot])` rotation. KiCAD
/// pad rotation is absolute (footprint angle folded in), so a rotated footprint
/// needs each pad's angle bumped. The pad `(at …)` is the first `(at ` on its
/// own line inside the pad node.
fn bump_pad_rotation(node: &str, fp_rot: i32) -> String {
    const AT: &str = "(at ";
    // Find the pad-level `(at …)` — the first one inside the node.
    let Some(at_pos) = node.find(AT) else {
        return node.to_owned();
    };
    let after = &node[at_pos + AT.len()..];
    let Some(line_end) = after.find(')') else {
        return node.to_owned();
    };
    let inside = &after[..line_end]; // "x y" or "x y rot"
    let nums: Vec<&str> = inside.split_whitespace().collect();
    let (x, y) = match (nums.first(), nums.get(1)) {
        (Some(x), Some(y)) => (*x, *y),
        _ => return node.to_owned(),
    };
    let pad_rot: f64 = nums.get(2).and_then(|r| r.parse().ok()).unwrap_or(0.0);
    let new_rot = (pad_rot + fp_rot as f64).rem_euclid(360.0);
    let replacement = if new_rot == 0.0 {
        format!("(at {x} {y}")
    } else {
        format!("(at {x} {y} {})", fmt_num(new_rot))
    };
    // Rebuild: prefix + new "(at …" + the rest after the original "(at …" up to
    // and including its ')'. We replace the substring `(at <inside>)`.
    let mut out = String::with_capacity(node.len() + 8);
    out.push_str(&node[..at_pos]);
    out.push_str(&replacement);
    out.push_str(&node[at_pos + AT.len() + line_end + 1..]); // after the ')'
    out
}

// ── s-expression helpers (paren-aware, no full parse) ─────────────────────────

/// The whole `(footprint …)` block in `source` (from its opening paren to its
/// matching closing paren), or `None` if absent/unbalanced.
fn footprint_body(source: &str) -> Option<&str> {
    let start = source.find("(footprint ")?;
    let end = matching_close(source, start)?;
    Some(&source[start..=end])
}

/// The inner text of a `(footprint "NAME" … )` block: everything after the
/// `(footprint "NAME"` opener up to (not including) the block's final `)`.
fn footprint_inner(body: &str) -> Option<&str> {
    // Skip `(footprint ` then the quoted name token.
    let after_kw = body.strip_prefix("(footprint ")?;
    let rest = after_kw.strip_prefix('"')?;
    let name_close = rest.find('"')?;
    let inner_start = "(footprint ".len() + 1 + name_close + 1;
    // The body ends with its matching ')'; inner is everything between.
    let inner = &body[inner_start..body.len() - 1];
    Some(inner)
}

/// Split a footprint body's inner text into its top-level child nodes (each a
/// balanced `( … )` s-expression), trimming the whitespace between them.
fn top_level_nodes(inner: &str) -> Vec<&str> {
    let bytes = inner.as_bytes();
    let mut nodes = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'('
            && let Some(end) = matching_close(inner, i)
        {
            nodes.push(&inner[i..=end]);
            i = end + 1;
            continue;
        }
        i += 1;
    }
    nodes
}

/// Index of the `)` matching the `(` at `open` in `s`, honoring string literals
/// (parens inside `"…"` do not count). `None` if unbalanced.
fn matching_close(s: &str, open: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    debug_assert_eq!(bytes[open], b'(');
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_str = !in_str,
            b'(' if !in_str => depth += 1,
            b')' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The head symbol of an s-expression node `(<head> …)`, e.g. `"pad"`,
/// `"fp_line"`, `"property"`. Empty string if the node is malformed.
fn node_head(node: &str) -> &str {
    let rest = node.strip_prefix('(').unwrap_or(node);
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .unwrap_or(rest.len());
    &rest[..end]
}

/// Whether a graphic node sits on a silkscreen layer (`*.SilkS`).
fn on_silk(node: &str) -> bool {
    // A graphic's layer appears as `(layer "F.SilkS")` / `"B.SilkS"`.
    node.contains("(layer \"F.SilkS\")") || node.contains("(layer \"B.SilkS\")")
}

/// The pad number token of a `(pad "N" …)` node, if present.
fn pad_number(node: &str) -> Option<String> {
    let rest = node.strip_prefix("(pad ")?;
    let rest = rest.strip_prefix('"')?;
    let close = rest.find('"')?;
    Some(rest[..close].to_owned())
}

/// Insert `insertion` on its own line immediately before the final closing paren
/// of the balanced s-expression `node`. The inserted line is indented to match
/// the node's children (the indentation of the first child line). `None` if the
/// node has no closing paren.
fn inject_before_close(node: &str, insertion: &str) -> Option<String> {
    let close = node.rfind(')')?;
    // Child indentation: the whitespace run after the first newline.
    let indent = child_indent(node);
    let mut out = String::with_capacity(node.len() + insertion.len() + indent.len() + 2);
    out.push_str(&node[..close]);
    // Ensure we start the inserted line cleanly (node[..close] ends with the
    // child block's trailing newline+indent before the ')').
    out.push_str(insertion);
    out.push('\n');
    out.push_str(&indent);
    out.push_str(&node[close..]);
    Some(out)
}

/// The indentation (leading whitespace) of the first child line of a multi-line
/// node — i.e. the run of tabs/spaces after the node's first `\n`. Empty for a
/// single-line node.
fn child_indent(node: &str) -> String {
    let Some(nl) = node.find('\n') else {
        return String::new();
    };
    node[nl + 1..]
        .chars()
        .take_while(|c| *c == '\t' || *c == ' ')
        .collect()
}

/// Append `node` to `out` with one extra leading tab on every non-empty line, so
/// a `.kicad_mod` child (indented one level) sits at the board-footprint depth.
fn push_reindented(out: &mut String, node: &str) {
    for (i, line) in node.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !line.is_empty() {
            out.push('\t');
        }
        out.push_str(line);
    }
    out.push('\n');
}

// ── formatting / ids ─────────────────────────────────────────────────────────

/// Fixed namespace UUID for synthesized board identifiers (distinct from the
/// copper namespace in [`crate::pcb`]). Content-derived so a given board re-emits
/// byte-identically.
const SYNTH_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x7b2e_91c0_4d3a_5e6f_8a9b_0c1d_2e3f_4a5b);

/// Content-derived UUID for a synthesized board element.
fn synth_uuid(key: &str) -> String {
    uuid::Uuid::new_v5(&SYNTH_NAMESPACE, key.as_bytes())
        .as_hyphenated()
        .to_string()
}

/// Format an `f64` the way KiCAD writes coordinates (shortest round-tripping
/// decimal, `-0.0` collapsed to `0`). Mirrors `pcb::fmt_num`/`placefp::fmt_num`.
fn fmt_num(v: f64) -> String {
    let v = if v == 0.0 { 0.0 } else { v };
    format!("{v}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_engine::problem::Point2;
    use std::path::PathBuf;

    #[test]
    fn plane_fill_empty_is_single_inset_rect() {
        let b = Bounds { min_x: 0.0, max_x: 20.0, min_y: 0.0, max_y: 10.0 };
        let rects = plane_fill_rects(&b, 0.5, &[], None);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0], [0.5, 0.5, 19.5, 9.5]);
    }

    #[test]
    fn plane_fill_carves_keepouts_and_stays_in_bounds() {
        let b = Bounds { min_x: 0.0, max_x: 20.0, min_y: 0.0, max_y: 20.0 };
        let ko = (Point2 { x: 10.0, y: 10.0 }, 0.65, 0.65);
        let rects = plane_fill_rects(&b, 0.5, &[ko], None);
        assert!(rects.len() > 1, "a central keep-out must split the fill");
        // The keep-out square [9.35,10.65]^2 must contain NO fill rect interior.
        let (kx0, kx1, ky0, ky1) = (9.35, 10.65, 9.35, 10.65);
        for r in &rects {
            // every rect within the inset board
            assert!(r[0] >= 0.5 - 1e-9 && r[2] <= 19.5 + 1e-9);
            assert!(r[1] >= 0.5 - 1e-9 && r[3] <= 19.5 + 1e-9);
            // and not overlapping the keep-out interior
            let overlap = r[0] < kx1 - 1e-6 && r[2] > kx0 + 1e-6 && r[1] < ky1 - 1e-6 && r[3] > ky0 + 1e-6;
            assert!(!overlap, "rect {r:?} overlaps the keep-out");
        }
    }

    fn fixture(name: &str) -> String {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/footprints")
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
    fn matching_close_handles_nested_and_strings() {
        let s = "(a (b \"x)y\") c)";
        assert_eq!(matching_close(s, 0), Some(s.len() - 1));
    }

    #[test]
    fn node_head_and_silk() {
        assert_eq!(node_head("(pad \"1\" smd)"), "pad");
        assert_eq!(node_head("(fp_line\n\t(layer \"F.SilkS\")\n)"), "fp_line");
        assert!(on_silk("(fp_line\n\t(layer \"F.SilkS\")\n)"));
        assert!(!on_silk("(fp_rect\n\t(layer \"F.CrtYd\")\n)"));
    }

    #[test]
    fn inject_net_into_thru_hole_pad_with_drill() {
        let pad = "(pad \"1\" thru_hole rect\n\t(at 0 0)\n\t(size 1.7 1.7)\n\t(drill 1)\n\t(layers \"*.Cu\" \"*.Mask\")\n)";
        let out = inject_before_close(pad, "(net 2 \"VIN\")").unwrap();
        assert!(out.contains("(net 2 \"VIN\")"));
        // The (drill 1) child is untouched and the net lands before the close.
        let net_at = out.find("(net 2").unwrap();
        let close = out.rfind(')').unwrap();
        assert!(net_at < close);
        assert!(out.contains("(drill 1)"));
    }

    #[test]
    fn bump_pad_rotation_adds_footprint_angle() {
        let pad = "(pad \"1\" smd roundrect\n\t(at -0.9375 -0.95)\n\t(size 1.475 0.6)\n)";
        let out = bump_pad_rotation(pad, 90);
        assert!(out.contains("(at -0.9375 -0.95 90)"), "{out}");
        // A pad already at 90 + footprint 90 → 180.
        let pad2 = "(pad \"1\" smd\n\t(at 0 0 90)\n)";
        assert!(bump_pad_rotation(pad2, 90).contains("(at 0 0 180)"));
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

    /// The synthesized board parses with `read_problem`, pads land at
    /// placement+offset, nets bind, and `write_solution` then round-trips copper.
    #[test]
    fn synthesized_board_round_trips_through_read_problem_and_write_solution() {
        use crate::pcb::{extract_copper, read_problem, write_solution};
        use pcb_engine::problem::{LayerRef, RouteSolution, Trace};

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
                    pcb_engine::problem::Point2 { x: 9.175, y: 10.0 },
                    pcb_engine::problem::Point2 { x: 20.9375, y: 10.0 },
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
