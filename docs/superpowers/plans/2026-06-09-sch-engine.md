# Plan 3: `sch-engine` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax. TDD throughout: failing test → fail → implement → pass → `cargo fmt --all` → commit. Per-task gate: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`.

**Goal:** Build `sch-engine` — the crate that turns a `circuit_lang::Design` (kernel model) into a real `.kicad_sch` file KiCAD 10 opens and ERCs clean, preserves user positions on re-emit (reconciliation), and lifts an existing `.kicad_sch` back to kernel YAML. This delivers **MVP acceptance criterion #1**: the founding bluepill prompt → ERC-clean schematic.

**Architecture (de-risked by the emission spike, commit 5666618):**
- **Emission is string/template assembly, NOT kiutils typed-write** — kiutils' schematic model is read-only over its CST (`ast_mut` makes `write()` error). We assemble `.kicad_sch` S-expression text, then **gate every emission by parsing it back through `kiutils_kicad::SchematicFile::read` (well-formedness) + `KicadCli::erc` (electrical validity).** The test for "did we emit correctly" IS "does KiCAD load + ERC it."
- **Connectivity = local labels at pin endpoints.** A local label carrying the net name, placed exactly on a pin's connection point, joins that pin to the net; same net name on multiple pins = electrically connected, with **no wire routing**. (Power nets optionally get power symbols.) This sidesteps 2D geometry entirely — the spike proved a label/wire on the exact pin endpoint + 1.27 mm grid gives 0 ERC errors.
- **Determinism is mandatory** (spec §5.1 "same state ⇒ byte-identical"). All UUIDs are **content-derived (UUIDv5)** from a fixed namespace + a stable key (refdes, net name, sheet) — never random — so re-emitting the same `Design` yields byte-identical output and snapshot tests are stable.
- **Reconciliation** keys on refdes (synthesized parts on `ap_parent/ap_role/ap_index`): on re-emit, surviving symbols keep their existing `(at …)` position read from the prior `.kicad_sch`; only new parts are auto-placed.
- **Lift** reads connectivity from `KicadCli::netlist` (authoritative) and semantics from `ap_*` properties, producing canonical kernel YAML.

Spec: `docs/superpowers/specs/2026-06-09-kicad-copilot-agent-design.md` §4 (React model), §6 (gauntlet), §7 (reconciliation), §8 (placement). Crates: `circuit-lang` (kernel + canonical YAML), `kicad-bridge` (symbols, geometry, kicad-cli, snapshots). The emission spike lives at `crates/kicad-bridge/examples/emit_spike.rs` — read it for the proven coordinate math and working `.kicad_sch` structure.

**Tech stack:** Rust 2024. New deps: `uuid` (v5, deterministic) — add to workspace. Reuse `kiutils_kicad`, `kiutils_sexpr` (for lib_symbols extraction — its `parse_one`/`CstDocument`/`Node`/`Atom` are public), `indexmap`.

**Testing policy:** Emission/ERC tests are integration tests gated on KiCAD (`KicadEnv::detect()` → SKIP-graceful); they RUN on this machine. Placement and reconciliation logic get pure unit tests + `insta`-style byte snapshots where deterministic.

---

### Task 1: Pin & symbol geometry in kicad-bridge

**Files:** Modify `crates/kicad-bridge/src/symlib.rs`; new `crates/kicad-bridge/src/geometry.rs`; test `crates/kicad-bridge/tests/geometry.rs`.

sch-engine needs, per used symbol: each pin's local position/angle/length (to compute connection endpoints) and the symbol's **full definition S-expression** (to embed in `(lib_symbols)`). `circuit_lang::PinMeta` stays geometry-free (pure crate); geometry lives in kicad-bridge.

- [ ] **Step 1 — failing tests** (`tests/geometry.rs`):

