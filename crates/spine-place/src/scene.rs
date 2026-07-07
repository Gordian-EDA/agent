//! Scene assembly: the placeable units after module formation. A scene node is
//! either a typeset MODULE (anchor + satellites, rigid) or a free CHAIN RUN (an
//! unconsumed chain's parts, typeset as one horizontal series run or one
//! vertical rail ladder). Ordering and coordinate assignment move scene nodes;
//! items inside a node keep their relative offsets.

use std::collections::BTreeMap;

use geom::Point2;
use sch_place::ir::Orient;
use sch_place::item::Item;

use crate::chain::{Chain, ChainRole, NodeKind, Reduced};
use crate::module::{ModuleForm, SatPlace, orient_for, pin_offset};
use crate::net::NetClass;

const GRID: f64 = 1.27;
const LEAD: f64 = 2.54;

fn snap(v: f64) -> f64 {
    (v / GRID).round() * GRID
}

/// One placeable unit.
#[derive(Debug)]
pub struct SceneNode {
    /// Items with offsets relative to the node origin.
    pub places: Vec<SatPlace>,
    /// Content envelope around the origin.
    pub env_min: Point2,
    pub env_max: Point2,
    /// Reduced-graph chains whose terminals attach to this node, with the
    /// world-offset of the attaching pin (for port-aware ordering/alignment).
    pub ports: Vec<Port>,
    /// Module anchor item (None for free chain runs).
    pub anchor: Option<usize>,
    /// A strap islet: belongs in the dedicated strap column at arrange time.
    pub strap: bool,
}

/// A chain terminal's attachment on a scene node.
#[derive(Debug, Clone)]
pub struct Port {
    pub chain: usize,
    /// Offset of the attaching pin endpoint relative to the node origin.
    pub at: Point2,
    /// True when this port is chain terminal `a` (else terminal `b`).
    pub is_a: bool,
}

/// The scene: nodes plus which scene node each chain terminal maps to.
#[derive(Debug, Default)]
pub struct Scene {
    pub nodes: Vec<SceneNode>,
    /// chain index → (scene node of terminal a, scene node of terminal b).
    /// None when the terminal is a rail (rails are realized, not placed) or the
    /// chain lives inside a module.
    pub ends: BTreeMap<usize, (Option<usize>, Option<usize>)>,
}

/// Typeset one free chain as a run: series chains run horizontally (in-line
/// parts, corpus law), rail-touching chains run vertically (ladder). Offsets are
/// relative to the FIRST pin endpoint of the run (the node origin). Outer nets
/// known to LABEL (pass-2 knowledge) extend the envelope by their text width so
/// neighbours never sit on the pennant.
fn typeset_chain_run(
    items: &[Item],
    chain: &Chain,
    classes: &BTreeMap<String, NetClass>,
    labeled: Option<&std::collections::BTreeSet<String>>,
) -> SceneNode {
    let vertical = chain.role(classes) != ChainRole::Series;
    let mut places = Vec::new();
    let mut cursor = Point2::new(0.0, 0.0);
    for (k, &p) in chain.parts.iter().enumerate() {
        let item = &items[p];
        let dir = if vertical {
            Orient::Down
        } else {
            Orient::Right
        };
        let angle = orient_for(item, &chain.nets[k], dir);
        let entry = item
            .pins
            .iter()
            .find(|(_, _, n)| n.as_deref() == Some(chain.nets[k].as_str()))
            .map(|(num, _, _)| num.clone())
            .unwrap_or_default();
        let exit = item
            .pins
            .iter()
            .find(|(_, _, n)| n.as_deref() == Some(chain.nets[k + 1].as_str()))
            .map(|(num, _, _)| num.clone())
            .unwrap_or_default();
        let e_off = pin_offset(item, &entry, angle);
        let x_off = pin_offset(item, &exit, angle);
        let origin = Point2::new(snap(cursor.x - e_off.x), snap(cursor.y - e_off.y));
        places.push(SatPlace {
            item: p,
            offset: origin,
            angle,
        });
        let step = if vertical {
            (x_off.y - e_off.y).abs() + LEAD
        } else {
            (x_off.x - e_off.x).abs() + LEAD
        };
        if vertical {
            cursor.y += step;
        } else {
            cursor.x += step;
        }
    }
    let mut node = SceneNode {
        places,
        env_min: Point2::new(0.0, 0.0),
        env_max: Point2::new(0.0, 0.0),
        ports: Vec::new(),
        anchor: None,
        strap: false,
    };
    envelope(items, &mut node);
    if let Some(labeled) = labeled {
        for (net, at_start) in [
            (&chain.nets[0], true),
            (&chain.nets[chain.nets.len() - 1], false),
        ] {
            if !labeled.contains(net.as_str()) {
                continue;
            }
            let text = crate::net::label_text_width(net);
            if vertical {
                if at_start {
                    node.env_min.y -= text.min(12.7);
                } else {
                    node.env_max.y += text.min(12.7);
                }
            } else if at_start {
                node.env_min.x -= text;
            } else {
                node.env_max.x += text;
            }
        }
    }
    node
}

