//! The connectivity-only input of the bulk-create tool.
//!
//! The LLM states parts and what each pin connects to — never a coordinate, and
//! never a wire. Layout *intent* rides along as [`Intent`]; solvers turn it into
//! geometry. [`into_design`] lowers the input to the kernel [`Design`] the
//! checkers and the placement engines already speak; the caller hands
//! [`Intent::into_layout_ir`] to the placement engine alongside it.

use std::collections::{BTreeMap, BTreeSet};

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use indexmap::IndexMap;
use sch_place::ir::{Band, Cell, Flow, LayoutIr, Relation, Side};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::*;
use crate::{Diagnostic, Diagnostics, SymbolTable, authored, decouple, nets, pins};

/// Bulk-create payload: the parts to add, plus optional layout intent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlacePartsInput {
    pub parts: Vec<PartSpec>,
    /// Sheet title, drawn in the frame's title block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Region every part without its own `block` joins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<BlockName>,
    /// Region → its internal placement grid: rows of refdes, `null` for a hole.
    /// A refdes repeated down a column spans those rows. Regions left out are
    /// arranged from connectivity.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub layout: BTreeMap<BlockName, LayoutGrid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<Intent>,
}

/// One part and its pin connections.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PartSpec {
    /// Refdes, e.g. `U1`.
    #[serde(rename = "ref")]
    pub refdes: RefDes,
    /// Placement region this part joins, when the sheet has more than one.
    /// Defaults to the payload's `block`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<BlockName>,
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
    /// Pin name or number → net name, or `"nc"` for an explicit no-connect. Pin
    /// numbers are unique across a multi-unit symbol's units, so this one map
    /// reaches every unit.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub pins: IndexMap<String, String>,
    /// Decoupling sugar: cap value → count, expanded across this part's rails.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub decouple: IndexMap<String, u32>,
}

/// Existing named nets and the number of live sheet pins already on each one.
pub type ExistingNetPins = BTreeMap<String, usize>;

/// A new pin whose named net would have no other pin after placement.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DanglingPin {
    #[serde(rename = "ref")]
    pub refdes: RefDes,
    pub pin: String,
    pub net: NetName,
}

/// Findings that make a bulk-create payload electrically incomplete.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct PayloadAudit {
    pub dangling: Vec<DanglingPin>,
    pub did_you_mean: BTreeMap<NetName, NetName>,
    pub unknown_pins: Vec<String>,
}

impl PayloadAudit {
    /// Whether the payload may proceed to placement.
    pub fn is_valid(&self) -> bool {
        self.dangling.is_empty() && self.unknown_pins.is_empty()
    }
}

/// The layout hints an LLM may state — the input-facing subset of the engine's
/// [`LayoutIr`]. What the engine derives for itself (recognized idioms, frozen
/// clusters, zone biases) is absent rather than silently accepted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    /// Global signal-flow direction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow: Option<Flow>,
    /// Net → band, for nets to draw as spanning rails.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rails: BTreeMap<NetName, Band>,
    /// Net → the sheet edge it exits toward.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ports: BTreeMap<NetName, Side>,
    /// Refdes → coarse unitless cell. Usually only the anchors.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub place: BTreeMap<RefDes, Cell>,
    /// Anchors to flip left-to-right.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub mirror: BTreeSet<RefDes>,
    /// Relative statements about parts — `left_of`, `group`, `align`. The only way
    /// to say where new parts go with respect to parts already on the sheet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<Relation>,
}

impl Intent {
    /// The engine-facing IR. Everything the engine derives itself stays empty.
    pub fn into_layout_ir(self) -> LayoutIr {
        LayoutIr {
            flow: self.flow.unwrap_or_default(),
            rails: self.rails,
            ports: self.ports,
            place: self.place,
            mirror: self.mirror,
            relations: self.relations,
            ..Default::default()
        }
    }
}

/// The sheet a payload without an explicit `block` fills.
pub const DEFAULT_BLOCK: &str = "main";

