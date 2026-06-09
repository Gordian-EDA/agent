# Plan 1b: circuit-lang Remediation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax. Each task is TDD: add the failing test (from the documented repro), watch it fail, fix, watch it pass, `cargo fmt --all`, commit.

**Goal:** Fix all 12 review-confirmed defects in `circuit-lang` and add kernel auto-no-connect, per the post-review decisions.

**Context:** `circuit-lang` (crates/circuit-lang/) is complete and green, but a holistic review found 2 Critical + 4 Important + 6 Minor confirmed defects (silent net corruption, non-determinism, round-trip instability, spec gaps). Spec: `docs/superpowers/specs/2026-06-09-kicad-copilot-agent-design.md` §5–6. Original plan: `docs/superpowers/plans/2026-06-09-circuit-lang.md`. **Decisions (binding):** auto-NC is materialized in the kernel; lowercase net names → warning; multiple YAML docs → error; lint suppression via design-level `lint: {allow: [...]}`. Work on `master`.

**Tech stack:** Rust edition 2024; deps saphyr + saphyr-parser (0.0.6) + indexmap + strsim. `circuit-lang` is PURE — no I/O.

**Global rules for every task:** TDD; full `cargo test -p circuit-lang` must stay green; run `cargo fmt --all` before committing; the gate is `cargo test -p circuit-lang && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`.

---

### Task R1: Duplicate-key detection + multi-document rejection (yaml.rs)

**Findings:** parser-invariant#0 (Important — duplicate map keys silently last-wins; a repeated `R1:` drops a component), parser-invariant#1 (Minor — trailing `---` documents silently discarded).

**Files:** Modify `crates/circuit-lang/src/yaml.rs`; tests in `crates/circuit-lang/src/parse.rs` (`mod tests`).

**Root cause:** saphyr's `MarkedYaml` mapping is a `LinkedHashMap` that dedups keys *before* our `convert` runs, so duplicates are unrecoverable post-hoc. Fix at the saphyr boundary.

- [ ] **Step 1 — failing tests** (add to `parse.rs` `mod tests`):

```rust
    #[test]
    fn duplicate_component_refdes_in_one_map_errors() {
        let src = "
version: 1
blocks:
  main:
    components:
      R1: {part: R, value: 1k, pins: {1: A, 2: GND}}
      R1: {part: C, value: 2k, pins: {1: A, 2: GND}}
";
        let (_, diags) = parse_str(src);
        assert!(diags.0.iter().any(|d| d.code == "duplicate-key"),
            "expected duplicate-key, got {:?}", diags);
    }

    #[test]
    fn duplicate_field_key_errors() {
        let (_, diags) = parse_str(
            "version: 1\nblocks: {main: {components: {R1: {part: R, value: 1k, value: 2k}}}}");
        assert!(diags.0.iter().any(|d| d.code == "duplicate-key"));
    }

    #[test]
    fn multiple_documents_error() {
        let src = "version: 1\nblocks: {main: {components: {}}}\n---\nversion: 1\nblocks: {other: {components: {}}}";
        let (_, diags) = parse_str(src);
        assert!(diags.0.iter().any(|d| d.code == "multiple-documents"));
    }
```

- [ ] **Step 2 — run, confirm fail:** `cargo test -p circuit-lang` → the three new tests fail.

