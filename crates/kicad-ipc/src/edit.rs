use std::collections::{BTreeMap, BTreeSet};

use crate::{Error, Kicad, proto};

use pcb_model::{LayerRef, RouteProblem, RouteSolution, ViaSpan};
use proto::kiapi::board::types::{
    BoardLayer, DrillProperties, DrillShape, FootprintInstance, Net, PadStack, PadStackLayer,
    PadStackShape, PadStackType, Track, UnconnectedLayerRemoval, Via, ViaType,
};
use proto::kiapi::common::types::{Angle, Distance, KiCadObjectType, Vector2};

#[derive(Debug, Clone)]
pub struct FootprintMove {
    pub reference: String,
    pub x_nm: i64,
    pub y_nm: i64,
    pub rotation_deg: Option<f64>,
}

/// The reference designator of a footprint (e.g. "U1"), or "" if unset.
pub fn footprint_reference(fp: &FootprintInstance) -> String {
    fp.reference_field
        .as_ref()
        .and_then(|f| f.text.as_ref())
        .and_then(|t| t.text.as_ref())
        .map(|x| x.text.clone())
        .unwrap_or_default()
}

impl Kicad {
    /// Move a footprint (by reference designator) to `(x,y)` nm, with optional
    /// rotation in degrees. One commit. Errors if no footprint has that reference.
    pub fn move_footprint(
        &mut self,
        reference: &str,
        x_nm: i64,
        y_nm: i64,
        rotation_deg: Option<f64>,
    ) -> Result<(), Error> {
        self.ensure_footprint_update_supported()?;
        let fp = self
            .footprints()?
            .into_iter()
            .find(|f| footprint_reference(f) == reference)
            .ok_or_else(|| Error::NotFound(format!("footprint {reference}")))?;
        let update = moved_footprint(fp, x_nm, y_nm, rotation_deg);
        self.commit(&format!("move {reference}"), |k| {
            k.update_items(vec![prost_types::Any::from_msg(&update)?])
        })
    }

    /// Move a set of footprints in one KiCAD undoable commit.
    pub fn move_footprints(&mut self, moves: &[FootprintMove]) -> Result<(), Error> {
        if moves.is_empty() {
            return Ok(());
        }
        self.ensure_footprint_update_supported()?;
        let by_ref: BTreeMap<&str, &FootprintMove> =
            moves.iter().map(|m| (m.reference.as_str(), m)).collect();
        let mut updates = Vec::new();
        let mut found_refs = BTreeSet::new();
        for fp in self.footprints()? {
            let reference = footprint_reference(&fp);
            let Some(mv) = by_ref.get(reference.as_str()) else {
                continue;
            };
            found_refs.insert(reference);
            updates.push(prost_types::Any::from_msg(&moved_footprint(
                fp,
                mv.x_nm,
                mv.y_nm,
                mv.rotation_deg,
            ))?);
        }
        if found_refs.len() != moves.len() {
            let missing: Vec<&str> = moves
                .iter()
                .map(|m| m.reference.as_str())
                .filter(|r| !found_refs.contains(*r))
                .collect();
            return Err(Error::NotFound(format!(
                "footprint(s) {}",
                missing.join(", ")
            )));
        }
        self.commit("place board", |k| k.update_items(updates))
    }

    /// Route a straight track segment on `layer` with `width_nm`, optionally on a
    /// named net (matched by name). One commit.
    pub fn add_track(
        &mut self,
        start_nm: (i64, i64),
        end_nm: (i64, i64),
        width_nm: i64,
        layer: BoardLayer,
        net_name: Option<&str>,
    ) -> Result<(), Error> {
        let net = match net_name {
            Some(name) => self.net_list()?.into_iter().find(|n| n.name == name),
            None => None,
        };
        let track = Track {
            start: Some(Vector2 {
                x_nm: start_nm.0,
                y_nm: start_nm.1,
            }),
            end: Some(Vector2 {
                x_nm: end_nm.0,
                y_nm: end_nm.1,
            }),
            width: Some(Distance { value_nm: width_nm }),
            layer: layer as i32,
            net,
            ..Default::default()
        };
        self.commit("add track", |k| {
            k.create_items(vec![prost_types::Any::from_msg(&track)?])
        })
    }

    /// Delete every track segment and via on the board, then save it.
    pub fn delete_tracks_and_vias(&mut self) -> Result<(usize, usize), Error> {
        let mut items = self.get_items(&[KiCadObjectType::KotPcbTrace])?;
        let tracks = items.len();
        let vias = self.get_items(&[KiCadObjectType::KotPcbVia])?;
        let via_count = vias.len();
        items.extend(vias);
        self.commit("clear copper", |k| k.delete_packed_items(&items))?;
        self.save()?;
        Ok((tracks, via_count))
    }

