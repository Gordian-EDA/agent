//! Board-construction tools and shared input parsers.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;

use anyhow::Result;
use kicad_cli::KicadCli;
use serde_json::{Value, json};

use kicad_footprint::FootprintId;
use pcb_model::{LayerRef, Point2, Polygon};
use pcb_place::placement::{Edge, GroupHint, LockedAt, PlacementHints, Rect};

use crate::tools::PcbToolCtx;

use super::seed::{BoardSeed, BoardSeedPart, BoardSeedRules, Keepout, PourSpec};

// ── derive_board ──────────────────────────────────────────────────────────────

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
}

impl Default for SeedRules {
    fn default() -> Self {
        Self {
            clearance: 0.2,
            min_trace_width: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            layer_count: 2,
            net_widths: BTreeMap::new(),
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
        }
    }
}

fn resolve_pour_layer(layer: &str, layer_count: u32) -> Option<(u32, String)> {
    match layer {
        "top" => Some((0, "F.Cu".to_string())),
        "bottom" => Some((layer_count - 1, "B.Cu".to_string())),
        _ if layer_count >= 6 && layer.starts_with("inner") => layer
            .trim_start_matches("inner")
            .parse::<u32>()
            .ok()
            .filter(|idx| *idx > 0 && *idx < layer_count - 1)
            .map(|idx| (idx, format!("In{idx}.Cu"))),
        _ => None,
    }
}

/// `derive_board` — seed the PCB from KiCAD's own schematic netlist export.
///
/// Footprints must already be assigned in the schematic. Missing footprints are a
/// hard error: the agent should edit the circuit YAML, apply it, then derive the
/// board again.
pub fn derive_board(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
    if !ctx.sch_path().exists() {
        return Ok(json!({
            "error": "no .kicad_sch yet — commit the schematic with apply_design first, \
                      then derive_board"
        }));
    }
    let netlist = match KicadCli::new(ctx.env()).netlist(ctx.sch_path()) {
        Ok(netlist) => netlist,
        Err(e) => {
            return Ok(json!({ "error": format!("could not export the schematic netlist: {e}") }));
        }
    };
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
    let rules = match parse_seed_rules(input.get("rules")) {
        Ok(r) => r,
        Err(msg) => return Ok(json!({ "error": msg })),
    };

    let spec = BoardSeedSpec {
        bounds,
        rules,
        parts,
        outline: None,
    };
    let part_count = spec.parts.len();

    if !missing_footprints.is_empty() {
        return Ok(json!({
            "ok": false,
            "part_count": part_count,
            "missing_footprints": missing_footprints,
            "note": "some schematic symbols have no footprint field — edit the circuit YAML footprint fields, apply_design, then derive_board again",
        }));
    }

    match write_seed_board(&spec, ctx) {
        Ok(()) => {}
        Err(msg) => return Ok(json!({ "error": msg })),
    }
    if let Err(e) = ctx.kicad().open(&ctx.pcb_path()) {
        return Ok(
            json!({ "error": format!("board was written, but KiCAD could not open it over IPC: {e}") }),
        );
    }
    Ok(json!({
        "ok": true,
        "part_count": part_count,
        "path": ctx.pcb_path().display().to_string(),
        "note": "board seeded from the schematic into the live KiCAD session — run place_board, then route_board, then check_board",
    }))
}

