//! Copper retraction: the copper a board edit invalidates.
//!
//! Moving a part, deleting one, or changing which net a pad belongs to leaves
//! traces that no longer describe the board. Retraction drops exactly those and
//! rewrites the file with what still stands, so the agent can re-route the named
//! nets instead of re-routing everything.

use std::collections::{BTreeMap, BTreeSet};

use geom::Rect;
use gordian_runtime::AgentRuntime;
use kicad_board::{BoardSnapshot, ImportedPad};
use pcb_model::{LayerRef, RouteSolution, RoutingView, Trace, Via, ViaSpan};

/// The copper an edit invalidates, and everything it leaves standing.
#[derive(Debug, Default, Clone)]
pub(crate) struct RetractedCopper {
    pub(crate) retained: RouteSolution,
    /// Nets that lost copper and therefore need re-routing.
    pub(crate) nets: BTreeSet<String>,
    pub(crate) count: usize,
    pub(crate) via_count: usize,
    pub(crate) zone_nets: BTreeSet<String>,
}

impl RetractedCopper {
    pub(crate) fn changed(&self) -> bool {
        self.count > 0 || self.via_count > 0 || !self.zone_nets.is_empty()
    }
}

/// Copper removed because one schematic pad moved from one net to another.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RetargetRetraction {
    pub(crate) net_a: String,
    pub(crate) net_b: String,
    pub(crate) segments: usize,
    pub(crate) vias: usize,
    pub(crate) zones: usize,
    pub(crate) refs: BTreeSet<String>,
}

/// Board-space pad extents of `references`, from the problem's `pad:<ref>`
/// obstacles.
pub(crate) fn pad_extents<'a>(
    problem: &RoutingView,
    references: impl IntoIterator<Item = &'a str>,
) -> Vec<Rect> {
    let wanted: BTreeSet<&str> = references.into_iter().collect();
    problem
        .obstacles
        .iter()
        .filter(|obstacle| {
            obstacle
                .kind
                .strip_prefix("pad:")
                .is_some_and(|reference| wanted.contains(reference))
        })
        .map(|obstacle| {
            Rect::new(
                obstacle.center.x - obstacle.width / 2.0,
                obstacle.center.y - obstacle.height / 2.0,
                obstacle.center.x + obstacle.width / 2.0,
                obstacle.center.y + obstacle.height / 2.0,
            )
        })
        .collect()
}

/// Retract every net with a trace touching one of `pads`, plus every net in
/// `nets`.
///
/// The unit of retraction is the NET, not the trace. A net whose copper is only
/// partly removed is left with a stub hanging off nothing — KiCAD reports it as
/// `track_dangling`, and the board fails DRC with no unrouted net to explain it.
/// So once an edit invalidates any of a net's copper, all of it goes and the net
/// is named for re-routing.
pub(crate) fn retract(
    copper: &RouteSolution,
    pads: &[Rect],
    nets: &BTreeSet<String>,
) -> RetractedCopper {
    let touches_pad = |trace: &Trace| {
        trace
            .path
            .iter()
            .any(|point| pads.iter().any(|pad| pad.contains(*point)))
    };
    let via_touches_pad = |via: &Via| {
        pads.iter()
            .any(|pad| pad.dist_to_point(via.at) <= via.diameter / 2.0 + geom::EPS)
    };
    let dropped: BTreeSet<String> = copper
        .traces
        .iter()
        .filter(|trace| nets.contains(&trace.connection) || touches_pad(trace))
        .map(|trace| trace.connection.clone())
        .chain(
            copper
                .vias
                .iter()
                .filter(|via| nets.contains(&via.connection) || via_touches_pad(via))
                .map(|via| via.connection.clone()),
        )
        .collect();
    let (out, kept): (Vec<_>, Vec<_>) = copper
        .traces
        .iter()
        .cloned()
        .partition(|trace| dropped.contains(&trace.connection));
    RetractedCopper {
        count: out.len(),
        via_count: copper
            .vias
            .iter()
            .filter(|via| dropped.contains(&via.connection))
            .count(),
        retained: RouteSolution {
            traces: kept,
            vias: copper
                .vias
                .iter()
                .filter(|via| !dropped.contains(&via.connection))
                .cloned()
                .collect(),
        },
        nets: dropped,
        zone_nets: BTreeSet::new(),
    }
}

