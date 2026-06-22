//! Compile a board-DSL [`BoardDesign`] down to the engine's [`BoardDraft`].
//!
//! The DSL ([`board_lang`]) is the agent-facing source of truth; this is the
//! one-way lowering into the imperative draft the place/route/export pipeline
//! already consumes. Geometry-free in → the engine still computes coordinates
//! and copper. (The reverse — `.kicad_pcb` → DSL — lives in the import path.)

use board_lang::model as bl;
use pcb_engine::placement::{Edge, GroupHint, LockedAt, PlacementHints, Rect};
use pcb_engine::problem::{Bounds, Point2};

use crate::tools_pcb::{BoardDraft, DraftPart, DraftRules, PourSpec};

/// Number of segments used to approximate a `circle` outline as a polygon.
const CIRCLE_SEGMENTS: usize = 48;

/// Lower a parsed, validated [`BoardDesign`] into a [`BoardDraft`]. Total — the
/// DSL is already structurally validated by `board_lang::compile`, so this
/// never fails; footprint/pad correctness is the draft pipeline's job.
pub fn design_to_draft(d: &bl::BoardDesign) -> BoardDraft {
    let (bounds, outline) = outline_to_bounds(&d.board.outline);

    let r = &d.board.rules;
    let rules = DraftRules {
        clearance: r.clearance,
        min_trace_width: r.trace_width,
        via_diameter: r.via_diameter,
        via_drill: r.via_drill,
        layer_count: d.board.layers,
        net_widths: r.net_widths.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        pours: r
            .pours
            .iter()
            .map(|p| PourSpec {
                net: p.net.clone(),
                layer: p.layer.clone(),
            })
            .collect(),
    };

    // Parts, plus the per-part edge/corner flags hoisted into the hint lists.
    let mut parts = Vec::with_capacity(d.parts.len());
    let mut edge_seek = Vec::new();
    let mut corner_seek = Vec::new();
    for (refdes, p) in &d.parts {
        parts.push(DraftPart {
            reference: refdes.clone(),
            footprint: p.footprint.clone(),
            pad_nets: p.pads.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            locked: p.lock.map(|l| LockedAt {
                at: Point2 { x: l.x, y: l.y },
                rotation: l.rot,
            }),
        });
        if p.edge {
            edge_seek.push(refdes.clone());
        }
        if p.corner {
            corner_seek.push(refdes.clone());
        }
    }

    let groups = d
        .groups
        .iter()
        .map(|(name, g)| GroupHint {
            name: name.clone(),
            members: g.members.clone(),
            // DSL region is [min_x, min_y, max_x, max_y]; Rect is {min_x,max_x,min_y,max_y}.
            region: g.region.map(|r| Rect {
                min_x: r[0],
                min_y: r[1],
                max_x: r[2],
                max_y: r[3],
            }),
            edge: g.edge.as_deref().and_then(parse_edge),
            grid: g.grid,
            surround: g.surround.clone(),
        })
        .collect();

    BoardDraft {
        bounds,
        rules,
        parts,
        keepouts: Vec::new(),
        hints: PlacementHints {
            groups,
            edge_seek,
            corner_seek,
        },
        last_placement: None,
        last_place_illegal: false,
        outline,
    }
}

/// Map a DSL [`bl::Outline`] to the draft's `(bounds, outline)` pair. A rect has
/// no custom polygon (the rectangular `bounds` IS the outline); a circle and a
/// polygon both yield a polygon plus its bounding-box `bounds`.
fn outline_to_bounds(outline: &bl::Outline) -> (Bounds, Option<Vec<Point2>>) {
    match outline {
        bl::Outline::Rect { w, h } => (
            Bounds {
                min_x: 0.0,
                max_x: *w,
                min_y: 0.0,
                max_y: *h,
            },
            None,
        ),
        bl::Outline::Circle { r } => {
            // Centre the circle in [0,2r]×[0,2r] and tessellate it.
            let pts: Vec<Point2> = (0..CIRCLE_SEGMENTS)
                .map(|i| {
                    let a = std::f64::consts::TAU * (i as f64) / (CIRCLE_SEGMENTS as f64);
                    Point2 {
                        x: r + r * a.cos(),
                        y: r + r * a.sin(),
                    }
                })
                .collect();
            (
                Bounds {
                    min_x: 0.0,
                    max_x: 2.0 * r,
                    min_y: 0.0,
                    max_y: 2.0 * r,
                },
                Some(pts),
            )
        }
        bl::Outline::Polygon(p) => {
            let mut b = Bounds {
                min_x: f64::INFINITY,
                max_x: f64::NEG_INFINITY,
                min_y: f64::INFINITY,
                max_y: f64::NEG_INFINITY,
            };
            for (x, y) in p {
                b.min_x = b.min_x.min(*x);
                b.max_x = b.max_x.max(*x);
                b.min_y = b.min_y.min(*y);
                b.max_y = b.max_y.max(*y);
            }
            let pts = p.iter().map(|(x, y)| Point2 { x: *x, y: *y }).collect();
            (b, Some(pts))
        }
    }
}

fn parse_edge(s: &str) -> Option<Edge> {
    match s {
        "n" => Some(Edge::N),
        "s" => Some(Edge::S),
        "e" => Some(Edge::E),
        "w" => Some(Edge::W),
        _ => None,
    }
}

