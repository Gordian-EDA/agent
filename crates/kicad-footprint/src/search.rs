//! Cross-library footprint search: the query/result contract plus the private
//! fuzzy ranking machinery that [`crate::FootprintCatalog::search`] drives.
//!
//! Ranking is fzf-style subsequence scoring via `fuzzy-matcher`'s
//! [`SkimMatcherV2`] — the project's standard fuzzy matcher. `strsim` edit
//! distance backs only the typo backfill, so a non-subsequence query still
//! returns the closest candidates rather than nothing.

use std::collections::HashSet;

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use crate::id::FootprintId;

/// A fuzzy search request: the raw query text and a result cap.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    query: String,
    limit: usize,
}

impl SearchQuery {
    /// A query with the default limit of 8 hits.
    pub fn new(query: impl Into<String>) -> Self {
        SearchQuery {
            query: query.into(),
            limit: 8,
        }
    }

    /// Set the maximum number of hits to return.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// The raw query text.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The configured result cap.
    pub fn limit_value(&self) -> usize {
        self.limit
    }
}

impl From<&str> for SearchQuery {
    fn from(value: &str) -> Self {
        SearchQuery::new(value)
    }
}

impl From<String> for SearchQuery {
    fn from(value: String) -> Self {
        SearchQuery::new(value)
    }
}

/// One ranked search hit.
///
/// `pad_count` is `None` when the footprint's `.kicad_mod` could not be parsed,
/// so a parse failure is visibly distinct from a genuine zero-pad footprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FootprintSearchHit {
    pub id: FootprintId,
    pub score: i64,
    pub pad_count: Option<usize>,
}

/// Lowercase and collapse runs of non-alphanumeric characters into single
/// spaces, so `R_0603_1608Metric` and `r 0603 1608metric` compare on equal
/// footing. Shared by search ranking and suggestion matching.
pub(crate) fn normalize(s: &str) -> String {
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

/// Backfilled did-you-mean candidates farther than this normalized distance
/// are noise, not suggestions, and are dropped.
const SUGGEST_MAX_BACKFILL_DISTANCE: f64 = 0.6;

/// Rank `items` against an already-normalized `needle`, returning the indices
/// of the best `n`, best first.
///
/// `norm` extracts each item's pre-normalized searchable text; `tiebreak`
/// extracts a deterministic tie-break string (the canonical lib id). Primary
/// order is fzf subsequence score; ties break on shorter text then `tiebreak`.
/// When fewer than `n` items match as a subsequence (e.g. a transposition), the
/// remainder is backfilled by edit distance so the caller is never starved of
/// candidates.
pub(crate) fn rank<T>(
    items: &[T],
    needle: &str,
    n: usize,
    norm: impl Fn(&T) -> &str,
    tiebreak: impl Fn(&T) -> &str,
) -> Vec<usize> {
    rank_by(
        items,
        n,
        |m, t| m.fuzzy_match(norm(t), needle),
        |t| Some(1.0 - strsim::normalized_levenshtein(needle, norm(t))),
        |t| norm(t).len(),
        tiebreak,
    )
}

/// Rank `items` as did-you-mean suggestions for an unresolved footprint id,
/// returning the indices of the best `n`, best first.
///
/// Each item scores the better of its full `Lib:Name` text against
/// `full_needle` and its bare name against `name_needle`, so a right name in
/// a wrong library still ranks. Non-subsequence candidates backfill by the
/// best of edit-distance and token-overlap closeness — KiCAD names are
/// dimension-token heavy, and token overlap keeps `LGA-8_2.5x2.5mm_P0.65mm`
/// variants together where raw edit distance drifts — but only within
/// [`SUGGEST_MAX_BACKFILL_DISTANCE`], so a hopeless id yields nothing rather
/// than arbitrary nearest neighbors.
pub(crate) fn rank_suggestions<T>(
    items: &[T],
    full_needle: &str,
    name_needle: &str,
    n: usize,
    full: impl Fn(&T) -> &str,
    name: impl Fn(&T) -> &str,
    tiebreak: impl Fn(&T) -> &str,
) -> Vec<usize> {
    rank_by(
        items,
        n,
        |m, t| {
            let by_full = m.fuzzy_match(full(t), full_needle);
            let by_name = m.fuzzy_match(name(t), name_needle);
            by_full.max(by_name)
        },
        |t| {
            let lev = |a: &str, b: &str| 1.0 - strsim::normalized_levenshtein(a, b);
            let d = [
                lev(full_needle, full(t)),
                lev(name_needle, name(t)),
                token_distance(full_needle, full(t)),
                token_distance(name_needle, name(t)),
            ]
            .into_iter()
            .fold(f64::INFINITY, f64::min);
            (d <= SUGGEST_MAX_BACKFILL_DISTANCE).then_some(d)
        },
        |t| full(t).len(),
        tiebreak,
    )
}

/// The shared ranking core: fzf subsequence scores first (higher is better,
/// ties break on shorter text then `tiebreak`), then non-matching items
/// backfill by ascending `distance` (returning `None` excludes an item).
fn rank_by<T>(
    items: &[T],
    n: usize,
    fuzzy: impl Fn(&SkimMatcherV2, &T) -> Option<i64>,
    distance: impl Fn(&T) -> Option<f64>,
    text_len: impl Fn(&T) -> usize,
    tiebreak: impl Fn(&T) -> &str,
) -> Vec<usize> {
    let matcher = SkimMatcherV2::default();

    let mut scored: Vec<(i64, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, t)| fuzzy(&matcher, t).map(|s| (s, i)))
        .collect();
    scored.sort_by(|&(sa, ia), &(sb, ib)| {
        sb.cmp(&sa)
            .then_with(|| text_len(&items[ia]).cmp(&text_len(&items[ib])))
            .then_with(|| tiebreak(&items[ia]).cmp(tiebreak(&items[ib])))
    });

    let mut chosen: Vec<usize> = scored.into_iter().take(n).map(|(_, i)| i).collect();
    if chosen.len() >= n {
        return chosen;
    }

    let taken: HashSet<usize> = chosen.iter().copied().collect();
    let mut rest: Vec<(f64, usize)> = items
        .iter()
        .enumerate()
        .filter(|(i, _)| !taken.contains(i))
        .filter_map(|(i, t)| distance(t).map(|d| (d, i)))
        .collect();
    rest.sort_by(|&(da, ia), &(db, ib)| {
        da.partial_cmp(&db)
            .expect("distances are finite")
            .then_with(|| tiebreak(&items[ia]).cmp(tiebreak(&items[ib])))
    });
    chosen.extend(rest.into_iter().take(n - chosen.len()).map(|(_, i)| i));
    chosen
}

