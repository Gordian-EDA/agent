//! Offline `.kicad_pcb` snapshots for placement and routing workflows.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use geom::{Point2, Polygon, Polyline, Rect, Segment};
use pcb_model::{
    Connection, LayerRef, Obstacle, RoutePoint, RouteSolution, RoutingView, Trace, Via, ViaSpan,
};

use crate::active::{BoardSide, ImportedBoard, ImportedPad, ImportedPart, IpcBoardSnapshot};
use crate::patch::{Node, child_nodes, node_head, root_body};

const DEFAULT_MIN_TRACE_WIDTH_MM: f64 = 0.2;
const DEFAULT_CLEARANCE_MM: f64 = 0.2;
const DEFAULT_VIA_DIAMETER_MM: f64 = 0.6;
const DEFAULT_VIA_DRILL_MM: f64 = 0.3;

#[derive(Clone, Debug)]
pub(crate) struct FileRules {
    pub(crate) min_trace_width: f64,
    pub(crate) clearance: f64,
    pub(crate) via_diameter: f64,
    pub(crate) via_drill: f64,
    pub(crate) net_widths: BTreeMap<String, f64>,
}

impl Default for FileRules {
    fn default() -> Self {
        Self {
            min_trace_width: DEFAULT_MIN_TRACE_WIDTH_MM,
            clearance: DEFAULT_CLEARANCE_MM,
            via_diameter: DEFAULT_VIA_DIAMETER_MM,
            via_drill: DEFAULT_VIA_DRILL_MM,
            net_widths: BTreeMap::new(),
        }
    }
}

/// Read one saved KiCad board without starting or connecting to pcbnew.
pub fn read_snapshot(path: &Path) -> Result<IpcBoardSnapshot, String> {
    if !path.exists() {
        return Err("no board exists yet — run sync_board first".to_owned());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("could not read board {}: {err}", path.display()))?;
    let (body_start, body_end) = root_body(&text)?;
    let top = child_nodes(&text, body_start, body_end);
    let layer_names = crate::patch::board_copper_layer_names(&text)?;
    let layer_count = layer_names.len() as u32;
    let nets = net_table(&text, &top);
    let outline = board_outline(&text, &top)?;
    let mut rules = file_rules(path, &text, &top)?;

    let mut obstacles = Vec::new();
    let mut net_points = BTreeMap::<String, Vec<RoutePoint>>::new();
    let mut parts = Vec::new();
    for footprint in top
        .iter()
        .filter(|node| node_head(&text, node) == "footprint")
    {
        if let Some(part) = read_footprint(
            &text,
            footprint,
            &layer_names,
            &mut obstacles,
            &mut net_points,
        ) {
            parts.push(part);
        }
    }

    let mut traces = Vec::new();
    for track in top
        .iter()
        .filter(|node| matches!(node_head(&text, node), "segment" | "arc"))
    {
        if let Some(obstacle) = read_track_obstacle(&text, track, &nets, &layer_names) {
            obstacles.push(obstacle);
        }
        if let Some(trace) = read_trace(&text, track, &nets, &layer_names) {
            traces.push(trace);
        }
    }

    let mut vias = Vec::new();
    for via_node in top.iter().filter(|node| node_head(&text, node) == "via") {
        if let Some(obstacle) = read_via_obstacle(&text, via_node, &nets, &layer_names) {
            obstacles.push(obstacle);
        }
        if let Some(via) = read_via(&text, via_node, &nets, &layer_names) {
            vias.push(via);
        }
    }

    let mut placement_keepouts = Vec::new();
    let mut keepout_count = 0usize;
    let mut copper_zone_layers = BTreeMap::<String, BTreeSet<u32>>::new();
    let mut zones = ZoneSnapshot {
        obstacles: &mut obstacles,
        placement_keepouts: &mut placement_keepouts,
        keepout_count: &mut keepout_count,
        copper_layers: &mut copper_zone_layers,
    };
    for zone in top.iter().filter(|node| node_head(&text, node) == "zone") {
        read_zone(&text, zone, &nets, &layer_names, &outline, &mut zones);
    }

    let bounds = outline.bbox();
    let known_nets: BTreeSet<&str> = parts
        .iter()
        .flat_map(|part| part.pads.iter())
        .filter_map(|pad| pad.net.as_deref())
        .collect();
    rules
        .net_widths
        .retain(|net, _| known_nets.contains(net.as_str()));
    let connections: Vec<_> = net_points
        .into_iter()
        .filter(|(_, points)| points.len() >= 2)
        .map(|(name, points_to_connect)| Connection {
            name,
            points_to_connect,
        })
        .collect();
    let plane_nets = observed_plane_nets(layer_count, &connections, &copper_zone_layers);

    Ok(IpcBoardSnapshot {
        problem: RoutingView {
            layer_count,
            min_trace_width: rules.min_trace_width,
            obstacles,
            connections,
            bounds,
            clearance: rules.clearance,
            via_diameter: rules.via_diameter,
            via_drill: rules.via_drill,
            net_widths: rules.net_widths,
            outline: Some(outline),
            escape_layers: BTreeMap::new(),
            plane_nets,
            fixed_copper: RouteSolution::default(),
            nets: None,
        },
        imported: ImportedBoard {
            layer_count,
            bounds,
            parts,
            placement_keepouts,
            keepout_count,
        },
        copper: RouteSolution { traces, vias },
        layer_names,
    })
}