```rust
use kicad_bridge::env::KicadEnv;
use kicad_bridge::geometry::SymbolGeometry;

#[test]
fn device_r_pin_geometry() {
    let Some(env) = KicadEnv::detect() else { eprintln!("SKIP"); return };
    let g = SymbolGeometry::load(&env, "Device:R").unwrap();
    assert_eq!(g.pins.len(), 2);
    // Device:R pins are vertical at x=0, y=±3.81, length 1.27 (per spike)
    let ys: Vec<f64> = g.pins.iter().map(|p| p.at[1]).collect();
    assert!(ys.contains(&3.81) && ys.contains(&-3.81), "{ys:?}");
    assert!(g.pins.iter().all(|p| (p.length - 1.27).abs() < 1e-9));
}

#[test]
fn lib_symbols_definition_is_embeddable() {
    let Some(env) = KicadEnv::detect() else { eprintln!("SKIP"); return };
    let g = SymbolGeometry::load(&env, "Device:R").unwrap();
    // the raw (symbol "Device:R" ...) block, ready to splice into (lib_symbols)
    let def = g.definition_sexpr();
    assert!(def.trim_start().starts_with("(symbol"));
    assert!(def.contains("\"Device:R\"") || def.contains("Device:R"));
    // balanced parens
    let opens = def.matches('(').count();
    let closes = def.matches(')').count();
    assert_eq!(opens, closes, "unbalanced lib_symbols block");
}
```

- [ ] **Step 2 — run, confirm fail.**
- [ ] **Step 3 — implement.** `geometry.rs`: `SymbolGeometry { lib_id, pins: Vec<PinGeom{ number, name, at: [f64;2], angle: f64, length: f64 }>, raw_definition: String }` with `load(&KicadEnv, lib_id) -> io::Result<SymbolGeometry>` and `definition_sexpr(&self) -> &str`. Source pin `at`/`angle`/`length` from kiutils `SymPin` (symlib.rs already reads these — surface them). For `raw_definition`, extract the balanced `(symbol "Lib:Name" …)` block — prefer `kiutils_sexpr::parse_one` + CST walk to find and serialize the node (robust); the spike's string-extraction is an acceptable fallback. The lib_id inside the embedded block must be the symbol's library-local name as KiCAD expects in `lib_symbols` (investigate against a real schematic; the spike shows the form). Resolve `extends` so a derived symbol embeds the parent's body under the derived name if needed (verify what KiCAD requires).
- [ ] **Step 4 — run, confirm pass against real libs.**
- [ ] **Step 5 — fmt + commit:** `feat(kicad-bridge): pin geometry and lib_symbols definition extraction`

---

### Task 2: `sch-engine` scaffold + deterministic primitives

**Files:** Create `crates/sch-engine/Cargo.toml`, `src/lib.rs`, `src/ids.rs`, `src/grid.rs`. Tests in-module.

- [ ] **Step 1 — failing tests** (in `ids.rs` and `grid.rs`):

```rust
// ids.rs
#[test]
fn uuid_is_deterministic_and_keyed() {
    let a = stable_uuid("symbol", "U1");
    let b = stable_uuid("symbol", "U1");
    let c = stable_uuid("symbol", "U2");
    assert_eq!(a, b);                 // same key -> same uuid (byte-identical re-emit)
    assert_ne!(a, c);                 // different key -> different uuid
    assert_eq!(a.len(), 36);          // canonical hyphenated form
}

// grid.rs
#[test]
fn snaps_to_1_27mm_grid() {
    assert_eq!(snap(46.19), 46.99.min(46.99)); // placeholder — see impl note
    assert_eq!(snap(0.0), 0.0);
    assert_eq!(snap(1.9), 2.54);   // nearest multiple of 1.27
    assert_eq!(snap(1.2), 1.27);
}
```

(Impl note: pick the exact rounding in implementation; the real assertions are: multiples of 1.27 are fixed points, and arbitrary values round to the nearest multiple. Replace the placeholder line with concrete nearest-multiple expectations.)

- [ ] **Step 2 — run, confirm fail.**
- [ ] **Step 3 — implement.** `Cargo.toml`: deps `circuit-lang`, `kicad-bridge` (path), `uuid` (workspace, features `v5`), `indexmap`. `ids.rs`: `stable_uuid(kind: &str, key: &str) -> String` = UUIDv5 over a fixed project namespace UUID (hardcode one) with name `"{kind}:{key}"`, hyphenated lowercase. `grid.rs`: `const GRID_MM: f64 = 1.27; fn snap(v: f64) -> f64` nearest multiple (round half away from zero), plus `snap_point([f64;2])`.
- [ ] **Step 4 — run, confirm pass.**
- [ ] **Step 5 — fmt + commit:** `feat(sch-engine): crate scaffold, deterministic UUIDs and grid snapping`

