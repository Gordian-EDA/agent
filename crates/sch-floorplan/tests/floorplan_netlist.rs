//! Connectivity regression oracle for the FLOORPLAN engine (the active layout
//! path). For each reference fixture, compile the YAML, lay it out per its
//! `*.layout.json` IR sidecar, emit, export the netlist via kicad-cli, and
//! assert the netlist is TRUTHFUL: every authored pin lands connected, no
//! authored net is split across netlist nets, and no two authored nets are
//! shorted onto one.
//!
//! This guards the wire-split finalize pass (`SchematicWriter::split_wires_at_nodes`):
//! KiCAD's netlister only connects wires at shared endpoints, so a mid-span tap
//! on an unsplit through-wire silently disconnects — the schematic renders fine
//! but netlists wrong. Without this test that class of bug is invisible.
//!
//! SKIPs without KiCAD.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

/// The floorplan engine reads `MULTISHEET_REFINE` from the PROCESS environment deep
/// in the emit path, so a test that toggles it must not run concurrently with one
/// that reads it. Every env-sensitive test in this file takes this lock for its whole
/// body; the multisheet variant additionally sets+restores the var inside the locked
/// region, so the two single-sheet-mode tests never observe it mid-flight. (Tests
/// serialize, but each is the same ~8 min either way — correctness over parallelism.)
static ENV_LOCK: Mutex<()> = Mutex::new(());

use kicad_cli::cli::{KicadCli, Netlist};
use kicad_env::KicadEnv;
use kicad_symbol::SymbolTable;
use sch_floorplan::floorplan::{self, LayoutIr};

/// TIER 1 — the hand-tuned reference targets. Held to the FULL bar: electrically
/// truthful AND zero layout warnings AND ERC-clean. These match the human
/// references, so any regression must show up here.
const REFERENCE_FIXTURES: &[&str] =
    &["divider-filter", "mcp1703-power-entry", "555-blinker", "uart-level-translator"];

/// TIER 2 — deliberately HARD circuits (large MCUs, BGAs, RF, mixed-signal) that
/// stress the engine far past the tuned cases. The aesthetic bar is NOT expected
/// to be met (a 100-pin part will lay out rough), so the layout-warning check is
/// relaxed — but the CORRECTNESS invariants are non-negotiable: every authored
/// pin lands connected, no authored net splits or merges, geometry stays on-grid,
/// and no real ERC errors. This is the coverage that catches a connectivity or
/// finalize bug the four clean fixtures are too small to surface.
const CHALLENGE_FIXTURES: &[&str] = &[
    "bedrock-oneshot-bluepill",      // STM32H743 100-pin LQFP dev board
    "bedrock-selfrepair-bluepill",   // STM32 + HSE/32k crystals
    "rf-lna-frontend",               // ADL5542 RF gain block + coax (HF/RF)
    "mixed-signal-adc-frontend",     // MCP6002 op-amp -> ADS1115 I2C ADC (mixed-signal)
    "bga-fpga-ice40",                // ICE40HX8K-BG121 121-ball BGA, dual-rail (BGA)
    "grid-demo",                     // authored `layout:` 2D grid (NE555 blinker)
    "idiom-stm32",                   // distributed local grounds (≥2 GND symbols) + idioms
];

fn doc(name: &str, ext: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../docs/validation/{name}.{ext}"))
}

/// The YAML keys pins by NAME; the KiCAD netlist reports pins by NUMBER. Resolve
/// the authored token (number-first then name, `find_pin` order) and compare.
fn nl_pin_matches(provider: &SymbolTable, lib_id: &str, authored: &str, nl_pin: &str) -> bool {
    if authored == nl_pin {
        return true;
    }
    let Some(sym) = provider.symbol(lib_id) else { return false };
    circuit_lang::provider::find_pin(&sym.pins, authored).map(|p| p.number.as_str()) == Some(nl_pin)
}

#[test]
fn floorplan_reference_fixtures_emit_truthful_netlists() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = SymbolTable::from_env(&env);
    for name in REFERENCE_FIXTURES {
        validate_fixture(&env, &provider, name, /* strict_warnings */ true);
    }
}

