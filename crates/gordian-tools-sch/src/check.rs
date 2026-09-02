//! `check_schematic`: the symbol-aware lints, deterministic electrical rules,
//! completeness audit, and KiCad ERC over the live file.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use circuit_graph::netclass::is_power_net;
use gordian_runtime::AgentRuntime;
use indexmap::IndexMap;
use sch_check::model::{Block, Component, Design, NetAttrs, PinTarget};
use sch_doc::{NetSource, Netlist, PlacedPin, SchDoc, placed_pins};
use serde::Serialize;
use serde_json::{Value, json};

const COMPACT_FINDING_LIMIT: usize = 40;

/// Reduce the live sheet to the kernel model the checkers run on.
///
/// One sheet is one block: the extractor's scope is a file, and the checkers
/// only use blocks to group, never to separate connectivity.
pub(crate) fn design(doc: &SchDoc, netlist: &Netlist) -> Design {
    let pins = placed_pins(doc);
    let mut components: IndexMap<String, Component> = IndexMap::new();
    // The units of a multi-unit part are separate symbols sharing one
    // reference; they are one component, and folding them together is what
    // stops the lints seeing each unit's pins as a design of its own.
    for symbol in doc.symbols() {
        let field = |name: &str| {
            symbol
                .fields
                .get(name)
                .map(|f| f.value.clone())
                .filter(|v| !v.is_empty())
        };
        let component = components
            .entry(symbol.refdes().to_string())
            .or_insert_with(|| Component {
                part: symbol.lib_id.clone(),
                value: field("Value"),
                footprint: field("Footprint"),
                dnp: symbol.dnp,
                ..Component::default()
            });
        for pin in pins.iter().filter(|p| p.owner == symbol.uuid) {
            let target = match crate::refs::net_of(netlist, &pin.refdes, &pin.number) {
                Some(net) => PinTarget::Net(net.to_string()),
                None => PinTarget::NoConnect,
            };
            component.pins.insert(pin.number.clone(), target);
        }
    }
    let nets = netlist
        .nets
        .iter()
        .map(|net| {
            (
                net.name.clone(),
                NetAttrs {
                    power: net.source == NetSource::Power,
                    port: net.source == NetSource::Global,
                    class: None,
                },
            )
        })
        .collect();
    let mut blocks = IndexMap::new();
    blocks.insert(
        "main".to_string(),
        Block {
            note: None,
            components,
            layout: Vec::new(),
        },
    );
    Design {
        name: None,
        description: None,
        blocks,
        nets,
        lint_allow: Default::default(),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct ToolFix {
    tool: &'static str,
    args: Value,
}

#[derive(Clone, Serialize)]
struct Finding {
    classification: &'static str,
    severity: String,
    source: &'static str,
    code: String,
    message: String,
    refs: Vec<String>,
    nets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    at: Option<[f64; 2]>,
    fix: Option<ToolFix>,
    why: String,
    #[serde(skip_serializing_if = "is_false")]
    advisory: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Finding {
    fn line(&self) -> String {
        let references = if self.refs.is_empty() {
            String::new()
        } else {
            format!(" {}", self.refs.join(", "))
        };
        let nets = if self.nets.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.nets.join(", "))
        };
        let fix = self.fix.as_ref().map_or_else(
            || "null".to_string(),
            |fix| format!("{}{}", fix.tool, compact_json(&fix.args)),
        );
        format!(
            "{}[{}]{}{}: {} → fix: {fix}",
            self.severity, self.code, references, nets, self.message
        )
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).expect("serializing JSON value cannot fail")
}

struct FindingLocator<'a> {
    doc: &'a SchDoc,
    netlist: &'a Netlist,
    pins: Vec<PlacedPin>,
}

impl<'a> FindingLocator<'a> {
    fn new(doc: &'a SchDoc, netlist: &'a Netlist) -> Self {
        Self {
            doc,
            netlist,
            pins: placed_pins(doc),
        }
    }

    fn locate(
        &self,
        text: &str,
        explicit_refs: impl IntoIterator<Item = String>,
        explicit_nets: impl IntoIterator<Item = String>,
    ) -> (Vec<String>, Vec<String>, Option<[f64; 2]>) {
        let mut refs = explicit_refs.into_iter().collect::<BTreeSet<_>>();
        let mut nets = explicit_nets.into_iter().collect::<BTreeSet<_>>();

        for pin in &self.pins {
            let numbered = format!("{}.{}", pin.refdes, pin.number);
            let named = format!("{}.{}", pin.refdes, pin.name);
            let described = contains_refdes(text, &pin.refdes)
                && (contains_name(text, &format!("Pin {}", pin.number))
                    || contains_name(text, &format!("pin {}", pin.number)));
            if contains_name(text, &numbered) || contains_name(text, &named) || described {
                refs.insert(numbered);
            }
        }
        for symbol in self.doc.symbols() {
            if contains_refdes(text, symbol.refdes())
                && !refs
                    .iter()
                    .any(|found| found.starts_with(&format!("{}.", symbol.refdes())))
            {
                refs.insert(symbol.refdes().to_string());
            }
        }
        for net in &self.netlist.nets {
            if contains_name(text, &net.name) {
                nets.insert(net.name.clone());
            }
        }
        for reference in &refs {
            let Some((refdes, pin)) = reference.rsplit_once('.') else {
                continue;
            };
            if let Some(net) = crate::refs::net_of(self.netlist, refdes, pin) {
                nets.insert(net.to_string());
            }
        }
        let at = refs
            .iter()
            .find_map(|reference| self.reference_at(reference));
        (refs.into_iter().collect(), nets.into_iter().collect(), at)
    }

    fn refs_for_uuid(&self, uuid: &str) -> Vec<String> {
        let mut refs = BTreeSet::new();
        for symbol in self.doc.symbols() {
            if symbol.uuid == uuid {
                refs.insert(symbol.refdes().to_string());
            }
            for (pin, pin_uuid) in &symbol.pin_uuids {
                if pin_uuid == uuid {
                    refs.insert(format!("{}.{pin}", symbol.refdes()));
                }
            }
        }
        refs.into_iter().collect()
    }

    fn at_for_uuid(&self, uuid: &str) -> Option<[f64; 2]> {
        for symbol in self.doc.symbols() {
            if symbol.uuid == uuid {
                return Some(round_point(symbol.at.x, symbol.at.y));
            }
            if let Some((pin, _)) = symbol
                .pin_uuids
                .iter()
                .find(|(_, pin_uuid)| pin_uuid.as_str() == uuid)
                && let Some(placed) = self
                    .pins
                    .iter()
                    .find(|placed| placed.owner == symbol.uuid && placed.number == *pin)
            {
                return Some(round_point(placed.at.x, placed.at.y));
            }
        }
        None
    }