---

### Task 3: Schematic document writer — header + lib_symbols + one symbol instance

**Files:** Create `crates/sch-engine/src/emit.rs`; test `crates/sch-engine/tests/emit_minimal.rs`.

The first emission milestone: produce a `.kicad_sch` with one placed symbol that KiCAD loads.

- [ ] **Step 1 — failing test:**

```rust
use kicad_bridge::env::KicadEnv;
use kicad_bridge::cli::KicadCli;
use sch_engine::emit::SchematicWriter;

#[test]
fn emits_single_symbol_that_kicad_loads() {
    let Some(env) = KicadEnv::detect() else { eprintln!("SKIP"); return };
    let mut w = SchematicWriter::new();
    w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0).unwrap();
    let text = w.finish();

    // 1) well-formedness: kiutils parses it
    let tmp = tempfile::Builder::new().suffix(".kicad_sch").tempfile().unwrap();
    std::fs::write(tmp.path(), &text).unwrap();
    kiutils_kicad::SchematicFile::read(tmp.path()).expect("kiutils must parse our output");

    // 2) KiCAD loads + ERC runs (single unconnected R: errors are about connectivity, but it must LOAD)
    let report = KicadCli::new(&env).erc(tmp.path()).unwrap();
    // loading succeeded if erc() returned Ok; assert the symbol is present via netlist
    let nl = KicadCli::new(&env).netlist(tmp.path()).unwrap();
    assert_eq!(nl.components.len(), 1);
    assert_eq!(nl.components[0].reference, "R1");
    let _ = report;
}
```

- [ ] **Step 2 — run, confirm fail.**
- [ ] **Step 3 — implement.** `SchematicWriter`: accumulates header (`(kicad_sch (version 20250114) (generator "auto-pcb") (generator_version "0.1") (uuid <stable>) (paper "A4") …)`), a `lib_symbols` set (dedup by lib_id, body from `SymbolGeometry::definition_sexpr`), symbol instances, and (Task 5) labels. `add_symbol(env, lib_id, refdes, value, at, angle)`: loads geometry, registers the lib_symbol, emits a `(symbol (lib_id …) (at x y angle) (uuid …) (property "Reference" refdes …) (property "Value" value …) … (instances (project "" (path "/<root-uuid>" (reference refdes) (unit 1)))))`. `finish()` assembles the full document deterministically (lib_symbols sorted by lib_id, symbols by refdes). Use the spike's proven structure. All UUIDs via `stable_uuid`. Match the exact property/effects layout KiCAD needs by iterating against parse-back + ERC.
- [ ] **Step 4 — run, confirm pass.** This is the first real emission — expect iteration on exact S-expr shape until kiutils parses and netlist shows the component.
- [ ] **Step 5 — fmt + commit:** `feat(sch-engine): schematic writer emits loadable symbol instances`

---

### Task 4: Placement engine

**Files:** Create `crates/sch-engine/src/place.rs`; tests in-module (pure, deterministic).

Turns a `Design` into per-component positions. MVP bar (spec §8): tidy, grid-aligned, ERC-clean — not beautiful.

- [ ] **Step 1 — failing tests:**

```rust
#[test]
fn places_blocks_into_non_overlapping_regions_on_grid() {
    let design = small_two_block_design(); // helper: 2 blocks, a few comps each
    let layout = place(&design);
    // every component has a position, all on the 1.27mm grid
    for pos in layout.positions.values() {
        assert_eq!(*pos, crate::grid::snap_point(*pos));
    }
    // components in different blocks don't overlap (bbox check with margins)
    assert!(no_overlaps(&layout));
    // determinism: same design -> same layout
    assert_eq!(layout, place(&design));
}

#[test]
fn edge_hints_push_blocks_to_sheet_sides() {
    let design = design_with_edge_hints(); // block A {edge: left}, block B {edge: right}
    let layout = place(&design);
    let ax = block_centroid_x(&layout, "a");
    let bx = block_centroid_x(&layout, "b");
    assert!(ax < bx, "left-edge block must be left of right-edge block");
}
```

(Provide the helper fns in the test module: build `circuit_lang::Design` values directly via its public model types, or compile small YAML via `circuit_lang::compile` with a `MockSymbolProvider`.)