fn write_seed_board(spec: &BoardSeedSpec, ctx: &PcbToolCtx) -> std::result::Result<(), String> {
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
            format!(
                "part {}: footprint `{}` source is not readable: {e} — edit the schematic footprint field",
                dp.reference, dp.footprint
            )
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

fn write_initial_board(seed: &BoardSeed, ctx: &PcbToolCtx) -> std::result::Result<(), String> {
    let spec = BoardSeedSpec {
        bounds: seed.bounds.clone(),
        rules: SeedRules::from(&seed.rules),
        parts: seed
            .parts
            .iter()
            .map(|part| SeedPart {
                reference: part.reference.clone(),
                footprint: part.footprint.clone(),
                pad_nets: part.pad_nets.clone(),
                locked: part.locked.clone(),
            })
            .collect(),
        outline: seed.outline.clone(),
    };
    write_seed_board(&spec, ctx)
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
            format!("Width_{}", kicad_sexpr::fmt_num(width).replace('.', "_"))
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
                format!("{}mm trace-width nets", kicad_sexpr::fmt_num(trace_width))
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
            let _ = writeln!(
                out,
                "\t\t(clearance {})",
                kicad_sexpr::fmt_num(class.clearance)
            );
            let _ = writeln!(
                out,
                "\t\t(trace_width {})",
                kicad_sexpr::fmt_num(class.trace_width)
            );
            let _ = writeln!(
                out,
                "\t\t(via_dia {})",
                kicad_sexpr::fmt_num(class.via_diameter)
            );
            let _ = writeln!(
                out,
                "\t\t(via_drill {})",
                kicad_sexpr::fmt_num(class.via_drill)
            );
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
                let (x0, y0) = (kicad_sexpr::fmt_num(a.x), kicad_sexpr::fmt_num(a.y));
                let (x1, y1) = (kicad_sexpr::fmt_num(b.x), kicad_sexpr::fmt_num(b.y));
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
        let (x0, y0) = (
            kicad_sexpr::fmt_num(self.bounds.min_x),
            kicad_sexpr::fmt_num(self.bounds.min_y),
        );
        let (x1, y1) = (
            kicad_sexpr::fmt_num(self.bounds.max_x),
            kicad_sexpr::fmt_num(self.bounds.max_y),
        );
        let uuid = seed_uuid(&format!("edge:{x0}:{y0}:{x1}:{y1}"));
        let _ = write!(
            out,
            "\t(gr_rect\n\t\t(start {x0} {y0})\n\t\t(end {x1} {y1})\n\
             \t\t(stroke\n\t\t\t(width 0.1)\n\t\t\t(type default)\n\t\t)\n\
             \t\t(fill no)\n\t\t(layer \"Edge.Cuts\")\n\t\t(uuid \"{uuid}\")\n\t)\n"
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
            kicad_sexpr::fmt_num(part.at.x),
            kicad_sexpr::fmt_num(part.at.y)
        );
    } else {
        let _ = writeln!(
            out,
            "\t\t(at {} {} {})",
            kicad_sexpr::fmt_num(part.at.x),
            kicad_sexpr::fmt_num(part.at.y),
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
        kicad_sexpr::fmt_num(w),
        kicad_sexpr::fmt_num(h),
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
        format!("(at {x} {y} {})", kicad_sexpr::fmt_num(new_rot))
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

/// Parse the board `bounds` from snake_case model input into the engine's
/// [`Rect`] (whose serde is camelCase, so we read fields explicitly rather than
/// deserializing directly — the tool API stays snake_case like the others).
fn parse_bounds(v: Option<&Value>) -> std::result::Result<Rect, String> {
    let Some(obj) = v else {
        return Err("missing required `bounds` ({min_x, max_x, min_y, max_y} in mm)".into());
    };
    Ok(Rect {
        min_x: req_num(obj, "min_x", "bounds")?,
        max_x: req_num(obj, "max_x", "bounds")?,
        min_y: req_num(obj, "min_y", "bounds")?,
        max_y: req_num(obj, "max_y", "bounds")?,
    })
}

/// Parse optional `rules` from snake_case model input.
fn parse_seed_rules(v: Option<&Value>) -> std::result::Result<SeedRules, String> {
    parse_rules(v).map(|rules| SeedRules::from(&rules))
}

fn parse_rules(v: Option<&Value>) -> std::result::Result<BoardSeedRules, String> {
    let d = BoardSeedRules::default();
    let obj = match v {
        None | Some(Value::Null) => return Ok(d),
        Some(obj) => obj,
    };
    // Partial rules are allowed: any omitted field falls back to the engine
    // default, so a caller can pass just `{ "layers": 4 }` or `{ "clearance": 0.15 }`.
    let num = |k: &str, fallback: f64| obj.get(k).and_then(Value::as_f64).unwrap_or(fallback);
    let layer_count = obj
        .get("layers")
        .or_else(|| obj.get("layer_count"))
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(d.layer_count);
    if !matches!(layer_count, 2 | 4 | 6 | 8) {
        return Err(format!(
            "rules.layers must be 2, 4, 6, or 8, got {layer_count}"
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
        for (net, w) in map {
            let w = w
                .as_f64()
                .ok_or_else(|| format!("rules.net_widths[{net}] must be a number (mm)"))?;
            if w <= 0.0 {
                return Err(format!("rules.net_widths[{net}] must be > 0, got {w}"));
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
                         {layer_count}-layer board — use top/bottom, or innerN on a 6-layer board"
                    ));
                }
                Some((idx, _))
                    if grid_astar::router::plane_layers(layer_count as usize).contains(&idx) =>
                {
                    return Err(format!(
                        "rules.pours[].layer '{layer}' is a GND/VCC PLANE on a {layer_count}-layer \
                         board — a plane is already full copper; pour on a signal layer instead"
                    ));
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

/// KiCAD 9 built-in (standard-fab) minimums, verified against `kicad-cli pcb drc`:
/// a via below these trips `via_diameter` / `drill_out_of_range` / `annular_width`.
const KICAD_MIN_VIA_DIAMETER: f64 = 0.5;
const KICAD_MIN_VIA_DRILL: f64 = 0.3;
const KICAD_MIN_ANNULAR: f64 = 0.1;

/// Parse one part JSON into a validated [`BoardSeedPart`].
fn parse_seed_part(
    pj: &Value,
    catalog: &kicad_footprint::FootprintCatalog,
    clearance: f64,
) -> std::result::Result<BoardSeedPart, Value> {
    let reference = pj
        .get("reference")
        .and_then(Value::as_str)
        .ok_or_else(|| json!({ "error": "a part is missing its string `reference`" }))?
        .to_string();
    let footprint = pj
        .get("footprint")
        .and_then(Value::as_str)
        .ok_or_else(
            || json!({ "error": format!("part {reference}: missing string `footprint` lib_id") }),
        )?
        .to_string();
    let id = match FootprintId::parse(&footprint) {
        Ok(id) => id,
        Err(_) => {
            return Err(json!({
                "error": format!("part {reference}: invalid footprint id `{footprint}`"),
            }));
        }
    };
    let resolved_fp = match catalog.footprint(&id) {
        Ok(fp) => fp,
        Err(e) if e.is_not_found() => {
            return Err(json!({
                "error": format!(
                    "part {reference}: unknown footprint `{footprint}` — \
                     search_footprints for the real lib_id, never guess it"
                ),
                "suggestions": catalog.suggest(&id).iter().map(|i| i.to_string()).collect::<Vec<_>>(),
            }));
        }
        Err(e) => {
            return Err(json!({
                "error": format!("part {reference}: footprint `{footprint}` could not be read: {e}"),
            }));
        }
    };
    let pad_nets: BTreeMap<String, String> = match pj.get("pad_nets") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
            json!({ "error": format!("part {reference}: pad_nets must map pad number → net name: {e}") })
        })?,
    };
    let _ = (resolved_fp, clearance);
    // Optional lock: pin a part at a position/rotation. Accepts {x,y,rotation?} or {at:{x,y},…}.
    let locked = match pj.get("locked") {
        None | Some(Value::Null) => None,
        Some(l) => {
            let at = l.get("at").unwrap_or(l);
            match (
                at.get("x").and_then(Value::as_f64),
                at.get("y").and_then(Value::as_f64),
            ) {
                (Some(x), Some(y)) => {
                    let raw = l.get("rotation").and_then(Value::as_f64).unwrap_or(0.0);
                    let rotation = axis_aligned_rotation(raw)
                        .map_err(|e| json!({ "error": format!("part {reference}: {e}") }))?;
                    Some(LockedAt {
                        at: Point2 { x, y },
                        rotation,
                    })
                }
                _ => {
                    return Err(
                        json!({ "error": format!("part {reference}: `locked` needs numeric x and y") }),
                    );
                }
            }
        }
    };
    Ok(BoardSeedPart {
        reference,
        footprint,
        pad_nets,
        locked,
    })
}

/// Build the first KiCAD board from a `{bounds, parts, rules?, outline?}` spec.
///
/// Internal builder, not an agent tool. [`derive_board`] is the normal path; the
/// deterministic harnesses call this directly with standalone JSON specs.
pub fn build_seed_board(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
    // Optional custom OUTLINE (polygon points, mm) — circle/square/star/any shape. When
    // given, `bounds` is its bounding box (placement/routing extent) and the polygon
    // becomes the Edge.Cuts at export (the render then shows the true shape).
    let outline: Option<Polygon> = match input.get("outline") {
        None | Some(Value::Null) => None,
        Some(o) => {
            let arr = match o.as_array() {
                Some(a) if a.len() >= 3 => a,
                _ => {
                    return Ok(json!({ "error": "outline must be an array of >= 3 [x,y] points" }));
                }
            };
            let mut pts = Vec::with_capacity(arr.len());
            for p in arr {
                match p.as_array().map(|xy| (xy.len(), xy)) {
                    Some((2, xy)) => match (xy[0].as_f64(), xy[1].as_f64()) {
                        (Some(x), Some(y)) => pts.push(Point2 { x, y }),
                        _ => return Ok(json!({ "error": "outline point must be [x, y] numbers" })),
                    },
                    _ => return Ok(json!({ "error": "outline point must be a [x, y] pair" })),
                }
            }
            match Polygon::new(pts) {
                Ok(poly) => Some(poly),
                Err(msg) => return Ok(json!({ "error": msg })),
            }
        }
    };
    let bounds = match &outline {
        Some(o) => o.bbox(),
        None => match parse_bounds(input.get("bounds")) {
            Ok(b) => b,
            Err(msg) => return Ok(json!({ "error": msg })),
        },
    };

    let rules = match parse_rules(input.get("rules")) {
        Ok(r) => r,
        Err(msg) => return Ok(json!({ "error": msg })),
    };

    let Some(parts_json) = input.get("parts").and_then(Value::as_array) else {
        return Ok(json!({
            "error": "missing required `parts` array (each {reference, footprint, pad_nets})",
        }));
    };

    let catalog = ctx.footprint_catalog()?;
    let mut parts: Vec<BoardSeedPart> = Vec::with_capacity(parts_json.len());

    // Resolve every footprint up front; a single unknown lib_id is a recoverable
    // error with suggestions (mirrors get_footprint_info), so the model can fix
    // exactly that part rather than re-sending the whole board.
    for pj in parts_json {
        match parse_seed_part(pj, catalog, rules.clearance) {
            Ok(p) => parts.push(p),
            Err(e) => return Ok(e),
        }
    }

    // Validate references are unique (the engine sorts/dedups by reference).
    let mut seen = std::collections::BTreeSet::new();
    for p in &parts {
        if !seen.insert(p.reference.clone()) {
            return Ok(json!({
                "error": format!("duplicate reference `{}` — references must be unique", p.reference),
            }));
        }
    }

    // Keep single-pin nets as warnings: they are valid, but there is nothing to route.
    let net_pins = net_pin_counts(&parts, ctx);

    // Nets with < 2 pins have nothing to connect — surface as warnings (not
    // errors): a board may legitimately carry a test point or a no-connect.
    let warnings: Vec<String> = net_pins
        .iter()
        .filter(|&(_, &count)| count < 2)
        .map(|(net, count)| {
            format!("net `{net}` has only {count} pin — nothing to route (single-pin net)")
        })
        .collect();

    let seed = BoardSeed {
        bounds,
        rules,
        parts,
        keepouts: Vec::new(),
        hints: PlacementHints::default(),
        outline,
    };
    if let Err(msg) = write_initial_board(&seed, ctx) {
        return Ok(json!({ "error": msg }));
    }

    Ok(json!({
        "ok": true,
        "board_written": true,
        "part_count": seed.parts.len(),
        "net_count": net_pins.len(),
        "warnings": warnings,
    }))
}

/// Pin count per net across seed parts.
pub(super) fn net_pin_counts(parts: &[BoardSeedPart], ctx: &PcbToolCtx) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let _ = ctx;
    for p in parts {
        for net in p.pad_nets.values().filter(|net| !net.is_empty()) {
            *counts.entry(net.clone()).or_default() += 1;
        }
    }
    counts
}

// ── placement-hint helpers (group parsing for the DSL) ───────────────────────

/// Validate a part rotation is axis-aligned (0/90/180/270), normalizing to
/// `[0,360)`. The placer + synth support only these; rejecting others HERE (at the
/// agent surface) fails fast with a clear message, instead of routing to wrong pad
/// positions (`rotate_offset` is identity for non-axis angles) and failing late at
/// export.
fn axis_aligned_rotation(rot: f64) -> std::result::Result<f64, String> {
    let r = rot.rem_euclid(360.0);
    let snapped = geom::snap_quadrant(r);
    if (snapped - r).abs() <= geom::EPS {
        Ok(snapped)
    } else {
        Err(format!(
            "rotation {rot}° is not supported — use 0, 90, 180, or 270 \
             (the engine places axis-aligned parts only)"
        ))
    }
}

/// Parse one group hint from snake_case model input, validating its members
/// against known references. Returns the engine [`GroupHint`] on
/// success or a model-readable error string.
pub(super) fn parse_group_hint(
    v: &Value,
    known_refs: &[&str],
) -> std::result::Result<GroupHint, String> {
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "each group needs a string `name`".to_string())?
        .to_string();
    let members_json = v
        .get("members")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("group `{name}`: needs a `members` array of references"))?;
    let mut members = Vec::with_capacity(members_json.len());
    for m in members_json {
        let r = m
            .as_str()
            .ok_or_else(|| format!("group `{name}`: members must be reference strings"))?;
        if !known_refs.contains(&r) {
            return Err(format!(
                "group `{name}`: member `{r}` is not a part on this board — known references: {}",
                known_refs.join(", ")
            ));
        }
        members.push(r.to_string());
    }
    // region / edge are optional; reuse the engine serde so the vocabulary stays
    // single-sourced (these are camelCase-or-simple shapes the LLM can author).
    let region = match v.get("region") {
        None | Some(Value::Null) => None,
        Some(r) => Some(parse_rect(r).map_err(|e| format!("group `{name}`: region {e}"))?),
    };
    let edge = match v.get("edge") {
        None | Some(Value::Null) => None,
        Some(e) => Some(parse_edge(e).map_err(|err| format!("group `{name}`: {err}"))?),
    };
    let grid = v.get("grid").and_then(Value::as_bool).unwrap_or(false);
    if grid && region.is_none() {
        return Err(format!(
            "group `{name}`: `grid` requires a `region` to tile into"
        ));
    }
    let surround = match v.get("surround") {
        None | Some(Value::Null) => None,
        Some(s) => {
            let r = s.as_str().ok_or_else(|| {
                format!("group `{name}`: `surround` must be a part-reference string")
            })?;
            if !known_refs.contains(&r) {
                return Err(format!(
                    "group `{name}`: surround target `{r}` is not a part on this board"
                ));
            }
            Some(r.to_string())
        }
    };
    Ok(GroupHint {
        name,
        members,
        region,
        edge,
        grid,
        surround,
    })
}

/// Parse a `{min_x,max_x,min_y,max_y}` rect from snake_case model input.
fn parse_rect(v: &Value) -> std::result::Result<Rect, String> {
    Ok(Rect {
        min_x: req_num(v, "min_x", "rect")?,
        max_x: req_num(v, "max_x", "rect")?,
        min_y: req_num(v, "min_y", "rect")?,
        max_y: req_num(v, "max_y", "rect")?,
    })
}

/// Parse an edge hint ("n"/"s"/"e"/"w", case-insensitive).
fn parse_edge(v: &Value) -> std::result::Result<Edge, String> {
    let s = v
        .as_str()
        .ok_or_else(|| "edge must be a string \"n\"/\"s\"/\"e\"/\"w\"".to_string())?;
    match s.to_ascii_lowercase().as_str() {
        "n" => Ok(Edge::N),
        "s" => Ok(Edge::S),
        "e" => Ok(Edge::E),
        "w" => Ok(Edge::W),
        other => Err(format!(
            "edge must be \"n\"/\"s\"/\"e\"/\"w\", got {other:?}"
        )),
    }
}

// ── keepout helpers (rect parsing for the DSL) ───────────────────────────────

/// Validate a keepout rectangle lies within the board bounds and lists only
/// known copper layers, then return the engine [`Keepout`].
pub(super) fn parse_keepout(
    v: &Value,
    bounds: &Rect,
    layer_count: u32,
    idx: usize,
) -> std::result::Result<Keepout, String> {
    let ctxstr = format!("keepouts[{idx}]");
    let rect_v = v
        .get("rect")
        .ok_or_else(|| format!("{ctxstr}: missing `rect` {{min_x,max_x,min_y,max_y}}"))?;
    let rect = parse_rect(rect_v).map_err(|e| format!("{ctxstr}: rect {e}"))?;
    if rect.min_x >= rect.max_x || rect.min_y >= rect.max_y {
        return Err(format!("{ctxstr}: rect is degenerate (min must be < max)"));
    }
    // Within bounds (a keepout outside the board is almost certainly a mistake).
    if rect.min_x < bounds.min_x - geom::EPS
        || rect.max_x > bounds.max_x + geom::EPS
        || rect.min_y < bounds.min_y - geom::EPS
        || rect.max_y > bounds.max_y + geom::EPS
    {
        return Err(format!(
            "{ctxstr}: rect [{},{}]x[{},{}] is outside the board bounds [{},{}]x[{},{}]",
            rect.min_x,
            rect.max_x,
            rect.min_y,
            rect.max_y,
            bounds.min_x,
            bounds.max_x,
            bounds.min_y,
            bounds.max_y
        ));
    }
    let layers_json = v
        .get("layers")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{ctxstr}: missing `layers` array (e.g. [\"top\",\"bottom\"])"))?;
    if layers_json.is_empty() {
        return Err(format!(
            "{ctxstr}: `layers` must name at least one copper layer"
        ));
    }
    let mut layers = Vec::with_capacity(layers_json.len());
    for l in layers_json {
        let name = l
            .as_str()
            .ok_or_else(|| format!("{ctxstr}: layer names must be strings"))?;
        let layer = LayerRef(name.to_string());
        // Resolve against THIS board's stackup: "top"/"bottom" always, plus
        // "inner1".."inner{layer_count-2}" on a multilayer board.
        if layer.index(layer_count).is_none() {
            let inners = if layer_count >= 4 {
                format!(", \"inner1\"..\"inner{}\"", layer_count - 2)
            } else {
                String::new()
            };
            return Err(format!(
                "{ctxstr}: unknown layer `{name}` — this board has \"top\", \"bottom\"{inners}"
            ));
        }
        layers.push(layer);
    }
    Ok(Keepout { rect, layers })
}
