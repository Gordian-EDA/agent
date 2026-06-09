# Plan 2: `kicad-bridge` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax. TDD throughout: failing test → fail → implement → pass → `cargo fmt --all` → commit. Gate per task: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`.

**Goal:** Build `kicad-bridge` — the I/O crate that connects circuit-lang to the real KiCAD 10 installation: real symbol provider, symbol search, `kicad-cli` ERC/netlist wrappers, and snapshot store.

**Architecture:** `crates/kicad-bridge/` depends on `circuit-lang` (implements its `SymbolProvider` trait) and wraps external reality: `/usr/share/kicad/symbols` (223 libs), `kicad-cli` 10.0.3, the filesystem. Spec: `docs/superpowers/specs/2026-06-09-kicad-copilot-agent-design.md` §9, §12. Ground truth from validation (commit 08e617b, memory `real-validation-findings`): symbols use `extends` inheritance (AMS1117-3.3 → AP1117-15); sub-symbol blocks `<NAME>_<unit>_<style>` carry the pins; KiCAD 10 renamed parts (USB_C_Receptacle_USB2.0 → `_16P`); STM32H743VITx has 100 pins, VCAP stacked at 48/73 power-out. A proven fallback scanner exists in `crates/autopcb/examples/llm_spike.rs`.

**Tech stack:** Rust 2024. New deps (workspace): `kiutils_kicad = "0.3"` (typed .kicad_sym parsing — **validate in Task 2; fall back to promoting the llm_spike scanner if insufficient**), `strsim` (search), `serde`/`serde_json` (kicad-cli report parsing), `tempfile` (dev-dep, tests). File watcher and IPC client are **deferred to Plan 4** (TUI-time concerns).

**Testing policy:** This crate is I/O-bound by nature. Tests that touch `/usr/share/kicad/symbols` or `kicad-cli` are integration tests in `crates/kicad-bridge/tests/`, **skipped gracefully** (not failed) when KiCAD is absent: every such test starts with `let Some(env) = KicadEnv::detect() else { eprintln!("SKIP: kicad not found"); return };`. On this machine KiCAD 10.0.3 IS installed, so they run for real.

---

### Task 1: Crate scaffold + KiCAD environment discovery

**Files:**
- Create: `crates/kicad-bridge/Cargo.toml`, `crates/kicad-bridge/src/lib.rs`, `crates/kicad-bridge/src/env.rs`
- Test: `crates/kicad-bridge/tests/env.rs`

- [ ] **Step 1 — failing test** (`tests/env.rs`):

```rust
use kicad_bridge::env::KicadEnv;

#[test]
fn detects_installed_kicad() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: kicad not found");
        return;
    };
    assert!(env.symbol_dir.join("Device.kicad_sym").exists());
    assert!(env.cli_version.starts_with("10."), "got {}", env.cli_version);
}

#[test]
fn env_override_wins() {
    // AUTO_PCB_SYMBOL_DIR overrides discovery (used by tests/other distros)
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Fake.kicad_sym"), "(kicad_symbol_lib)").unwrap();
    let env = KicadEnv::with_symbol_dir(tmp.path().to_path_buf());
    assert_eq!(env.symbol_dir, tmp.path());
}
```

