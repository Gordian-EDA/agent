//! Board-construction tools and shared input parsers.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;

use anyhow::Result;
use kicad_cli::KicadCli;
use serde_json::{Value, json};

use kicad_footprint::FootprintId;
use pcb_model::{Point2, Polygon};
use pcb_place::placement::{LockedAt, Rect};

use crate::AgentRuntime;

use super::fmt_num;
use super::seed::{BoardSeedRules, PourSpec};

// ── regenerate_board ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct BoardSeedSpec {
    bounds: Rect,
    rules: SeedRules,
    parts: Vec<SeedPart>,
    outline: Option<Polygon>,
}

#[derive(Debug, Clone)]
struct SeedPart {
    reference: String,
    footprint: String,
    pad_nets: BTreeMap<String, String>,
    locked: Option<LockedAt>,
}

#[derive(Debug, Clone)]
struct SeedRules {
    clearance: f64,
    min_trace_width: f64,
    via_diameter: f64,
    via_drill: f64,
    layer_count: u32,
    net_widths: BTreeMap<String, f64>,
    pours: Vec<PourSpec>,
}

impl Default for SeedRules {
    fn default() -> Self {
        Self {
            clearance: 0.15,
            min_trace_width: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            layer_count: 2,
            net_widths: BTreeMap::new(),
            pours: Vec::new(),
        }
    }
}

impl From<&BoardSeedRules> for SeedRules {
    fn from(rules: &BoardSeedRules) -> Self {
        Self {
            clearance: rules.clearance,
            min_trace_width: rules.min_trace_width,
            via_diameter: rules.via_diameter,
            via_drill: rules.via_drill,
            layer_count: rules.layer_count,
            net_widths: rules.net_widths.clone(),
            pours: rules.pours.clone(),
        }
    }
}

fn resolve_pour_layer(layer: &str, layer_count: u32) -> Option<(u32, String)> {
    match layer {
        "top" => Some((0, "F.Cu".to_string())),
        "bottom" => Some((layer_count - 1, "B.Cu".to_string())),
        _ if layer.starts_with("inner") => layer
            .trim_start_matches("inner")
            .parse::<u32>()
            .ok()
            .filter(|idx| *idx > 0 && *idx < layer_count.max(1) - 1)
            .map(|idx| (idx, format!("In{idx}.Cu"))),
        _ => None,
    }
}

fn blocking_erc_warnings(report: &kicad_cli::ErcReport) -> Vec<Value> {
    report
        .violations
        .iter()
        .filter(|v| {
            v.severity == "warning"
                && !v.kind.starts_with("lib_symbol")
                && v.kind != "global_label_dangling"
        })
        .map(|v| {
            let items: Vec<_> = v
                .items
                .iter()
                .map(|item| item.description.clone())
                .collect();
            json!({
                "type": v.kind,
                "description": v.description,
                "items": items,
            })
        })
        .collect()
}

/// `regenerate_board` — seed the PCB from KiCAD's own schematic netlist export.
///
/// Footprints must already be assigned in the schematic. Missing footprints are a
/// hard error: the agent should edit the circuit YAML, apply it, then regenerate
/// the board again.
pub fn regenerate_board(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    if !ctx.sch_path().exists() {
        return Ok(json!({
            "error": "no .kicad_sch yet — commit the schematic with apply_design first, \
                      then regenerate_board"
        }));
    }
    let netlist = match KicadCli::new(ctx.env()).netlist(ctx.sch_path()) {
        Ok(netlist) => netlist,
        Err(e) => {
            return Ok(json!({ "error": format!("could not export the schematic netlist: {e}") }));
        }
    };
    let unapplied_footprints = unapplied_draft_footprint_changes(ctx, &netlist);
    if !unapplied_footprints.is_empty() {
        return Ok(json!({
            "ok": false,
            "unapplied_draft_footprints": unapplied_footprints,
            "next_tool": "apply_design",
            "next": "call apply_design() to write the draft footprint fields, then regenerate_board again",
            "note": "footprint fields live in circuit-YAML/schematic state; do not retry regenerate_board until the draft footprint changes are applied",
        }));
    }
    let erc = match KicadCli::new(ctx.env()).erc(ctx.sch_path()) {
        Ok(report) => report,
        Err(e) => {
            return Ok(
                json!({ "error": format!("could not run ERC before regenerating the board: {e}") }),
            );
        }
    };
    if erc.error_count() > 0 {
        let violations: Vec<Value> = erc
            .violations
            .iter()
            .filter(|v| v.severity == "error")
            .map(|v| {
                json!({
                    "type": v.kind,
                    "description": v.description,
                })
            })
            .collect();
        return Ok(json!({
            "ok": false,
            "error": format!(
                "schematic ERC has {} error(s); fix and apply_design before regenerate_board",
                erc.error_count()
            ),
            "erc": {
                "errors": erc.error_count(),
                "warnings": erc.warning_count(),
                "violations": violations,
            },
        }));
    }
    let blocking_warnings = blocking_erc_warnings(&erc);
    if !blocking_warnings.is_empty() {
        return Ok(json!({
            "ok": false,
            "error": format!(
                "schematic ERC has {} actionable warning(s); fix and apply_design before regenerate_board",
                blocking_warnings.len()
            ),
            "erc": {
                "errors": erc.error_count(),
                "warnings": erc.warning_count(),
                "blocking_warnings": blocking_warnings,
            },
            "note": "Library symbol warnings and composed-sheet dangling global-label artifacts are allowed; same local/global labels and other connectivity warnings must be fixed before PCB work.",
        }));
    }
    let mut pad_nets_by_ref: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for net in &netlist.nets {
        if net.name.is_empty() {
            continue;
        }
        for (reference, pin) in &net.nodes {
            if reference.is_empty() || pin.is_empty() {
                continue;
            }
            pad_nets_by_ref
                .entry(reference.clone())
                .or_default()
                .insert(pin.clone(), net.name.clone());
        }
    }

    let mut parts = Vec::with_capacity(netlist.components.len());
    let mut missing_footprints = Vec::new();
    for component in &netlist.components {
        let footprint = component
            .properties
            .get("Footprint")
            .cloned()
            .unwrap_or_default();
        if footprint.is_empty() {
            missing_footprints.push(component.reference.clone());
        }
        parts.push(SeedPart {
            reference: component.reference.clone(),
            footprint,
            pad_nets: pad_nets_by_ref
                .remove(&component.reference)
                .unwrap_or_default(),
            locked: None,
        });
    }

    let bounds = if input.get("bounds").is_some() {
        match parse_bounds(input.get("bounds")) {
            Ok(b) => b,
            Err(e) => return Ok(json!({ "error": e })),
        }
    } else {
        Rect {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 50.0,
            max_y: 40.0,
        }
    };
    let mut rules = match parse_seed_rules(input.get("rules")) {
        Ok(r) => r,
        Err(msg) => return Ok(json!({ "error": msg })),
    };

    let part_count = parts.len();

    if !missing_footprints.is_empty() {
        return Ok(json!({
            "ok": false,
            "part_count": part_count,
            "missing_footprints": missing_footprints,
            "next_tool": "assign_footprints",
            "next": "call assign_footprints({assignments:[{reference, footprint}, ...]}), then apply_design(), then regenerate_board again",
            "note": "some schematic symbols have no footprint field — do not retry regenerate_board until footprints are assigned in the circuit-YAML draft and applied",
        }));
    }

    add_default_power_pours(&mut rules, &parts);

    let spec = BoardSeedSpec {
        bounds,
        rules,
        parts,
        outline: None,
    };

    match write_seed_board(&spec, ctx) {
        Ok(()) => {}
        Err(msg) => return Ok(json!({ "error": msg })),
    }
    Ok(json!({
        "ok": true,
        "part_count": part_count,
        "path": ctx.pcb_path().display().to_string(),
        "note": "board regenerated from the committed schematic file (not F8 sync; existing placement/routing may be replaced) — run place_board, then route_board, then check_board",
    }))
}

