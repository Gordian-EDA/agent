//! Live-board adapters from KiCad IPC protobufs into `pcb_model`.

use std::collections::{BTreeMap, BTreeSet};

use geom::Polyline;

use crate::{
    Error, Kicad, footprint_reference,
    proto::kiapi::{
        board::types::{
            BoardGraphicShape, BoardLayer, FootprintInstance, Net, Pad, PadStack, PadStackShape,
            Track, Via, ViaType, Zone, zone,
        },
        common::{
            project::NetClass,
            types::{LockedState, PolySet, Vector2, graphic_shape::Geometry, poly_line_node},
        },
    },
};
use pcb_model::{
    Connection, LayerRef, Obstacle, Point2, Polygon, Rect, RoutePoint, RouteProblem, RouteSolution,
    Segment, Trace, Via as ModelVia, ViaSpan,
    place::{LockedAt, Part, PartPad, PlaceProblem},
};

const DEFAULT_MIN_TRACE_WIDTH_MM: f64 = 0.2;
const DEFAULT_CLEARANCE_MM: f64 = 0.2;
const DEFAULT_VIA_DIAMETER_MM: f64 = 0.6;
const DEFAULT_VIA_DRILL_MM: f64 = 0.3;

#[derive(Debug, Clone)]
struct BoardRules {
    min_trace_width: f64,
    clearance: f64,
    via_diameter: f64,
    via_drill: f64,
    net_widths: BTreeMap<String, f64>,
}

/// Routing and footprint views of the currently open KiCad board.
#[derive(Debug, Clone)]
pub struct IpcBoardSnapshot {
    pub problem: RouteProblem,
    pub imported: ImportedBoard,
    pub copper: RouteSolution,
    pub net_codes: BTreeMap<String, i32>,
    pub layer_names: Vec<String>,
}

/// Footprint-level board data imported from a live KiCad IPC board.
#[derive(Debug, Clone)]
pub struct ImportedBoard {
    pub layer_count: u32,
    pub bounds: Rect,
    pub parts: Vec<ImportedPart>,
    /// Rule-area bounds that disallow footprint/pad placement.
    pub placement_keepouts: Vec<Rect>,
    /// Number of source rule areas with at least one basic keepout flag.
    pub keepout_count: usize,
}

/// One placed footprint recovered from IPC.
#[derive(Debug, Clone)]
pub struct ImportedPart {
    pub reference: String,
    pub lib_id: String,
    pub at: Point2,
    pub rotation: i32,
    pub locked: bool,
    pub pads: Vec<(String, Option<String>)>,
}

impl IpcBoardSnapshot {
    /// Convert the imported footprint view into a placement problem.
    pub fn place_problem(&self) -> PlaceProblem {
        let parts = self
            .imported
            .parts
            .iter()
            .map(|part| {
                let footprint = self
                    .problem
                    .obstacles
                    .iter()
                    .filter(|ob| ob.kind == format!("pad:{}", part.reference))
                    .collect::<Vec<_>>();
                let (courtyard_w, courtyard_h) =
                    footprint_extents(&footprint).unwrap_or((1.0, 1.0));
                Part {
                    reference: part.reference.clone(),
                    courtyard_w,
                    courtyard_h,
                    pads: footprint
                        .into_iter()
                        .enumerate()
                        .map(|(idx, ob)| PartPad {
                            number: part
                                .pads
                                .get(idx)
                                .map(|(n, _)| n.clone())
                                .unwrap_or_else(|| (idx + 1).to_string()),
                            offset: Point2 {
                                x: ob.center.x - part.at.x,
                                y: ob.center.y - part.at.y,
                            },
                            width: ob.width,
                            height: ob.height,
                            layers: ob.layers.clone(),
                            net: ob.connected_to.first().cloned(),
                        })
                        .collect(),
                    locked: part.locked.then_some(LockedAt {
                        at: part.at,
                        rotation: part.rotation as f64,
                    }),
                }
            })
            .collect();
        PlaceProblem {
            bounds: self.imported.bounds,
            clearance: self.problem.clearance,
            layer_count: self.problem.layer_count,
            min_trace_width: self.problem.min_trace_width,
            parts,
            keepouts: self.imported.placement_keepouts.clone(),
            outline: self.problem.outline.clone(),
        }
    }
}

impl Kicad {
    /// Read the open IPC board once and derive model-level board snapshots.
    pub fn board_snapshot(&mut self) -> Result<IpcBoardSnapshot, Error> {
        let footprints = self.footprints()?;
        let tracks = self.tracks()?;
        let vias = self.vias()?;
        let zones = self.zones()?;
        let shapes = self.board_shapes()?;
        let nets = self.net_list()?;
        let layer_names = match self.enabled_layers() {
            Ok(layers) => copper_layer_names(layers.copper_layer_count),
            Err(err) if is_unimplemented(&err) => {
                infer_layer_names(&footprints, &tracks, &vias, &zones)
            }
            Err(err) => return Err(err),
        };
        let outline = edge_cuts_outline(&shapes)
            .ok_or_else(|| Error::NotFound("Edge.Cuts board outline from KiCAD IPC".to_owned()))?;
        let outline = Polygon::new(outline).map_err(Error::Unsupported)?;
        let rules = match (self.net_classes(), self.net_classes_for_nets(nets.clone())) {
            (Ok(net_classes), Ok(effective)) => board_rules(net_classes, effective)?,
            (Err(err), _) | (_, Err(err)) if is_unimplemented(&err) => default_rules(),
            (Err(err), _) | (_, Err(err)) => return Err(err),
        };
        Ok(snapshot_from_items_with_context(
            footprints,
            tracks,
            vias,
            zones,
            nets,
            layer_names,
            Some(outline),
            rules,
        ))
    }

