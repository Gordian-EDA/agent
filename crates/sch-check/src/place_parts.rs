//! The connectivity-only input of the bulk-create tool.
//!
//! The LLM states parts and what each pin connects to — never a coordinate, and
//! never a wire. Layout *intent* rides along as a [`LayoutIr`]; solvers turn it
//! into geometry. [`into_design`] lowers the input to the kernel [`Design`] the
//! checkers and the placement engines already speak.

use indexmap::IndexMap;
use sch_place::ir::LayoutIr;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::*;
use crate::{Diagnostic, Diagnostics, SymbolTable, decouple, nets, pins};

/// Bulk-create payload: the parts to add, plus optional layout intent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacePartsInput {
    pub parts: Vec<PartSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<LayoutIr>,
}

/// One part and its pin connections.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PartSpec {
    /// Refdes, e.g. `U1`.
    #[serde(rename = "ref")]
    pub refdes: RefDes,
    /// Full KiCAD lib_id, e.g. `Device:R`.
    pub part: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footprint: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dnp: bool,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub props: IndexMap<String, String>,
    /// Pin name or number → net name, or `"nc"` for an explicit no-connect.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub pins: IndexMap<String, String>,
    /// Decoupling sugar: cap value → count, expanded across this part's rails.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub decouple: IndexMap<String, u32>,
}

/// The single block a bulk create lands in. One tool call describes one sheet.
pub const BLOCK: &str = "main";

/// Lower the input to a [`Design`]: pin keys resolved to physical pin numbers
/// against the symbol table, `decouple` expanded into [`Origin::Synthesized`]
/// caps, net attributes derived.
///
/// Diagnostics are advisory except where a part or pin does not exist — the
/// caller decides whether to apply a design that carries errors.
pub fn into_design(input: &PlacePartsInput, provider: &SymbolTable) -> (Design, Diagnostics) {
    let mut diags = Diagnostics::default();
    let mut block = Block::default();
    for spec in &input.parts {
        block
            .components
            .insert(spec.refdes.clone(), component(spec, provider, &mut diags));
    }
    let mut design = Design::default();
    design.blocks.insert(BLOCK.to_string(), block);
    expand_decouple(input, &mut design, provider, &mut diags);
    decouple::renumber(&mut design);
    for net in referenced_nets(&design) {
        design.nets.entry(net).or_default();
    }
    nets::derive_attrs(&mut design);
    (design, diags)
}

fn component(spec: &PartSpec, provider: &SymbolTable, diags: &mut Diagnostics) -> Component {
    let mut comp = Component {
        part: spec.part.clone(),
        value: spec.value.clone(),
        footprint: spec.footprint.clone(),
        dnp: spec.dnp,
        props: spec.props.clone(),
        ..Default::default()
    };
    let meta = provider.symbol(&spec.part);
    if meta.is_none() {
        let mut e = Diagnostic::error(
            "unknown-part",
            format!(
                "{}: symbol `{}` not found in any library",
                spec.refdes, spec.part
            ),
        );
        if let Some(s) = provider.suggest(&spec.part).into_iter().next() {
            e = e.with_suggestion(s);
        }
        diags.push(e);
    }
    for (key, net) in &spec.pins {
        let target = if net.eq_ignore_ascii_case("nc") {
            PinTarget::NoConnect
        } else {
            PinTarget::Net(net.clone())
        };
        let Some(meta) = &meta else {
            comp.pins.insert(key.clone(), target);
            continue;
        };
        let numbers: Vec<String> = pins::resolve(meta, key)
            .iter()
            .map(|p| p.number.clone())
            .collect();
        if numbers.is_empty() {
            let mut e = Diagnostic::error(
                "unknown-pin",
                format!("pin `{key}` not found on {} ({})", spec.refdes, spec.part),
            );
            if let Some(s) = pins::nearest(meta, key) {
                e = e.with_suggestion(s);
            }
            diags.push(e);
            continue;
        }
        // A pin NAME covering several physical pins (a stacked `VDD`) connects
        // every one of them; the model is keyed by number so nothing is implicit.
        for number in numbers {
            if let Some(prev) = comp.pins.insert(number.clone(), target.clone())
                && prev != target
            {
                diags.push(Diagnostic::error(
                    "pin-conflict",
                    format!(
                        "{}: physical pin {number} is given two different nets",
                        spec.refdes
                    ),
                ));
            }
        }
    }
    comp
}

