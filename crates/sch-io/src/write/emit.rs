//! S-expression serialization: assembling the final `.kicad_sch` document
//! ([`SchematicWriter::finish`]) and the per-element `render_*` helpers, plus
//! string escaping and coordinate formatting.

use std::fmt::Write as _;

use geom::stable_uuid;

use super::{
    Dir, Instance, NoConnect, PinLabel, ROOT_SHEET_KEY, SchematicWriter, field_anchors,
    justify_token,
};

impl SchematicWriter {
    /// The page size for a content-fit `User` page: the maximum x/y extent of
    /// all drawn geometry (symbol bodies, wires, labels, junctions, no-connects)
    /// plus a margin. `None` when there is nothing to draw.
    ///
    /// Geometry is laid out near the origin by the floorplan engine, so the
    /// content fills a page of `max + margin`. The minimum corner is not
    /// subtracted (KiCAD's page origin is the top-left); the floorplan
    /// normalizes content to a small positive margin already.
    fn content_extent(&self) -> Option<[f64; 2]> {
        use crate::label::text_width;
        const PAGE_MARGIN: f64 = 12.7;
        let mut max_x = f64::MIN;
        let mut max_y = f64::MIN;
        let mut acc = |x: f64, y: f64| {
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        };
        for i in &self.instances {
            let h = i.half_extents.rotated_half_extents(i.angle);
            acc(i.at[0] + h[0], i.at[1] + h[1]);
            for p in [i.ref_pos, i.val_pos].into_iter().flatten() {
                acc(p.at[0] + 5.0, p.at[1]);
            }
        }
        for w in &self.wires {
            acc(w.a[0], w.a[1]);
            acc(w.b[0], w.b[1]);
        }
        for l in &self.labels {
            acc(l.at[0] + text_width(&l.net), l.at[1]);
        }
        for j in &self.junctions {
            acc(j.at[0], j.at[1]);
        }
        for nc in &self.no_connects {
            acc(nc.at[0], nc.at[1]);
        }
        if max_x == f64::MIN {
            return None;
        }
        Some([max_x + PAGE_MARGIN, max_y + PAGE_MARGIN])
    }

    /// Size `[w, h]` of the laid-out content, for the multi-block composer's tile
    /// packing. After `prepare`/`reframe` the content's min corner sits at the page
    /// margin `M`, so its extent above the margin is `content_extent - 2·M`. Returns
    /// `None` for an empty writer.
    pub fn content_size(&self) -> Option<[f64; 2]> {
        const M: f64 = 12.7; // `content_extent`'s PAGE_MARGIN / `reframe`'s M
        self.content_extent()
            .map(|[w, h]| [(w - 2.0 * M).max(1.0), (h - 2.0 * M).max(1.0)])
    }

