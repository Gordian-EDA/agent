//! Board-construction tools and shared input parsers.

use sch_io::write::escape_sexpr_string as sexpr_escape;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;

use anyhow::Result;
use kicad_cli::KicadCli;
use serde_json::{Value, json};

use kicad_footprint::{FootprintCatalog, FootprintId};
use pcb_model::{Point2, Polygon};
use geom::Rect;
use place_model::LockedAt;

use crate::AgentRuntime;
use crate::tools::footprint_suggestion_clause;

use super::fmt_num;
use super::seed::{BoardSeedRules, PourPadConnection, PourSpec};

// ── regenerate_board ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(super) struct BoardSeedSpec {
    pub(super) bounds: Rect,
    pub(super) rules: SeedRules,
    pub(super) parts: Vec<SeedPart>,
    pub(super) outline: Option<Polygon>,
}

#[derive(Debug, Clone)]
pub(super) struct SeedPart {
    pub(super) reference: String,
    /// Value from the committed schematic netlist. Corpus-only seeds have no
    /// schematic counterpart and leave this unset to retain the library value.
    pub(super) value: Option<String>,
    pub(super) footprint: String,
    pub(super) pad_nets: BTreeMap<String, String>,
    pub(super) locked: Option<LockedAt>,
}

#[derive(Debug, Clone)]
pub(super) struct SeedRules {
    pub(super) clearance: f64,
    pub(super) min_trace_width: f64,
    pub(super) via_diameter: f64,
    pub(super) via_drill: f64,
    pub(super) layer_count: u32,
    pub(super) net_widths: BTreeMap<String, f64>,
    pub(super) pours: Vec<PourSpec>,
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
    let normalized = layer.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "top" | "f.cu" => Some((0, "F.Cu".to_string())),
        "bottom" | "b.cu" => Some((layer_count - 1, "B.Cu".to_string())),
        _ if normalized.starts_with("inner") || normalized.starts_with("in") => normalized
            .trim_start_matches("inner")
            .trim_start_matches("in")
            .trim_end_matches(".cu")
            .parse::<u32>()
            .ok()
            .filter(|idx| *idx > 0 && *idx < layer_count.max(1) - 1)
            .map(|idx| (idx, format!("In{idx}.Cu"))),
        _ => None,
    }
}

