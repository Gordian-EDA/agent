//! Framework-level PCB problem, solution, and engine contract.
//!
//! Placement and routing views are implementation details.  The framework only
//! knows that an engine turns the complete physical-design problem into a
//! complete physical-design solution.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    Connection, FailedNet, LayerRef, Obstacle, Point2, Polygon, Rect, RoutePoint, RouteSolution,
};

/// A footprint-local PCB-edge datum.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EdgeDatum {
    pub start: Point2,
    pub end: Point2,
}

impl EdgeDatum {
    pub fn rotated(self, rotation: f64) -> Self {
        Self {
            start: self.start.rotate(rotation),
            end: self.end.rotate(rotation),
        }
    }

    pub fn midpoint(self) -> Point2 {
        Point2::new(
            (self.start.x + self.end.x) / 2.0,
            (self.start.y + self.end.y) / 2.0,
        )
    }

    pub fn is_horizontal(self) -> bool {
        (self.end.x - self.start.x).abs() >= (self.end.y - self.start.y).abs()
    }
}

/// One footprint pad in component-local coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PartPad {
    pub number: String,
    pub offset: Point2,
    pub width: f64,
    pub height: f64,
    pub layers: Vec<LayerRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<String>,
}

/// A fixed component placement supplied as part of the problem.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LockedAt {
    pub at: Point2,
    #[serde(default)]
    pub rotation: f64,
}

/// A physical component to place and connect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Part {
    pub reference: String,
    pub courtyard_w: f64,
    pub courtyard_h: f64,
    pub pads: Vec<PartPad>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_datum: Option<EdgeDatum>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<LockedAt>,
}

/// One component's position in a PCB solution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Placement {
    pub reference: String,
    pub at: Point2,
    pub rotation: f64,
}

/// The complete immutable physical-design problem presented to a PCB engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PcbProblem {
    pub bounds: Rect,
    pub layer_count: u32,
    pub clearance: f64,
    /// Minimum copper-to-board-edge distance.
    pub edge_clearance: f64,
    pub min_trace_width: f64,
    pub via_diameter: f64,
    pub via_drill: f64,
    pub parts: Vec<Part>,
    /// Fixed non-component obstacles and keep-outs.
    #[serde(default)]
    pub obstacles: Vec<Obstacle>,
    /// Fixed terminals not represented by component pads.
    #[serde(default)]
    pub connections: Vec<Connection>,
    #[serde(default)]
    pub net_widths: BTreeMap<String, f64>,
    #[serde(default)]
    pub outline: Option<Polygon>,
    #[serde(default)]
    pub plane_nets: BTreeMap<String, u32>,
    #[serde(default)]
    pub escape_layers: BTreeMap<String, u32>,
    /// Copper which an engine must preserve.
    #[serde(default)]
    pub fixed_copper: RouteSolution,
}

impl PcbProblem {
    /// Derive world-space routing geometry from a component placement.
    pub fn routing_geometry(&self, placements: &[Placement]) -> (Vec<Obstacle>, Vec<Connection>) {
        let place_by_ref: BTreeMap<&str, &Placement> = placements
            .iter()
            .map(|placement| (placement.reference.as_str(), placement))
            .collect();
        let mut obstacles = self.obstacles.clone();
        let mut net_points: BTreeMap<String, Vec<RoutePoint>> = BTreeMap::new();

        for part in &self.parts {
            let Some(placement) = place_by_ref.get(part.reference.as_str()) else {
                continue;
            };
            let rotation = geom::snap_quadrant(placement.rotation) as i32;
            for pad in &part.pads {
                let offset = pad.offset.rotate(rotation as f64);
                let center = Point2::new(placement.at.x + offset.x, placement.at.y + offset.y);
                let (width, height) = match rotation {
                    90 | 270 => (pad.height, pad.width),
                    _ => (pad.width, pad.height),
                };
                obstacles.push(Obstacle {
                    kind: "rect".to_owned(),
                    layers: pad.layers.clone(),
                    center,
                    width,
                    height,
                    connected_to: pad.net.clone().into_iter().collect(),
                });
                if let Some(net) = &pad.net {
                    net_points.entry(net.clone()).or_default().push(RoutePoint {
                        x: center.x,
                        y: center.y,
                        layer: pad.layers.first().cloned().unwrap_or_else(LayerRef::top),
                    });
                }
            }
        }

        let mut by_name: BTreeMap<String, Vec<RoutePoint>> = self
            .connections
            .iter()
            .map(|connection| {
                (
                    connection.name.clone(),
                    connection.points_to_connect.clone(),
                )
            })
            .collect();
        for (name, points) in net_points {
            by_name.entry(name).or_default().extend(points);
        }
        let connections = by_name
            .into_iter()
            .filter(|(_, points)| points.len() >= 2)
            .map(|(name, points_to_connect)| Connection {
                name,
                points_to_connect,
            })
            .collect();
        (obstacles, connections)
    }
}

/// The complete physical-design solution returned by a PCB engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PcbSolution {
    pub placements: Vec<Placement>,
    pub copper: RouteSolution,
    pub failed: Vec<FailedNet>,
    pub placement_legal: bool,
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

/// The only framework-level PCB algorithm contract.
pub trait PcbEngine {
    fn solve(&self, problem: &PcbProblem) -> PcbSolution;
}