fn unapplied_draft_footprint_changes(
    ctx: &AgentRuntime,
    netlist: &kicad_cli::Netlist,
) -> Vec<Value> {
    let Some(draft) = ctx.workspace().read_draft() else {
        return Vec::new();
    };
    let Some(design) = circuit_lang::compile(&draft, ctx.provider()).design else {
        return Vec::new();
    };
    let committed: BTreeMap<String, String> = netlist
        .components
        .iter()
        .map(|c| {
            (
                c.reference.clone(),
                c.properties.get("Footprint").cloned().unwrap_or_default(),
            )
        })
        .collect();
    let mut changes = Vec::new();
    for block in design.blocks.values() {
        for (reference, component) in &block.components {
            let Some(draft_fp) = component.footprint.as_deref().filter(|s| !s.is_empty()) else {
                continue;
            };
            let committed_fp = committed.get(reference).map(String::as_str).unwrap_or("");
            if committed_fp != draft_fp {
                changes.push(json!({
                    "reference": reference,
                    "draft": draft_fp,
                    "committed": committed_fp,
                }));
            }
        }
    }
    changes
}

fn write_seed_board(spec: &BoardSeedSpec, ctx: &AgentRuntime) -> std::result::Result<(), String> {
    let catalog = ctx
        .footprint_catalog()
        .map_err(|e| format!("footprint catalog unavailable: {e}"))?;
    let mut parts = Vec::with_capacity(spec.parts.len());
    let mut x = spec.bounds.min_x + 2.0;
    let y = spec.bounds.min_y + 2.0;
    for dp in &spec.parts {
        let id = FootprintId::parse(&dp.footprint).map_err(|e| {
            format!(
                "part {}: invalid footprint id `{}`: {e}",
                dp.reference, dp.footprint
            )
        })?;
        let source = catalog.source(&id).map_err(|e| {
            if e.is_not_found() {
                let suggestions = catalog
                    .suggest(&id)
                    .iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "part {}: unknown footprint `{}` — use one of these real lib_ids if suitable: {suggestions}",
                    dp.reference, dp.footprint
                )
            } else {
                format!(
                    "part {}: footprint `{}` source is not readable: {e} — edit the schematic footprint field",
                    dp.reference, dp.footprint
                )
            }
        })?;
        parts.push(SeedFootprint {
            reference: dp.reference.clone(),
            lib_id: dp.footprint.clone(),
            source,
            pad_nets: dp.pad_nets.clone(),
            at: dp.locked.as_ref().map(|l| l.at).unwrap_or(Point2 { x, y }),
            rotation: dp.locked.as_ref().map(|l| l.rotation).unwrap_or(0.0),
            locked: dp.locked.is_some(),
        });
        x += 2.54;
    }
    let text = SeedBoardWriter::new(&parts, &spec.bounds, &spec.rules, spec.outline.as_ref())
        .emit()
        .map_err(|e| format!("board synthesis failed: {e}"))?;
    std::fs::write(ctx.pcb_path(), text)
        .map_err(|e| format!("could not write {}: {e}", ctx.pcb_path().display()))
}

fn add_default_power_pours(rules: &mut SeedRules, parts: &[SeedPart]) {
    if !rules.pours.is_empty() || rules.layer_count < 6 {
        return;
    }
    let nets: std::collections::BTreeSet<&str> = parts
        .iter()
        .flat_map(|part| part.pad_nets.values().map(String::as_str))
        .collect();
    if nets.contains("GND") {
        rules.pours.push(PourSpec {
            net: "GND".to_string(),
            layer: "bottom".to_string(),
        });
        rules.pours.push(PourSpec {
            net: "GND".to_string(),
            layer: format!("inner{}", rules.layer_count - 2),
        });
    }
    if nets.contains("V3V3") {
        rules.pours.push(PourSpec {
            net: "V3V3".to_string(),
            layer: "inner1".to_string(),
        });
    }
}