    fn reference_at(&self, reference: &str) -> Option<[f64; 2]> {
        if let Some((refdes, pin)) = reference.rsplit_once('.')
            && let Some(found) = self
                .pins
                .iter()
                .find(|found| found.refdes == refdes && found.number == pin)
        {
            return Some(round_point(found.at.x, found.at.y));
        }
        self.doc
            .symbols()
            .find(|symbol| symbol.refdes() == reference)
            .map(|symbol| round_point(symbol.at.x, symbol.at.y))
    }
}

fn round_point(x: f64, y: f64) -> [f64; 2] {
    [(x * 1000.0).round() / 1000.0, (y * 1000.0).round() / 1000.0]
}

fn contains_name(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    text.match_indices(name).any(|(start, _)| {
        let end = start + name.len();
        let boundary = |character: Option<char>| {
            character.is_none_or(|character| !character.is_ascii_alphanumeric())
        };
        boundary(text[..start].chars().next_back()) && boundary(text[end..].chars().next())
    })
}

fn contains_refdes(text: &str, reference: &str) -> bool {
    text.match_indices(reference).any(|(start, _)| {
        let end = start + reference.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        before.is_none_or(|character| !character.is_ascii_alphanumeric())
            && after.is_none_or(|character| !character.is_ascii_alphanumeric() && character != '.')
    })
}

fn strip_subject_prefix(message: String, refs: &[String], nets: &[String]) -> String {
    refs.iter()
        .chain(nets)
        .find_map(|subject| message.strip_prefix(&format!("{subject}: ")))
        .unwrap_or(&message)
        .to_string()
}

#[derive(Clone)]
struct FixPin {
    id: String,
    refdes: String,
    name: String,
    etype: String,
    net: Option<String>,
    unconnected: bool,
}

struct FixPlanner {
    pins: Vec<FixPin>,
    baseline_nets: BTreeMap<String, String>,
    parts: BTreeMap<String, String>,
    rotations: BTreeMap<String, f64>,
    default_footprints: BTreeMap<String, String>,
}

impl FixPlanner {
    fn new(
        doc: &SchDoc,
        netlist: &Netlist,
        baseline: Option<&Netlist>,
        ctx: &AgentRuntime,
    ) -> Self {
        let pins = placed_pins(doc)
            .into_iter()
            .map(|pin| {
                let id = format!("{}.{}", pin.refdes, pin.number);
                FixPin {
                    net: crate::refs::net_of(netlist, &pin.refdes, &pin.number).map(str::to_string),
                    unconnected: netlist
                        .unconnected
                        .iter()
                        .any(|loose| loose.refdes == pin.refdes && loose.pin == pin.number),
                    id,
                    refdes: pin.refdes,
                    name: pin.name,
                    etype: pin.etype,
                }
            })
            .collect();
        let baseline_nets = baseline
            .into_iter()
            .flat_map(|netlist| &netlist.nets)
            .flat_map(|net| {
                net.pins
                    .iter()
                    .map(move |pin| (format!("{}.{}", pin.refdes, pin.pin), net.name.clone()))
            })
            .collect();
        let parts = doc
            .symbols()
            .map(|symbol| (symbol.refdes().to_string(), symbol.lib_id.clone()))
            .collect::<BTreeMap<_, _>>();
        let rotations = doc
            .symbols()
            .map(|symbol| (symbol.refdes().to_string(), symbol.at.rot))
            .collect();
        let default_footprints = parts
            .iter()
            .filter_map(|(reference, part)| {
                ctx.provider()
                    .symbol(part)
                    .and_then(|symbol| symbol.footprint)
                    .filter(|footprint| !footprint.is_empty())
                    .map(|footprint| (reference.clone(), footprint))
            })
            .collect();
        Self {
            pins,
            baseline_nets,
            parts,
            rotations,
            default_footprints,
        }
    }

    fn plan(&self, finding: &mut Finding, ctx: &AgentRuntime) {
        let code = finding.code.to_ascii_lowercase().replace('_', "-");
        let message = finding.message.to_ascii_lowercase();
        let planned =
            if code == "library-no-connect-wired" || message.contains("library no-connect pin") {
                self.library_no_connect(finding)
            } else if is_power_finding(&code, &message) {
                self.power(finding)
            } else if is_output_conflict(&code, &message) {
                self.output_conflict(finding)
            } else if code.contains("polarity")
                || (message.contains("reversed") && message.contains("led"))
            {
                self.polarity(finding)
            } else if is_assignable_footprint(&code, &message) {
                self.footprint(finding, ctx)
            } else if is_connection_finding(&code, &message) {
                self.connection(finding)
            } else {
                None
            };
        if let Some((fix, why)) = planned {
            finding.fix = Some(fix);
            finding.why = why;
        } else if is_connection_finding(&code, &message) {
            finding.why =
                "No baseline, same-net, or same-function endpoint proves the intended connection."
                    .to_string();
        } else if is_output_conflict(&code, &message) {
            finding.why =
                "The turn baseline does not identify exactly one newly added driver to disconnect."
                    .to_string();
        } else if is_assignable_footprint(&code, &message) {
            finding.why =
                "No installed footprint matches both the symbol family and its pad numbers."
                    .to_string();
        } else if code.contains("polarity") {
            finding.why = "The finding does not identify a two-pin symbol whose assignments can be swapped safely.".to_string();
        } else if finding.why.is_empty() {
            finding.why = "The finding does not identify a safe one-call repair.".to_string();
        }
    }

    fn pin(&self, id: &str) -> Option<&FixPin> {
        self.pins.iter().find(|pin| pin.id == id)
    }

