//! What a sheet's text overlaps, as the render draws it.
//!
//! `cargo run -p sch-doc --example drawn_text -- <sheet.kicad_sch>…` prints one
//! line per sheet — parts, drawn texts, overlapping pairs and pairs per part —
//! then every pair. The measurement the readability lint is built on, without
//! the engine or KiCAD in the way.

use sch_doc::{SchDoc, drawn_texts};

fn main() {
    let (mut pairs, mut parts) = (0usize, 0usize);
    for path in std::env::args().skip(1) {
        let doc = match SchDoc::read(&path) {
            Ok(doc) => doc,
            Err(e) => {
                eprintln!("{path}: {e}");
                continue;
            }
        };
        let sheet_parts = doc
            .symbols()
            .filter(|s| !s.refdes().is_empty() && !s.refdes().starts_with('#'))
            .count();
        let texts = drawn_texts(&doc);
        let mut hits = Vec::new();
        for (i, a) in texts.iter().enumerate() {
            for b in &texts[i + 1..] {
                let same_pin = a.owner.is_some()
                    && a.owner == b.owner
                    && is_pin_text(a.kind)
                    && is_pin_text(b.kind);
                if !same_pin && a.bbox.overlaps(&b.bbox) {
                    hits.push(format!("  {:?} {:?} x {:?} {:?}", a.kind, a.text, b.kind, b.text));
                }
            }
        }
        let extent = texts
            .iter()
            .map(|t| t.bbox)
            .reduce(|a, b| geom::Rect::new(a.min_x.min(b.min_x), a.min_y.min(b.min_y), a.max_x.max(b.max_x), a.max_y.max(b.max_y)));
        println!(
            "{path}\textent={extent:?}\tparts={sheet_parts}\ttexts={}\tpairs={}\tper_part={:.3}",
            texts.len(),
            hits.len(),
            hits.len() as f64 / sheet_parts.max(1) as f64
        );
        for hit in &hits {
            println!("{hit}");
        }
        pairs += hits.len();
        parts += sheet_parts;
    }
    println!("TOTAL\tparts={parts}\tpairs={pairs}\tper_part={:.3}", pairs as f64 / parts.max(1) as f64);
}

fn is_pin_text(kind: sch_model::text::TextKind) -> bool {
    matches!(
        kind,
        sch_model::text::TextKind::PinName | sch_model::text::TextKind::PinNumber
    )
}
