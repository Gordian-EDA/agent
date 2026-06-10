# Fuzzy-matcher Swap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `SymbolIndex::search`'s length-dominated levenshtein ranking with `fuzzy-matcher`'s fzf-style `SkimMatcherV2`, index the library name, and backfill typos by edit distance — so the agent's `search_symbols` reliably surfaces the right `Lib:Name`.

**Architecture:** Ranking is extracted into a pure `rank(entries, needle, n) -> Vec<usize>` function (testable without I/O); `search` becomes `normalize → rank → resolve pin counts lazily`. Primary ranking is `SkimMatcherV2` subsequence scoring; when fewer than `n` candidates match as a subsequence, the remainder is backfilled by `strsim::normalized_levenshtein`. `build` now normalizes the full `Lib:Name` into the searchable field.

**Tech Stack:** Rust (edition 2024), `fuzzy-matcher` 0.3 (`SkimMatcherV2`), `strsim` (backfill only), `tempfile` (tests).

**Legacy removal:** The `(tier, distance, index)` tuple ranking is deleted outright — no flag, no fallback path kept. `strsim` is retained only in its new backfill role and in the unrelated "did-you-mean" suggesters.

**Note on cargo:** Per project convention, never run `cargo` concurrently with another agent that also runs `cargo`. Execute these tasks' cargo commands serially.

Spec: `docs/superpowers/specs/2026-06-10-fuzzy-matcher-swap-design.md`

---

## File Structure

- `Cargo.toml` (workspace) — add `fuzzy-matcher` to `[workspace.dependencies]`.
- `crates/kicad-bridge/Cargo.toml` — depend on `fuzzy-matcher.workspace = true`.
- `crates/kicad-bridge/src/search.rs` — index full lib_id in `build`; new pure `rank`; rewrite `search`; module-doc note; new tests.
- `CLAUDE.md` (new, repo root) — record the fuzzy-search-engine convention for future agents.

---

## Task 1: Add the `fuzzy-matcher` dependency

**Files:**
- Modify: `Cargo.toml` (workspace `[workspace.dependencies]`)
- Modify: `crates/kicad-bridge/Cargo.toml`

- [ ] **Step 1: Add to workspace dependencies**

