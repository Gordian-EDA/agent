# Footprint Round-Trip Repair Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a component's footprint survive the schematic round-trip — written into the `.kicad_sch` symbol `Footprint` field by `emit`, and read back into the kernel `Component` by `lift`.

**Architecture:** Two independent drops in the same round-trip, fixed in the layer that owns each direction. `lift::design_from_netlist` currently builds the `Component` with `..Component::default()`, discarding the `Footprint` property the netlist parser already captured — fix the read. `emit::render_instance` hard-writes `(property "Footprint" "")` — thread the real footprint from the kernel `Component` through `Item` → `add_symbol_full` → `Instance` and fix the write.

**Tech Stack:** Rust, `kicad_sexpr` S-expr emit, `kicad-cli` `kicadxml` netlist, `circuit_lang` model.

**Prerequisite for:** the `.kicad_pcb`-state-unification plan (`docs/specs/unified-kicad-pcb-state.md`). `create_board` cannot read footprint assignments from the schematic until the schematic actually carries them. This plan is independently shippable and fixes a standalone bug (no DSL-assigned footprint reaches `.kicad_sch` today).

---

## File Structure

- `crates/sch-layout/src/lift.rs` — **read** direction. Add `kernel_footprint` helper; populate `Component.footprint` in `design_from_netlist`. Add a unit test.
- `crates/sch-layout/src/floorplan.rs` — plumb footprint onto `Item` (the placed-component struct) and into the emit call.
- `crates/sch-layout/src/emit.rs` — **write** direction. Add `footprint` to `Instance`; thread it through `add_symbol_full`; emit the real property in `render_instance`. Add a unit test.
- No new files. `circuit_lang::model::Component.footprint` already exists (`model.rs:42`); the netlist parser already captures `<footprint>` into `NetComp.properties["Footprint"]` (`cli.rs:334`). Nothing to change there.

---

### Task 1: `lift` reads the `Footprint` property into `Component.footprint`

