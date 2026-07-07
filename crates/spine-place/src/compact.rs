//! Node packing: the de-sprawl pass. Humans pack a sheet so the median wire
//! hop is ~5 mm and the bounding box is ~23× the summed part area; a typeset
//! layout accumulates air — channel allowances that went unused, margins around
//! small modules, fold slack. This pass slides whole SCENE NODES (modules keep
//! their internal geometry exactly) left then up until each rests against the
//! previously packed obstacle field, VLSI-legalization style. Row membership
//! and port alignment survive: the X pass never changes Y and vice versa; the
//! caller re-routes and A/B-gates the result.

use std::collections::BTreeMap;

use geom::Rect;
use sch_place::item::{Incidence, Item};

use crate::net::NetClass;

const GRID: f64 = 1.27;
/// Clearance kept between packed obstacle rects: a column gap's worth — the
/// strip estimates run ~a text-height of error, so a two-lane cushion collides.
const GAP: f64 = 5.08;

fn snap(v: f64) -> f64 {
    (v / GRID).round() * GRID
}

/// Nets whose CURRENT pin span exceeds the realizer's wire threshold — these
/// will carry labels, so their pins need name-width strips; wired pins need
/// only a lead. Doubles as the wire-first gate term: a layout change that
/// grows this count is trading wires for labels.
pub fn labeled_nets(
    items: &[Item],
    inc: &Incidence,
    classes: &BTreeMap<String, NetClass>,
) -> std::collections::BTreeSet<String> {
    const LABEL_SPAN: f64 = 40.0;
    let mut out = std::collections::BTreeSet::new();
    for (net, pins) in inc {
        if classes.get(net).is_some_and(|c| c.is_rail()) {
            continue;
        }
        let eps: Vec<[f64; 2]> = pins
            .iter()
            .filter_map(|(i, num)| {
                let it = &items[*i];
                it.geom.pins.iter().find(|p| &p.number == num).map(|p| {
                    let off = p.at.transform_offset(it.angle, it.mirror);
                    [it.at[0] + off[0], it.at[1] + off[1]]
                })
            })
            .collect();
        let span = eps
            .iter()
            .flat_map(|a| {
                eps.iter()
                    .map(move |b| (a[0] - b[0]).abs() + (a[1] - b[1]).abs())
            })
            .fold(0.0, f64::max);
        if span > LABEL_SPAN {
            out.insert(net.clone());
        }
    }
    out
}

/// An item's full obstacle rect: placed body+text rect, extended by the strips
/// the realizer draws at its pins (net labels on E/W pins, power glyphs and
/// short risers on N/S pins). Over-reserving only limits how tightly the pack
/// closes — never correctness.
fn obstacle(
    item: &Item,
    classes: &BTreeMap<String, NetClass>,
    labeled: &std::collections::BTreeSet<String>,
) -> Rect {
    let mut r = sch_floorplan::contract::item_rect(item, item.at);
    for (num, _name, net) in &item.pins {
        let Some(net) = net else { continue };
        let Some(pg) = item.geom.pins.iter().find(|p| &p.number == num) else {
            continue;
        };
        let off = pg.at.transform_offset(item.angle, item.mirror);
        let (px, py) = (item.at[0] + off[0], item.at[1] + off[1]);
        let class = *classes.get(net).unwrap_or(&NetClass::Signal);
        let connectorish = sch_place::netclass::is_connector_like(&item.part);
        let text = if class.is_rail() {
            7.62
        } else if labeled.contains(net.as_str()) || connectorish {
            crate::net::label_text_width(net)
        } else {
            2.54
        };
        // Strip direction from the pin's angle in WORLD terms: KiCAD pins point
        // INTO the body, so the strip extends the opposite way.
        let world = (pg.angle + item.angle).rem_euclid(360.0) as i64;
        let (dx, dy): (f64, f64) = match world {
            0 => (-1.0, 0.0),   // pin points east into body → strip west
            180 => (1.0, 0.0),
            90 => (0.0, 1.0),   // symbol-space up → sheet-space down strip
            _ => (0.0, -1.0),
        };
        let strip = Rect::new(
            px + dx.min(0.0) * text - 1.27,
            py + dy.min(0.0) * text - 1.27,
            px + dx.max(0.0) * text + 1.27,
            py + dy.max(0.0) * text + 1.27,
        );
        r = Rect::new(
            r.min_x.min(strip.min_x),
            r.min_y.min(strip.min_y),
            r.max_x.max(strip.max_x),
            r.max_y.max(strip.max_y),
        );
    }
    r
}

/// Union obstacle rect of a group's members.
fn group_rect(
    items: &[Item],
    group: &[usize],
    classes: &BTreeMap<String, NetClass>,
    labeled: &std::collections::BTreeSet<String>,
) -> Rect {
    let mut it = group.iter();
    let first = obstacle(&items[*it.next().expect("non-empty group")], classes, labeled);
    it.fold(first, |acc, &i| {
        let r = obstacle(&items[i], classes, labeled);
        Rect::new(
            acc.min_x.min(r.min_x),
            acc.min_y.min(r.min_y),
            acc.max_x.max(r.max_x),
            acc.max_y.max(r.max_y),
        )
    })
}

/// Pack groups left, then up. `groups` partitions all item indices; members of
/// a group translate together.
pub fn pack_nodes(
    items: &mut [Item],
    groups: &[Vec<usize>],
    inc: &Incidence,
    classes: &BTreeMap<String, NetClass>,
) {
    // Two rounds: packing shortens nets below the label threshold, and the
    // second round packs against their now-leaner strips.
    for _round in 0..2 {
        pack_once(items, groups, inc, classes);
    }
}

fn pack_once(
    items: &mut [Item],
    groups: &[Vec<usize>],
    inc: &Incidence,
    classes: &BTreeMap<String, NetClass>,
) {
    let labeled = labeled_nets(items, inc, classes);
    for axis in 0..2 {
        let mut rects: Vec<Rect> = groups
            .iter()
            .map(|grp| group_rect(items, grp, classes, &labeled))
            .collect();
        let mut order: Vec<usize> = (0..groups.len()).collect();
        order.sort_by(|&a, &b| {
            let (ka, kb) = if axis == 0 {
                (rects[a].min_x, rects[b].min_x)
            } else {
                (rects[a].min_y, rects[b].min_y)
            };
            ka.total_cmp(&kb).then(a.cmp(&b))
        });

        let mut placed: Vec<Rect> = Vec::new();
        for &gi in &order {
            let r = rects[gi];
            // How far this group can slide toward the origin: it rests against
            // the farthest-reaching placed rect it overlaps on the OTHER axis.
            let limit = placed
                .iter()
                .filter(|p| {
                    if axis == 0 {
                        r.min_y < p.max_y + GAP && p.min_y < r.max_y + GAP
                    } else {
                        r.min_x < p.max_x + GAP && p.min_x < r.max_x + GAP
                    }
                })
                .map(|p| if axis == 0 { p.max_x } else { p.max_y } + GAP)
                .fold(0.0_f64, f64::max);
            let cur = if axis == 0 { r.min_x } else { r.min_y };
            let shift = snap((cur - limit).max(0.0));
            if shift > 0.0 {
                for &i in &groups[gi] {
                    items[i].at[axis] -= shift;
                }
            }
            let packed = if axis == 0 {
                Rect::new(r.min_x - shift, r.min_y, r.max_x - shift, r.max_y)
            } else {
                Rect::new(r.min_x, r.min_y - shift, r.max_x, r.max_y - shift)
            };
            rects[gi] = packed;
            placed.push(packed);
        }
    }
}