#[derive(Debug, Clone)]
struct SeedFootprint {
    reference: String,
    lib_id: String,
    source: String,
    pad_nets: BTreeMap<String, String>,
    at: Point2,
    rotation: f64,
    locked: bool,
}

#[derive(Debug, Clone)]
struct SeedNetClass {
    name: String,
    description: String,
    clearance: f64,
    trace_width: f64,
    via_diameter: f64,
    via_drill: f64,
    members: Vec<String>,
}

fn seed_net_classes(
    rules: &SeedRules,
    nets: impl IntoIterator<Item = String>,
) -> Vec<SeedNetClass> {
    let mut by_width: std::collections::BTreeMap<String, (f64, Vec<String>)> =
        std::collections::BTreeMap::new();
    for net in nets {
        let width = rules
            .net_widths
            .get(&net)
            .copied()
            .unwrap_or(rules.min_trace_width);
        let key = if (width - rules.min_trace_width).abs() < geom::EPS {
            "Default".to_owned()
        } else {
            format!("Width_{}", fmt_num(width).replace('.', "_"))
        };
        by_width
            .entry(key)
            .or_insert_with(|| (width, Vec::new()))
            .1
            .push(net);
    }

    let mut net_classes = Vec::new();
    for (name, (trace_width, mut members)) in by_width {
        members.sort();
        net_classes.push(SeedNetClass {
            description: if name == "Default" {
                "default board routing rules".to_owned()
            } else {
                format!("{}mm trace-width nets", fmt_num(trace_width))
            },
            name,
            clearance: rules.clearance,
            trace_width,
            via_diameter: rules.via_diameter,
            via_drill: rules.via_drill,
            members,
        });
    }
    net_classes
}

struct SeedBoardWriter<'a> {
    parts: &'a [SeedFootprint],
    bounds: &'a Rect,
    rules: &'a SeedRules,
    outline: Option<&'a Polygon>,
    net_codes: BTreeMap<String, i32>,
    net_classes: Vec<SeedNetClass>,
}

impl<'a> SeedBoardWriter<'a> {
    fn new(
        parts: &'a [SeedFootprint],
        bounds: &'a Rect,
        rules: &'a SeedRules,
        outline: Option<&'a Polygon>,
    ) -> Self {
        let net_codes = seed_net_codes(parts);
        let net_classes = seed_net_classes(rules, net_codes.keys().cloned());
        Self {
            parts,
            bounds,
            rules,
            outline,
            net_codes,
            net_classes,
        }
    }

    fn emit(&self) -> io::Result<String> {
        let mut out = String::with_capacity(4096 + self.parts.len() * 1024);
        out.push_str("(kicad_pcb\n");
        out.push_str("\t(version 20241229)\n");
        out.push_str("\t(generator \"gordian\")\n");
        out.push_str("\t(generator_version \"0.1\")\n");
        out.push_str("\t(general\n\t\t(thickness 1.6)\n\t\t(legacy_teardrops no)\n\t)\n");
        out.push_str("\t(paper \"A4\")\n");
        self.push_layers(&mut out);
        out.push_str(
            "\t(setup\n\t\t(pad_to_mask_clearance 0)\n\
             \t\t(allow_soldermask_bridges_in_footprints no)\n\
             \t\t(aux_axis_origin 0 0)\n\t\t(grid_origin 0 0)\n\t)\n",
        );
        self.push_nets(&mut out);
        self.push_net_classes(&mut out);
        self.push_edge_cuts(&mut out);
        self.push_zones(&mut out)?;
        for part in self.parts {
            out.push_str(&self.emit_footprint(part)?);
        }
        out.push_str(")\n");
        Ok(out)
    }

    fn push_layers(&self, out: &mut String) {
        out.push_str("\t(layers\n");
        out.push_str("\t\t(0 \"F.Cu\" signal)\n");
        if self.rules.layer_count >= 4 {
            for i in 1..=(self.rules.layer_count - 2) {
                let _ = writeln!(out, "\t\t({i} \"In{i}.Cu\" signal)");
            }
            let _ = writeln!(out, "\t\t({} \"B.Cu\" signal)", self.rules.layer_count - 1);
        } else {
            out.push_str("\t\t(2 \"B.Cu\" signal)\n");
        }
        out.push_str("\t\t(36 \"B.SilkS\" user \"B.Silkscreen\")\n");
        out.push_str("\t\t(37 \"F.SilkS\" user \"F.Silkscreen\")\n");
        out.push_str("\t\t(38 \"B.Mask\" user)\n");
        out.push_str("\t\t(39 \"F.Mask\" user)\n");
        out.push_str("\t\t(44 \"Edge.Cuts\" user)\n");
        out.push_str("\t)\n");
    }

    fn push_nets(&self, out: &mut String) {
        out.push_str("\t(net 0 \"\")\n");
        let mut by_code: Vec<_> = self
            .net_codes
            .iter()
            .map(|(name, code)| (code, name))
            .collect();
        by_code.sort();
        for (code, name) in by_code {
            let _ = writeln!(out, "\t(net {code} \"{name}\")");
        }
    }

    fn push_net_classes(&self, out: &mut String) {
        for class in &self.net_classes {
            let members: Vec<_> = class
                .members
                .iter()
                .filter(|net| self.net_codes.contains_key(*net))
                .collect();
            if members.is_empty() {
                continue;
            }
            let _ = writeln!(
                out,
                "\t(net_class \"{}\" \"{}\"",
                class.name, class.description
            );
            let _ = writeln!(out, "\t\t(clearance {})", fmt_num(class.clearance));
            let _ = writeln!(out, "\t\t(trace_width {})", fmt_num(class.trace_width));
            let _ = writeln!(out, "\t\t(via_dia {})", fmt_num(class.via_diameter));
            let _ = writeln!(out, "\t\t(via_drill {})", fmt_num(class.via_drill));
            for member in members {
                let _ = writeln!(out, "\t\t(add_net \"{member}\")");
            }
            out.push_str("\t)\n");
        }
    }