fn read_footprint(
    text: &str,
    node: &Node,
    layer_names: &[String],
    obstacles: &mut Vec<Obstacle>,
    net_points: &mut BTreeMap<String, Vec<RoutePoint>>,
) -> Option<ImportedPart> {
    let children = child_nodes(text, node.start + 1, node.end - 1);
    let reference = children
        .iter()
        .filter(|child| node_head(text, child) == "property")
        .find_map(|child| property(text, child, "Reference"))?;
    if reference.is_empty() {
        return None;
    }
    let lib_id = node_atoms(text, node).get(1).cloned().unwrap_or_default();
    let (x, y, rotation) = children
        .iter()
        .find(|child| node_head(text, child) == "at")
        .and_then(|child| point_angle(text, child))
        .unwrap_or((0.0, 0.0, 0.0));
    let at = Point2::new(x, y);
    let rotation = geom::snap_quadrant(rotation) as i32;
    let back = child_text(text, &children, "layer").as_deref() == Some("B.Cu");
    let locked = children.iter().any(|child| {
        node_head(text, child) == "locked"
            && node_atoms(text, child)
                .get(1)
                .is_some_and(|value| value == "yes")
    });
    let mut pads = Vec::new();
    for pad_node in children
        .iter()
        .filter(|child| node_head(text, child) == "pad")
    {
        let Some(pad) = read_pad(text, pad_node, at, rotation as f64, back, layer_names) else {
            continue;
        };
        let connected_to = pad.net.clone().into_iter().collect();
        obstacles.push(Obstacle {
            kind: format!("pad:{reference}"),
            layers: pad.layers.clone(),
            center: pad.at,
            width: pad.width,
            height: pad.height,
            connected_to,
        });
        if let Some(net) = &pad.net {
            net_points.entry(net.clone()).or_default().push(RoutePoint {
                x: pad.at.x,
                y: pad.at.y,
                layer: pad.layers.first().cloned().unwrap_or_else(LayerRef::top),
            });
        }
        pads.push(ImportedPad {
            number: pad.number,
            net: pad.net,
            at: pad.at,
            layers: pad.layers,
            shape: pad.shape,
            size: pad.size,
            drill: pad.drill,
        });
    }
    Some(ImportedPart {
        reference,
        lib_id,
        at,
        rotation,
        side: if back {
            BoardSide::Back
        } else {
            BoardSide::Front
        },
        locked,
        courtyard: courtyard_bounds(text, &children),
        pads,
    })
}

struct ParsedPad {
    number: String,
    net: Option<String>,
    at: Point2,
    layers: Vec<LayerRef>,
    shape: String,
    size: Point2,
    drill: Option<Point2>,
    width: f64,
    height: f64,
}

fn read_pad(
    text: &str,
    node: &Node,
    footprint_at: Point2,
    footprint_rotation: f64,
    back: bool,
    layer_names: &[String],
) -> Option<ParsedPad> {
    let atoms = node_atoms(text, node);
    let number = atoms.get(1).cloned().unwrap_or_default();
    let pad_type = atoms.get(2).map(String::as_str).unwrap_or("");
    let children = child_nodes(text, node.start + 1, node.end - 1);
    let (local_x, local_y, pad_rotation) = children
        .iter()
        .find(|child| node_head(text, child) == "at")
        .and_then(|child| point_angle(text, child))
        .unwrap_or((0.0, 0.0, footprint_rotation));
    let mut local = Point2::new(local_x, local_y);
    if back {
        local.x = -local.x;
    }
    let offset = local.rotate(footprint_rotation);
    let at = quantize_point(Point2::new(
        footprint_at.x + offset.x,
        footprint_at.y + offset.y,
    ));
    let layers = pad_layers(text, &children, layer_names);
    if layers.is_empty() {
        return None;
    }
    let size = children
        .iter()
        .find(|child| node_head(text, child) == "size")
        .and_then(|child| point(text, child))
        .unwrap_or(Point2::new(0.0, 0.0));
    let drill = children
        .iter()
        .find(|child| node_head(text, child) == "drill")
        .and_then(|child| drill_size(text, child));
    let half =
        Point2::new(size.x.abs() / 2.0, size.y.abs() / 2.0).rotated_half_extents(pad_rotation);
    let drill_half = drill
        .map(|drill| {
            Point2::new(drill.x.abs() / 2.0, drill.y.abs() / 2.0).rotated_half_extents(pad_rotation)
        })
        .unwrap_or(Point2::new(0.0, 0.0));
    let net = if pad_type == "np_thru_hole" || !has_explicit_copper(text, &children) {
        None
    } else {
        child_net_name(text, &children, &BTreeMap::new())
    };
    Some(ParsedPad {
        number,
        net,
        at,
        layers,
        shape: atoms
            .get(3)
            .cloned()
            .unwrap_or_else(|| "unknown".to_owned()),
        size,
        drill,
        width: 2.0 * half.x.max(drill_half.x),
        height: 2.0 * half.y.max(drill_half.y),
    })
}