In `Cargo.toml`, add this line to `[workspace.dependencies]` (alphabetically near `futures`/`indexmap` is fine; exact position doesn't matter):

```toml
fuzzy-matcher = "0.3"
```

- [ ] **Step 2: Depend on it from kicad-bridge**

In `crates/kicad-bridge/Cargo.toml`, under `[dependencies]`, add after the `strsim.workspace = true` line:

```toml
fuzzy-matcher.workspace = true
```

- [ ] **Step 3: Verify it resolves and compiles**

Run: `cargo build -p kicad-bridge`
Expected: builds successfully; `fuzzy-matcher v0.3.x` appears in the dependency resolution on first fetch. No code uses it yet, so no warnings about it.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock crates/kicad-bridge/Cargo.toml
git commit -m "build(kicad-bridge): add fuzzy-matcher dependency"
```

---

## Task 2: Replace ranking with a pure `rank` (fuzzy + backfill)

**Files:**
- Modify: `crates/kicad-bridge/src/search.rs` (imports; `search` body ~lines 92-123; add `rank`; tests)

This task adds `rank`, rewrites `search` to call it, and deletes the old tuple ranking. Tests construct `Entry` directly via a helper, so they need no filesystem.

- [ ] **Step 1: Write the failing unit tests**

Add this helper and these tests inside the existing `#[cfg(test)] mod tests { ... }` block in `search.rs` (which already has `use super::*;`):

```rust
    /// Build an `Entry` the way `build` does: normalized over the full lib_id.
    fn entry(lib_id: &str) -> Entry {
        Entry {
            normalized: normalize(lib_id),
            lib_id: lib_id.to_string(),
        }
    }

    #[test]
    fn fuzzy_ranks_fragment_match_first() {
        let entries = vec![
            entry("Device:R"),
            entry("MCU_ST_STM32F1:STM32F103C8Tx"),
            entry("MCU_ST_STM32H7:STM32H743VITx"),
        ];
        let ranked = rank(&entries, &normalize("stm32h743"), 3);
        assert_eq!(
            entries[ranked[0]].lib_id,
            "MCU_ST_STM32H7:STM32H743VITx",
            "the precise part should rank first for a clean fragment"
        );
    }

    #[test]
    fn qualified_query_matches_via_lib_name() {
        let entries = vec![
            entry("Device:C"),
            entry("Connector:Conn_01x02"),
            entry("Device:R"),
        ];
        let ranked = rank(&entries, &normalize("Device:R"), 3);
        assert_eq!(entries[ranked[0]].lib_id, "Device:R");
    }

    #[test]
    fn typo_returns_closest_via_backfill() {
        let entries = vec![
            entry("Connector_Audio:AudioJack3"),
            entry("Device:R"),
        ];
        // "deivce" transposes "device"; the 'v' before 'i' breaks the
        // subsequence, so SkimMatcherV2 finds nothing and backfill by edit
        // distance must still return the closest candidate.
        let ranked = rank(&entries, &normalize("deivce"), 1);
        assert_eq!(ranked.len(), 1, "backfill must guarantee n results");
        assert_eq!(entries[ranked[0]].lib_id, "Device:R");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p kicad-bridge rank 2>&1 | head -40` (and `fuzzy`, `qualified`, `typo`)
Expected: FAIL — compile error, `cannot find function `rank` in this scope`.

- [ ] **Step 3: Add imports**

At the top of `search.rs`, below the existing `use circuit_lang::SymbolProvider;` line, add:

```rust
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
```

- [ ] **Step 4: Replace the body of `search` and add `rank`**

Replace the entire current `search` method body (the `(tier, distance, index)` block, lines ~92-123) so the method reads exactly:

```rust
    /// Return the `n` best matches for `query`.
    ///
    /// Ranking is fzf-style subsequence scoring (see [`rank`]). Pin counts are
    /// resolved lazily, for the returned hits only.
    pub fn search(&self, query: &str, n: usize) -> Vec<Hit> {
        let needle = normalize(query);
        if needle.is_empty() {
            return Vec::new();
        }

        rank(&self.entries, &needle, n)
            .into_iter()
            .map(|i| {
                let lib_id = self.entries[i].lib_id.clone();
                let pin_count = self
                    .provider
                    .symbol(&lib_id)
                    .map_or(0, |meta| meta.pins.len());
                Hit { lib_id, pin_count }
            })
            .collect()
    }
```

Then add this free function immediately after the `impl SymbolIndex { ... }` block (before `fn normalize`):

```rust
/// Rank `entries` against an already-normalized `needle`, returning the indices
/// of the best `n`, best first.
///
/// Primary ranking is fzf-style subsequence scoring via [`SkimMatcherV2`]
/// (higher score = better; candidates the needle is not a subsequence of score
/// `None` and drop out). When fewer than `n` candidates match as a subsequence
/// — e.g. the query has a transposition — the remainder is backfilled by edit
/// distance, so the caller is never starved of candidates. Ordering is
/// deterministic: fuzzy ties break on shorter normalized text then `lib_id`;
/// backfill ties break on `lib_id`.
fn rank(entries: &[Entry], needle: &str, n: usize) -> Vec<usize> {
    let matcher = SkimMatcherV2::default();

    let mut fuzzy: Vec<(i64, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| matcher.fuzzy_match(&e.normalized, needle).map(|s| (s, i)))
        .collect();
    fuzzy.sort_by(|&(sa, ia), &(sb, ib)| {
        sb.cmp(&sa)
            .then_with(|| entries[ia].normalized.len().cmp(&entries[ib].normalized.len()))
            .then_with(|| entries[ia].lib_id.cmp(&entries[ib].lib_id))
    });

    let mut chosen: Vec<usize> = fuzzy.into_iter().take(n).map(|(_, i)| i).collect();
    if chosen.len() >= n {
        return chosen;
    }

    // Backfill: never starve the agent of candidates on a typo / non-subsequence.
    let taken: std::collections::HashSet<usize> = chosen.iter().copied().collect();
    let mut rest: Vec<(f64, usize)> = entries
        .iter()
        .enumerate()
        .filter(|(i, _)| !taken.contains(i))
        .map(|(i, e)| (1.0 - strsim::normalized_levenshtein(needle, &e.normalized), i))
        .collect();
    rest.sort_by(|&(da, ia), &(db, ib)| {
        da.partial_cmp(&db)
            .expect("distances are finite")
            .then_with(|| entries[ia].lib_id.cmp(&entries[ib].lib_id))
    });
    chosen.extend(rest.into_iter().take(n - chosen.len()).map(|(_, i)| i));
    chosen
}
```

Confirm the old `(tier, distance, index)` code is fully gone — no `tier`, no `ranked` tuple vector remains.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p kicad-bridge 2>&1 | tail -25`
Expected: PASS — the three new tests plus the existing `normalize_*` and `scanner_*` tests all green.

- [ ] **Step 6: Commit**

```bash
git add crates/kicad-bridge/src/search.rs
git commit -m "feat(kicad-bridge): rank symbol search with fuzzy-matcher + levenshtein backfill"
```

---

## Task 3: Index the full lib_id in `build`

**Files:**
- Modify: `crates/kicad-bridge/src/search.rs` (`build` loop ~lines 63-68; tests)

- [ ] **Step 1: Write the failing tests**

Add inside the same `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn build_indexes_full_lib_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("Device.kicad_sym"),
            "(kicad_symbol_lib (symbol \"R\"))",
        )
        .expect("write lib");
        let index =
            SymbolIndex::build(&KicadEnv::with_symbol_dir(dir.path().to_path_buf())).expect("build");

        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].lib_id, "Device:R");
        assert_eq!(
            index.entries[0].normalized, "device r",
            "the library name must be part of the searchable field"
        );
    }

    #[test]
    fn empty_normalized_query_returns_no_hits() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("Device.kicad_sym"),
            "(kicad_symbol_lib (symbol \"R\"))",
        )
        .expect("write lib");
        let index =
            SymbolIndex::build(&KicadEnv::with_symbol_dir(dir.path().to_path_buf())).expect("build");

        assert!(
            index.search("@@@", 5).is_empty(),
            "a query that normalizes to empty must yield no hits"
        );
    }