    fn push_edge_cuts(&self, out: &mut String) {
        if let Some(polygon) = self.outline {
            let points = polygon.points();
            for idx in 0..points.len() {
                let a = points[idx];
                let b = points[(idx + 1) % points.len()];
                let (x0, y0) = (fmt_num(a.x), fmt_num(a.y));
                let (x1, y1) = (fmt_num(b.x), fmt_num(b.y));
                let uuid = seed_uuid(&format!("edge:{x0}:{y0}:{x1}:{y1}"));
                let _ = write!(
                    out,
                    "\t(gr_line\n\t\t(start {x0} {y0})\n\t\t(end {x1} {y1})\n\
                     \t\t(stroke\n\t\t\t(width 0.1)\n\t\t\t(type default)\n\t\t)\n\
                     \t\t(layer \"Edge.Cuts\")\n\t\t(uuid \"{uuid}\")\n\t)\n"
                );
            }
            return;
        }
        let (x0, y0) = (fmt_num(self.bounds.min_x), fmt_num(self.bounds.min_y));
        let (x1, y1) = (fmt_num(self.bounds.max_x), fmt_num(self.bounds.max_y));
        let uuid = seed_uuid(&format!("edge:{x0}:{y0}:{x1}:{y1}"));
        let _ = write!(
            out,
            "\t(gr_rect\n\t\t(start {x0} {y0})\n\t\t(end {x1} {y1})\n\
             \t\t(stroke\n\t\t\t(width 0.1)\n\t\t\t(type default)\n\t\t)\n\
             \t\t(fill no)\n\t\t(layer \"Edge.Cuts\")\n\t\t(uuid \"{uuid}\")\n\t)\n"
        );
    }

    fn push_zones(&self, out: &mut String) -> io::Result<()> {
        for (idx, pour) in self.rules.pours.iter().enumerate() {
            let net_code = self.net_codes.get(&pour.net).copied().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "rules.pours[{idx}] net {:?} is not present on any pad",
                        pour.net
                    ),
                )
            })?;
            let Some((_, layer_name)) = resolve_pour_layer(&pour.layer, self.rules.layer_count)
            else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "rules.pours[{idx}] layer {:?} is invalid for {} layers",
                        pour.layer, self.rules.layer_count
                    ),
                ));
            };
            self.write_zone(out, net_code, &pour.net, &layer_name, &format!("{idx}"));
        }
        // Solid GND/VCC planes on the centred inner layers — the physical
        // counterpart of the router's plane fanout (per-pad vias assume real
        // plane copper, and kicad-cli DRC checks the file, not our oracle).
        let pad_counts = self.parts.iter().flat_map(|p| p.pad_nets.values()).fold(
            BTreeMap::<String, usize>::new(),
            |mut acc, net| {
                *acc.entry(net.clone()).or_default() += 1;
                acc
            },
        );
        for (net, layer_idx) in
            pcb_model::default_plane_nets(self.rules.layer_count, pad_counts.into_iter())
        {
            let Some(net_code) = self.net_codes.get(&net).copied() else {
                continue;
            };
            let layer_name = format!("In{layer_idx}.Cu");
            self.write_zone(out, net_code, &net, &layer_name, "plane");
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn write_zone(
        &self,
        out: &mut String,
        net_code: i32,
        net: &str,
        layer_name: &str,
        tag: &str,
    ) {
        let clearance = fmt_num(self.rules.clearance);
        let min_thickness = fmt_num(self.rules.min_trace_width.max(0.1));
        let thermal_gap = fmt_num((self.rules.clearance * 2.0).max(0.2));
        let thermal_bridge_width = fmt_num(self.rules.min_trace_width.max(0.25));
        let x0 = fmt_num(self.bounds.min_x);
        let y0 = fmt_num(self.bounds.min_y);
        let x1 = fmt_num(self.bounds.max_x);
        let y1 = fmt_num(self.bounds.max_y);
        let uuid = seed_uuid(&format!("zone:{tag}:{net}:{layer_name}"));
        let _ = write!(
            out,
            "\t(zone\n\
             \t\t(net {net_code})\n\
             \t\t(net_name \"{net}\")\n\
             \t\t(layer \"{layer_name}\")\n\
             \t\t(uuid \"{uuid}\")\n\
             \t\t(name \"{net}\")\n\
             \t\t(hatch full 0.508)\n\
             \t\t(connect_pads\n\
             \t\t\t(clearance {clearance})\n\
             \t\t)\n\
             \t\t(min_thickness {min_thickness})\n\
             \t\t(filled_areas_thickness no)\n\
             \t\t(fill\n\
             \t\t\t(thermal_gap {thermal_gap})\n\
             \t\t\t(thermal_bridge_width {thermal_bridge_width})\n\
             \t\t)\n\
             \t\t(polygon\n\
             \t\t\t(pts\n\
             \t\t\t\t(xy {x0} {y0}) (xy {x1} {y0}) (xy {x1} {y1}) (xy {x0} {y1})\n\
             \t\t\t)\n\
             \t\t)\n\
             \t)\n",
        );
    }

    fn emit_footprint(&self, part: &SeedFootprint) -> io::Result<String> {
        emit_seed_footprint(part, &self.net_codes)
    }
}

fn seed_net_codes(parts: &[SeedFootprint]) -> BTreeMap<String, i32> {
    let mut names = std::collections::BTreeSet::new();
    for part in parts {
        for net in part.pad_nets.values() {
            if !net.is_empty() {
                names.insert(net.clone());
            }
        }
    }
    names
        .into_iter()
        .enumerate()
        .map(|(idx, net)| (net, idx as i32 + 1))
        .collect()
}

