//! Turning a pair of labels back into the wire it stands for.
//!
//! A generator that cannot route reaches for a local label at both ends and
//! calls the net connected. It is connected, and it reads as two parts floating
//! in space. Where the two ends are close enough that a person would have drawn
//! the wire, this draws it: delete the pair, route between the anchors, and keep
//! the result only if the netlist is untouched.

use std::collections::HashMap;

use geom::Point2;
use sch_doc::{LabelKind, SchDoc};

use crate::drag::{DragError, partition, settle_junctions};
use crate::route::{self, Obstacles};
use crate::sheet::{NodeKey, Sheet, key, point_of};

/// A local label pair that is standing in for a wire.
#[derive(Debug, Clone)]
pub struct Substitute {
    pub name: String,
    pub ends: [Point2; 2],
}

/// Every pair of local labels that names a net *and nothing else does*, so
/// deleting the pair and drawing the wire says exactly what they said.
///
/// A power symbol, a global or hierarchical label, or a third use of the name
/// all mean the name is carrying more than this one connection; those are left
/// alone.
pub fn substitutes(sheet: &Sheet) -> Vec<Substitute> {
    let mut anchors: HashMap<&str, Vec<NodeKey>> = HashMap::new();
    for (node, name) in &sheet.label_names {
        if !sheet.local_labels.contains(node) {
            continue;
        }
        anchors.entry(name.as_str()).or_default().push(*node);
    }
    let mut found: Vec<Substitute> = anchors
        .into_iter()
        .filter(|(_, nodes)| nodes.len() == 2)
        .filter(|(name, nodes)| {
            nodes
                .iter()
                .all(|node| sheet.node_net.get(node).map(String::as_str) == Some(*name))
        })
        .map(|(name, nodes)| Substitute {
            name: name.to_string(),
            ends: [point_of(&nodes[0]), point_of(&nodes[1])],
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// The direction a wire should leave an anchor: away from the pin it sits on,
/// or along the wire already there.
fn leaving(sheet: &Sheet, at: Point2) -> Point2 {
    if let Some(pins) = sheet.pins_at.get(&key(at))
        && let Some(pin) = pins.first()
    {
        return sheet.pins[*pin].out;
    }
    match sheet.incident.get(&key(at)).and_then(|w| w.first()) {
        Some(index) => {
            let wire = &sheet.wires[*index];
            let far = if key(wire.a) == key(at) {
                wire.b
            } else {
                wire.a
            };
            let d = Point2::new(at.x - far.x, at.y - far.y);
            let len = d.x.hypot(d.y).max(geom::EPS);
            Point2::new(d.x / len, d.y / len)
        }
        None => Point2::new(1.0, 0.0),
    }
}

/// Replace a label pair with the wire it stands for.
///
/// Refused — and rolled back — when the two ends are far enough apart or the
/// route bent enough that a person would have reached for the label too, and
/// whenever the netlist would not come out identical.
pub fn promote(
    doc: &mut SchDoc,
    substitute: &Substitute,
    before: &Sheet,
) -> Result<usize, DragError> {
    let [a, b] = substitute.ends;
    let backup = doc.clone();
    let was = partition(before);

    let labels: Vec<String> = doc
        .labels()
        .filter(|l| l.kind == LabelKind::Local && l.text == substitute.name)
        .map(|l| l.uuid.clone())
        .collect();
    doc.remove_drawing(&labels);

    let sheet = Sheet::of(doc);
    let net = sheet.net_at(a).unwrap_or_default().to_string();
    let obstacles = Obstacles::new(&sheet);
    let out = leaving(&sheet, a);
    let path = route::route(&obstacles, a, out, b, &net)
        .or_else(|| route::maze(&obstacles, a, out, b, &net));

    // A wire only reads better than a name while it stays short and straight.
    let acceptable = path.as_ref().is_some_and(|p| {
        let length: f64 = p.windows(2).map(|w| w[0].manhattan(w[1])).sum();
        p.len() <= 4 && length <= 2.0 * a.manhattan(b) + 5.08
    });
    let Some(path) = path.filter(|_| acceptable) else {
        *doc = backup;
        return Err(DragError::Doc(format!(
            "no wire worth drawing for {}",
            substitute.name
        )));
    };

    let mut touched = std::collections::HashSet::new();
    for pair in path.windows(2) {
        doc.add_wire(pair[0], pair[1]);
    }
    touched.extend(path.iter().map(|p| key(*p)));
    let drawn = Sheet::of(doc);
    settle_junctions(doc, &drawn, &touched);

    let after = Sheet::of(doc);
    if partition(&after) != was {
        *doc = backup;
        return Err(DragError::Truthfulness(vec![substitute.name.clone()]));
    }
    Ok(path.len() - 1)
}
