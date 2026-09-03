//! Fast cross-library symbol search — backs the agent's `search_symbols`
//! anti-hallucination tool.
//!
//! [`SymbolIndex::build`] scans symbol *names only* across every flat
//! `*.kicad_sym` or KiCad 10 split `*.kicad_symdir` library. Libraries are not
//! parsed into ASTs at build time; instead the raw s-expression text is
//! walked once per file, tracking paren depth (string-literal aware), and
//! `(symbol "NAME"` blocks at depth 1 — direct children of
//! `(kicad_symbol_lib` — are recorded. Sub-unit blocks (`NAME_0_1` etc.)
//! sit one level deeper and are skipped by construction.
//!
//! Pin counts are resolved lazily: only the symbols actually returned by
//! [`SymbolIndex::search`] are parsed, via [`SymbolTable`].
//!
//! Ranking uses `fuzzy-matcher`'s `SkimMatcherV2` (fzf-style subsequence
//! scoring) — the project's standard fuzzy matcher; reuse it rather than adding
//! another. `strsim` edit distance backs only the typo fallback in `rank`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use crate::{SymbolMeta, SymbolTable};

/// A search hit: a fully qualified `Lib:Name` id and its resolved pin count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub lib_id: String,
    pub pin_count: usize,
}

/// One indexed symbol: pre-normalized for ranking.
struct Entry {
    lib_id: String,
    /// Lowercased `Lib:Name` with non-alphanumeric runs collapsed to single spaces.
    normalized: String,
    /// The same treatment of the `Name` half alone.
    normalized_name: String,
}

/// Name index over every symbol in every installed library.
pub struct SymbolIndex {
    names: SymbolNames,
    table: SymbolTable,
}

/// Every `Lib:Name` in every installed library, with nothing else — the ranking
/// substrate [`SymbolIndex::search`] serves the agent from and
/// [`SymbolTable::suggest`](crate::SymbolTable::suggest) reaches for when a lib_id
/// names a library that does not exist.
pub struct SymbolNames {
    entries: Vec<Entry>,
}

impl SymbolNames {
    /// Scan all KiCad symbol libraries under `symbol_dir` and index their top-level
    /// symbol names. Names only — no AST parsing.
    pub fn scan(symbol_dir: &Path) -> io::Result<SymbolNames> {
        let mut entries = Vec::new();
        for lib in discover_libraries(symbol_dir)? {
            for path in lib.symbol_files() {
                // A single unreadable file must not take down the whole index.
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                for name in top_level_symbol_names(&text) {
                    entries.push(Entry::new(&format!("{}:{name}", lib.name)));
                }
            }
        }
        Ok(SymbolNames { entries })
    }

    /// An index over no libraries — what an unreadable symbol directory yields.
    pub fn empty() -> SymbolNames {
        SymbolNames {
            entries: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The `n` closest lib_ids to a *qualified* one, best first.
    ///
    /// A wrong lib_id is nearly always the right symbol NAME under the wrong
    /// library — `Device:Conn_01x02` for `Connector_Generic:Conn_01x02`,
    /// `Regulator_Switching:TPS62160` for its `TPS62160DGK` variant — so the name
    /// half is what ranks, across every library, and the library half only breaks
    /// ties. Ranking the whole `Lib:Name` instead scores those `None`: the wrong
    /// library's letters are not a subsequence of the right one's.
    ///
    /// An unqualified query has no name half to isolate and falls back to [`best`].
    ///
    /// [`best`]: Self::best
    pub fn best_lib_id(&self, lib_id: &str, n: usize) -> Vec<&str> {
        let Some((library, name)) = lib_id.split_once(':') else {
            return self.best(lib_id, n);
        };
        let (needle, library) = (normalize(name), normalize(library));
        if needle.is_empty() {
            return Vec::new();
        }
        let matcher = SkimMatcherV2::default();
        let tokens: Vec<&str> = needle.split_whitespace().collect();
        let mut hits: Vec<(i64, i64, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let name = score(&matcher, &entry.normalized_name, &needle, &tokens)?;
                let lib = matcher
                    .fuzzy_match(&entry.normalized, &library)
                    .unwrap_or_default();
                Some((name, lib, index))
            })
            .collect();
        hits.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| right.1.cmp(&left.1))
                .then_with(|| self.entries[left.2].lib_id.cmp(&self.entries[right.2].lib_id))
        });
        hits.into_iter()
            .take(n)
            .map(|(_, _, index)| self.entries[index].lib_id.as_str())
            .collect()
    }

    /// The `n` best-matching lib_ids for `query`, best first.
    pub fn best(&self, query: &str, n: usize) -> Vec<&str> {
        let needle = normalize(query);
        if needle.is_empty() {
            return Vec::new();
        }
        rank(&self.entries, &needle, n)
            .into_iter()
            .map(|i| self.entries[i].lib_id.as_str())
            .collect()
    }
}