```

`KicadEnv` is already imported at the top of `search.rs` (`use crate::env::KicadEnv;`) and is in scope via `use super::*;`. `tempfile` is a regular dependency of the crate.

- [ ] **Step 2: Run to verify the indexing test fails**

Run: `cargo test -p kicad-bridge build_indexes_full_lib_id 2>&1 | tail -20`
Expected: FAIL — assertion `index.entries[0].normalized == "device r"` fails (current code stores `"r"`, the bare name). `empty_normalized_query_returns_no_hits` already passes (guards preserved behavior).

- [ ] **Step 3: Normalize the full lib_id in `build`**

In `build`, replace the inner push loop:

```rust
            for name in top_level_symbol_names(&text) {
                entries.push(Entry {
                    normalized: normalize(&name),
                    lib_id: format!("{lib}:{name}"),
                });
            }
```

with:

```rust
            for name in top_level_symbol_names(&text) {
                let lib_id = format!("{lib}:{name}");
                entries.push(Entry {
                    normalized: normalize(&lib_id),
                    lib_id,
                });
            }
```

- [ ] **Step 4: Run to verify the tests pass**

Run: `cargo test -p kicad-bridge 2>&1 | tail -25`
Expected: PASS — all `search.rs` tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/kicad-bridge/src/search.rs
git commit -m "feat(kicad-bridge): index full Lib:Name so qualified queries match"
```

---

## Task 4: Record the convention for future agents

**Files:**
- Modify: `crates/kicad-bridge/src/search.rs` (module doc, top of file)
- Create: `CLAUDE.md` (repo root)