#[test]
fn floorplan_challenge_fixtures_emit_truthful_netlists() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    run_challenge_fixtures();
}

/// HARDENED GATE: the SAME challenge fixtures, but emitted through the PRODUCTION
/// `MULTISHEET_REFINE` finalize path (the path `compose_single_sheet` — the agent's real
/// board flow — uses for each block). The default variant above runs the engine in single-sheet
/// mode, which DOES NOT EXERCISE the multisheet-only finalize passes (distributed
/// rails, the driven-rail star, the dead-last re-gathers). Those passes can route a
/// rail/trunk wire through an IC body or a column of foreign pins, merging two nets
/// into one — a real SHORT that ships on agent boards but that the default run is
/// structurally blind to. This variant closes that blind spot: it sets the env var
/// (inside the ENV_LOCK so the single-sheet tests never observe it) and asserts the
/// finalize-path netlists are still truthful. Regression-tested by reverting the
/// emit_rail driven-star spread guard: this test FAILS, the default one PASSES.
#[test]
fn floorplan_challenge_fixtures_emit_truthful_netlists_multisheet() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Set the production finalize flag for the duration of this locked region, then
    // RESTORE it so a later (lock-serialized) single-sheet test sees the original env.
    // SAFETY (edition 2024 `set_var`/`remove_var` are unsafe): every env-sensitive
    // test in this file holds `ENV_LOCK` for its whole body, so no other thread reads
    // or writes the process environment while we mutate it here.
    let prev = std::env::var_os("MULTISHEET_REFINE");
    unsafe { std::env::set_var("MULTISHEET_REFINE", "1") };
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(run_challenge_fixtures));
    unsafe {
        match prev {
            Some(v) => std::env::set_var("MULTISHEET_REFINE", v),
            None => std::env::remove_var("MULTISHEET_REFINE"),
        }
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

/// Run the challenge tier over every (or `FLOORPLAN_ONLY`-restricted) fixture in the
/// CURRENT engine mode (single-sheet, or multisheet when the caller set the env var).
fn run_challenge_fixtures() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = SymbolTable::from_env(&env);
    // Collect EVERY fixture's verdict (don't stop at the first failure) so one run
    // reports the full coverage picture across all hard circuit classes. A fixture
    // that panics on a real truthfulness/ERC defect is recorded; the test fails at
    // the end listing all offenders. `KNOWN_TRUTHFULNESS_BUGS` carries fixtures
    // whose failure documents a real, tracked engine bug (not a flaky test) so the
    // suite stays green for the validated coverage while the bug is on record.
    // `FLOORPLAN_ONLY=bga-fpga-ice40,...` restricts the run to a subset, for fast
    // iteration on one hard fixture (the full set is ~9 min). Empty = all.
    let only = std::env::var("FLOORPLAN_ONLY").unwrap_or_default();
    let only: Vec<&str> = only.split(',').filter(|s| !s.is_empty()).collect();
    let mut failures: Vec<String> = Vec::new();
    for name in CHALLENGE_FIXTURES {
        if !only.is_empty() && !only.contains(name) {
            continue;
        }
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            validate_fixture(&env, &provider, name, /* strict_warnings */ false)
        }));
        match (res.is_ok(), KNOWN_TRUTHFULNESS_BUGS.contains(name)) {
            (true, false) => {}
            (true, true) => panic!(
                "{name}: now PASSES but is still listed in KNOWN_TRUTHFULNESS_BUGS — \
                 remove it (the engine bug it tracked is fixed)"
            ),
            (false, true) => eprintln!("KNOWN-BUG (tracked, tolerated): {name} not yet truthful"),
            (false, false) => failures.push(name.to_string()),
        }
    }
    assert!(
        failures.is_empty(),
        "challenge fixtures emitted UNTRUTHFUL netlists (new, untracked): {failures:?}"
    );
}