impl Entry {
    fn new(lib_id: &str) -> Entry {
        let name = lib_id.rsplit(':').next().unwrap_or(lib_id);
        Entry {
            normalized: normalize(lib_id),
            normalized_name: normalize(name),
            lib_id: lib_id.to_string(),
        }
    }
}

impl SymbolIndex {
    /// Index `symbol_dir`'s names and pair them with a table that resolves them.
    pub fn build(symbol_dir: &Path) -> io::Result<SymbolIndex> {
        Ok(SymbolIndex {
            names: SymbolNames::scan(symbol_dir)?,
            table: SymbolTable::from_symbol_dir(symbol_dir.to_path_buf()),
        })
    }

    /// Number of indexed symbols.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Return the `n` best matches for `query`.
    ///
    /// Ranking is fzf-style subsequence scoring (see `rank`). Pin counts are
    /// resolved lazily, for the returned hits only.
    pub fn search(&self, query: &str, n: usize) -> Vec<Hit> {
        self.names
            .best(query, n.saturating_mul(4))
            .into_iter()
            .filter_map(|lib_id| {
                let pin_count = self.table.symbol(lib_id)?.pins.len();
                Some(Hit {
                    lib_id: lib_id.to_string(),
                    pin_count,
                })
            })
            .take(n)
            .collect()
    }

    /// Resolve metadata through the same table that validates search hits.
    pub fn symbol(&self, lib_id: &str) -> Option<SymbolMeta> {
        self.table.symbol(lib_id)
    }
}

struct SymbolLibrary {
    name: String,
    path: PathBuf,
    split: bool,
}

impl SymbolLibrary {
    fn symbol_files(&self) -> Vec<PathBuf> {
        if !self.split {
            return vec![self.path.clone()];
        }
        let Ok(entries) = fs::read_dir(&self.path) else {
            return Vec::new();
        };
        let mut paths: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "kicad_sym"))
            .collect();
        paths.sort();
        paths
    }
}

fn discover_libraries(symbol_dir: &Path) -> io::Result<Vec<SymbolLibrary>> {
    let mut libs: Vec<_> = fs::read_dir(symbol_dir)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "kicad_sym") {
                let name = path.file_stem()?.to_str()?.to_string();
                return Some(SymbolLibrary {
                    name,
                    path,
                    split: false,
                });
            }
            if path.extension().is_some_and(|ext| ext == "kicad_symdir") && path.is_dir() {
                let name = path.file_stem()?.to_str()?.to_string();
                return Some(SymbolLibrary {
                    name,
                    path,
                    split: true,
                });
            }
            None
        })
        .collect();
    libs.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    Ok(libs)
}


/// Share of a multi-word query's words a candidate must match to qualify.
const MIN_TOKEN_COVERAGE: f64 = 0.5;