- [ ] **Step 3 — implement.** Rewrite `yaml::load` to drive the event stream via `saphyr_parser::Parser` (already a dependency) with a receiver/visitor that builds our `Node` tree directly, so we observe every key as it is inserted and can flag a duplicate within the same mapping. Preserve the existing invariants exactly: scalars surface as their literal source string (`4.7k`, `NO`, `true`, `1`), plain-style empty/`~`/`null` become `Node::Null`. Detection:
  - While building a `Node::Map`, keep a `HashSet<String>` of keys seen *in that mapping*; on a repeat, push `Diagnostic::error("duplicate-key", format!("duplicate key `{k}`"))` with the duplicate's span. Still insert (or keep first — choose first-wins and record the dup) so downstream parsing proceeds.
  - For documents: collect all parsed documents; if more than one, push `Diagnostic::error("multiple-documents", "only a single YAML document is supported; N extra document(s) ignored")` and use the first.
  - If driving the raw parser proves impractical, an acceptable alternative is to keep `MarkedYaml` for the tree but additionally run a lightweight duplicate-scan over the raw event stream for mapping keys; either way the literal-string invariant and all existing parse tests must still hold.
  - Diagnostics need a `Diagnostics` channel out of `yaml::load`; thread the collected diagnostics back (e.g. return `(Node, Diagnostics)` or accept `&mut Diagnostics`) and have `parse_str` merge them.

- [ ] **Step 4 — run, confirm pass:** `cargo test -p circuit-lang` (all green, including every pre-existing parse test).

- [ ] **Step 5 — fmt + commit:**
```bash
cargo fmt --all && git add -A && git commit -m "fix(circuit-lang): detect duplicate map keys and reject multi-document YAML"
```

---

### Task R2: Strict refdes regex + net-name casing warning (parse.rs)

**Findings:** parser-invariant#2 / spec-conformance#5 (Minor — refdes accepts interleaved `R1A2`), spec-conformance#4 / parser-invariant#3 (Minor — lowercase net names accepted).

**Files:** Modify `crates/circuit-lang/src/parse.rs`; tests in its `mod tests`.

- [ ] **Step 1 — failing tests:**

```rust
    #[test]
    fn refdes_must_be_letters_then_digits() {
        for bad in ["R1A2", "RA1B2", "R2C3"] {
            let src = format!(
                "version: 1\nblocks: {{main: {{components: {{{bad}: {{part: R, pins: {{1: A, 2: B}}}}}}}}}}");
            let (_, d) = parse_str(&src);
            assert!(d.0.iter().any(|x| x.code == "bad-refdes"), "{bad} should be rejected");
        }
        for ok in ["R1", "U10", "J2"] {
            let src = format!(
                "version: 1\nblocks: {{main: {{components: {{{ok}: {{part: R, pins: {{1: A, 2: B}}}}}}}}}}");
            let (_, d) = parse_str(&src);
            assert!(!d.0.iter().any(|x| x.code == "bad-refdes"), "{ok} should be accepted");
        }
    }

    #[test]
    fn lowercase_net_name_warns() {
        let (_, d) = parse_str(
            "version: 1\nblocks: {main: {components: {R1: {part: R, pins: {1: sda, 2: GND}}}}}");
        assert!(d.0.iter().any(|x| x.code == "net-name-case"
            && x.severity == crate::diag::Severity::Warning));
    }
```

- [ ] **Step 2 — run, confirm fail.**

- [ ] **Step 3 — implement.**
  - Refdes check (currently first-upper / all-alnum / last-digit): replace with strict `[A-Z]+[0-9]+` — split at the first ASCII digit; require the prefix is non-empty all-uppercase-letters and the suffix is non-empty all-digits, nothing else. Keep code `bad-refdes`.
  - In `check_net_name`, after the space/`/` checks, if the name contains any ASCII lowercase letter push `Diagnostic::warning("net-name-case", format!("net `{name}` is not UPPER_SNAKE"))` with span. Keep it a warning (do not block compilation).

- [ ] **Step 4 — run, confirm pass.**

- [ ] **Step 5 — fmt + commit:**
```bash
cargo fmt --all && git add -A && git commit -m "fix(circuit-lang): strict [A-Z]+[0-9]+ refdes and UPPER_SNAKE net-name warning"
```

---

### Task R3: Union-find correctness — comp/unit collision, determinism, duplicate-refdes, N_ name uniqueness (desugar.rs)