/// Challenge fixtures whose emitted netlist is NOT yet truthful because of a REAL,
/// tracked engine bug. Tolerated so the suite stays green for the rest of the
/// coverage; each must have a documented root cause. **Empty = the goal, and we
/// are there:** all six challenge fixtures now emit truthful netlists. The bugs
/// the expanded coverage surfaced, and how each was closed:
///
///   - `mixed-signal-adc-frontend` and `bga-fpga-ice40` — **multi-unit symbols
///     were electrically broken**: the engine emitted only UNIT 1's pins, dropping
///     every other unit (the MCP6002's V-/V+ on unit B, the ICE40's power/IO balls
///     on units B-E). FIXED: `gather` splits a multi-unit part into one Item per
///     used unit and the writer emits each as a distinct `(unit N)` instance, so
///     every unit's pins reach the netlist.
///   - `bga-fpga-ice40` (rail follow-up) and `bedrock-selfrepair-bluepill` — two
///     rails whose vertical risers shared a column merged into one net: stacked
///     BGA balls (GND below / 1V2 above) and a vertical 3-pin header J1
///     (VBUS/3V3/GND on adjacent pins) both put two rails' risers in one column,
///     overlapping in y. FIXED: `plan_riser_offsets` fans colliding risers into
///     separate lanes so the verticals never become collinear. Both fixtures now
///     emit a real, un-merged GND/VBUS/3V3.
const KNOWN_TRUTHFULNESS_BUGS: &[&str] = &[];

/// ERC violation kinds the CHALLENGE tier tolerates (the reference tier forbids
/// ALL of them). These are HYGIENE lints that a rough-but-truthful layout of a
/// hard/partially-wired part trips, NOT netlist-correctness failures — the
/// truthfulness (no-merge/-split, every authored pin connected) and on-grid
/// checks remain the hard gate:
///   - `lib_symbol_issues`: standalone-ERC artifact (no symbol-lib-table).
///   - `pin_to_pin` / `power_pin_not_driven`: redundant PWR_FLAG vs a regulator VO
///     it can't see through the symbol's `extends` chain.
///   - `pin_not_connected` / `pin_not_driven`: an UNASSIGNED IC pin (a BGA ball the
///     fixture doesn't wire) or a single-pin off-sheet port; truthfulness still
///     verifies every AUTHORED pin reaches its net.
///   - `no_connect_dangling` / `no_connect_connected`: NC-marker placement on a
///     dense multi-unit part (a layout-quality follow-up, not a wiring error).
///   - `multiple_net_names`: two labels on one net.
const TOLERATED_ERC_KINDS: &[&str] = &[
    "lib_symbol_issues",
    "pin_to_pin",
    "power_pin_not_driven",
    "pin_not_connected",
    "pin_not_driven",
    "no_connect_dangling",
    "no_connect_connected",
    "multiple_net_names",
];

