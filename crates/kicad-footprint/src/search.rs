use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use kicad_env::KicadEnv;

use crate::Footprint;
use crate::discover::{discover_libraries, footprint_dir};

const SUGGEST_MAX_DISTANCE: usize = 6;
const SUGGEST_LIMIT: usize = 3;

/// A search hit: a fully qualified `Nickname:Name` id and its pad count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FootprintHit {
    pub lib_id: String,
    pub pad_count: usize,
}

struct Entry {
    lib_id: String,
    path: PathBuf,
    normalized: String,
}

/// Name index over every footprint in every installed `.pretty` library.
pub struct FootprintIndex {
    entries: Vec<Entry>,
    libraries: Vec<(String, PathBuf)>,
    cache: Mutex<HashMap<String, Option<Footprint>>>,
}

impl FootprintIndex {
    /// Scan every `*.pretty/*.kicad_mod` under the environment's footprint
    /// directory and index footprint names.
    pub fn build(env: &KicadEnv) -> io::Result<FootprintIndex> {
        Self::build_from_dir(&footprint_dir(env))
    }

    /// Build directly from a footprints directory containing `.pretty` dirs.
    pub fn build_from_dir(footprint_dir: &Path) -> io::Result<FootprintIndex> {
        let libraries = discover_libraries(footprint_dir)?;
        let mut entries = Vec::new();

        for (nick, dir) in &libraries {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut names: Vec<(String, PathBuf)> = rd
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "kicad_mod"))
                .filter_map(|p| {
                    let name = p.file_stem()?.to_str()?.to_string();
                    Some((name, p))
                })
                .collect();
            names.sort();
            for (name, path) in names {
                let lib_id = format!("{nick}:{name}");
                entries.push(Entry {
                    normalized: normalize(&lib_id),
                    lib_id,
                    path,
                });
            }
        }

        Ok(FootprintIndex {
            entries,
            libraries,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Number of indexed footprints.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of discovered `.pretty` libraries.
    pub fn library_count(&self) -> usize {
        self.libraries.len()
    }

    /// The discovered library nicknames, sorted.
    pub fn libraries(&self) -> impl Iterator<Item = &str> {
        self.libraries.iter().map(|(nick, _)| nick.as_str())
    }

    /// Fully-qualified `Nickname:Name` ids of every footprint in `nickname`.
    pub fn footprints_in(&self, nickname: &str) -> Vec<&str> {
        let prefix = format!("{nickname}:");
        self.entries
            .iter()
            .filter(|e| e.lib_id.starts_with(&prefix))
            .map(|e| e.lib_id.as_str())
            .collect()
    }

    /// Return the `n` best matches for `query`, best first.
    pub fn search(&self, query: &str, n: usize) -> Vec<FootprintHit> {
        let needle = normalize(query);
        if needle.is_empty() {
            return Vec::new();
        }
        rank(&self.entries, &needle, n)
            .into_iter()
            .map(|i| {
                let lib_id = self.entries[i].lib_id.clone();
                let pad_count = self.footprint(&lib_id).map_or(0, |fp| fp.pad_count());
                FootprintHit { lib_id, pad_count }
            })
            .collect()
    }

    /// Parse and memoize the footprint for a `Nickname:Name` id.
    pub fn footprint(&self, lib_id: &str) -> Option<Footprint> {
        {
            let cache = self.cache.lock().expect("footprint cache poisoned");
            if let Some(slot) = cache.get(lib_id) {
                return slot.clone();
            }
        }
        let parsed = self
            .entries
            .iter()
            .find(|e| e.lib_id == lib_id)
            .and_then(|e| Footprint::load(&e.path).ok());
        let mut cache = self.cache.lock().expect("footprint cache poisoned");
        cache.insert(lib_id.to_string(), parsed.clone());
        parsed
    }

    /// The `.kicad_mod` file path for a `Nickname:Name` id, if the id is known.
    pub fn footprint_path(&self, lib_id: &str) -> Option<&Path> {
        self.entries
            .iter()
            .find(|e| e.lib_id == lib_id)
            .map(|e| e.path.as_path())
    }

    /// The raw `.kicad_mod` source text for a `Nickname:Name` id, if readable.
    pub fn footprint_source(&self, lib_id: &str) -> Option<String> {
        let path = self.footprint_path(lib_id)?;
        std::fs::read_to_string(path).ok()
    }

    /// "Did-you-mean" suggestions for a `Nickname:Name` id.
    pub fn suggest(&self, lib_id: &str) -> Vec<String> {
        let Some((nick, name)) = lib_id.split_once(':') else {
            return Vec::new();
        };
        let needle = name.to_lowercase();
        let prefix = format!("{nick}:");
        let mut hits: Vec<(usize, &str)> = self
            .entries
            .iter()
            .filter(|e| e.lib_id.starts_with(&prefix))
            .map(|e| {
                let bare = &e.lib_id[prefix.len()..];
                (
                    strsim::levenshtein(&needle, &bare.to_lowercase()),
                    e.lib_id.as_str(),
                )
            })
            .filter(|(d, _)| *d <= SUGGEST_MAX_DISTANCE)
            .collect();
        hits.sort();
        hits.into_iter()
            .take(SUGGEST_LIMIT)
            .map(|(_, id)| id.to_string())
            .collect()
    }
}

fn rank(entries: &[Entry], needle: &str, n: usize) -> Vec<usize> {
    let matcher = SkimMatcherV2::default();

    let mut fuzzy: Vec<(i64, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| matcher.fuzzy_match(&e.normalized, needle).map(|s| (s, i)))
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
    chosen.extend(rest.into_iter().take(n - chosen.len()).map(|(_, i)| i));
    chosen
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(lib_id: &str) -> Entry {
        Entry {
            normalized: normalize(lib_id),
            lib_id: lib_id.to_string(),
            path: PathBuf::new(),
        }
    }

    #[test]
    fn fuzzy_ranks_exact_fragment_first() {
        let entries = vec![
            entry("Resistor_SMD:R_0402_1005Metric"),
            entry("Resistor_SMD:R_0603_1608Metric"),
            entry("Package_TO_SOT_SMD:SOT-23"),
        ];
        let ranked = rank(&entries, &normalize("R_0603_1608Metric"), 3);
        assert_eq!(entries[ranked[0]].lib_id, "Resistor_SMD:R_0603_1608Metric");
    }

    #[test]
    fn typo_returns_closest_via_backfill() {
        let entries = vec![
            entry("Package_TO_SOT_SMD:SOT-23"),
            entry("Resistor_SMD:R_0603_1608Metric"),
        ];
        let ranked = rank(&entries, &normalize("R_0663"), 1);
        assert_eq!(ranked.len(), 1);
        assert_eq!(entries[ranked[0]].lib_id, "Resistor_SMD:R_0603_1608Metric");
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