fn emit_seed_footprint(
    part: &SeedFootprint,
    net_codes: &BTreeMap<String, i32>,
) -> io::Result<String> {
    let body = footprint_body(&part.source).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "part {}: source has no (footprint ...) block",
                part.reference
            ),
        )
    })?;
    let inner = footprint_inner(body).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("part {}: malformed footprint source", part.reference),
        )
    })?;
    let norm = part.rotation.rem_euclid(360.0);
    let rot = geom::snap_quadrant(norm) as i32;
    if (rot as f64 - norm).abs() > geom::EPS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "part {}: non-axis-aligned seed rotation {norm}",
                part.reference
            ),
        ));
    }

    let mut out = String::with_capacity(body.len() + 256);
    let _ = writeln!(out, "\t(footprint \"{}\"", part.lib_id);
    if part.locked {
        out.push_str("\t\t(locked yes)\n");
    }
    out.push_str("\t\t(layer \"F.Cu\")\n");
    let _ = writeln!(
        out,
        "\t\t(uuid \"{}\")",
        seed_uuid(&format!("fp:{}:{}", part.reference, part.lib_id))
    );
    if rot == 0 {
        let _ = writeln!(
            out,
            "\t\t(at {} {})",
            fmt_num(part.at.x),
            fmt_num(part.at.y)
        );
    } else {
        let _ = writeln!(
            out,
            "\t\t(at {} {} {})",
            fmt_num(part.at.x),
            fmt_num(part.at.y),
            rot
        );
    }

    for node in top_level_nodes(inner) {
        if let Some(transformed) = transform_seed_node(node, part, net_codes, rot)? {
            push_reindented(&mut out, &transformed);
        }
    }
    out.push_str("\t)\n");
    Ok(out)
}

fn transform_seed_node(
    node: &str,
    part: &SeedFootprint,
    net_codes: &BTreeMap<String, i32>,
    fp_rot: i32,
) -> io::Result<Option<String>> {
    match node_head(node) {
        "at" | "uuid" | "layer" => Ok(None),
        "fp_text" if on_silk(node) => Ok(None),
        "version"
        | "generator"
        | "generator_version"
        | "embedded_fonts"
        | "model"
        | "tags"
        | "descr"
        | "duplicate_pad_numbers_are_jumpers" => Ok(None),
        "property" => Ok(Some(transform_seed_property(node, &part.reference))),
        "pad" => Ok(Some(transform_seed_pad(node, part, net_codes, fp_rot)?)),
        _ => Ok(Some(node.to_owned())),
    }
}

const REF_TEXT_SIZE_MM: f64 = 0.8;

fn transform_seed_property(node: &str, reference: &str) -> String {
    if let Some(rest) = node.strip_prefix("(property \"Reference\" \"")
        && let Some(close) = rest.find('"')
    {
        let body = cap_font_size(&rest[close + 1..], REF_TEXT_SIZE_MM);
        return format!("(property \"Reference\" \"{reference}\"{body}");
    }
    if node.starts_with("(property \"Value\"")
        && !node.contains("(hide yes)")
        && let Some(hidden) = inject_before_close(node, "(hide yes)")
    {
        return hidden;
    }
    node.to_owned()
}

fn transform_seed_pad(
    node: &str,
    part: &SeedFootprint,
    net_codes: &BTreeMap<String, i32>,
    fp_rot: i32,
) -> io::Result<String> {
    let number = pad_number(node);
    let rotated = if fp_rot != 0 {
        bump_pad_rotation(node, fp_rot)
    } else {
        node.to_owned()
    };
    let Some(number) = number else {
        return Ok(rotated);
    };
    let Some(net) = part.pad_nets.get(&number).filter(|n| !n.is_empty()) else {
        return Ok(rotated);
    };
    let code = net_codes.get(net).copied().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "part {}: pad {number} net {net:?} missing from net table",
                part.reference
            ),
        )
    })?;
    inject_before_close(&rotated, &format!("(net {code} \"{net}\")")).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("part {}: pad {number} has no closing paren", part.reference),
        )
    })
}

fn footprint_body(source: &str) -> Option<&str> {
    let start = source.find("(footprint ")?;
    let end = matching_close(source, start)?;
    Some(&source[start..=end])
}

fn footprint_inner(body: &str) -> Option<&str> {
    let after_kw = body.strip_prefix("(footprint ")?;
    let rest = after_kw.strip_prefix('"')?;
    let name_close = rest.find('"')?;
    let inner_start = "(footprint ".len() + 1 + name_close + 1;
    Some(&body[inner_start..body.len() - 1])
}

fn top_level_nodes(inner: &str) -> Vec<&str> {
    let bytes = inner.as_bytes();
    let mut nodes = Vec::new();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'('
            && let Some(end) = matching_close(inner, idx)
        {
            nodes.push(&inner[idx..=end]);
            idx = end + 1;
            continue;
        }
        idx += 1;
    }
    nodes
}

fn matching_close(s: &str, open: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut idx = open;
    while idx < bytes.len() {
        match bytes[idx] {
            b'"' => in_string = !in_string,
            b'(' if !in_string => depth += 1,
            b')' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
        idx += 1;
    }
    None
}

fn node_head(node: &str) -> &str {
    let rest = node.strip_prefix('(').unwrap_or(node);
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .unwrap_or(rest.len());
    &rest[..end]
}

fn on_silk(node: &str) -> bool {
    node.contains("(layer \"F.SilkS\")") || node.contains("(layer \"B.SilkS\")")
}

fn pad_number(node: &str) -> Option<String> {
    let rest = node.strip_prefix("(pad ")?;
    let rest = rest.strip_prefix('"')?;
    let close = rest.find('"')?;
    Some(rest[..close].to_owned())
}

fn inject_before_close(node: &str, insertion: &str) -> Option<String> {
    let close = node.rfind(')')?;
    let close_line_start = node[..close].rfind('\n').map(|p| p + 1).unwrap_or(close);
    let close_indent = &node[close_line_start..close];
    let indent = child_indent(node);
    let mut out = String::with_capacity(node.len() + insertion.len() + indent.len() + 2);
    out.push_str(node[..close].trim_end());
    out.push('\n');
    out.push_str(&indent);
    out.push_str(insertion);
    out.push('\n');
    out.push_str(close_indent);
    out.push_str(&node[close..]);
    Some(out)
}

fn child_indent(node: &str) -> String {
    let Some(nl) = node.find('\n') else {
        return String::new();
    };
    node[nl + 1..]
        .chars()
        .take_while(|c| *c == '\t' || *c == ' ')
        .collect()
}

fn push_reindented(out: &mut String, node: &str) {
    for (i, line) in node.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !line.is_empty() {
            out.push('\t');
        }
        out.push_str(line);
    }
    out.push('\n');
}