- [ ] **Step 1: Add a module-doc pointer**

In `search.rs`, append these lines to the module doc comment, right after the existing line `//! Pin counts are resolved lazily: only the symbols actually returned by` / `//! [`SymbolIndex::search`] are parsed, via [`RealSymbolProvider`].`:

```rust
//!
//! Ranking uses `fuzzy-matcher`'s `SkimMatcherV2` (fzf-style subsequence
//! scoring) — the project's standard fuzzy matcher; reuse it rather than adding
//! another. `strsim` edit distance backs only the typo fallback in `rank`.
```

- [ ] **Step 2: Create `CLAUDE.md`**

Create `CLAUDE.md` at the repo root with exactly:

```markdown
# auto-pcb

A Rust workspace for an LLM agent that designs KiCAD schematics.

## Conventions

### Fuzzy / approximate string matching

Use `fuzzy-matcher`'s `SkimMatcherV2` (fzf-style subsequence scoring) for any
fuzzy search, ranking, or autocomplete over a candidate set. The reference
implementation is `SymbolIndex::search` in `crates/kicad-bridge/src/search.rs`.
Reuse it — do **not** hand-roll another fuzzy matcher or add a second
fuzzy-search dependency.

`strsim` (edit distance) is appropriate only for "did-you-mean" single-best
suggestions (e.g. `provider.rs`, `lint.rs`, `parse.rs`), not for ranking a list.
```

- [ ] **Step 3: Verify the crate still builds (doc comment is valid)**

Run: `cargo build -p kicad-bridge`
Expected: builds successfully.

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md crates/kicad-bridge/src/search.rs
git commit -m "docs: record fuzzy-matcher as the project's fuzzy-search engine"
```

---

## Task 5: Full verification

**Files:** none (verification only)

- [ ] **Step 1: Run the full test suite**

Run: `cargo test -p kicad-bridge 2>&1 | tail -30`
Expected: all tests pass, including the pre-existing `search_symbols_tool_finds_stm32` integration test in `crates/agent` if you widen to `cargo test` — run `cargo test -p agent search_symbols 2>&1 | tail -20` to confirm the tool layer still finds the STM32 (this exercises the real installed libraries end-to-end).

- [ ] **Step 2: Lint**

Run: `cargo clippy -p kicad-bridge --all-targets 2>&1 | tail -20`
Expected: no new warnings introduced by the changed code.

- [ ] **Step 3: Confirm no legacy ranking remains**

Run: `grep -n "tier" crates/kicad-bridge/src/search.rs`
Expected: no output (the old tiered ranking is gone).

- [ ] **Step 4: Final commit (only if Step 2 surfaced fixable nits)**

```bash
git add -A
git commit -m "chore(kicad-bridge): clippy cleanups for fuzzy search"
```

---

## Self-Review

- **Spec coverage:** dependency (Task 1), `SkimMatcherV2` ranking + backfill (Task 2), lib-name indexing (Task 3), `normalize` unchanged (untouched), convention in CLAUDE.md + module doc (Task 4), all four spec test scenarios — fragment-first (T2), qualified (T2), typo-backfill (T2), empty query (T3) — plus the indexing test (T3) and the existing integration test re-run (T5). Covered.
- **Backfill caveat (honest note):** `qualified_query_matches_via_lib_name` and `typo_returns_closest_via_backfill` assert *behavior* through `rank`; on tiny entry sets the backfill can mask whether lib-indexing specifically fired, which is why `build_indexes_full_lib_id` (T3) asserts the normalized field directly. The two together give real coverage.
- **Type consistency:** `rank(&[Entry], &str, usize) -> Vec<usize>` is defined in T2 and only consumed by `search`; `Entry { normalized, lib_id }` fields match their use in the `entry` helper and `build`. `SkimMatcherV2::default()` + `FuzzyMatcher::fuzzy_match(choice, pattern) -> Option<i64>` is the API used.
- **Placeholder scan:** none.