/// A dangling wire shorter than 0.1mm is emitter rounding residue, not a
/// broken connection: its endpoints sit inside any pin snap tolerance.
fn degenerate_wire_endpoint(v: &kicad_cli::Violation) -> bool {
    v.kind == "unconnected_wire_endpoint"
        && v.items.iter().all(|item| {
            item.description
                .split("length ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|len| len.parse::<f64>().ok())
                .is_some_and(|len| len < 0.1)
        })
}

fn blocking_erc_warnings(report: &kicad_cli::ErcReport) -> Vec<Value> {
    report
        .violations
        .iter()
        .filter(|v| {
            v.severity == "warning"
                && !v.kind.starts_with("lib_symbol")
                && v.kind != "global_label_dangling"
                && !degenerate_wire_endpoint(v)
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
    let unapplied_footprints = unapplied_draft_footprint_changes(ctx, &netlist)?;
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
            value: Some(component.value.clone()),
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
    apply_complexity_default_layer_count(&mut rules, input.get("rules"), part_count);

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

    let footprint_pin_mismatches = crate::footprint_compat::netlist_pin_mismatches(ctx, &netlist)?;
    if !footprint_pin_mismatches.is_empty() {
        return Ok(json!({
            "ok": false,
            "error": "schematic symbol and assigned footprint have incompatible numbered pins/pads",
            "footprint_pin_mismatches": footprint_pin_mismatches,
            "next_tool": "assign_footprints",
            "next": "choose a package whose named pad numbers match the symbol pins, apply_design(), then regenerate_board again",
            "note": "Every named electrical pad must match a symbol pin and every symbol pin must have a physical pad. Unnumbered mechanical pads and repeated pads with a valid shared number are allowed.",
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
        "layer_count": spec.rules.layer_count,
        "path": ctx.pcb_path().display().to_string(),
        "note": "board regenerated from the committed schematic file (not F8 sync; existing placement/routing may be replaced) — run place_board, then route_board, then check_board",
    }))
}

fn apply_complexity_default_layer_count(
    rules: &mut SeedRules,
    input: Option<&Value>,
    part_count: usize,
) {
    let explicitly_selected = input
        .and_then(Value::as_object)
        .is_some_and(|rules| rules.contains_key("layer_count"));
    if part_count >= 40 && !explicitly_selected {
        rules.layer_count = 4;
    }
}

fn unapplied_draft_footprint_changes(
    ctx: &AgentRuntime,
    netlist: &kicad_cli::Netlist,
) -> anyhow::Result<Vec<Value>> {
    let Some(draft) = ctx.workspace().read_draft()? else {
        return Ok(Vec::new());
    };
    let Some(design) = circuit_lang::compile(&draft, ctx.provider()).design else {
        return Ok(Vec::new());
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
    Ok(changes)
}

fn write_seed_board(spec: &BoardSeedSpec, ctx: &AgentRuntime) -> std::result::Result<(), String> {
    let catalog = ctx
        .footprint_catalog()
        .map_err(|e| format!("footprint catalog unavailable: {e}"))?;
    let text = emit_seed_board(spec, catalog)?;
    // Regeneration replaces the document, not merely its on-disk bytes. A
    // cached pcbnew session otherwise keeps serving the old in-memory board for
    // the same pathname, so the next tool sees stale bounds and footprints.
    // Close before overwrite to prevent that process from later saving stale
    // state back over the fresh seed.
    ctx.close_kicad_session();
    std::fs::write(ctx.pcb_path(), text)
        .map_err(|e| format!("could not write {}: {e}", ctx.pcb_path().display()))
}

/// Synthesize the production seed-board representation without writing it.
///
/// Keeping the emitter independent of [`AgentRuntime`] lets offline validation
/// compose the exact same seed writer with the production placement/copper
/// patchers.
pub(super) fn emit_seed_board(
    spec: &BoardSeedSpec,
    catalog: &FootprintCatalog,
) -> std::result::Result<String, String> {
    let mut parts = Vec::with_capacity(spec.parts.len());
    let mut x = spec.bounds.min_x + 2.0;
    let y = spec.bounds.min_y + 2.0;
    for dp in &spec.parts {
        let id = FootprintId::parse(&dp.footprint).map_err(|e| {
            let clause = footprint_suggestion_clause(&catalog.suggest_text(&dp.footprint));
            format!(
                "part {}: invalid footprint id `{}`: {e}{clause}",
                dp.reference, dp.footprint
            )
        })?;
        let source = catalog.source(&id).map_err(|e| {
            if e.is_not_found() {
                let clause = footprint_suggestion_clause(&catalog.suggest(&id));
                format!(
                    "part {}: unknown footprint `{}`{clause} — assign a real lib_id via search_footprints",
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
            value: dp.value.clone(),
            lib_id: dp.footprint.clone(),
            source,
            pad_nets: dp.pad_nets.clone(),
            at: dp.locked.as_ref().map(|l| l.at).unwrap_or(Point2 { x, y }),
            rotation: dp.locked.as_ref().map(|l| l.rotation).unwrap_or(0.0),
            locked: dp.locked.is_some(),
        });
        x += 2.54;
    }
    let effective_rules = effective_seed_rules(&spec.rules, &parts);
    let text = SeedBoardWriter::new(
        &parts,
        &spec.bounds,
        &effective_rules,
        spec.outline.as_ref(),
    )
    .emit()
    .map_err(|e| format!("board synthesis failed: {e}"))?;
    Ok(text)
}

/// KiCad applies a footprint's direct `(clearance ...)` override in addition to its board
/// netclass.  The router only sees the board/netclass clearance through IPC, so seed both with
/// the strictest value present in the canonical footprint sources.  This keeps router-clean
/// copper clean under KiCad DRC without rewriting the library footprint text.
fn effective_seed_rules(requested: &SeedRules, parts: &[SeedFootprint]) -> SeedRules {
    let mut effective = requested.clone();
    for override_clearance in parts
        .iter()
        .filter_map(|part| footprint_clearance_override(&part.source))
    {
        effective.clearance = effective.clearance.max(override_clearance);
    }
    effective
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
            pad_connection: PourPadConnection::Thermal,
        });
        rules.pours.push(PourSpec {
            net: "GND".to_string(),
            layer: format!("inner{}", rules.layer_count - 2),
            pad_connection: PourPadConnection::Thermal,
        });
    }
    if nets.contains("V3V3") {
        rules.pours.push(PourSpec {
            net: "V3V3".to_string(),
            layer: "inner1".to_string(),
            pad_connection: PourPadConnection::Thermal,
        });
    }
}

#[derive(Debug, Clone)]
struct SeedFootprint {
    reference: String,
    value: Option<String>,
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
        let canonical = net.strip_prefix('/').unwrap_or(&net);
        let width = rules
            .net_widths
            .get(&net)
            .or_else(|| rules.net_widths.get(canonical))
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
        self.push_opto_isolation_corridor(&mut out);
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

    /// A dense 817 input bank is an isolation boundary, not merely a repeated component row.
    /// Preserve that boundary in the native board file so KiCad refill, interactive routing,
    /// and the IPC router all see the same copper-free corridor.  Pads and footprints remain
    /// allowed because each optocoupler intentionally bridges the rule area.
    fn push_opto_isolation_corridor(&self, out: &mut String) {
        if self.parts.iter().filter(|part| is_817_family(part)).count() < 8 {
            return;
        }

        const HALF_WIDTH: f64 = 1.0;
        let x0 = fmt_num(self.bounds.min_x);
        let x1 = fmt_num(self.bounds.max_x);
        let center_y = (self.bounds.min_y + self.bounds.max_y) / 2.0;
        let y0 = fmt_num(center_y - HALF_WIDTH);
        let y1 = fmt_num(center_y + HALF_WIDTH);
        let uuid = seed_uuid(&format!("opto-isolation:{x0}:{y0}:{x1}:{y1}"));
        let _ = write!(
            out,
            "\t(zone\n\
             \t\t(net 0)\n\
             \t\t(net_name \"\")\n\
             \t\t(layers \"*.Cu\")\n\
             \t\t(uuid \"{uuid}\")\n\
             \t\t(name \"OPTO_ISOLATION_CORRIDOR\")\n\
             \t\t(hatch edge 0.5)\n\
             \t\t(connect_pads\n\
             \t\t\t(clearance 0)\n\
             \t\t)\n\
             \t\t(min_thickness 0.25)\n\
             \t\t(filled_areas_thickness no)\n\
             \t\t(keepout\n\
             \t\t\t(tracks not_allowed)\n\
             \t\t\t(vias not_allowed)\n\
             \t\t\t(pads allowed)\n\
             \t\t\t(copperpour not_allowed)\n\
             \t\t\t(footprints allowed)\n\
             \t\t)\n\
             \t\t(fill\n\
             \t\t\t(thermal_gap 0.3)\n\
             \t\t\t(thermal_bridge_width 0.3)\n\
             \t\t)\n\
             \t\t(polygon\n\
             \t\t\t(pts\n\
             \t\t\t\t(xy {x0} {y0}) (xy {x1} {y0}) (xy {x1} {y1}) (xy {x0} {y1})\n\
             \t\t\t)\n\
             \t\t)\n\
             \t)\n"
        );
    }

    fn push_zones(&self, out: &mut String) -> io::Result<()> {
        let mut explicit_by_layer = BTreeMap::<u32, (i32, String, String, bool)>::new();
        for (idx, pour) in self.rules.pours.iter().enumerate() {
            let resolved = self.net_codes.get_key_value(&pour.net).or_else(|| {
                let hierarchical = format!("/{}", pour.net.trim_start_matches('/'));
                self.net_codes.get_key_value(&hierarchical)
            });
            let (net_name, net_code) = resolved.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "rules.pours[{idx}] net {:?} is not present on any pad",
                        pour.net
                    ),
                )
            })?;
            let Some((layer_idx, layer_name)) =
                resolve_pour_layer(&pour.layer, self.rules.layer_count)
            else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "rules.pours[{idx}] layer {:?} is invalid for {} layers",
                        pour.layer, self.rules.layer_count
                    ),
                ));
            };
            let solid = pour.pad_connection == PourPadConnection::Solid;
            if let Some((existing_code, existing_net, _, existing_solid)) =
                explicit_by_layer.get(&layer_idx)
            {
                if existing_code == net_code && *existing_solid == solid {
                    continue;
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "rules.pours assigns incompatible full-board zones {existing_net:?} and {net_name:?} to {layer_name}"
                    ),
                ));
            }
            explicit_by_layer.insert(layer_idx, (*net_code, net_name.clone(), layer_name, solid));
        }
        for (layer_idx, (net_code, net, layer_name, solid)) in &explicit_by_layer {
            self.write_zone(
                out,
                *net_code,
                net,
                layer_name,
                &format!("explicit:{layer_idx}"),
                *solid,
            );
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
        for (net, layer_idx) in pcb_model::default_plane_nets_excluding(
            self.rules.layer_count,
            pad_counts.into_iter(),
            explicit_by_layer.keys().copied(),
        ) {
            let Some(net_code) = self.net_codes.get(&net).copied() else {
                continue;
            };
            let layer_name = format!("In{layer_idx}.Cu");
            self.write_zone(out, net_code, &net, &layer_name, "plane", false);
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
        solid_pad_connections: bool,
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
        let connect_pads = if solid_pad_connections {
            "connect_pads yes"
        } else {
            "connect_pads"
        };
        let _ = write!(
            out,
            "\t(zone\n\
             \t\t(net {net_code})\n\
             \t\t(net_name \"{net}\")\n\
             \t\t(layer \"{layer_name}\")\n\
             \t\t(uuid \"{uuid}\")\n\
             \t\t(name \"{net}\")\n\
             \t\t(hatch full 0.508)\n\
             \t\t({connect_pads}\n\
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

fn is_817_family(part: &SeedFootprint) -> bool {
    if !part.reference.starts_with('U') {
        return false;
    }
    let Some(value) = part.value.as_deref() else {
        return false;
    };
    let identity = value.rsplit(':').next().unwrap_or(value);
    let compact: String = identity
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_uppercase)
        .collect();
    compact.starts_with("PC817") || compact.starts_with("LTV817")
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
    if let Some(function) = connector_function_property(inner, part) {
        push_reindented(&mut out, &function);
    }
    out.push_str("\t)\n");
    Ok(out)
}

/// Add a concise functional label to connector silkscreen without changing the
/// canonical Value field used by KiCad/BOM tooling.  The library's Value field
/// already occupies a deliberate text location outside the footprint body, so
/// reusing its geometry avoids inventing an unbounded board-space coordinate and
/// keeps the label attached when placement rotates or moves the connector.
fn connector_function_property(inner: &str, part: &SeedFootprint) -> Option<String> {
    // Tiny two/three-pin headers are commonly tucked beside support passives;
    // their library value position is not a dependable free silk lane.  Larger
    // interface headers are edge-biased by placement and have room for a legend.
    if !part.reference.starts_with('J') || part.pad_nets.len() < 4 {
        return None;
    }
    let summary = connector_net_summary(&part.pad_nets)?;
    let value = top_level_nodes(inner)
        .into_iter()
        .find(|node| node.starts_with("(property \"Value\""))?;
    if !value.contains("(layer \"F.Fab\")") && !value.contains("(layer \"F.SilkS\")") {
        return None;
    }

    let mut property = replace_property_name_and_value(value, "Function", &summary)?;
    property = property.replace("(layer \"F.Fab\")", "(layer \"F.SilkS\")");
    property = property.replace("(hide yes)", "");
    Some(cap_font_size(&property, REF_TEXT_SIZE_MM))
}

fn replace_property_name_and_value(node: &str, name: &str, value: &str) -> Option<String> {
    let rest = node.strip_prefix("(property \"")?;
    let name_close = rest.find('"')?;
    let rest = rest[name_close + 1..].strip_prefix(" \"")?;
    let mut escaped = false;
    let mut value_close = None;
    for (idx, ch) in rest.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            value_close = Some(idx);
            break;
        }
    }
    let value_close = value_close?;
    Some(format!(
        "(property \"{}\" \"{}{}",
        sexpr_escape(name),
        sexpr_escape(value),
        &rest[value_close..]
    ))
}