**Findings:** desugar-correctness#1 (Critical — comp/unit same-pin silently merges nets), desugar-correctness#0 (Critical — non-deterministic net naming), spec-conformance#2 (Important — duplicate refdes across blocks corrupts), desugar-correctness#5 (Minor — sanitize/N_ name collision).

**Files:** Modify `crates/circuit-lang/src/desugar.rs` (`resolve_pins`, `name_groups`, block-build, `sanitize`/N_-name); tests in `mod tests`.

- [ ] **Step 1 — failing tests** (use the review repros verbatim):

```rust
    #[test]
    fn comp_and_unit_same_pin_different_nets_is_hard_error() {
        let (_, diags) = run("
version: 1
blocks:
  main:
    components:
      U1:
        part: M:Op
        pins: {1: NET_A}
        units:
          A: {pins: {1: NET_B}}
");
        assert!(diags.0.iter().any(|d| d.code == "pin-conflict"),
            "comp+unit duplicate pin must hard-error, got {:?}", diags);
    }

    #[test]
    fn duplicate_refdes_across_blocks_errors() {
        let (_, diags) = run("
version: 1
blocks:
  a: {components: {R1: {part: R, pins: {1: NA1, 2: NA2}}}}
  b: {components: {R1: {part: R, pins: {1: NB1, 2: NB2}}}}
");
        assert!(diags.0.iter().any(|d| d.code == "duplicate-refdes"));
    }

    #[test]
    fn net_name_for_joined_named_groups_is_deterministic() {
        // run many times; the conflict-resolved winner must be stable (lexicographically smallest)
        for _ in 0..50 {
            let (d, _) = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:X, pins: {1: NET_A}}
      J2: {part: M:Y, pins: {1: NET_B, 2: U1.1}}
");
            // U1.1 and J2.2 are joined; both groups named -> deterministic winner NET_A (smallest)
            assert_eq!(d.blocks["main"].components["J2"].pins["2"],
                crate::model::PinTarget::Net("NET_A".into()));
        }
    }

    #[test]
    fn generated_unnamed_net_names_are_unique() {
        let (d, _) = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:X, pins: {VDD: 3V3}}
      Z8: {part: M:Y, pins: {1: U1.A_B}}
      Z9: {part: M:Y, pins: {1: U1.A.B}}
");
        let n8 = &d.blocks["main"].components["Z8"].pins["1"];
        let n9 = &d.blocks["main"].components["Z9"].pins["1"];
        assert_ne!(n8, n9, "distinct unnamed nets must get distinct generated names");
    }
```

(If the `run` helper or `M:X`/`M:Y`/`M:Op` mock symbols are not present in the existing `mod tests`, add minimal mock entries; `M:*` symbols only need enough pins for these probes. Use `MockSymbolProvider`.)

- [ ] **Step 2 — run, confirm fail.**

- [ ] **Step 3 — implement (four coordinated fixes):**
  1. **comp/unit collision → hard error.** In `resolve_pins`, when two `RawPin`s share the same `(refdes, pin)` node but carry *different* net targets (and at least one is a plain net name, not a pin-ref), push `Diagnostic::error("pin-conflict", …)` instead of silently unioning/overwriting. (Detect before/at union time by tracking the first concrete net assigned to each node.)
  2. **Deterministic naming.** In `name_groups`, replace `for (i,name) in named { group_name.insert(root, name) }` (HashMap-iteration-order-dependent) with: for each root, pick the **lexicographically smallest** author name among that group's named members. Make the `net-conflict` diagnostic message list the two names in sorted order too.
  3. **Global duplicate-refdes.** While building blocks (before `resolve_pins`/`comp_block`), track a `HashSet<RefDes>` across all blocks; on a second occurrence push `Diagnostic::error("duplicate-refdes", …)`. Do this before `comp_block` is collected so its map is unambiguous.
  4. **N_ name uniqueness.** After computing all unnamed-group `N_<smallest member>` names, detect collisions among *distinct roots*; deterministically disambiguate (e.g. sort colliding roots by smallest member and append `_2`, `_3`, …). Connectivity is unaffected; only the display name changes.

