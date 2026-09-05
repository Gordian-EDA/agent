//! `check_schematic`: the symbol-aware lints, deterministic electrical rules,
//! completeness audit, netlist fidelity, and KiCad ERC over the live file.

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
/// The live sheet as the kernel [`Design`] every checker reads.
///
/// Benched symbols are left out: they are on their nets but not laid out, and
/// every finding about one — an unassigned footprint, an undriven rail — is a
/// statement about work that has not been done yet rather than about the drawing.
/// `check_schematic` reports the bench as a count instead, and `export_fab`
/// refuses while it is non-empty, so nothing benched can reach a board house.
pub(crate) fn design(doc: &SchDoc, netlist: &Netlist) -> Design {
    let pins = placed_pins(doc);
    let mut components: IndexMap<String, Component> = IndexMap::new();
    // The units of a multi-unit part are separate symbols sharing one
    // reference; they are one component, and folding them together is what
    // stops the lints seeing each unit's pins as a design of its own.
    for symbol in doc
        .symbols()
        .filter(|s| !sch_floorplan::bench::is_benched(s))
    {
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
            title: None,
            note: None,
            components,
            layout: None,
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
    /// Rail glyphs KiCAD reported as bare, gathered before any fix is planned.
    bare_rails: BTreeSet<String>,
    pins: Vec<FixPin>,
    parts: BTreeMap<String, String>,
    rotations: BTreeMap<String, f64>,
    default_footprints: BTreeMap<String, String>,
}

impl FixPlanner {
    fn new(doc: &SchDoc, netlist: &Netlist, ctx: &AgentRuntime) -> Self {
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
        let parts = doc
            .symbols()
            .map(|symbol| (symbol.refdes().to_string(), symbol.lib_id.clone()))
            .collect::<BTreeMap<_, _>>();
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
        let rotations = doc
            .symbols()
            .map(|symbol| (symbol.refdes().to_string(), symbol.at.rot))
            .collect();
        Self {
            bare_rails: BTreeSet::new(),
            pins,
            parts,
            rotations,
            default_footprints,
        }
    }

    /// Every rail glyph ERC reported as bare, learned before any of them is
    /// planned for. KiCAD mints a fresh reference per glyph and the thrash guard
    /// budgets rail removals by the CALL, so offering them one at a time would
    /// spend that budget on the repair this planner just recommended. One call
    /// takes all of them.
    fn note_bare_rails(&mut self, findings: &[Finding]) {
        self.bare_rails = findings
            .iter()
            .filter(|finding| is_bare_pin_rule(&finding.code))
            .filter_map(|finding| self.affected_pin(finding))
            .filter(|pin| self.is_lone_rail_pin(pin))
            .map(|pin| pin.refdes.clone())
            .collect();
    }

    /// A `power:` symbol contributes exactly one pin; anything else is a part.
    fn is_lone_rail_pin(&self, pin: &FixPin) -> bool {
        self.parts
            .get(&pin.refdes)
            .is_some_and(|lib_id| lib_id.starts_with("power:"))
            && self
                .pins
                .iter()
                .filter(|other| other.refdes == pin.refdes)
                .count()
                == 1
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
                "No same-net or same-function endpoint proves the intended connection.".to_string();
        } else if is_output_conflict(&code, &message) {
            finding.why =
                "The finding does not identify which driver should be disconnected.".to_string();
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
        if let Some(fix) = self.orphan_rail(finding, from) {
            return Some(fix);
        }
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
        let desired_net = finding.nets.first().or(from.net.as_ref());
        let to = desired_net
            .and_then(|net| {
                self.pins
                    .iter()
                    .filter(|pin| pin.id != from.id)
                    .filter(|pin| pin.net.as_ref() == Some(net))
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

    /// A rail glyph connects by NAME, but only where its one pin TOUCHES the wire
    /// or the pin it names. When an electrical rule reports that pin as
    /// unconnected the glyph is reaching nothing at all, and the same-net search
    /// below answers with the wrong repair: the nearest thing already on that net
    /// is another glyph on the same rail, and joining two of those changes
    /// nothing. Both ends already read the name, so `connect` degrades to a label
    /// pair, the commit drops it as a duplicate of the name the glyphs print
    /// themselves, and the identical finding comes back — until the caller gives
    /// up and starts deleting the parts the rails were feeding.
    ///
    /// A pin that touches nothing cannot be holding anything up, so taking the
    /// glyph away is both the one call that provably clears the rule and one that
    /// provably loosens no other pin.
    fn orphan_rail(&self, finding: &Finding, from: &FixPin) -> Option<(ToolFix, String)> {
        // The extractor reads a rail glyph as sitting on the net it prints whether
        // or not its pin touches anything, so `unconnected` never flags one. The
        // rule that fired IS the measurement: only KiCAD knows the pin has nothing
        // under it, and only that reading may license taking the symbol away.
        if !is_bare_pin_rule(&finding.code) || !self.is_lone_rail_pin(from) {
            return None;
        }
        let refs: Vec<&String> = match self.bare_rails.is_empty() {
            true => vec![&from.refdes],
            false => self.bare_rails.iter().collect(),
        };
        let net = from.net.clone().unwrap_or_else(|| from.name.clone());
        Some((
            ToolFix {
                tool: "remove_symbols",
                args: json!({"refs": refs}),
            },
            format!(
                "{}'s only pin touches nothing, so it carries no connection and removing it \
                 changes the netlist by zero pins. That test is what makes this one safe, and \
                 it holds only for a rail symbol ERC reports as bare — a rail whose pin does \
                 touch a wire or a part pin is carrying `{net}`, and deleting that one opens \
                 the net. If a pin still needs `{net}`, seat a fresh rail on that pin with \
                 add_power; do not wire this glyph to another `{net}` glyph, which joins \
                 nothing because both ends already read the name.",
                from.refdes
            ),
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

    fn output_conflict(&self, _finding: &Finding) -> Option<(ToolFix, String)> {
        None
    }

    /// A reversed two-pin indicator is repaired by turning the symbol in place:
    /// a half turn lands each pin exactly where the other one was, so the two
    /// pins exchange nets and every wire, junction and label stays put.
    fn polarity(&self, finding: &Finding) -> Option<(ToolFix, String)> {
        let refdes = finding
            .refs
            .iter()
            .map(|reference| reference.split('.').next().unwrap_or(reference))
            .find(|reference| self.rotations.contains_key(*reference))?;
        if self.pins.iter().filter(|pin| pin.refdes == refdes).count() != 2 {
            return None;
        }
        let turned = (self.rotations[refdes] + 180.0).rem_euclid(360.0);
        Some((
            ToolFix {
                tool: "move_symbols",
                args: json!({"moves": [{"ref": refdes, "rot": turned, "turn_in_place": true}]}),
            },
            format!(
                "A half turn in place puts each of {refdes}'s two pins where the other one was, \
                 so they exchange nets and no wire moves."
            ),
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

/// A rule that says, in KiCAD's own reading of the drawing, that a pin has
/// nothing under it.
fn is_bare_pin_rule(code: &str) -> bool {
    let code = code.replace('_', "-");
    code.contains("pin-not-connected") || code == "single-pin-net"
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

/// Findings that count toward `checks`: everything the sheet itself states,
/// as opposed to advice (completeness), a reference comparison, or KiCAD's ERC.
/// Two nets whose names say they are one signal, with the wire that would join
/// them when the sheet makes the two ends unambiguous.
fn near_twin_finding(locator: &FindingLocator<'_>, twin: &sch_check::NearTwin) -> Finding {
    let message = format!(
        "probable missing connection between `{}` and `{}` — the names say one signal, \
         the sheet draws two nets with nothing between them",
        twin.a, twin.b
    );
    let (refs, nets, at) = locator.locate(&message, [], [twin.a.clone(), twin.b.clone()]);
    Finding {
        severity: "warning".to_string(),
        source: "near_twin_nets",
        code: "near-twin-nets".to_string(),
        message,
        refs,
        nets,
        at,
        fix: twin.join.as_ref().map(|(from, to)| ToolFix {
            tool: "connect",
            args: json!({"from": from, "to": to}),
        }),
        why: match &twin.join {
            Some((from, to)) => format!(
                "{from} and {to} are the only pins these nets put on a header or an IC, so they \
                 are the two ends of the signal. Join them, or rename one net if they really are \
                 different signals."
            ),
            None => "Join the two nets, or rename one of them if they really are different \
                     signals."
                .to_string(),
        },
        advisory: true,
    }
}

fn local_findings(findings: &[Finding], severity: &str) -> usize {
    findings
        .iter()
        .filter(|finding| {
            !matches!(
                finding.source,
                "completeness" | "netlist_fidelity" | "kicad_erc"
            ) && finding.severity == severity
        })
        .count()
}

/// An electrical-rule error the planner could not turn into a call is not
/// something the model can clear. Presented as blocking, it is what drives the
/// agent to delete the very parts and rails the request named — the LED, the
/// USB-serial bridge, every `#PWR`/`#FLG` on the sheet — because destruction is
/// the only move that makes the finding go away. Demote those to reported
/// findings, name them in the result, and let the turn finish.
///
/// Lint, footprint and netlist-fidelity errors keep blocking: they are statements
/// about the file rather than about the circuit, and deleting a part does not
/// make one of them pass.
fn demote_unrepairable(findings: &mut [Finding]) -> Vec<Value> {
    let mut reported = Vec::new();
    for finding in findings {
        if finding.severity != "error"
            || finding.fix.is_some()
            || !matches!(finding.source, "deterministic_erc" | "kicad_erc")
        {
            continue;
        }
        finding.severity = "warning".to_string();
        finding.advisory = true;
        finding.why = unrepairable_why(&finding.code, &finding.message);
        reported.push(json!({
            "code": finding.code,
            "refs": finding.refs,
            "message": finding.message,
            "why": finding.why,
        }));
    }
    reported
}

/// Why no edit clears this finding, said plainly enough that the honest answer
/// is obvious.
fn unrepairable_why(code: &str, message: &str) -> String {
    let message = message.to_ascii_lowercase();
    if message.contains("power output") && message.contains("connected") {
        return "Two library pins are both typed Power output on one rail. That is how the \
                symbols are typed, not a fault in the wiring: leave it, or attach a single \
                PWR_FLAG to the rail. Removing the rail symbols does not fix it."
            .to_string();
    }
    if code.contains("polarity") {
        return "This part does not have exactly two placed pins, so no half turn exchanges \
                them. Re-pick the symbol or wire it deliberately; do not delete it."
            .to_string();
    }
    "No edit clears this rule. Report it in your summary; deleting the parts it names is not \
     a repair."
        .to_string()
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
    let groups = fix_groups(findings);
    let mut lines = vec![format!(
        "{} findings, {} fixes",
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
    /// Symbols placed and wired but not laid out — progress, not a defect.
    bench: Vec<String>,
    gaps: Vec<sch_check::completeness::Gap>,
    /// The request fixes the part list, so no finding may suggest adding to it.
    strict: bool,
    /// How far the sheet is from the reference netlist, when the project carries
    /// one; `Err` when it carries one that will not parse.
    fidelity: Option<std::result::Result<sch_check::Fidelity, String>>,
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

/// The netlist the request pins the design to, `<project>/netlist.json`.
///
/// Absent means the request named no reference; present but unparseable is
/// reported, never swallowed — it is the one input this whole check rests on.
fn reference_netlist(
    ctx: &AgentRuntime,
) -> Option<std::result::Result<sch_check::ReferenceNetlist, String>> {
    let path = ctx.project_dir().join("netlist.json");
    let text = std::fs::read_to_string(&path).ok()?;
    Some(sch_check::ReferenceNetlist::parse(&text).map_err(|error| error.to_string()))
}

/// One finding per way the sheet departs from the reference netlist.
///
/// Advisory: the sheet may still be mid-construction, and it is the model — not
/// this check — that decides the circuit is finished.
fn fidelity_findings(locator: &FindingLocator, fidelity: &sch_check::Fidelity) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut push = |message: String, refs: Vec<String>, why: String| {
        let (refs, nets, at) = locator.locate(&message, refs, std::iter::empty::<String>());
        findings.push(Finding {
            severity: "warning".to_string(),
            source: "netlist_fidelity",
            code: "netlist_fidelity".to_string(),
            message,
            refs,
            nets,
            at,
            fix: None,
            why,
            advisory: true,
        });
    };
    if !fidelity.parts_missing.is_empty() {
        push(
            format!(
                "the reference netlist names {} that the sheet does not have",
                fidelity.parts_missing.join(", ")
            ),
            fidelity.parts_missing.clone(),
            "Place them; the request fixed the part list.".to_string(),
        );
    }
    if !fidelity.parts_extra.is_empty() {
        push(
            format!(
                "{} are not in the reference netlist",
                fidelity.parts_extra.join(", ")
            ),
            fidelity.parts_extra.clone(),
            "Remove them; the request fixed the part list.".to_string(),
        );
    }
    if fidelity.pins_mis_netted_total > 0 {
        let named: Vec<String> = fidelity
            .pins_mis_netted
            .iter()
            .map(|pin| {
                let expected = pin.expected_net.as_deref().unwrap_or("no connection");
                let actual = pin.actual_net.as_deref().unwrap_or("no connection");
                format!("{} is on {actual}, not {expected}", pin.pin)
            })
            .collect();
        let refs = fidelity
            .pins_mis_netted
            .iter()
            .filter_map(|pin| pin.pin.split('.').next().map(str::to_owned))
            .collect();
        push(
            format!(
                "{} pin(s) are not on the net the reference netlist puts them on",
                fidelity.pins_mis_netted_total
            ),
            refs,
            named.join("; "),
        );
    }
    findings
}

fn inspect_schematic(path: &Path, ctx: &AgentRuntime) -> Result<Inspection> {
    let doc = SchDoc::read(path).with_context(|| format!("reading {}", path.display()))?;
    let netlist = sch_doc::connect::extract(&doc);
    let bench = sch_floorplan::bench::benched(&doc);
    let design = design(&doc, &netlist);
    let locator = FindingLocator::new(&doc, &netlist);
    let strict = ctx.request_scope().no_additions;
    let gaps = if strict {
        Vec::new()
    } else {
        sch_check::completeness::audit(&design, ctx.provider())
    };
    let fidelity = reference_netlist(ctx).map(|reference| {
        reference.map(|reference| sch_check::reference::compare(&reference, &design))
    });
    let mut findings = Vec::new();

    let lint = sch_check::lint::lint(&design, ctx.provider());
    findings.extend(
        lint.0
            .iter()
            .map(|diagnostic| diagnostic_finding(&locator, diagnostic, "lint")),
    );
    findings.extend(
        sch_check::twins::near_twins(&design)
            .iter()
            .map(|twin| near_twin_finding(&locator, twin)),
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

    if let Some(Ok(fidelity)) = &fidelity {
        findings.extend(fidelity_findings(&locator, fidelity));
    }

    let advisory_source =
        |finding: &&Finding| matches!(finding.source, "completeness" | "netlist_fidelity");
    let local_errors = findings
        .iter()
        .filter(|finding| !advisory_source(finding) && finding.severity == "error")
        .count();
    let local_warnings = findings
        .iter()
        .filter(|finding| !advisory_source(finding) && finding.severity == "warning")
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
    // A finding that is only ABOUT benched symbols is a statement about layout
    // work not yet done. The bench count is what reports that.
    let on_bench: BTreeSet<&str> = bench.iter().map(String::as_str).collect();
    findings.retain(|finding| {
        finding.refs.is_empty()
            || !finding.refs.iter().all(|reference| {
                on_bench.contains(reference.split('.').next().unwrap_or(reference))
            })
    });
    Ok(Inspection {
        doc,
        netlist,
        bench,
        gaps,
        strict,
        fidelity,
        findings,
        local_errors,
        local_warnings,
        erc,
    })
}

/// Lint and run ERC over the live schematic.
pub fn check_schematic(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let detail = input.get("detail").and_then(Value::as_bool) == Some(true);
    let mut inspection = inspect_schematic(ctx.sch_path(), ctx)?;
    let mut planner = FixPlanner::new(&inspection.doc, &inspection.netlist, ctx);
    planner.note_bare_rails(&inspection.findings);
    for finding in &mut inspection.findings {
        planner.plan(finding, ctx);
    }
    inherit_duplicate_footprint_fixes(&mut inspection.findings);
    let unrepairable = demote_unrepairable(&mut inspection.findings);
    inspection.local_errors = local_findings(&inspection.findings, "error");
    inspection.local_warnings = local_findings(&inspection.findings, "warning");
    inspection
        .findings
        .sort_by_key(|finding| finding.severity != "error");
    let errors = inspection
        .findings
        .iter()
        .filter(|finding| finding.severity == "error")
        .count();
    let erc_errors = inspection
        .findings
        .iter()
        .filter(|finding| finding.source == "kicad_erc" && finding.severity == "error")
        .count();
    let mut report = json!({
        "ok": errors == 0,
        "bench": inspection.bench.len(),
        "bench_refs": inspection.bench,
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
            "strict": inspection.strict,
        },
    });
    match &inspection.fidelity {
        Some(Ok(fidelity)) => report["netlist_fidelity"] = json!(fidelity),
        Some(Err(error)) => {
            report["netlist_fidelity"] =
                json!({"error": format!("netlist.json is not a reference netlist: {error}")});
        }
        None => {}
    }

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
            report["erc_clean"] = json!(erc_errors == 0);
            if errors == 0 {
                let unfaithful =
                    matches!(&inspection.fidelity, Some(Ok(fidelity)) if !fidelity.matches);
                report["message"] = if unfaithful {
                    json!(
                        "schematic is electrically clean but does not reproduce the reference netlist; fix `netlist_fidelity`"
                    )
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
    if !unrepairable.is_empty() {
        report["reported_not_blocking"] = json!(unrepairable);
        if report["ok"] == json!(true) {
            report["message"] = json!(format!(
                "no finding left that an edit can clear; {} electrical rule(s) remain reported \
                 under `reported_not_blocking` — say what they are, do not delete parts to \
                 silence them",
                unrepairable.len()
            ));
        }
    }
    report["finding_counts"] = json!({
        "errors": inspection.findings.iter().filter(|finding| finding.severity == "error").count(),
        "warnings": inspection.findings.iter().filter(|finding| finding.severity == "warning").count(),
        "exclusions": inspection.findings.iter().filter(|finding| finding.severity == "exclusion").count(),
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
            bare_rails: BTreeSet::new(),
            pins,
            parts: BTreeMap::new(),
            rotations: BTreeMap::new(),
            default_footprints: BTreeMap::new(),
        }
    }

    fn finding(code: &str, refs: &[&str], nets: &[&str], message: &str) -> Finding {
        Finding {
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

    /// The audit works the two ends out from the netlist; the planner has no
    /// rule that knows better, so it must not claim the finding and overwrite it.
    #[test]
    fn a_near_twin_keeps_the_connect_the_audit_worked_out() {
        let code = "near-twin-nets";
        let message = "probable missing connection between `nrst` and `reset` — the names \
                       say one signal, the sheet draws two nets with nothing between them";
        assert!(!is_connection_finding(code, message));
        assert!(!is_power_finding(code, message));
        assert!(!is_output_conflict(code, message));
        assert!(!is_assignable_footprint(code, message));
        assert!(!code.contains("polarity"));
    }

    #[test]
    fn single_pin_net_reconnects_to_the_live_endpoint_exactly() {
        let planner = planner(vec![
            pin("R5.2", "~", "passive", None, 10.0),
            pin("U1.3", "IN", "input", Some("AUDIO_IN"), 80.0),
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
