//! Retrieval over a corpus of real human KiCAD schematics, so the design agent
//! can ground itself in professional patterns (block partitioning, decoupling,
//! idioms) at the source rather than inventing them.
//!
//! The corpus is a directory of paired files: for each design, a `<id>.json`
//! (metadata with a free-text `description`) sits next to a `<id>.kicad_sch`.
//! Given a design INTENT string, [`Corpus::find_similar`] ranks the descriptions
//! with `fuzzy-matcher`'s `SkimMatcherV2` (fzf-style subsequence scoring, the
//! project's mandated fuzzy ranker) and lifts the top matches' schematics to
//! circuit-YAML via the existing [`sch_io::read::lift`] path — so the agent
//! studies references in the SAME language it authors in.
//!
//! ## Configurable + absent-safe
//!
//! The corpus directory comes from `$GORDIAN_CORPUS_DIR`, falling back to
//! `~/kicad-scraper/dataset`. [`Corpus::discover`] never fails when the corpus is
//! missing — it returns an empty corpus, so retrieval degrades to a no-op on
//! machines without the dataset.
//!
//! ## Lift is best-effort
//!
//! Lifting an arbitrary human schematic runs `kicad-cli` and reconstructs the
//! kernel model; it WILL sometimes fail (exotic libs, multi-sheet hierarchies,
//! corrupt files). [`Corpus::find_similar`] skips an un-liftable match and walks
//! down the ranked list to backfill up to `k` successes, reporting the lift
//! success rate. A match whose schematic won't lift still carries its description,
//! which is itself useful grounding.

use std::path::{Path, PathBuf};

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use kicad_cli::env::KicadEnv;
use serde::Deserialize;

/// Environment variable naming the corpus directory; overrides the default.
pub const CORPUS_DIR_ENV: &str = "GORDIAN_CORPUS_DIR";

/// Default corpus location (under `$HOME`) when [`CORPUS_DIR_ENV`] is unset.
const DEFAULT_CORPUS_SUBPATH: &str = "kicad-scraper/dataset";

/// Default number of references [`Corpus::find_similar`] returns.
pub const DEFAULT_K: usize = 3;

/// To bound the search, rank descriptions but only attempt to lift this many
/// extra candidates beyond `k` before giving up — a deep miss-streak shouldn't
/// lift dozens of schematics for one tool call.
const MAX_LIFT_ATTEMPTS_BEYOND_K: usize = 7;

/// One design's `<id>.json` metadata. Only the fields the agent reasons about are
/// modelled; the scraper writes more (`metrics`, `source_url`, …) which we ignore.
#[derive(Debug, Clone, Deserialize)]
pub struct DesignMeta {
    /// The scraper's stable id; also the `<id>.kicad_sch` / `<id>.json` stem.
    pub id: String,
    /// Free-text description of what the design is — the field we rank against.
    #[serde(default)]
    pub description: String,
    /// Origin repo (`owner/name`), surfaced as light provenance.
    #[serde(default)]
    pub repo: String,
}

/// One reference handed back from [`Corpus::find_similar`]: the matched design's
/// metadata, the fuzzy score, and — when the lift succeeded — its circuit-YAML.
#[derive(Debug, Clone)]
pub struct Reference {
    pub meta: DesignMeta,
    /// The `SkimMatcherV2` score for the intent against the description (higher is
    /// a better subsequence match).
    pub score: i64,
    /// The lifted circuit-YAML, or `None` when the schematic could not be lifted.
    pub yaml: Option<String>,
    /// The lift failure message when `yaml` is `None` (for honest reporting).
    pub lift_error: Option<String>,
}

/// A discovered corpus of paired `<id>.json` / `<id>.kicad_sch` designs.
///
/// Cheap to hold: stores only the directory and the parsed metadata (descriptions
/// are tiny). Schematics are lifted lazily, only for matched designs.
#[derive(Debug, Default)]
pub struct Corpus {
    dir: Option<PathBuf>,
    designs: Vec<DesignMeta>,
}

