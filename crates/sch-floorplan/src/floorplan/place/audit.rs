//! `place::audit` — the realiser's net-occupancy check.
//!
//! Every wire, pin, label anchor and junction the realiser draws belongs to exactly one
//! net. KiCAD's netlister welds anything that shares a point, so the drawing is truthful
//! only while no point is shared by two nets. This module reads that property back off a
//! finished [`SchematicWriter`] and names the offending pairs, so a short is a failing
//! assertion here rather than a refused `place_parts` a whole engine-run later.
//!
//! The question is only "do two NETS meet". A wire drawn onto a no-connect pin is a
//! separate defect (KiCAD reports it as `no_connect_connected`) with no second net to
//! name, and belongs to the ERC gate rather than here.

use std::collections::BTreeSet;

use geom::{EPS, Point2, Segment};
use kicad::KicadInstallation;
use sch_model::item::{Incidence, Item};

use crate::write::SchematicWriter;

/// How two nets came to share a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictKind {
    /// Two connection points (pin tips, power-symbol pins, label anchors) coincide.
    Terminals,
    /// A connection point of one net lies on another net's wire.
    TerminalOnWire,
    /// Two nets' wires run along the same line over a shared stretch.
    Overlap,
    /// Two nets' wires meet at a point that ends at least one of them.
    WireTouch,
    /// A junction dot sits where two nets' wires pass, welding them.
    Junction,
}

/// Two nets the realiser drew onto one point.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NetConflict {
    /// The two net names, sorted, so the same defect reports identically.
    pub nets: (String, String),
    /// Where they meet, to 1 µm.
    pub at: (i64, i64),
    pub kind: ConflictKind,
}

impl std::fmt::Display for NetConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?}: {} + {} at [{:.2},{:.2}]",
            self.kind,
            self.nets.0,
            self.nets.1,
            self.at.0 as f64 / 1000.0,
            self.at.1 as f64 / 1000.0
        )
    }
}

/// Every connection point the realiser drew, tagged with its net: component pin tips,
/// power-symbol pins, and label anchors (a label binds the point it sits on).
fn terminals(
    env: &KicadInstallation,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
) -> Vec<(Point2, String)> {
    let mut out = Vec::new();
    for (net, pins) in inc {
        for (i, num) in pins {
            let Ok(eps) = w.pin_dirs(env, &items[*i].refdes, num) else {
                continue;
            };
            out.extend(eps.into_iter().map(|(ep, _)| (ep.into(), net.clone())));
        }
    }
    out.extend(w.power_pins().into_iter().map(|(p, n)| (p.into(), n)));
    out.extend(w.label_anchors().into_iter().map(|(p, n)| (p.into(), n)));
    out
}

/// Every point two different nets share on a finished sheet, sorted and deduplicated.
///
/// Empty is the invariant: a non-empty result is a short the netlist will show, whatever
/// the sheet looks like.
pub fn net_conflicts(
    env: &KicadInstallation,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
) -> Vec<NetConflict> {
    let terminals = terminals(env, w, items, inc);
    // Every wire the realiser draws carries its net, so the occupancy model is total.
    let wires: Vec<(Segment, String)> = w
        .wires_with_nets()
        .into_iter()
        .filter_map(|seg| Some((seg.segment, seg.net?)))
        .collect();
    let junctions = w.junction_positions();
    let mut found: BTreeSet<NetConflict> = BTreeSet::new();
    let mut note = |a: &str, b: &str, at: Point2, kind: ConflictKind| {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        found.insert(NetConflict {
            nets: (lo.to_string(), hi.to_string()),
            at: (
                (at.x * 1000.0).round() as i64,
                (at.y * 1000.0).round() as i64,
            ),
            kind,
        });
    };

    for i in 0..terminals.len() {
        for j in (i + 1)..terminals.len() {
            let (p, a) = &terminals[i];
            let (q, b) = &terminals[j];
            if a != b && p.near_eq(*q, EPS) {
                note(a, b, *p, ConflictKind::Terminals);
            }
        }
    }
    for (p, net) in &terminals {
        for (seg, wnet) in &wires {
            if wnet != net && seg.contains_point(*p) {
                note(net, wnet, *p, ConflictKind::TerminalOnWire);
            }
        }
    }
    for i in 0..wires.len() {
        for j in (i + 1)..wires.len() {
            let (s, a) = &wires[i];
            let (t, b) = &wires[j];
            if a == b {
                continue;
            }
            if s.axis_aligned_collinear_overlap(*t) {
                note(a, b, s.a, ConflictKind::Overlap);
            } else if let Some(p) = touching_point(*s, *t) {
                note(a, b, p, ConflictKind::WireTouch);
            }
        }
    }
    for jp in &junctions {
        let at = Point2::from(*jp);
        let nets: BTreeSet<&str> = wires
            .iter()
            .filter(|(seg, _)| seg.contains_point(at))
            .map(|(_, net)| net.as_str())
            .collect();
        let mut nets = nets.into_iter();
        if let (Some(a), Some(b)) = (nets.next(), nets.next()) {
            note(a, b, at, ConflictKind::Junction);
        }
    }
    found.into_iter().collect()
}

/// Where two non-collinear segments meet, when the meeting point ENDS at least one of
/// them — the geometry KiCAD connects without a junction. A pure interior crossing
/// returns `None`: those wires only cross over (a dot there is [`ConflictKind::Junction`]).
fn touching_point(s: Segment, t: Segment) -> Option<Point2> {
    [s.a, s.b]
        .into_iter()
        .find(|&p| t.contains_point(p))
        .or_else(|| [t.a, t.b].into_iter().find(|&p| s.contains_point(p)))
}
