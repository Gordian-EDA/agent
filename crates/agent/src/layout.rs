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

Output ONLY by calling the `emit_layout` tool with these keys:

- flow: "lr" (signals left→right, the usual choice) or "tb".
- rails: map EVERY power/ground net to a band — "top" for positive supplies (VCC, +5V, +3V3,
  9V, VBUS...), "bottom" for grounds (GND, GNDD, VSS...). Include INTERMEDIATE supplies too
  (e.g. a regulator's output 3V3 as well as its input 5V). The engine decides whether to draw
  each as one spanning wire or as scattered power symbols.
- place: map EVERY component refdes to a cell {col, row, orient}. col grows right (downstream
  along the flow), row grows down; both are small integers and need not be contiguous. orient is
  the direction the part's pins run, from its FIRST `between` net (pin 1) toward its SECOND
  (pin 2) — one of "up", "down", "left", "right". Decide it from where the two nets sit in YOUR
  layout:
    * Bridges a top thing and a bottom thing (a supply rail down to ground, a signal down to
      ground): vertical. "down" if pin 1 is the top net (the usual case, e.g. a divider leg or a
      decoupling cap `between:[VCC, GND]`), "up" if pin 1 is the bottom net (e.g. a pull-down or
      an LED-to-ground `between:[GND, NODE]`).
    * Sits IN the left-to-right signal path (a series resistor, a fuse, an LED feeding a
      downstream part): horizontal. "right" if pin 1 is the left (upstream) net, "left" if pin 1
      is the right (downstream) net. Example: an indicator LED `between:[LED_K, 3V3]` fed from a
      3V3 node on its left with its cathode going right to a resistor is "left".
  Think like a draughtsman laying parts on a grid:
    * The central IC sits in a middle column; input connectors at col 0; downstream parts right.
    * A decoupling/bypass cap goes in the column next to the power pin it serves, same row band.
    * A voltage divider stacks vertically: the two resistors share a column, consecutive rows.
    * A pull-up/pull-down belongs in a row ABOVE/BELOW the signal line it taps, not in the line.
    * Parts that share a node should be near each other so the wire is short — but two parts that
      connect to EACH OTHER (e.g. an LED and its series resistor) need a clear path between them:
      give them their own adjacent columns/rows with no third part dropped in the wire's way.
  Give every refdes a cell — do not leave any unplaced.
- ports: map each net that should EXIT the sheet as a labelled port to a side
  ("left"/"right"/"top"/"bottom"). Use this for true I/O: a board input on the left, an output
  on the right. Do NOT make internal power rails ports.
- mirror: list any IC OR CONNECTOR to flip left-right so the pins that face a neighbour point
  the right way. A connector feeding the circuit from the left (col 0) usually needs mirroring so
  its pins face RIGHT into the circuit. A level translator whose B-side faces a connector on its
  left but whose B pins draw on the right by default needs mirroring too. When in doubt, check
  the pin sides in the summary: if the pins that must connect rightward are drawn "left", mirror.

Aim for the layout a careful engineer would draw: clear left-to-right signal flow, power rails
top and bottom, the IC as the centred anchor, passives grouped tightly around the pins they serve."#;

/// Compact tool-call payload (the LLM's view of the IR), converted to a real
/// [`LayoutIr`]. `place` maps every refdes to a `{col,row,orient}` cell.
#[derive(Deserialize, Default)]
struct ToolIn {
    #[serde(default)]
    flow: Option<String>,
    #[serde(default)]
    rails: BTreeMap<String, String>,
    #[serde(default)]
    place: BTreeMap<String, Cell>,
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
                "place": {"type": "object", "additionalProperties": {
                    "type": "object",
                    "properties": {
                        "col": {"type": "integer"},
                        "row": {"type": "integer"},
                        "orient": {"type": "string", "enum": ["up", "down", "left", "right"]}
                    },
                    "required": ["col", "row"]
                }},
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
    let place = raw.place;
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
