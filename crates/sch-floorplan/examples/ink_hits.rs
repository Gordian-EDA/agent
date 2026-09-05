//! Every wire segment that runs over drawn INK — a symbol body, a pin's text, a
//! power symbol's value text, or a label — classified and located.
//!
//! ```text
//! cargo run --release -p sch-floorplan --example ink_hits -- <dir-or-sheet>…
//! ```
//!
//! `visual_facts` reports only the body class, and only as a net/refdes pair. This
//! is the same measurement widened to the text the router also cannot see, with the
//! offending endpoints, so a routing change can be judged hit by hit.

use std::collections::BTreeSet;
use std::path::PathBuf;

use geom::{EPS, Point2, Rect, Segment};
use sch_doc::SchDoc;
use sch_model::text::TextKind;

/// Which kind of ink a wire ran over.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    Body,
    PinText,
    PowerText,
    FieldText,
    Label,
}

impl Class {
    fn name(&self) -> &'static str {
        match self {
            Class::Body => "through-body",
            Class::PinText => "through-pin-text",
            Class::PowerText => "through-power-text",
            Class::FieldText => "through-field-text",
            Class::Label => "through-label",
        }
    }
}

fn main() {
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    assert!(!args.is_empty(), "usage: ink_hits <dir-or-sheet>…");
    let mut sheets: Vec<PathBuf> = Vec::new();
    for arg in args {
        match arg.is_dir() {
            true => sheets.extend(std::fs::read_dir(&arg).unwrap().flatten().map(|e| e.path())),
            false => sheets.push(arg),
        }
    }
    sheets.retain(|p| p.extension().is_some_and(|e| e == "kicad_sch"));
    sheets.sort();

    let mut totals = [0usize; 5];
    for path in &sheets {
        let doc = SchDoc::read(path).unwrap();
        let hits = hits(&doc);
        if hits.is_empty() {
            continue;
        }
        println!("{}", path.file_stem().unwrap().to_string_lossy());
        for (class, owner, what, net, a, b) in &hits {
            totals[*class as usize] += 1;
            println!(
                "  {:<19} {owner:<12} {what:<28} net={net:<20} ({:.2},{:.2})-({:.2},{:.2})",
                class.name(),
                a.x,
                a.y,
                b.x,
                b.y
            );
        }
    }
    println!(
        "TOTAL through-body={} through-pin-text={} through-power-text={} through-field-text={} through-label={}",
        totals[0], totals[1], totals[2], totals[3], totals[4]
    );
}

type Hit = (Class, String, String, String, Point2, Point2);

/// Wire segments running over ink that is not their own pin's symbol.
fn hits(doc: &SchDoc) -> Vec<Hit> {
    let pins = sch_doc::placed_pins(doc);
    let benched: BTreeSet<String> = doc
        .symbols()
        .filter(|s| sch_floorplan::bench::is_benched(s))
        .map(|s| s.refdes().to_string())
        .collect();
    let bodies: Vec<(String, Rect)> = doc
        .symbols()
        .filter(|s| !benched.contains(s.refdes()))
        .filter_map(|s| Some((s.refdes().to_string(), sch_doc::body_rect(doc, s)?)))
        .collect();
    let texts: Vec<sch_model::text::DrawnText> = sch_doc::drawn_texts(doc)
        .into_iter()
        .filter(|t| !t.owner.as_ref().is_some_and(|r| benched.contains(r)))
        .collect();

    // A segment may run over the ink of a symbol it CONNECTS to: that is the stub
    // leaving its own pin, which no router can avoid. `INK_HITS_ALL=1` drops the
    // exemption, to see what a route gives up for it.
    let exempt = std::env::var("INK_HITS_ALL").is_err();
    let owns = |seg: &Segment, refdes: &str| {
        exempt &&
        pins.iter()
            .filter(|p| p.refdes == refdes)
            .any(|p| p.at.dist(seg.a) < EPS || p.at.dist(seg.b) < EPS)
    };

    let mut out = Vec::new();
    for (a, b, net) in sch_doc::connect::scene(doc).segments {
        let seg = Segment::new(a, b);
        for (refdes, body) in &bodies {
            if refdes.starts_with('#') || owns(&seg, refdes) {
                continue;
            }
            if seg.axis_aligned_hits_rect_interior(body) {
                out.push((Class::Body, refdes.clone(), "body".into(), net.clone(), a, b));
            }
        }
        for text in &texts {
            let owner = text.owner.clone().unwrap_or_default();
            if owns(&seg, &owner) {
                continue;
            }
            if !seg.axis_aligned_hits_rect_interior(&text.bbox) {
                continue;
            }
            // A label rides the wire it names; that is the idiom, not a defect.
            if text.text == net {
                continue;
            }
            let class = match text.kind {
                TextKind::PinName | TextKind::PinNumber => Class::PinText,
                TextKind::Field if owner.starts_with('#') => Class::PowerText,
                TextKind::Field | TextKind::FreeText => Class::FieldText,
                TextKind::Label | TextKind::PortLabel => Class::Label,
            };
            out.push((
                class,
                owner,
                format!("{:?} {:?}", text.kind, text.text),
                net.clone(),
                a,
                b,
            ));
        }
    }
    out.sort_by(|x, y| (&x.0, &x.1, &x.2).cmp(&(&y.0, &y.1, &y.2)));
    out
}