/// Lower the input to a [`Design`]: pin keys resolved to physical pin numbers
/// against the symbol table, `decouple` expanded into [`Origin::Synthesized`]
/// caps, unconnected signal pins marked no-connect, net attributes derived.
///
/// Diagnostics carry what makes the input un-buildable — an unknown part or
/// pin, a duplicate refdes. The payload audit separately reports new pins whose
/// nets would have no other pin across the payload and existing sheet.
pub fn into_design(
    input: &PlacePartsInput,
    provider: &SymbolTable,
    existing: &ExistingNetPins,
) -> (Design, Diagnostics, PayloadAudit) {
    let mut diags = Diagnostics::default();
    let mut design = Design {
        name: input.name.clone(),
        ..Design::default()
    };
    let default_block = input.block.as_deref().unwrap_or(DEFAULT_BLOCK);
    for spec in &input.parts {
        let name = spec.block.as_deref().unwrap_or(default_block);
        let comp = component(spec, provider, &mut diags);
        let block = design.blocks.entry(name.to_string()).or_default();
        if block.components.insert(spec.refdes.clone(), comp).is_some() {
            diags.push(Diagnostic::error(
                "duplicate-ref",
                format!("`{}` is declared twice — one of them is lost", spec.refdes),
            ));
        }
    }
    if design.blocks.is_empty() {
        design
            .blocks
            .insert(default_block.to_string(), Block::default());
    }
    for (name, grid) in &input.layout {
        match design.blocks.get_mut(name) {
            Some(block) => block.layout = grid.clone(),
            None => diags.push(Diagnostic::error(
                "unknown-block",
                format!("`layout` names region `{name}`, which no part joins"),
            )),
        }
    }
    expand_decouple(input, default_block, &mut design, provider, &mut diags);
    decouple::renumber(&mut design);
    pins::mark_unused_no_connect(&mut design, provider);
    nets::derive_attrs(&mut design);
    let audit = audit_payload(input, &design, provider, existing, &diags);
    (design, diags, audit)
}

fn audit_payload(
    input: &PlacePartsInput,
    design: &Design,
    provider: &SymbolTable,
    existing: &ExistingNetPins,
    lowering: &Diagnostics,
) -> PayloadAudit {
    let mut pin_counts = existing.clone();
    for component in design
        .blocks
        .values()
        .flat_map(|block| block.components.values())
    {
        for target in component
            .pins
            .values()
            .chain(component.units.values().flatten().map(|(_, target)| target))
        {
            if let PinTarget::Net(net) = target {
                *pin_counts.entry(net.clone()).or_default() += 1;
            }
        }
    }

    let mut audit = PayloadAudit::default();
    for spec in &input.parts {
        let Some(meta) = provider.symbol(&spec.part) else {
            continue;
        };
        for (pin, net) in &spec.pins {
            if net.eq_ignore_ascii_case("nc") || pins::resolve(&meta, pin).is_empty() {
                continue;
            }
            if pin_counts.get(net).copied().unwrap_or_default() < 2 {
                audit.dangling.push(DanglingPin {
                    refdes: spec.refdes.clone(),
                    pin: pin.clone(),
                    net: net.clone(),
                });
                if !existing.contains_key(net)
                    && let Some(candidate) =
                        closest_net_name(net, existing.keys().map(String::as_str))
                {
                    audit.did_you_mean.insert(net.clone(), candidate);
                }
            }
        }
    }
    audit.dangling.sort_by(|left, right| {
        left.refdes
            .cmp(&right.refdes)
            .then_with(|| left.pin.cmp(&right.pin))
            .then_with(|| left.net.cmp(&right.net))
    });
    audit.unknown_pins.extend(
        lowering
            .0
            .iter()
            .filter(|diagnostic| diagnostic.code == "unknown-pin")
            .map(ToString::to_string),
    );
    audit.unknown_pins.extend(
        crate::lint::lint(design, provider)
            .0
            .into_iter()
            .filter(|diagnostic| diagnostic.code == "library-no-connect-wired")
            .map(|diagnostic| diagnostic.to_string()),
    );
    audit.unknown_pins.sort();
    audit.unknown_pins.dedup();
    audit
}