    /// A `pcb_model::RouteProblem` derived from the live IPC board.
    pub fn route_problem(&mut self) -> Result<RouteProblem, Error> {
        Ok(self.board_snapshot()?.problem)
    }

    /// Footprint-level import data derived from the live IPC board.
    pub fn imported_board(&mut self) -> Result<ImportedBoard, Error> {
        Ok(self.board_snapshot()?.imported)
    }

    /// Routed copper derived from the live IPC board.
    pub fn copper_solution(&mut self) -> Result<RouteSolution, Error> {
        Ok(self.board_snapshot()?.copper)
    }
}

/// Build model snapshots from IPC item payloads.
pub fn snapshot_from_items(
    footprints: Vec<FootprintInstance>,
    tracks: Vec<Track>,
    vias: Vec<Via>,
    zones: Vec<Zone>,
    nets: Vec<Net>,
) -> IpcBoardSnapshot {
    let layer_names = infer_layer_names(&footprints, &tracks, &vias, &zones);
    snapshot_from_items_with_context(
        footprints,
        tracks,
        vias,
        zones,
        nets,
        layer_names,
        None,
        default_rules(),
    )
}

#[allow(clippy::too_many_arguments)]
fn snapshot_from_items_with_context(
    footprints: Vec<FootprintInstance>,
    tracks: Vec<Track>,
    vias: Vec<Via>,
    zones: Vec<Zone>,
    nets: Vec<Net>,
    layer_names: Vec<String>,
    outline: Option<Polygon>,
    rules: BoardRules,
) -> IpcBoardSnapshot {
    let copper = copper_from_ipc(&tracks, &vias, &layer_names);
    SnapshotBuilder::new(layer_names, outline, rules)
        .with_footprints(&footprints)
        .with_tracks(&tracks)
        .with_vias(&vias)
        .with_zones(&zones)
        .finish(&nets, copper)
}

struct SnapshotBuilder {
    layer_names: Vec<String>,
    outline: Option<Polygon>,
    rules: BoardRules,
    obstacles: Vec<Obstacle>,
    net_points: BTreeMap<String, Vec<RoutePoint>>,
    copper_zone_layers: BTreeMap<String, BTreeSet<u32>>,
    placement_keepouts: Vec<Rect>,
    keepout_count: usize,
    parts: Vec<ImportedPart>,
}

impl SnapshotBuilder {
    fn new(layer_names: Vec<String>, outline: Option<Polygon>, rules: BoardRules) -> Self {
        Self {
            layer_names,
            outline,
            rules,
            obstacles: Vec::new(),
            net_points: BTreeMap::new(),
            copper_zone_layers: BTreeMap::new(),
            placement_keepouts: Vec::new(),
            keepout_count: 0,
            parts: Vec::new(),
        }
    }

    fn with_footprints(mut self, footprints: &[FootprintInstance]) -> Self {
        for fp in footprints {
            self.push_footprint(fp);
        }
        self
    }

    fn with_tracks(mut self, tracks: &[Track]) -> Self {
        for track in tracks {
            self.push_track(track);
        }
        self
    }

    fn with_vias(mut self, vias: &[Via]) -> Self {
        for via in vias {
            self.push_via(via);
        }
        self
    }

    fn with_zones(mut self, zones: &[Zone]) -> Self {
        for zone in zones {
            self.push_zone(zone);
        }
        self
    }

    fn push_footprint(&mut self, fp: &FootprintInstance) {
        let reference = footprint_reference(fp);
        if reference.is_empty() {
            return;
        }
        let at = fp
            .position
            .as_ref()
            .map(point)
            .unwrap_or(Point2 { x: 0.0, y: 0.0 });
        let rotation = geom::snap_quadrant(
            fp.orientation
                .as_ref()
                .map(|a| a.value_degrees)
                .unwrap_or(0.0),
        );
        let lib_id = fp
            .definition
            .as_ref()
            .and_then(|d| d.id.as_ref())
            .map(|id| format!("{}:{}", id.library_nickname, id.entry_name))
            .unwrap_or_default();
        let locked = fp.locked == LockedState::LsLocked as i32;
        let mut pads = Vec::new();
        if let Some(definition) = &fp.definition {
            for item in &definition.items {
                let Ok(pad) = item.to_msg::<Pad>() else {
                    continue;
                };
                let center = pad_world(fp, &pad);
                let layers = pad_layers(&pad, &self.layer_names);
                let (width, height) = pad_size(&pad);
                let half = Point2::new(width / 2.0, height / 2.0)
                    .rotated_half_extents(pad_angle(fp, &pad));
                let net = pad.net.as_ref().and_then(net_name);
                self.obstacles.push(Obstacle {
                    kind: format!("pad:{reference}"),
                    layers: layers.clone(),
                    center,
                    width: half.x * 2.0,
                    height: half.y * 2.0,
                    connected_to: net.clone().into_iter().collect(),
                });
                if let Some(net) = &net {
                    self.net_points
                        .entry(net.clone())
                        .or_default()
                        .push(RoutePoint {
                            x: center.x,
                            y: center.y,
                            layer: layers.first().cloned().unwrap_or_else(LayerRef::top),
                        });
                }
                pads.push((pad.number, net));
            }
        }
        self.parts.push(ImportedPart {
            reference,
            lib_id,
            at,
            rotation: rotation as i32,
            locked,
            pads,
        });
    }

