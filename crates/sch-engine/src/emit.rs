//! Schematic document writer: turns placed symbols into a loadable
//! `.kicad_sch`.
//!
//! This is the first real emission stage. [`SchematicWriter`] accumulates
//! placed symbol instances and the set of `(lib_symbols)` definitions they
//! reference, then [`SchematicWriter::finish`] assembles the full document
//! deterministically (header, `lib_symbols` sorted by `lib_id`, symbol
//! instances sorted by refdes, `sheet_instances`).
//!
//! ## Why the structure is exactly this shape
//!
//! The S-expression layout below is the form proven (via `kicad-cli`) to load
//! in KiCAD 10 by `crates/kicad-bridge/examples/emit_spike.rs`. The
//! load-bearing details:
//!
//! - **`(lib_symbols)` embedding.** Every distinct `lib_id` used contributes
//!   one `(symbol "Lib:Name" …)` block, taken verbatim from
//!   [`SymbolGeometry::raw_definition`] (already retargeted to the
//!   fully-qualified name by `kicad-bridge`). The set is keyed by `lib_id`, so
//!   placing N resistors embeds the `Device:R` body exactly once.
//! - **The `(instances …)` path root.** Each symbol instance carries an
//!   `(instances (project "" (path "/<root-uuid>" (reference …) (unit 1))))`
//!   block whose path is `"/" + the schematic's own root uuid`. KiCAD resolves
//!   a placed symbol's reference/unit through this path; if the root uuid here
//!   does not match the document's `(uuid …)`, the component is not annotated
//!   and drops out of the netlist. So the writer computes the root uuid once
//!   and threads it into every instance.
//! - **Determinism.** Every uuid comes from [`crate::ids::stable_uuid`] and
//!   positions are snapped via [`crate::grid::snap_point`], so re-emitting the
//!   same placements yields byte-identical output (spec §5.1).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;

use kicad_bridge::env::KicadEnv;
use kicad_bridge::geometry::SymbolGeometry;

use crate::grid::snap_point;
use crate::ids::stable_uuid;

/// Stable key identifying *this* schematic sheet for root-uuid derivation.
///
/// Task 3 emits a single root sheet, so a fixed key suffices; later tasks that
/// emit multiple sheets will key the root uuid on sheet identity instead.
const ROOT_SHEET_KEY: &str = "root";

/// One placed symbol instance, captured at `add_symbol` time and rendered in
/// `finish`.
struct Instance {
    lib_id: String,
    refdes: String,
    value: String,
    /// Grid-snapped sheet position.
    at: [f64; 2],
    /// Orientation in degrees (0/90/180/270).
    angle: f64,
}

/// Accumulates placed symbols and emits a deterministic `.kicad_sch` document.
#[derive(Default)]
pub struct SchematicWriter {
    /// `(lib_symbols)` bodies, keyed by `lib_id` for dedup; `BTreeMap` keeps the
    /// emitted set sorted by `lib_id` with no extra sort step.
    lib_symbols: BTreeMap<String, String>,
    /// Placed instances, in insertion order; sorted by refdes at `finish`.
    instances: Vec<Instance>,
}

impl SchematicWriter {
    /// A new, empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Place one symbol instance.
    ///
    /// Loads the symbol's geometry/definition for `lib_id` from `env`, registers
    /// its `(lib_symbols)` body (deduplicated by `lib_id`), and records a placed
    /// instance with the given `refdes`, `value`, position `at` (snapped to the
    /// grid), and `angle` (degrees). Nothing is written until [`Self::finish`].
    ///
    /// Returns the error from [`SymbolGeometry::load`] if the symbol cannot be
    /// resolved.
    pub fn add_symbol(
        &mut self,
        env: &KicadEnv,
        lib_id: &str,
        refdes: &str,
        value: &str,
        at: [f64; 2],
        angle: f64,
    ) -> io::Result<()> {
        // Register the lib_symbol body once per lib_id (dedup).
        if !self.lib_symbols.contains_key(lib_id) {
            let geom = SymbolGeometry::load(env, lib_id)?;
            self.lib_symbols
                .insert(lib_id.to_string(), geom.raw_definition);
        }

        self.instances.push(Instance {
            lib_id: lib_id.to_string(),
            refdes: refdes.to_string(),
            value: value.to_string(),
            at: snap_point(at),
            angle,
        });
        Ok(())
    }