fn cap_font_size(body: &str, max: f64) -> String {
    let Some(start) = body.find("(size ") else {
        return body.to_owned();
    };
    let open = start + "(size ".len();
    let Some(rel_close) = body[open..].find(')') else {
        return body.to_owned();
    };
    let inner = &body[open..open + rel_close];
    let nums: Vec<f64> = inner
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    if nums.len() != 2 {
        return body.to_owned();
    }
    let (w, h) = (nums[0].min(max), nums[1].min(max));
    format!(
        "{}(size {} {}){}",
        &body[..start],
        fmt_num(w),
        fmt_num(h),
        &body[open + rel_close + 1..]
    )
}

fn bump_pad_rotation(node: &str, fp_rot: i32) -> String {
    const AT: &str = "(at ";
    let Some(at_pos) = node.find(AT) else {
        return node.to_owned();
    };
    let after = &node[at_pos + AT.len()..];
    let Some(line_end) = after.find(')') else {
        return node.to_owned();
    };
    let inside = &after[..line_end];
    let nums: Vec<&str> = inside.split_whitespace().collect();
    let (x, y) = match (nums.first(), nums.get(1)) {
        (Some(x), Some(y)) => (*x, *y),
        _ => return node.to_owned(),
    };
    let pad_rot: f64 = nums.get(2).and_then(|r| r.parse().ok()).unwrap_or(0.0);
    let new_rot = (pad_rot + fp_rot as f64).rem_euclid(360.0);
    let replacement = if new_rot == 0.0 {
        format!("(at {x} {y}")
    } else {
        format!("(at {x} {y} {})", fmt_num(new_rot))
    };
    let mut out = String::with_capacity(node.len() + 8);
    out.push_str(&node[..at_pos]);
    out.push_str(&replacement);
    out.push_str(&node[at_pos + AT.len() + line_end + 1..]);
    out
}

fn seed_uuid(key: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h1 = std::collections::hash_map::DefaultHasher::new();
    "gordian-seed-a".hash(&mut h1);
    key.hash(&mut h1);
    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    "gordian-seed-b".hash(&mut h2);
    key.hash(&mut h2);
    let a = h1.finish();
    let b = h2.finish();
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (a >> 32) as u32,
        (a >> 16) as u16,
        (a as u16 & 0x0fff) | 0x5000,
        ((b >> 48) as u16 & 0x3fff) | 0x8000,
        b & 0x0000_ffff_ffff_ffff
    )
}

// ── seed input parsing ───────────────────────────────────────────────────────

/// Read one required `f64` field from a JSON object, returning a model-readable
/// error message string on failure.
pub(super) fn req_num(obj: &Value, key: &str, ctx: &str) -> std::result::Result<f64, String> {
    obj.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("{ctx}: missing or non-numeric `{key}`"))
}

/// Parse the board `bounds` from model input into the engine's [`Rect`].
///
/// This is a live LLM boundary: models send the rect in whatever shape their
/// training favors, and a strict parser turns each guess into a dead retry
/// loop (observed: DeepSeek burning 6+ calls on `missing max_x`). Accept the
/// common shapes — snake_case/camelCase corners, `{x, y, width, height}`,
/// `{width, height}` (origin 0), a `[min_x, min_y, max_x, max_y]` array,
/// numbers-as-strings with an optional `mm` suffix — and teach the canonical
/// shape in the error when nothing matches.
fn parse_bounds(v: Option<&Value>) -> std::result::Result<Rect, String> {
    const EXPECT: &str = r#"expected {"min_x":0,"min_y":0,"max_x":60,"max_y":40} in mm (or {x,y,width,height}, {width,height}, or [min_x,min_y,max_x,max_y])"#;
    let Some(val) = v else {
        return Err(format!("missing required `bounds`; {EXPECT}"));
    };
    fn num(v: &Value) -> Option<f64> {
        v.as_f64().or_else(|| {
            v.as_str()?
                .trim()
                .trim_end_matches("mm")
                .trim()
                .parse()
                .ok()
        })
    }
    let rect = |min_x: f64, min_y: f64, max_x: f64, max_y: f64| Rect {
        min_x: min_x.min(max_x),
        min_y: min_y.min(max_y),
        max_x: min_x.max(max_x),
        max_y: min_y.max(max_y),
    };
    if let Some(arr) = val.as_array() {
        if let [a, b, c, d] = arr.as_slice()
            && let (Some(a), Some(b), Some(c), Some(d)) = (num(a), num(b), num(c), num(d))
        {
            return Ok(rect(a, b, c, d));
        }
        return Err(format!("bounds: array must be 4 numbers; {EXPECT}"));
    }
    let get = |keys: &[&str]| keys.iter().find_map(|k| val.get(*k)).and_then(num);
    let corners = (
        get(&["min_x", "minX", "x0", "left"]),
        get(&["min_y", "minY", "y0", "top"]),
        get(&["max_x", "maxX", "x1", "right"]),
        get(&["max_y", "maxY", "y1", "bottom"]),
    );
    if let (Some(x0), Some(y0), Some(x1), Some(y1)) = corners {
        return Ok(rect(x0, y0, x1, y1));
    }
    if let (Some(w), Some(h)) = (get(&["width", "w"]), get(&["height", "h"])) {
        let (x, y) = (get(&["x", "min_x", "minX"]).unwrap_or(0.0), get(&["y", "min_y", "minY"]).unwrap_or(0.0));
        return Ok(rect(x, y, x + w, y + h));
    }
    Err(format!("bounds: could not read a rect from {val}; {EXPECT}"))
}