- [ ] **Step 2 — run, confirm fail.**
- [ ] **Step 3 — implement.** `place(&Design) -> Layout { positions: IndexMap<RefDes, [f64;2]> }`. Algorithm: order blocks (edge-pinned first by edge, then the rest); each block gets a rectangular region sized to its component count; within a block, lay components in a grid (rows/cols), each cell on the 1.27 mm grid with generous margins. Synthesized decouple caps placed adjacent to their `ap_parent`. Deterministic ordering throughout (natural refdes sort). No force-directed refinement (deferred). Bounding-box size per component can be approximate (fixed cell size is fine for MVP).
- [ ] **Step 4 — run, confirm pass.**
- [ ] **Step 5 — fmt + commit:** `feat(sch-engine): deterministic block placement engine`

---

### Task 5: Connectivity emission — labels at pin endpoints

**Files:** Modify `crates/sch-engine/src/emit.rs` (label emission + pin-endpoint transform); test `crates/sch-engine/tests/emit_connected.rs`.

- [ ] **Step 1 — failing test:**

```rust
use kicad_bridge::{env::KicadEnv, cli::KicadCli};
use sch_engine::emit::SchematicWriter;

#[test]
fn two_pins_same_net_name_are_electrically_connected() {
    let Some(env) = KicadEnv::detect() else { eprintln!("SKIP"); return };
    let mut w = SchematicWriter::new();
    // two resistors, pin "1" of each on net SIG, pin "2" on GND
    w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0).unwrap();
    w.add_symbol(&env, "Device:R", "R2", "1k", [152.4, 63.5], 0.0).unwrap();
    w.add_pin_label(&env, "R1", "1", "SIG").unwrap();
    w.add_pin_label(&env, "R2", "1", "SIG").unwrap();
    w.add_pin_label(&env, "R1", "2", "GND").unwrap();
    w.add_pin_label(&env, "R2", "2", "GND").unwrap();
    let text = w.finish();
    let tmp = tempfile::Builder::new().suffix(".kicad_sch").tempfile().unwrap();
    std::fs::write(tmp.path(), &text).unwrap();

    let nl = KicadCli::new(&env).netlist(tmp.path()).unwrap();
    // SIG net joins R1.1 and R2.1 (2 nodes) — connectivity via same-named labels
    let sig = nl.nets.iter().find(|n| n.name.contains("SIG")).expect("SIG net");
    assert_eq!(sig.nodes.len(), 2);
    // no off-grid endpoint ERC violations
    let report = KicadCli::new(&env).erc(tmp.path()).unwrap();
    assert_eq!(report.violations.iter().filter(|v| v.kind == "endpoint_off_grid").count(), 0);
}
```

- [ ] **Step 2 — run, confirm fail.**
- [ ] **Step 3 — implement.** `add_pin_label(env, refdes, pin, net)`: look up the symbol geometry for that refdes's lib_id, find the pin by number/name, compute the connection endpoint = transform(local pin `at` + `length` projected along `angle`, instance position, instance angle, mirror) into sheet coordinates — reuse the spike's proven transform (Y-down; handle angle 0/90/180/270 and mirror). Snap to grid. Emit a `(label "<net>" (at x y rot) (effects …) (uuid …))` at that point. Power nets MAY instead emit a power symbol — but plain labels suffice for ERC; keep power symbols as an optional enhancement (a `#GND`/`#PWR` global label or a power-symbol instance). Verify the endpoint math against parse-back + netlist (the netlist proving 2 nodes share the net is the real test).
- [ ] **Step 4 — run, confirm pass.** Iterate the transform until the netlist shows shared nets and ERC shows no off-grid endpoints.
- [ ] **Step 5 — fmt + commit:** `feat(sch-engine): connectivity via net-name labels at pin endpoints`

---

### Task 6: Full emit — `Design` → `.kicad_sch`, ERC-clean on bluepill (THE milestone)

**Files:** Create `crates/sch-engine/src/lib.rs` facade fn `emit_design`; test `crates/sch-engine/tests/bluepill_emit.rs`.

- [ ] **Step 1 — failing test:**