    /// Assemble the complete `.kicad_sch` document as a deterministic string.
    ///
    /// `lib_symbols` are emitted sorted by `lib_id` (via the backing
    /// `BTreeMap`); symbol instances are emitted sorted by refdes. All uuids are
    /// content-derived, so the same placements always produce identical bytes.
    pub fn finish(self) -> String {
        let root_uuid = stable_uuid("sheet", ROOT_SHEET_KEY);

        let mut out = String::new();
        out.push_str("(kicad_sch\n");
        out.push_str("\t(version 20250114)\n");
        out.push_str("\t(generator \"auto-pcb\")\n");
        out.push_str("\t(generator_version \"0.1\")\n");
        let _ = writeln!(out, "\t(uuid \"{root_uuid}\")");
        out.push_str("\t(paper \"A4\")\n");

        // lib_symbols set, sorted by lib_id (BTreeMap order).
        out.push_str("\t(lib_symbols\n");
        for body in self.lib_symbols.values() {
            out.push_str("\t\t");
            out.push_str(body);
            out.push('\n');
        }
        out.push_str("\t)\n");

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
/// KiCAD S-expressions quote string atoms with `"`; a literal backslash or
/// double-quote in the payload must be escaped or the document fails to parse.
/// Order matters: escape backslash first, then the quote, so the backslash we
/// add in front of a quote is not itself doubled.
///
/// Apply this to every LLM-/user-derived string written as `"…"` (e.g. the
/// component value). Do **not** apply it to the verbatim `raw_definition`
/// splice (already valid KiCAD output) or to internally generated tokens
/// (uuids, validated lib_ids).
fn escape_sexpr_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Format a snapped coordinate, canonicalizing `-0.0` to `0.0`.
///
/// Snapping can produce `-0.0`, which `f64`'s `Display` renders as `-0`. That
/// is harmless to KiCAD but breaks byte-for-byte determinism (the same logical
/// position could render as `0` or `-0`), so we collapse negative zero here.
fn fmt_coord(v: f64) -> f64 {
    if v == 0.0 { 0.0 } else { v }
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

    let sym_uuid = stable_uuid("symbol", &inst.refdes);
    // Property text offsets mirror the spike's working layout.
    let ref_x = fmt_coord(x + 2.54);
    let ref_y = fmt_coord(y - 1.27);
    let val_x = fmt_coord(x + 2.54);
    let val_y = fmt_coord(y + 1.27);

    let mut s = String::new();
    s.push_str("\t(symbol\n");
    let _ = writeln!(s, "\t\t(lib_id \"{lib_id}\")");
    let _ = writeln!(s, "\t\t(at {x} {y} {angle})");
    s.push_str("\t\t(unit 1)\n");
    s.push_str("\t\t(exclude_from_sim no)\n");
    s.push_str("\t\t(in_bom yes)\n");
    s.push_str("\t\t(on_board yes)\n");
    s.push_str("\t\t(dnp no)\n");
    let _ = writeln!(s, "\t\t(uuid \"{sym_uuid}\")");
    let _ = writeln!(s, "\t\t(property \"Reference\" \"{refdes}\"");
    let _ = writeln!(s, "\t\t\t(at {ref_x} {ref_y} 0)");
    s.push_str("\t\t\t(effects (font (size 1.27 1.27)) (justify left))\n");
    s.push_str("\t\t)\n");
    let _ = writeln!(s, "\t\t(property \"Value\" \"{value}\"");
    let _ = writeln!(s, "\t\t\t(at {val_x} {val_y} 0)");
    s.push_str("\t\t\t(effects (font (size 1.27 1.27)) (justify left))\n");
    s.push_str("\t\t)\n");
    let _ = writeln!(s, "\t\t(property \"Footprint\" \"\"");
    let _ = writeln!(s, "\t\t\t(at {x} {y} 0)");
    s.push_str("\t\t\t(effects (font (size 1.27 1.27)) (hide yes))\n");
    s.push_str("\t\t)\n");
    let _ = writeln!(
        s,
        "\t\t(instances\n\t\t\t(project \"\"\n\t\t\t\t(path \"/{root_uuid}\"\n\t\t\t\t\t(reference \"{refdes}\")\n\t\t\t\t\t(unit 1)\n\t\t\t\t)\n\t\t\t)\n\t\t)"
    );
    s.push('\n');
    s.push_str("\t)\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn reemit_is_byte_identical() {
        let Some(env) = detect_env() else { return };

        let build = || {
            let mut w = SchematicWriter::new();
            w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
                .unwrap();
            w.add_symbol(&env, "Device:R", "R2", "4.7k", [101.6, 63.5], 90.0)
                .unwrap();
            w.finish()
        };

        assert_eq!(build(), build(), "re-emit must be byte-identical");
    }
}
