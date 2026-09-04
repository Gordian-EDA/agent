//! What a sheet's text overlaps, as the render draws it.
//!
//! `cargo run -p sch-doc --example drawn_text -- <sheet.kicad_sch>…` prints one
//! line per sheet — the extent its text really covers, its parts, its drawn
//! texts, and the pairs of them that overlap — then every pair. The
//! measurement the readability lint is built on, without the engine or KiCAD
//! in the way.

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
                    && a.kind.is_pin_text()
                    && b.kind.is_pin_text();
                if !same_pin && a.bbox.overlaps(&b.bbox) {
                    let o = a.bbox.intersection(&b.bbox).expect("they overlap");
                    hits.push(format!(
                        "  {:?} {:?} x {:?} {:?} at ({:.2},{:.2}) by {:.2}x{:.2}",
                        a.kind,
                        a.text,
                        b.kind,
                        b.text,
                        o.center().x,
                        o.center().y,
                        o.width(),
                        o.height(),
                    ));
                }
            }
        }
        let extent = texts.iter().map(|t| t.bbox).reduce(|a, b| {
            geom::Rect::new(
                a.min_x.min(b.min_x),
                a.min_y.min(b.min_y),
                a.max_x.max(b.max_x),
                a.max_y.max(b.max_y),
            )
        });
        println!(
            "{path}\tparts={sheet_parts}\ttexts={}\tpairs={}\tper_part={:.3}\textent={}",
            texts.len(),
            hits.len(),
            hits.len() as f64 / sheet_parts.max(1) as f64,
            extent.map_or("none".into(), |r| format!(
                "{:.1}x{:.1} to ({:.1},{:.1})",
                r.width(),
                r.height(),
                r.max_x,
                r.max_y
            ))
        );
        for hit in &hits {
            println!("{hit}");
        }
        pairs += hits.len();
        parts += sheet_parts;
    }
    println!(
        "TOTAL\tparts={parts}\tpairs={pairs}\tper_part={:.3}",
        pairs as f64 / parts.max(1) as f64
    );
}
