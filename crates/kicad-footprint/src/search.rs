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
    let matcher = SkimMatcherV2::default();

    let mut fuzzy: Vec<(i64, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, t)| matcher.fuzzy_match(norm(t), needle).map(|s| (s, i)))
        .collect();
    fuzzy.sort_by(|&(sa, ia), &(sb, ib)| {
        sb.cmp(&sa)
            .then_with(|| norm(&items[ia]).len().cmp(&norm(&items[ib]).len()))
            .then_with(|| tiebreak(&items[ia]).cmp(tiebreak(&items[ib])))
    });

    let mut chosen: Vec<usize> = fuzzy.into_iter().take(n).map(|(_, i)| i).collect();
    if chosen.len() >= n {
        return chosen;
    }

    let taken: HashSet<usize> = chosen.iter().copied().collect();
    let mut rest: Vec<(f64, usize)> = items
        .iter()
        .enumerate()
        .filter(|(i, _)| !taken.contains(i))
        .map(|(i, t)| (1.0 - strsim::normalized_levenshtein(needle, norm(t)), i))
        .collect();
    rest.sort_by(|&(da, ia), &(db, ib)| {
        da.partial_cmp(&db)
            .expect("distances are finite")
            .then_with(|| tiebreak(&items[ia]).cmp(tiebreak(&items[ib])))
    });
    chosen.extend(rest.into_iter().take(n - chosen.len()).map(|(_, i)| i));
    chosen
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