/// Collapse common numbered connector buses (`IN1` ... `IN8`) while retaining
/// named power/domain nets.  Labels that cannot be made compact are omitted;
/// an overlong silk legend is worse than the reference-only baseline.
fn connector_net_summary(pad_nets: &BTreeMap<String, String>) -> Option<String> {
    let mut nets: Vec<String> = pad_nets
        .values()
        .map(|net| net.trim_start_matches('/').to_owned())
        .filter(|net| !net.is_empty() && !net.starts_with("Net-("))
        .collect();
    nets.sort();
    nets.dedup();
    if nets.is_empty() {
        return None;
    }

    let mut numbered = BTreeMap::<String, Vec<u32>>::new();
    let mut plain = Vec::new();
    for net in nets {
        if let Some((prefix, number)) = split_numeric_suffix(&net) {
            numbered.entry(prefix.to_owned()).or_default().push(number);
        } else {
            plain.push(net);
        }
    }
    for numbers in numbered.values_mut() {
        numbers.sort_unstable();
        numbers.dedup();
    }

    let mut labels = plain;
    for (prefix, numbers) in numbered {
        let consecutive = numbers
            .windows(2)
            .all(|pair| pair[1] == pair[0].saturating_add(1));
        if numbers.len() >= 3 && consecutive {
            labels.push(format!(
                "{prefix}{}..{}",
                numbers[0],
                numbers[numbers.len() - 1]
            ));
        } else {
            labels.extend(
                numbers
                    .into_iter()
                    .map(|number| format!("{prefix}{number}")),
            );
        }
    }
    labels.sort();
    let summary = labels.join("/");
    (!summary.is_empty() && summary.chars().count() <= 28).then_some(summary)
}