    fn affected_pin<'a>(&'a self, finding: &Finding) -> Option<&'a FixPin> {
        finding
            .refs
            .iter()
            .filter_map(|reference| self.pin(reference))
            .next()
            .or_else(|| {
                finding.refs.iter().find_map(|reference| {
                    let candidates = self
                        .pins
                        .iter()
                        .filter(|pin| pin.refdes == *reference)
                        .collect::<Vec<_>>();
                    candidates
                        .iter()
                        .copied()
                        .find(|pin| pin.name != "~" && contains_name(&finding.message, &pin.name))
                        .or_else(|| {
                            let loose = candidates
                                .iter()
                                .copied()
                                .filter(|pin| pin.unconnected)
                                .collect::<Vec<_>>();
                            (loose.len() == 1).then(|| loose[0])
                        })
                })
            })
    }

    fn power(&self, finding: &Finding) -> Option<(ToolFix, String)> {
        let affected = self.affected_pin(finding);
        let net = finding
            .nets
            .iter()
            .find(|net| is_power_net(net))
            .cloned()
            .or_else(|| {
                affected
                    .filter(|pin| is_power_net(&pin.name))
                    .map(|pin| pin.name.clone())
            })
            .or_else(|| affected.and_then(|pin| pin.net.clone()))
            .or_else(|| affected.map(|pin| pin.name.clone()))?;
        if is_power_net(&net) {
            let anchor = self
                .pins
                .iter()
                .filter(|pin| pin.net.as_deref() == Some(&net))
                .filter(|pin| is_power_input(&pin.etype))
                .min_by(|left, right| left.id.cmp(&right.id))
                .or(affected)?;
            return Some((
                ToolFix {
                    tool: "add_power",
                    args: json!({"net": net, "pin": anchor.id}),
                },
                format!(
                    "One rail symbol on {} drives every power-input pin on {net}.",
                    anchor.id
                ),
            ));
        }
        let driver = self
            .pins
            .iter()
            .filter(|pin| pin.net.as_deref() == Some(&net))
            .find(|pin| is_output(&pin.etype) || is_power_output(&pin.etype));
        if let (Some(load), Some(driver)) = (affected, driver) {
            return Some((
                ToolFix {
                    tool: "connect",
                    args: json!({"from": load.id, "to": driver.id}),
                },
                format!(
                    "{} is the output pin that should drive {}.",
                    driver.id, load.id
                ),
            ));
        }
        Some((
            ToolFix {
                tool: "place_parts",
                args: json!({
                    "block": "erc_repair",
                    "parts": [{"part": "power:PWR_FLAG", "pins": {"1": net}}]
                }),
            },
            format!(
                "No output or power-output pin drives {net}; this flag declares the rail, but the intended regulator or connector output should be used when known."
            ),
        ))
    }

    fn connection(&self, finding: &Finding) -> Option<(ToolFix, String)> {
        let from = self.affected_pin(finding)?;
        if is_library_no_connect(&from.etype) || is_unused_output(from, finding) {
            return Some((
                ToolFix {
                    tool: "no_connect",
                    args: json!({"pin": from.id}),
                },
                if is_library_no_connect(&from.etype) {
                    format!("{} is declared NC by the symbol library.", from.id)
                } else {
                    format!(
                        "{} is an unused output, so it should be explicitly no-connect.",
                        from.id
                    )
                },
            ));
        }
        let desired_net = self
            .baseline_nets
            .get(&from.id)
            .or_else(|| finding.nets.first())
            .or(from.net.as_ref());
        let to = desired_net
            .and_then(|net| {
                self.pins
                    .iter()
                    .filter(|pin| pin.id != from.id)
                    .filter(|pin| {
                        pin.net.as_ref() == Some(net)
                            || self.baseline_nets.get(&pin.id) == Some(net)
                    })
                    .min_by(|left, right| left.id.cmp(&right.id))
            })
            .or_else(|| self.intent_matched_pin(from))?;
        let (from, to) = if from.unconnected && to.unconnected && from.id > to.id {
            (to, from)
        } else {
            (from, to)
        };
        Some((
            ToolFix {
                tool: "connect",
                args: json!({"from": from.id, "to": to.id}),
            },
            match desired_net {
                Some(net) => format!(
                    "{} restores {} to its matching {net} endpoint.",
                    to.id, from.id
                ),
                None => format!(
                    "{} has the same explicit pin function as {}.",
                    to.id, from.id
                ),
            },
        ))
    }

    fn intent_matched_pin<'a>(&'a self, from: &FixPin) -> Option<&'a FixPin> {
        let intended_net = if circuit_graph::netclass::is_ground(&from.name) {
            Some("GND")
        } else if is_power_net(&from.name) {
            Some(from.name.as_str())
        } else {
            None
        };
        if let Some(net) = intended_net {
            return self
                .pins
                .iter()
                .filter(|pin| pin.refdes != from.refdes)
                .filter(|pin| pin.net.as_deref() == Some(net))
                .min_by(|left, right| left.id.cmp(&right.id));
        }
        if from.name == "~" || from.name.is_empty() {
            return None;
        }
        self.pins
            .iter()
            .filter(|pin| pin.refdes != from.refdes && pin.unconnected)
            .filter(|pin| !is_library_no_connect(&pin.etype))
            .filter(|pin| pin.name.eq_ignore_ascii_case(&from.name))
            .min_by(|left, right| left.id.cmp(&right.id))
    }

    fn output_conflict(&self, finding: &Finding) -> Option<(ToolFix, String)> {
        let net = finding.nets.first()?;
        let mut drivers = self
            .pins
            .iter()
            .filter(|pin| pin.net.as_deref() == Some(net))
            .filter(|pin| is_output(&pin.etype) || is_power_output(&pin.etype))
            .collect::<Vec<_>>();
        drivers.sort_by(|left, right| left.id.cmp(&right.id));
        let newcomers = drivers
            .iter()
            .copied()
            .filter(|driver| self.baseline_nets.get(&driver.id).map(String::as_str) != Some(net))
            .collect::<Vec<_>>();
        let [disconnect] = newcomers.as_slice() else {
            return None;
        };
        Some((
            ToolFix {
                tool: "delete_wires",
                args: json!({"pins": [disconnect.id.clone()]}),
            },
            format!(
                "Disconnecting {} leaves a single driver on {net}.",
                disconnect.id
            ),
        ))
    }

    fn polarity(&self, finding: &Finding) -> Option<(ToolFix, String)> {
        let reference = finding
            .refs
            .iter()
            .map(|reference| reference.split('.').next().unwrap_or(reference))
            .find(|reference| self.rotations.contains_key(*reference))?;
        let mut pins = self
            .pins
            .iter()
            .filter(|pin| pin.refdes == reference)
            .filter_map(|pin| pin.id.rsplit_once('.').map(|(_, number)| number.to_owned()))
            .collect::<Vec<_>>();
        pins.sort();
        pins.dedup();
        if pins.len() != 2 {
            return None;
        }
        Some((
            ToolFix {
                tool: "move_symbols",
                args: json!({
                    "moves": [{
                        "ref": reference,
                        "rot": (self.rotations[reference] + 180.0).rem_euclid(360.0)
                    }]
                }),
            },
            format!("Rotating {reference} 180 degrees swaps its two fixed net positions."),
        ))
    }

    fn footprint(&self, finding: &Finding, ctx: &AgentRuntime) -> Option<(ToolFix, String)> {
        let reference = finding
            .refs
            .iter()
            .map(|reference| reference.split('.').next().unwrap_or(reference))
            .find(|reference| self.parts.contains_key(*reference))?;
        let footprint = self
            .suggested_footprint(&finding.message)
            .and_then(|footprint| self.verified_footprint(reference, footprint, ctx))
            .or_else(|| {
                self.default_footprints
                    .get(reference)
                    .and_then(|footprint| self.verified_footprint(reference, footprint, ctx))
            })
            .or_else(|| {
                conventional_footprint(&self.parts[reference])
                    .and_then(|footprint| self.verified_footprint(reference, footprint, ctx))
            })
            .or_else(|| {
                let query = format!("{} {}", self.parts[reference], finding.message);
                ctx.footprint_catalog()
                    .ok()?
                    .search(query)
                    .into_iter()
                    .find_map(|hit| self.verified_footprint(reference, &hit.id.to_string(), ctx))
            })?;
        Some(footprint_assignment(
            reference,
            &footprint,
            &self.parts[reference],
        ))
    }

    fn suggested_footprint<'a>(&self, message: &'a str) -> Option<&'a str> {
        message
            .split_once("did you mean ")
            .map(|(_, candidates)| candidates)
            .and_then(|candidates| candidates.split([',', '?']).next())
            .map(str::trim)
            .filter(|candidate| candidate.contains(':'))
    }

    fn verified_footprint(
        &self,
        reference: &str,
        candidate: &str,
        ctx: &AgentRuntime,
    ) -> Option<String> {
        let part = self.parts.get(reference)?;
        let pins = self
            .pins
            .iter()
            .filter(|pin| pin.refdes == reference)
            .filter_map(|pin| pin.id.rsplit_once('.').map(|(_, number)| number));
        gordian_runtime::footprint_compat::footprint_compatibility_for_pins(
            ctx, part, pins, candidate,
        )
        .ok()
        .filter(|verdict| verdict.compatible)
        .map(|_| candidate.to_owned())
    }

    fn library_no_connect(&self, finding: &Finding) -> Option<(ToolFix, String)> {
        let pin = self.affected_pin(finding)?;
        let functional = self
            .pins
            .iter()
            .filter(|candidate| candidate.refdes == pin.refdes)
            .filter(|candidate| !is_library_no_connect(&candidate.etype) && candidate.unconnected)
            .collect::<Vec<_>>();
        Some((
            ToolFix {
                tool: "delete_wires",
                args: json!({"pins": [pin.id.clone()]}),
            },
            (functional.len() == 1)
                .then_some(functional[0])
                .map_or_else(
                    || format!("{} is a library NC pin and must not be wired.", pin.id),
                    |functional| {
                        format!(
                            "{} is a library NC pin; use functional pin {} instead.",
                            pin.id, functional.id
                        )
                    },
                ),
        ))
    }
}