    /// Create every track/via in a routed solution as native KiCAD board items.
    pub fn create_route_solution(
        &mut self,
        problem: &RouteProblem,
        solution: &RouteSolution,
        layer_names: &[String],
    ) -> Result<(), Error> {
        let nets: BTreeMap<String, Net> = self
            .net_list()?
            .into_iter()
            .map(|net| (net.name.clone(), net))
            .collect();
        let items = route_solution_items(problem, solution, layer_names, &nets)?;
        self.commit("route board", |k| k.create_items(items))?;
        self.save()
    }
}

fn moved_footprint(
    mut source: FootprintInstance,
    x_nm: i64,
    y_nm: i64,
    rotation_deg: Option<f64>,
) -> FootprintInstance {
    source.position = Some(Vector2 { x_nm, y_nm });
    if let Some(deg) = rotation_deg {
        source.orientation = Some(Angle { value_degrees: deg });
    }
    source
}

fn route_solution_items(
    problem: &RouteProblem,
    solution: &RouteSolution,
    layer_names: &[String],
    nets: &BTreeMap<String, Net>,
) -> Result<Vec<prost_types::Any>, Error> {
    let mut items = Vec::new();
    for trace in &solution.traces {
        let layer = board_layer_for_route_layer(&trace.layer, problem.layer_count, layer_names);
        let net = net_for_route(nets, &trace.connection)?;
        for segment in trace.path.windows(2) {
            items.push(prost_types::Any::from_msg(&Track {
                start: Some(point_nm(&segment[0])),
                end: Some(point_nm(&segment[1])),
                width: Some(distance_mm(trace.width)),
                layer,
                net: Some(net.clone()),
                ..Default::default()
            })?);
        }
    }
    for via in &solution.vias {
        let (from, to) = via_span_layers(&via.span, layer_names);
        items.push(prost_types::Any::from_msg(&Via {
            position: Some(point_nm(&via.at)),
            pad_stack: Some(via_pad_stack(via.diameter, via.drill, from, to)),
            net: Some(net_for_route(nets, &via.connection)?.clone()),
            r#type: via_type(&via.span),
            ..Default::default()
        })?);
    }
    Ok(items)
}

fn net_for_route<'a>(nets: &'a BTreeMap<String, Net>, name: &str) -> Result<&'a Net, Error> {
    nets.get(name)
        .ok_or_else(|| Error::NotFound(format!("net {name}")))
}

fn point_nm(point: &pcb_model::Point2) -> Vector2 {
    Vector2 {
        x_nm: mm_to_nm(point.x),
        y_nm: mm_to_nm(point.y),
    }
}

fn distance_mm(value: f64) -> Distance {
    Distance {
        value_nm: mm_to_nm(value),
    }
}

fn mm_to_nm(value: f64) -> i64 {
    (value * 1_000_000.0).round() as i64
}

fn board_layer_for_route_layer(layer: &LayerRef, layer_count: u32, layer_names: &[String]) -> i32 {
    let idx = layer.index(layer_count).unwrap_or(0) as usize;
    layer_names
        .get(idx)
        .map(|name| board_layer_from_name(name))
        .unwrap_or_else(|| board_layer_from_index(idx, layer_count as usize))
}

fn via_span_layers(span: &ViaSpan, layer_names: &[String]) -> (i32, i32) {
    match *span {
        ViaSpan::Through => {
            let top = layer_names
                .first()
                .map(|name| board_layer_from_name(name))
                .unwrap_or(BoardLayer::BlFCu as i32);
            let bottom = layer_names
                .last()
                .map(|name| board_layer_from_name(name))
                .unwrap_or(BoardLayer::BlBCu as i32);
            (top, bottom)
        }
        ViaSpan::Partial { from, to, .. } => (
            layer_names
                .get(from as usize)
                .map(|name| board_layer_from_name(name))
                .unwrap_or_else(|| board_layer_from_index(from as usize, layer_names.len())),
            layer_names
                .get(to as usize)
                .map(|name| board_layer_from_name(name))
                .unwrap_or_else(|| board_layer_from_index(to as usize, layer_names.len())),
        ),
    }
}