    /// Assemble the complete `.kicad_sch` document as a deterministic string.
    ///
    /// `lib_symbols` are emitted sorted by `lib_id` (via the backing
    /// `BTreeMap`); symbol instances are emitted sorted by refdes. All uuids are
    /// content-derived, so the same placements always produce identical bytes.
    pub fn finish(mut self) -> String {
        self.prepare();

        let root_uuid = stable_uuid("sheet", ROOT_SHEET_KEY);

        let mut out = String::new();
        out.push_str("(kicad_sch\n");
        out.push_str("\t(version 20250114)\n");
        out.push_str("\t(generator \"gordian\")\n");
        out.push_str("\t(generator_version \"0.1\")\n");
        let _ = writeln!(out, "\t(uuid \"{root_uuid}\")");
        // Content-fit page: a custom `User` page just larger than the drawn
        // content so the schematic fills the view (no tiny-in-an-A4-corner).
        // Falls back to A4 when there is no content to measure.
        match self.content_extent() {
            Some([w, h]) => {
                // KiCAD draws the title block inside the page at bottom-right; a tight
                // content-fit page leaves no room, so metadata overprints the lowest parts.
                // Reserve a bottom band whenever we emit a title block.
                const TITLE_BLOCK_RESERVE: f64 = 33.0;
                let reserve = self.title.is_some();
                let h = if reserve { h + TITLE_BLOCK_RESERVE } else { h };
                let _ = writeln!(out, "\t(paper \"User\" {} {})", fmt_coord(w), fmt_coord(h));
            }
            None => out.push_str("\t(paper \"A4\")\n"),
        }
        if let Some(title) = &self.title {
            let t = escape_sexpr_string(title);
            let _ = writeln!(out, "\t(title_block\n\t\t(title \"{t}\")\n\t)");
        }

        // lib_symbols set, sorted by lib_id (BTreeMap order).
        out.push_str("\t(lib_symbols\n");
        for body in self.lib_symbols.values() {
            out.push_str("\t\t");
            out.push_str(body);
            out.push('\n');
        }
        out.push_str("\t)\n");

        // `(no_connect)` markers at intentionally-unconnected pins, sorted by
        // their stable uuid_key for deterministic order/uuids.
        let mut no_connects = self.no_connects;
        no_connects.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        for nc in &no_connects {
            out.push_str(&render_no_connect(nc));
        }

        // Net-name labels at pin endpoints, sorted by their stable uuid_key so
        // the emitted order (and uuids) are deterministic.
        let mut labels = self.labels;
        labels.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        for label in &labels {
            out.push_str(&render_label(label));
        }

        // Wire segments, sorted by uuid_key for deterministic order.
        let mut wires = self.wires;
        wires.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        for wire in &wires {
            let uuid = stable_uuid("wire", &wire.uuid_key);
            let _ = writeln!(
                out,
                "\t(wire\n\t\t(pts\n\t\t\t(xy {} {}) (xy {} {})\n\t\t)\n\t\t(stroke (width 0) (type default))\n\t\t(uuid \"{uuid}\")\n\t)",
                fmt_coord(wire.a[0]),
                fmt_coord(wire.a[1]),
                fmt_coord(wire.b[0]),
                fmt_coord(wire.b[1]),
            );
        }

        // Junction dots, sorted by uuid_key for deterministic order/uuids.
        let mut junctions = self.junctions;
        junctions.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        for j in &junctions {
            let uuid = stable_uuid("junction", &j.uuid_key);
            let _ = writeln!(
                out,
                "\t(junction\n\t\t(at {} {})\n\t\t(diameter 0)\n\t\t(color 0 0 0 0)\n\t\t(uuid \"{uuid}\")\n\t)",
                fmt_coord(j.at[0]),
                fmt_coord(j.at[1]),
            );
        }

        // Free-standing graphic decoration (block titles/notes + frames), both
        // sorted by uuid_key for deterministic order/uuids.
        let mut texts = self.texts;
        texts.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        let mut rects = self.rects;
        rects.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        for t in &texts {
            let body = escape_sexpr_string(&t.text);
            let uuid = stable_uuid("text", &t.uuid_key);
            let weight = if t.bold { " bold" } else { "" };
            let _ = writeln!(
                out,
                "\t(text \"{body}\"\n\t\t(exclude_from_sim no)\n\t\t(at {} {} 0)\n\t\t(effects (font (size {sz} {sz}){weight}) (justify left bottom))\n\t\t(uuid \"{uuid}\")\n\t)",
                fmt_coord(t.at[0]),
                fmt_coord(t.at[1]),
                sz = t.size,
            );
        }
        for r in &rects {
            let uuid = stable_uuid("rect", &r.uuid_key);
            let _ = writeln!(
                out,
                "\t(rectangle\n\t\t(start {} {})\n\t\t(end {} {})\n\t\t(stroke (width 0.1524) (type dash))\n\t\t(fill (type none))\n\t\t(uuid \"{uuid}\")\n\t)",
                fmt_coord(r.start[0]),
                fmt_coord(r.start[1]),
                fmt_coord(r.end[0]),
                fmt_coord(r.end[1]),
            );
        }

        // Symbol instances, sorted by refdes for deterministic output.
        let mut instances = self.instances;
        instances.sort_by(|a, b| a.refdes.cmp(&b.refdes));
        for inst in &instances {
            out.push_str(&render_instance(inst, &root_uuid));
        }

        // A single root sheet.
        out.push_str("\t(sheet_instances\n");
        out.push_str("\t\t(path \"/\"\n");
        out.push_str("\t\t\t(page \"1\")\n");
        out.push_str("\t\t)\n");
        out.push_str("\t)\n");

        out.push_str(")\n");
        out
    }
}