fn courtyard_bounds(text: &str, children: &[Node]) -> Option<Rect> {
    let mut points = Vec::new();
    for graphic in children.iter().filter(|node| {
        matches!(
            node_head(text, node),
            "fp_line" | "fp_rect" | "fp_circle" | "fp_arc" | "fp_poly" | "fp_curve"
        ) && child_nodes(text, node.start + 1, node.end - 1)
            .iter()
            .any(|child| {
                node_head(text, child) == "layer"
                    && node_atoms(text, child)
                        .get(1)
                        .is_some_and(|layer| matches!(layer.as_str(), "F.CrtYd" | "B.CrtYd"))
            })
    }) {
        let graphic_children = child_nodes(text, graphic.start + 1, graphic.end - 1);
        match node_head(text, graphic) {
            "fp_line" | "fp_rect" => {
                points.extend(child_point(text, &graphic_children, "start"));
                points.extend(child_point(text, &graphic_children, "end"));
            }
            "fp_circle" => {
                if let (Some(center), Some(end)) = (
                    child_point(text, &graphic_children, "center"),
                    child_point(text, &graphic_children, "end"),
                ) {
                    let radius = center.dist(end);
                    points.extend([
                        Point2::new(center.x - radius, center.y - radius),
                        Point2::new(center.x + radius, center.y + radius),
                    ]);
                }
            }
            "fp_arc" => {
                points.extend(child_point(text, &graphic_children, "start"));
                points.extend(child_point(text, &graphic_children, "mid"));
                points.extend(child_point(text, &graphic_children, "end"));
            }
            "fp_poly" | "fp_curve" => points.extend(polygon_points(text, graphic)),
            _ => {}
        }
    }
    Rect::bounding(&points)
}

fn read_trace(
    text: &str,
    node: &Node,
    nets: &BTreeMap<i32, String>,
    layer_names: &[String],
) -> Option<Trace> {
    let children = child_nodes(text, node.start + 1, node.end - 1);
    let start = child_point(text, &children, "start")?;
    let end = child_point(text, &children, "end")?;
    let mut path = vec![start];
    if node_head(text, node) == "arc"
        && let Some(mid) = child_point(text, &children, "mid")
    {
        path.push(mid);
    }
    path.push(end);
    let connection = child_net_name(text, &children, nets)?;
    let layer = child_text(text, &children, "layer")?;
    Some(Trace {
        connection,
        layer: layer_ref(&layer, layer_names),
        width: child_number(text, &children, "width").unwrap_or(0.0),
        path,
    })
}

fn read_track_obstacle(
    text: &str,
    node: &Node,
    nets: &BTreeMap<i32, String>,
    layer_names: &[String],
) -> Option<Obstacle> {
    let children = child_nodes(text, node.start + 1, node.end - 1);
    let start = child_point(text, &children, "start")?;
    let end = child_point(text, &children, "end")?;
    let width = child_number(text, &children, "width").unwrap_or(0.0);
    let bounds = Rect::from_points(start, end).inflate(width / 2.0);
    let layer = child_text(text, &children, "layer")?;
    let connected_to = child_net_name(text, &children, nets).into_iter().collect();
    Some(Obstacle {
        kind: "track".to_owned(),
        layers: vec![layer_ref(&layer, layer_names)],
        center: Point2::new(
            (bounds.min_x + bounds.max_x) / 2.0,
            (bounds.min_y + bounds.max_y) / 2.0,
        ),
        width: bounds.width(),
        height: bounds.height(),
        connected_to,
    })
}

fn read_via(
    text: &str,
    node: &Node,
    nets: &BTreeMap<i32, String>,
    layer_names: &[String],
) -> Option<Via> {
    let children = child_nodes(text, node.start + 1, node.end - 1);
    let at = child_point(text, &children, "at")?;
    let connection = child_net_name(text, &children, nets)?;
    let named_layers = child_atoms(text, &children, "layers");
    let indices: Vec<u32> = named_layers
        .iter()
        .filter_map(|name| layer_names.iter().position(|layer| layer == name))
        .map(|index| index as u32)
        .collect();
    let via_type = node_atoms(text, node).get(1).cloned().unwrap_or_default();
    let span = match (indices.first().copied(), indices.last().copied()) {
        (Some(from), Some(to)) if from != 0 || to != layer_names.len().saturating_sub(1) as u32 => {
            ViaSpan::Partial {
                from,
                to,
                micro: via_type == "micro",
            }
        }
        _ => ViaSpan::Through,
    };
    Some(Via {
        connection,
        at,
        diameter: child_number(text, &children, "size").unwrap_or(DEFAULT_VIA_DIAMETER_MM),
        drill: child_number(text, &children, "drill").unwrap_or(DEFAULT_VIA_DRILL_MM),
        span,
    })
}