fn is_power_finding(code: &str, message: &str) -> bool {
    code.contains("power-pin-not-driven")
        || code == "power-pin-unconnected"
        || code == "unsourced-power-net"
        || message.contains("power input") && message.contains("not driven")
}

fn is_connection_finding(code: &str, message: &str) -> bool {
    code.contains("dangling")
        || code == "single-pin-net"
        || code.contains("pin-not-connected")
        || code.contains("wire-dangling")
        || code == "floating-input"
        || message.contains("unconnected input")
}

fn is_output_conflict(code: &str, message: &str) -> bool {
    code == "output-short"
        || code.contains("output-to-output")
        || message.contains("conflicting drivers")
        || message.contains("multiple outputs")
}

fn is_assignable_footprint(code: &str, message: &str) -> bool {
    matches!(
        code,
        "footprint-unknown" | "footprint-id" | "missing-footprint"
    ) || message.contains("missing footprint")
}

fn is_power_input(etype: &str) -> bool {
    matches!(etype, "power_in" | "power_input")
}

fn is_power_output(etype: &str) -> bool {
    matches!(etype, "power_out" | "power_output")
}

fn is_output(etype: &str) -> bool {
    matches!(etype, "output" | "tri_state")
}

fn is_library_no_connect(etype: &str) -> bool {
    matches!(etype, "no_connect" | "no-connect")
}

fn is_unused_output(pin: &FixPin, finding: &Finding) -> bool {
    is_output(&pin.etype)
        && (pin.unconnected
            || finding
                .message
                .to_ascii_lowercase()
                .contains("unused output"))
}

fn conventional_footprint(part: &str) -> Option<&'static str> {
    let symbol = part.split_once(':').map_or(part, |(_, symbol)| symbol);
    if symbol == "R" {
        Some("Resistor_SMD:R_0805_2012Metric")
    } else if symbol == "C" {
        Some("Capacitor_SMD:C_0805_2012Metric")
    } else if symbol.starts_with("LED") {
        Some("LED_SMD:LED_0805_2012Metric")
    } else {
        None
    }
}

fn footprint_assignment(reference: &str, footprint: &str, part: &str) -> (ToolFix, String) {
    (
        ToolFix {
            tool: "assign_footprints",
            args: json!({"assignments": [{"reference": reference, "footprint": footprint}]}),
        },
        format!("{footprint} is the best installed catalog match for {part}."),
    )
}

fn finding_message(message: &str) -> String {
    let message = message.trim().trim_start_matches("- ");
    match message.rsplit_once(" — ") {
        Some((message, _)) => message.to_string(),
        None => message.to_string(),
    }
}

fn diagnostic_finding(
    locator: &FindingLocator<'_>,
    diagnostic: &sch_check::Diagnostic,
    source: &'static str,
) -> Finding {
    let severity = match diagnostic.severity {
        sch_check::Severity::Error => "error",
        sch_check::Severity::Warning => "warning",
    };
    let message = finding_message(&diagnostic.message);
    let (refs, nets, at) = locator.locate(
        &message,
        diagnostic.subjects.refs.iter().cloned(),
        diagnostic.subjects.nets.iter().cloned(),
    );
    let message = strip_subject_prefix(message, &refs, &nets);
    Finding {
        classification: "introduced",
        severity: severity.to_string(),
        source,
        code: diagnostic.code.to_string(),
        message,
        refs,
        nets,
        at,
        fix: None,
        why: diagnostic
            .suggestion
            .clone()
            .unwrap_or_else(|| "No safe one-call repair is known for this finding.".to_string()),
        advisory: diagnostic.severity == sch_check::Severity::Warning,
    }
}