- [ ] **Step 4 — run, confirm pass** (all new + all pre-existing desugar tests).

- [ ] **Step 5 — fmt + commit:**
```bash
cargo fmt --all && git add -A && git commit -m "fix(circuit-lang): union-find correctness — pin-conflict, determinism, duplicate-refdes, unique N_ names"
```

---

### Task R4: `between` desugars in pin-NUMBER order (desugar.rs)

**Finding:** spec-conformance#3 (Minor).

**Files:** Modify `crates/circuit-lang/src/desugar.rs` (`apply_between` + polarized-warning message); test in `mod tests`.

- [ ] **Step 1 — failing test:**

```rust
    #[test]
    fn between_assigns_by_numeric_pin_order() {
        // symbol whose library lists pins out of numeric order: index0=number "2", index1=number "1"
        use crate::provider::{MockSymbolProvider, PinType};
        let mut p = MockSymbolProvider::with_basics();
        p.add("My:Weird", vec![("2", "~", PinType::Passive, 1), ("1", "~", PinType::Passive, 1)]);
        let (s, _) = crate::parse::parse_str("
version: 1
blocks:
  main:
    components:
      X1: {part: My:Weird, between: [AAA, BBB]}
");
        let (d, diags) = desugar(&s.unwrap(), &p);
        assert!(!diags.has_errors(), "{:?}", diags);
        let x1 = &d.blocks["main"].components["X1"];
        assert_eq!(x1.pins["1"], crate::model::PinTarget::Net("AAA".into())); // a -> lowest pin number
        assert_eq!(x1.pins["2"], crate::model::PinTarget::Net("BBB".into()));
    }
```

- [ ] **Step 2 — run, confirm fail.**

- [ ] **Step 3 — implement.** In `apply_between`, before assigning, sort the symbol's two pins by numeric pin number (parse the number; fall back to string order if non-numeric), then map first arg → lower-numbered pin, second arg → higher. Update the polarized-warning message to print the sorted pin order.

- [ ] **Step 4 — run, confirm pass.**

- [ ] **Step 5 — fmt + commit:**
```bash
cargo fmt --all && git add -A && git commit -m "fix(circuit-lang): between desugars in numeric pin order"
```

---

### Task R5: decouple — provider-resolved rails + model-stable index order (desugar.rs, canon.rs)

**Findings:** spec-conformance#1 / desugar-correctness#3 (Important — number-keyed power pins → false ambiguous), lints-canon-purity#0 (Important — re-sugar permutes (index,value), breaking `compile(canon(d)) == d`).

**Files:** Modify `crates/circuit-lang/src/desugar.rs` (`synth_decouple` + its call site to receive the provider), `crates/circuit-lang/src/canon.rs` (coordination only if needed); tests in both `mod tests`.

- [ ] **Step 1 — failing tests:**

```rust
    // in desugar.rs mod tests
    #[test]
    fn decouple_resolves_power_pins_by_number() {
        use crate::provider::{MockSymbolProvider, PinType};
        let mut p = MockSymbolProvider::with_basics();
        p.add("M:CPU", vec![
            ("1", "VDD", PinType::PowerInput, 1), ("2", "VSS", PinType::PowerInput, 1)]);
        let (s, _) = crate::parse::parse_str("
version: 1
rails: [3V3, GND]
blocks:
  mcu:
    components:
      U1: {part: M:CPU, decouple: {100nF: 1}, pins: {1: 3V3, 2: GND}}
");
        let (d, diags) = desugar(&s.unwrap(), &p);
        assert!(!diags.0.iter().any(|x| x.code == "decouple-ambiguous"), "{:?}", diags);
        let caps = d.blocks["mcu"].components.values()
            .filter(|c| matches!(c.origin, crate::model::Origin::Synthesized { .. })).count();
        assert_eq!(caps, 1);
    }
```

