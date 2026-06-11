//! The four reference fixtures (docs/validation/references/*.png) emitted from
//! circuit-YAML: ERC-clean, netlist == YAML, structurally wired. SKIPs without
//! KiCAD.

use std::path::Path;

use circuit_lang::provider::SymbolProvider;
use kicad_bridge::cli::{KicadCli, Netlist};
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

const FIXTURES: &[&str] = &[
    "divider-filter",
    "mcp1703-power-entry",
    "555-blinker",
    "uart-level-translator",
];

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../docs/validation/{name}.circuit.yaml"))
}

/// The YAML keys pins by NAME (e.g. `VI`, `VCCA`) but the KiCAD netlist reports
/// pins by NUMBER (e.g. `3`, `1`). Resolve the authored name to the symbol's
/// pin number (number-first, then name — `circuit_lang::provider::find_pin`
/// order) and compare against the netlist node's pin string.
fn nl_pin_matches(
    provider: &RealSymbolProvider,
    lib_id: &str,
    authored: &str,
    nl_pin: &str,
) -> bool {
    if authored == nl_pin {
        return true;
    }
    let Some(sym) = provider.symbol(lib_id) else {
        return false;
    };
    // authored may be a NUMBER or a NAME; translate to the canonical number with
    // circuit-lang's number-first-then-name resolution order (`find_pin`).
    circuit_lang::provider::find_pin(&sym.pins, authored).map(|p| p.number.as_str()) == Some(nl_pin)
}

/// lib_id for a refdes, applying the same short-form expansion circuit-lang does
/// (`R` -> `Device:R` etc.). The fixtures all use fully-qualified lib_ids, so the
/// component's stored `part` is the lib_id verbatim.
fn lib_id_of(comp: &circuit_lang::model::Component) -> &str {
    &comp.part
}

#[test]
fn fixtures_emit_erc_clean_with_truthful_netlists() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = RealSymbolProvider::new(env.clone());
    for name in FIXTURES {
        let src = std::fs::read_to_string(fixture_path(name)).unwrap();
        let result = circuit_lang::compile(&src, &provider);
        assert!(
            !result.diagnostics.has_errors(),
            "{name}: {:#?}",
            result.diagnostics
        );
        let design = result.design.unwrap();
        let out = sch_engine::emit_design_reconciled(&env, &design, None, &Default::default())
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        let tmp = tempfile::tempdir().unwrap();
        let sch = tmp.path().join(format!("{name}.kicad_sch"));
        std::fs::write(&sch, &out.sch).unwrap();

        // ERC clean.
        let erc = KicadCli::new(&env).erc(&sch).unwrap();
        assert_eq!(erc.error_count(), 0, "{name}: {:#?}", erc.violations);

        // Netlist == YAML: every authored pin->net lands on the same net.
        let nl: Netlist = KicadCli::new(&env).netlist(&sch).unwrap();
        // Truthfulness == connectivity equivalence: every pair of authored pins
        // sharing an authored net must land on the SAME netlist net, and the
        // netlist net for an authored *power rail* must carry that rail's name.
        //
        // We intentionally check equivalence rather than name-equality, because
        // KiCAD auto-names any internal net that carries no label (e.g. a 2-node
        // D1.K<->R2 net becomes `Net-(D1-K)`). The connectivity is truthful even
        // though the synthesized name differs from the authored placeholder; an
        // authored net name like `LED_K` is a wiring intent, not a label demand.
        // Power rails DO get global labels, so their netlist name is meaningful
        // and we assert it.
        let power_rails: std::collections::HashSet<&str> = design
            .nets
            .iter()
            .filter(|(_, a)| a.power)
            .map(|(n, _)| n.as_str())
            .collect();
        // authored net name -> the netlist net id each of its pins resolved to.
        let mut authored_to_nl: std::collections::HashMap<&str, Option<usize>> =
            std::collections::HashMap::new();
        for block in design.blocks.values() {
            for (refdes, comp) in &block.components {
                let lib_id = lib_id_of(comp);
                for (pin, target) in &comp.pins {
                    let circuit_lang::model::PinTarget::Net(want) = target else {
                        continue;
                    };
                    let got = nl.nets.iter().position(|n| {
                        n.nodes
                            .iter()
                            .any(|(r, p)| r == refdes && nl_pin_matches(&provider, lib_id, pin, p))
                    });
                    assert!(
                        got.is_some(),
                        "{name}/{refdes}.{pin}: authored to net {want} but no \
                         netlist net carries that pin"
                    );
                    match authored_to_nl.entry(want.as_str()) {
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(got);
                        }
                        std::collections::hash_map::Entry::Occupied(e) => {
                            assert_eq!(
                                *e.get(),
                                got,
                                "{name}: authored net {want} split across netlist \
                                 nets — {refdes}.{pin} landed elsewhere"
                            );
                        }
                    }
                }
            }
        }
        // Power rails must surface under their authored name in the netlist.
        for (want, nl_idx) in &authored_to_nl {
            if power_rails.contains(want) {
                let got = nl_idx.map(|i| nl.nets[i].name.trim_start_matches('/'));
                assert_eq!(
                    got,
                    Some(*want),
                    "{name}: power rail {want} not named in netlist"
                );
            }
        }

        // No power-net text labels anywhere.
        for rail in design.nets.iter().filter(|(_, a)| a.power).map(|(n, _)| n) {
            assert!(
                !out.sch.contains(&format!("(label \"{rail}\"")),
                "{name}: rail {rail} leaked as a text label"
            );
        }
    }
}

#[test]
fn mcp1703_structure() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = RealSymbolProvider::new(env.clone());
    let src = std::fs::read_to_string(fixture_path("mcp1703-power-entry")).unwrap();
    let design = circuit_lang::compile(&src, &provider).design.unwrap();
    let g = sch_engine::grammar::analyze(&design, "power_entry", &provider);
    // U1 anchors; {C1,C2} and {C3,C4} are banks; D1+R2 is one rail-rail chain
    // (3V3 -> ... -> GND).
    assert_eq!(g.anchors, vec!["U1".to_string()], "anchors: {:?}", g.anchors);
    let banks: Vec<_> = g.clusters.iter().flat_map(|c| &c.banks).collect();
    assert_eq!(banks.len(), 2, "{banks:#?}");
    let led = g
        .clusters
        .iter()
        .flat_map(|c| &c.chains)
        .find(|c| c.links.iter().any(|l| l.refdes == "D1"))
        .expect("D1 chain");
    assert_eq!(led.class, sch_engine::grammar::ChainClass::RailRail);
    assert_eq!(led.links[0].a_net, "3V3");
    assert_eq!(led.links.last().unwrap().b_net, "GND");
}

#[test]
fn blinker_structure() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = RealSymbolProvider::new(env.clone());
    let src = std::fs::read_to_string(fixture_path("555-blinker")).unwrap();
    let design = circuit_lang::compile(&src, &provider).design.unwrap();
    let g = sch_engine::grammar::analyze(&design, "blinker", &provider);
    // R1 -> R2 -> C1 chain through the anchor-tapped nets into ONE chain.
    let chain = g
        .clusters
        .iter()
        .flat_map(|c| &c.chains)
        .find(|c| c.links.iter().any(|l| l.refdes == "R1"))
        .expect("R1 chain");
    let refs: Vec<&str> = chain.links.iter().map(|l| l.refdes.as_str()).collect();
    assert_eq!(refs, ["R1", "R2", "C1"]);
}