fn expand_decouple(
    input: &PlacePartsInput,
    design: &mut Design,
    provider: &SymbolTable,
    diags: &mut Diagnostics,
) {
    for spec in &input.parts {
        if spec.decouple.is_empty() {
            continue;
        }
        let comp = &design.blocks[BLOCK].components[&spec.refdes];
        match decouple::rails(&spec.refdes, comp, provider) {
            Ok(rails) => {
                let caps = decouple::expand(&spec.refdes, &spec.decouple, &rails);
                let block = design.blocks.get_mut(BLOCK).unwrap();
                for (key, cap) in caps {
                    block.components.insert(key, cap);
                }
            }
            Err(diag) => diags.push(diag),
        }
    }
}

fn referenced_nets(design: &Design) -> Vec<NetName> {
    design
        .blocks
        .values()
        .flat_map(|b| b.components.values())
        .flat_map(|c| {
            c.pins
                .values()
                .chain(c.units.values().flatten().map(|(_, t)| t))
        })
        .filter_map(|t| match t {
            PinTarget::Net(n) => Some(n.clone()),
            PinTarget::NoConnect => None,
        })
        .collect()
}

/// JSON Schema for the tool's `input_schema`. Deliberately terse: the LLM needs
/// the shape and the two rules that are not obvious (`"nc"`, and that a pin key
/// may be a name or a number).
pub fn place_parts_input_schema() -> Value {
    json!({
        "type": "object",
        "required": ["parts"],
        "additionalProperties": false,
        "properties": {
            "parts": {
                "type": "array",
                "description": "Parts to create, with their connectivity. No coordinates, no wires.",
                "items": {
                    "type": "object",
                    "required": ["ref", "part"],
                    "additionalProperties": false,
                    "properties": {
                        "ref": {"type": "string", "description": "Refdes, e.g. U1."},
                        "part": {"type": "string", "description": "KiCAD lib_id, e.g. Device:R."},
                        "value": {"type": "string"},
                        "footprint": {"type": "string"},
                        "dnp": {"type": "boolean"},
                        "props": {
                            "type": "object",
                            "description": "Extra symbol properties.",
                            "additionalProperties": {"type": "string"}
                        },
                        "pins": {
                            "type": "object",
                            "description":
                                "Pin name or number -> net name, or \"nc\" for an explicit no-connect. \
                                 A name shared by several physical pins connects all of them.",
                            "additionalProperties": {"type": "string"}
                        },
                        "decouple": {
                            "type": "object",
                            "description":
                                "Capacitor value -> count. Expands into caps across this part's \
                                 single VDD*/VCC* and VSS*/GND* nets.",
                            "additionalProperties": {"type": "integer", "minimum": 1}
                        }
                    }
                }
            },
            "intent": {
                "type": "object",
                "description": "Optional layout intent (the layout IR). Hints only; the solver owns geometry.",
                "properties": {
                    "flow": {"enum": ["lr", "tb"], "description": "Global signal-flow direction."},
                    "rails": {
                        "type": "object",
                        "description": "Net -> \"top\"|\"bottom\": nets drawn as spanning rails.",
                        "additionalProperties": {"enum": ["top", "bottom"]}
                    },
                    "ports": {
                        "type": "object",
                        "description": "Net -> sheet edge it exits toward.",
                        "additionalProperties": {"enum": ["left", "right", "top", "bottom"]}
                    },
                    "place": {
                        "type": "object",
                        "description": "Refdes -> coarse unitless cell.",
                        "additionalProperties": {
                            "type": "object",
                            "required": ["col", "row"],
                            "properties": {
                                "col": {"type": "integer"},
                                "row": {"type": "integer"},
                                "orient": {"enum": ["up", "down", "left", "right"]}
                            }
                        }
                    },
                    "mirror": {
                        "type": "array",
                        "description": "Refdes to flip left-to-right.",
                        "items": {"type": "string"}
                    }
                }
            }
        }
    })
}