```rust
    // in canon.rs mod tests — model stability across a round-trip with out-of-order multi-value decouple
    #[test]
    fn decouple_multivalue_round_trip_is_model_stable() {
        // compile() lives in lib.rs; use parse+desugar here with a provider exposing VDD/VSS by name.
        use crate::provider::{MockSymbolProvider, PinType};
        let mut p = MockSymbolProvider::with_basics();
        p.add("M:CPU", vec![
            ("VDD", "VDD", PinType::PowerInput, 1), ("VSS", "VSS", PinType::PowerInput, 1)]);
        let src = "
version: 1
rails: [3V3, GND]
blocks:
  mcu:
    components:
      U1: {part: M:CPU, decouple: {10uF: 1, 100nF: 1}, pins: {VDD: 3V3, VSS: GND}}
";
        let (s1, _) = crate::parse::parse_str(src);
        let (d1, _) = crate::desugar::desugar(&s1.unwrap(), &p);
        let out1 = to_canonical_yaml(&d1);
        let (s2, _) = crate::parse::parse_str(&out1);
        let (d2, _) = crate::desugar::desugar(&s2.unwrap(), &p);
        assert_eq!(d1, d2, "kernel model must be stable across canonical round-trip");
        assert_eq!(out1, to_canonical_yaml(&d2), "canonical emit must be a text fixpoint");
    }
```

- [ ] **Step 2 — run, confirm fail.**

- [ ] **Step 3 — implement.**
  - Thread the `&dyn SymbolProvider` into `synth_decouple` (change its signature and the call in `desugar`). In the `rail` inference, resolve each author pin-map key to the symbol pin NAME (number-first, then name; fall back to the raw key only when the symbol is unknown), and prefix-test VDD\*/VCC\* and VSS\*/GND\* against the resolved NAME.
  - Make `synth_decouple` assign `Origin::Synthesized { index }` by iterating the component's `decouple` map in the SAME order `canon` re-sugars it — i.e. sort the `(value, count)` entries by value string before assigning indices. This makes `compile(canon(d)) == d` for multi-value decouple.

- [ ] **Step 4 — run, confirm pass.**

- [ ] **Step 5 — fmt + commit:**
```bash
cargo fmt --all && git add -A && git commit -m "fix(circuit-lang): decouple resolves rails via provider and is round-trip model-stable"
```

---

### Task R6: Auto-no-connect materialization in the kernel (desugar.rs)

**Finding:** spec-conformance#0 (Important). **Decision:** materialize in kernel.

**Files:** Modify `crates/circuit-lang/src/desugar.rs` (new final pass); update any test expectations that now gain `nc` pins; tests in `mod tests`.

- [ ] **Step 1 — failing test:**

```rust
    #[test]
    fn unmentioned_non_power_pins_become_no_connect() {
        use crate::provider::{MockSymbolProvider, PinType};
        let mut p = MockSymbolProvider::with_basics();
        p.add("M:Chip", vec![
            ("1", "PA0", PinType::Other, 1), ("2", "PB6", PinType::Other, 1)]);
        let (s, _) = crate::parse::parse_str("
version: 1
blocks:
  main:
    components:
      U1: {part: M:Chip, pins: {PA0: SIG}}
");
        let (d, diags) = desugar(&s.unwrap(), &p);
        assert!(!diags.has_errors(), "{:?}", diags);
        let u1 = &d.blocks["main"].components["U1"];
        // unmentioned non-power pin PB6 (number "2") is auto-NC
        assert_eq!(u1.pins["2"], crate::model::PinTarget::NoConnect);
        // mentioned pin still on its net
        assert_eq!(u1.pins["PA0"], crate::model::PinTarget::Net("SIG".into()));
    }

    #[test]
    fn auto_nc_is_idempotent_through_canon() {
        use crate::provider::{MockSymbolProvider, PinType};
        let mut p = MockSymbolProvider::with_basics();
        p.add("M:Chip", vec![
            ("1", "PA0", PinType::Other, 1), ("2", "PB6", PinType::Other, 1)]);
        let (s1, _) = crate::parse::parse_str(
            "version: 1\nblocks: {main: {components: {U1: {part: M:Chip, pins: {PA0: SIG}}}}}");
        let (d1, _) = desugar(&s1.unwrap(), &p);
        let out1 = crate::canon::to_canonical_yaml(&d1);
        let (s2, _) = crate::parse::parse_str(&out1);
        let (d2, _) = desugar(&s2.unwrap(), &p);
        assert_eq!(d1, d2);
        assert_eq!(out1, crate::canon::to_canonical_yaml(&d2));
    }
```

