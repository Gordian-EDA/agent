//! The as-drawn text model, checked against ink measured off real renders.
//!
//! Each fixture pairs a placed sheet with the text-vs-text overlaps
//! `kicad-cli sch export svg` actually draws — every pair of `stroked-text`
//! ink boxes that intersect. Two things must hold: each model box CONTAINS the
//! ink KiCAD strokes, and the model finds every overlap the render shows. A
//! readability lint built on it then reports what the eye sees.

use std::path::PathBuf;

use geom::Rect;
use sch_doc::{SchDoc, drawn_texts};

const SHEETS: [(&str, usize); 3] = [
    ("C-campaign-bms-10s", 35),
    ("C-campaign-stm32-buck", 42),
    ("C-campaign-esp32-sensor-node", 12),
];

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/asdrawn")
}

/// One measured ink overlap: the two strings and each one's ink box.
struct InkPair {
    texts: [String; 2],
    boxes: [Rect; 2],
}

fn read_ink_overlaps(sheet: &str) -> Vec<InkPair> {
    let path = fixtures().join(format!("{sheet}.ink-overlaps.tsv"));
    let body = std::fs::read_to_string(&path).expect("ink-overlap fixture");
    body.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let cols: Vec<&str> = line.split('\t').collect();
            let rect = |s: &str| {
                let v: Vec<f64> = s.split(',').map(|n| n.parse().unwrap()).collect();
                Rect::new(v[0], v[1], v[2], v[3])
            };
            InkPair {
                texts: [cols[0].to_string(), cols[2].to_string()],
                boxes: [rect(cols[1]), rect(cols[3])],
            }
        })
        .collect()
}

fn sheet_doc(sheet: &str) -> SchDoc {
    SchDoc::read(fixtures().join(format!("{sheet}.kicad_sch"))).expect("fixture sheet")
}

/// How far the ink pokes out of `model` on its worst side (negative = clear).
fn shortfall(model: Rect, ink: Rect) -> f64 {
    [
        model.min_x - ink.min_x,
        model.min_y - ink.min_y,
        ink.max_x - model.max_x,
        ink.max_y - model.max_y,
    ]
    .into_iter()
    .fold(f64::MIN, f64::max)
}

/// Every ink box the render strokes sits inside the box the model predicts.
///
/// This is the calibration itself: get a band, a lift or a glyph advance
/// wrong and some string's ink escapes its box.
#[test]
fn model_boxes_contain_the_drawn_ink() {
    let mut worst: (f64, String) = (f64::MIN, String::new());
    let mut checked = 0usize;
    for (sheet, _) in SHEETS {
        let model = drawn_texts(&sheet_doc(sheet));
        for pair in read_ink_overlaps(sheet) {
            for side in 0..2 {
                // The model box for this ink is the one with the same string
                // that fits it best; a sheet repeats strings many times over.
                let best = model
                    .iter()
                    .filter(|t| t.text == pair.texts[side])
                    .map(|t| shortfall(t.bbox, pair.boxes[side]))
                    .fold(f64::MAX, f64::min);
                assert!(
                    best < f64::MAX,
                    "{sheet}: no model box for {:?}",
                    pair.texts[side]
                );
                checked += 1;
                if best > worst.0 {
                    let m = model
                        .iter()
                        .filter(|t| t.text == pair.texts[side])
                        .min_by(|a, b| {
                            shortfall(a.bbox, pair.boxes[side])
                                .total_cmp(&shortfall(b.bbox, pair.boxes[side]))
                        })
                        .unwrap();
                    worst = (
                        best,
                        format!(
                            "{sheet}: {:?} kind={:?} model={:?} ink={:?}",
                            pair.texts[side], m.kind, m.bbox, pair.boxes[side]
                        ),
                    );
                }
            }
        }
    }
    assert!(
        checked > 150,
        "the fixtures must exercise the model broadly"
    );
    // Sub-0.1 mm is a stroke half-width: the box is the advance box, so it
    // rides right on the ink of a glyph with no side bearing.
    eprintln!(
        "worst containment shortfall {:.3} mm at {}",
        worst.0, worst.1
    );
    assert!(
        worst.0 < 0.1,
        "ink escapes its model box by {:.3} mm at {}",
        worst.0,
        worst.1
    );
}

/// The model finds every overlap the render draws.
///
/// A model pair matches an ink pair when each model box contains the centre of
/// the ink box it stands for — identity, not proximity, so a repeated string
/// cannot be matched against the wrong occurrence.
#[test]
fn model_finds_every_drawn_overlap() {
    let (mut drawn, mut near_miss) = (0usize, 0usize);
    for (sheet, expected) in SHEETS {
        let ink = read_ink_overlaps(sheet);
        assert_eq!(ink.len(), expected, "{sheet}: fixture pair count");
        let texts = drawn_texts(&sheet_doc(sheet));
        let mut model = Vec::new();
        for (i, a) in texts.iter().enumerate() {
            for b in &texts[i + 1..] {
                if a.bbox.overlaps(&b.bbox) {
                    model.push((a, b));
                }
            }
        }
        let holds = |t: &sch_model::text::DrawnText, text: &str, ink: Rect| {
            t.text == text && t.bbox.contains(ink.center())
        };
        let mut used = vec![false; model.len()];
        let mut missed = Vec::new();
        for pair in &ink {
            let hit = model.iter().enumerate().position(|(i, (a, b))| {
                !used[i]
                    && ((holds(a, &pair.texts[0], pair.boxes[0])
                        && holds(b, &pair.texts[1], pair.boxes[1]))
                        || (holds(a, &pair.texts[1], pair.boxes[1])
                            && holds(b, &pair.texts[0], pair.boxes[0])))
            });
            match hit {
                Some(i) => used[i] = true,
                None => missed.push(format!("{} x {}", pair.texts[0], pair.texts[1])),
            }
        }
        assert!(
            missed.is_empty(),
            "{sheet}: model missed drawn overlaps {missed:?}"
        );
        drawn += ink.len();
        near_miss += model.len() - used.iter().filter(|u| **u).count();
    }
    // What is left over are advance boxes that touch where the ink inside them
    // clears by a fraction of a millimetre. Reporting those is the safe side
    // for a lint that drives avoidance, but the model must not cry wolf.
    eprintln!("drawn={drawn} near_miss={near_miss}");
    assert!(
        near_miss < drawn / 2,
        "{near_miss} near-miss pairs beyond the {drawn} the render draws"
    );
}