fn board_layer_from_name(name: &str) -> i32 {
    match name {
        "F.Cu" => BoardLayer::BlFCu as i32,
        "B.Cu" => BoardLayer::BlBCu as i32,
        name if name.starts_with("In") && name.ends_with(".Cu") => name[2..name.len() - 3]
            .parse::<i32>()
            .ok()
            .filter(|n| (1..=30).contains(n))
            .map(|n| BoardLayer::BlIn1Cu as i32 + n - 1)
            .unwrap_or(BoardLayer::BlFCu as i32),
        _ => BoardLayer::BlFCu as i32,
    }
}

fn board_layer_from_index(idx: usize, layer_count: usize) -> i32 {
    if idx == 0 {
        BoardLayer::BlFCu as i32
    } else if idx + 1 >= layer_count.max(2) {
        BoardLayer::BlBCu as i32
    } else {
        BoardLayer::BlIn1Cu as i32 + idx as i32 - 1
    }
}

fn via_type(span: &ViaSpan) -> i32 {
    match *span {
        ViaSpan::Through => ViaType::VtThrough as i32,
        ViaSpan::Partial { micro: true, .. } => ViaType::VtMicro as i32,
        ViaSpan::Partial { micro: false, .. } => ViaType::VtBlindBuried as i32,
    }
}

fn via_pad_stack(diameter_mm: f64, drill_mm: f64, from: i32, to: i32) -> PadStack {
    PadStack {
        r#type: PadStackType::PstNormal as i32,
        layers: vec![from, to],
        drill: Some(DrillProperties {
            start_layer: from,
            end_layer: to,
            diameter: Some(Vector2 {
                x_nm: mm_to_nm(drill_mm),
                y_nm: mm_to_nm(drill_mm),
            }),
            shape: DrillShape::DsCircle as i32,
        }),
        unconnected_layer_removal: UnconnectedLayerRemoval::UlrKeep as i32,
        copper_layers: vec![PadStackLayer {
            layer: from,
            shape: PadStackShape::PssCircle as i32,
            size: Some(Vector2 {
                x_nm: mm_to_nm(diameter_mm),
                y_nm: mm_to_nm(diameter_mm),
            }),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[cfg(test)]
mod route_write_tests {
    use super::*;
    use pcb_model::{LayerRef, Point2, RouteProblem, RouteSolution, Trace, ViaSpan};
    use proto::kiapi::board::types::{Net, NetCode, Via, ViaType};
    use std::collections::BTreeMap;

    fn route_problem() -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 20.0,
                max_y: 20.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        }
    }

    fn nets() -> BTreeMap<String, Net> {
        [(
            "GND".to_owned(),
            Net {
                code: Some(NetCode { value: 7 }),
                name: "GND".to_owned(),
            },
        )]
        .into()
    }

    #[test]
    fn route_solution_becomes_typed_ipc_tracks_and_vias() {
        let problem = route_problem();
        let solution = RouteSolution {
            traces: vec![Trace {
                connection: "GND".to_owned(),
                layer: LayerRef::top(),
                width: 0.25,
                path: vec![Point2 { x: 1.0, y: 2.0 }, Point2 { x: 3.0, y: 4.0 }],
            }],
            vias: vec![pcb_model::Via {
                connection: "GND".to_owned(),
                at: Point2 { x: 3.0, y: 4.0 },
                diameter: 0.6,
                drill: 0.3,
                span: ViaSpan::Through,
            }],
        };

        let items = route_solution_items(
            &problem,
            &solution,
            &["F.Cu".to_owned(), "B.Cu".to_owned()],
            &nets(),
        )
        .unwrap();

        assert_eq!(items.len(), 2);

        let track = items[0].to_msg::<Track>().unwrap();
        assert_eq!(track.start.unwrap().x_nm, 1_000_000);
        assert_eq!(track.end.unwrap().y_nm, 4_000_000);
        assert_eq!(track.width.unwrap().value_nm, 250_000);
        assert_eq!(track.layer, BoardLayer::BlFCu as i32);
        assert_eq!(track.net.unwrap().code.unwrap().value, 7);

        let via = items[1].to_msg::<Via>().unwrap();
        assert_eq!(via.position.unwrap().x_nm, 3_000_000);
        assert_eq!(via.r#type, ViaType::VtThrough as i32);
        assert_eq!(via.net.unwrap().name, "GND");
        let stack = via.pad_stack.unwrap();
        assert_eq!(stack.drill.unwrap().diameter.unwrap().x_nm, 300_000);
        assert_eq!(stack.copper_layers[0].size.as_ref().unwrap().x_nm, 600_000);
    }
}