fn read_via_obstacle(
    text: &str,
    node: &Node,
    nets: &BTreeMap<i32, String>,
    layer_names: &[String],
) -> Option<Obstacle> {
    let children = child_nodes(text, node.start + 1, node.end - 1);
    let at = child_point(text, &children, "at")?;
    let diameter = child_number(text, &children, "size").unwrap_or(DEFAULT_VIA_DIAMETER_MM);
    let named_layers = child_atoms(text, &children, "layers");
    let indices: Vec<usize> = named_layers
        .iter()
        .filter_map(|name| layer_names.iter().position(|layer| layer == name))
        .collect();
    let (from, to) = match (indices.first().copied(), indices.last().copied()) {
        (Some(from), Some(to)) => (from.min(to), from.max(to)),
        _ => (0, layer_names.len().saturating_sub(1)),
    };
    let connected_to = child_net_name(text, &children, nets).into_iter().collect();
    Some(Obstacle {
        kind: "via".to_owned(),
        layers: layer_names[from..=to]
            .iter()
            .map(|name| layer_ref(name, layer_names))
            .collect(),
        center: at,
        width: diameter,
        height: diameter,
        connected_to,
    })
}

struct ZoneSnapshot<'a> {
    obstacles: &'a mut Vec<Obstacle>,
    placement_keepouts: &'a mut Vec<Rect>,
    keepout_count: &'a mut usize,
    copper_layers: &'a mut BTreeMap<String, BTreeSet<u32>>,
}

fn read_zone(
    text: &str,
    node: &Node,
    nets: &BTreeMap<i32, String>,
    layer_names: &[String],
    outline: &Polygon,
    snapshot: &mut ZoneSnapshot<'_>,
) {
    let children = child_nodes(text, node.start + 1, node.end - 1);
    let Some(polygon) = children
        .iter()
        .find(|child| node_head(text, child) == "polygon")
    else {
        return;
    };
    let points = polygon_points(text, polygon);
    if let Some(net) = child_text(text, &children, "net_name")
        .filter(|net| !net.is_empty())
        .or_else(|| child_net_name(text, &children, nets))
        && zone_covers_board(&points, outline)
    {
        for layer in zone_layers(text, &children, layer_names) {
            if let Some(index) = layer.index(layer_names.len().max(2) as u32) {
                snapshot
                    .copper_layers
                    .entry(net.clone())
                    .or_default()
                    .insert(index);
            }
        }
    }
    let keepout = children
        .iter()
        .find(|child| node_head(text, child) == "keepout");
    let Some(keepout) = keepout else {
        return;
    };
    let flags: BTreeMap<String, String> = child_nodes(text, keepout.start + 1, keepout.end - 1)
        .into_iter()
        .filter_map(|flag| {
            let atoms = node_atoms(text, &flag);
            Some((atoms.first()?.clone(), atoms.get(1)?.clone()))
        })
        .collect();
    let has_flag = flags.values().any(|value| value == "not_allowed");
    if has_flag {
        *snapshot.keepout_count += 1;
    }
    let placement = ["footprints", "pads"]
        .iter()
        .any(|name| flags.get(*name).is_some_and(|value| value == "not_allowed"));
    if placement && let Some(bounds) = Rect::bounding(&points) {
        snapshot.placement_keepouts.push(bounds);
    }
    let routing = ["tracks", "vias"]
        .iter()
        .any(|name| flags.get(*name).is_some_and(|value| value == "not_allowed"));
    if !routing {
        return;
    }
    let layers = zone_layers(text, &children, layer_names);
    if let Some(bounds) = Rect::bounding(&points) {
        snapshot.obstacles.push(Obstacle {
            kind: "zone".to_owned(),
            layers,
            center: Point2::new(
                (bounds.min_x + bounds.max_x) / 2.0,
                (bounds.min_y + bounds.max_y) / 2.0,
            ),
            width: bounds.width(),
            height: bounds.height(),
            connected_to: Vec::new(),
        });
    }
}

fn observed_plane_nets(
    layer_count: u32,
    connections: &[Connection],
    copper_zone_layers: &BTreeMap<String, BTreeSet<u32>>,
) -> BTreeMap<String, u32> {
    let mut assigned = pcb_model::default_plane_nets(
        layer_count,
        connections
            .iter()
            .map(|connection| (connection.name.clone(), connection.points_to_connect.len())),
    );
    assigned.retain(|net, layer| {
        copper_zone_layers
            .get(net)
            .is_some_and(|layers| layers.contains(layer))
    });
    let connected: BTreeSet<_> = connections
        .iter()
        .map(|connection| connection.name.as_str())
        .collect();
    for (net, layers) in copper_zone_layers {
        if connected.contains(net.as_str())
            && !assigned.contains_key(net)
            && layers.len() == 1
            && let Some(layer) = layers.iter().next()
        {
            assigned.insert(net.clone(), *layer);
        }
    }
    assigned
}

fn zone_covers_board(points: &[Point2], outline: &Polygon) -> bool {
    let Ok(zone) = Polygon::new(points.to_vec()) else {
        return false;
    };
    polygon_is_convex(&zone)
        && outline
            .points()
            .iter()
            .all(|point| zone.contains_point(*point))
}