/// How well `candidate` answers `needle`, or `None` if it does not.
///
/// Skim scores a needle only when the whole of it is a subsequence of the
/// candidate, which is right for one word and wrong for a description: no symbol
/// name contains "barrel jack horizontal" in order, so every descriptive query
/// scored `None` and fell through to the edit-distance backfill. Scoring the
/// words separately and requiring most of them to land keeps the fzf behaviour
/// for a single token while letting a phrase degrade to its best coverage.
fn score(matcher: &SkimMatcherV2, candidate: &str, needle: &str, tokens: &[&str]) -> Option<i64> {
    if let Some(whole) = matcher.fuzzy_match(candidate, needle) {
        return Some(whole);
    }
    if tokens.len() < 2 {
        return None;
    }
    let hits: Vec<i64> = tokens
        .iter()
        .filter_map(|token| matcher.fuzzy_match(candidate, token))
        .collect();
    let coverage = hits.len() as f64 / tokens.len() as f64;
    if coverage < MIN_TOKEN_COVERAGE {
        return None;
    }
    // Scale by coverage so a candidate matching every word outranks one matching
    // half, and keep it under any whole-needle score.
    Some((hits.iter().sum::<i64>() as f64 * coverage) as i64 / 2)
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
/// backfill ties break on `lib_id`. Only a single-word needle is backfilled.
fn rank(entries: &[Entry], needle: &str, n: usize) -> Vec<usize> {
    let matcher = SkimMatcherV2::default();
    let tokens: Vec<&str> = needle.split_whitespace().collect();

    let mut fuzzy: Vec<(i64, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| score(&matcher, &e.normalized, needle, &tokens).map(|s| (s, i)))
        .collect();
    fuzzy.sort_by(|&(sa, ia), &(sb, ib)| {
        sb.cmp(&sa)
            .then_with(|| {
                entries[ia]
                    .normalized
                    .len()
                    .cmp(&entries[ib].normalized.len())
            })
            .then_with(|| entries[ia].lib_id.cmp(&entries[ib].lib_id))
    });

    let mut chosen: Vec<usize> = fuzzy.into_iter().take(n).map(|(_, i)| i).collect();
    if chosen.len() >= n {
        return chosen;
    }

    // Backfill by edit distance recovers a TYPO — one word that nearly spells one
    // symbol. It cannot answer a phrase: a footprint name written where a lib_id
    // belongs matched nothing above and would come back with the least-bad neighbour
    // out of twenty thousand, which reads as an answer and sent one agent chasing a
    // part that never existed. Distance cannot tell the two apart (both sit near
    // 0.5); token count can, and a phrase has already had its coverage pass.
    if tokens.len() > 1 {
        return chosen;
    }
    let taken: std::collections::HashSet<usize> = chosen.iter().copied().collect();
    let mut rest: Vec<(f64, usize)> = entries
        .iter()
        .enumerate()
        .filter(|(i, _)| !taken.contains(i))
        .map(|(i, e)| {
            (
                1.0 - strsim::normalized_levenshtein(needle, &e.normalized),
                i,
            )
        })
        .collect();
    rest.sort_by(|&(da, ia), &(db, ib)| {
        da.partial_cmp(&db)
            .expect("distances are finite")
            .then_with(|| entries[ia].lib_id.cmp(&entries[ib].lib_id))
    });
    let want = n - chosen.len();
    chosen.extend(rest.into_iter().take(want).map(|(_, i)| i));
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

    fn entry(lib_id: &str) -> Entry {
        Entry::new(lib_id)
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
        let index = SymbolIndex::build(dir.path()).expect("build");

        assert_eq!(index.names.entries.len(), 1);
        assert_eq!(index.names.entries[0].lib_id, "Device:R");
        assert_eq!(
            index.names.entries[0].normalized, "device r",
            "the library name must be part of the searchable field"
        );
    }

    #[test]
    fn build_indexes_split_symbol_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lib = dir.path().join("Device.kicad_symdir");
        std::fs::create_dir(&lib).expect("symbol dir");
        std::fs::write(lib.join("R.kicad_sym"), "(kicad_symbol_lib (symbol \"R\"))")
            .expect("write split symbol");
        std::fs::write(lib.join("C.kicad_sym"), "(kicad_symbol_lib (symbol \"C\"))")
            .expect("write split symbol");

        let index = SymbolIndex::build(dir.path()).expect("build");
        let lib_ids: std::collections::BTreeSet<_> = index
            .names
            .entries
            .iter()
            .map(|e| e.lib_id.as_str())
            .collect();

        assert!(lib_ids.contains("Device:R"), "{lib_ids:?}");
        assert!(lib_ids.contains("Device:C"), "{lib_ids:?}");
    }

    #[test]
    fn empty_normalized_query_returns_no_hits() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("Device.kicad_sym"),
            "(kicad_symbol_lib (symbol \"R\"))",
        )
        .expect("write lib");
        let index = SymbolIndex::build(dir.path()).expect("build");

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

    fn names(lib_ids: &[&str]) -> SymbolNames {
        SymbolNames {
            entries: lib_ids.iter().copied().map(Entry::new).collect(),
        }
    }

    /// The right symbol under the wrong library is what an unknown lib_id nearly
    /// always is, and ranking the whole `Lib:Name` cannot see it.
    #[test]
    fn a_qualified_suggestion_ranks_on_the_symbol_name() {
        let index = names(&[
            "Connector_Generic:Conn_01x02",
            "Device:C",
            "Regulator_Switching:TPS62160DGK",
            "Regulator_Switching:TPS62160DSG",
        ]);

        assert_eq!(
            index.best_lib_id("Device:Conn_01x02", 1),
            ["Connector_Generic:Conn_01x02"]
        );
        assert_eq!(
            index.best_lib_id("Regulator_Switching:TPS62160", 2),
            ["Regulator_Switching:TPS62160DGK", "Regulator_Switching:TPS62160DSG"]
        );
    }

    /// The library half still breaks ties between equally named symbols.
    #[test]
    fn the_library_half_breaks_ties() {
        let index = names(&["Device:R", "Device_Old:R"]);
        assert_eq!(index.best_lib_id("Device:R", 1), ["Device:R"]);
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

    #[test]
    fn search_never_returns_an_unresolvable_top_level_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("Broken.kicad_sym"),
            "(kicad_symbol_lib (symbol \"Ghost\"",
        )
        .expect("write lib");
        let index = SymbolIndex::build(dir.path()).expect("build");

        assert!(index.search("Ghost", 8).is_empty());
    }
}