- [ ] **Step 2 — run, confirm fail.**

- [ ] **Step 3 — implement.** Add a final desugar pass (after `resolve_pins` and `synth_decouple`) that, for each component whose symbol is known to the provider, enumerates the symbol's physical pins and for every pin NOT covered by an author key (resolve coverage number-first then name; a stacked name covers all its physical pins) AND whose `etype` is not `PowerInput`, inserts `PinTarget::NoConnect` keyed by the pin's number. Power-input pins are intentionally skipped (lint.rs already errors if they are unconnected). Explicit `nc` pins and net-mapped pins count as covered. Synthesized decouple caps (`Device:C`, known symbol, both pins mapped) are unaffected. Ensure determinism (insert in pin order).

- [ ] **Step 4 — run, confirm pass.** Existing tests that assert exact pin-map contents for KNOWN symbols may now legitimately gain `nc` entries — update those expectations. The **bluepill acceptance test** (`tests/bluepill.rs`) will gain auto-NC pins for the symbols whose providers list pins beyond those mentioned; update its assertions accordingly (it should still compile clean with no errors). Run `cargo test -p circuit-lang`.

- [ ] **Step 5 — fmt + commit:**
```bash
cargo fmt --all && git add -A && git commit -m "feat(circuit-lang): materialize auto no-connect for unmentioned pins in the kernel"
```

---

### Task R7: near-name declared-only nets + lint suppression `lint.allow` (lint.rs + language)

**Findings:** lints-canon-purity#1 (Minor — near-name skips declared-only nets), lints-canon-purity#2 (Minor — no suppression mechanism).

**Files:** Modify `crates/circuit-lang/src/lint.rs`, `parse.rs` (parse `lint:` section), `model.rs` (carry allow-set), `canon.rs` (emit `lint:` if present), `surface.rs` if needed; tests in `lint.rs mod tests`.

- [ ] **Step 1 — failing tests:**

```rust
    #[test]
    fn near_name_compares_declared_only_nets() {
        let diags = run("
version: 1
blocks:
  main:
    components:
      R1: {part: R, pins: {1: I2C_SDA, 2: GND}}
nets:
  I2C1_SDA: {class: x}
");
        assert!(diags.0.iter().any(|d| d.code == "near-name"),
            "declared-only net one edit away must warn");
    }

    #[test]
    fn lint_allow_suppresses_codes() {
        let diags = run("
version: 1
lint: {allow: [single-pin-net, near-name, unreferenced-net]}
blocks:
  main:
    components:
      R1: {part: R, pins: {1: I2C_SDA, 2: GND}}
      TP1: {part: R, pins: {1: PROBE_ONLY, 2: PROBE_ONLY}}
nets:
  I2C1_SDA: {class: x}
");
        assert!(!diags.0.iter().any(|d| d.code == "single-pin-net"));
        assert!(!diags.0.iter().any(|d| d.code == "near-name"));
        assert!(!diags.0.iter().any(|d| d.code == "unreferenced-net"));
    }
```

- [ ] **Step 2 — run, confirm fail.**

