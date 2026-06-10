# Fuzzy-matcher swap for `SymbolIndex::search`

**Date:** 2026-06-10
**Status:** Approved, pending implementation

## Problem

The agent's `search_symbols` anti-hallucination tool sometimes fails to surface
the right `Lib:Name` even for queries that should match cleanly. Two root causes
in `crates/kicad-bridge/src/search.rs`:

1. **Wrong ranking metric.** Fuzzy ranking uses
   `1.0 - strsim::normalized_levenshtein(needle, name)` (`search.rs:105`), which
   measures *global similarity between two whole strings*. It is dominated by
   length mismatch (a short query against a long part name is penalized for every
   extra character) and has no anchoring or consecutive/word-boundary bonus. A
   `contains`-based tier byte is the only thing keeping exact substrings on top;
   the moment a query isn't a contiguous substring, the length-dominated metric
   takes over.
2. **The library name isn't searchable.** `build` indexes
   `normalized: normalize(&name)` — the bare symbol name only. Qualified queries
   like `Device:R` or `MCU_ST_STM32H7 STM32H743` carry library text that can't
   match anything and drags the score down.

## Decision

Swap the ranking metric to **`fuzzy-matcher`'s `SkimMatcherV2`** (an fzf-style
subsequence scorer with consecutive/word-boundary bonuses), kept in-process. We
do *not* shell out to the `fzf` binary — it's an interactive TUI, and shelling
out per query to stream tens of thousands of candidates is the wrong shape.

Scope, decided with the user:
- **Core:** replace the metric with `SkimMatcherV2`.
- **Index the lib name** so qualified queries match.
- **Levenshtein backfill** so typos/transpositions (not a subsequence of
  anything) still return candidates.
- *Out of scope:* a "hit-count beyond limit" hint in the tool response.

The `strsim` "did-you-mean" suggestions in `provider.rs`, `lint.rs`, and
`parse.rs` are a different concern and are left untouched. `strsim` stays a
workspace dependency (used by the backfill and elsewhere).

## Changes

**Dependency.** Add `fuzzy-matcher = "0.3"` to `[workspace.dependencies]` and to
`crates/kicad-bridge/Cargo.toml`.

**Index the lib name** (`build`, ~`search.rs:64`). Normalize the full `Lib:Name`
into the searchable field:

```rust
let lib_id = format!("{lib}:{name}");
entries.push(Entry { normalized: normalize(&lib_id), lib_id });
```

So `MCU_ST_STM32H7:STM32H743VITx` → `"mcu st stm32h7 stm32h743vitx"`. Bare-name
queries still subsequence-match inside it; qualified queries now match too.

**Extract a pure `rank` function** — separating ranking (pure, testable) from pin
resolution (I/O). This is the test seam.

```rust
/// Pure ranking: indices into `entries`, best first. No I/O.
fn rank(entries: &[Entry], needle: &str, n: usize) -> Vec<usize>
```

`search` becomes: `normalize(query) → rank → resolve pin counts lazily for the
chosen indices only` (preserving today's lazy pin-count behavior).

**Ranking inside `rank`:**
- Primary: `SkimMatcherV2::default().fuzzy_match(&entry.normalized, needle)` →
  `Option<i64>`. Keep `Some(score)` hits, sort by **score descending**,
  tie-broken by shorter `normalized` then `lib_id` lexicographically (so order is
  deterministic — `SkimMatcherV2` scores are deterministic).
- Backfill: if fewer than `n` fuzzy hits, fill the remainder from entries *not
  already chosen*, ranked by `strsim::normalized_levenshtein`. Guarantees the
  agent never gets a short/empty list on a typo.

**`normalize()` is unchanged.** Collapsing punctuation to single spaces gives
`SkimMatcherV2` uniform word boundaries (it bonuses the char after a separator)
and keeps `usb-c receptacle` matching `USB_C_Receptacle`. `top_level_symbol_names`
and `symbol_block_name` are unchanged.

**Documentation deliverables** (part of this change's commit):
- Create a root `CLAUDE.md` with a "Conventions" section recording the
  fuzzy-search-engine rule below.
- Add a one-line note to the `search.rs` module doc pointing at `SkimMatcherV2`
  as the project's fuzzy matcher.

## Behavior change to expect

- Clean fragments rank much better (consecutive/boundary bonuses).
- Qualified `Lib:Name` queries work.
- Typos still return candidates via levenshtein backfill.
- Deterministic ordering and lazy pin-count resolution are preserved.

## Convention for future code agents

This is the project's fuzzy-search engine. **Future changes that need fuzzy /
approximate string matching (ranking, search, autocomplete) should reuse
`fuzzy-matcher`'s `SkimMatcherV2` rather than hand-rolling another matcher or
adding a second fuzzy-search dependency.** `strsim` (edit distance) remains
appropriate for "did-you-mean" single-best-suggestion use, not for ranking a
candidate set. This convention will be recorded in a root `CLAUDE.md` and in the
`search.rs` module doc as part of this change.

## Tests (TDD, in-module so they can construct `Entry` directly)

1. A fragment query ranks the precise long part first.
2. A qualified `Device:R` query matches via the indexed lib name.
3. A transposition typo still returns `n` results via backfill, closest first.
4. An empty/whitespace query returns nothing.

Existing tests (`normalize_treats_non_alnum_as_separators`,
`scanner_reports_only_depth_one_symbols`) and the `tools.rs` integration test
must still pass.
