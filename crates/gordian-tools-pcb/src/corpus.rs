//! Offline PCB corpus loading for placement/router validation.
//!
//! The JSON files under `examples/pcb_circuits/` are not live tool calls: they
//! predate some public tool field names and include direct footprint/pad-net
//! seed data. This module owns that corpus shape and converts it through the
//! same footprint geometry path used by live `place_board`.

use std::collections::BTreeMap;
use std::path::Path;

use kicad_footprint::{FootprintCatalog, FootprintId};
use kicad_ipc::FootprintMove;
use pcb_model::{
    LayerRef, Obstacle, PcbProblem, Point2, Polygon, Rect, RouteSolution, RoutingView,
};
use pcb_place::{Edge, GroupHint, Placement, PlacementHints};
use pcb_place::{LockedAt, PlacementView};
use serde::Deserialize;

use super::place::{is_connector, is_mounting_hole, part_from_footprint_layers, routing_bounds};

#[derive(Debug, Clone)]
pub struct CorpusBoard {
    pub description: Option<String>,
    pub problem: PlacementView,
    pub hints: PlacementHints,
    pub rules: CorpusRules,
    pub keepouts: Vec<CorpusKeepout>,
    board_bounds: Rect,
    seed_parts: Vec<CorpusSeedPart>,
}

#[derive(Debug, Clone)]
struct CorpusSeedPart {
    reference: String,
    footprint: String,
    pad_nets: BTreeMap<String, String>,
    locked: Option<LockedAt>,
}

#[derive(Debug, Clone)]
pub struct CorpusDrcResult {
    pub copper_violations: usize,
    pub unconnected_items: usize,
    pub ignored_zone_self_unconnected: usize,
    pub issues: Vec<String>,
}

impl CorpusDrcResult {
    pub fn is_ok(&self) -> bool {
        self.copper_violations == 0 && self.unconnected_items == 0
    }
}

#[derive(Debug, Clone)]
pub struct CorpusRules {
    pub clearance: f64,
    pub min_trace_width: f64,
    pub via_diameter: f64,
    pub via_drill: f64,
    pub layer_count: u32,
    pub net_widths: BTreeMap<String, f64>,
}

