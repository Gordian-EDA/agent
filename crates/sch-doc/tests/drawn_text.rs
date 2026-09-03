//! The as-drawn text model, checked against ink measured off real renders.
//!
//! Each fixture pairs a placed sheet with the text-vs-text overlaps
//! `kicad-cli sch export svg` actually draws — every pair of `stroked-text`
//! ink boxes that intersect. The model must FIND all of them: a lint built on
//! it then reports what the render shows.

use std::path::PathBuf;

use sch_doc::{SchDoc, drawn_texts};

const SHEETS: [(&str, usize); 3] = [
    ("C-campaign-bms-10s", 35),
    ("C-campaign-stm32-buck", 42),
    ("C-campaign-esp32-sensor-node", 12),
];

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/asdrawn")
}

/// One measured ink overlap: the two strings and the centre of each ink box.
struct InkPair {
    texts: [String; 2],
    centres: [[f64; 2]; 2],
}

fn read_ink_overlaps(sheet: &str) -> Vec<InkPair> {
    let path = fixtures().join(format!("{sheet}.ink-overlaps.tsv"));
    let body = std::fs::read_to_string(&path).expect("ink-overlap fixture");
    body.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let cols: Vec<&str> = line.split('\t').collect();
            let centre = |s: &str| {
                let v: Vec<f64> = s.split(',').map(|n| n.parse().unwrap()).collect();
                [(v[0] + v[2]) / 2.0, (v[1] + v[3]) / 2.0]
            };
            InkPair {
                texts: [cols[0].to_string(), cols[2].to_string()],
                centres: [centre(cols[1]), centre(cols[3])],
            }
        })
        .collect()
}

/// Model pairs, as (text, box centre) couples.
fn model_overlaps(sheet: &str) -> Vec<([String; 2], [[f64; 2]; 2])> {
    let doc = SchDoc::read(fixtures().join(format!("{sheet}.kicad_sch"))).expect("fixture sheet");
    // A multi-unit part stacks coincident duplicates; dedupe identical boxes so
    // one drawn glyph counts once, as the ink measurement does.
    let mut texts = drawn_texts(&doc);
    let mut seen = std::collections::BTreeSet::new();
    texts.retain(|t| {
        let k = (
            t.text.clone(),
            (t.bbox.min_x * 1000.0).round() as i64,
            (t.bbox.min_y * 1000.0).round() as i64,
            (t.bbox.max_x * 1000.0).round() as i64,
            (t.bbox.max_y * 1000.0).round() as i64,
        );
        seen.insert(k)
    });
    let mut pairs = Vec::new();
    for i in 0..texts.len() {
        for j in (i + 1)..texts.len() {
            let (a, b) = (&texts[i], &texts[j]);
            if a.bbox.overlaps(&b.bbox) {
                pairs.push((
                    [a.text.clone(), b.text.clone()],
                    [
                        [a.bbox.center().x, a.bbox.center().y],
                        [b.bbox.center().x, b.bbox.center().y],
                    ],
                ));
            }
        }
    }
    pairs
}

/// The model finds every overlap the render draws.
///
/// A pair matches when both strings match and both box centres land within a
/// tolerance that grows with the string: an advance box is a little wider than
/// the ink it contains, so a long note's centre sits a couple of millimetres
/// past the ink's.
#[test]
fn model_finds_every_drawn_overlap() {
    let (mut found, mut want, mut extra) = (0usize, 0usize, 0usize);
    for (sheet, expected) in SHEETS {
        let ink = read_ink_overlaps(sheet);
        assert_eq!(ink.len(), expected, "{sheet}: fixture pair count");
        let model = model_overlaps(sheet);
        let tol = |s: &str| 3.0 + 0.05 * s.len() as f64;
        let mut used = vec![false; model.len()];
        let mut missed = Vec::new();
        for pair in &ink {
            let hit = model.iter().enumerate().position(|(i, (texts, centres))| {
                !used[i]
                    && [[0, 1], [1, 0]].iter().any(|order| {
                        (0..2).all(|k| {
                            let o = order[k];
                            texts[k] == pair.texts[o]
                                && (centres[k][0] - pair.centres[o][0])
                                    .hypot(centres[k][1] - pair.centres[o][1])
                                    < tol(&texts[k])
                        })
                    })
            });
            match hit {
                Some(i) => {
                    used[i] = true;
                    found += 1;
                }
                None => missed.push(format!("{} x {}", pair.texts[0], pair.texts[1])),
            }
        }
        eprintln!(
            "{sheet}: drawn={} model={} found={}",
            ink.len(),
            model.len(),
            used.iter().filter(|u| **u).count()
        );
        want += ink.len();
        extra += model.len() - used.iter().filter(|u| **u).count();
        assert!(
            missed.is_empty(),
            "{sheet}: model missed drawn overlaps {missed:?}"
        );
    }
    assert_eq!(found, want, "every drawn overlap is found");
    // The remaining model pairs are near-misses: advance boxes that touch where
    // the ink inside them clears by a fraction of a millimetre. Keeping them is
    // the safe side for a lint that drives avoidance, but the model must not
    // drift into crying wolf.
    assert!(
        extra <= want / 4,
        "too many near-miss pairs: {extra} beyond {want}"
    );
}