```rust
use kicad_bridge::{env::KicadEnv, provider::RealSymbolProvider, cli::KicadCli};
use sch_engine::emit_design;

#[test]
fn bluepill_design_emits_and_ercs_clean() {
    let Some(env) = KicadEnv::detect() else { eprintln!("SKIP"); return };
    let provider = RealSymbolProvider::new(KicadEnv::detect().unwrap());
    let src = include_str!("../../../docs/validation/bedrock-oneshot-bluepill.circuit.yaml");
    let design = circuit_lang::compile(src, &provider).design.expect("compiles");

    let text = emit_design(&env, &design).unwrap();          // Design -> .kicad_sch text
    let tmp = tempfile::Builder::new().suffix(".kicad_sch").tempfile().unwrap();
    std::fs::write(tmp.path(), &text).unwrap();

    // KiCAD loads it; netlist has all components; ERC has ZERO errors
    let nl = KicadCli::new(&env).netlist(tmp.path()).unwrap();
    let comp_count: usize = design.blocks.values().map(|b| b.components.len()).sum();
    assert_eq!(nl.components.len(), comp_count);
    let report = KicadCli::new(&env).erc(tmp.path()).unwrap();
    assert_eq!(report.error_count(), 0, "ERC errors: {:?}", report.violations);
}

#[test]
fn emit_is_byte_deterministic() {
    let Some(env) = KicadEnv::detect() else { eprintln!("SKIP"); return };
    let provider = RealSymbolProvider::new(KicadEnv::detect().unwrap());
    let src = include_str!("../../../docs/validation/bedrock-oneshot-bluepill.circuit.yaml");
    let design = circuit_lang::compile(src, &provider).design.unwrap();
    assert_eq!(emit_design(&env, &design).unwrap(), emit_design(&env, &design).unwrap());
}
```

- [ ] **Step 2 — run, confirm fail.**
- [ ] **Step 3 — implement `emit_design(&KicadEnv, &Design) -> io::Result<String>`:** run `place()` for positions; for each component `add_symbol`; for each component pin mapped to a net, `add_pin_label` (skip `NoConnect` pins — emit a `(no_connect (at …))` marker at those endpoints instead, so ERC is happy about intentional NCs); `finish()`. Auto-NC pins from the kernel (PinTarget::NoConnect) become `(no_connect …)` markers. Power nets (kernel `nets[x].power`) may get power symbols. **Iterate against real ERC on the bluepill until `error_count() == 0`.** Warnings (e.g. single-pin-net isolated labels) are acceptable; errors are not. If specific ERC errors are structural (e.g. power-input needs a power flag), address them (a `(no_connect)` or power symbol) — document any residual accepted warnings.
- [ ] **Step 4 — run, confirm pass.** This is MVP criterion #1. Expect real iteration. If a class of ERC error is intractable for a specific part, document it and assert `error_count()` excludes that documented class with a tracking note — but aim for a true zero.
- [ ] **Step 5 — fmt + commit:** `feat(sch-engine): emit full Design to ERC-clean .kicad_sch (bluepill milestone)`

---

### Task 7: Reconciliation — preserve user positions on re-emit

**Files:** Create `crates/sch-engine/src/reconcile.rs`; modify `emit_design` to accept an optional existing `.kicad_sch`; test `crates/sch-engine/tests/reconcile.rs`.

- [ ] **Step 1 — failing test:**

```rust
#[test]
fn surviving_symbols_keep_their_positions() {
    let Some(env) = KicadEnv::detect() else { eprintln!("SKIP"); return };
    // emit v1, then move R1 to a known spot (simulate user edit by rewriting its (at ...)),
    // then emit v2 from a design that adds a part; R1 must keep the moved position.
    // (Build a tiny 2-part design; emit; parse positions via kiutils; mutate R1's at in the
    //  text; re-emit with the prior text as the reconcile base; assert R1.at unchanged, new part placed.)
    // See plan body for the full helper sequence.
    todo!("implement per the step-3 contract")
}
```

(Write this test concretely in implementation: emit a 2-part design → read R1's `(at …)` via `kiutils_kicad::SchematicFile::read` → produce a modified base where R1 is at a distinctive position → call `emit_design_reconciled(env, &design_with_3_parts, Some(&base_text))` → parse output → assert R1's position equals the distinctive one and the new part R3 has a fresh placed position.)

