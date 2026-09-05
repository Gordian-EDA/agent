//! Audit drawn junction dots against KiCAD's rule on finished sheets.
//!
//! `cargo run -p sch-floorplan --example junction_audit -- <dir>` reads every
//! `.kicad_sch` in `<dir>` and reports, per sheet, how many dots sit where three
//! or more conductors meet (legitimate) versus fewer (spurious), plus the joins
//! that need a dot and lack one.

use std::collections::BTreeMap;
use std::path::PathBuf;

use geom::{Point2, Segment};
use sch_doc::{Item, SchDoc};

const EPS: f64 = 1e-6;

fn key(p: Point2) -> (i64, i64) {
    ((p[0] * 1000.0).round() as i64, (p[1] * 1000.0).round() as i64)
}

/// Conductors meeting at one sheet point.
#[derive(Default)]
struct Node {
    ends: usize,
    passes: usize,
    pins: usize,
    labels: usize,
}

impl Node {
    /// KiCAD's degree: one per wire end and pin tip, two per interior split.
    /// A label names the net without branching it, so it adds nothing.
    fn degree(&self) -> usize {
        self.ends + self.pins + 2 * self.passes
    }
    /// Conductors visible at the point, the metric the defect was measured with.
    fn conductors(&self) -> usize {
        self.ends + self.passes + self.pins + self.labels
    }
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: junction_audit <dir>"));
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "kicad_sch"))
        .collect();
    files.sort();
    let (mut t_dots, mut t_bad, mut t_good, mut t_thin, mut t_missing) = (0, 0, 0, 0, 0);
    let mut t_missing_t = 0;
    for path in &files {
        let doc = SchDoc::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut segs: Vec<Segment> = Vec::new();
        let mut dots: Vec<Point2> = Vec::new();
        let mut nodes: BTreeMap<(i64, i64), Node> = BTreeMap::new();
        for item in doc.items() {
            match item {
                Item::Wire(w) => {
                    for pair in w.points.windows(2) {
                        segs.push(Segment::new(pair[0], pair[1]));
                    }
                }
                Item::Junction(j) => dots.push(j.at),
                Item::Label(l) => nodes.entry(key(Point2::from([l.at.x, l.at.y]))).or_default().labels += 1,
                _ => {}
            }
        }
        for s in &segs {
            for e in [s.a, s.b] {
                nodes.entry(key(e)).or_default().ends += 1;
            }
        }
        for p in sch_doc::placed_pins(&doc) {
            nodes.entry(key(p.at)).or_default().pins += 1;
        }
        let interior = |s: &Segment, p: Point2| {
            s.contains_point(p)
                && (p[0] - s.a[0]).abs() + (p[1] - s.a[1]).abs() > EPS
                && (p[0] - s.b[0]).abs() + (p[1] - s.b[1]).abs() > EPS
        };
        let pts: Vec<((i64, i64), Point2)> = nodes
            .keys()
            .map(|k| (*k, Point2::from([k.0 as f64 / 1000.0, k.1 as f64 / 1000.0])))
            .collect();
        for (k, p) in pts {
            let passes = segs.iter().filter(|s| interior(s, p)).count();
            nodes.get_mut(&k).unwrap().passes = passes;
        }
        let empty = Node::default();
        let at = |p: Point2| nodes.get(&key(p)).unwrap_or(&empty);
        let bad = dots.iter().filter(|d| at(**d).degree() < 3).count();
        let good = dots.len() - bad;
        let thin = dots.iter().filter(|d| at(**d).conductors() < 3).count();
        let dotted: std::collections::BTreeSet<(i64, i64)> = dots.iter().map(|p| key(*p)).collect();
        let undotted: Vec<(&(i64, i64), &Node)> = nodes
            .iter()
            .filter(|(k, n)| n.degree() >= 3 && !dotted.contains(k))
            .collect();
        let missing = undotted.len();
        // The subclass that changes what the netlist reads as: a wire END sitting on
        // another wire's interior, undotted.
        let missing_t = undotted
            .iter()
            .filter(|(_, n)| n.ends >= 1 && n.passes >= 1)
            .count();
        if std::env::var("JUNCTION_AUDIT_VERBOSE").is_ok() {
            for (k, n) in &undotted {
                println!(
                    "  undotted ({:.2},{:.2}) ends={} passes={} pins={} labels={}",
                    k.0 as f64 / 1000.0,
                    k.1 as f64 / 1000.0,
                    n.ends,
                    n.passes,
                    n.pins,
                    n.labels
                );
            }
        }
        let name = path.file_stem().unwrap().to_string_lossy();
        println!(
            "{name}: dots={} deg_lt3={bad} deg_ge3={good} cond_lt3={thin} missing={missing} missing_T={missing_t}",
            dots.len()
        );
        t_dots += dots.len();
        t_bad += bad;
        t_good += good;
        t_thin += thin;
        t_missing += missing;
        t_missing_t += missing_t;
    }
    println!(
        "TOTAL: dots={t_dots} deg_lt3={t_bad} deg_ge3={t_good} cond_lt3={t_thin} missing={t_missing} missing_T={t_missing_t}"
    );
}
