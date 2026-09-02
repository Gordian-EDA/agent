//! Same-net copper termination checks for trace endpoints and vias.

use std::collections::BTreeSet;

use geom::EPS;
use pcb_model::{Finding, LayerRef, Point2, Trace, Via, ViaSpan};

use crate::{DrcCtx, Rule};

/// Reports trace ends without an anchor and vias that stitch fewer than two layers.
pub struct DanglingEndRule;

impl Rule for DanglingEndRule {
    fn name(&self) -> &'static str {
        "dangling-end"
    }

    fn check(&self, ctx: &DrcCtx) -> Vec<Finding> {
        let mut findings = Vec::new();
        for (trace_idx, trace) in ctx.solution.traces.iter().enumerate() {
            let Some(first) = trace.path.first().copied() else {
                continue;
            };
            let last = *trace.path.last().expect("non-empty trace");
            let endpoints = if first.near_eq(last, EPS) {
                vec![first]
            } else {
                vec![first, last]
            };
            for at in endpoints {
                if !trace_end_is_anchored(ctx, trace_idx, trace, at) {
                    findings.push(Finding::DanglingEnd {
                        net: trace.connection.clone(),
                        at,
                        layer: trace.layer.0.clone(),
                    });
                }
            }
        }
        for via in &ctx.solution.vias {
            let connected = via_connected_layers(ctx, via);
            if connected.len() < 2 {
                findings.push(Finding::DanglingEnd {
                    net: via.connection.clone(),
                    at: via.at,
                    layer: first_unconnected_layer(ctx, via, &connected).0,
                });
            }
        }
        findings
    }
}

fn trace_end_is_anchored(ctx: &DrcCtx, own_idx: usize, trace: &Trace, at: Point2) -> bool {
    let half_width = trace.width / 2.0;
    if trace.path.len() > 2
        && trace.path[0].near_eq(*trace.path.last().expect("non-empty trace"), EPS)
    {
        return true;
    }
    if ctx.problem.obstacles.iter().any(|pad| {
        is_pad(pad)
            && pad.connected_to.iter().any(|net| net == &trace.connection)
            && pad.layers.contains(&trace.layer)
            && pad.proven_capsule().dist_to_point(at) <= half_width + EPS
    }) {
        return true;
    }
    if ctx.problem.connections.iter().any(|connection| {
        connection.name == trace.connection
            && connection.points_to_connect.iter().any(|point| {
                point.layer == trace.layer && point.point().dist(at) <= half_width + EPS
            })
    }) {
        return true;
    }
    if own_trace_nonincident_segment_touches(trace, at) {
        return true;
    }
    if all_traces(ctx).enumerate().any(|(idx, other)| {
        idx != own_idx
            && other.connection == trace.connection
            && other.layer == trace.layer
            && trace_touches_point(other, at, half_width)
    }) {
        return true;
    }
    if all_vias(ctx).any(|via| {
        via.connection == trace.connection
            && via_spans_layer(via, &trace.layer, ctx.problem.layer_count)
            && via.at.dist(at) <= via.diameter / 2.0 + half_width + EPS
    }) {
        return true;
    }
    plane_carries(ctx, &trace.connection, &trace.layer)
}

fn own_trace_nonincident_segment_touches(trace: &Trace, at: Point2) -> bool {
    let segment_count = trace.path.len().saturating_sub(1);
    trace.path.windows(2).enumerate().any(|(index, pair)| {
        let incident = (at.near_eq(trace.path[0], EPS) && index == 0)
            || (at.near_eq(*trace.path.last().expect("non-empty trace"), EPS)
                && index + 1 == segment_count);
        !incident && geom::Segment::new(pair[0], pair[1]).dist_to_point(at) <= trace.width + EPS
    })
}

fn trace_touches_point(trace: &Trace, at: Point2, point_radius: f64) -> bool {
    let reach = trace.width / 2.0 + point_radius + EPS;
    if trace.path.len() == 1 {
        return trace.path[0].dist(at) <= reach;
    }
    trace
        .path
        .windows(2)
        .any(|pair| geom::Segment::new(pair[0], pair[1]).dist_to_point(at) <= reach)
}