fn split_numeric_suffix(net: &str) -> Option<(&str, u32)> {
    let suffix_start = net
        .char_indices()
        .rev()
        .take_while(|(_, ch)| ch.is_ascii_digit())
        .map(|(idx, _)| idx)
        .last()?;
    let (prefix, suffix) = net.split_at(suffix_start);
    (!prefix.is_empty())
        .then(|| suffix.parse::<u32>().ok().map(|number| (prefix, number)))
        .flatten()
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
        "property" => Ok(Some(transform_seed_property(
            node,
            &part.reference,
            part.value.as_deref(),
        ))),
        "pad" => Ok(Some(transform_seed_pad(node, part, net_codes, fp_rot)?)),
        _ => Ok(Some(node.to_owned())),
    }
}

const REF_TEXT_SIZE_MM: f64 = 0.8;

fn transform_seed_property(node: &str, reference: &str, value: Option<&str>) -> String {
    if let Some(rest) = node.strip_prefix("(property \"Reference\" \"")
        && let Some(close) = rest.find('"')
    {
        let body = cap_font_size(&rest[close + 1..], REF_TEXT_SIZE_MM);
        return format!("(property \"Reference\" \"{reference}\"{body}");
    }
    if node.starts_with("(property \"Value\"") {
        let mut transformed = value
            .and_then(|value| replace_property_value(node, value))
            .unwrap_or_else(|| node.to_owned());
        if !transformed.contains("(hide yes)")
            && let Some(hidden) = inject_before_close(&transformed, "(hide yes)")
        {
            transformed = hidden;
        }
        return transformed;
    }
    node.to_owned()
}

fn replace_property_value(node: &str, value: &str) -> Option<String> {
    let prefix = "(property \"Value\" \"";
    let rest = node.strip_prefix(prefix)?;
    let mut escaped = false;
    let mut close = None;
    for (idx, ch) in rest.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            close = Some(idx);
            break;
        }
    }
    let close = close?;
    Some(format!("{prefix}{}{}", sexpr_escape(value), &rest[close..]))
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

