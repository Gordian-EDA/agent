//! Grammar-driven emission: banks bus + single ports; divider hangs; ERC and
//! netlist stay truthful. SKIPs without KiCAD.

use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

fn compile(env: &KicadEnv, src: &str) -> circuit_lang::Design {
    let provider = RealSymbolProvider::new(env.clone());
    let result = circuit_lang::compile(src, &provider);
    assert!(!result.diagnostics.has_errors(), "{:?}", result.diagnostics);
    result.design.unwrap()
}

const DIVIDER: &str = "
version: 1
name: divider
rails: [VCC, GND]
blocks:
  div:
    components:
      R7: {part: Device:R, value: 649k, between: [VCC, OUT]}
      R8: {part: Device:R, value: 200k, between: [OUT, GND]}
      C3: {part: Device:C, value: 47n, between: [OUT, GND]}
";

#[test]
fn divider_emits_wired_star_with_clean_erc_and_netlist() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let design = compile(&env, DIVIDER);
    let out = sch_engine::emit_design_reconciled(&env, &design, None, &Default::default()).unwrap();

    assert!(out.sch.contains("(junction"), "node tap needs a junction");
    assert!(out.sch.contains("(label \"OUT\""), "OUT keeps exactly one label");
    assert_eq!(out.sch.matches("(label \"OUT\"").count(), 1);
    assert!(!out.sch.contains("(label \"GND\""));

    let tmp = tempfile::tempdir().unwrap();
    let sch = tmp.path().join("divider.kicad_sch");
    std::fs::write(&sch, &out.sch).unwrap();

    let nl = KicadCli::new(&env).netlist(&sch).unwrap();
    // KiCAD prefixes a *local-label* net with the sheet path ("/OUT"); global
    // power ports stay bare ("VCC"/"GND"). Strip the leading "/" so the
    // connectivity assertions read the logical net name either way.
    let net_of = |r: &str, p: &str| {
        nl.nets
            .iter()
            .find(|n| n.nodes.contains(&(r.to_string(), p.to_string())))
            .map(|n| n.name.trim_start_matches('/').to_string())
            .unwrap_or_default()
    };
    assert_eq!(net_of("R7", "1"), "VCC");
    assert_eq!(net_of("R7", "2"), "OUT");
    assert_eq!(net_of("R8", "1"), "OUT");
    assert_eq!(net_of("R8", "2"), "GND");
    assert_eq!(net_of("C3", "1"), "OUT");

    let erc = KicadCli::new(&env).erc(&sch).unwrap();
    assert_eq!(erc.error_count(), 0, "{:?}", erc.violations);
}

#[test]
fn bank_emits_one_power_symbol_per_bus_and_cluster_drag_survives() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let src = "
version: 1
name: bank
rails: [3V3, GND]
blocks:
  pwr:
    components:
      C1: {part: Device:C, value: 100n, between: [3V3, GND]}
      C2: {part: Device:C, value: 100n, between: [3V3, GND]}
      C3: {part: Device:C, value: 10u, between: [3V3, GND]}
";
    let design = compile(&env, src);
    let out = sch_engine::emit_design_reconciled(&env, &design, None, &Default::default()).unwrap();
    // One +3V3 and one GND power symbol INSTANCE for the whole bank (no
    // per-pin spam). Count instances via `(lib_id …)` — the bare lib name also
    // appears once in the lib_symbols section, which must not be counted.
    assert_eq!(
        out.sch.matches("(lib_id \"power:+3V3\")").count(),
        1,
        "single top port"
    );
    assert_eq!(
        out.sch.matches("(lib_id \"power:GND\")").count(),
        1,
        "single bottom port"
    );

    // Whole-cluster drag: translate every member by (+25.4, +12.7) in the
    // prior, re-emit -> members keep the dragged positions (rigid group).
    let dragged = {
        // Parse C1's emitted position, then rewrite all three (at x y 0) lines.
        // Cheap text surgery is fine here: positions are unique-enough strings.
        let mut s = out.sch.clone();
        for r in ["C1", "C2", "C3"] {
            let at = sch_engine::test_util::symbol_at(&s, r);
            let new = [at[0] + 25.4, at[1] + 12.7];
            s = sch_engine::test_util::replace_symbol_at(&s, r, new);
        }
        s
    };
    let out2 =
        sch_engine::emit_design_reconciled(&env, &design, Some(&dragged), &Default::default())
            .unwrap();
    let c1 = sch_engine::test_util::symbol_at(&out2.sch, "C1");
    let c1_orig = sch_engine::test_util::symbol_at(&out.sch, "C1");
    assert_eq!(c1, [c1_orig[0] + 25.4, c1_orig[1] + 12.7], "drag survived");
}

#[test]
fn degradations_and_sparseness_surface_in_warnings() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    // R1/R2/R3 form a pure cycle -> one cycle-break degradation note.
    let src = "
version: 1
name: cyc
rails: []
blocks:
  fb:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, N2]}
      R2: {part: Device:R, value: 1k, between: [N2, N3]}
      R3: {part: Device:R, value: 1k, between: [N3, N1]}
";
    let design = compile(&env, src);
    let out = sch_engine::emit_design_reconciled(&env, &design, None, &Default::default()).unwrap();
    assert!(
        out.layout_warnings
            .iter()
            .any(|w| w.starts_with("grammar: block fb: cycle")),
        "{:?}",
        out.layout_warnings
    );
}
