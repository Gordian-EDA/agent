//! Fast cross-library symbol search — backs the agent's `search_symbols`
//! anti-hallucination tool.
//!
//! [`SymbolIndex::build`] scans symbol *names only* across every
//! `*.kicad_sym` in the environment's symbol directory. Libraries are not
//! parsed into ASTs at build time; instead the raw s-expression text is
//! walked once per file, tracking paren depth (string-literal aware), and
//! `(symbol "NAME"` blocks at depth 1 — direct children of
//! `(kicad_symbol_lib` — are recorded. Sub-unit blocks (`NAME_0_1` etc.)
//! sit one level deeper and are skipped by construction.
//!
//! Pin counts are resolved lazily: only the symbols actually returned by
//! [`SymbolIndex::search`] are parsed, via [`RealSymbolProvider`].
//!
//! Ranking uses `fuzzy-matcher`'s `SkimMatcherV2` (fzf-style subsequence
//! scoring) — the project's standard fuzzy matcher; reuse it rather than adding
//! another. `strsim` edit distance backs only the typo fallback in `rank`.

use std::fs;
use std::io;

use crate::SymbolProvider;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use kicad_cli::env::KicadEnv;
use crate::provider::RealSymbolProvider;

/// A search hit: a fully qualified `Lib:Name` id and its resolved pin count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub lib_id: String,
    pub pin_count: usize,
}

/// One indexed symbol: pre-normalized for ranking.
struct Entry {
    lib_id: String,
    /// Lowercased name with non-alphanumeric runs collapsed to single spaces.
    normalized: String,
}

/// Name index over every symbol in every installed library.
pub struct SymbolIndex {
    entries: Vec<Entry>,
    provider: RealSymbolProvider,
}

impl SymbolIndex {
    /// Scan all `*.kicad_sym` files under the environment's symbol directory
    /// and index their top-level symbol names. Names only — no AST parsing.
    pub fn build(env: &KicadEnv) -> io::Result<SymbolIndex> {
        let mut entries = Vec::new();
        let mut lib_paths: Vec<_> = fs::read_dir(&env.symbol_dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "kicad_sym"))
            .collect();
        lib_paths.sort(); // deterministic order, stable tie-breaks

        for path in lib_paths {
            let Some(lib) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // A single unreadable lib must not take down the whole index.
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            for name in top_level_symbol_names(&text) {
                let lib_id = format!("{lib}:{name}");
                entries.push(Entry {
                    normalized: normalize(&lib_id),
                    lib_id,
                });
            }
        }

        Ok(SymbolIndex {
            entries,
            provider: RealSymbolProvider::new(env.clone()),
        })
    }

    /// Number of indexed symbols.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return the `n` best matches for `query`.
    ///
    /// Ranking is fzf-style subsequence scoring (see `rank`). Pin counts are
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
}

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

/// Lowercase and collapse runs of non-alphanumeric characters into single
/// spaces, so `USB_C_Receptacle_USB2.0` and `usb-c receptacle usb2` compare
/// on equal footing.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with(' ') && !out.is_empty() {
            out.push(' ');
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Extract the names of `(symbol "NAME"` blocks that are direct children of
/// the top-level `(kicad_symbol_lib` form, by tracking paren depth across the
/// raw text. String literals (with `\"` escapes) are skipped, so quotes and
/// parens inside names or property values cannot desync the depth counter.
/// Sub-unit blocks (`NAME_0_1` …) live at depth 2 and are never reported.
fn top_level_symbol_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut depth = 0u32;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                // Skip the string literal, honoring backslash escapes.
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                continue;
            }
            b'(' => {
                depth += 1;
                if depth == 2
                    && let Some(name) = symbol_block_name(&text[i + 1..])
                {
                    names.push(name.to_string());
                }
            }
            b')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    names
}

/// Given the text immediately after a `(`, return the quoted name if the
/// block is `symbol "NAME"`.
fn symbol_block_name(rest: &str) -> Option<&str> {
    let rest = rest.strip_prefix("symbol")?;
    let rest = rest.trim_start();
    // Require the quote so `symbols`/`symbolic` tokens don't match: the
    // strip above leaves a non-empty residue for those, which then fails
    // either the trim (no whitespace consumed) or this prefix check.
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

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
            entries[ranked[0]].lib_id, "MCU_ST_STM32H7:STM32H743VITx",
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
        let entries = vec![entry("Connector_Audio:AudioJack3"), entry("Device:R")];
        // "deivce" transposes "device"; the 'v' before 'i' breaks the
        // subsequence, so SkimMatcherV2 finds nothing and backfill by edit
        // distance must still return the closest candidate.
        let ranked = rank(&entries, &normalize("deivce"), 1);
        assert_eq!(ranked.len(), 1, "backfill must guarantee n results");
        assert_eq!(entries[ranked[0]].lib_id, "Device:R");
    }

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

    #[test]
    fn normalize_treats_non_alnum_as_separators() {
        assert_eq!(
            normalize("USB_C_Receptacle_USB2.0_16P"),
            "usb c receptacle usb2 0 16p"
        );
        assert_eq!(normalize("usb-c receptacle usb2"), "usb c receptacle usb2");
        assert_eq!(normalize("--R_Small--"), "r small");
    }

    #[test]
    fn scanner_reports_only_depth_one_symbols() {
        let text = r#"(kicad_symbol_lib (generator "x(y) \" (symbol \"Fake\"")
  (symbol "A" (symbol "A_0_1") (symbol "A_1_1"))
  (symbols "NotASymbolBlock")
  (symbol "B"))
"#;
        assert_eq!(top_level_symbol_names(text), ["A", "B"]);
    }
}