/// Round to 2 decimals so imported coordinates read cleanly in YAML.
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Import an existing `.kicad_pcb` into a [`bl::BoardDesign`] — the round-trip
/// companion to the DSL exporter, so the agent can start from a given board.
///
/// Recovers each part's reference + footprint lib_id + pad→net, the layer count,
/// and the outline bbox. Parts are emitted with an explicit `lock` at their
/// existing position (shifted so the board origin is 0,0), so the imported board
/// reproduces the original LAYOUT — the agent unlocks a part (drops its `lock:`)
/// to let the engine re-place it. Mirrors the schematic `lift` contract:
/// connectivity round-trips exactly; design rules default (a `.kicad_pcb` doesn't
/// surface board clearance/width in this kiutils version) and a non-rectangular
/// outline is approximated by its bbox for now.
pub fn import_to_design(path: &std::path::Path) -> std::io::Result<bl::BoardDesign> {
    let b = kicad_bridge::pcb::read_board(path)?;
    let (ox, oy) = (b.bounds.min_x, b.bounds.min_y);

    let mut parts = indexmap::IndexMap::new();
    for ip in &b.parts {
        let mut pads = indexmap::IndexMap::new();
        for (num, net) in &ip.pads {
            if let Some(net) = net {
                pads.insert(num.clone(), net.clone());
            }
        }
        parts.insert(
            ip.reference.clone(),
            bl::Part {
                footprint: ip.lib_id.clone(),
                pads,
                edge: false,
                corner: false,
                lock: Some(bl::Lock {
                    x: round2(ip.at.x - ox),
                    y: round2(ip.at.y - oy),
                    rot: ip.rotation,
                }),
            },
        );
    }

    Ok(bl::BoardDesign {
        name: path.file_stem().and_then(|s| s.to_str()).map(String::from),
        board: bl::BoardSpec {
            layers: b.layer_count,
            outline: bl::Outline::Rect {
                w: round2(b.bounds.max_x - b.bounds.min_x),
                h: round2(b.bounds.max_y - b.bounds.min_y),
            },
            rules: bl::Rules::default(),
        },
        parts,
        groups: indexmap::IndexMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn design(src: &str) -> bl::BoardDesign {
        let r = board_lang::compile(src);
        assert!(!r.diagnostics.has_errors(), "diags: {:?}", r.diagnostics);
        r.design.unwrap()
    }

    #[test]
    fn lowers_rules_parts_and_hints() {
        let d = design(
            r#"
version: 1
board:
  layers: 4
  outline: {rect: [44, 32]}
  rules:
    clearance: 0.15
    trace_width: 0.2
    via: [0.6, 0.3]
    net_widths: {VIN: 0.8}
    pours: [{net: GND, layer: bottom}]
parts:
  U1: {footprint: 'P:SOIC-8', pads: {1: VIN, 3: GND}}
  J1: {footprint: 'C:Hdr', pads: {1: VIN, 2: GND}, edge: true}
  H1: {footprint: 'M:Hole', corner: true, lock: {at: [2, 2], rot: 90}}
place:
  groups:
    deco: {members: [U1], surround: J1, region: [1, 2, 3, 4]}
"#,
        );
        let draft = design_to_draft(&d);
        assert_eq!(draft.bounds.max_x, 44.0);
        assert_eq!(draft.bounds.max_y, 32.0);
        assert!(draft.outline.is_none(), "rect = no custom polygon");
        assert_eq!(draft.rules.layer_count, 4);
        assert_eq!(draft.rules.clearance, 0.15);
        assert_eq!(draft.rules.net_widths["VIN"], 0.8);
        assert_eq!(draft.rules.pours[0].net, "GND");
        assert_eq!(draft.parts.len(), 3);
        assert_eq!(draft.hints.edge_seek, vec!["J1".to_string()]);
        assert_eq!(draft.hints.corner_seek, vec!["H1".to_string()]);
        let h1 = draft.parts.iter().find(|p| p.reference == "H1").unwrap();
        assert_eq!(h1.locked.as_ref().unwrap().rotation, 90);
        let g = &draft.hints.groups[0];
        assert_eq!(g.surround.as_deref(), Some("J1"));
        let region = g.region.clone().unwrap();
        assert_eq!((region.min_x, region.min_y, region.max_x, region.max_y), (1.0, 2.0, 3.0, 4.0));
    }

    #[test]
    fn circle_outline_becomes_a_polygon_with_bbox_bounds() {
        let d = design(
            "version: 1\nboard:\n  layers: 2\n  outline: {circle: 16}\n  rules: {clearance: 0.2, trace_width: 0.2, via: [0.6, 0.3]}\nparts:\n  R1: {footprint: 'X:Y', pads: {1: A, 2: B}}\n",
        );
        let draft = design_to_draft(&d);
        assert_eq!(draft.bounds.max_x, 32.0);
        let pts = draft.outline.expect("circle yields a polygon");
        assert_eq!(pts.len(), CIRCLE_SEGMENTS);
        // every vertex within the bbox
        for p in &pts {
            assert!(p.x >= -1e-9 && p.x <= 32.0 + 1e-9);
            assert!(p.y >= -1e-9 && p.y <= 32.0 + 1e-9);
        }
    }
}