/// Escape a free-form string for embedding inside a double-quoted S-expr atom.
///
/// KiCAD S-expressions quote string atoms with `"`; a literal backslash,
/// double-quote, or control character (newline, carriage return, tab) in the
/// payload must be escaped or the document fails to parse. Order matters:
/// escape backslash first so the backslashes we add for the other cases are
/// not themselves doubled.
///
/// Apply this to every LLM-/user-derived string written as `"…"` (e.g. the
/// component value). Do **not** apply it to the verbatim `raw_definition`
/// splice (already valid KiCAD output) or to internally generated tokens
/// (uuids, validated lib_ids).
fn escape_sexpr_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// Format a snapped coordinate, canonicalizing `-0.0` to `0.0`.
///
/// Snapping can produce `-0.0`, which `f64`'s `Display` renders as `-0`. That
/// is harmless to KiCAD but breaks byte-for-byte determinism (the same logical
/// position could render as `0` or `-0`), so we collapse negative zero here.
pub fn fmt_coord(v: f64) -> f64 {
    if v == 0.0 { 0.0 } else { v }
}

/// Render one net-name label at a pin endpoint into a `(label …)` block.
///
/// The net name is free-form (LLM-/user-derived), so it is escaped before
/// embedding. The label's rotation + justification derive from its `dir` so the
/// text reads *away* from the symbol body along the stub: East→0/left,
/// West→180/right, North→90/left, South→270/right. (Rotation does not affect
/// connectivity — a label binds to whatever pin shares its `(at …)` — only how
/// the text reads.) The `East` case is byte-identical to the pre-stub output
/// (angle 0, justify left bottom). The uuid is content-derived from the label's
/// stable key for byte-identical re-emission.
fn render_label(label: &PinLabel) -> String {
    let x = fmt_coord(label.at[0]);
    let y = fmt_coord(label.at[1]);
    let net = escape_sexpr_string(&label.net);
    let uuid = stable_uuid("label", &label.uuid_key);
    let (angle, justify) = match label.dir {
        Dir::East => (0, "left"),
        Dir::West => (180, "right"),
        Dir::North => (90, "left"),
        Dir::South => (270, "right"),
    };

    let mut s = String::new();
    if label.global {
        // A port: render the off-sheet I/O pentagon. `bidirectional` suits a
        // generic board-edge signal and KiCAD does not flag a global label as an
        // isolated single-pin net (it is, by definition, a cross-sheet link).
        let _ = writeln!(s, "\t(global_label \"{net}\"");
        let _ = writeln!(s, "\t\t(shape bidirectional)");
        let _ = writeln!(s, "\t\t(at {x} {y} {angle})");
        let _ = writeln!(
            s,
            "\t\t(effects (font (size 1.27 1.27)) (justify {justify}))"
        );
        let _ = writeln!(s, "\t\t(uuid \"{uuid}\")");
        s.push_str("\t)\n");
        return s;
    }
    let _ = writeln!(s, "\t(label \"{net}\"");
    let _ = writeln!(s, "\t\t(at {x} {y} {angle})");
    let _ = writeln!(
        s,
        "\t\t(effects (font (size 1.27 1.27)) (justify {justify} bottom))"
    );
    let _ = writeln!(s, "\t\t(uuid \"{uuid}\")");
    s.push_str("\t)\n");
    s
}

/// Render one `(no_connect …)` marker at a pin endpoint.
///
/// The marker carries only its `(at …)` position and a content-derived uuid. Its
/// position must coincide with the pin's connection endpoint (the same point a
/// label would attach to) for KiCAD to associate it with that pin and suppress
/// the unconnected-pin ERC report.
fn render_no_connect(nc: &NoConnect) -> String {
    let x = fmt_coord(nc.at[0]);
    let y = fmt_coord(nc.at[1]);
    let uuid = stable_uuid("no_connect", &nc.uuid_key);

    let mut s = String::new();
    let _ = writeln!(s, "\t(no_connect");
    let _ = writeln!(s, "\t\t(at {x} {y})");
    let _ = writeln!(s, "\t\t(uuid \"{uuid}\")");
    s.push_str("\t)\n");
    s
}