impl Default for CorpusRules {
    fn default() -> Self {
        Self {
            clearance: 0.15,
            min_trace_width: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            layer_count: 2,
            net_widths: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CorpusKeepout {
    pub rect: Rect,
    pub layers: Vec<LayerRef>,
}

pub fn pcb_problem(board: &CorpusBoard) -> PcbProblem {
    let obstacles = board
        .keepouts
        .iter()
        .map(|keepout| Obstacle {
            kind: "rect".to_owned(),
            layers: keepout.layers.clone(),
            center: Point2::new(
                (keepout.rect.min_x + keepout.rect.max_x) / 2.0,
                (keepout.rect.min_y + keepout.rect.max_y) / 2.0,
            ),
            width: keepout.rect.max_x - keepout.rect.min_x,
            height: keepout.rect.max_y - keepout.rect.min_y,
            connected_to: Vec::new(),
        })
        .collect();
    let net_counts = pcb_place::derive_nets(&board.problem)
        .into_iter()
        .map(|net| (net.name, net.pins.len()));
    PcbProblem {
        bounds: board.problem.bounds,
        layer_count: board.rules.layer_count,
        clearance: board.rules.clearance,
        edge_clearance: 0.5,
        min_trace_width: board.rules.min_trace_width,
        via_diameter: board.rules.via_diameter,
        via_drill: board.rules.via_drill,
        parts: board.problem.parts.clone(),
        obstacles,
        connections: Vec::new(),
        net_widths: board.rules.net_widths.clone(),
        outline: board.problem.outline.clone(),
        plane_nets: pcb_model::default_plane_nets(board.rules.layer_count, net_counts),
        escape_layers: Default::default(),
        fixed_copper: RouteSolution::default(),
    }
}

pub fn load_corpus_board(
    path: &Path,
    catalog: &FootprintCatalog,
) -> std::result::Result<CorpusBoard, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let raw: RawBoard = serde_json::from_str(&text)
        .map_err(|e| format!("could not parse {}: {e}", path.display()))?;
    raw.into_board(catalog)
        .map_err(|e| format!("{}: {e}", path.display()))
}

pub fn route_problem_for_placement(board: &CorpusBoard, placements: &[Placement]) -> RoutingView {
    let mut rp = pcb_place::routing_view(&board.problem, placements);
    rp.bounds = routing_bounds(&rp.bounds, rp.outline.as_ref());
    rp.plane_nets = pcb_model::default_plane_nets(
        rp.layer_count,
        rp.connections
            .iter()
            .map(|c| (c.name.clone(), c.points_to_connect.len())),
    );
    rp.clearance = board.rules.clearance;
    rp.min_trace_width = board.rules.min_trace_width;
    rp.via_diameter = board.rules.via_diameter;
    rp.via_drill = board.rules.via_drill;
    rp.net_widths = board.rules.net_widths.clone();
    for keepout in &board.keepouts {
        rp.obstacles.push(Obstacle {
            kind: "rect".to_owned(),
            layers: keepout.layers.clone(),
            center: Point2 {
                x: (keepout.rect.min_x + keepout.rect.max_x) / 2.0,
                y: (keepout.rect.min_y + keepout.rect.max_y) / 2.0,
            },
            width: keepout.rect.max_x - keepout.rect.min_x,
            height: keepout.rect.max_y - keepout.rect.min_y,
            connected_to: Vec::new(),
        });
    }
    rp
}

/// Build a routed KiCad board for external DRC using the production seed
/// writer and offline patchers.
pub fn routed_board_text(
    board: &CorpusBoard,
    placements: &[Placement],
    solution: &RouteSolution,
    catalog: &FootprintCatalog,
) -> std::result::Result<String, String> {
    let spec = super::create::BoardSeedSpec {
        bounds: board.board_bounds,
        rules: super::create::SeedRules {
            clearance: board.rules.clearance,
            min_trace_width: board.rules.min_trace_width,
            via_diameter: board.rules.via_diameter,
            via_drill: board.rules.via_drill,
            layer_count: board.rules.layer_count,
            net_widths: board.rules.net_widths.clone(),
            pours: Vec::new(),
        },
        parts: board
            .seed_parts
            .iter()
            .map(|part| super::create::SeedPart {
                reference: part.reference.clone(),
                value: None,
                footprint: part.footprint.clone(),
                pad_nets: part.pad_nets.clone(),
                locked: part.locked.clone(),
            })
            .collect(),
        outline: board.problem.outline.clone(),
    };
    let seed = super::create::emit_seed_board(&spec, catalog)?;
    let moves = placements
        .iter()
        .map(|placement| FootprintMove {
            reference: placement.reference.clone(),
            x_nm: kicad_ipc::units::mm_to_nm(placement.at.x),
            y_nm: kicad_ipc::units::mm_to_nm(placement.at.y),
            rotation_deg: Some(placement.rotation),
        })
        .collect::<Vec<_>>();
    let placed = super::patch::patch_placements(&seed, &moves)?;
    let layer_names = copper_layer_names(board.rules.layer_count);
    super::patch::append_copper(&placed, solution, board.rules.layer_count, &layer_names)
}

/// Synthesize a temporary routed board and check it with KiCad's DRC using the
/// same acceptance policy as the live `check_board` tool.
pub fn run_kicad_drc(
    board: &CorpusBoard,
    placements: &[Placement],
    solution: &RouteSolution,
    catalog: &FootprintCatalog,
    env: &kicad::KicadInstallation,
) -> std::result::Result<CorpusDrcResult, String> {
    let text = routed_board_text(board, placements, solution, catalog)?;
    let dir = tempfile::Builder::new()
        .prefix("gordian-corpus-drc-")
        .tempdir()
        .map_err(|e| format!("could not create temporary DRC directory: {e}"))?;
    let path = dir.path().join("corpus.kicad_pcb");
    std::fs::write(&path, text)
        .map_err(|e| format!("could not write temporary routed board: {e}"))?;
    let sessions = kicad_ipc::SessionManager::with_installation(
        env.pcbnew_path().to_path_buf(),
        env.major_version(),
        false,
        false,
    );
    let materialized = super::export::materialize_zones_for_drc(&path, env, &sessions);
    sessions.close();
    materialized?;
    let report = env
        .drc(&path)
        .map_err(|e| format!("kicad-cli pcb drc failed: {e}"))?;
    let gate = super::export::gate_drc(&report);
    let issues = report
        .violations
        .iter()
        .filter(|violation| !super::export::is_non_copper(violation))
        .chain(
            report
                .unconnected_items
                .iter()
                .filter(|violation| !super::export::is_zone_self_unconnected(violation)),
        )
        .take(12)
        .map(|violation| {
            let items = violation
                .items
                .iter()
                .map(|item| item.description.as_str())
                .collect::<Vec<_>>()
                .join(" | ");
            format!("{}: {} [{items}]", violation.kind, violation.description)
        })
        .collect();
    Ok(CorpusDrcResult {
        copper_violations: gate.copper_violations,
        unconnected_items: gate.meaningful_unconnected,
        ignored_zone_self_unconnected: gate.ignored_zone_self_unconnected,
        issues,
    })
}

fn copper_layer_names(layer_count: u32) -> Vec<String> {
    let count = layer_count.max(2);
    let mut names = Vec::with_capacity(count as usize);
    names.push("F.Cu".to_owned());
    for idx in 1..count.saturating_sub(1) {
        names.push(format!("In{idx}.Cu"));
    }
    names.push("B.Cu".to_owned());
    names
}

#[derive(Debug, Deserialize)]
struct RawBoard {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    rules: RawRules,
    bounds: Option<RawRect>,
    #[serde(default)]
    outline: Option<Vec<[f64; 2]>>,
    #[serde(default)]
    keepouts: Vec<RawKeepout>,
    parts: Vec<RawPart>,
    #[serde(default)]
    hints: RawHints,
}

impl RawBoard {
    fn into_board(self, catalog: &FootprintCatalog) -> std::result::Result<CorpusBoard, String> {
        let rules = self.rules.into_rules()?;
        let outline = self
            .outline
            .map(|points| Polygon::new(points.into_iter().map(|[x, y]| Point2 { x, y }).collect()))
            .transpose()?;
        let bounds = self
            .bounds
            .map(RawRect::into_rect)
            .or_else(|| outline.as_ref().and_then(|p| Rect::bounding(p.points())))
            .ok_or_else(|| "missing `bounds` or `outline`".to_owned())?;
        let keepouts = self
            .keepouts
            .into_iter()
            .map(|k| k.into_keepout(rules.layer_count))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let place_keepouts = keepouts.iter().map(|k| k.rect).collect();

        let mut part_specs = Vec::with_capacity(self.parts.len());
        let mut seed_parts = Vec::with_capacity(self.parts.len());
        let mut parts = Vec::with_capacity(self.parts.len());
        for raw_part in self.parts {
            let id = FootprintId::parse(&raw_part.footprint).map_err(|e| {
                format!(
                    "part {}: invalid footprint id `{}`: {e}",
                    raw_part.reference, raw_part.footprint
                )
            })?;
            let footprint = catalog.footprint(&id).map_err(|e| {
                format!(
                    "part {}: footprint `{}` is not resolvable: {e}",
                    raw_part.reference, raw_part.footprint
                )
            })?;
            let locked = raw_part.locked.map(RawLocked::into_locked);
            parts.push(part_from_footprint_layers(
                &footprint,
                &raw_part.reference,
                &raw_part.pad_nets,
                rules.layer_count,
                locked.clone(),
            ));
            seed_parts.push(CorpusSeedPart {
                reference: raw_part.reference.clone(),
                footprint: raw_part.footprint.clone(),
                pad_nets: raw_part.pad_nets.clone(),
                locked,
            });
            part_specs.push((raw_part.reference, raw_part.footprint));
        }

        let mut hints = self.hints.into_hints()?;
        add_auto_edge_hints(&mut hints, &part_specs);

        let problem = PlacementView {
            bounds,
            clearance: rules.clearance,
            layer_count: rules.layer_count,
            min_trace_width: rules.min_trace_width,
            parts,
            keepouts: place_keepouts,
            outline,
        };
        Ok(CorpusBoard {
            description: self.description,
            problem,
            hints,
            rules,
            keepouts,
            board_bounds: bounds,
            seed_parts,
        })
    }
}

fn add_auto_edge_hints(hints: &mut PlacementHints, parts: &[(String, String)]) {
    let explicitly_edged: std::collections::BTreeSet<&str> = hints
        .groups
        .iter()
        .filter(|g| g.edge.is_some())
        .flat_map(|g| g.members.iter().map(String::as_str))
        .collect();
    for (reference, footprint) in parts {
        if explicitly_edged.contains(reference.as_str()) {
            continue;
        }
        if is_mounting_hole(footprint) {
            push_unique(&mut hints.corner_seek, reference);
            push_unique(&mut hints.edge_seek, reference);
        } else if is_connector(footprint, reference) {
            push_unique(&mut hints.edge_seek, reference);
        }
    }
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|v| v == value) {
        values.push(value.to_owned());
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawRules {
    #[serde(default)]
    clearance: Option<f64>,
    #[serde(default)]
    min_trace_width: Option<f64>,
    #[serde(default)]
    via_diameter: Option<f64>,
    #[serde(default)]
    via_drill: Option<f64>,
    #[serde(default)]
    layer_count: Option<u32>,
    #[serde(default)]
    layers: Option<u32>,
    #[serde(default)]
    net_widths: BTreeMap<String, f64>,
}

impl RawRules {
    fn into_rules(self) -> std::result::Result<CorpusRules, String> {
        let default = CorpusRules::default();
        let layer_count = self
            .layer_count
            .or(self.layers)
            .unwrap_or(default.layer_count);
        if !matches!(layer_count, 2 | 4 | 6 | 8) {
            return Err(format!(
                "rules.layers/layer_count must be 2, 4, 6, or 8, got {layer_count}"
            ));
        }
        Ok(CorpusRules {
            clearance: self.clearance.unwrap_or(default.clearance),
            min_trace_width: self.min_trace_width.unwrap_or(default.min_trace_width),
            via_diameter: self.via_diameter.unwrap_or(default.via_diameter),
            via_drill: self.via_drill.unwrap_or(default.via_drill),
            layer_count,
            net_widths: self.net_widths,
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawPart {
    reference: String,
    footprint: String,
    #[serde(default)]
    pad_nets: BTreeMap<String, String>,
    #[serde(default)]
    locked: Option<RawLocked>,
}

#[derive(Debug, Deserialize)]
struct RawLocked {
    x: f64,
    y: f64,
    #[serde(default)]
    rotation: f64,
}

impl RawLocked {
    fn into_locked(self) -> LockedAt {
        LockedAt {
            at: Point2 {
                x: self.x,
                y: self.y,
            },
            rotation: self.rotation,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct RawRect {
    min_x: f64,
    max_x: f64,
    min_y: f64,
    max_y: f64,
}

impl RawRect {
    fn into_rect(self) -> Rect {
        Rect {
            min_x: self.min_x,
            max_x: self.max_x,
            min_y: self.min_y,
            max_y: self.max_y,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawKeepout {
    rect: RawRect,
    #[serde(default)]
    layers: Vec<String>,
}

impl RawKeepout {
    fn into_keepout(self, layer_count: u32) -> std::result::Result<CorpusKeepout, String> {
        let layers = if self.layers.is_empty() {
            vec![LayerRef::top(), LayerRef::bottom()]
        } else {
            self.layers
                .iter()
                .map(|layer| parse_layer(layer, layer_count))
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        Ok(CorpusKeepout {
            rect: self.rect.into_rect(),
            layers,
        })
    }
}

fn parse_layer(layer: &str, layer_count: u32) -> std::result::Result<LayerRef, String> {
    LayerRef::resolve(layer, layer_count)
        .map(|(idx, _)| {
            if idx == 0 {
                LayerRef::top()
            } else if idx + 1 == layer_count {
                LayerRef::bottom()
            } else {
                LayerRef(format!("inner{idx}"))
            }
        })
        .ok_or_else(|| format!("invalid layer `{layer}` for {layer_count}-layer board"))
}

#[derive(Debug, Default, Deserialize)]
struct RawHints {
    #[serde(default)]
    groups: Vec<RawGroupHint>,
    #[serde(default)]
    edge_seek: Vec<String>,
    #[serde(default)]
    edge_seek_refs: Vec<String>,
    #[serde(default)]
    corner_seek: Vec<String>,
}

impl RawHints {
    fn into_hints(self) -> std::result::Result<PlacementHints, String> {
        let mut edge_seek = self.edge_seek;
        edge_seek.extend(self.edge_seek_refs);
        Ok(PlacementHints {
            groups: self
                .groups
                .into_iter()
                .map(RawGroupHint::into_group)
                .collect::<std::result::Result<Vec<_>, _>>()?,
            edge_seek,
            corner_seek: self.corner_seek,
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawGroupHint {
    name: String,
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    region: Option<RawRect>,
    #[serde(default)]
    edge: Option<String>,
    #[serde(default)]
    grid: bool,
    #[serde(default)]
    rotation: Option<f64>,
    #[serde(default)]
    surround: Option<String>,
}

impl RawGroupHint {
    fn into_group(self) -> std::result::Result<GroupHint, String> {
        if let Some(rotation) = self.rotation
            && ![0.0, 90.0, 180.0, 270.0].contains(&rotation)
        {
            return Err(format!(
                "invalid placement rotation `{rotation}`; expected 0, 90, 180, or 270"
            ));
        }
        Ok(GroupHint {
            name: self.name,
            members: self.members,
            region: self.region.map(RawRect::into_rect),
            edge: self.edge.as_deref().map(parse_edge).transpose()?,
            grid: self.grid,
            rotation: self.rotation,
            surround: self.surround,
        })
    }
}

fn parse_edge(edge: &str) -> std::result::Result<Edge, String> {
    match edge.to_ascii_lowercase().as_str() {
        "n" | "north" => Ok(Edge::N),
        "s" | "south" => Ok(Edge::S),
        "e" | "east" => Ok(Edge::E),
        "w" | "west" => Ok(Edge::W),
        _ => Err(format!("invalid placement edge `{edge}`")),
    }
}