fn erc_finding(locator: &FindingLocator<'_>, violation: &kicad::Violation) -> Finding {
    let mut explicit_refs = BTreeSet::new();
    let mut explicit_at = None;
    let mut item_text = String::new();
    for item in &violation.items {
        item_text.push(' ');
        item_text.push_str(&item.description);
        if let Some(uuid) = &item.uuid {
            explicit_refs.extend(locator.refs_for_uuid(uuid));
            explicit_at = explicit_at.or_else(|| locator.at_for_uuid(uuid));
        }
    }
    let searchable = format!("{}{}", violation.description, item_text);
    let message = finding_message(&violation.description);
    let (refs, nets, inferred_at) = locator.locate(&searchable, explicit_refs, []);
    let reported_at = violation
        .items
        .iter()
        .find_map(|item| item.pos)
        .map(|position| round_point(position.x, position.y));
    Finding {
        classification: "introduced",
        severity: violation.severity.clone(),
        source: "kicad_erc",
        code: violation.kind.clone(),
        message,
        refs,
        nets,
        at: explicit_at.or(inferred_at).or(reported_at),
        fix: None,
        why: "No safe one-call repair is known for this finding.".to_string(),
        advisory: violation.severity != "error",
    }
}

fn pad_clause(prefix: &str, pads: &[String]) -> String {
    if pads.is_empty() {
        String::new()
    } else {
        format!("{prefix}{})", pads.join(", "))
    }
}