    fn push_track(&mut self, track: &Track) {
        let (Some(start), Some(end)) = (&track.start, &track.end) else {
            return;
        };
        let width = nm_to_mm(track.width.as_ref().map(|w| w.value_nm).unwrap_or(0));
        let start = point(start);
        let end = point(end);
        let min_x = start.x.min(end.x) - width / 2.0;
        let max_x = start.x.max(end.x) + width / 2.0;
        let min_y = start.y.min(end.y) - width / 2.0;
        let max_y = start.y.max(end.y) + width / 2.0;
        self.obstacles.push(Obstacle {
            kind: "track".to_owned(),
            layers: vec![layer_ref_for_i32(track.layer, &self.layer_names)],
            center: Point2 {
                x: (min_x + max_x) / 2.0,
                y: (min_y + max_y) / 2.0,
            },
            width: max_x - min_x,
            height: max_y - min_y,
            connected_to: track.net.as_ref().and_then(net_name).into_iter().collect(),
        });
    }

    fn push_via(&mut self, via: &Via) {
        let Some(position) = &via.position else {
            return;
        };
        let diameter = via_diameter(via);
        self.obstacles.push(Obstacle {
            kind: "via".to_owned(),
            layers: via_layers(via, &self.layer_names),
            center: point(position),
            width: diameter,
            height: diameter,
            connected_to: via.net.as_ref().and_then(net_name).into_iter().collect(),
        });
    }

    fn push_zone(&mut self, zone: &Zone) {
        let points = polyset_points(zone.outline.as_ref());
        if zone_has_keepout_flags(zone) {
            self.keepout_count += 1;
        }
        if let Some(zone::Settings::CopperSettings(settings)) = &zone.settings
            && let Some(net) = settings.net.as_ref().and_then(net_name)
            && zone_covers_board(&points, self.outline.as_ref())
        {
            let layer_count = self.layer_names.len().max(2) as u32;
            for layer in zone_layers(zone, &self.layer_names) {
                if let Some(idx) = layer.index(layer_count)
                    && idx > 0
                    && idx + 1 < layer_count
                {
                    self.copper_zone_layers
                        .entry(net.clone())
                        .or_default()
                        .insert(idx);
                }
            }
        }
        if zone_is_placement_keepout(zone)
            && let Some(bounds) = Rect::bounding(&points)
        {
            self.placement_keepouts.push(bounds);
        }
        // Copper pours adapt around tracks, pads, and vias when KiCad refills
        // them. Treating their outline bbox as fixed copper makes the router see
        // a board-sized obstacle (and can invent cross-plane shorts). Plane
        // connectivity is modeled separately by `RouteProblem::plane_nets`.
        if !zone_is_routing_keepout(zone) {
            return;
        }
        let layers = zone_layers(zone, &self.layer_names);
        if let Some(obstacle) = bbox_obstacle("zone", layers, Vec::new(), &points) {
            self.obstacles.push(obstacle);
        }
    }