/// Parse optional `rules` from snake_case model input.
fn parse_seed_rules(v: Option<&Value>) -> std::result::Result<SeedRules, String> {
    parse_rules(v).map(|rules| SeedRules::from(&rules))
}

fn parse_rules(v: Option<&Value>) -> std::result::Result<BoardSeedRules, String> {
    let d = BoardSeedRules::default();
    let obj = match v {
        None | Some(Value::Null) => return Ok(d),
        Some(Value::Object(obj)) => obj,
        Some(_) => return Err("rules must be an object".to_string()),
    };
    for key in obj.keys() {
        if !matches!(
            key.as_str(),
            "clearance"
                | "min_trace_width"
                | "via_diameter"
                | "via_drill"
                | "layer_count"
                | "net_widths"
                | "pours"
        ) {
            return Err(format!(
                "rules.{key} is not supported; use snake_case rule names"
            ));
        }
    }
    // Partial rules are allowed: any omitted field falls back to the engine
    // default, so a caller can pass just `{ "layer_count": 4 }` or `{ "clearance": 0.15 }`.
    let num = |k: &str, fallback: f64| obj.get(k).and_then(Value::as_f64).unwrap_or(fallback);
    let layer_count = obj
        .get("layer_count")
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(d.layer_count);
    if !matches!(layer_count, 2 | 4 | 6 | 8) {
        return Err(format!(
            "rules.layer_count must be 2, 4, 6, or 8, got {layer_count}"
        ));
    }
    let via_diameter = num("via_diameter", d.via_diameter);
    let via_drill = num("via_drill", d.via_drill);
    // Vias must be fabricable to KiCAD's built-in standard-fab minimums (verified
    // against kicad-cli DRC): via ≥ 0.5 mm, drill ≥ 0.3 mm, annular ring ≥ 0.1 mm
    // (i.e. via ≥ drill + 0.2). Below these the board would route but fail KiCAD
    // DRC (via_diameter / drill_out_of_range / annular_width) — reject up front
    // with the floor, rather than silently emit copper that lies about fab.
    if via_diameter < KICAD_MIN_VIA_DIAMETER {
        return Err(format!(
            "rules.via_diameter {via_diameter} is below KiCAD's standard-fab minimum \
             {KICAD_MIN_VIA_DIAMETER}mm — raise it (microvias need custom board rules / a finer fab class)"
        ));
    }
    if via_drill < KICAD_MIN_VIA_DRILL {
        return Err(format!(
            "rules.via_drill {via_drill} is below KiCAD's standard-fab minimum {KICAD_MIN_VIA_DRILL}mm — raise it"
        ));
    }
    if via_diameter - via_drill < 2.0 * KICAD_MIN_ANNULAR {
        return Err(format!(
            "rules via annular ring {:.3}mm (= (via_diameter {via_diameter} − via_drill {via_drill})/2) is below \
             KiCAD's {KICAD_MIN_ANNULAR}mm minimum — widen the via or shrink the drill",
            (via_diameter - via_drill) / 2.0
        ));
    }
    // Per-net trace widths: {"VCC": 0.8, "GND": 0.8} — fat power, thin signals.
    let mut net_widths = std::collections::BTreeMap::new();
    if let Some(nw) = obj.get("net_widths") {
        let map = nw
            .as_object()
            .ok_or_else(|| "rules.net_widths must be an object {net: width_mm}".to_string())?;
        for (net, value) in map {
            let w = parse_net_width_value(net, value)?;
            if w <= 0.0 {
                return Err(format!("rules.net_widths[{net}] must be > 0, got {w}"));
            }
            if w > 1.5 {
                return Err(format!(
                    "rules.net_widths[{net}] is {w} mm; use <= 1.5 mm for routed traces, or enlarge the board/add planes"
                ));
            }
            net_widths.insert(net.clone(), w);
        }
    }
    // Copper pours: [{"net":"GND","layer":"bottom"}] — flood a net on a signal layer.
    let mut pours = Vec::new();
    if let Some(pv) = obj.get("pours") {
        let arr = pv
            .as_array()
            .ok_or_else(|| "rules.pours must be an array of {net, layer}".to_string())?;
        for p in arr {
            let net = p
                .get("net")
                .and_then(Value::as_str)
                .ok_or_else(|| "rules.pours[].net must be a string".to_string())?;
            let layer = p.get("layer").and_then(Value::as_str).unwrap_or("bottom");
            // A pour floods a SIGNAL layer (top/bottom, or an inner signal layer on a
            // 6-layer board) — never a GND/VCC PLANE (already a full copper layer) or a
            // non-existent layer. Resolve + reject up front rather than silently drop it.
            match resolve_pour_layer(layer, layer_count) {
                None => {
                    return Err(format!(
                        "rules.pours[].layer '{layer}' is not a valid copper layer on a \
                         {layer_count}-layer board — use top/bottom, or an existing innerN"
                    ));
                }
                Some((idx, _))
                    if grid_astar::router::plane_layers(layer_count as usize).contains(&idx) =>
                {
                    // Already a full copper plane there — the pour request is
                    // satisfied by construction; don't fail the regenerate.
                    continue;
                }
                Some(_) => {}
            }
            pours.push(PourSpec {
                net: net.to_string(),
                layer: layer.to_string(),
            });
        }
    }
    Ok(BoardSeedRules {
        clearance: num("clearance", d.clearance),
        min_trace_width: num("min_trace_width", d.min_trace_width),
        via_diameter,
        via_drill,
        layer_count,
        net_widths,
        pours,
    })
}