fn via_connected_layers(ctx: &DrcCtx, via: &Via) -> BTreeSet<u32> {
    let mut layers = BTreeSet::new();
    let radius = via.diameter / 2.0;
    for pad in &ctx.problem.obstacles {
        if !is_pad(pad)
            || !pad.connected_to.iter().any(|net| net == &via.connection)
            || pad.proven_capsule().dist_to_point(via.at) > radius + EPS
        {
            continue;
        }
        for layer in &pad.layers {
            if let Some(index) = layer.index(ctx.problem.layer_count)
                && via_spans_index(via, index, ctx.problem.layer_count)
            {
                layers.insert(index);
            }
        }
    }
    for trace in all_traces(ctx) {
        let Some(index) = trace.layer.index(ctx.problem.layer_count) else {
            continue;
        };
        if trace.connection == via.connection
            && via_spans_index(via, index, ctx.problem.layer_count)
            && trace_touches_point(trace, via.at, radius)
        {
            layers.insert(index);
        }
    }
    for connection in &ctx.problem.connections {
        if connection.name != via.connection {
            continue;
        }
        for point in &connection.points_to_connect {
            if point.point().dist(via.at) <= radius + EPS
                && let Some(index) = point.layer.index(ctx.problem.layer_count)
                && via_spans_index(via, index, ctx.problem.layer_count)
            {
                layers.insert(index);
            }
        }
    }
    for other in all_vias(ctx) {
        if std::ptr::eq(other, via)
            || other.connection != via.connection
            || other.at.dist(via.at) > radius + other.diameter / 2.0 + EPS
        {
            continue;
        }
        for index in via_layer_indices(other, ctx.problem.layer_count) {
            if via_spans_index(via, index, ctx.problem.layer_count) {
                layers.insert(index);
            }
        }
    }
    for (net, &index) in &ctx.problem.plane_nets {
        if net == &via.connection && via_spans_index(via, index, ctx.problem.layer_count) {
            layers.insert(index);
        }
    }
    layers
}

fn is_pad(obstacle: &pcb_model::Obstacle) -> bool {
    obstacle.kind == "rect" || obstacle.kind == "pad" || obstacle.kind.starts_with("pad:")
}

fn first_unconnected_layer(ctx: &DrcCtx, via: &Via, connected: &BTreeSet<u32>) -> LayerRef {
    via_layer_indices(via, ctx.problem.layer_count)
        .find(|layer| !connected.contains(layer))
        .map(|layer| layer_ref(layer, ctx.problem.layer_count))
        .unwrap_or_else(LayerRef::top)
}

fn plane_carries(ctx: &DrcCtx, net: &str, layer: &LayerRef) -> bool {
    let Some(index) = layer.index(ctx.problem.layer_count) else {
        return false;
    };
    ctx.problem.plane_nets.get(net) == Some(&index)
}

fn via_spans_layer(via: &Via, layer: &LayerRef, layer_count: u32) -> bool {
    layer
        .index(layer_count)
        .is_some_and(|index| via_spans_index(via, index, layer_count))
}

fn via_spans_index(via: &Via, index: u32, layer_count: u32) -> bool {
    match via.span {
        ViaSpan::Through => index < layer_count,
        ViaSpan::Partial { from, to, .. } => {
            let (from, to) = (from.min(to), from.max(to));
            index >= from && index <= to && index < layer_count
        }
    }
}

fn via_layer_indices(via: &Via, layer_count: u32) -> impl Iterator<Item = u32> {
    let (from, to) = match via.span {
        ViaSpan::Through => (0, layer_count.saturating_sub(1)),
        ViaSpan::Partial { from, to, .. } => (from.min(to), from.max(to)),
    };
    (from..=to).filter(move |&index| index < layer_count)
}

fn layer_ref(index: u32, layer_count: u32) -> LayerRef {
    if index == 0 {
        LayerRef::top()
    } else if index + 1 == layer_count {
        LayerRef::bottom()
    } else {
        LayerRef(format!("inner{index}"))
    }
}

fn all_traces<'a>(ctx: &'a DrcCtx<'_>) -> impl Iterator<Item = &'a Trace> {
    ctx.solution
        .traces
        .iter()
        .chain(ctx.problem.fixed_copper.traces.iter())
}

fn all_vias<'a>(ctx: &'a DrcCtx<'_>) -> impl Iterator<Item = &'a Via> {
    ctx.solution
        .vias
        .iter()
        .chain(ctx.problem.fixed_copper.vias.iter())
}