/// Retract only old-net copper components that still touch re-netted pads.
pub(crate) fn retract_retargeted<'a>(
    board: &BoardSnapshot,
    changes: impl IntoIterator<Item = (&'a str, &'a str, Option<&'a str>, Option<&'a str>)>,
) -> (RetractedCopper, Vec<RetargetRetraction>) {
    let mut dropped_traces = BTreeSet::new();
    let mut dropped_vias = BTreeSet::new();
    let mut dropped_zone_nets = BTreeSet::new();
    let mut reports =
        BTreeMap::<(String, String), (RetargetRetraction, BTreeSet<usize>, BTreeSet<usize>)>::new();

    for (reference, pad_number, from, to) in changes {
        let (Some(from), Some(to)) = (from, to) else {
            continue;
        };
        if from == to {
            continue;
        }
        let Some(pad) = board
            .imported
            .parts
            .iter()
            .find(|part| part.reference == reference)
            .and_then(|part| part.pads.iter().find(|pad| pad.number == pad_number))
        else {
            continue;
        };
        let pad_rect = board
            .problem
            .obstacles
            .iter()
            .find(|obstacle| {
                obstacle.kind == format!("pad:{reference}")
                    && obstacle.center.dist(pad.at) <= geom::EPS
                    && obstacle.connected_to.iter().any(|net| net == from)
            })
            .map(pcb_model::Obstacle::bounds)
            .unwrap_or_else(|| {
                Rect::from_center_half(pad.at, (pad.size.x.abs() / 2.0, pad.size.y.abs() / 2.0))
            });
        let (component_traces, component_vias) =
            copper_component_at_pad(board, pad, &pad_rect, from);
        let has_zone = board.problem.plane_nets.get(from).is_some_and(|index| {
            pad.layers
                .contains(&layer(*index, board.problem.layer_count))
        });
        if component_traces.is_empty() && component_vias.is_empty() && !has_zone {
            continue;
        }

        let key = if from <= to {
            (from.to_owned(), to.to_owned())
        } else {
            (to.to_owned(), from.to_owned())
        };
        let (report, report_traces, report_vias) =
            reports.entry(key.clone()).or_insert_with(|| {
                (
                    RetargetRetraction {
                        net_a: key.0,
                        net_b: key.1,
                        ..RetargetRetraction::default()
                    },
                    BTreeSet::new(),
                    BTreeSet::new(),
                )
            });
        report.refs.insert(format!("{reference}.{pad_number}"));
        report.refs.extend(copper_component_refs(
            board,
            from,
            &component_traces,
            &component_vias,
        ));
        report_traces.extend(component_traces.iter().copied());
        report_vias.extend(component_vias.iter().copied());
        dropped_traces.extend(component_traces.iter().copied());
        dropped_vias.extend(component_vias.iter().copied());
        if has_zone {
            report.zones = 1;
            dropped_zone_nets.insert(from.to_owned());
        }
    }

    for (report, traces, vias) in reports.values_mut() {
        report.segments = traces
            .iter()
            .map(|index| board.copper.traces[*index].path.len().saturating_sub(1))
            .sum();
        report.vias = vias.len();
    }
    let nets = reports
        .values()
        .flat_map(|(report, _, _)| [report.net_a.clone(), report.net_b.clone()])
        .collect();
    let retained = RouteSolution {
        traces: board
            .copper
            .traces
            .iter()
            .enumerate()
            .filter(|(index, _)| !dropped_traces.contains(index))
            .map(|(_, trace)| trace.clone())
            .collect(),
        vias: board
            .copper
            .vias
            .iter()
            .enumerate()
            .filter(|(index, _)| !dropped_vias.contains(index))
            .map(|(_, via)| via.clone())
            .collect(),
    };
    (
        RetractedCopper {
            retained,
            nets,
            count: dropped_traces.len(),
            via_count: dropped_vias.len(),
            zone_nets: dropped_zone_nets,
        },
        reports.into_values().map(|(report, _, _)| report).collect(),
    )
}