impl Corpus {
    /// Resolve the corpus directory from [`CORPUS_DIR_ENV`] or the default
    /// `~/kicad-scraper/dataset`, returning `None` only when no home directory is
    /// known and no override is set.
    pub fn default_dir() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os(CORPUS_DIR_ENV) {
            return Some(PathBuf::from(dir));
        }
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(DEFAULT_CORPUS_SUBPATH))
    }

    /// Discover the corpus at the configured directory. ABSENT-SAFE: a missing or
    /// unreadable directory yields an empty corpus (never an error), so callers on
    /// machines without the dataset get a graceful no-op.
    pub fn discover() -> Self {
        match Self::default_dir() {
            Some(dir) => Self::from_dir(&dir),
            None => Self::default(),
        }
    }

    /// Discover the corpus rooted at an explicit directory (the env/default
    /// resolution bypassed). Also absent-safe.
    pub fn from_dir(dir: &Path) -> Self {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Self::default();
        };
        let mut designs = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(meta) = serde_json::from_str::<DesignMeta>(&text) {
                designs.push(meta);
            }
        }
        Self {
            dir: Some(dir.to_path_buf()),
            designs,
        }
    }

    /// Whether the corpus has any usable designs (an absent corpus is empty).
    pub fn is_empty(&self) -> bool {
        self.designs.is_empty()
    }

    /// Number of designs whose metadata parsed.
    pub fn len(&self) -> usize {
        self.designs.len()
    }

    /// The `<id>.kicad_sch` path for a design id under this corpus.
    fn sch_path(&self, id: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(format!("{id}.kicad_sch")))
    }

    /// Rank every design's description against `intent` with `SkimMatcherV2`,
    /// best first. A non-match scores `None` and is dropped. Returns indices into
    /// [`Self::designs`] paired with their score, in a deterministic order.
    ///
    /// The intent is a multi-word REQUEST and the descriptions are short, so the
    /// orientation that works for short part-name search (whole-needle-as-
    /// subsequence) fails here — the full intent is almost never a subsequence of
    /// a one-line description. Instead this scores each description as a BAG OF THE
    /// INTENT'S TERMS: every intent word is fuzzy-matched against the description
    /// and the best per-word scores are summed (a word the description lacks
    /// contributes nothing). A description that hits MORE of the intent's salient
    /// terms — "stm32", "usb", "regulator" — ranks higher, order-independently.
    fn rank(&self, intent: &str) -> Vec<(usize, i64)> {
        let matcher = SkimMatcherV2::default();
        let terms = intent_terms(intent);
        if terms.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(usize, i64)> = self
            .designs
            .iter()
            .enumerate()
            .filter_map(|(i, d)| {
                let total: i64 = terms
                    .iter()
                    .filter_map(|t| matcher.fuzzy_match(&d.description, t))
                    .sum();
                (total > 0).then_some((i, total))
            })
            .collect();
        // Stable, deterministic order: score desc, then id asc to break ties.
        scored.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| self.designs[a.0].id.cmp(&self.designs[b.0].id))
        });
        scored
    }

    /// The single best-ranked description for `intent` (no lift), or `None` when
    /// the corpus is empty or nothing matches. Used for the cheap "is there a
    /// plausible reference?" probe and description-only ranking.
    pub fn best_meta(&self, intent: &str) -> Option<(&DesignMeta, i64)> {
        self.rank(intent)
            .first()
            .map(|&(i, score)| (&self.designs[i], score))
    }

    /// Rank the corpus by `intent` and return up to `k` references, lifting each
    /// matched schematic to circuit-YAML on demand.
    ///
    /// Lifting is best-effort: an un-liftable match is still returned (with its
    /// description and the lift error), and the walk continues down the ranked
    /// list — attempting at most `k + MAX_LIFT_ATTEMPTS_BEYOND_K` candidates — to
    /// backfill `k` SUCCESSFUL lifts where possible. The returned vec is ordered
    /// by score (successful and failed lifts interleaved by rank).
    ///
    /// [`RetrievalReport::lift_success_rate`] on the result tells callers how many
    /// of the attempted lifts produced YAML — honest data about corpus usability.
    pub fn find_similar(&self, env: &KicadEnv, intent: &str, k: usize) -> RetrievalReport {
        let ranked = self.rank(intent);
        let mut references = Vec::new();
        let mut successes = 0usize;
        let max_attempts = k.saturating_add(MAX_LIFT_ATTEMPTS_BEYOND_K);

        for &(idx, score) in ranked.iter() {
            if successes >= k || references.len() >= max_attempts {
                break;
            }
            let meta = self.designs[idx].clone();
            let (yaml, lift_error) = match self.sch_path(&meta.id) {
                Some(path) if path.is_file() => match sch_io::read::lift(env, &path) {
                    Ok(y) => {
                        successes += 1;
                        (Some(y), None)
                    }
                    Err(e) => (None, Some(e.to_string())),
                },
                Some(path) => (None, Some(format!("no schematic at {}", path.display()))),
                None => (None, Some("corpus directory unknown".to_string())),
            };
            references.push(Reference {
                meta,
                score,
                yaml,
                lift_error,
            });
        }

        RetrievalReport {
            references,
            ranked_total: ranked.len(),
            successes,
        }
    }
}