fn footprint_clearance_override(source: &str) -> Option<f64> {
    let body = footprint_body(source)?;
    let inner = footprint_inner(body)?;
    top_level_nodes(inner)
        .into_iter()
        .filter(|node| node_head(node) == "clearance")
        .filter_map(|node| {
            let value = node
                .strip_prefix("(clearance")?
                .strip_suffix(')')?
                .trim()
                .parse::<f64>()
                .ok()?;
            (value.is_finite() && value >= 0.0).then_some(value)
        })
        .max_by(f64::total_cmp)
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
        let (x, y) = (
            get(&["x", "min_x", "minX"]).unwrap_or(0.0),
            get(&["y", "min_y", "minY"]).unwrap_or(0.0),
        );
        return Ok(rect(x, y, x + w, y + h));
    }
    Err(format!(
        "bounds: could not read a rect from {val}; {EXPECT}"
    ))
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
    let mut pours_by_layer = BTreeMap::<u32, (String, PourPadConnection)>::new();
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
            let pad_connection = match p.get("connect").and_then(Value::as_str) {
                None | Some("thermal") => PourPadConnection::Thermal,
                Some("solid") => PourPadConnection::Solid,
                Some(other) => {
                    return Err(format!(
                        "rules.pours[].connect must be 'thermal' or 'solid', got '{other}'"
                    ));
                }
            };
            // A pour floods the requested copper layer. Reserved inner plane indices
            // are valid explicit overrides of the automatic GND/supply assignment;
            // the emitter suppresses the competing default on that physical layer.
            match resolve_pour_layer(layer, layer_count) {
                None => {
                    return Err(format!(
                        "rules.pours[].layer '{layer}' is not a valid copper layer on a \
                         {layer_count}-layer board — use top/bottom, innerN, or F.Cu/B.Cu/InN.Cu"
                    ));
                }
                Some((idx, _)) => {
                    if let Some((existing_net, existing_connection)) = pours_by_layer.get(&idx) {
                        if existing_net == net && *existing_connection == pad_connection {
                            // Duplicate requests describe the same physical
                            // full-board zone; emit it once.
                            continue;
                        }
                        if existing_net == net {
                            return Err(format!(
                                "rules.pours assigns conflicting pad connections to net '{net}' on layer '{layer}'; use one connect policy per net/layer"
                            ));
                        }
                        return Err(format!(
                            "rules.pours assigns both '{existing_net}' and '{net}' to layer '{layer}'; a copper layer can have only one full-board pour net"
                        ));
                    }
                    pours_by_layer.insert(idx, (net.to_string(), pad_connection));
                }
            }
            pours.push(PourSpec {
                net: net.to_string(),
                layer: layer.to_string(),
                pad_connection,
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
    fn dense_board_implicit_stackup_defaults_to_four_layers() {
        for (part_count, input, expected) in [
            (39, None, 2),
            (40, None, 4),
            (40, Some(json!(null)), 4),
            (40, Some(json!({})), 4),
            (40, Some(json!({ "clearance": 0.2 })), 4),
        ] {
            let mut rules = parse_seed_rules(input.as_ref()).unwrap();
            apply_complexity_default_layer_count(&mut rules, input.as_ref(), part_count);
            assert_eq!(
                rules.layer_count, expected,
                "parts={part_count} input={input:?}"
            );
        }
    }

    #[test]
    fn explicit_stackup_is_honored_at_every_supported_layer_count() {
        for layer_count in [2, 4, 6, 8] {
            let input = json!({ "layer_count": layer_count });
            let mut rules = parse_seed_rules(Some(&input)).unwrap();

            apply_complexity_default_layer_count(&mut rules, Some(&input), 40);

            assert_eq!(rules.layer_count, layer_count);
        }
    }

    #[test]
    fn seed_net_classes_match_hierarchical_net_names_to_requested_widths() {
        let rules = parse_seed_rules(Some(&json!({
            "net_widths": { "V3V3": 0.5, "GND": 0.6 }
        })))
        .unwrap();

        let classes = seed_net_classes(&rules, ["/V3V3".to_string(), "GND".to_string()]);

        assert!(classes.iter().any(|class| {
            class.trace_width == 0.5 && class.members == vec!["/V3V3".to_string()]
        }));
        assert!(
            classes.iter().any(|class| {
                class.trace_width == 0.6 && class.members == vec!["GND".to_string()]
            })
        );
    }

    #[test]
    fn parse_seed_rules_keeps_requested_pours() {
        let rules = parse_seed_rules(Some(&json!({
            "layer_count": 6,
            "pours": [
                { "net": "GND", "layer": "bottom", "connect": "solid" },
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
                    pad_connection: PourPadConnection::Solid,
                },
                PourSpec {
                    net: "V3V3".to_string(),
                    layer: "inner1".to_string(),
                    pad_connection: PourPadConnection::Thermal,
                },
            ]
        );
    }

    #[test]
    fn parse_seed_rules_keeps_explicit_reserved_plane_overrides() {
        let rules = parse_seed_rules(Some(&json!({
            "layer_count": 4,
            "pours": [
                { "net": "GND", "layer": "inner1" },
                { "net": "GND", "layer": "inner2" }
            ]
        })))
        .unwrap();

        assert_eq!(rules.pours.len(), 2);
        assert_eq!(rules.pours[0].layer, "inner1");
        assert_eq!(rules.pours[1].layer, "inner2");
    }

    #[test]
    fn parse_seed_rules_accepts_kicad_inner_layer_names() {
        let rules = parse_seed_rules(Some(&json!({
            "layer_count": 4,
            "pours": [
                { "net": "FIELD_GND", "layer": "In1.Cu" },
                { "net": "+5V", "layer": "Inner2.Cu" }
            ]
        })))
        .unwrap();

        assert_eq!(
            resolve_pour_layer(&rules.pours[0].layer, 4).unwrap().1,
            "In1.Cu"
        );
        assert_eq!(
            resolve_pour_layer(&rules.pours[1].layer, 4).unwrap().1,
            "In2.Cu"
        );
    }

    #[test]
    fn parse_seed_rules_rejects_competing_full_board_pours() {
        let err = parse_seed_rules(Some(&json!({
            "layer_count": 2,
            "pours": [
                { "net": "GND", "layer": "top" },
                { "net": "3V3", "layer": "top" }
            ]
        })))
        .unwrap_err();

        assert!(err.contains("both 'GND' and '3V3'"), "{err}");
        assert!(err.contains("only one full-board pour net"), "{err}");
    }

    #[test]
    fn parse_seed_rules_deduplicates_identical_pours() {
        let rules = parse_seed_rules(Some(&json!({
            "layer_count": 2,
            "pours": [
                { "net": "GND", "layer": "bottom" },
                { "net": "GND", "layer": "bottom" }
            ]
        })))
        .unwrap();

        assert_eq!(rules.pours.len(), 1);
    }

    #[test]
    fn parse_seed_rules_rejects_conflicting_duplicate_pour_connections() {
        let err = parse_seed_rules(Some(&json!({
            "layer_count": 2,
            "pours": [
                { "net": "GND", "layer": "bottom", "connect": "thermal" },
                { "net": "GND", "layer": "bottom", "connect": "solid" }
            ]
        })))
        .unwrap_err();

        assert!(err.contains("conflicting pad connections"), "{err}");
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
            value: None,
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
            pad_connection: PourPadConnection::Solid,
        });

        let board = SeedBoardWriter::new(&parts, &bounds, &rules, None)
            .emit()
            .unwrap();

        assert!(board.contains("\n\t(zone\n"));
        assert!(board.contains("\n\t\t(net_name \"GND\")\n"));
        assert!(board.contains("\n\t\t(layer \"B.Cu\")\n"));
        assert!(board.contains("\n\t\t(connect_pads yes\n"));
        assert!(board.contains("(xy 0 0) (xy 20 0) (xy 20 10) (xy 0 10)"));
    }

    fn clearance_fixture(source: &str) -> SeedFootprint {
        SeedFootprint {
            reference: "U1".to_string(),
            value: Some("Fixture".to_string()),
            lib_id: "Test:Fixture".to_string(),
            source: source.to_string(),
            pad_nets: BTreeMap::from([("1".to_string(), "SIG".to_string())]),
            at: Point2 { x: 5.0, y: 5.0 },
            rotation: 0.0,
            locked: false,
        }
    }

    #[test]
    fn footprint_clearance_raises_board_netclass_and_router_rule() {
        let part = clearance_fixture(
            "(footprint \"Fixture\"\n\
             \t(clearance 0.2)\n\
             \t(pad \"1\" smd circle (at 0 0) (size 1 1) (layers \"F.Cu\")\n\
             \t\t(clearance 0.4)\n\
             \t)\n\
             )",
        );
        let effective = effective_seed_rules(&SeedRules::default(), std::slice::from_ref(&part));
        let classes = seed_net_classes(&effective, ["SIG".to_string()]);

        assert_eq!(effective.clearance, 0.2);
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "Default");
        assert_eq!(classes[0].clearance, 0.2);

        let board =
            SeedBoardWriter::new(&[part], &Rect::new(0.0, 0.0, 20.0, 10.0), &effective, None)
                .emit()
                .unwrap();
        assert!(board.contains("(net_class \"Default\""));
        // Library overrides remain present; only the board/router rule was raised.
        assert!(board.contains("\n\t\t(clearance 0.2)\n"));
        assert!(board.contains("\n\t(clearance 0.2)\n"));
        assert!(board.contains("\n\t\t\t(clearance 0.4)\n"));
    }

    #[test]
    fn explicit_stricter_clearance_is_never_lowered() {
        let part = clearance_fixture(
            "(footprint \"Fixture\"\n\
             \t(clearance 0.2)\n\
             \t(pad \"1\" smd circle (at 0 0) (size 1 1) (layers \"F.Cu\"))\n\
             )",
        );
        let requested = SeedRules {
            clearance: 0.25,
            ..SeedRules::default()
        };

        let effective = effective_seed_rules(&requested, &[part]);

        assert_eq!(effective.clearance, 0.25);
    }

    #[test]
    fn nested_pad_and_zone_clearances_are_not_footprint_overrides() {
        let part = clearance_fixture(
            "(footprint \"Fixture\"\n\
             \t(pad \"1\" smd circle (at 0 0) (size 1 1) (layers \"F.Cu\")\n\
             \t\t(clearance 0.4)\n\
             \t)\n\
             \t(zone (net 0) (connect_pads (clearance 0.5)))\n\
             )",
        );

        assert_eq!(footprint_clearance_override(&part.source), None);
        assert_eq!(
            effective_seed_rules(&SeedRules::default(), &[part]).clearance,
            0.15
        );
    }

    #[test]
    fn seed_writer_emits_all_copper_corridor_for_dense_817_bank() {
        let source =
            "(footprint \"SO4\" (pad \"1\" smd circle (at 0 0) (size 1 1) (layers \"F.Cu\")))";
        let parts: Vec<_> = (1..=8)
            .map(|index| SeedFootprint {
                reference: format!("U{index}"),
                value: Some(if index % 2 == 0 { "LTV-817" } else { "PC817C" }.to_string()),
                lib_id: "Package_SO:SO-4".to_string(),
                source: source.to_string(),
                pad_nets: BTreeMap::new(),
                at: Point2 { x: 0.0, y: 0.0 },
                rotation: 0.0,
                locked: false,
            })
            .collect();
        let bounds = Rect::new(0.0, 0.0, 90.0, 58.0);

        let board = SeedBoardWriter::new(&parts, &bounds, &SeedRules::default(), None)
            .emit()
            .unwrap();

        assert_eq!(
            board.matches("(name \"OPTO_ISOLATION_CORRIDOR\")").count(),
            1
        );
        assert!(board.contains("\n\t\t(layers \"*.Cu\")\n"));
        assert!(board.contains("\n\t\t\t(tracks not_allowed)\n"));
        assert!(board.contains("\n\t\t\t(vias not_allowed)\n"));
        assert!(board.contains("\n\t\t\t(pads allowed)\n"));
        assert!(board.contains("\n\t\t\t(copperpour not_allowed)\n"));
        assert!(board.contains("\n\t\t\t(footprints allowed)\n"));
        assert!(board.contains("(xy 0 28) (xy 90 28) (xy 90 30) (xy 0 30)"));
    }

    #[test]
    fn seed_writer_leaves_smaller_or_unrelated_banks_unchanged() {
        let source =
            "(footprint \"SO4\" (pad \"1\" smd circle (at 0 0) (size 1 1) (layers \"F.Cu\")))";
        let parts: Vec<_> = (1..=8)
            .map(|index| SeedFootprint {
                reference: format!("U{index}"),
                value: Some(if index == 8 { "PC818" } else { "PC817" }.to_string()),
                lib_id: "Package_SO:SO-4".to_string(),
                source: source.to_string(),
                pad_nets: BTreeMap::new(),
                at: Point2 { x: 0.0, y: 0.0 },
                rotation: 0.0,
                locked: false,
            })
            .collect();
        let bounds = Rect::new(0.0, 0.0, 90.0, 58.0);

        let board = SeedBoardWriter::new(&parts, &bounds, &SeedRules::default(), None)
            .emit()
            .unwrap();

        assert!(!board.contains("OPTO_ISOLATION_CORRIDOR"));
        assert!(!board.contains("(layers \"*.Cu\")"));
    }

    #[test]
    fn explicit_plane_pours_replace_defaults_without_duplicate_zones() {
        let parts = vec![SeedFootprint {
            reference: "U1".to_string(),
            value: None,
            lib_id: "Test:TwoPad".to_string(),
            source: "(footprint \"TwoPad\" \
                (pad \"1\" smd circle (at -1 0) (size 1 1) (layers \"F.Cu\")) \
                (pad \"2\" smd circle (at 1 0) (size 1 1) (layers \"F.Cu\")))"
                .to_string(),
            pad_nets: BTreeMap::from([
                ("1".to_string(), "GND".to_string()),
                ("2".to_string(), "V3V3".to_string()),
            ]),
            at: Point2 { x: 5.0, y: 5.0 },
            rotation: 0.0,
            locked: false,
        }];
        let bounds = Rect::new(0.0, 0.0, 20.0, 10.0);
        let rules = SeedRules {
            layer_count: 4,
            pours: vec![
                PourSpec {
                    net: "GND".to_string(),
                    layer: "inner1".to_string(),
                    pad_connection: PourPadConnection::Thermal,
                },
                PourSpec {
                    net: "GND".to_string(),
                    layer: "inner2".to_string(),
                    pad_connection: PourPadConnection::Thermal,
                },
            ],
            ..SeedRules::default()
        };

        let board = SeedBoardWriter::new(&parts, &bounds, &rules, None)
            .emit()
            .unwrap();

        assert_eq!(board.matches("\n\t(zone\n").count(), 2, "{board}");
        assert_eq!(board.matches("\n\t\t(net_name \"GND\")\n").count(), 2);
        assert_eq!(board.matches("\n\t\t(layer \"In1.Cu\")\n").count(), 1);
        assert_eq!(board.matches("\n\t\t(layer \"In2.Cu\")\n").count(), 1);
        assert!(!board.contains("\n\t\t(net_name \"V3V3\")\n"));
    }

    #[test]
    fn seed_writer_emits_committed_values_for_default_passive_and_ic() {
        let temp = tempfile::tempdir().unwrap();
        let library = temp.path().join("Test.pretty");
        std::fs::create_dir(&library).unwrap();
        std::fs::write(
            library.join("Fixture.kicad_mod"),
            "(footprint \"Fixture\"\n\
                 \t(property \"Reference\" \"REF**\"\n\
                 \t\t(at 0 -2 0)\n\
                 \t\t(layer \"F.SilkS\")\n\
                 \t\t(effects (font (size 1 1) (thickness 0.15)))\n\
                 \t)\n\
                 \t(property \"Value\" \"Fixture\"\n\
                 \t\t(at 0 2 0)\n\
                 \t\t(layer \"F.Fab\")\n\
                 \t\t(effects (font (size 1 1) (thickness 0.15)))\n\
                 \t)\n\
                 \t(pad \"1\" smd circle (at 0 0) (size 1 1) (layers \"F.Cu\"))\n\
                 )",
        )
        .unwrap();
        let catalog = FootprintCatalog::from_root(temp.path()).unwrap();
        let part = |reference: &str, value: &str| SeedPart {
            reference: reference.to_string(),
            value: Some(value.to_string()),
            footprint: "Test:Fixture".to_string(),
            pad_nets: BTreeMap::new(),
            locked: None,
        };
        let spec = BoardSeedSpec {
            bounds: Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 20.0,
                max_y: 10.0,
            },
            rules: SeedRules::default(),
            parts: vec![part("R1", "R"), part("R2", "10k"), part("U1", "NE555P")],
            outline: None,
        };
        let board = emit_seed_board(&spec, &catalog).unwrap();

        assert!(board.contains("(property \"Value\" \"R\""));
        assert!(board.contains("(property \"Value\" \"10k\""));
        assert!(board.contains("(property \"Value\" \"NE555P\""));
        assert!(!board.contains("(property \"Value\" \"Fixture\""));
        assert_eq!(board.matches("(hide yes)").count(), 3);
    }

    #[test]
    fn connector_net_summary_compacts_numbered_buses_and_keeps_domains() {
        let mut nets = BTreeMap::from([
            ("1".to_string(), "/IN1".to_string()),
            ("9".to_string(), "FIELD_GND".to_string()),
        ]);
        for pin in 2..=8 {
            nets.insert(pin.to_string(), format!("IN{pin}"));
        }

        assert_eq!(
            connector_net_summary(&nets).as_deref(),
            Some("FIELD_GND/IN1..8")
        );
        assert_eq!(split_numeric_suffix("OUT12"), Some(("OUT", 12)));
        assert_eq!(split_numeric_suffix("V5"), Some(("V", 5)));
        assert_eq!(split_numeric_suffix("GND"), None);

        nets.insert("1".to_string(), "Net-(J1-Pin_1)".to_string());
        assert_eq!(
            connector_net_summary(&nets).as_deref(),
            Some("FIELD_GND/IN2..8")
        );
    }

    #[test]
    fn seed_writer_adds_function_silk_to_connectors_without_changing_value() {
        let source = "(footprint \"Header\"\n\
            \t(property \"Reference\" \"REF**\"\n\
            \t\t(at 0 -2 0)\n\
            \t\t(layer \"F.SilkS\")\n\
            \t\t(effects (font (size 1 1) (thickness 0.15)))\n\
            \t)\n\
            \t(property \"Value\" \"Header\"\n\
            \t\t(at 0 4 0)\n\
            \t\t(layer \"F.Fab\")\n\
            \t\t(effects (font (size 1 1) (thickness 0.15)))\n\
            \t)\n\
            \t(pad \"1\" thru_hole circle (at 0 0) (size 1 1) (layers \"*.Cu\"))\n\
            \t(pad \"2\" thru_hole circle (at 0 2.54) (size 1 1) (layers \"*.Cu\"))\n\
            )";
        let part = SeedFootprint {
            reference: "J1".to_string(),
            value: Some("Conn_01x02".to_string()),
            lib_id: "Test:Header".to_string(),
            source: source.to_string(),
            pad_nets: BTreeMap::from([
                ("1".to_string(), "CAN_H".to_string()),
                ("2".to_string(), "CAN_L".to_string()),
                ("3".to_string(), "CAN_H".to_string()),
                ("4".to_string(), "CAN_L".to_string()),
            ]),
            at: Point2 { x: 5.0, y: 5.0 },
            rotation: 90.0,
            locked: false,
        };
        let board = SeedBoardWriter::new(
            &[part],
            &Rect::new(0.0, 0.0, 20.0, 10.0),
            &SeedRules::default(),
            None,
        )
        .emit()
        .unwrap();

        assert!(board.contains("(property \"Value\" \"Conn_01x02\""));
        assert!(board.contains("(property \"Function\" \"CAN_H/CAN_L\""));
        assert!(board.contains("(property \"Function\" \"CAN_H/CAN_L\"\n\t\t\t(at 0 4 0)"));
        assert_eq!(
            board
                .matches("(property \"Function\" \"CAN_H/CAN_L\"")
                .count(),
            1
        );
        let function = board
            .split("(property \"Function\"")
            .nth(1)
            .expect("function property");
        assert!(function.contains("(layer \"F.SilkS\")"));
    }

    #[test]
    fn connector_function_silk_omits_non_connectors_and_overlong_legends() {
        let inner = "\n\
            (property \"Value\" \"Header\"\n\
            \t(at 0 4 0)\n\
            \t(layer \"F.Fab\")\n\
            )\n";
        let mut part = SeedFootprint {
            reference: "U1".to_string(),
            value: Some("IC".to_string()),
            lib_id: "Test:Header".to_string(),
            source: format!("(footprint \"Header\"{inner})"),
            pad_nets: BTreeMap::from([
                ("1".to_string(), "A".to_string()),
                ("2".to_string(), "B".to_string()),
            ]),
            at: Point2 { x: 0.0, y: 0.0 },
            rotation: 0.0,
            locked: false,
        };
        assert!(connector_function_property(inner, &part).is_none());

        part.reference = "J1".to_string();
        part.pad_nets = BTreeMap::from([
            ("1".to_string(), "UNABBREVIATED_SIGNAL_ALPHA".to_string()),
            ("2".to_string(), "UNABBREVIATED_SIGNAL_BETA".to_string()),
            ("3".to_string(), "UNABBREVIATED_SIGNAL_ALPHA".to_string()),
            ("4".to_string(), "UNABBREVIATED_SIGNAL_BETA".to_string()),
        ]);
        assert!(connector_function_property(inner, &part).is_none());
    }

    #[test]
    fn seed_property_value_escapes_schematic_text() {
        let node = "(property \"Value\" \"library\" (at 0 0 0))";
        let transformed = transform_seed_property(node, "U1", Some("MPN \\\"A\\\" \\\\ rev"));

        assert!(transformed.contains("(property \"Value\" \"MPN \\\\\\\"A\\\\\\\" \\\\\\\\ rev\""));
        assert!(transformed.contains("(hide yes)"));
    }

    #[test]
    fn default_power_pours_are_added_on_dense_stackups() {
        let mut rules = SeedRules {
            layer_count: 6,
            ..SeedRules::default()
        };
        let parts = vec![SeedPart {
            reference: "U1".to_string(),
            value: None,
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
                    pad_connection: PourPadConnection::Thermal,
                },
                PourSpec {
                    net: "GND".to_string(),
                    layer: "inner4".to_string(),
                    pad_connection: PourPadConnection::Thermal,
                },
                PourSpec {
                    net: "V3V3".to_string(),
                    layer: "inner1".to_string(),
                    pad_connection: PourPadConnection::Thermal,
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

    #[test]
    fn seed_replacement_invalidates_same_runtime_live_session() {
        let Some(ctx) = AgentRuntime::detect_for_test() else {
            eprintln!("SKIP: KiCad is not installed");
            return;
        };
        let spec = |width, height| BoardSeedSpec {
            bounds: Rect::new(0.0, 0.0, width, height),
            rules: SeedRules::default(),
            parts: vec![],
            outline: None,
        };

        write_seed_board(&spec(20.0, 10.0), &ctx).unwrap();
        let first = super::super::active::board_problem(&ctx).unwrap();
        assert_eq!(first.imported.bounds, Rect::new(0.0, 0.0, 20.0, 10.0));

        // `first` opened and cached a pcbnew session. Replacing the same path
        // must force the next snapshot to open the new document, not reuse it.
        write_seed_board(&spec(40.0, 30.0), &ctx).unwrap();
        let second = super::super::active::board_problem(&ctx).unwrap();
        assert_eq!(second.imported.bounds, Rect::new(0.0, 0.0, 40.0, 30.0));
        ctx.close_kicad_session();
    }
}
