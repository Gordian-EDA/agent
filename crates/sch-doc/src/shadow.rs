//! Ink a coincident power symbol already puts on the sheet.

use std::collections::{BTreeMap, BTreeSet};

use geom::Point2;

use crate::{Label, LabelKind, SchDoc};

fn key(p: Point2) -> (i64, i64) {
    ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64)
}

/// The nets a power port drives, by the point its pin sits on.
fn ports(doc: &SchDoc) -> BTreeMap<(i64, i64), BTreeSet<String>> {
    let value: BTreeMap<&str, &str> = doc
        .symbols()
        .map(|symbol| (symbol.uuid.as_str(), symbol.value()))
        .collect();
    let mut out: BTreeMap<(i64, i64), BTreeSet<String>> = BTreeMap::new();
    for pin in crate::placed_pins(doc).into_iter().filter(|p| p.power_symbol) {
        if let Some(net) = value.get(pin.owner.as_str()) {
            out.entry(key(pin.at)).or_default().insert(net.to_string());
        }
    }
    out
}

/// The labels a same-named power symbol seated at the very same point already says.
///
/// A power port names its node globally, so a label printed on its pin prints one
/// name twice at one node. The exception is the label that BRIDGES: a *local* label
/// merges only with same-named local labels, never with a global rail, so when the
/// sheet names that net locally somewhere else, one coincident local label has to
/// stay to weld the local group onto the rail. Dropping the rest is netlist-neutral
/// by construction.
pub fn power_shadowed_labels(doc: &SchDoc) -> Vec<String> {
    let ports = ports(doc);
    let covered = |l: &Label| {
        ports
            .get(&key(l.at.point()))
            .is_some_and(|nets| nets.contains(&l.text))
    };
    let mut by_text: BTreeMap<&str, Vec<&Label>> = BTreeMap::new();
    for label in doc.labels() {
        by_text.entry(label.text.as_str()).or_default().push(label);
    }
    let mut out = Vec::new();
    for labels in by_text.into_values() {
        let mut on_port: Vec<&Label> = labels.iter().copied().filter(|l| covered(l)).collect();
        if on_port.is_empty() {
            continue;
        }
        on_port.sort_by(|a, b| {
            let at = |l: &Label| (key(l.at.point()), l.uuid.clone());
            at(a).cmp(&at(b))
        });
        let needs_bridge = labels
            .iter()
            .any(|l| l.kind == LabelKind::Local && !covered(l));
        let mut bridged = !needs_bridge;
        for label in on_port {
            if !bridged && label.kind == LabelKind::Local {
                bridged = true;
                continue;
            }
            out.push(label.uuid.clone());
        }
    }
    out
}