- [ ] **Step 2 — run, confirm fail.**
- [ ] **Step 3 — implement.** `reconcile.rs`: parse the prior `.kicad_sch` (kiutils) into `{refdes -> (at, angle)}` (match synthesized parts by `ap_parent/ap_role/ap_index` properties). `emit_design_reconciled(env, design, prior: Option<&str>)`: placement uses prior positions for surviving refdes, `place()` only for new ones; UUIDs for surviving symbols reused from prior (read them) so diffs stay minimal; deleted parts' labels/NC markers dropped. Write `ap_block`/`ap_role`/`ap_parent`/`ap_index` properties on every emitted symbol so the next lift/reconcile is self-describing (spec §4). Make `emit_design` delegate to `emit_design_reconciled(env, design, None)`.
- [ ] **Step 4 — run, confirm pass.**
- [ ] **Step 5 — fmt + commit:** `feat(sch-engine): reconciliation preserves user positions and ap_* identity`

---

### Task 8: Lift — `.kicad_sch` → kernel YAML, round-trip

**Files:** Create `crates/sch-engine/src/lift.rs`; test `crates/sch-engine/tests/lift_roundtrip.rs`.

- [ ] **Step 1 — failing test:**

```rust
#[test]
fn emit_then_lift_roundtrips_to_canonical_yaml() {
    let Some(env) = KicadEnv::detect() else { eprintln!("SKIP"); return };
    let provider = RealSymbolProvider::new(KicadEnv::detect().unwrap());
    let src = include_str!("../../../docs/validation/bedrock-oneshot-bluepill.circuit.yaml");
    let design = circuit_lang::compile(src, &provider).design.unwrap();
    let canon_in = circuit_lang::canon::to_canonical_yaml(&design);

    let text = sch_engine::emit_design(&env, &design).unwrap();
    let tmp = tempfile::Builder::new().suffix(".kicad_sch").tempfile().unwrap();
    std::fs::write(tmp.path(), &text).unwrap();

    let lifted_yaml = sch_engine::lift::lift(&env, tmp.path()).unwrap();   // -> canonical kernel YAML
    let lifted = circuit_lang::compile(&lifted_yaml, &provider).design.unwrap();
    let canon_out = circuit_lang::canon::to_canonical_yaml(&lifted);

    // connectivity + structure survive the round-trip (positions are intentionally absent from YAML)
    assert_eq!(canon_in, canon_out, "emit->lift must preserve the kernel model");
}
```

- [ ] **Step 2 — run, confirm fail.**
- [ ] **Step 3 — implement `lift::lift(&KicadEnv, &Path) -> io::Result<String>`:** run `KicadCli::netlist` for authoritative connectivity (components + nets/nodes); read `ap_block/ap_role/ap_parent/ap_index` and value/footprint via kiutils symbol properties; reconstruct a `circuit_lang::Design` (group components into blocks by `ap_block`; pins → nets from the netlist nodes; re-sugar synthesized decouple caps by their `ap_*` tags; drop auto-NC pins so the lifted YAML stays sparse) and emit canonical YAML via `circuit_lang::canon`. **Round-trip caveat:** the emit→lift cycle must preserve connectivity and structure; auto-NC pins are re-derived by compile, not stored in YAML. If exact `canon_in == canon_out` is blocked by a representable difference (e.g. net auto-naming `N_*` vs netlist names), narrow the assertion to structural equality (same components, same net partition) and document the residual — but target full equality.
- [ ] **Step 4 — run, confirm pass.**
- [ ] **Step 5 — fmt + commit:** `feat(sch-engine): lift .kicad_sch back to canonical kernel YAML`

---

## Definition of done

- **MVP criterion #1 met:** the validated bluepill design emits a `.kicad_sch` that KiCAD 10 loads and ERCs with **0 errors** (Task 6).
- Emission is byte-deterministic; placement and IDs are deterministic.
- Reconciliation preserves user positions and `ap_*` identity (Task 7).
- emit → lift round-trips to the same kernel model (Task 8).
- All gates green; KiCAD integration tests RUN on this machine.
- Deferred (recorded): pretty wire routing / force-directed refinement (spec §8 "prettify"); power-symbol styling; multi-sheet. Plan 4 (agent loop + TUI) wires this into the copilot UX and real Bedrock.