    fn finish(self, nets: &[Net], copper: RouteSolution) -> IpcBoardSnapshot {
        let layer_count = self.layer_names.len().max(2) as u32;
        let bounds = self
            .outline
            .as_ref()
            .map(Polygon::bbox)
            .or_else(|| bounds_from_obstacles(&self.obstacles))
            .unwrap_or(Rect {
                min_x: 0.0,
                max_x: 0.0,
                min_y: 0.0,
                max_y: 0.0,
            });
        let connections: Vec<Connection> = self
            .net_points
            .into_iter()
            .filter(|(_, points)| points.len() >= 2)
            .map(|(name, points_to_connect)| Connection {
                name,
                points_to_connect,
            })
            .collect();

        let plane_nets = observed_plane_nets(layer_count, &connections, &self.copper_zone_layers);
        let problem = RouteProblem {
            layer_count,
            min_trace_width: self.rules.min_trace_width,
            obstacles: self.obstacles,
            connections,
            bounds,
            clearance: self.rules.clearance,
            via_diameter: self.rules.via_diameter,
            via_drill: self.rules.via_drill,
            net_widths: self.rules.net_widths,
            outline: self.outline,
            escape_layers: BTreeMap::new(),
            plane_nets,
        };
        let imported = ImportedBoard {
            layer_count,
            bounds,
            parts: self.parts,
            placement_keepouts: self.placement_keepouts,
            keepout_count: self.keepout_count,
        };
        IpcBoardSnapshot {
            problem,
            imported,
            copper,
            net_codes: net_codes(nets),
            layer_names: self.layer_names,
        }
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
    assigned
}

fn copper_from_ipc(tracks: &[Track], vias: &[Via], layer_names: &[String]) -> RouteSolution {
    let traces = tracks
        .iter()
        .filter_map(|track| {
            let (Some(start), Some(end), Some(net)) = (&track.start, &track.end, &track.net) else {
                return None;
            };
            if net.name.is_empty() {
                return None;
            }
            Some(Trace {
                connection: net.name.clone(),
                layer: layer_ref_for_i32(track.layer, layer_names),
                width: nm_to_mm(track.width.as_ref().map(|w| w.value_nm).unwrap_or(0)),
                path: vec![point(start), point(end)],
            })
        })
        .collect();
    let vias = vias
        .iter()
        .filter_map(|via| {
            let (Some(position), Some(net)) = (&via.position, &via.net) else {
                return None;
            };
            if net.name.is_empty() {
                return None;
            }
            let (diameter, drill) = via_geometry(via);
            Some(ModelVia {
                connection: net.name.clone(),
                at: point(position),
                diameter,
                drill,
                span: via_span(via, layer_names),
            })
        })
        .collect();
    RouteSolution { traces, vias }
}

fn via_geometry(via: &Via) -> (f64, f64) {
    let diameter = via
        .pad_stack
        .as_ref()
        .and_then(|stack| {
            stack
                .copper_layers
                .iter()
                .find_map(|layer| layer.size.as_ref())
        })
        .map(|size| nm_to_mm(size.x_nm.max(size.y_nm)))
        .unwrap_or(DEFAULT_VIA_DIAMETER_MM);
    let drill = via
        .pad_stack
        .as_ref()
        .and_then(|stack| stack.drill.as_ref())
        .and_then(|drill| drill.diameter.as_ref())
        .map(|diameter| nm_to_mm(diameter.x_nm.max(diameter.y_nm)))
        .unwrap_or(DEFAULT_VIA_DRILL_MM);
    (diameter, drill)
}

fn via_span(via: &Via, layer_names: &[String]) -> ViaSpan {
    let via_type = ViaType::try_from(via.r#type).unwrap_or(ViaType::VtThrough);
    let micro = via_type == ViaType::VtMicro;
    if via_type == ViaType::VtThrough {
        return ViaSpan::Through;
    }
    let Some(drill) = via
        .pad_stack
        .as_ref()
        .and_then(|stack| stack.drill.as_ref())
    else {
        return ViaSpan::Through;
    };
    let Some(from) = copper_index(drill.start_layer, layer_names) else {
        return ViaSpan::Through;
    };
    let Some(to) = copper_index(drill.end_layer, layer_names) else {
        return ViaSpan::Through;
    };
    if from == 0 && to == layer_names.len().saturating_sub(1) as u32 {
        ViaSpan::Through
    } else {
        ViaSpan::Partial { from, to, micro }
    }
}

fn copper_layer_names(copper_layer_count: u32) -> Vec<String> {
    let count = copper_layer_count.max(2);
    let mut names = Vec::with_capacity(count as usize);
    names.push("F.Cu".to_owned());
    for idx in 1..count.saturating_sub(1) {
        names.push(format!("In{idx}.Cu"));
    }
    names.push("B.Cu".to_owned());
    names
}

fn default_rules() -> BoardRules {
    BoardRules {
        min_trace_width: DEFAULT_MIN_TRACE_WIDTH_MM,
        clearance: DEFAULT_CLEARANCE_MM,
        via_diameter: DEFAULT_VIA_DIAMETER_MM,
        via_drill: DEFAULT_VIA_DRILL_MM,
        net_widths: BTreeMap::new(),
    }
}

fn is_unimplemented(err: &Error) -> bool {
    matches!(err, Error::Api { code: 5, message } if message.contains("no handler available"))
}

fn board_rules(
    net_classes: Vec<NetClass>,
    effective: BTreeMap<String, NetClass>,
) -> Result<BoardRules, Error> {
    let default = net_classes
        .iter()
        .find(|class| class.name == "Default")
        .or_else(|| net_classes.iter().find(|class| class.board.is_some()))
        .ok_or_else(|| Error::NotFound("KiCAD default netclass rules".to_owned()))?;
    let default_board = default
        .board
        .as_ref()
        .ok_or_else(|| Error::NotFound("KiCAD default netclass board rules".to_owned()))?;
    let min_trace_width = distance_mm(default_board.track_width.as_ref())
        .ok_or_else(|| Error::NotFound("KiCAD default track width".to_owned()))?;
    let clearance = distance_mm(default_board.clearance.as_ref())
        .ok_or_else(|| Error::NotFound("KiCAD default clearance".to_owned()))?;
    let (via_diameter, via_drill) = via_rule(default_board.via_stack.as_ref())?;
    let mut net_widths = BTreeMap::new();
    for (net, class) in effective {
        let Some(board) = class.board.as_ref() else {
            continue;
        };
        if let Some(width) = distance_mm(board.track_width.as_ref())
            && (width - min_trace_width).abs() > 1e-9
        {
            net_widths.insert(net, width);
        }
    }
    Ok(BoardRules {
        min_trace_width,
        clearance,
        via_diameter,
        via_drill,
        net_widths,
    })
}

fn via_rule(stack: Option<&PadStack>) -> Result<(f64, f64), Error> {
    let stack = stack.ok_or_else(|| Error::NotFound("KiCAD default via stack".to_owned()))?;
    let diameter = stack
        .copper_layers
        .iter()
        .filter_map(|layer| layer.size.as_ref())
        .map(|size| nm_to_mm(size.x_nm.max(size.y_nm)))
        .find(|d| *d > 0.0)
        .ok_or_else(|| Error::NotFound("KiCAD default via diameter".to_owned()))?;
    let drill = stack
        .drill
        .as_ref()
        .and_then(|drill| drill.diameter.as_ref())
        .map(|diameter| nm_to_mm(diameter.x_nm.max(diameter.y_nm)))
        .filter(|d| *d > 0.0)
        .ok_or_else(|| Error::NotFound("KiCAD default via drill".to_owned()))?;
    Ok((diameter, drill))
}

fn distance_mm(distance: Option<&crate::proto::kiapi::common::types::Distance>) -> Option<f64> {
    distance.map(|d| nm_to_mm(d.value_nm)).filter(|v| *v > 0.0)
}

fn edge_cuts_outline(shapes: &[BoardGraphicShape]) -> Option<Vec<Point2>> {
    let mut outline_points = Vec::new();
    let mut segments = Vec::new();
    for shape in shapes
        .iter()
        .filter(|shape| layer_enum(shape.layer) == Some(BoardLayer::BlEdgeCuts))
    {
        let Some(graphic) = &shape.shape else {
            continue;
        };
        match &graphic.geometry {
            Some(Geometry::Segment(segment)) => {
                let (Some(start), Some(end)) = (&segment.start, &segment.end) else {
                    continue;
                };
                segments.push(Segment::new(point(start), point(end)));
            }
            Some(Geometry::Rectangle(rectangle)) => {
                let (Some(top_left), Some(bottom_right)) =
                    (&rectangle.top_left, &rectangle.bottom_right)
                else {
                    continue;
                };
                let a = point(top_left);
                let c = point(bottom_right);
                let b = Point2 { x: c.x, y: a.y };
                let d = Point2 { x: a.x, y: c.y };
                outline_points.extend([a, b, c, d]);
            }
            Some(Geometry::Arc(arc)) => {
                let (Some(start), Some(mid), Some(end)) = (&arc.start, &arc.mid, &arc.end) else {
                    continue;
                };
                let start = point(start);
                let mid = point(mid);
                let end = point(end);
                outline_points.extend([start, mid, end]);
                segments.push(Segment::new(start, end));
            }
            Some(Geometry::Circle(circle)) => {
                let (Some(center), Some(radius_point)) = (&circle.center, &circle.radius_point)
                else {
                    continue;
                };
                let center = point(center);
                let radius_point = point(radius_point);
                let radius = ((center.x - radius_point.x).powi(2)
                    + (center.y - radius_point.y).powi(2))
                .sqrt();
                outline_points.extend((0..32).map(|idx| {
                    let theta = (idx as f64) * std::f64::consts::TAU / 32.0;
                    Point2 {
                        x: center.x + radius * theta.cos(),
                        y: center.y + radius * theta.sin(),
                    }
                }));
            }
            Some(Geometry::Polygon(polyset)) => {
                outline_points.extend(polyset_points(Some(polyset)));
            }
            Some(Geometry::Bezier(bezier)) => {
                for point_ref in [
                    bezier.start.as_ref(),
                    bezier.control1.as_ref(),
                    bezier.control2.as_ref(),
                    bezier.end.as_ref(),
                ]
                .into_iter()
                .flatten()
                {
                    outline_points.push(point(point_ref));
                }
            }
            None => {}
        }
    }
    Polyline::from_unordered_segments(segments)
        .map(Polyline::into_points)
        .filter(|points| points.len() >= 3)
        .or_else(|| (outline_points.len() >= 3).then_some(outline_points))
}

fn infer_layer_names(
    footprints: &[FootprintInstance],
    tracks: &[Track],
    vias: &[Via],
    zones: &[Zone],
) -> Vec<String> {
    let mut layers = BTreeSet::new();
    for fp in footprints {
        if is_copper(fp.layer) {
            layers.insert(fp.layer);
        }
        if let Some(definition) = &fp.definition {
            for item in &definition.items {
                if let Ok(pad) = item.to_msg::<Pad>()
                    && let Some(stack) = &pad.pad_stack
                {
                    layers.extend(stack.layers.iter().copied().filter(|l| is_copper(*l)));
                    for copper in &stack.copper_layers {
                        if is_copper(copper.layer) {
                            layers.insert(copper.layer);
                        }
                    }
                }
            }
        }
    }
    layers.extend(tracks.iter().map(|t| t.layer).filter(|l| is_copper(*l)));
    for via in vias {
        if let Some(stack) = &via.pad_stack {
            layers.extend(stack.layers.iter().copied().filter(|l| is_copper(*l)));
            if let Some(drill) = &stack.drill {
                if is_copper(drill.start_layer) {
                    layers.insert(drill.start_layer);
                }
                if is_copper(drill.end_layer) {
                    layers.insert(drill.end_layer);
                }
            }
        }
    }
    for zone in zones {
        layers.extend(zone.layers.iter().copied().filter(|l| is_copper(*l)));
    }
    layers.insert(BoardLayer::BlFCu as i32);
    layers.insert(BoardLayer::BlBCu as i32);
    layers.into_iter().map(layer_name).collect()
}

fn pad_world(fp: &FootprintInstance, pad: &Pad) -> Point2 {
    pad.position
        .as_ref()
        .map(point)
        .or_else(|| fp.position.as_ref().map(point))
        .unwrap_or(Point2 { x: 0.0, y: 0.0 })
}

fn pad_size(pad: &Pad) -> (f64, f64) {
    let Some(stack) = &pad.pad_stack else {
        return (0.0, 0.0);
    };
    let layer = stack
        .copper_layers
        .iter()
        .find(|l| l.size.is_some() && layer_enum(l.layer) == Some(BoardLayer::BlFCu))
        .or_else(|| stack.copper_layers.iter().find(|l| l.size.is_some()));
    layer
        .and_then(|l| l.size.as_ref())
        .map(|size| (nm_to_mm(size.x_nm), nm_to_mm(size.y_nm)))
        .unwrap_or((0.0, 0.0))
}

fn pad_angle(fp: &FootprintInstance, pad: &Pad) -> f64 {
    fp.orientation
        .as_ref()
        .map(|a| a.value_degrees)
        .unwrap_or(0.0)
        + pad
            .pad_stack
            .as_ref()
            .and_then(|s| s.angle.as_ref())
            .map(|a| a.value_degrees)
            .unwrap_or(0.0)
}

fn pad_layers(pad: &Pad, layer_names: &[String]) -> Vec<LayerRef> {
    let Some(stack) = &pad.pad_stack else {
        return all_layer_refs(layer_names);
    };
    let mut layers: Vec<_> = stack
        .layers
        .iter()
        .copied()
        .filter(|l| is_copper(*l))
        .map(|l| layer_ref_for_i32(l, layer_names))
        .collect();
    if layers.is_empty() {
        layers = stack
            .copper_layers
            .iter()
            .filter(|l| is_copper(l.layer))
            .map(|l| layer_ref_for_i32(l.layer, layer_names))
            .collect();
    }
    if layers.is_empty() {
        all_layer_refs(layer_names)
    } else {
        layers.sort_by_key(|l| l.index(layer_names.len() as u32).unwrap_or(u32::MAX));
        layers.dedup();
        layers
    }
}

fn via_diameter(via: &Via) -> f64 {
    via.pad_stack
        .as_ref()
        .and_then(|s| {
            s.copper_layers
                .iter()
                .find(|l| {
                    matches!(
                        PadStackShape::try_from(l.shape),
                        Ok(PadStackShape::PssCircle
                            | PadStackShape::PssRectangle
                            | PadStackShape::PssOval
                            | PadStackShape::PssRoundrect)
                    )
                })
                .or_else(|| s.copper_layers.iter().find(|l| l.size.is_some()))
        })
        .and_then(|l| l.size.as_ref())
        .map(|s| nm_to_mm(s.x_nm.max(s.y_nm)))
        .unwrap_or(DEFAULT_VIA_DIAMETER_MM)
}

fn via_layers(via: &Via, layer_names: &[String]) -> Vec<LayerRef> {
    let Some(stack) = &via.pad_stack else {
        return all_layer_refs(layer_names);
    };
    let Some(drill) = &stack.drill else {
        return all_layer_refs(layer_names);
    };
    let Some(start) = copper_index(drill.start_layer, layer_names) else {
        return all_layer_refs(layer_names);
    };
    let Some(end) = copper_index(drill.end_layer, layer_names) else {
        return all_layer_refs(layer_names);
    };
    let (lo, hi) = (start.min(end), start.max(end));
    layer_names
        .iter()
        .enumerate()
        .filter(|(idx, _)| (*idx as u32) >= lo && (*idx as u32) <= hi)
        .map(|(_, name)| layer_ref_for(name, layer_names))
        .collect()
}

fn zone_layers(zone: &Zone, layer_names: &[String]) -> Vec<LayerRef> {
    let layers: Vec<_> = zone
        .layers
        .iter()
        .copied()
        .filter(|l| is_copper(*l))
        .map(|l| layer_ref_for_i32(l, layer_names))
        .collect();
    if layers.is_empty() {
        all_layer_refs(layer_names)
    } else {
        layers
    }
}

fn zone_is_routing_keepout(zone: &Zone) -> bool {
    // RouteProblem has one generic obstacle kind, so a via-only area is
    // intentionally conservative: it blocks tracks too rather than allowing a
    // route that may later require an illegal via inside the area.
    matches!(
        zone.settings,
        Some(zone::Settings::RuleAreaSettings(ref area))
            if area.keepout_tracks || area.keepout_vias
    )
}

fn zone_has_keepout_flags(zone: &Zone) -> bool {
    matches!(
        zone.settings,
        Some(zone::Settings::RuleAreaSettings(ref area))
            if area.keepout_copper
                || area.keepout_tracks
                || area.keepout_vias
                || area.keepout_pads
                || area.keepout_footprints
    )
}

fn zone_is_placement_keepout(zone: &Zone) -> bool {
    matches!(
        zone.settings,
        Some(zone::Settings::RuleAreaSettings(ref area))
            if area.keepout_footprints || area.keepout_pads
    )
}

fn zone_covers_board(points: &[Point2], outline: Option<&Polygon>) -> bool {
    let (Some(zone_bounds), Some(board)) = (Rect::bounding(points), outline) else {
        return false;
    };
    if !zone_bounds.contains_rect_eps(&board.bbox(), 1e-6) {
        return false;
    }
    let Ok(zone) = Polygon::new(points.to_vec()) else {
        return false;
    };
    // Matching bboxes alone are insufficient: a diamond or concave local pour
    // can touch every board extreme without carrying a solid board-wide plane.
    // Require the actual zone polygon to contain every board vertex and edge
    // midpoint. False negatives only disable the plane optimization; false
    // positives would invent connectivity, so this check intentionally errs safe.
    board
        .points()
        .iter()
        .copied()
        .all(|p| zone.contains_point(p))
        && board
            .edges()
            .all(|edge| zone.contains_point(edge.midpoint()))
}

fn net_codes(nets: &[Net]) -> BTreeMap<String, i32> {
    nets.iter()
        .filter_map(|net| {
            let code = net.code.as_ref()?.value;
            (code != 0 && !net.name.is_empty()).then(|| (net.name.clone(), code))
        })
        .collect()
}

fn net_name(net: &Net) -> Option<String> {
    (!net.name.is_empty()).then(|| net.name.clone())
}

fn layer_ref_for_i32(layer: i32, layer_names: &[String]) -> LayerRef {
    layer_enum(layer)
        .map(|layer| layer_name(layer as i32))
        .map(|name| layer_ref_for(&name, layer_names))
        .unwrap_or_else(LayerRef::top)
}

fn layer_ref_for(kicad_layer: &str, layer_names: &[String]) -> LayerRef {
    if kicad_layer == "F.Cu" {
        LayerRef::top()
    } else if kicad_layer == "B.Cu" {
        LayerRef::bottom()
    } else if let Some(idx) = layer_names.iter().position(|n| n == kicad_layer) {
        LayerRef(format!("inner{idx}"))
    } else {
        LayerRef(kicad_layer.to_owned())
    }
}

fn all_layer_refs(layer_names: &[String]) -> Vec<LayerRef> {
    layer_names
        .iter()
        .map(|l| layer_ref_for(l, layer_names))
        .collect()
}

fn copper_index(layer: i32, layer_names: &[String]) -> Option<u32> {
    let name = layer_name(layer);
    layer_names
        .iter()
        .position(|n| n == &name)
        .map(|i| i as u32)
}

fn is_copper(layer: i32) -> bool {
    matches!(
        layer_enum(layer),
        Some(BoardLayer::BlFCu | BoardLayer::BlBCu)
    ) || (BoardLayer::BlIn1Cu as i32..=BoardLayer::BlIn30Cu as i32).contains(&layer)
}

fn layer_enum(layer: i32) -> Option<BoardLayer> {
    BoardLayer::try_from(layer).ok()
}

fn layer_name(layer: i32) -> String {
    match layer_enum(layer) {
        Some(BoardLayer::BlFCu) => "F.Cu".to_owned(),
        Some(BoardLayer::BlBCu) => "B.Cu".to_owned(),
        Some(inner)
            if (BoardLayer::BlIn1Cu as i32..=BoardLayer::BlIn30Cu as i32)
                .contains(&(inner as i32)) =>
        {
            format!("In{}.Cu", inner as i32 - BoardLayer::BlFCu as i32)
        }
        _ => format!("layer:{layer}"),
    }
}

fn polyset_points(polyset: Option<&PolySet>) -> Vec<Point2> {
    polyset
        .into_iter()
        .flat_map(|set| &set.polygons)
        .filter_map(|polygon| polygon.outline.as_ref())
        .flat_map(|line| &line.nodes)
        .filter_map(|node| match &node.geometry {
            Some(poly_line_node::Geometry::Point(v)) => Some(point(v)),
            Some(poly_line_node::Geometry::Arc(arc)) => arc
                .start
                .as_ref()
                .or(arc.mid.as_ref())
                .or(arc.end.as_ref())
                .map(point),
            None => None,
        })
        .collect()
}

fn bbox_obstacle(
    kind: &str,
    layers: Vec<LayerRef>,
    connected_to: Vec<String>,
    points: &[Point2],
) -> Option<Obstacle> {
    let bounds = Rect::bounding(points)?;
    Some(Obstacle {
        kind: kind.to_owned(),
        layers,
        center: Point2 {
            x: (bounds.min_x + bounds.max_x) / 2.0,
            y: (bounds.min_y + bounds.max_y) / 2.0,
        },
        width: bounds.max_x - bounds.min_x,
        height: bounds.max_y - bounds.min_y,
        connected_to,
    })
}

fn bounds_from_obstacles(obstacles: &[Obstacle]) -> Option<Rect> {
    let mut points = Vec::with_capacity(obstacles.len() * 2);
    for ob in obstacles {
        points.push(Point2 {
            x: ob.center.x - ob.width / 2.0,
            y: ob.center.y - ob.height / 2.0,
        });
        points.push(Point2 {
            x: ob.center.x + ob.width / 2.0,
            y: ob.center.y + ob.height / 2.0,
        });
    }
    Rect::bounding(&points)
}

fn footprint_extents(obstacles: &[&Obstacle]) -> Option<(f64, f64)> {
    let bounds = Rect::bounding(
        &obstacles
            .iter()
            .flat_map(|ob| {
                [
                    Point2 {
                        x: ob.center.x - ob.width / 2.0,
                        y: ob.center.y - ob.height / 2.0,
                    },
                    Point2 {
                        x: ob.center.x + ob.width / 2.0,
                        y: ob.center.y + ob.height / 2.0,
                    },
                ]
            })
            .collect::<Vec<_>>(),
    )?;
    Some((bounds.max_x - bounds.min_x, bounds.max_y - bounds.min_y))
}

fn point(v: &Vector2) -> Point2 {
    Point2 {
        x: nm_to_mm(v.x_nm),
        y: nm_to_mm(v.y_nm),
    }
}

fn nm_to_mm(nm: i64) -> f64 {
    nm as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::kiapi::common::types::{
        Distance, GraphicSegmentAttributes, GraphicShape, PolyLine, PolyLineNode, PolygonWithHoles,
    };

    fn v(x_mm: f64, y_mm: f64) -> Vector2 {
        Vector2 {
            x_nm: (x_mm * 1_000_000.0) as i64,
            y_nm: (y_mm * 1_000_000.0) as i64,
        }
    }

    fn edge_segment(a: (f64, f64), b: (f64, f64)) -> BoardGraphicShape {
        BoardGraphicShape {
            shape: Some(GraphicShape {
                geometry: Some(Geometry::Segment(GraphicSegmentAttributes {
                    start: Some(v(a.0, a.1)),
                    end: Some(v(b.0, b.1)),
                })),
                ..Default::default()
            }),
            layer: BoardLayer::BlEdgeCuts as i32,
            ..Default::default()
        }
    }

    fn rectangular_zone(settings: zone::Settings) -> Zone {
        let nodes = [(2.0, 3.0), (8.0, 3.0), (8.0, 7.0), (2.0, 7.0)]
            .into_iter()
            .map(|(x, y)| PolyLineNode {
                geometry: Some(poly_line_node::Geometry::Point(v(x, y))),
            })
            .collect();
        Zone {
            layers: vec![BoardLayer::BlFCu as i32],
            outline: Some(PolySet {
                polygons: vec![PolygonWithHoles {
                    outline: Some(PolyLine {
                        nodes,
                        closed: true,
                    }),
                    holes: vec![],
                }],
            }),
            settings: Some(settings),
            ..Default::default()
        }
    }

    #[test]
    fn fallback_layers_keep_top_and_bottom() {
        let snapshot = snapshot_from_items(
            Vec::new(),
            vec![Track {
                start: Some(v(1.0, 1.0)),
                end: Some(v(2.0, 1.0)),
                width: Some(Distance { value_nm: 200_000 }),
                layer: BoardLayer::BlFCu as i32,
                ..Default::default()
            }],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(snapshot.layer_names, vec!["F.Cu", "B.Cu"]);
        assert_eq!(snapshot.problem.layer_count, 2);
    }

    #[test]
    fn edge_cuts_drive_snapshot_bounds() {
        let outline = edge_cuts_outline(&[
            edge_segment((0.0, 0.0), (100.0, 0.0)),
            edge_segment((100.0, 0.0), (100.0, 50.0)),
            edge_segment((100.0, 50.0), (0.0, 50.0)),
            edge_segment((0.0, 50.0), (0.0, 0.0)),
        ])
        .expect("outline");
        let outline = Polygon::new(outline).unwrap();
        let snapshot = snapshot_from_items_with_context(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            copper_layer_names(2),
            Some(outline),
            BoardRules {
                min_trace_width: DEFAULT_MIN_TRACE_WIDTH_MM,
                clearance: DEFAULT_CLEARANCE_MM,
                via_diameter: DEFAULT_VIA_DIAMETER_MM,
                via_drill: DEFAULT_VIA_DRILL_MM,
                net_widths: BTreeMap::new(),
            },
        );
        assert_eq!(snapshot.problem.bounds.min_x, 0.0);
        assert_eq!(snapshot.problem.bounds.min_y, 0.0);
        assert_eq!(snapshot.problem.bounds.max_x, 100.0);
        assert_eq!(snapshot.problem.bounds.max_y, 50.0);
        assert_eq!(snapshot.problem.outline.as_ref().unwrap().points().len(), 4);
    }

    #[test]
    fn only_routing_rule_areas_become_zone_obstacles() {
        use crate::proto::kiapi::board::types::{CopperZoneSettings, RuleAreaSettings};

        let copper = Zone {
            settings: Some(zone::Settings::CopperSettings(CopperZoneSettings::default())),
            ..Default::default()
        };
        assert!(!zone_is_routing_keepout(&copper));

        let placement_only = Zone {
            settings: Some(zone::Settings::RuleAreaSettings(RuleAreaSettings {
                keepout_footprints: true,
                ..Default::default()
            })),
            ..Default::default()
        };
        assert!(!zone_is_routing_keepout(&placement_only));
        assert!(zone_is_placement_keepout(&placement_only));

        let zone_fill_only = Zone {
            settings: Some(zone::Settings::RuleAreaSettings(RuleAreaSettings {
                keepout_copper: true,
                ..Default::default()
            })),
            ..Default::default()
        };
        assert!(!zone_is_routing_keepout(&zone_fill_only));
        assert!(!zone_is_placement_keepout(&zone_fill_only));

        let routing_keepout = Zone {
            settings: Some(zone::Settings::RuleAreaSettings(RuleAreaSettings {
                keepout_tracks: true,
                ..Default::default()
            })),
            ..Default::default()
        };
        assert!(zone_is_routing_keepout(&routing_keepout));
        assert!(!zone_is_placement_keepout(&routing_keepout));
    }

    #[test]
    fn rule_area_flags_survive_into_route_and_place_problems() {
        use crate::proto::kiapi::board::types::RuleAreaSettings;

        let combined = rectangular_zone(zone::Settings::RuleAreaSettings(RuleAreaSettings {
            keepout_tracks: true,
            keepout_footprints: true,
            ..Default::default()
        }));
        let adaptive_copper = rectangular_zone(zone::Settings::CopperSettings(Default::default()));
        let snapshot = snapshot_from_items_with_context(
            vec![],
            vec![],
            vec![],
            vec![combined, adaptive_copper],
            vec![],
            copper_layer_names(2),
            None,
            BoardRules {
                min_trace_width: DEFAULT_MIN_TRACE_WIDTH_MM,
                clearance: DEFAULT_CLEARANCE_MM,
                via_diameter: DEFAULT_VIA_DIAMETER_MM,
                via_drill: DEFAULT_VIA_DRILL_MM,
                net_widths: BTreeMap::new(),
            },
        );

        assert_eq!(snapshot.problem.obstacles.len(), 1);
        assert_eq!(snapshot.problem.obstacles[0].kind, "zone");
        assert_eq!(snapshot.imported.keepout_count, 1);
        assert_eq!(
            snapshot.place_problem().keepouts,
            vec![Rect::new(2.0, 3.0, 8.0, 7.0)]
        );
    }

    #[test]
    fn plane_assignment_requires_a_matching_inner_copper_zone() {
        let connections = vec![Connection {
            name: "GND".to_owned(),
            points_to_connect: vec![
                RoutePoint {
                    x: 1.0,
                    y: 1.0,
                    layer: LayerRef::top(),
                },
                RoutePoint {
                    x: 2.0,
                    y: 1.0,
                    layer: LayerRef::top(),
                },
            ],
        }];

        assert!(observed_plane_nets(4, &connections, &BTreeMap::new()).is_empty());

        let observed = BTreeMap::from([("GND".to_owned(), BTreeSet::from([1]))]);
        assert_eq!(
            observed_plane_nets(4, &connections, &observed),
            BTreeMap::from([("GND".to_owned(), 1)])
        );
    }

    #[test]
    fn only_board_spanning_inner_zones_can_be_planes() {
        let outline = Polygon::new(vec![
            Point2::new(0.0, 0.0),
            Point2::new(20.0, 0.0),
            Point2::new(20.0, 10.0),
            Point2::new(0.0, 10.0),
        ])
        .unwrap();
        let full = vec![
            Point2::new(0.0, 0.0),
            Point2::new(20.0, 0.0),
            Point2::new(20.0, 10.0),
            Point2::new(0.0, 10.0),
        ];
        let local = vec![
            Point2::new(5.0, 2.0),
            Point2::new(15.0, 2.0),
            Point2::new(15.0, 8.0),
            Point2::new(5.0, 8.0),
        ];
        let same_bbox_but_local = vec![
            Point2::new(0.0, 5.0),
            Point2::new(10.0, 0.0),
            Point2::new(20.0, 5.0),
            Point2::new(10.0, 10.0),
        ];

        assert!(zone_covers_board(&full, Some(&outline)));
        assert!(!zone_covers_board(&local, Some(&outline)));
        assert!(!zone_covers_board(&same_bbox_but_local, Some(&outline)));
        assert!(!zone_covers_board(&full, None));
    }
}
