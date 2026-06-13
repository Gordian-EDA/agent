//! Layout subagent — Layer 1 of the floorplan engine.
//!
//! Given a compiled [`Design`] (connectivity only), an LLM proposes a
//! geometry-free [`LayoutIr`] (the four-key floorplan language) which the
//! deterministic geometry compiler in `sch_engine::floorplan` turns into a real
//! schematic. The LLM never emits coordinates — only the *frame*: which nets
//! are rails (and their band), where the ICs go on a coarse grid, which nets
//! exit as ports, and which ICs to mirror.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use circuit_lang::model::{Design, PinTarget};
use circuit_lang::SymbolProvider;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::geometry::SymbolGeometry;
use kicad_bridge::provider::RealSymbolProvider;
use sch_engine::floorplan::{Band, Cell, Flow, LayoutIr, Side};
use serde::Deserialize;
use serde_json::json;

use crate::llm::{LlmClient, Message, ToolDef};

const SYSTEM: &str = r#"You are a schematic LAYOUT planner. Given a netlist, you output a compact
floorplan that a deterministic engine turns into a professional, human-style KiCad schematic.
You NEVER give millimetre coordinates — only the coarse "frame". The engine handles all exact
geometry (placement, orthogonal wires, rail synthesis, label placement).

Output ONLY by calling the `emit_layout` tool with these four keys:

- flow: "lr" (signals left→right, the usual choice) or "tb".
- rails: map each POWER/GROUND net to a band — "top" for positive supplies (VCC, +5V, +3V3,
  9V, VBUS...), "bottom" for grounds (GND, GNDD, VSS...). List every power net.
- place: map each multi-pin IC/connector refdes to a coarse [col,row] cell (small integers,
  left→right along the flow; row 0 is the main row, negative is up). Put the central chip at
  a middle column, input connectors at col 0, downstream parts to the right. You usually only
  place ICs/connectors; the engine auto-places the 2-pin passives (decoupling caps hang under
  their rail, dividers stack, series resistors sit beside the IC pin they tap).
- ports: map each net that should EXIT the sheet as a labelled port to a side
  ("left"/"right"/"top"/"bottom"). Use this for true I/O: a board input on the left, an output
  on the right. Do NOT make internal power rails ports.
- mirror: list ICs to flip left-right so the pins that face a neighbour point the right way
  (e.g. a level translator whose B-side connects a connector on its left, but whose B pins are
  drawn on its right by default — mirror it).

Aim for the layout a careful engineer would draw: clear left-to-right signal flow, power rails
top and bottom, the IC as the centred anchor."#;

/// Compact tool-call payload (the LLM's view of the IR), converted to a real
/// [`LayoutIr`]. `place` is `[col,row]` arrays here for token economy.
#[derive(Deserialize, Default)]
struct ToolIn {
    #[serde(default)]
    flow: Option<String>,
    #[serde(default)]
    rails: BTreeMap<String, String>,
    #[serde(default)]
    place: BTreeMap<String, [i32; 2]>,
    #[serde(default)]
    ports: BTreeMap<String, String>,
    #[serde(default)]
    mirror: Vec<String>,
}

fn tool_def() -> ToolDef {
    ToolDef {
        name: "emit_layout".into(),
        description: "Emit the floorplan for this schematic.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "flow": {"type": "string", "enum": ["lr", "tb"]},
                "rails": {"type": "object", "additionalProperties": {"type": "string", "enum": ["top", "bottom"]}},
                "place": {"type": "object", "additionalProperties": {"type": "array", "items": {"type": "integer"}, "minItems": 2, "maxItems": 2}},
                "ports": {"type": "object", "additionalProperties": {"type": "string", "enum": ["left", "right", "top", "bottom"]}},
                "mirror": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["rails"]
        }),
    }
}

