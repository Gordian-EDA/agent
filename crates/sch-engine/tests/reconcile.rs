//! Reconciliation milestone (spec §4/§7): the `.kicad_sch` is the source of
//! truth for positions. On re-emit, a component that survives (matched by
//! identity — refdes for authored, `(ap_parent, ap_role, ap_index)` for
//! synthesized) keeps its existing `(at …)`, including any user move; only a
//! brand-new component is auto-placed.
//!
//! The test emits a 2-part design (R1, R2), reads R1's position, rewrites it to
//! a distinctive spot to simulate a user drag, then emits a 3-part design (adds
//! R3) reconciled against that modified base. R1 must keep the distinctive
//! position; R3 must get a fresh placed position. SKIP-graceful when no KiCAD is
//! detected; runs against KiCAD 10 here.

use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;
use kiutils_kicad::SchematicFile;
use sch_engine::{emit_design, emit_design_reconciled};

/// Compile a tiny design from YAML with real symbols so emission resolves.
fn compile(src: &str, provider: &RealSymbolProvider) -> circuit_lang::Design {
    let result = circuit_lang::compile(src, provider);
    assert!(
        !result.diagnostics.has_errors(),
        "compile errors: {:?}",
        result.diagnostics
    );
    result.design.expect("design compiles")
}

/// Two-resistor design.
fn design2(provider: &RealSymbolProvider) -> circuit_lang::Design {
    compile(
        "
version: 1
name: t
rails: [GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
      R2: {part: Device:R, value: 2k, between: [N2, GND]}
",
        provider,
    )
}

/// Same design plus a third resistor R3.
fn design3(provider: &RealSymbolProvider) -> circuit_lang::Design {
    compile(
        "
version: 1
name: t
rails: [GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
      R2: {part: Device:R, value: 2k, between: [N2, GND]}
      R3: {part: Device:R, value: 3k, between: [N3, GND]}
",
        provider,
    )
}

/// Read a symbol's `(at …)` and uuid by refdes from schematic text.
fn read_symbol(text: &str, refdes: &str) -> ([f64; 2], Option<String>) {
    let tmp = tempfile::Builder::new()
        .suffix(".kicad_sch")
        .tempfile()
        .unwrap();
    std::fs::write(tmp.path(), text).unwrap();
    let doc = SchematicFile::read(tmp.path()).expect("kiutils parses emitted schematic");
    let sym = doc
        .ast()
        .symbols
        .iter()
        .find(|s| s.reference.as_deref() == Some(refdes))
        .unwrap_or_else(|| panic!("symbol {refdes} present"));
    (sym.at.expect("symbol has (at …)"), sym.uuid.clone())
}

#[test]
fn surviving_symbols_keep_their_positions() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = RealSymbolProvider::new(env.clone());

    // v1: a 2-part design.
    let d2 = design2(&provider);
    let v1 = emit_design(&env, &d2).unwrap();

    // Find R1's emitted `(at x y a)` line so we can rewrite it to a distinctive
    // spot, simulating a user moving R1 in the KiCAD editor.
    let (r1_at, _) = read_symbol(&v1, "R1");
    let old_at_line = format!("(at {} {} 0)", fmt(r1_at[0]), fmt(r1_at[1]));
    assert!(
        v1.contains(&old_at_line),
        "expected R1's symbol (at …) line {old_at_line:?} in v1 text"
    );
    // R1's symbol block always precedes R2's (instances sorted by refdes), and
    // its `(at …)` is the first occurrence of that exact line in the file. The
    // distinctive spot is grid-aligned (150 * 1.27 mm) so the reconcile snap is a
    // no-op and the assertion is exact.
    let distinctive = [190.5_f64, 190.5_f64];
    let new_at_line = format!("(at {} {} 0)", fmt(distinctive[0]), fmt(distinctive[1]));
    let modified_base = v1.replacen(&old_at_line, &new_at_line, 1);
    assert_ne!(
        modified_base, v1,
        "the user-move rewrite must change the text"
    );

    // Confirm the rewrite landed on R1 specifically.
    let (moved_at, r1_uuid) = read_symbol(&modified_base, "R1");
    assert_eq!(
        moved_at, distinctive,
        "R1 must read at the distinctive spot"
    );

    // v2: a 3-part design reconciled against the user-edited base.
    let d3 = design3(&provider);
    let v2 = emit_design_reconciled(&env, &d3, Some(&modified_base)).unwrap();

    // R1 survives by refdes -> keeps the user move.
    let (r1_v2_at, r1_v2_uuid) = read_symbol(&v2, "R1");
    assert_eq!(
        r1_v2_at, distinctive,
        "R1 must preserve the user-moved position {distinctive:?}, got {r1_v2_at:?}"
    );
    // The surviving symbol reuses the prior uuid so diffs stay minimal.
    assert_eq!(
        r1_v2_uuid, r1_uuid,
        "R1 must reuse its prior uuid across reconcile"
    );

    // R3 is new -> auto-placed somewhere, and not at R1's distinctive spot.
    let (r3_at, _) = read_symbol(&v2, "R3");
    assert_ne!(
        r3_at, distinctive,
        "the new part R3 must be placed by the placer, not at R1's spot"
    );

    // R2 also survives and keeps its v1 position (it was not user-moved).
    let (r2_v1_at, _) = read_symbol(&v1, "R2");
    let (r2_v2_at, _) = read_symbol(&v2, "R2");
    assert_eq!(
        r2_v1_at, r2_v2_at,
        "R2 keeps its prior position across reconcile"
    );
}

/// Format a coordinate the way the emitter does (drops a trailing `.0`).
fn fmt(v: f64) -> String {
    format!("{v}")
}
