//! `frame_facts` — what the block frames on a finished sheet actually do.
//!
//! Reports, per `.kicad_sch` in a directory: how many frames are drawn, how many pairs
//! of them overlap, how many drawn texts a frame BORDER cuts through, the paper, and how
//! much of the hull the frames span is left empty between them.
//!
//! ```text
//! cargo run --release -p sch-floorplan --example frame_facts -- <dir>
//! ```

use std::path::PathBuf;

use geom::Rect;
use sch_doc::{Item, SchDoc};
use sch_model::text::TextKind;

fn frames(doc: &SchDoc) -> Vec<Rect> {
    doc.items()
        .iter()
        .filter_map(|item| match item {
            Item::Rectangle(r) => Some(Rect::from_points(r.start, r.end)),
            _ => None,
        })
        .collect()
}

/// A text the border of `frame` runs through: it meets the rectangle but is not wholly
/// inside it, so the dashed line crosses the glyphs.
fn cut(frame: &Rect, b: &Rect) -> bool {
    let inside = b.min_x >= frame.min_x
        && b.max_x <= frame.max_x
        && b.min_y >= frame.min_y
        && b.max_y <= frame.max_y;
    frame.intersection(b).is_some() && !inside
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: frame_facts <dir>"));
    let detail = std::env::args().any(|a| a == "--detail");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let n = e.ok()?.file_name().into_string().ok()?;
            n.strip_suffix(".kicad_sch").map(str::to_string)
        })
        .collect();
    names.sort();
    let (mut tf, mut to, mut tc, mut tr) = (0usize, 0usize, 0usize, 0usize);
    for name in &names {
        let doc = SchDoc::read(dir.join(format!("{name}.kicad_sch"))).unwrap();
        let fr = frames(&doc);
        let mut over = 0;
        for i in 0..fr.len() {
            for j in (i + 1)..fr.len() {
                if fr[i].overlaps(&fr[j]) {
                    over += 1;
                }
            }
        }
        let rails: std::collections::BTreeSet<String> = doc
            .symbols()
            .filter(|s| s.refdes().starts_with('#'))
            .map(|s| s.refdes().to_string())
            .collect();
        let (mut cuts, mut railcuts) = (0, 0);
        for t in sch_doc::drawn_texts(&doc) {
            let rail = t.owner.as_ref().is_some_and(|o| rails.contains(o))
                && t.kind == TextKind::Field;
            if !rail && !matches!(t.kind, TextKind::Label | TextKind::PortLabel) {
                continue;
            }
            if let Some(f) = fr.iter().find(|f| cut(f, &t.bbox)) {
                cuts += 1;
                if rail {
                    railcuts += 1;
                }
                if detail {
                    println!("  cut {name}: {:?} {:?} box={:?} frame={f:?}", t.kind, t.text, t.bbox);
                }
            }
        }
        let hull = fr.iter().copied().reduce(|a, b| {
            Rect::new(
                a.min_x.min(b.min_x),
                a.min_y.min(b.min_y),
                a.max_x.max(b.max_x),
                a.max_y.max(b.max_y),
            )
        });
        let ink: f64 = fr.iter().map(|f| f.width() * f.height()).sum();
        let hull_area = hull.map_or(0.0, |h| h.width() * h.height());
        let page = doc
            .page()
            .map(|p| format!("{:.0}x{:.0}", p[0], p[1]))
            .unwrap_or_else(|| "?".into());
        tf += fr.len();
        to += over;
        tc += cuts;
        tr += railcuts;
        println!(
            "{name}: frames={} overlap_pairs={over} cut_texts={cuts} (rail={railcuts}) \
             page={page} frame_area={ink:.0} hull={hull_area:.0} free={:.0}%",
            fr.len(),
            if hull_area > 0.0 {
                100.0 * (1.0 - ink / hull_area)
            } else {
                0.0
            },
        );
    }
    println!("TOTAL frames={tf} overlap_pairs={to} cut_texts={tc} rail_cuts={tr}");
}