/// Propose a [`LayoutIr`] for `design` via `client`. Retries once with the
/// validation error fed back if the first IR references unknown nets/refdes.
pub async fn propose_layout(
    client: &dyn LlmClient,
    env: &KicadEnv,
    design: &Design,
) -> Result<LayoutIr> {
    let summary = summarize(env, design);
    let tools = [tool_def()];
    let mut messages = vec![Message::user(summary)];

    for attempt in 0..2 {
        let completion = client.complete(SYSTEM, &messages, &tools).await?;
        let call = completion
            .tool_calls
            .into_iter()
            .find(|c| c.name == "emit_layout")
            .ok_or_else(|| anyhow!("model did not call emit_layout: {}", completion.text))?;
        let raw: ToolIn = serde_json::from_value(call.input.clone())
            .context("emit_layout input did not match schema")?;
        let ir = to_ir(raw);
        match validate(&ir, design) {
            Ok(()) => return Ok(ir),
            Err(e) if attempt == 0 => {
                messages.push(Message::assistant(format!("(proposed layout)")));
                messages.push(Message::user(format!(
                    "That layout is invalid: {e}. Call emit_layout again with the fix."
                )));
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

fn to_ir(raw: ToolIn) -> LayoutIr {
    let flow = match raw.flow.as_deref() {
        Some("tb") => Flow::Tb,
        _ => Flow::Lr,
    };
    let rails = raw
        .rails
        .into_iter()
        .map(|(n, b)| (n, if b == "bottom" { Band::Bottom } else { Band::Top }))
        .collect();
    let place = raw
        .place
        .into_iter()
        .map(|(r, c)| (r, Cell { col: c[0], row: c[1] }))
        .collect();
    let ports = raw
        .ports
        .into_iter()
        .map(|(n, s)| {
            let side = match s.as_str() {
                "left" => Side::Left,
                "top" => Side::Top,
                "bottom" => Side::Bottom,
                _ => Side::Right,
            };
            (n, side)
        })
        .collect();
    LayoutIr { flow, rails, place, ports, mirror: raw.mirror.into_iter().collect() }
}

/// Validate that the IR only references nets/refdes that exist in the design.
fn validate(ir: &LayoutIr, design: &Design) -> Result<()> {
    let mut nets = std::collections::BTreeSet::new();
    let mut refs = std::collections::BTreeSet::new();
    for block in design.blocks.values() {
        for (refdes, comp) in &block.components {
            refs.insert(refdes.clone());
            for t in comp.pins.values() {
                if let PinTarget::Net(n) = t {
                    nets.insert(n.clone());
                }
            }
        }
    }
    for n in ir.rails.keys().chain(ir.ports.keys()) {
        if !nets.contains(n) {
            return Err(anyhow!("net '{n}' is not in the design"));
        }
    }
    for r in ir.place.keys().chain(ir.mirror.iter()) {
        if !refs.contains(r) {
            return Err(anyhow!("refdes '{r}' is not in the design"));
        }
    }
    Ok(())
}

/// A compact, LLM-friendly description of the netlist: components (with IC pin
/// sides) and nets (with their pins and whether they are declared power).
fn summarize(env: &KicadEnv, design: &Design) -> String {
    let provider = RealSymbolProvider::new(env.clone());
    let mut s = String::new();
    if let Some(name) = &design.name {
        s.push_str(&format!("Design: {name}\n"));
    }
    s.push_str("\nComponents:\n");
    for block in design.blocks.values() {
        for (refdes, comp) in &block.components {
            let npins = provider.symbol(&comp.part).map(|m| m.pins.len()).unwrap_or(0);
            let kind = if npins >= 3 { "IC/connector" } else { "2-pin" };
            s.push_str(&format!("- {refdes} ({}, {kind})", comp.part));
            if let Some(v) = &comp.value {
                s.push_str(&format!(" = {v}"));
            }
            // For ICs, list pin → net with the pin's side on the symbol body.
            if npins >= 3 {
                if let Ok(geom) = SymbolGeometry::load(env, &comp.part) {
                    let sides = pin_sides(&geom);
                    s.push_str(": ");
                    let pins: Vec<String> = comp
                        .pins
                        .iter()
                        .filter_map(|(pin, t)| match t {
                            PinTarget::Net(n) => {
                                let side = sides.get(pin).map(String::as_str).unwrap_or("?");
                                Some(format!("{pin}[{side}]→{n}"))
                            }
                            _ => None,
                        })
                        .collect();
                    s.push_str(&pins.join(", "));
                }
            } else {
                let nets: Vec<String> = comp
                    .pins
                    .values()
                    .filter_map(|t| match t {
                        PinTarget::Net(n) => Some(n.clone()),
                        _ => None,
                    })
                    .collect();
                s.push_str(&format!(": {}", nets.join(" — ")));
            }
            s.push('\n');
        }
    }
    s.push_str("\nDeclared power/ground nets: ");
    let rails: Vec<&String> = design.nets.iter().filter(|(_, a)| a.power).map(|(n, _)| n).collect();
    s.push_str(&rails.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
    s.push_str("\n\nPropose the floorplan via emit_layout.");
    s
}

/// Per-pin side label ("left"/"right"/"top"/"bottom") from the symbol geometry,
/// keyed by both pin number and name.
fn pin_sides(geom: &SymbolGeometry) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for p in &geom.pins {
        let [x, y] = p.at;
        let side = if x.abs() >= y.abs() {
            if x < 0.0 { "left" } else { "right" }
        } else if y > 0.0 {
            "top"
        } else {
            "bottom"
        };
        out.insert(p.number.clone(), side.to_string());
        out.insert(p.name.clone(), side.to_string());
    }
    out
}