- [ ] **Step 3 — implement.**
  - near-name: build the candidate set from the **dedup union** of `net_pins.keys()` and `d.nets.keys()` so declared-only nets participate.
  - Language `lint.allow`: add an optional top-level `lint:` mapping with a single key `allow:` (a list of lint-code strings). Parse it in `parse.rs` with strict unknown-key checking (`lint` → only `allow`); carry it into the kernel `Design` (e.g. `pub lint_allow: Vec<String>` or `BTreeSet<String>` on `Design`). In `lint()`, before pushing any of `single-pin-net` / `near-name` / `unreferenced-net`, skip if its code is in the allow-set. Emit `lint:` back in `canon.rs` (deterministic, sorted) only when non-empty, so it round-trips. Update the top-level allowed-keys list in `parse.rs` to include `lint`.

- [ ] **Step 4 — run, confirm pass** (including the canonical round-trip tests, which must still be fixpoints with a `lint:` section present).

- [ ] **Step 5 — fmt + commit:**
```bash
cargo fmt --all && git add -A && git commit -m "feat(circuit-lang): near-name covers declared nets; add lint.allow suppression"
```

---

### Task R8: Cleanup — remove unused `thiserror`, document `q()` invariant (Cargo.toml, canon.rs)

**Findings:** lints-canon-purity#3 (Nit — unused dep), lints-canon-purity#4 (Nit — `q()` colon comment).

**Files:** Modify `crates/circuit-lang/Cargo.toml`, `crates/circuit-lang/src/canon.rs`.

- [ ] **Step 1 — implement** (no test; verified by build + clippy):
  - Remove the `thiserror.workspace = true` line from `crates/circuit-lang/Cargo.toml` (leave it in the workspace `[workspace.dependencies]` for later crates). Confirm nothing in `crates/circuit-lang/src` references `thiserror`.
  - Add a comment above `q()` in `canon.rs`: note that leaving `':'` unquoted is valid only because emission is always flow-style (`{…}`); revisit if block-style mapping values are ever introduced.

- [ ] **Step 2 — verify:** `cargo build -p circuit-lang && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p circuit-lang` all green.

- [ ] **Step 3 — fmt + commit:**
```bash
cargo fmt --all && git add -A && git commit -m "chore(circuit-lang): drop unused thiserror dep; document q() flow-style invariant"
```

---

### Task R9: Spec doc updates (reflect the remediation decisions)

**Files:** Modify `docs/superpowers/specs/2026-06-09-kicad-copilot-agent-design.md`.

- [ ] **Step 1 — edit the spec** to record the binding decisions (no code, doc only):
  - §5.2: add the optional top-level `lint: {allow: [<code>...]}` section to the kernel grammar block, with a one-line description.
  - §5.3.5: clarify that auto-no-connect markers ARE materialized in the circuit-lang kernel (unmentioned non-power pins of a known symbol become `nc`), and note the consequence that canonical YAML lists them.
  - §5.3.8: note net-name casing is enforced as a **warning** (`net-name-case`), while refdes/blocks remain hard errors; refdes is strictly `[A-Z]+[0-9]+`.
  - §6 / parsing notes: single YAML document only (multiple documents are a hard error); duplicate map keys are a hard error.
  - §5.3.9: note the lints are suppressible via `lint.allow`.

- [ ] **Step 2 — verify** the spec reads consistently (no contradictions with §5.5/§5.6).

- [ ] **Step 3 — commit:**
```bash
git add docs/superpowers/specs/2026-06-09-kicad-copilot-agent-design.md && git commit -m "docs: record circuit-lang remediation decisions (kernel auto-NC, lint.allow, strict naming)"
```

---

## Definition of done

- `cargo test -p circuit-lang` green (all new regression tests + updated bluepill).
- `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo fmt --check` clean.
- All 12 confirmed findings resolved; auto-NC materialized; spec updated.
- `circuit-lang` still pure (deps: saphyr, saphyr-parser, indexmap, strsim — no thiserror, no I/O).