fn copper_component_at_pad(
    board: &BoardSnapshot,
    pad: &ImportedPad,
    pad_rect: &Rect,
    net: &str,
) -> (BTreeSet<usize>, BTreeSet<usize>) {
    let mut traces = board
        .copper
        .traces
        .iter()
        .enumerate()
        .filter(|(_, trace)| {
            trace.connection == net
                && pad.layers.contains(&trace.layer)
                && trace.path.windows(2).any(|points| {
                    geom::Segment::new(points[0], points[1]).dist_to_rect(pad_rect)
                        <= trace.width / 2.0 + geom::EPS
                })
        })
        .map(|(index, _)| index)
        .collect::<BTreeSet<_>>();
    let mut vias = board
        .copper
        .vias
        .iter()
        .enumerate()
        .filter(|(_, via)| {
            via.connection == net
                && pad_rect.dist_to_point(via.at) <= via.diameter / 2.0 + geom::EPS
        })
        .map(|(index, _)| index)
        .collect::<BTreeSet<_>>();

    loop {
        let before = (traces.len(), vias.len());
        for (index, trace) in board.copper.traces.iter().enumerate() {
            if trace.connection != net || traces.contains(&index) {
                continue;
            }
            let touches_trace = traces
                .iter()
                .any(|other| traces_touch(trace, &board.copper.traces[*other]));
            let touches_via = vias.iter().any(|via| {
                trace_touches_via(trace, &board.copper.vias[*via], board.problem.layer_count)
            });
            if touches_trace || touches_via {
                traces.insert(index);
            }
        }
        for (index, via) in board.copper.vias.iter().enumerate() {
            if via.connection != net || vias.contains(&index) {
                continue;
            }
            let touches_trace = traces.iter().any(|trace| {
                trace_touches_via(&board.copper.traces[*trace], via, board.problem.layer_count)
            });
            let touches_via = vias.iter().any(|other| {
                vias_touch(via, &board.copper.vias[*other], board.problem.layer_count)
            });
            if touches_trace || touches_via {
                vias.insert(index);
            }
        }
        if before == (traces.len(), vias.len()) {
            break;
        }
    }
    (traces, vias)
}

fn traces_touch(a: &Trace, b: &Trace) -> bool {
    a.layer == b.layer
        && a.path.windows(2).any(|left| {
            b.path.windows(2).any(|right| {
                geom::Segment::new(left[0], left[1])
                    .dist_to_segment(geom::Segment::new(right[0], right[1]))
                    <= (a.width + b.width) / 2.0 + geom::EPS
            })
        })
}

fn trace_touches_via(trace: &Trace, via: &Via, layer_count: u32) -> bool {
    via_layers(&via.span, layer_count).contains(&trace.layer)
        && trace.path.windows(2).any(|points| {
            geom::Segment::new(points[0], points[1]).dist_to_point(via.at)
                <= trace.width / 2.0 + via.diameter / 2.0 + geom::EPS
        })
}

fn vias_touch(a: &Via, b: &Via, layer_count: u32) -> bool {
    via_layers(&a.span, layer_count)
        .iter()
        .any(|layer| via_layers(&b.span, layer_count).contains(layer))
        && a.at.dist(b.at) <= (a.diameter + b.diameter) / 2.0 + geom::EPS
}

fn via_layers(span: &ViaSpan, layer_count: u32) -> Vec<LayerRef> {
    match *span {
        ViaSpan::Through => (0..layer_count)
            .map(|index| layer(index, layer_count))
            .collect(),
        ViaSpan::Partial { from, to, .. } => (from.min(to)..=from.max(to))
            .map(|index| layer(index, layer_count))
            .collect(),
    }
}