/// The best subsequence match for a net name, when any candidate is related.
pub fn closest_net_name<'a>(
    net: &str,
    candidates: impl Iterator<Item = &'a str>,
) -> Option<String> {
    let matcher = SkimMatcherV2::default().ignore_case();
    candidates
        .filter(|candidate| *candidate != net)
        .filter_map(|candidate| {
            let score = matcher
                .fuzzy_match(candidate, net)
                .into_iter()
                .chain(matcher.fuzzy_match(net, candidate))
                .max()?;
            Some((score, candidate))
        })
        .max_by(|(left_score, left), (right_score, right)| {
            left_score.cmp(right_score).then_with(|| right.cmp(left))
        })
        .map(|(_, candidate)| candidate.to_string())
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
    let Some(meta) = provider.symbol(&spec.part) else {
        diags.push(authored::unknown_part(&spec.refdes, &spec.part, provider));
        for (key, net) in &spec.pins {
            comp.pins.insert(key.clone(), target(net));
        }
        return comp;
    };
    // Keyed by physical pin number — the identity a symbol cannot restate, and
    // what the writer and the extractor both use. A name covering several pins
    // (a stacked `VDD`) connects every one of them.
    let mut claimed: IndexMap<String, &String> = IndexMap::new();
    for (key, net) in &spec.pins {
        let numbers: Vec<String> = pins::resolve(&meta, key)
            .iter()
            .map(|p| p.number.clone())
            .collect();
        if numbers.is_empty() {
            diags.push(authored::unknown_pin(&spec.refdes, &spec.part, &meta, key));
            continue;
        }
        for number in numbers {
            if let Some(prev) = claimed.insert(number.clone(), key)
                && prev != key
            {
                diags.push(authored::pin_conflict(&spec.refdes, &number, prev, key));
            }
            comp.pins.insert(number, target(net));
        }
    }
    comp
}

fn target(net: &str) -> PinTarget {
    if net.eq_ignore_ascii_case("nc") {
        PinTarget::NoConnect
    } else {
        PinTarget::Net(net.to_string())
    }
}

fn expand_decouple(
    input: &PlacePartsInput,
    default_block: &str,
    design: &mut Design,
    provider: &SymbolTable,
    diags: &mut Diagnostics,
) {
    for spec in &input.parts {
        if spec.decouple.is_empty() {
            continue;
        }
        let block = spec.block.as_deref().unwrap_or(default_block);
        let comp = &design.blocks[block].components[&spec.refdes];
        match decouple::rails(&spec.refdes, comp, provider) {
            Ok(rails) => {
                let caps = decouple::expand(&spec.refdes, &spec.decouple, &rails);
                let block = design.blocks.get_mut(block).unwrap();
                for (key, cap) in caps {
                    block.components.insert(key, cap);
                }
            }
            Err(diag) => diags.push(diag),
        }
    }
}

/// JSON Schema for the tool's `input_schema`. Deliberately terse: the LLM needs
/// the shape and the rules that are not obvious (`"nc"`, that a pin key may be a
/// name or a number, and that anything left out is a no-connect).
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
                        "block": {
                            "type": "string",
                            "description": "Placement region this part joins. Defaults to the payload's block."
                        },
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
                                 A name shared by several physical pins connects all of them; every \
                                 signal pin left out becomes a no-connect. Every named net must land \
                                 on at least two pins across these parts and the existing sheet.",
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
            "name": {
                "type": "string",
                "description": "Sheet title, drawn in the frame's title block."
            },
            "block": {
                "type": "string",
                "description": "Region every part without its own `block` joins."
            },
            "layout": {
                "type": "object",
                "description":
                    "Region -> rows of refdes (null for a hole): that region's internal grid. \
                     A refdes repeated down a column spans those rows.",
                "additionalProperties": {
                    "type": "array",
                    "items": {
                        "type": "array",
                        "items": {"type": ["string", "null"]}
                    }
                }
            },
            "intent": {
                "type": "object",
                "description": "Optional layout intent. Hints only; the solver owns all geometry.",
                "additionalProperties": false,
                "properties": {
                    "flow": {"enum": ["lr", "tb"], "description": "Global signal-flow direction."},
                    "rails": {
                        "type": "object",
                        "description": "Net -> \"top\"|\"bottom\": nets drawn as spanning rails.",
                        "additionalProperties": {"enum": ["top", "bottom"]}
                    },
                    "ports": {
                        "type": "object",
                        "description": "Net -> the sheet edge it exits toward.",
                        "additionalProperties": {"enum": ["left", "right", "top", "bottom"]}
                    },
                    "place": {
                        "type": "object",
                        "description": "Refdes -> coarse unitless cell (col grows right, row grows down).",
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
                    },
                    "relations": {
                        "type": "array",
                        "description":
                            "Relative placement. Each entry is tagged by \"kind\": \
                             left_of|right_of|above|below with {a, b}; \
                             group with {name, members, side: [edge, anchor]}; \
                             align with {members, axis}. `b` and `anchor` may name a \
                             part that is already on the sheet.",
                        "items": {"type": "object"}
                    }
                }
            }
        }
    })
}