**Files:**
- Modify: `crates/sch-layout/src/lift.rs` (the `Component` build at ~`lift.rs:111`, plus a new helper near `kernel_value` at `lift.rs:259`)
- Test: `crates/sch-layout/src/lift.rs` (the `#[cfg(test)] mod tests` at `lift.rs:267`)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/sch-layout/src/lift.rs`. The existing `comp(reference, value, lib_id, props)` helper (`lift.rs:271`) builds a `NetComp` whose `props` land in `.properties`, so a `("Footprint", …)` pair drives the read path. Find the component across blocks (don't hardcode the block key):

```rust
    #[test]
    fn lifts_footprint_from_property() {
        let netlist = Netlist {
            components: vec![comp(
                "C1",
                "100nF",
                "Device:C",
                &[("Footprint", "Capacitor_SMD:C_0603_1608Metric")],
            )],
            nets: vec![],
        };
        let d = design_from_netlist(&netlist);
        let c = d
            .blocks
            .values()
            .flat_map(|b| b.components.iter())
            .find(|(r, _)| r.as_str() == "C1")
            .map(|(_, c)| c)
            .expect("C1 present");
        assert_eq!(
            c.footprint.as_deref(),
            Some("Capacitor_SMD:C_0603_1608Metric")
        );
    }

    #[test]
    fn empty_or_tilde_footprint_lifts_to_none() {
        for fp in ["", "~"] {
            let netlist = Netlist {
                components: vec![comp("R1", "1k", "Device:R", &[("Footprint", fp)])],
                nets: vec![],
            };
            let d = design_from_netlist(&netlist);
            let c = d
                .blocks
                .values()
                .flat_map(|b| b.components.iter())
                .find(|(r, _)| r.as_str() == "R1")
                .map(|(_, c)| c)
                .expect("R1 present");
            assert_eq!(c.footprint, None, "footprint {fp:?} must lift to None");
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --release -p sch-layout lifts_footprint_from_property empty_or_tilde_footprint_lifts_to_none`
Expected: FAIL — `lifts_footprint_from_property` asserts `Some(...)` but gets `None` (the `Component` is built with `..Component::default()`, so `footprint` is `None`).

- [ ] **Step 3: Add the `kernel_footprint` helper**

Add next to `kernel_value` in `crates/sch-layout/src/lift.rs` (after the `kernel_value` fn ending at `lift.rs:265`). Mirror its empty/`~` → `None` convention:

```rust
/// A component's footprint lib_id, or `None` when it has none.
///
/// The netlist parser folds `<footprint>` / a `Footprint` `<field>` / `<property>`
/// into `NetComp.properties["Footprint"]` (`cli.rs:334`). Emit writes KiCAD's
/// empty placeholder (`""`, or `~` for some fields) when a part is unassigned, so
/// both map to `None` here — exactly as `kernel_value` treats the `Value` field.
fn kernel_footprint(props: &std::collections::HashMap<String, String>) -> Option<String> {
    match props.get("Footprint") {
        Some(f) if !f.is_empty() && f != "~" => Some(f.clone()),
        _ => None,
    }
}
```

- [ ] **Step 4: Populate `Component.footprint` in `design_from_netlist`**

In `crates/sch-layout/src/lift.rs`, the `Component` build at `lift.rs:111`:

```rust
        let kernel = Component {
            part: comp.lib_id.clone(),
            value: kernel_value(&comp.value),
            origin,
            ..Component::default()
        };
```

becomes:

```rust
        let kernel = Component {
            part: comp.lib_id.clone(),
            value: kernel_value(&comp.value),
            footprint: kernel_footprint(&comp.properties),
            origin,
            ..Component::default()
        };
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --release -p sch-layout lifts_footprint_from_property empty_or_tilde_footprint_lifts_to_none`
Expected: PASS (both).

- [ ] **Step 6: Run the existing lift suite to confirm no regression**

Run: `cargo test --release -p sch-layout lift`
Expected: PASS (existing `reconstructs_blocks_components_and_connectivity` etc. unaffected — `footprint` is additive).

- [ ] **Step 7: Commit**

```bash
git add crates/sch-layout/src/lift.rs
git commit -m "fix(lift): read the Footprint property into Component.footprint"
```

---

### Task 2: `emit` writes the real `Footprint` property

**Files:**
- Modify: `crates/sch-layout/src/floorplan.rs` (the `Item` struct at `floorplan.rs:1548`; the `Item` build at `floorplan.rs:2030`; the emit call at `floorplan.rs:1916`)
- Modify: `crates/sch-layout/src/emit.rs` (the `Instance` struct at `emit.rs:65`; `add_symbol`/`add_symbol_full` at `emit.rs:280`/`emit.rs:304`; the `Instance` push at `emit.rs:331`; `render_instance` at `emit.rs:2136`)
- Test: `crates/sch-layout/src/emit.rs` (the `tests` module)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/sch-layout/src/emit.rs`. Follow the local skip-without-KiCAD pattern (`let Some(env) = detect_env() else { return };`, as in `escapes_free_form_strings_in_output` at `emit.rs:2432`) and the `SchematicWriter::new()` → `add_symbol_full` → `finish()` shape:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails to compile**

Run: `cargo test --release -p sch-layout writes_footprint_property_from_instance`
Expected: FAIL — compile error: `add_symbol_full` takes 8 args, not 9 (the `Some("…")` footprint arg does not exist yet).

- [ ] **Step 3: Add `footprint` to the `Instance` struct**

In `crates/sch-layout/src/emit.rs`, the `Instance` struct (`emit.rs:65`), add after `value: String,` (`emit.rs:68`):

```rust
    /// Footprint lib_id for the symbol's `Footprint` property, or `None` when the
    /// part is unassigned (emits an empty property, KiCAD's placeholder). Sourced
    /// from the kernel `Component.footprint` via `Item` (the schematic-side home
    /// of footprint assignment — see docs/specs/unified-kicad-pcb-state.md).
    footprint: Option<String>,
```

- [ ] **Step 4: Thread `footprint` through `add_symbol` / `add_symbol_full`**

In `crates/sch-layout/src/emit.rs`, change the 6-arg convenience `add_symbol` (`emit.rs:281`) to pass `None` for footprint:

```rust
    pub fn add_symbol(
        &mut self,
        env: &KicadEnv,
        lib_id: &str,
        refdes: &str,
        value: &str,
        at: [f64; 2],
        angle: f64,
    ) -> io::Result<()> {
        self.add_symbol_full(env, lib_id, refdes, value, at, angle, None, &[], None)
    }
```

Add the `footprint: Option<&str>` parameter to `add_symbol_full` (`emit.rs:304`), inserted after `angle: f64,` and before `extra_props`:

```rust
    #[allow(clippy::too_many_arguments)]
    pub fn add_symbol_full(
        &mut self,
        env: &KicadEnv,
        lib_id: &str,
        refdes: &str,
        value: &str,
        at: [f64; 2],
        angle: f64,
        footprint: Option<&str>,
        extra_props: &[(String, String)],
        uuid: Option<String>,
    ) -> io::Result<()> {
```

In the `Instance` push inside `add_symbol_full` (`emit.rs:331`), add the field after `value: value.to_string(),`:

```rust
            footprint: footprint.map(str::to_string),
```

- [ ] **Step 5: Emit the real property in `render_instance`**

In `crates/sch-layout/src/emit.rs`, `render_instance` (`emit.rs:2136`), replace:

```rust
    let _ = writeln!(s, "\t\t(property \"Footprint\" \"\"");
```

with:

```rust
    let footprint = escape_sexpr_string(inst.footprint.as_deref().unwrap_or(""));
    let _ = writeln!(s, "\t\t(property \"Footprint\" \"{footprint}\"");
```

(`escape_sexpr_string` is already used in this file for `value`/`extra_props`, so a lib_id with odd characters stays well-formed.)

- [ ] **Step 6: Run the emit tests to verify they pass**

Run: `cargo test --release -p sch-layout writes_footprint_property_from_instance omitted_footprint_emits_empty_property`
Expected: PASS (both). If KiCAD is absent both early-return and report as passed — run on a host with KiCAD to exercise them.

- [ ] **Step 7: Add `footprint` to `Item` and populate it from the kernel `Component`**

In `crates/sch-layout/src/floorplan.rs`, the `Item` struct (`floorplan.rs:1548`), add after `value: String,`:

```rust
    /// Footprint lib_id from the kernel `Component`, carried to emit so the
    /// `.kicad_sch` symbol records its assignment. Multi-unit parts set this on
    /// the FIRST emitted unit only (like `value`) to avoid duplicate fields.
    footprint: Option<String>,
```

In the `Item` build (`floorplan.rs:2030`), add the field alongside `value` (first-unit-only, matching the `value` rule one line above):

```rust
                items.push(Item {
                    refdes: refdes.clone(),
                    part: comp.part.clone(),
                    value: if k == 0 { value.clone() } else { String::new() },
                    footprint: if k == 0 { comp.footprint.clone() } else { None },
                    geom: geom.clone(),
                    pins: unit_pins,
                    at: [0.0, 0.0],
                    angle: 0.0,
                    unit: u,
                    mirror: false,
                    frozen: false,
                });
```

- [ ] **Step 8: Pass the footprint at the emit call**

In `crates/sch-layout/src/floorplan.rs`, the emit call (`floorplan.rs:1916`):

```rust
        w.add_symbol(env, &it.part, &it.refdes, &it.value, it.at, it.angle)?;
```

becomes (route through `add_symbol_full` to pass the footprint; `&[]`/`None` preserve the prior no-extra-props, content-uuid behaviour):

```rust
        w.add_symbol_full(
            env,
            &it.part,
            &it.refdes,
            &it.value,
            it.at,
            it.angle,
            it.footprint.as_deref(),
            &[],
            None,
        )?;
```

- [ ] **Step 9: Run the sch-layout suite to confirm the plumbing compiles and nothing regressed**

Run: `cargo test --release -p sch-layout`
Expected: PASS. (The new `Item.footprint` field is consumed at the single emit site; any other `Item { … }` literal in the crate — if the compiler flags one — needs `footprint: None` added.)

- [ ] **Step 10: Commit**

```bash
git add crates/sch-layout/src/emit.rs crates/sch-layout/src/floorplan.rs
git commit -m "fix(emit): write the component footprint into the symbol Footprint field"
```

---

### Task 3: End-to-end round-trip test (emit → lift)

**Files:**
- Test: `crates/sch-layout/tests/footprint_roundtrip.rs` (new integration test)

- [ ] **Step 1: Write the round-trip test**

Create `crates/sch-layout/tests/footprint_roundtrip.rs`. It compiles a one-component design with a footprint, emits a `.kicad_sch`, lifts it back via `kicad-cli`, and asserts the footprint survived. Gate on KiCAD like the crate's other integration tests (early-return when `KicadEnv::detect()` is `None`):

```rust
//! End-to-end guard for the footprint round-trip: a footprint authored in the
//! circuit model must survive emit -> .kicad_sch -> lift back into the model.
//! This is the path the harnesses miss (they build drafts from standalone JSON).

use circuit_lang::compile;
use kicad_cli_rs::env::KicadEnv;
use sch_layout::lift::lift;

#[test]
fn footprint_survives_emit_then_lift() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("no KiCAD environment — skipping round-trip test");
        return;
    };

    // Minimal design: one capacitor carrying a footprint assignment.
    let yaml = r#"
blocks:
  main:
    components:
      C1:
        part: Device:C
        value: 100nF
        footprint: Capacitor_SMD:C_0603_1608Metric
        pins: { "1": VCC, "2": GND }
"#;
    let design = compile(yaml, /* provider */ Default::default())
        .design
        .expect("yaml compiles to a design");

    // Emit to a temp .kicad_sch (use the crate's public emit/commit entry point).
    let dir = tempfile::tempdir().unwrap();
    let sch_path = dir.path().join("rt.kicad_sch");
    sch_layout::emit_design_to_path(&env, &design, &sch_path)
        .expect("emit a .kicad_sch");

    // Lift it back and confirm the footprint is present on C1.
    let lifted_yaml = lift(&env, &sch_path).expect("lift the schematic");
    let lifted = compile(&lifted_yaml, Default::default())
        .design
        .expect("lifted yaml compiles");
    let c1 = lifted
        .blocks
        .values()
        .flat_map(|b| b.components.iter())
        .find(|(r, _)| r.as_str() == "C1")
        .map(|(_, c)| c)
        .expect("C1 round-trips");
    assert_eq!(
        c1.footprint.as_deref(),
        Some("Capacitor_SMD:C_0603_1608Metric"),
        "footprint must survive emit -> lift"
    );
}
```

- [ ] **Step 2: Reconcile the emit entry point**

The test above calls `sch_layout::emit_design_to_path(&env, &design, &sch_path)` and `compile(yaml, Default::default())` as the expected names. Confirm the crate's real public emit-and-write entry point and `compile` provider argument:

Run: `grep -rn "pub fn emit\|pub fn commit\|pub fn write_schematic\|fn emit_design\|pub fn compile" crates/sch-layout/src/lib.rs crates/circuit-lang/src/lib.rs`

If the public emit function has a different name/signature (e.g. it takes the floorplan result, or returns a `String` you write yourself), adjust the test's emit line to match it — the test's *assertion* (footprint present after lift) is the contract; the plumbing to produce the `.kicad_sch` uses whatever the crate already exposes. If `compile` takes no provider, drop the `Default::default()` arg.

- [ ] **Step 3: Run the round-trip test**

Run: `cargo test --release -p sch-layout --test footprint_roundtrip -- --nocapture`
Expected: PASS on a host with KiCAD installed (footprint round-trips). Prints the skip message and passes where KiCAD is absent.

- [ ] **Step 4: Run the netlist oracle gate (CLAUDE.md requirement)**

Run: `cargo test --release -p sch-layout --test floorplan_netlist`
Expected: PASS — emit/lift changes must not break connectivity. A prettier or richer emit that breaks the netlist oracle is a regression.

- [ ] **Step 5: Commit**

```bash
git add crates/sch-layout/tests/footprint_roundtrip.rs
git commit -m "test(sch-layout): end-to-end footprint emit->lift round-trip"
```

---

## Self-Review

**Spec coverage** (against `docs/specs/unified-kicad-pcb-state.md` §1 "Repair the footprint round-trip"):
- emit writes real footprint — Task 2, Steps 3-8. ✓
- lift reads footprint into `Component.footprint` — Task 1, Steps 3-4. ✓
- Round-trip test (the gap harnesses miss) — Task 3. ✓
- Empty/unassigned preserved as `None`/`""` — Task 1 Step 1 (`empty_or_tilde…`), Task 2 Step 1 (`omitted_footprint…`). ✓
- Netlist oracle gate — Task 3, Step 4. ✓

(Spec §2-§6 — `assign_footprint`, `create_board`/`auto_layout`/`auto_route` over the file, session save, `board.json` deletion — are the **follow-on** unification plan, deliberately out of scope here. This plan is the prerequisite.)

**Placeholder scan:** No "TBD"/"handle edge cases"/"similar to". Task 3 Step 2 is an explicit *reconcile-the-entry-point* step with a concrete `grep` and a stated contract, not a placeholder — the assertion is fixed; only the emit-call plumbing adapts to the crate's actual public API (which I did not fully read for this crate's top-level emit fn).

**Type consistency:** `Component.footprint: Option<String>` (model.rs:42) ↔ `kernel_footprint -> Option<String>` ↔ `Item.footprint: Option<String>` ↔ `add_symbol_full(footprint: Option<&str>)` via `it.footprint.as_deref()` ↔ `Instance.footprint: Option<String>` via `footprint.map(str::to_string)` ↔ `inst.footprint.as_deref().unwrap_or("")`. Consistent end to end. `add_symbol` stays 6-arg (test call sites unaffected); only `add_symbol_full` gains the parameter (one non-test caller, updated in Step 8).

## Known follow-up (next plan)

The state-unification plan builds on this: `assign_footprint` patches `draft.circuit.yaml`'s `footprint:` and re-commits via `apply_design` (now durable, because of this plan); `create_board` reads assignment from the schematic; `auto_layout`/`auto_route` operate on the `.kicad_pcb`; `board.json`/`route.json` are deleted. It needs a `.kicad_pcb` footprint-position writer (new `kicad_sexpr` infra — `write_solution` writes copper, not positions) and the non-destructive re-sync via `ap_*` identity tags — both to be detailed against the actual `kicad_sexpr::pcb` API after this lands.