fn layer(index: u32, layer_count: u32) -> LayerRef {
    if index == 0 {
        LayerRef::top()
    } else if index + 1 == layer_count {
        LayerRef::bottom()
    } else {
        LayerRef(format!("inner{index}"))
    }
}

fn copper_component_refs(
    board: &BoardSnapshot,
    net: &str,
    traces: &BTreeSet<usize>,
    vias: &BTreeSet<usize>,
) -> BTreeSet<String> {
    board
        .imported
        .parts
        .iter()
        .flat_map(|part| {
            part.pads.iter().filter_map(move |pad| {
                if pad.net.as_deref() != Some(net) {
                    return None;
                }
                let rect = Rect::from_center_half(
                    pad.at,
                    (pad.size.x.abs() / 2.0, pad.size.y.abs() / 2.0),
                );
                let touches_trace = traces.iter().any(|index| {
                    let trace = &board.copper.traces[*index];
                    pad.layers.contains(&trace.layer)
                        && trace.path.windows(2).any(|points| {
                            geom::Segment::new(points[0], points[1]).dist_to_rect(&rect)
                                <= trace.width / 2.0 + geom::EPS
                        })
                });
                let touches_via = vias.iter().any(|index| {
                    let via = &board.copper.vias[*index];
                    rect.dist_to_point(via.at) <= via.diameter / 2.0 + geom::EPS
                });
                (touches_trace || touches_via).then(|| format!("{}.{}", part.reference, pad.number))
            })
        })
        .collect()
}

