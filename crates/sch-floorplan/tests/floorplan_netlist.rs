//! Connectivity regression oracle for the FLOORPLAN engine (the active layout
//! path). For each reference fixture, lower its connectivity-only input, emit,
//! export the netlist via kicad, and
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

use kicad::KicadInstallation;
use kicad::Netlist;
use kicad_symbol::SymbolTable;
use sch_floorplan::floorplan;

/// TIER 1 — the hand-tuned reference targets. Held to the FULL bar: electrically
/// truthful AND zero layout warnings AND ERC-clean. These match the human
/// references, so any regression must show up here.
const REFERENCE_FIXTURES: &[&str] = &[
    "divider-filter",
    "mcp1703-power-entry",
    "555-blinker",
    "uart-level-translator",
];

/// TIER 2 — deliberately HARD circuits (large MCUs, BGAs, RF, mixed-signal) that
/// stress the engine far past the tuned cases. The aesthetic bar is NOT expected
/// to be met (a 100-pin part will lay out rough), so the layout-warning check is
/// relaxed — but the CORRECTNESS invariants are non-negotiable: every authored
/// pin lands connected, no authored net splits or merges, geometry stays on-grid,
/// and no real ERC errors. This is the coverage that catches a connectivity or
/// finalize bug the four clean fixtures are too small to surface.
const CHALLENGE_FIXTURES: &[&str] = &[
    "bedrock-oneshot-bluepill",    // STM32H743 100-pin LQFP dev board
    "bedrock-selfrepair-bluepill", // STM32 + HSE/32k crystals
    "rf-lna-frontend",             // ADL5542 RF gain block + coax (HF/RF)
    "mixed-signal-adc-frontend",   // MCP6002 op-amp -> ADS1115 I2C ADC (mixed-signal)
    "bga-fpga-ice40",              // ICE40HX8K-BG121 121-ball BGA, dual-rail (BGA)
    "grid-demo",                   // authored `layout:` 2D grid (NE555 blinker)
    "idiom-stm32",                 // distributed local grounds (≥2 GND symbols) + idioms
];

fn doc(name: &str, ext: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/validation/{name}.{ext}"))
}

fn validation_corpus_available() -> bool {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/validation")
        .is_dir()
}

/// The YAML keys pins by NAME; the KiCAD netlist reports pins by NUMBER. Resolve
/// the authored token (number-first then name, `find_pin` order) and compare.
fn nl_pin_matches(provider: &SymbolTable, lib_id: &str, authored: &str, nl_pin: &str) -> bool {
    if authored == nl_pin {
        return true;
    }
    let Some(sym) = provider.symbol(lib_id) else {
        return false;
    };
    sch_check::find_pin(&sym.pins, authored).map(|p| p.number.as_str()) == Some(nl_pin)
}

#[test]
fn floorplan_reference_fixtures_emit_truthful_netlists() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !validation_corpus_available() {
        eprintln!("SKIP: docs/validation corpus not present");
        return;
    }
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    for name in REFERENCE_FIXTURES {
        validate_fixture(&env, &provider, name, /* strict_warnings */ true);
    }
}

#[test]
fn floorplan_challenge_fixtures_emit_truthful_netlists() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    run_challenge_fixtures();
}

/// Run the challenge tier over every (or `FLOORPLAN_ONLY`-restricted) fixture.
fn run_challenge_fixtures() {
    if !validation_corpus_available() {
        eprintln!("SKIP: docs/validation corpus not present");
        return;
    }
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
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
/// coverage; each must have a documented root cause. Empty is the goal.
///
/// The bugs the expanded coverage surfaced, and how each was closed:
///
///   - `rf-lna-frontend` — a rail riser SHORTED `RF_IN` onto `GND`: C3's ground riser
///     ran straight down the column it shares with J1 and terminated on J1's `In` pin,
///     which `split_wires_at_nodes` then welded. `plan_riser_offsets` only fans
///     riser-vs-riser and `riser_hits_body` only sees 2-pin bodies, so neither looked for
///     a riser landing on a foreign PIN. FIXED: `place::route::riser_hits_foreign_pin`
///     joins the body and lane checks in one finalize-jog predicate, giving rails the
///     foreign-pin guard the signal router already had through its routing scene.
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

/// Load `<name>.place-parts.json`, emit through the floorplan engine, and assert the sheet is electrically
/// TRUTHFUL + on-grid + ERC-clean. With `strict_warnings`, also assert zero
/// layout warnings (tier-1 readability bar).
fn validate_fixture(
    env: &KicadInstallation,
    provider: &SymbolTable,
    name: &str,
    strict_warnings: bool,
) {
    {
        let src = std::fs::read_to_string(doc(name, "place-parts.json")).unwrap();
        let input: sch_check::PlacePartsInput = serde_json::from_str(&src).unwrap();
        let (design, diagnostics) = sch_check::into_design(&input, provider);
        assert!(!diagnostics.has_errors(), "{name}: {:#?}", diagnostics);
        let ir = input
            .intent
            .map(sch_check::Intent::into_layout_ir)
            .unwrap_or_else(|| floorplan::baseline_ir(&design));
        // `SCH_ENGINE=spine` runs the same oracle over the spine engine; the
        // default stays anneal so existing runs are untouched.
        let engine: Box<dyn sch_floorplan::contract::PlacementEngine> =
            match std::env::var("SCH_ENGINE").as_deref() {
                Ok("spine") => Box::new(spine_place::SpinePlace),
                _ => Box::new(anneal_place::Anneal),
            };
        let out = floorplan::emit_strategy(env, &design, engine, Some(ir))
            .unwrap_or_else(|e| panic!("{name}: {e}"));

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
        let erc = env.erc(&sch).unwrap();
        let tolerated = |kind: &str| {
            kind == "lib_symbol_issues" || (!strict_warnings && TOLERATED_ERC_KINDS.contains(&kind))
        };
        let real_errors: Vec<_> = erc
            .violations
            .iter()
            .filter(|v| v.severity == "error" && !tolerated(&v.kind))
            .collect();
        assert!(
            real_errors.is_empty(),
            "{name}: real ERC errors: {real_errors:#?}"
        );

        // No off-grid wire/pin endpoints: the reframe shift must stay a grid
        // multiple so connectivity geometry remains on the 1.27 mm grid (a
        // non-grid shift connects fine but ERCs every endpoint as off-grid).
        let off_grid = erc
            .violations
            .iter()
            .filter(|v| v.kind == "endpoint_off_grid")
            .count();
        assert_eq!(
            off_grid, 0,
            "{name}: {off_grid} off-grid endpoints (reframe shift not snapped?)"
        );

        // Truthfulness: every authored pin lands on exactly one netlist net;
        // authored nets neither split nor merge.
        let nl: Netlist = env.netlist(&sch).unwrap();
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
                    let sch_check::model::PinTarget::Net(want) = target else {
                        continue;
                    };
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