fn polygon_is_convex(polygon: &Polygon) -> bool {
    let points = polygon.points();
    let mut direction = 0.0_f64;
    for index in 0..points.len() {
        let a = points[index];
        let b = points[(index + 1) % points.len()];
        let c = points[(index + 2) % points.len()];
        let cross = (b.x - a.x) * (c.y - b.y) - (b.y - a.y) * (c.x - b.x);
        if cross.abs() <= geom::EPS {
            continue;
        }
        if direction != 0.0 && direction.signum() != cross.signum() {
            return false;
        }
        direction = cross;
    }
    direction != 0.0
}

fn quantize_point(point: Point2) -> Point2 {
    Point2::new(
        kicad_ipc::units::nm_to_mm(kicad_ipc::units::mm_to_nm(point.x)),
        kicad_ipc::units::nm_to_mm(kicad_ipc::units::mm_to_nm(point.y)),
    )
}

fn board_outline(text: &str, top: &[Node]) -> Result<Polygon, String> {
    let edge_nodes: Vec<_> = top
        .iter()
        .filter(|node| {
            let children = child_nodes(text, node.start + 1, node.end - 1);
            child_text(text, &children, "layer").as_deref() == Some("Edge.Cuts")
        })
        .collect();
    let mut points = Vec::new();
    let mut segments = Vec::new();
    for node in edge_nodes {
        let children = child_nodes(text, node.start + 1, node.end - 1);
        match node_head(text, node) {
            "gr_line" => {
                if let (Some(start), Some(end)) = (
                    child_point(text, &children, "start"),
                    child_point(text, &children, "end"),
                ) {
                    segments.push(Segment::new(start, end));
                }
            }
            "gr_rect" => {
                if let (Some(a), Some(c)) = (
                    child_point(text, &children, "start"),
                    child_point(text, &children, "end"),
                ) {
                    points.extend([a, Point2::new(c.x, a.y), c, Point2::new(a.x, c.y)]);
                }
            }
            "gr_arc" => {
                if let (Some(start), Some(mid), Some(end)) = (
                    child_point(text, &children, "start"),
                    child_point(text, &children, "mid"),
                    child_point(text, &children, "end"),
                ) {
                    points.extend([start, mid, end]);
                    segments.push(Segment::new(start, end));
                }
            }
            "gr_circle" => {
                if let (Some(center), Some(end)) = (
                    child_point(text, &children, "center"),
                    child_point(text, &children, "end"),
                ) {
                    let radius = center.dist(end);
                    points.extend((0..32).map(|index| {
                        let theta = index as f64 * std::f64::consts::TAU / 32.0;
                        Point2::new(
                            center.x + radius * theta.cos(),
                            center.y + radius * theta.sin(),
                        )
                    }));
                }
            }
            "gr_poly" | "gr_curve" => points.extend(polygon_points(text, node)),
            _ => {}
        }
    }
    let ordered = Polyline::from_unordered_segments(segments)
        .map(Polyline::into_points)
        .filter(|ordered| ordered.len() >= 3)
        .unwrap_or(points);
    Polygon::new(ordered).map_err(|_| "board has no closed Edge.Cuts outline".to_owned())
}

fn board_rules(text: &str, top: &[Node]) -> FileRules {
    let mut rules = FileRules::default();
    let mut classes = BTreeMap::<String, (f64, Vec<String>)>::new();
    for class in top
        .iter()
        .filter(|node| node_head(text, node) == "net_class")
    {
        let atoms = node_atoms(text, class);
        let Some(name) = atoms.get(1).cloned() else {
            continue;
        };
        let children = child_nodes(text, class.start + 1, class.end - 1);
        let width = child_number(text, &children, "trace_width");
        let clearance = child_number(text, &children, "clearance");
        let diameter = child_number(text, &children, "via_dia");
        let drill = child_number(text, &children, "via_drill");
        if name == "Default" {
            rules.min_trace_width = width.unwrap_or(rules.min_trace_width);
            rules.clearance = clearance.unwrap_or(rules.clearance);
            rules.via_diameter = diameter.unwrap_or(rules.via_diameter);
            rules.via_drill = drill.unwrap_or(rules.via_drill);
        } else if let Some(width) = width {
            classes.insert(name, (width, child_values(text, &children, "add_net")));
        }
    }
    for (_, (width, nets)) in classes {
        for net in nets {
            rules.net_widths.insert(net, width);
        }
    }
    rules
}

pub(crate) fn file_rules(path: &Path, text: &str, top: &[Node]) -> Result<FileRules, String> {
    let mut rules = board_rules(text, top);
    let project_path = path.with_extension("kicad_pro");
    let project = match std::fs::read_to_string(&project_path) {
        Ok(project) => project,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(rules),
        Err(err) => {
            return Err(format!(
                "could not read project net classes {}: {err}",
                project_path.display()
            ));
        }
    };
    let root: serde_json::Value = serde_json::from_str(&project)
        .map_err(|err| format!("invalid project JSON {}: {err}", project_path.display()))?;
    if let Some(default) = root
        .pointer("/net_settings/classes")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .find(|class| class.get("name").and_then(serde_json::Value::as_str) == Some("Default"))
    {
        rules.min_trace_width = default
            .get("track_width")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(rules.min_trace_width);
        rules.clearance = default
            .get("clearance")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(rules.clearance);
        rules.via_diameter = default
            .get("via_diameter")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(rules.via_diameter);
        rules.via_drill = default
            .get("via_drill")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(rules.via_drill);
    }
    rules
        .net_widths
        .extend(crate::project_net_widths(&project)?);
    Ok(rules)
}