fn parse_net_width_value(net: &str, value: &Value) -> std::result::Result<f64, String> {
    if let Some(width) = value.as_f64() {
        return Ok(width);
    }
    if let Some(obj) = value.as_object()
        && let Some(width) = obj.get("width").and_then(Value::as_f64)
    {
        return Ok(width);
    }
    Err(format!(
        "rules.net_widths[{net}] must be a number in mm, e.g. {{\"{net}\": 0.6}}"
    ))
}

/// KiCAD 9 built-in (standard-fab) minimums, verified against `kicad-cli pcb drc`:
/// a via below these trips `via_diameter` / `drill_out_of_range` / `annular_width`.
const KICAD_MIN_VIA_DIAMETER: f64 = 0.5;
const KICAD_MIN_VIA_DRILL: f64 = 0.3;
const KICAD_MIN_ANNULAR: f64 = 0.1;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_seed_rules_accepts_plain_and_object_net_widths() {
        let rules = parse_seed_rules(Some(&json!({
            "layer_count": 4,
            "net_widths": {
                "GND": 0.6,
                "V3V3": { "width": 0.5 }
            }
        })))
        .unwrap();

        assert_eq!(rules.layer_count, 4);
        assert_eq!(rules.net_widths["GND"], 0.6);
        assert_eq!(rules.net_widths["V3V3"], 0.5);
    }

    #[test]
    fn parse_seed_rules_keeps_requested_pours() {
        let rules = parse_seed_rules(Some(&json!({
            "layer_count": 6,
            "pours": [
                { "net": "GND", "layer": "bottom" },
                { "net": "V3V3", "layer": "inner1" }
            ]
        })))
        .unwrap();

        assert_eq!(
            rules.pours,
            vec![
                PourSpec {
                    net: "GND".to_string(),
                    layer: "bottom".to_string(),
                },
                PourSpec {
                    net: "V3V3".to_string(),
                    layer: "inner1".to_string(),
                },
            ]
        );
    }

    #[test]
    fn parse_seed_rules_rejects_legacy_rule_names() {
        let err = parse_seed_rules(Some(&json!({
            "minTraceWidth": 0.15,
        })))
        .unwrap_err();

        assert!(err.contains("rules.minTraceWidth is not supported"));

        let err = parse_seed_rules(Some(&json!({
            "layers": 4,
        })))
        .unwrap_err();

        assert!(err.contains("rules.layers is not supported"));
    }

    #[test]
    fn seed_writer_emits_requested_pour_zone() {
        let mut pad_nets = BTreeMap::new();
        pad_nets.insert("1".to_string(), "GND".to_string());
        let parts = vec![SeedFootprint {
            reference: "TP1".to_string(),
            lib_id: "Test:Pad".to_string(),
            source:
                "(footprint \"Pad\" (pad \"1\" smd circle (at 0 0) (size 1 1) (layers \"F.Cu\")))"
                    .to_string(),
            pad_nets,
            at: Point2 { x: 5.0, y: 5.0 },
            rotation: 0.0,
            locked: false,
        }];
        let bounds = Rect {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 20.0,
            max_y: 10.0,
        };
        let mut rules = SeedRules::default();
        rules.pours.push(PourSpec {
            net: "GND".to_string(),
            layer: "bottom".to_string(),
        });

        let board = SeedBoardWriter::new(&parts, &bounds, &rules, None)
            .emit()
            .unwrap();

        assert!(board.contains("\n\t(zone\n"));
        assert!(board.contains("\n\t\t(net_name \"GND\")\n"));
        assert!(board.contains("\n\t\t(layer \"B.Cu\")\n"));
        assert!(board.contains("(xy 0 0) (xy 20 0) (xy 20 10) (xy 0 10)"));
    }

    #[test]
    fn default_power_pours_are_added_on_dense_stackups() {
        let mut rules = SeedRules {
            layer_count: 6,
            ..SeedRules::default()
        };
        let parts = vec![SeedPart {
            reference: "U1".to_string(),
            footprint: "Test:U".to_string(),
            pad_nets: BTreeMap::from([
                ("1".to_string(), "GND".to_string()),
                ("2".to_string(), "V3V3".to_string()),
            ]),
            locked: None,
        }];

        add_default_power_pours(&mut rules, &parts);

        assert_eq!(
            rules.pours,
            vec![
                PourSpec {
                    net: "GND".to_string(),
                    layer: "bottom".to_string(),
                },
                PourSpec {
                    net: "GND".to_string(),
                    layer: "inner4".to_string(),
                },
                PourSpec {
                    net: "V3V3".to_string(),
                    layer: "inner1".to_string(),
                },
            ]
        );
    }

    #[test]
    fn blocking_erc_warnings_allow_only_library_mismatch() {
        let report = kicad_cli::ErcReport {
            violations: vec![
                kicad_cli::Violation {
                    severity: "warning".to_string(),
                    kind: "lib_symbol_mismatch".to_string(),
                    description: "cached symbol differs".to_string(),
                    items: vec![],
                },
                kicad_cli::Violation {
                    severity: "warning".to_string(),
                    kind: "lib_symbol_issues".to_string(),
                    description: "library unavailable".to_string(),
                    items: vec![],
                },
                kicad_cli::Violation {
                    severity: "warning".to_string(),
                    kind: "same_local_global_label".to_string(),
                    description: "Local and global labels have same name".to_string(),
                    items: vec![kicad_cli::ViolationItem {
                        description: "Label 'USB_DP'".to_string(),
                        uuid: None,
                    }],
                },
            ],
        };

        let warnings = blocking_erc_warnings(&report);

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0]["type"], "same_local_global_label");
        assert_eq!(warnings[0]["items"][0], "Label 'USB_DP'");
    }
}