/// Render one placed symbol instance into its `(symbol …)` S-expression block.
///
/// The instance uuid is keyed on the refdes; the property/effects layout and
/// the `(instances (project "" (path "/<root-uuid>" …)))` block match the form
/// proven to load + netlist in the emission spike. The instance path root uuid
/// is the schematic's own `root_uuid` — this is what binds the placement to its
/// reference/unit annotation.
fn render_instance(inst: &Instance, root_uuid: &str) -> String {
    let x = fmt_coord(inst.at[0]);
    let y = fmt_coord(inst.at[1]);
    let angle = inst.angle;
    let lib_id = &inst.lib_id;
    // Free-form, LLM-/user-derived strings must be escaped before embedding.
    let refdes = escape_sexpr_string(&inst.refdes);
    let value = escape_sexpr_string(&inst.value);

    // Reuse the prior instance uuid for a surviving symbol (minimal diff on
    // reconcile); otherwise derive it from the refdes for byte-identical re-emit.
    // A multi-unit part places several instances under one refdes, so units >1
    // take a unit-distinguished key to keep instance uuids unique. Unit 1 keeps
    // the bare-refdes key so single-unit parts stay byte-identical.
    let sym_uuid = inst.uuid.clone().unwrap_or_else(|| {
        if inst.unit <= 1 {
            stable_uuid("symbol", &inst.refdes)
        } else {
            stable_uuid("symbol", &format!("{}#u{}", inst.refdes, inst.unit))
        }
    });
    // Field anchors: solver-assigned when present, else the fallback fixed
    // right-of-body offset (text clear of the glyph via the half-extent).
    let (rp, vp) = field_anchors(inst);
    let (ref_at, ref_j) = (rp.at, rp.justify);
    let (val_at, val_j) = (vp.at, vp.justify);
    let (ref_x, ref_y) = (fmt_coord(ref_at[0]), fmt_coord(ref_at[1]));
    let (val_x, val_y) = (fmt_coord(val_at[0]), fmt_coord(val_at[1]));
    // KiCAD renders a field's text angle RELATIVE to the symbol's rotation,
    // with an auto-flip that already keeps 180-rotated text readable. So a
    // 90/270 symbol needs the inverse angle to render horizontal text, while
    // 0/180 symbols take 0 (compensating 180 with 180 renders upside-down —
    // verified empirically against kicad-cli 10.0.3). The solver models all
    // field text as horizontal, so this keeps geometry and render in sync.
    let field_angle = match inst.angle.rem_euclid(360.0) as i32 {
        90 => 270,
        270 => 90,
        _ => 0,
    };

    // Hide Reference for power/flag symbols whose refdes is `#`-prefixed
    // (KiCAD convention: #PWR…, #FLG…) — they must not appear in the netlist
    // component list or on the visible schematic.
    let hide_ref = inst.refdes.starts_with('#');
    // Hide the Value of PWR_FLAG symbols (keyed on lib_id) — the graphic makes
    // the flag self-evident and the "PWR_FLAG" string would clutter power rail
    // junctions. Also hide solver-suppressed values (`val_hidden`).
    let hide_val = inst.lib_id == "power:PWR_FLAG" || inst.val_hidden;

    let mut s = String::new();
    s.push_str("\t(symbol\n");
    let _ = writeln!(s, "\t\t(lib_id \"{lib_id}\")");
    let _ = writeln!(s, "\t\t(at {x} {y} {angle})");
    // A left-right flip negates local x — that is `(mirror y)` in KiCAD — matching
    // `Point2::transform_offset`.
    if inst.mirror {
        s.push_str("\t\t(mirror y)\n");
    }
    let _ = writeln!(s, "\t\t(unit {})", inst.unit);
    s.push_str("\t\t(exclude_from_sim no)\n");
    s.push_str("\t\t(in_bom yes)\n");
    s.push_str("\t\t(on_board yes)\n");
    s.push_str("\t\t(dnp no)\n");
    let _ = writeln!(s, "\t\t(uuid \"{sym_uuid}\")");
    let _ = writeln!(s, "\t\t(property \"Reference\" \"{refdes}\"");
    let _ = writeln!(s, "\t\t\t(at {ref_x} {ref_y} {field_angle})");
    if hide_ref {
        let _ = writeln!(
            s,
            "\t\t\t(effects (font (size 1.27 1.27)){} (hide yes))",
            justify_token(ref_j)
        );
    } else {
        let _ = writeln!(
            s,
            "\t\t\t(effects (font (size 1.27 1.27)){})",
            justify_token(ref_j)
        );
    }
    s.push_str("\t\t)\n");
    let _ = writeln!(s, "\t\t(property \"Value\" \"{value}\"");
    let _ = writeln!(s, "\t\t\t(at {val_x} {val_y} {field_angle})");
    if hide_val {
        let _ = writeln!(
            s,
            "\t\t\t(effects (font (size 1.27 1.27)){} (hide yes))",
            justify_token(val_j)
        );
    } else {
        let _ = writeln!(
            s,
            "\t\t\t(effects (font (size 1.27 1.27)){})",
            justify_token(val_j)
        );
    }
    s.push_str("\t\t)\n");
    let footprint = escape_sexpr_string(inst.footprint.as_deref().unwrap_or(""));
    let _ = writeln!(s, "\t\t(property \"Footprint\" \"{footprint}\"");
    let _ = writeln!(s, "\t\t\t(at {x} {y} 0)");
    s.push_str("\t\t\t(effects (font (size 1.27 1.27)) (hide yes))\n");
    s.push_str("\t\t)\n");

    // Hidden reconciliation identity tags (`ap_*`), in insertion order. These
    // make the file self-describing: on the next lift/reconcile a synthesized
    // part is matched by `(ap_parent, ap_role, ap_index)` and every part by its
    // block. They are hidden so they never clutter the schematic visually. Keys
    // and values are internally generated (block/role names, refdes, indices),
    // but escaping them is cheap insurance against odd block names.
    for (key, val) in &inst.extra_props {
        let k = escape_sexpr_string(key);
        let v = escape_sexpr_string(val);
        let _ = writeln!(s, "\t\t(property \"{k}\" \"{v}\"");
        let _ = writeln!(s, "\t\t\t(at {x} {y} 0)");
        s.push_str("\t\t\t(effects (font (size 1.27 1.27)) (hide yes))\n");
        s.push_str("\t\t)\n");
    }

    let _ = writeln!(
        s,
        "\t\t(instances\n\t\t\t(project \"\"\n\t\t\t\t(path \"/{root_uuid}\"\n\t\t\t\t\t(reference \"{refdes}\")\n\t\t\t\t\t(unit {unit})\n\t\t\t\t)\n\t\t\t)\n\t\t)",
        unit = inst.unit
    );
    s.push('\n');
    s.push_str("\t)\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use kicad_env::KicadEnv;

    /// `add_symbol` needs a real symbol library to resolve geometry, so these
    /// tests SKIP-gracefully when no KiCAD environment is detected.
    fn detect_env() -> Option<KicadEnv> {
        match KicadEnv::detect() {
            Some(env) => Some(env),
            None => {
                eprintln!("SKIP: no KiCAD environment detected");
                None
            }
        }
    }

    #[test]
    fn writes_footprint_property_from_instance() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol_full(
            &env,
            "Device:C",
            "C1",
            "100nF",
            [127.0, 63.5],
            0.0,
            Some("Capacitor_SMD:C_0603_1608Metric"),
            &[],
            None,
        )
        .unwrap();
        let text = w.finish();
        assert!(
            text.contains("(property \"Footprint\" \"Capacitor_SMD:C_0603_1608Metric\""),
            "emitted Footprint property must carry the lib_id:\n{text}"
        );
    }

    #[test]
    fn omitted_footprint_emits_empty_property() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        // The 6-arg convenience passes no footprint -> empty property (unchanged behaviour).
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
            .unwrap();
        let text = w.finish();
        assert!(
            text.contains("(property \"Footprint\" \"\""),
            "an unassigned part still emits an empty Footprint property:\n{text}"
        );
    }

    #[test]
    fn escapes_free_form_strings_in_output() {
        let Some(env) = detect_env() else { return };

        // A value containing a double-quote (e.g. inches) must be escaped so the
        // emitted S-expr stays well-formed. LLM-derived values make this real.
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "4.7\"", [127.0, 63.5], 0.0)
            .unwrap();
        let text = w.finish();

        // The quote inside the value must be backslash-escaped in the output.
        assert!(
            text.contains("4.7\\\""),
            "value quote must be escaped (expected `4.7\\\"`):\n{text}"
        );

        // And the result must still parse as a valid KiCAD schematic.
        let tmp = tempfile::Builder::new()
            .suffix(".kicad_sch")
            .tempfile()
            .unwrap();
        std::fs::write(tmp.path(), &text).unwrap();
        kiutils_kicad::SchematicFile::read(tmp.path())
            .expect("kiutils must parse output with an escaped value");
    }

    #[test]
    fn escape_sexpr_string_backslash_then_quote() {
        // Backslash is escaped first, then quote — order matters so that an
        // escaped quote's backslash is not itself re-escaped.
        assert_eq!(escape_sexpr_string("a"), "a");
        assert_eq!(escape_sexpr_string("4.7\""), "4.7\\\"");
        assert_eq!(escape_sexpr_string("a\\b"), "a\\\\b");
        // `\"` in the input becomes `\\\"` (backslash escaped, then quote escaped).
        assert_eq!(escape_sexpr_string("\\\""), "\\\\\\\"");
    }

    #[test]
    fn escape_sexpr_string_control_chars() {
        // Newline, carriage return and tab map to their two-char escapes.
        assert_eq!(escape_sexpr_string("a\nb"), "a\\nb");
        assert_eq!(escape_sexpr_string("a\rb"), "a\\rb");
        assert_eq!(escape_sexpr_string("a\tb"), "a\\tb");
        // A literal backslash-n stays distinct: `\` is doubled, the `n` is left
        // alone, so it cannot be confused with an escaped newline.
        assert_eq!(escape_sexpr_string("a\\nb"), "a\\\\nb");
    }

    #[test]
    fn titled_user_page_reserves_bottom_title_block() {
        let mut w = SchematicWriter::new();
        w.set_title("my_board");
        w.add_junction([25.4, 25.4]);

        let text = w.finish();
        let paper = text
            .lines()
            .find(|line| line.contains("(paper \"User\""))
            .expect("titled content should emit a custom User page");
        let nums: Vec<f64> = paper
            .split_whitespace()
            .filter_map(|token| token.trim_end_matches(')').parse::<f64>().ok())
            .collect();

        assert_eq!(
            nums.len(),
            2,
            "paper dimensions should parse from {paper:?}"
        );
        assert!(
            nums[1] >= 70.0,
            "title block reserve must extend page height, got line {paper:?}"
        );
    }

    #[test]
    fn reemit_is_deterministic() {
        let Some(env) = detect_env() else { return };

        let build = || {
            let mut w = SchematicWriter::new();
            w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
                .unwrap();
            w.add_symbol(&env, "Device:R", "R2", "4.7k", [101.6, 63.5], 90.0)
                .unwrap();
            w.finish()
        };

        assert_eq!(build(), build(), "re-emit must be deterministic");
    }

    #[test]
    fn label_orientation_per_direction() {
        let mk = |dir| PinLabel {
            net: "X".into(),
            at: [0.0, 0.0].into(),
            uuid_key: "k".into(),
            dir,
            stub: None,
            global: false,
        };
        assert!(render_label(&mk(Dir::East)).contains("(at 0 0 0)"));
        assert!(render_label(&mk(Dir::East)).contains("justify left"));
        assert!(render_label(&mk(Dir::West)).contains("(at 0 0 180)"));
        assert!(render_label(&mk(Dir::West)).contains("justify right"));
        assert!(render_label(&mk(Dir::North)).contains("(at 0 0 90)"));
        assert!(render_label(&mk(Dir::South)).contains("(at 0 0 270)"));
        assert!(render_label(&mk(Dir::North)).contains("justify left"));
        assert!(render_label(&mk(Dir::South)).contains("justify right"));
    }

    #[test]
    fn junctions_render_sorted_and_deduped() {
        let mut w = SchematicWriter::new();
        w.add_junction([50.8, 25.4]);
        w.add_junction([25.4, 25.4]);
        w.add_junction([50.8, 25.4]); // duplicate -> dropped
        let sch = w.finish();
        let count = sch.matches("(junction").count();
        assert_eq!(count, 2);
        let first = sch.find("(at 25.4 25.4)").unwrap();
        let second = sch.find("(at 50.8 25.4)").unwrap();
        assert!(first < second, "junctions sorted by uuid_key");
    }

    #[test]
    fn rotated_symbol_fields_render_horizontal() {
        // KiCAD field angles are relative to the symbol rotation; a 90-degree
        // symbol must carry 270-degree fields so the text reads horizontal.
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [101.6, 101.6], 90.0)
            .unwrap();
        let sch = w.finish();
        let seg = sch.split("(property \"Reference\" \"R1\"").nth(1).unwrap();
        let at_line = seg.lines().nth(1).unwrap();
        assert!(
            at_line.trim_end().ends_with(" 270)"),
            "90-degree symbol fields must compensate to 270, got {at_line:?}"
        );
    }

    #[test]
    fn solver_is_idempotent_across_finish() {
        let Some(env) = detect_env() else { return };
        let build = |presolve: bool| {
            let mut w = SchematicWriter::new();
            w.add_symbol(&env, "Device:R", "R1", "1k", [101.6, 101.6], 0.0)
                .unwrap();
            w.add_symbol(&env, "Device:R", "R2", "2k", [111.76, 101.6], 0.0)
                .unwrap();
            w.add_signal_label(&env, "R1", "1", "SIG").unwrap();
            if presolve {
                w.retract_colliding_stubs();
                w.solve_text_positions();
            }
            w.finish()
        };
        assert_eq!(
            build(false),
            build(true),
            "pre-solving must not change output"
        );
    }

    #[test]
    fn retracted_label_keeps_outward_direction() {
        // Device:R pin 1 at angle 0 points North; a foreign wire across the
        // stub end forces retraction. The label must land on the pin endpoint
        // KEEPING dir North (angle 90 in the rendered label) so the text still
        // reads away from the body — not reset to East across the pin line.
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
            .unwrap();
        w.add_signal_label(&env, "R1", "1", "SIG").unwrap();
        // Foreign wire through the stub end (127.0, 55.88).
        w.add_wire_on_net([121.92, 55.88], [132.08, 55.88], "OTHER");
        let sch = w.finish();
        assert!(
            sch.contains("(label \"SIG\"\n\t\t(at 127 59.69 90)"),
            "retracted label keeps its North orientation:\n{sch}"
        );
    }

    #[test]
    fn same_net_wire_touch_survives_foreign_retracts() {
        // Device:R pin 1 at (127, 63.5) angle 0:
        //   pin endpoint = (127.0, 59.69)  (inst_y - 3.81)
        //   stub direction = North, STUB_MM = 3.81 -> stub end = (127.0, 55.88)
        //
        // Device:R pin 1 at (177.8, 63.5) angle 0:
        //   pin endpoint = (177.8, 59.69)
        //   stub end = (177.8, 55.88)
        //
        // A horizontal SIG cluster wire running through R1's stub end (127.0, 55.88)
        // is same-net -> R1's stub must survive (label stays at stub end, not pin
        // endpoint). A horizontal OTHER cluster wire running through R2's stub end
        // (177.8, 55.88) is foreign -> R2's stub retracts (label snaps to pin ep).
        let Some(env) = KicadEnv::detect() else {
            eprintln!("SKIP: no KiCAD environment detected");
            return;
        };

        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
            .unwrap();
        w.add_symbol(&env, "Device:R", "R2", "1k", [177.8, 63.5], 0.0)
            .unwrap();
        w.add_signal_label(&env, "R1", "1", "SIG").unwrap();
        w.add_signal_label(&env, "R2", "1", "SIG").unwrap();

        // SIG wire spans R1's stub end at y=55.88 -> same-net, stub survives.
        w.add_wire_on_net([121.92, 55.88], [132.08, 55.88], "SIG");
        // OTHER wire spans R2's stub end at y=55.88 -> foreign, stub retracts.
        w.add_wire_on_net([172.72, 55.88], [182.88, 55.88], "OTHER");

        let sch = w.finish();

        // R2's stub retracted: its SIG label must now sit at R2's pin endpoint (177.8, 59.69).
        assert!(
            sch.contains("(label \"SIG\"\n\t\t(at 177.8 59.69"),
            "R2 label must retract to its pin endpoint (177.8, 59.69):\n{sch}"
        );

        // R1's stub survived: its SIG label must NOT sit at R1's pin endpoint (127, 59.69).
        // (It should be at the stub end (127, 55.88) instead.)
        assert!(
            !sch.contains("(label \"SIG\"\n\t\t(at 127 59.69"),
            "R1 label must NOT retract to pin endpoint (127, 59.69) — same-net wire should allow the stub to survive:\n{sch}"
        );
    }

    #[test]
    fn u1_fields_dodge_out_label() {
        let Some(env) = KicadEnv::detect() else {
            eprintln!("SKIP");
            return;
        };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Timer:NE555P", "U1", "", [45.72, 45.72], 0.0)
            .unwrap();
        // KiCad 9's Timer:NE555P names the output pin "Q" (older libs used "OUT").
        w.add_signal_label(&env, "U1", "Q", "N_Q").unwrap();
        let sch = w.finish();
        let seg = sch.split("(property \"Reference\" \"U1\"").nth(1).unwrap();
        let at = seg.lines().nth(1).unwrap();
        println!("U1 ref at: {at}");
        assert!(
            !at.contains("(at 59.69"),
            "U1 ref must not sit on the N_Q label:\n{at}"
        );
    }
}