/// Recompute a node's envelope from its placed items' FULL rects (body + text).
pub fn envelope(items: &[Item], node: &mut SceneNode) {
    let (mut min, mut max) = (
        Point2::new(f64::MAX, f64::MAX),
        Point2::new(f64::MIN, f64::MIN),
    );
    for s in &node.places {
        let r = crate::module::placed_rect(items, s);
        min.x = min.x.min(r.min_x);
        min.y = min.y.min(r.min_y);
        max.x = max.x.max(r.max_x);
        max.y = max.y.max(r.max_y);
    }
    if min.x > max.x {
        (min, max) = (Point2::new(0.0, 0.0), Point2::new(0.0, 0.0));
    }
    node.env_min = min;
    node.env_max = max;
}

/// Build the scene from module formation output plus the leftover chains.
pub fn build_scene(
    items: &[Item],
    g: &Reduced,
    form: ModuleForm,
    classes: &BTreeMap<String, NetClass>,
    labeled: Option<&std::collections::BTreeSet<String>>,
) -> Scene {
    let mut scene = Scene::default();

    // Modules become scene nodes 1:1 (anchor at the node origin).
    let strap_items = form.strap_items.clone();
    let mut node_of_anchor: BTreeMap<usize, usize> = BTreeMap::new();
    for m in form.modules {
        node_of_anchor.insert(m.anchor, scene.nodes.len());
        scene.nodes.push(SceneNode {
            places: {
                let mut v = vec![SatPlace {
                    item: m.anchor,
                    offset: Point2::new(0.0, 0.0),
                    angle: 0.0,
                }];
                v.extend(m.sats);
                v
            },
            env_min: m.env_min,
            env_max: m.env_max,
            ports: Vec::new(),
            anchor: Some(m.anchor),
            strap: strap_items.contains(&m.anchor),
        });
    }

    // Free chains with parts become chain-run nodes; remember terminal mapping.
    let mut run_of_chain: BTreeMap<usize, usize> = BTreeMap::new();
    for (ci, c) in g.chains.iter().enumerate() {
        if c.parts.is_empty() || form.consumed.contains_key(&ci) {
            continue;
        }
        run_of_chain.insert(ci, scene.nodes.len());
        scene
            .nodes
            .push(typeset_chain_run(items, c, classes, labeled));
    }

    // Terminal → scene node resolution + port registration.
    for (ci, c) in g.chains.iter().enumerate() {
        let resolve = |t: &crate::chain::Terminal| -> Option<usize> {
            match &g.nodes[t.node] {
                NodeKind::Part(i) => node_of_anchor.get(i).copied(),
                NodeKind::Rail(_) => None,
                NodeKind::Junction(_) => None,
            }
        };
        let (na, nb) = (resolve(&c.a), resolve(&c.b));
        if form.consumed.contains_key(&ci) {
            // Module-internal — no ports, no run. But a TAIL-style consumption
            // must keep its terminal mapping: the junction's arrange-edge
            // through the consumed part is what holds the junction's OTHER
            // chains in the module's neighborhood — sever it and they strand
            // in the first column (the hollow-middle audio bug). Junction-to-
            // junction tails register as (None, None); the edge builder still
            // reads both g-terminals. Only the both-ends-same-module case
            // stays hidden (a bridge is genuinely internal).
            if na != nb || na.is_none() {
                scene.ends.insert(ci, (na, nb));
            }
            continue;
        }
        // Ports on module nodes: the anchor pin's world offset at angle 0.
        for (t, n, is_a) in [(&c.a, na, true), (&c.b, nb, false)] {
            if let (Some(n), false) = (n, t.pin.is_empty())
                && let Some(anchor) = scene.nodes[n].anchor
            {
                let at = pin_offset(&items[anchor], &t.pin, 0.0);
                scene.nodes[n].ports.push(Port {
                    chain: ci,
                    at,
                    is_a,
                });
            }
        }
        // Ports on a chain-run node itself (its two ends).
        if let Some(rn) = run_of_chain.get(&ci).copied() {
            let node = &scene.nodes[rn];
            let (first, last) = (node.places.first(), node.places.last());
            if let (Some(f), Some(l)) = (first, last) {
                let fe = chain_end_offset(items, c, f, true);
                let le = chain_end_offset(items, c, l, false);
                scene.nodes[rn].ports.push(Port {
                    chain: ci,
                    at: fe,
                    is_a: true,
                });
                scene.nodes[rn].ports.push(Port {
                    chain: ci,
                    at: le,
                    is_a: false,
                });
            }
        }
        scene.ends.insert(ci, (na, nb));
    }
    scene
}

/// World offset (within the run node) of a chain run's outer pin at end a/b.
fn chain_end_offset(items: &[Item], c: &Chain, place: &SatPlace, a_end: bool) -> Point2 {
    let net = if a_end {
        &c.nets[0]
    } else {
        &c.nets[c.nets.len() - 1]
    };
    let item = &items[place.item];
    let pin = item
        .pins
        .iter()
        .find(|(_, _, n)| n.as_deref() == Some(net.as_str()))
        .map(|(num, _, _)| num.clone())
        .unwrap_or_default();
    let off = pin_offset(item, &pin, place.angle);
    Point2::new(place.offset.x + off.x, place.offset.y + off.y)
}