/// `1 −` the Dice coefficient over the whitespace tokens of two normalized
/// strings: 0.0 for identical token multisets, 1.0 for disjoint ones.
fn token_distance(a: &str, b: &str) -> f64 {
    let ta: Vec<&str> = a.split_whitespace().collect();
    let mut tb: Vec<&str> = b.split_whitespace().collect();
    if ta.is_empty() || tb.is_empty() {
        return 1.0;
    }
    let total = ta.len() + tb.len();
    let mut matched = 0usize;
    for t in ta {
        if let Some(pos) = tb.iter().position(|&x| x == t) {
            tb.swap_remove(pos);
            matched += 1;
        }
    }
    1.0 - (2.0 * matched as f64) / total as f64
}

/// The fuzzy score `needle` earns against `text`, if it matches as a
/// subsequence. Used by the catalog to populate [`FootprintSearchHit::score`].
pub(crate) fn fuzzy_score(text: &str, needle: &str) -> i64 {
    SkimMatcherV2::default()
        .fuzzy_match(text, needle)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(lib_id: &str) -> (String, String) {
        (normalize(lib_id), lib_id.to_string())
    }

    fn rank_ids(items: &[(String, String)], needle: &str, n: usize) -> Vec<usize> {
        rank(items, needle, n, |t| t.0.as_str(), |t| t.1.as_str())
    }

    #[test]
    fn fuzzy_ranks_exact_fragment_first() {
        let items = vec![
            item("Resistor_SMD:R_0402_1005Metric"),
            item("Resistor_SMD:R_0603_1608Metric"),
            item("Package_TO_SOT_SMD:SOT-23"),
        ];
        let ranked = rank_ids(&items, &normalize("R_0603_1608Metric"), 3);
        assert_eq!(items[ranked[0]].1, "Resistor_SMD:R_0603_1608Metric");
    }

    #[test]
    fn typo_returns_closest_via_backfill() {
        let items = vec![
            item("Package_TO_SOT_SMD:SOT-23"),
            item("Resistor_SMD:R_0603_1608Metric"),
        ];
        let ranked = rank_ids(&items, &normalize("R_0663"), 1);
        assert_eq!(ranked.len(), 1);
        assert_eq!(items[ranked[0]].1, "Resistor_SMD:R_0603_1608Metric");
    }

    #[test]
    fn normalize_matches_symbol_search_conventions() {
        assert_eq!(
            normalize("Resistor_SMD:R_0603_1608Metric"),
            "resistor smd r 0603 1608metric"
        );
        assert_eq!(normalize("SOT-23"), "sot 23");
    }
}