/// Generic words that carry no discriminating signal for a hardware intent — if
/// every board is "a board with ...", matching on them just adds noise.
const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "with", "for", "of", "to", "on", "in",
    "board", "design", "circuit", "schematic", "pcb", "module", "system",
];

/// Split an intent into the salient lowercase terms used for ranking: alphanumeric
/// runs, stop-words and single characters dropped. Deduplicated so a repeated word
/// can't dominate the score.
fn intent_terms(intent: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    intent
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.len() > 1 && !STOP_WORDS.contains(&w.as_str()))
        .filter(|w| seen.insert(w.clone()))
        .collect()
}

/// The outcome of a [`Corpus::find_similar`] call: the references plus the
/// bookkeeping needed to report the lift success rate honestly.
#[derive(Debug, Clone)]
pub struct RetrievalReport {
    /// The matched references, score-ordered (successful + failed lifts).
    pub references: Vec<Reference>,
    /// How many designs matched the intent at all (the full ranked pool size).
    pub ranked_total: usize,
    /// How many of the attempted lifts produced circuit-YAML.
    pub successes: usize,
}

impl RetrievalReport {
    /// References whose schematic lifted to YAML.
    pub fn lifted(&self) -> impl Iterator<Item = &Reference> {
        self.references.iter().filter(|r| r.yaml.is_some())
    }

    /// Fraction of attempted lifts that succeeded, in `[0.0, 1.0]`; `0.0` when no
    /// lift was attempted.
    pub fn lift_success_rate(&self) -> f64 {
        if self.references.is_empty() {
            return 0.0;
        }
        self.successes as f64 / self.references.len() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Stage a tiny corpus of `<id>.json` files (no schematics needed — these
    /// tests exercise ranking + absent-safety, not the kicad-cli lift) and return
    /// the tempdir guard plus its path.
    fn staged_corpus() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let designs = [
            ("aaa", "STM32 microcontroller board with USB and a 3V3 regulator."),
            ("bbb", "Simple resistor voltage divider reference design."),
            ("ccc", "Audio amplifier with an op-amp and power supply filtering."),
        ];
        for (id, desc) in designs {
            let mut f = std::fs::File::create(tmp.path().join(format!("{id}.json"))).unwrap();
            write!(
                f,
                r#"{{"id":"{id}","repo":"acme/{id}","description":"{desc}"}}"#
            )
            .unwrap();
        }
        let path = tmp.path().to_path_buf();
        (tmp, path)
    }

    #[test]
    fn absent_corpus_is_empty_and_never_errors() {
        let corpus = Corpus::from_dir(Path::new("/no/such/corpus/dir"));
        assert!(corpus.is_empty());
        assert_eq!(corpus.len(), 0);
        assert!(corpus.best_meta("anything").is_none());
    }

    #[test]
    fn known_intent_ranks_a_plausible_design_first() {
        let (_guard, dir) = staged_corpus();
        let corpus = Corpus::from_dir(&dir);
        assert_eq!(corpus.len(), 3);

        // An STM32/USB/3V3 intent must rank the STM32 design ("aaa") above the
        // voltage-divider and audio-amp references.
        let (best, score) = corpus
            .best_meta("STM32 microcontroller board with USB and 3V3 regulator")
            .expect("a plausible match");
        assert_eq!(best.id, "aaa", "STM32 intent must rank the STM32 design first");
        assert!(score > 0, "a real subsequence match scores positive");
    }

    #[test]
    fn rank_skips_non_matches() {
        let (_guard, dir) = staged_corpus();
        let corpus = Corpus::from_dir(&dir);
        // A query with characters not present as a subsequence in ANY description
        // matches nothing.
        assert!(corpus.best_meta("zzzz qqqq xxxx").is_none());
    }
}