fn add_rendered_findings(report: &mut Value, findings: &[Finding], detail: bool) {
    let shown = if detail {
        findings.len()
    } else if findings.len() > COMPACT_FINDING_LIMIT {
        COMPACT_FINDING_LIMIT - 1
    } else {
        findings.len()
    };
    let introduced = findings
        .iter()
        .filter(|finding| finding.classification == "introduced")
        .count();
    let pre_existing = findings.len() - introduced;
    let groups = fix_groups(findings);
    let mut lines = vec![format!(
        "{} findings, {} fixes; {introduced} introduced, {pre_existing} pre-existing",
        findings.len(),
        groups.len()
    )];
    lines.extend(findings[..shown].iter().map(Finding::line));
    let omitted = findings.len() - shown;
    if omitted > 0 {
        lines.push(format!(
            "+{omitted} more — rerun check_schematic with {{\"detail\":true}}"
        ));
        report["findings_omitted"] = json!(omitted);
    }
    report["findings"] = json!(&findings[..shown]);
    report["fix_count"] = json!(groups.len());
    report["fix_groups"] = json!(groups);
    report["diagnostics"] = json!(lines);
    report["text"] = json!(
        report["diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn fix_groups(findings: &[Finding]) -> Vec<Value> {
    let mut groups: Vec<(ToolFix, Vec<usize>)> = Vec::new();
    for (index, finding) in findings.iter().enumerate() {
        let Some(fix) = &finding.fix else {
            continue;
        };
        if let Some((_, indexes)) = groups.iter_mut().find(|(group, _)| group == fix) {
            indexes.push(index);
        } else {
            groups.push((fix.clone(), vec![index]));
        }
    }
    groups
        .into_iter()
        .map(|(fix, finding_indexes)| {
            let codes = finding_indexes
                .iter()
                .map(|&index| findings[index].code.clone())
                .collect::<BTreeSet<_>>();
            json!({
                "fix": fix,
                "closes": finding_indexes.len(),
                "finding_indexes": finding_indexes,
                "finding_codes": codes,
            })
        })
        .collect()
}

fn inherit_duplicate_footprint_fixes(findings: &mut [Finding]) {
    let fixes = findings
        .iter()
        .filter(|finding| finding.code == "footprint-unknown")
        .filter_map(|finding| {
            let reference = finding.refs.first()?.split('.').next()?.to_owned();
            Some((reference, (finding.fix.clone()?, finding.why.clone())))
        })
        .collect::<BTreeMap<_, _>>();
    for finding in findings {
        if finding.code != "footprint_link_issues" || finding.fix.is_some() {
            continue;
        }
        let Some(reference) = finding
            .refs
            .first()
            .and_then(|reference| reference.split('.').next())
        else {
            continue;
        };
        if let Some((fix, why)) = fixes.get(reference) {
            finding.fix = Some(fix.clone());
            finding.why = why.clone();
        }
    }
}

struct Inspection {
    doc: SchDoc,
    netlist: Netlist,
    gaps: Vec<sch_check::completeness::Gap>,
    findings: Vec<Finding>,
    local_errors: usize,
    local_warnings: usize,
    erc: std::result::Result<kicad::ErcReport, String>,
}

fn live_footprint_mismatches(
    ctx: &AgentRuntime,
    doc: &SchDoc,
    netlist: &Netlist,
) -> Result<Vec<gordian_runtime::footprint_compat::FootprintPinMismatch>> {
    let mut seen = BTreeSet::new();
    let mut mismatches = Vec::new();
    for symbol in doc.symbols() {
        if ctx.provider().symbol(&symbol.lib_id).is_none() {
            continue;
        }
        let reference = symbol.refdes();
        if !seen.insert(reference.to_owned()) {
            continue;
        }
        let Some(footprint) = symbol
            .fields
            .get("Footprint")
            .map(|field| field.value.as_str())
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let ignored_pins = netlist
            .no_connect
            .iter()
            .filter(|pin| pin.refdes == reference)
            .map(|pin| pin.pin.clone())
            .collect();
        if let Some(mismatch) = gordian_runtime::footprint_compat::assignment_pin_mismatch_ignoring(
            ctx,
            reference,
            &symbol.lib_id,
            footprint,
            &ignored_pins,
        )? {
            mismatches.push(mismatch);
        }
    }
    Ok(mismatches)
}

fn inspect_schematic(path: &Path, ctx: &AgentRuntime) -> Result<Inspection> {
    let doc = SchDoc::read(path).with_context(|| format!("reading {}", path.display()))?;
    let netlist = sch_doc::connect::extract(&doc);
    let design = design(&doc, &netlist);
    let locator = FindingLocator::new(&doc, &netlist);
    let gaps = sch_check::completeness::audit(&design, ctx.provider());
    let mut findings = Vec::new();

    let lint = sch_check::lint::lint(&design, ctx.provider());
    findings.extend(
        lint.0
            .iter()
            .map(|diagnostic| diagnostic_finding(&locator, diagnostic, "lint")),
    );
    for defect in sch_check::erc::defects(&design, ctx.provider()) {
        let diagnostic = if defect.blocking {
            sch_check::Diagnostic::error(defect.code, defect.line)
        } else {
            sch_check::Diagnostic::warning(defect.code, defect.line)
        }
        .with_refs(defect.refs)
        .with_nets(defect.nets);
        findings.push(diagnostic_finding(
            &locator,
            &diagnostic,
            "deterministic_erc",
        ));
    }
    for problem in gordian_runtime::footprint_compat::unresolvable_footprints(ctx, &design)? {
        let diagnostic = if problem.malformed {
            sch_check::Diagnostic::error("footprint-id", problem.message)
        } else {
            sch_check::Diagnostic::warning("footprint-unknown", problem.message)
        };
        findings.push(diagnostic_finding(&locator, &diagnostic, "footprint"));
    }
    for mismatch in live_footprint_mismatches(ctx, &doc, &netlist)? {
        let message = format!(
            "symbol `{}` and footprint `{}` do not agree{}{}{}",
            mismatch.symbol,
            mismatch.footprint,
            pad_clause(" (missing pads: ", &mismatch.missing_pads),
            pad_clause(" (extra electrical pins: ", &mismatch.extra_pins),
            mismatch
                .polarity_mismatch
                .as_deref()
                .map(|why| format!(" ({why})"))
                .unwrap_or_default(),
        );
        let (fix, why) = if let Some(suggestion) = mismatch
            .suggestion
            .as_deref()
            .filter(|_| mismatch.suggestion_compatible)
        {
            footprint_assignment(&mismatch.reference, suggestion, &mismatch.symbol)
        } else if !mismatch.missing_pads.is_empty() {
            (
                ToolFix {
                    tool: "no_connect",
                    args: json!({
                        "pins": mismatch.missing_pads.iter().map(|pin| {
                            format!("{}.{}", mismatch.reference, pin)
                        }).collect::<Vec<_>>()
                    }),
                },
                format!(
                    "The footprint has no pad for symbol pin(s) {}; mark them no-connect only if the package intentionally omits them.",
                    mismatch.missing_pads.join(", ")
                ),
            )
        } else if let Some(symbol) = mismatch.symbol_suggestion.as_deref() {
            (
                ToolFix {
                    tool: "swap_symbol",
                    args: json!({"ref": mismatch.reference, "lib_id": symbol}),
                },
                format!(
                    "No compatible footprint exists in the catalog for {}; `{symbol}` matches the desired package.",
                    mismatch.symbol
                ),
            )
        } else {
            let (refs, nets, at) = locator.locate(
                &message,
                [mismatch.reference.clone()],
                std::iter::empty::<String>(),
            );
            findings.push(Finding {
                classification: "introduced",
                severity: "error".to_string(),
                source: "footprint",
                code: "footprint-pins".to_string(),
                message: format!(
                    "{message}; no compatible footprint exists in the installed catalog"
                ),
                refs,
                nets,
                at,
                fix: None,
                why: "Swap to a symbol variant whose pins match the desired package.".to_string(),
                advisory: false,
            });
            continue;
        };
        let (refs, nets, at) = locator.locate(
            &message,
            [mismatch.reference.clone()],
            std::iter::empty::<String>(),
        );
        findings.push(Finding {
            classification: "introduced",
            severity: "error".to_string(),
            source: "footprint",
            code: "footprint-pins".to_string(),
            message,
            refs,
            nets,
            at,
            fix: Some(fix),
            why,
            advisory: false,
        });
    }
    for warning in &netlist.warnings {
        let message = finding_message(warning);
        let (refs, nets, at) = locator.locate(&message, [], []);
        findings.push(Finding {
            classification: "introduced",
            severity: "warning".to_string(),
            source: "extractor",
            code: "extractor".to_string(),
            message,
            refs,
            nets,
            at,
            fix: None,
            why: "The extractor warning does not identify a safe edit.".to_string(),
            advisory: true,
        });
    }
    for gap in &gaps {
        let refs = gap.refdes.iter().cloned();
        let nets = gap.net.iter().cloned();
        let message = format!(
            "{} support circuitry is missing",
            gap.kind.replace('_', " ")
        );
        let (refs, nets, at) = locator.locate(&message, refs, nets);
        findings.push(Finding {
            classification: "introduced",
            severity: "warning".to_string(),
            source: "completeness",
            code: gap.kind.clone(),
            message,
            refs,
            nets,
            at,
            fix: None,
            why: gap.suggestion.clone(),
            advisory: true,
        });
    }

    let local_errors = findings
        .iter()
        .filter(|finding| finding.source != "completeness" && finding.severity == "error")
        .count();
    let local_warnings = findings
        .iter()
        .filter(|finding| finding.source != "completeness" && finding.severity == "warning")
        .count();
    let erc = ctx
        .env()
        .erc(path)
        .map_err(|error| format!("running ERC: {error}"));
    if let Ok(erc) = &erc {
        findings.extend(
            erc.violations
                .iter()
                .map(|violation| erc_finding(&locator, violation)),
        );
    }
    Ok(Inspection {
        doc,
        netlist,
        gaps,
        findings,
        local_errors,
        local_warnings,
        erc,
    })
}

fn inspect_turn_baseline(ctx: &AgentRuntime) -> Result<(bool, Option<Inspection>)> {
    let Some(baseline) = ctx.turn_baseline()? else {
        return Ok((false, None));
    };
    if baseline.file(ctx.project_dir(), ctx.sch_path()).is_none() {
        return Ok((true, None));
    }
    let directory = tempfile::tempdir().context("creating turn-start inspection directory")?;
    for (relative, bytes) in &baseline.files {
        let path = directory.path().join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, bytes)
            .with_context(|| format!("preparing turn-start file {}", relative.display()))?;
    }
    let relative = ctx
        .sch_path()
        .strip_prefix(ctx.project_dir())
        .context("schematic is outside project")?;
    inspect_schematic(&directory.path().join(relative), ctx)
        .map(|inspection| (true, Some(inspection)))
}

fn finding_key(finding: &Finding) -> (String, Vec<String>, Vec<String>) {
    (
        finding.code.clone(),
        finding.refs.clone(),
        finding.nets.clone(),
    )
}

fn classify_findings(findings: &mut [Finding], baseline: &[Finding]) {
    let mut available = BTreeMap::new();
    for finding in baseline {
        *available.entry(finding_key(finding)).or_insert(0usize) += 1;
    }
    for finding in findings {
        let count = available.entry(finding_key(finding)).or_default();
        if *count > 0 {
            finding.classification = "pre_existing";
            *count -= 1;
        }
    }
}

/// Lint and run ERC, classifying findings against this turn's first snapshot.
///
/// `ok` and `erc_clean` consider introduced errors only; inherited errors do
/// not block completion of an otherwise clean focused edit.
pub fn check_schematic(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let detail = input.get("detail").and_then(Value::as_bool) == Some(true);
    let mut inspection = inspect_schematic(ctx.sch_path(), ctx)?;
    let (has_baseline, baseline_inspection) = inspect_turn_baseline(ctx)?;
    if let Some(baseline) = &baseline_inspection {
        classify_findings(&mut inspection.findings, &baseline.findings);
    }
    let planner = FixPlanner::new(
        &inspection.doc,
        &inspection.netlist,
        baseline_inspection
            .as_ref()
            .map(|baseline| &baseline.netlist),
        ctx,
    );
    for finding in &mut inspection.findings {
        planner.plan(finding, ctx);
    }
    inherit_duplicate_footprint_fixes(&mut inspection.findings);
    inspection.findings.sort_by_key(|finding| {
        (
            finding.classification != "introduced",
            finding.severity != "error",
        )
    });
    let introduced = inspection
        .findings
        .iter()
        .filter(|finding| finding.classification == "introduced")
        .count();
    let pre_existing = inspection.findings.len() - introduced;
    let introduced_errors = inspection
        .findings
        .iter()
        .filter(|finding| finding.classification == "introduced" && finding.severity == "error")
        .count();
    let introduced_erc_errors = inspection
        .findings
        .iter()
        .filter(|finding| {
            finding.classification == "introduced"
                && finding.source == "kicad_erc"
                && finding.severity == "error"
        })
        .count();
    let mut report = json!({
        "ok": introduced_errors == 0,
        "baseline": has_baseline.then_some("turn-start"),
        "introduced": introduced,
        "pre_existing": pre_existing,
        "detail": detail,
        "checks": {
            "errors": inspection.local_errors,
            "warnings": inspection.local_warnings,
        },
        "extractor_warnings": inspection.netlist.warnings,
        "unconnected_pins": inspection.netlist.unconnected.iter().map(crate::refs::label).collect::<Vec<_>>(),
        "completeness": {
            "warnings": inspection.gaps.len(),
            "gaps": inspection.gaps,
        },
    });

    match &inspection.erc {
        Ok(erc) => {
            let errors = erc.error_count();
            let warnings = erc.warning_count();
            report["errors"] = json!(errors);
            report["warnings"] = json!(warnings);
            report["erc"] = json!({
                "errors": errors,
                "warnings": warnings,
                "findings": erc.violations.len(),
            });
            report["erc_clean"] = json!(introduced_erc_errors == 0);
            if introduced_errors == 0 {
                report["message"] = if pre_existing > 0 {
                    json!(format!(
                        "no introduced errors; {pre_existing} pre-existing finding(s) remain"
                    ))
                } else if inspection.gaps.is_empty() {
                    json!(
                        "schematic is clean and complete by deterministic rules; placement is final"
                    )
                } else {
                    json!("schematic is electrically clean; completeness findings are advisory")
                };
            }
        }
        Err(error) => {
            report["ok"] = json!(false);
            report["erc_clean"] = json!(false);
            report["erc"] = json!({ "error": error });
        }
    }
    report["finding_counts"] = json!({
        "errors": inspection.findings.iter().filter(|finding| finding.severity == "error").count(),
        "warnings": inspection.findings.iter().filter(|finding| finding.severity == "warning").count(),
        "exclusions": inspection.findings.iter().filter(|finding| finding.severity == "exclusion").count(),
        "introduced": introduced,
        "pre_existing": pre_existing,
        "introduced_errors": introduced_errors,
    });
    add_rendered_findings(&mut report, &inspection.findings, detail);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(id: &str, name: &str, etype: &str, net: Option<&str>, _x: f64) -> FixPin {
        FixPin {
            id: id.to_owned(),
            refdes: id.split('.').next().unwrap().to_owned(),
            name: name.to_owned(),
            etype: etype.to_owned(),
            net: net.map(str::to_owned),
            unconnected: net.is_none(),
        }
    }

    fn planner(pins: Vec<FixPin>) -> FixPlanner {
        FixPlanner {
            pins,
            baseline_nets: BTreeMap::new(),
            parts: BTreeMap::new(),
            rotations: BTreeMap::new(),
            default_footprints: BTreeMap::new(),
        }
    }

    fn finding(code: &str, refs: &[&str], nets: &[&str], message: &str) -> Finding {
        Finding {
            classification: "introduced",
            severity: "error".to_owned(),
            source: "test",
            code: code.to_owned(),
            message: message.to_owned(),
            refs: refs.iter().map(|value| (*value).to_owned()).collect(),
            nets: nets.iter().map(|value| (*value).to_owned()).collect(),
            at: None,
            fix: None,
            why: "No safe one-call repair is known for this finding.".to_owned(),
            advisory: false,
        }
    }

    fn warning(code: &str, reference: &str, at: [f64; 2]) -> Finding {
        Finding {
            classification: "introduced",
            severity: "warning".to_owned(),
            source: "test",
            code: code.to_owned(),
            message: "warning".to_owned(),
            refs: vec![reference.to_owned()],
            nets: vec!["NET".to_owned()],
            at: Some(at),
            fix: Some(ToolFix {
                tool: "connect",
                args: json!({"from": reference, "to": "U1.1"}),
            }),
            why: "test repair".to_owned(),
            advisory: true,
        }
    }

    #[test]
    fn edit_classifies_one_introduced_and_two_pre_existing_warnings() {
        let baseline = vec![
            warning("first", "R1", [1.0, 1.0]),
            warning("second", "R2", [2.0, 2.0]),
        ];
        let mut current = vec![
            warning("first", "R1", [10.0, 10.0]),
            warning("second", "R2", [2.0, 2.0]),
            warning("third", "R3", [3.0, 3.0]),
        ];

        classify_findings(&mut current, &baseline);

        assert_eq!(
            current
                .iter()
                .filter(|finding| finding.classification == "introduced")
                .count(),
            1
        );
        assert_eq!(
            current
                .iter()
                .filter(|finding| finding.classification == "pre_existing")
                .count(),
            2
        );
    }

    #[test]
    fn restored_baseline_has_no_introduced_warnings() {
        let baseline = vec![
            warning("first", "R1", [1.0, 1.0]),
            warning("second", "R2", [2.0, 2.0]),
        ];
        let mut restored = vec![
            warning("first", "R1", [1.0, 1.0]),
            warning("second", "R2", [2.0, 2.0]),
        ];

        classify_findings(&mut restored, &baseline);

        assert!(
            restored
                .iter()
                .all(|finding| finding.classification == "pre_existing")
        );
    }

    #[test]
    fn fresh_sheet_without_baseline_has_only_introduced_warnings() {
        let mut findings = vec![
            warning("first", "R1", [1.0, 1.0]),
            warning("second", "R2", [2.0, 2.0]),
            warning("third", "R3", [3.0, 3.0]),
        ];

        classify_findings(&mut findings, &[]);

        assert!(
            findings
                .iter()
                .all(|finding| finding.classification == "introduced")
        );
    }

    #[test]
    fn power_findings_on_one_rail_share_one_exact_add_power_fix() {
        let planner = planner(vec![
            pin("U1.8", "VSS", "power_in", Some("GND"), 10.0),
            pin("U2.4", "GND", "power_in", Some("GND"), 20.0),
            pin("U3.1", "GND", "power_in", Some("GND"), 30.0),
        ]);
        let expected = ToolFix {
            tool: "add_power",
            args: json!({"net": "GND", "pin": "U1.8"}),
        };
        let findings = ["U1.8", "U2.4", "U3.1"].map(|reference| {
            let mut finding = finding(
                "power_pin_not_driven",
                &[reference],
                &["GND"],
                "power input not driven",
            );
            let (fix, why) = planner.power(&finding).unwrap();
            finding.fix = Some(fix);
            finding.why = why;
            finding
        });

        assert!(
            findings
                .iter()
                .all(|finding| finding.fix.as_ref() == Some(&expected))
        );
        assert_eq!(fix_groups(&findings).len(), 1);
        assert_eq!(fix_groups(&findings)[0]["closes"], 3);
    }

    #[test]
    fn single_pin_net_reconnects_to_the_baseline_endpoint_exactly() {
        let mut planner = planner(vec![
            pin("R5.2", "~", "passive", None, 10.0),
            pin("U1.3", "IN", "input", None, 80.0),
        ]);
        planner.baseline_nets = BTreeMap::from([
            ("R5.2".to_owned(), "AUDIO_IN".to_owned()),
            ("U1.3".to_owned(), "AUDIO_IN".to_owned()),
        ]);
        let finding = finding(
            "single-pin-net",
            &["R5.2"],
            &["AUDIO_IN"],
            "net reaches only R5.2",
        );

        assert_eq!(
            planner.connection(&finding).unwrap().0,
            ToolFix {
                tool: "connect",
                args: json!({"from": "R5.2", "to": "U1.3"}),
            }
        );
    }

    #[test]
    fn dangling_passive_never_connects_its_own_two_loose_pins() {
        let planner = planner(vec![
            pin("R1.1", "~", "passive", None, 10.0),
            pin("R1.2", "~", "passive", None, 20.0),
        ]);
        let finding = finding(
            "dangling-passive",
            &["R1.1"],
            &[],
            "a 2-pin part has a pin left unconnected",
        );

        assert!(planner.connection(&finding).is_none());
    }

    #[test]
    fn loose_nonrail_power_input_gets_a_power_flag_fix() {
        let planner = planner(vec![pin("U1.4", "VREF", "power_in", None, 10.0)]);
        let finding = finding(
            "power_pin_not_driven",
            &["U1.4"],
            &[],
            "power input not driven",
        );

        assert_eq!(
            planner.power(&finding).unwrap().0,
            ToolFix {
                tool: "place_parts",
                args: json!({
                    "block": "erc_repair",
                    "parts": [{"part": "power:PWR_FLAG", "pins": {"1": "VREF"}}]
                }),
            }
        );
    }

    #[test]
    fn unused_output_gets_an_exact_no_connect_fix() {
        let planner = planner(vec![pin("U1.7", "OUT2", "output", None, 10.0)]);
        let finding = finding(
            "pin_not_connected",
            &["U1.7"],
            &[],
            "unused output is unconnected",
        );

        assert_eq!(
            planner.connection(&finding).unwrap().0,
            ToolFix {
                tool: "no_connect",
                args: json!({"pin": "U1.7"}),
            }
        );
    }

    #[test]
    fn conflicting_outputs_disconnect_the_second_driver_exactly() {
        let mut planner = planner(vec![
            pin("U1.1", "OUT", "output", Some("BUS"), 10.0),
            pin("U2.1", "OUT", "output", Some("BUS"), 20.0),
        ]);
        planner
            .baseline_nets
            .insert("U1.1".to_owned(), "BUS".to_owned());
        let finding = finding(
            "output-short",
            &["U1", "U2"],
            &["BUS"],
            "multiple outputs tie to this net",
        );

        assert_eq!(
            planner.output_conflict(&finding).unwrap().0,
            ToolFix {
                tool: "delete_wires",
                args: json!({"pins": ["U2.1"]}),
            }
        );
    }

    #[test]
    fn conflicting_outputs_without_one_new_driver_have_no_destructive_guess() {
        let planner = planner(vec![
            pin("U1.1", "OUT", "output", Some("BUS"), 10.0),
            pin("U2.1", "OUT", "output", Some("BUS"), 20.0),
        ]);
        let finding = finding(
            "output-short",
            &["U1", "U2"],
            &["BUS"],
            "multiple outputs tie to this net",
        );

        assert!(planner.output_conflict(&finding).is_none());
    }

    #[test]
    fn reversed_led_rotates_onto_the_fixed_net_positions_exactly() {
        let mut planner = planner(vec![
            pin("D1.1", "K", "passive", Some("LED_K"), 10.0),
            pin("D1.2", "A", "passive", Some("GND"), 20.0),
        ]);
        planner.rotations.insert("D1".to_owned(), 90.0);
        let finding = finding("led-polarity", &["D1"], &["GND", "+3V3"], "LED is reversed");

        assert_eq!(
            planner.polarity(&finding).unwrap().0,
            ToolFix {
                tool: "move_symbols",
                args: json!({"moves": [{"ref": "D1", "rot": 270.0}]}),
            }
        );
    }

    #[test]
    fn footprint_assignment_has_the_executable_batch_shape() {
        assert_eq!(
            footprint_assignment("R5", "Resistor_SMD:R_0805_2012Metric", "Device:R").0,
            ToolFix {
                tool: "assign_footprints",
                args: json!({"assignments": [{
                    "reference": "R5",
                    "footprint": "Resistor_SMD:R_0805_2012Metric"
                }]}),
            }
        );
    }

    #[test]
    fn wired_library_no_connect_pin_is_disconnected_exactly() {
        let planner = planner(vec![
            pin("U1.3", "NC", "no_connect", Some("SIG"), 10.0),
            pin("U1.4", "OUT", "output", None, 20.0),
        ]);
        let finding = finding(
            "library-no-connect-wired",
            &["U1.3"],
            &["SIG"],
            "library no-connect pin is wired",
        );
        let (fix, why) = planner.library_no_connect(&finding).unwrap();

        assert_eq!(
            fix,
            ToolFix {
                tool: "delete_wires",
                args: json!({"pins": ["U1.3"]}),
            }
        );
        assert!(why.contains("functional pin U1.4"));
    }

    #[test]
    fn unknown_finding_has_null_fix_and_a_reason() {
        let finding = finding("unknown-rule", &["U1"], &[], "requires judgment");
        let serialized = serde_json::to_value(finding).unwrap();

        assert_eq!(serialized["fix"], Value::Null);
        assert!(
            serialized["why"]
                .as_str()
                .is_some_and(|why| !why.is_empty())
        );
    }
}