fn net_table(text: &str, top: &[Node]) -> BTreeMap<i32, String> {
    top.iter()
        .filter(|node| node_head(text, node) == "net")
        .filter_map(|node| {
            let atoms = node_atoms(text, node);
            Some((atoms.get(1)?.parse().ok()?, atoms.get(2)?.clone()))
        })
        .collect()
}

fn property(text: &str, node: &Node, name: &str) -> Option<String> {
    let atoms = node_atoms(text, node);
    (atoms.get(1).map(String::as_str) == Some(name)).then(|| atoms.get(2).cloned())?
}

fn point(text: &str, node: &Node) -> Option<Point2> {
    let atoms = node_atoms(text, node);
    Some(Point2::new(
        atoms.get(1)?.parse().ok()?,
        atoms.get(2)?.parse().ok()?,
    ))
}

fn point_angle(text: &str, node: &Node) -> Option<(f64, f64, f64)> {
    let atoms = node_atoms(text, node);
    Some((
        atoms.get(1)?.parse().ok()?,
        atoms.get(2)?.parse().ok()?,
        atoms
            .get(3)
            .and_then(|value| value.parse().ok())
            .unwrap_or(0.0),
    ))
}

fn drill_size(text: &str, node: &Node) -> Option<Point2> {
    let atoms = node_atoms(text, node);
    let start = usize::from(atoms.get(1).is_some_and(|atom| atom == "oval")) + 1;
    let x: f64 = atoms.get(start)?.parse().ok()?;
    let y = atoms
        .get(start + 1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(x);
    Some(Point2::new(x, y))
}

fn child_point(text: &str, children: &[Node], head: &str) -> Option<Point2> {
    children
        .iter()
        .find(|node| node_head(text, node) == head)
        .and_then(|node| point(text, node))
}

fn child_number(text: &str, children: &[Node], head: &str) -> Option<f64> {
    children
        .iter()
        .find(|node| node_head(text, node) == head)
        .and_then(|node| node_atoms(text, node).get(1)?.parse().ok())
}

fn child_text(text: &str, children: &[Node], head: &str) -> Option<String> {
    children
        .iter()
        .find(|node| node_head(text, node) == head)
        .and_then(|node| node_atoms(text, node).get(1).cloned())
}

fn child_net_name(text: &str, children: &[Node], nets: &BTreeMap<i32, String>) -> Option<String> {
    let atoms = children
        .iter()
        .find(|node| node_head(text, node) == "net")
        .map(|node| node_atoms(text, node))?;
    let name = match atoms.as_slice() {
        [_, code, name, ..] if code.parse::<i32>().is_ok() => name.clone(),
        [_, code_or_name, ..] => code_or_name
            .parse::<i32>()
            .ok()
            .and_then(|code| nets.get(&code).cloned())
            .unwrap_or_else(|| code_or_name.clone()),
        _ => return None,
    };
    (!name.is_empty()).then_some(name)
}

fn child_atoms(text: &str, children: &[Node], head: &str) -> Vec<String> {
    children
        .iter()
        .find(|node| node_head(text, node) == head)
        .map(|node| node_atoms(text, node).into_iter().skip(1).collect())
        .unwrap_or_default()
}

fn child_values(text: &str, children: &[Node], head: &str) -> Vec<String> {
    children
        .iter()
        .filter(|node| node_head(text, node) == head)
        .filter_map(|node| node_atoms(text, node).get(1).cloned())
        .collect()
}

fn pad_layers(text: &str, children: &[Node], layer_names: &[String]) -> Vec<LayerRef> {
    let names = child_atoms(text, children, "layers");
    let mut layers: Vec<LayerRef> = if names
        .iter()
        .any(|name| matches!(name.as_str(), "*.Cu" | "F&B.Cu"))
    {
        layer_names
            .iter()
            .map(|name| layer_ref(name, layer_names))
            .collect()
    } else {
        names
            .iter()
            .filter(|name| is_copper_layer(name))
            .map(|name| layer_ref(name, layer_names))
            .collect()
    };
    layers.sort_by_key(|layer| layer.index(layer_names.len() as u32).unwrap_or(u32::MAX));
    layers.dedup();
    layers
}

fn has_explicit_copper(text: &str, children: &[Node]) -> bool {
    child_atoms(text, children, "layers")
        .iter()
        .any(|name| matches!(name.as_str(), "*.Cu" | "F&B.Cu") || is_copper_layer(name))
}

fn zone_layers(text: &str, children: &[Node], layer_names: &[String]) -> Vec<LayerRef> {
    let mut names = child_atoms(text, children, "layers");
    if names.is_empty()
        && let Some(layer) = child_text(text, children, "layer")
    {
        names.push(layer);
    }
    if names.is_empty()
        || names
            .iter()
            .any(|name| matches!(name.as_str(), "*.Cu" | "F&B.Cu"))
    {
        return layer_names
            .iter()
            .map(|name| layer_ref(name, layer_names))
            .collect();
    }
    names
        .iter()
        .filter(|name| is_copper_layer(name))
        .map(|name| layer_ref(name, layer_names))
        .collect()
}

fn polygon_points(text: &str, node: &Node) -> Vec<Point2> {
    let mut points = Vec::new();
    let mut stack = vec![(node.start + 1, node.end - 1)];
    while let Some((start, end)) = stack.pop() {
        for child in child_nodes(text, start, end) {
            if node_head(text, &child) == "xy" {
                if let Some(point) = point(text, &child) {
                    points.push(point);
                }
            } else {
                stack.push((child.start + 1, child.end - 1));
            }
        }
    }
    points
}

fn layer_ref(name: &str, layer_names: &[String]) -> LayerRef {
    match name {
        "F.Cu" => LayerRef::top(),
        "B.Cu" => LayerRef::bottom(),
        _ => layer_names
            .iter()
            .position(|layer| layer == name)
            .map(|index| LayerRef(format!("inner{index}")))
            .unwrap_or_else(|| LayerRef(name.to_owned())),
    }
}

fn is_copper_layer(name: &str) -> bool {
    name == "F.Cu" || name == "B.Cu" || (name.starts_with("In") && name.ends_with(".Cu"))
}

fn node_atoms(text: &str, node: &Node) -> Vec<String> {
    let body = &text[node.start + 1..node.end - 1];
    let mut atoms = Vec::new();
    let mut chars = body.chars().peekable();
    while let Some(character) = chars.next() {
        if character.is_whitespace() {
            continue;
        }
        if character == '(' {
            break;
        }
        if character == '"' {
            let mut value = String::new();
            let mut escaped = false;
            for character in chars.by_ref() {
                if escaped {
                    value.push(character);
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    break;
                } else {
                    value.push(character);
                }
            }
            atoms.push(value);
        } else {
            let mut value = String::from(character);
            while chars
                .peek()
                .is_some_and(|next| !next.is_whitespace() && *next != '(' && *next != ')')
            {
                value.push(chars.next().expect("peeked character"));
            }
            atoms.push(value);
        }
    }
    atoms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_saved_board_without_a_session() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pcb-workflow/tests/fixtures/two_res.kicad_pcb");
        let snapshot = read_snapshot(&path).unwrap();

        assert_eq!(snapshot.imported.parts.len(), 2);
        assert_eq!(snapshot.imported.parts[0].reference, "R1");
        assert_eq!(snapshot.imported.parts[0].side, BoardSide::Front);
        assert_eq!(snapshot.imported.parts[0].pads[0].shape, "roundrect");
        assert_eq!(
            snapshot.imported.parts[0].pads[0].size,
            Point2::new(1.025, 1.4)
        );
        assert_eq!(snapshot.imported.parts[0].pads[0].drill, None);
        assert_eq!(snapshot.imported.parts[1].rotation, 90);
        assert_eq!(snapshot.problem.connections.len(), 2);
        assert_eq!(snapshot.problem.bounds, Rect::new(0.0, 0.0, 30.0, 20.0));
        assert_eq!(snapshot.problem.obstacles.len(), 4);
    }

    #[test]
    fn reads_an_outline_only_board_as_an_empty_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(
            &path,
            r#"(kicad_pcb
 (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (44 "Edge.Cuts" user))
 (gr_rect (start 0 0) (end 10 10) (layer "Edge.Cuts")))"#,
        )
        .unwrap();

        let snapshot = read_snapshot(&path).unwrap();
        assert!(snapshot.imported.parts.is_empty());
        assert!(snapshot.problem.connections.is_empty());
        assert!(snapshot.copper.traces.is_empty());
        assert!(snapshot.copper.vias.is_empty());
    }

    #[test]
    fn reads_direct_named_nets_without_a_top_level_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(
            &path,
            r#"(kicad_pcb
 (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (44 "Edge.Cuts" user))
 (gr_rect (start 0 0) (end 10 10) (layer "Edge.Cuts"))
 (footprint "Test:Pad" (layer "F.Cu") (at 2 2)
   (property "Reference" "J1")
   (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net "SIG")))
 (footprint "Test:Pad" (layer "F.Cu") (at 8 8)
   (property "Reference" "J2")
   (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net "SIG")))
 (segment (start 2 2) (end 5 5) (width 0.25) (layer "F.Cu") (net "SIG"))
 (via (at 5 5) (size 0.7) (drill 0.35) (layers "F.Cu" "B.Cu") (net "SIG"))
 (segment (start 5 5) (end 8 8) (width 0.25) (layer "B.Cu") (net "SIG")))"#,
        )
        .unwrap();

        let snapshot = read_snapshot(&path).unwrap();
        assert_eq!(snapshot.problem.connections.len(), 1);
        assert_eq!(snapshot.problem.connections[0].name, "SIG");
        assert_eq!(
            snapshot.imported.parts[0].pads[0].net.as_deref(),
            Some("SIG")
        );
        assert_eq!(snapshot.copper.traces.len(), 2);
        assert_eq!(snapshot.copper.vias.len(), 1);
    }

    #[test]
    fn reads_copper_keepouts_and_embedded_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(
            &path,
            r#"(kicad_pcb
 (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (44 "Edge.Cuts" user))
 (net 0 "") (net 1 "GND")
 (net_class "Default" "" (clearance 0.15) (trace_width 0.25) (via_dia 0.7) (via_drill 0.35) (add_net "GND"))
 (gr_rect (start 0 0) (end 10 10) (layer "Edge.Cuts"))
 (footprint "Test:Pad" (layer "F.Cu") (at 2 2)
   (property "Reference" "J1")
   (pad "1" thru_hole circle (at 0 0) (size 1 1) (drill 0.6) (layers "*.Cu") (net 1 "GND")))
 (footprint "Test:Pad" (layer "B.Cu") (at 8 8)
   (property "Reference" "J2")
   (fp_rect (start -1 -2) (end 1 2) (layer "B.CrtYd"))
   (pad "1" smd rect (at 0 0) (size 1 2) (layers "B.Cu") (net 1 "GND")))
 (segment (start 2 2) (end 8 8) (width 0.25) (layer "F.Cu") (net 1))
 (via (at 5 5) (size 0.7) (drill 0.35) (layers "F.Cu" "B.Cu") (net 1))
 (zone (net 0) (net_name "") (layers "*.Cu")
   (keepout (tracks not_allowed) (vias not_allowed) (pads allowed) (footprints not_allowed))
   (polygon (pts (xy 4 0) (xy 6 0) (xy 6 10) (xy 4 10))))
)"#,
        )
        .unwrap();

        let snapshot = read_snapshot(&path).unwrap();
        assert_eq!(snapshot.problem.min_trace_width, 0.25);
        assert_eq!(snapshot.problem.clearance, 0.15);
        assert_eq!(snapshot.problem.via_diameter, 0.7);
        assert_eq!(snapshot.problem.via_drill, 0.35);
        assert_eq!(snapshot.copper.traces.len(), 1);
        assert_eq!(snapshot.copper.vias.len(), 1);
        assert_eq!(snapshot.imported.keepout_count, 1);
        assert_eq!(snapshot.imported.placement_keepouts.len(), 1);
        assert_eq!(snapshot.problem.connections.len(), 1);
        assert_eq!(
            snapshot.imported.parts[0].pads[0].drill,
            Some(Point2::new(0.6, 0.6))
        );
        assert_eq!(snapshot.imported.parts[1].side, BoardSide::Back);
        assert_eq!(
            snapshot.imported.parts[1].courtyard,
            Some(Rect::new(-1.0, -2.0, 1.0, 2.0))
        );
    }

    #[test]
    fn project_rules_override_embedded_defaults_and_assign_named_widths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(
            &path,
            r#"(kicad_pcb
 (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (44 "Edge.Cuts" user))
 (net 0 "") (net 1 "SIG")
 (net_class "Default" "" (clearance 0.15) (trace_width 0.25) (via_dia 0.7) (via_drill 0.35) (add_net "SIG"))
 (gr_rect (start 0 0) (end 10 10) (layer "Edge.Cuts"))
 (footprint "Test:Pad" (layer "F.Cu") (at 2 2)
   (property "Reference" "J1")
   (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "SIG")))
 (footprint "Test:Pad" (layer "F.Cu") (at 8 8)
   (property "Reference" "J2")
   (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "SIG"))))"#,
        )
        .unwrap();
        std::fs::write(
            path.with_extension("kicad_pro"),
            r#"{
 "board": {"design_settings": {"rules": {"min_clearance": 0.18}}},
 "net_settings": {
   "classes": [
     {"name": "Default", "clearance": 0.2, "track_width": 0.3, "via_diameter": 0.8, "via_drill": 0.4},
     {"name": "Signal", "clearance": 0.2, "track_width": 0.5, "via_diameter": 0.8, "via_drill": 0.4}
   ],
   "netclass_patterns": [{"netclass": "Signal", "pattern": "SIG"}]
 }
}"#,
        )
        .unwrap();

        let snapshot = read_snapshot(&path).unwrap();
        assert_eq!(snapshot.problem.min_trace_width, 0.3);
        assert_eq!(snapshot.problem.clearance, 0.2);
        assert_eq!(snapshot.problem.via_diameter, 0.8);
        assert_eq!(snapshot.problem.via_drill, 0.4);
        assert_eq!(snapshot.problem.net_widths["SIG"], 0.5);
    }

    #[test]
    #[ignore = "live KiCad IPC comparison; run through tools/live_kicad_test.sh"]
    fn saved_fixture_matches_live_snapshot_field_for_field() {
        let Some(env) = kicad::KicadInstallation::detect() else {
            eprintln!("SKIP: no KiCad installation detected");
            return;
        };
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pcb-workflow/tests/fixtures/two_res.kicad_pcb");
        let offline = read_snapshot(&path).unwrap();
        let sessions = kicad_ipc::SessionManager::with_installation(
            env.pcbnew_path().to_path_buf(),
            env.major_version(),
            false,
            false,
        );
        let live = match crate::active::read_live_snapshot(&path, &sessions) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                eprintln!("SKIP: headless pcbnew unavailable: {error}");
                sessions.close();
                return;
            }
        };
        sessions.close();
        assert_eq!(offline, live);
    }
}