/// Rewrite the board file so it carries only the retained copper. A retraction
/// that dropped nothing writes nothing.
pub(crate) fn write_retained(
    ctx: &AgentRuntime,
    layer_count: u32,
    layer_names: &[String],
    retract: &RetractedCopper,
) -> std::result::Result<(), String> {
    if !retract.changed() {
        return Ok(());
    }
    let path = ctx.pcb_path();
    let text =
        std::fs::read_to_string(&path).map_err(|err| format!("could not read the board: {err}"))?;
    let (stripped, _, _) = kicad_board::strip_copper(&text)?;
    let replacement =
        kicad_board::append_copper(&stripped, &retract.retained, layer_count, layer_names)?;
    // The same atomic replace the route path uses: an interrupted edit must not
    // leave a half-written board.
    crate::route::write_board_atomically(&path, replacement.as_bytes())
        .map_err(|err| format!("could not write the board: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_model::{LayerRef, Obstacle, Point2};

    fn trace(connection: &str, path: &[(f64, f64)]) -> Trace {
        Trace {
            connection: connection.into(),
            layer: LayerRef::top(),
            width: 0.2,
            path: path.iter().map(|&(x, y)| Point2::new(x, y)).collect(),
        }
    }

    fn problem() -> RoutingView {
        RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![Obstacle {
                kind: "pad:R1".into(),
                layers: vec![LayerRef::top()],
                center: Point2::new(10.0, 10.0),
                width: 1.0,
                height: 1.0,
                connected_to: vec!["VIN".into()],
            }],
            connections: Vec::new(),
            bounds: Rect::new(0.0, 0.0, 50.0, 40.0),
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            plane_nets: Default::default(),
            escape_layers: Default::default(),
            nets: Default::default(),
            fixed_copper: Default::default(),
        }
    }

    #[test]
    fn traces_on_a_named_pad_or_a_named_net_come_out() {
        let copper = RouteSolution {
            traces: vec![
                trace("VIN", &[(10.0, 10.0), (20.0, 10.0)]),
                trace("GND", &[(0.0, 0.0), (5.0, 0.0)]),
                trace("SENSE", &[(30.0, 30.0), (31.0, 30.0)]),
            ],
            vias: Vec::new(),
        };
        let pads = pad_extents(&problem(), ["R1"]);
        let out = retract(&copper, &pads, &BTreeSet::from(["SENSE".to_string()]));
        assert_eq!(out.count, 2);
        assert_eq!(
            out.nets,
            BTreeSet::from(["VIN".to_string(), "SENSE".to_string()])
        );
        assert_eq!(out.retained.traces.len(), 1);
        assert_eq!(out.retained.traces[0].connection, "GND");
    }

    #[test]
    fn a_nets_copper_comes_out_whole() {
        // The far trace never touches R1; it goes anyway, because half a net of
        // copper is a dangling stub, not a partial route.
        let copper = RouteSolution {
            traces: vec![
                trace("VIN", &[(10.0, 10.0), (20.0, 10.0)]),
                trace("VIN", &[(30.0, 30.0), (40.0, 30.0)]),
            ],
            vias: vec![pcb_model::Via {
                connection: "VIN".into(),
                at: Point2::new(30.0, 30.0),
                diameter: 0.6,
                drill: 0.3,
                span: pcb_model::ViaSpan::Through,
            }],
        };
        let out = retract(&copper, &pad_extents(&problem(), ["R1"]), &BTreeSet::new());
        assert_eq!(out.count, 2);
        assert!(out.retained.traces.is_empty());
        assert!(out.retained.vias.is_empty(), "the net's vias go with it");
    }

    #[test]
    fn an_untouched_board_retracts_nothing() {
        let copper = RouteSolution {
            traces: vec![trace("GND", &[(0.0, 0.0), (5.0, 0.0)])],
            vias: Vec::new(),
        };
        let out = retract(&copper, &pad_extents(&problem(), ["R9"]), &BTreeSet::new());
        assert_eq!(out.count, 0);
        assert_eq!(out.retained.traces.len(), 1);
    }

    #[test]
    fn retarget_retracts_the_connected_via_component_and_old_zone_only() {
        let mut problem = problem();
        problem.plane_nets.insert("VIN".to_owned(), 0);
        problem.plane_nets.insert("GND".to_owned(), 1);
        let bottom = Trace {
            connection: "VIN".into(),
            layer: LayerRef::bottom(),
            width: 0.2,
            path: vec![Point2::new(15.0, 10.0), Point2::new(20.0, 10.0)],
        };
        let board = BoardSnapshot {
            problem,
            imported: kicad_board::ImportedBoard {
                layer_count: 2,
                bounds: Rect::new(0.0, 0.0, 50.0, 40.0),
                parts: vec![kicad_board::ImportedPart {
                    reference: "R1".to_owned(),
                    lib_id: "Test:R".to_owned(),
                    at: Point2::new(10.0, 10.0),
                    rotation: 0,
                    side: kicad_board::BoardSide::Front,
                    locked: false,
                    properties: BTreeMap::new(),
                    courtyard: None,
                    pads: vec![ImportedPad {
                        number: "1".to_owned(),
                        net: Some("VIN".to_owned()),
                        at: Point2::new(10.0, 10.0),
                        layers: vec![LayerRef::top()],
                        shape: "rect".to_owned(),
                        size: Point2::new(1.0, 1.0),
                        drill: None,
                    }],
                }],
                placement_keepouts: Vec::new(),
                keepout_count: 0,
            },
            copper: RouteSolution {
                traces: vec![trace("VIN", &[(10.0, 10.0), (15.0, 10.0)]), bottom],
                vias: vec![Via {
                    connection: "VIN".to_owned(),
                    at: Point2::new(15.0, 10.0),
                    diameter: 0.6,
                    drill: 0.3,
                    span: ViaSpan::Through,
                }],
            },
            layer_names: vec!["F.Cu".to_owned(), "B.Cu".to_owned()],
        };

        let (retracted, reports) =
            retract_retargeted(&board, [("R1", "1", Some("VIN"), Some("GND"))]);

        assert_eq!(retracted.count, 2);
        assert_eq!(retracted.via_count, 1);
        assert_eq!(retracted.zone_nets, BTreeSet::from(["VIN".to_owned()]));
        assert!(retracted.retained.traces.is_empty());
        assert!(retracted.retained.vias.is_empty());
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].segments, 2);
        assert_eq!(reports[0].vias, 1);
        assert_eq!(reports[0].zones, 1);
    }
}