- [ ] **Step 2 — run, confirm fail** (crate doesn't exist).

- [ ] **Step 3 — implement.**
  - `Cargo.toml`: deps `circuit-lang` (path), `strsim`, `serde`, `serde_json` (workspace); dev-dep `tempfile`. Add `kiutils_kicad` only in Task 2 if it passes validation. Add new workspace deps to the root `Cargo.toml`.
  - `env.rs`: `pub struct KicadEnv { pub symbol_dir: PathBuf, pub cli_path: PathBuf, pub cli_version: String }`.
    - `detect()`: symbol dir = `$AUTO_PCB_SYMBOL_DIR` if set, else first existing of `/usr/share/kicad/symbols`, `/usr/local/share/kicad/symbols`, `/Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols`. cli = `which kicad-cli` (run `kicad-cli version` for the version string). Return `None` if either missing.
    - `with_symbol_dir(PathBuf)`: constructor for tests (cli fields empty/`"0"`).
- [ ] **Step 4 — run, confirm pass** (real machine: both tests run; the first against real KiCAD).
- [ ] **Step 5 — fmt + commit:** `feat(kicad-bridge): crate scaffold and KiCAD environment discovery`

---

### Task 2: Symbol-library parsing — kiutils validation gate

**Files:**
- Create: `crates/kicad-bridge/src/symlib.rs`
- Test: `crates/kicad-bridge/tests/symlib.rs`

This task DECIDES the parsing backend. Acceptance is fixed by ground truth; the implementation may be `kiutils_kicad` OR a promoted/hardened version of the `llm_spike` scanner — whichever meets the tests with less risk. **Document the choice in the commit message.**

- [ ] **Step 1 — failing tests** (`tests/symlib.rs`) — these encode the validated ground truth:

```rust
use kicad_bridge::env::KicadEnv;
use kicad_bridge::symlib::SymbolLib;
use circuit_lang::PinType;

fn lib(name: &str) -> Option<SymbolLib> {
    let env = KicadEnv::detect()?;
    SymbolLib::load(&env.symbol_dir.join(format!("{name}.kicad_sym"))).ok()
}

#[test]
fn stm32h743vitx_has_100_pins_with_correct_types() {
    let Some(l) = lib("MCU_ST_STM32H7") else { eprintln!("SKIP"); return };
    let s = l.symbol("STM32H743VITx").unwrap();
    assert_eq!(s.pins.len(), 100);
    // VCAP: stacked name, power-out, at numbers 48 and 73
    let vcaps: Vec<_> = s.pins.iter().filter(|p| p.name == "VCAP").collect();
    assert_eq!(vcaps.len(), 2);
    assert!(vcaps.iter().all(|p| p.etype == PinType::PowerOutput));
    let mut nums: Vec<_> = vcaps.iter().map(|p| p.number.as_str()).collect();
    nums.sort();
    assert_eq!(nums, ["48", "73"]);
    // VDD stacked power-in
    assert!(s.pins.iter().filter(|p| p.name == "VDD")
        .all(|p| p.etype == PinType::PowerInput));
}

#[test]
fn extends_chain_resolves() {
    let Some(l) = lib("Regulator_Linear") else { eprintln!("SKIP"); return };
    let s = l.symbol("AMS1117-3.3").unwrap(); // extends AP1117-15
    assert_eq!(s.pins.len(), 3);
    let names: std::collections::BTreeSet<_> =
        s.pins.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["GND", "VI", "VO"].into_iter().collect());
}

#[test]
fn multi_unit_symbol_reports_units() {
    let Some(l) = lib("Amplifier_Operational") else { eprintln!("SKIP"); return };
    let s = l.symbol("LM358").unwrap();
    let max_unit = s.pins.iter().map(|p| p.unit).max().unwrap();
    assert!(max_unit >= 2, "LM358 has at least 2 symbol units, got {max_unit}");
}

#[test]
fn sub_symbol_blocks_are_not_listed_as_symbols() {
    let Some(l) = lib("Device") else { eprintln!("SKIP"); return };
    assert!(l.symbol("R").is_some());
    assert!(l.symbol("R_0_1").is_none(), "unit blocks must be merged, not exposed");
}
```

- [ ] **Step 2 — run, confirm fail.**
- [ ] **Step 3 — implement.** First try `kiutils_kicad` (add dep, parse, map to `circuit_lang::PinMeta`: name/number/etype/unit; merge `_<unit>_<style>` sub-blocks; resolve `extends` within the lib, ≤4 hops). **If kiutils_kicad cannot express any acceptance test** (missing pin types, no extends info, etc.), drop the dep and instead port the `llm_spike` scanner into `symlib.rs`, extending it with: unit extraction from sub-block names (`_1_1` → unit 1), proper string-skip, and `extends` (already proven). API: `SymbolLib::load(path) -> io::Result<SymbolLib>`, `fn symbol(&self, name: &str) -> Option<&SymbolMeta>` (circuit-lang's `SymbolMeta`), `fn names(&self) -> impl Iterator<Item = &str>`.
- [ ] **Step 4 — run, confirm pass against real libs.**
- [ ] **Step 5 — fmt + commit:** `feat(kicad-bridge): symbol library parsing (backend: <kiutils|native>, reason in body)`

---

### Task 3: `RealSymbolProvider` (implements circuit-lang's trait) + cache

**Files:**
- Create: `crates/kicad-bridge/src/provider.rs`
- Test: `crates/kicad-bridge/tests/provider.rs`

- [ ] **Step 1 — failing tests:**

```rust
use kicad_bridge::{env::KicadEnv, provider::RealSymbolProvider};
use circuit_lang::SymbolProvider;

#[test]
fn compiles_validated_bedrock_design_against_real_libs() {
    let Some(env) = KicadEnv::detect() else { eprintln!("SKIP"); return };
    let provider = RealSymbolProvider::new(env);
    let src = include_str!("../../../docs/validation/bedrock-oneshot-bluepill.circuit.yaml");
    let result = circuit_lang::compile(src, &provider);
    assert!(!result.diagnostics.has_errors(), "{:?}", result.diagnostics);
    assert!(result.design.is_some());
}

#[test]
fn unknown_part_gets_real_suggestions() {
    let Some(env) = KicadEnv::detect() else { eprintln!("SKIP"); return };
    let provider = RealSymbolProvider::new(env);
    // stale KiCAD-8-era name an LLM will emit (validated failure mode)
    assert!(provider.symbol("Connector:USB_C_Receptacle_USB2.0").is_none());
    let sugg = provider.suggest("Connector:USB_C_Receptacle_USB2.0");
    assert!(sugg.iter().any(|s| s.contains("USB_C_Receptacle_USB2.0_16P")
        || s.contains("USB_C_Receptacle_USB2.0_14P")), "{sugg:?}");
}
```

- [ ] **Step 2 — run, confirm fail.**
- [ ] **Step 3 — implement.** `RealSymbolProvider { env, libs: RefCell<HashMap<String, SymbolLib>>, metas: ... }` — lazy per-lib load via Task 2, memoized `SymbolMeta` per lib_id. The trait returns `Option<&SymbolMeta>`: store metas in a stable arena (`elsa::FrozenMap` or pre-loaded `HashMap` behind `OnceCell` per lib — pick the simplest safe approach, NOT `Box::leak`). `suggest`: levenshtein over the referenced lib's symbol names (cap 3, distance ≤6), lib part fixed.
- [ ] **Step 4 — run, confirm pass.** The first test is the milestone: **real LLM output × real libraries × production provider.**
- [ ] **Step 5 — fmt + commit:** `feat(kicad-bridge): RealSymbolProvider over installed libraries`

---

### Task 4: Cross-library symbol search (backs the `search_symbols` agent tool)

**Files:**
- Create: `crates/kicad-bridge/src/search.rs`
- Test: `crates/kicad-bridge/tests/search.rs`

- [ ] **Step 1 — failing tests:**

```rust
use kicad_bridge::{env::KicadEnv, search::SymbolIndex};

#[test]
fn finds_stm32h743_by_substring() {
    let Some(env) = KicadEnv::detect() else { eprintln!("SKIP"); return };
    let idx = SymbolIndex::build(&env).unwrap();
    let hits = idx.search("STM32H743VI", 5);
    assert!(hits.iter().any(|h| h.lib_id == "MCU_ST_STM32H7:STM32H743VITx"), "{hits:?}");
    let top = &hits[0];
    assert!(top.pin_count > 0);
}

#[test]
fn fuzzy_finds_usb_c_receptacle() {
    let Some(env) = KicadEnv::detect() else { eprintln!("SKIP"); return };
    let idx = SymbolIndex::build(&env).unwrap();
    let hits = idx.search("usb-c receptacle usb2", 8);
    assert!(hits.iter().any(|h| h.lib_id.contains("USB_C_Receptacle_USB2.0_16P")), "{hits:?}");
}
```

- [ ] **Step 2 — run, confirm fail.**
- [ ] **Step 3 — implement.** `SymbolIndex::build(&KicadEnv)`: scan all `*.kicad_sym` file names + symbol names (names only — do NOT fully parse 223 libs; read each file once, extract top-level symbol names; reuse Task 2's block-name scan). `search(query, n) -> Vec<Hit{lib_id, pin_count}>`: rank = case-insensitive substring match first, then normalized levenshtein on the symbol name with non-alnum chars treated as word separators. `pin_count` is resolved lazily for the returned hits only (parse those symbols via Task 2). Build time target: <2s for 223 libs (names only) — add `#[test] #[ignore]` perf probe if needed.
- [ ] **Step 4 — run, confirm pass.**
- [ ] **Step 5 — fmt + commit:** `feat(kicad-bridge): fuzzy cross-library symbol search`

---

### Task 5: `kicad-cli` wrapper — ERC

**Files:**
- Create: `crates/kicad-bridge/src/cli.rs`, `crates/kicad-bridge/tests/fixtures/blank.kicad_sch`
- Test: `crates/kicad-bridge/tests/cli_erc.rs`

- [ ] **Step 1 — fixture.** Create a minimal valid empty schematic (KiCAD 10 format) at `tests/fixtures/blank.kicad_sch`:

```
(kicad_sch
	(version 20250114)
	(generator "auto-pcb")
	(generator_version "0.1")
	(uuid "b2a3e2c0-0000-4000-8000-000000000001")
	(paper "A4")
	(lib_symbols)
	(sheet_instances
		(path "/"
			(page "1")
		)
	)
)
```

(If KiCAD 10.0.3 rejects this version stamp, open the file with `kicad-cli sch erc` manually once and adjust `version` to the current format date — the implementer fixes the fixture until ERC accepts the file.)

- [ ] **Step 2 — failing test** (`tests/cli_erc.rs`):

```rust
use kicad_bridge::{cli::KicadCli, env::KicadEnv};

#[test]
fn erc_runs_on_blank_schematic_and_parses_report() {
    let Some(env) = KicadEnv::detect() else { eprintln!("SKIP"); return };
    let cli = KicadCli::new(&env);
    let report = cli.erc(std::path::Path::new("tests/fixtures/blank.kicad_sch")).unwrap();
    // blank sheet: no violations expected (or only sheet-level warnings)
    assert_eq!(report.error_count(), 0, "{report:?}");
}
```

- [ ] **Step 3 — implement.** `KicadCli::erc(path)`: run `kicad-cli sch erc --format json --output <tmp.json> --exit-code-violations <path>` (verify exact flags via `kicad-cli sch erc --help` at implementation time; adapt). Parse JSON into `ErcReport { violations: Vec<Violation{severity, type, description, items}> }` with `error_count()`/`warning_count()`. Surface stderr in the error path. Use `tempfile` for the output.
- [ ] **Step 4 — run, confirm pass (real kicad-cli).**
- [ ] **Step 5 — fmt + commit:** `feat(kicad-bridge): kicad-cli ERC wrapper with JSON report parsing`

---

### Task 6: `kicad-cli` wrapper — netlist export (the lift oracle)

**Files:**
- Modify: `crates/kicad-bridge/src/cli.rs`
- Test: `crates/kicad-bridge/tests/cli_netlist.rs`

- [ ] **Step 1 — failing test:**

```rust
use kicad_bridge::{cli::KicadCli, env::KicadEnv};

#[test]
fn netlist_export_runs_and_parses() {
    let Some(env) = KicadEnv::detect() else { eprintln!("SKIP"); return };
    let cli = KicadCli::new(&env);
    let nl = cli.netlist(std::path::Path::new("tests/fixtures/blank.kicad_sch")).unwrap();
    assert!(nl.components.is_empty() && nl.nets.is_empty()); // blank sheet
}
```

- [ ] **Step 2 — implement.** `kicad-cli sch export netlist --format kicadxml --output <tmp> <path>` (verify flag names at impl time; `kicadxml` gives structured XML). Parse with a minimal XML reader — add `quick-xml` (workspace dep) — into `Netlist { components: Vec<NetComp{ref, value, lib_id, properties}>, nets: Vec<Net{name, nodes: Vec<(ref, pin)>}> }`. This struct is the input to Plan 3's lift.
- [ ] **Step 3 — run, confirm pass.**
- [ ] **Step 4 — fmt + commit:** `feat(kicad-bridge): kicad-cli netlist export wrapper (lift oracle)`

---

### Task 7: Snapshot store

**Files:**
- Create: `crates/kicad-bridge/src/snapshot.rs`
- Test: unit tests in-module (pure-ish: uses tempdir, no KiCAD needed)

- [ ] **Step 1 — failing tests (in-module):**

```rust
#[test]
fn snapshot_and_undo_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let sch = tmp.path().join("x.kicad_sch");
    std::fs::write(&sch, "v1").unwrap();
    let store = SnapshotStore::for_project(tmp.path()).unwrap();
    store.snapshot(&sch).unwrap();           // history/0001-*.kicad_sch
    std::fs::write(&sch, "v2").unwrap();
    store.snapshot(&sch).unwrap();
    std::fs::write(&sch, "v3").unwrap();
    store.undo(&sch).unwrap();               // restores v2 (latest snapshot)
    assert_eq!(std::fs::read_to_string(&sch).unwrap(), "v2");
    assert_eq!(store.list().unwrap().len(), 2);
}
```

- [ ] **Step 2 — implement.** `.auto-pcb/history/<NNNN>-<filename>` under the project dir; monotonically numbered (scan existing); `snapshot` copies current file in; `undo` copies the latest snapshot back **and removes it** (pop semantics); `list()` sorted. No timestamps in filenames (determinism; numbering suffices).
- [ ] **Step 3 — run, confirm pass; fmt + commit:** `feat(kicad-bridge): snapshot store with pop-undo`

---

### Task 8: Re-point `llm_spike` at the production provider

**Files:**
- Modify: `crates/autopcb/examples/llm_spike.rs`, `crates/autopcb/Cargo.toml` (add `kicad-bridge` dep)

- [ ] **Step 1 — replace** the example's hand-rolled `RealLibProvider`/`LibScanner` with `kicad_bridge::provider::RealSymbolProvider` + `kicad_bridge::search::SymbolIndex` (add a `--search <query>` mode). The example shrinks to arg-parsing + printing.
- [ ] **Step 2 — verify:** `cargo run -p autopcb --example llm_spike -- docs/validation/bedrock-oneshot-bluepill.circuit.yaml` → `COMPILE OK`; `--dump MCU_ST_STM32H7:STM32H743VITx` → 100 pins; `--search "usb-c receptacle"` lists `_16P`. Full gate green.
- [ ] **Step 3 — fmt + commit:** `refactor(autopcb): llm_spike uses kicad-bridge provider/search`

---

## Definition of done

- All gates green; integration tests RUN (not skipped) on this machine and pass against real KiCAD 10.0.3.
- `circuit_lang::compile` works against the production provider on the validated Bedrock fixtures.
- ERC + netlist wrappers proven against a real schematic file.
- Deferred (recorded): file watcher + IPC presence detection → Plan 4; `.kicad_sch` writing → Plan 3 (sch-engine).