/// Compile `<name>.circuit.yaml`, emit through the floorplan engine (sidecar IR if
/// present, else `baseline_ir`), and assert the emitted sheet is electrically
/// TRUTHFUL + on-grid + ERC-clean. With `strict_warnings`, also assert zero
/// layout warnings (tier-1 readability bar).
fn validate_fixture(
    env: &KicadEnv,
    provider: &SymbolTable,
    name: &str,
    strict_warnings: bool,
) {
    {
        let src = std::fs::read_to_string(doc(name, "circuit.yaml")).unwrap();
        let result = circuit_lang::compile(&src, provider);
        assert!(!result.diagnostics.has_errors(), "{name}: {:#?}", result.diagnostics);
        let design = result.design.unwrap();

        let ir = match std::fs::read_to_string(doc(name, "layout.json")) {
            Ok(s) => LayoutIr::from_json(&s).unwrap(),
            Err(_) => floorplan::baseline_ir(&design),
        };
        let out = floorplan::emit_strategy(env, &design, &ir, Box::new(greedy_place::Greedy)).unwrap_or_else(|e| panic!("{name}: {e}"));

        // Readability invariant (tier-1 only): the reference fixtures emit with ZERO
        // layout warnings (no symbol/text overlap, no value-text smeared onto a
        // neighbour, no wire through a body). This guards the IC-MPN placement, spine
        // collinearity, and IC-body-crossing work — any of which regressing would
        // re-introduce a warning here long before a human re-renders. The hard
        // challenge fixtures are exempt (a 100-pin part lays out rough on purpose).
        if strict_warnings {
            assert!(
                out.layout_warnings.is_empty(),
                "{name}: expected 0 layout warnings, got: {:#?}",
                out.layout_warnings
            );
        }

        let tmp = tempfile::tempdir().unwrap();
        let sch = tmp.path().join(format!("{name}.kicad_sch"));
        std::fs::write(&sch, &out.sch).unwrap();

        // No REAL ERC errors. `lib_symbol_issues` is a standalone-context artifact
        // (the symbol library table isn't registered when ERCing a lone file);
        // the embedded `lib_symbols` make connectivity sound regardless, so it is
        // filtered. Anything else at error severity is a true defect.
        //
        // `pin_to_pin` is filtered for the CHALLENGE tier only: it fires "Power
        // output and Power output are connected" when the engine adds a redundant
        // PWR_FLAG to a rail that a regulator VO already drives (it fails to see VO
        // is a power output for EXTENDS-derived regulator symbols like AMS1117, and
        // for a part's tied multi-VCAP power-output pins). This is an ERC-HYGIENE
        // gap, NOT a connectivity defect — the netlist stays truthful (verified
        // below: no rail merges, every pin connected). The reference tier still
        // forbids it. KNOWN-ENGINE-GAP: suppress the flag when a power-output pin
        // (resolved through the extends chain) already drives the rail.
        let erc = KicadCli::new(env).erc(&sch).unwrap();
        let tolerated = |kind: &str| {
            kind == "lib_symbol_issues"
                || (!strict_warnings && TOLERATED_ERC_KINDS.contains(&kind))
        };
        let real_errors: Vec<_> = erc
            .violations
            .iter()
            .filter(|v| v.severity == "error" && !tolerated(&v.kind))
            .collect();
        assert!(real_errors.is_empty(), "{name}: real ERC errors: {real_errors:#?}");

        // No off-grid wire/pin endpoints: the reframe shift must stay a grid
        // multiple so connectivity geometry remains on the 1.27 mm grid (a
        // non-grid shift connects fine but ERCs every endpoint as off-grid).
        let off_grid = erc.violations.iter().filter(|v| v.kind == "endpoint_off_grid").count();
        assert_eq!(off_grid, 0, "{name}: {off_grid} off-grid endpoints (reframe shift not snapped?)");

        // Truthfulness: every authored pin lands on exactly one netlist net;
        // authored nets neither split nor merge.
        let nl: Netlist = KicadCli::new(env).netlist(&sch).unwrap();
        let mut authored_to_nl: HashMap<&str, Option<usize>> = HashMap::new();
        for block in design.blocks.values() {
            for (refdes, comp) in &block.components {
                let lib_id = &comp.part;
                // power:* symbols are declarations of net-power (consumed by
                // mark_power_nets); the engine synthesizes the actual rail/per-pin
                // power graphics, so these refdes never appear in the netlist.
                if comp.part.starts_with("power:") {
                    continue;
                }
                for (pin, target) in &comp.pins {
                    let circuit_lang::model::PinTarget::Net(want) = target else { continue };
                    let got = nl.nets.iter().position(|n| {
                        n.nodes
                            .iter()
                            .any(|(r, p)| r == refdes && nl_pin_matches(provider, lib_id, pin, p))
                    });
                    assert!(
                        got.is_some(),
                        "{name}/{refdes}.{pin}: authored to {want} but no netlist net carries it \
                         (a disconnected tap — the wire-split pass regressed?)"
                    );
                    match authored_to_nl.entry(want.as_str()) {
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(got);
                        }
                        std::collections::hash_map::Entry::Occupied(e) => assert_eq!(
                            *e.get(),
                            got,
                            "{name}: authored net {want} SPLIT across netlist nets — {refdes}.{pin} \
                             landed elsewhere"
                        ),
                    }
                }
            }
        }
        // No merges: distinct authored nets map to distinct netlist nets.
        let mut nl_to_authored: HashMap<usize, &str> = HashMap::new();
        for (authored, nl_idx) in &authored_to_nl {
            let Some(idx) = nl_idx else { continue };
            if let Some(prev) = nl_to_authored.insert(*idx, authored) {
                panic!(
                    "{name}: authored nets {prev:?} and {authored:?} are SHORTED onto one netlist \
                     net {:?}",
                    nl.nets[*idx].name
                );
            }
        }
    }
}
