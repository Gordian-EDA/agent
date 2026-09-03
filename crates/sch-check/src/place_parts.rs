//! The connectivity-only input of the bulk-create tool.
//!
//! The LLM states parts and what each pin connects to — never a coordinate, and
//! never a wire. Layout *intent* rides along as [`Intent`]; solvers turn it into
//! geometry. [`into_design`] lowers the input to the kernel [`Design`] the
//! checkers and the placement engines already speak; the caller hands
//! [`Intent::into_layout_ir`] to the placement engine alongside it.

use std::collections::{BTreeMap, BTreeSet};

use circuit_graph::netclass::is_power_net;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use indexmap::IndexMap;
use sch_model::ir::{Band, Cell, Flow, LayoutIr, Relation, Side};
use sch_model::tree::Tree;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::*;
use crate::{Diagnostic, Diagnostics, PinType, SymbolTable, authored, decouple, nets, pins};

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
    /// Region → the row/col tree it is drawn from. This IS the layout: the
    /// typesetter measures the symbols and computes every coordinate from it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub layout: BTreeMap<BlockName, Tree>,
    /// Region → how it is documented on the sheet: the caption drawn on its frame
    /// and a note explaining a decision. Regions left out are captioned with their
    /// own name and carry no note.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub blocks: BTreeMap<BlockName, BlockDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<Intent>,
    /// Placement engine override. The refusal a placement-engine failure returns
    /// names this as the way out, so it has to exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
}

/// What a region says about itself on the drawn sheet.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BlockDoc {
    /// Frame caption. Defaults to the region's own name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// One line under the frame, for the decision a reader cannot infer from the
    /// netlist — "150 kHz, sized for 500 mA", "pull-ups on the host side only".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One part and its pin connections.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PartSpec {
    /// Refdes, e.g. `U1`; omitted to allocate from the library prefix.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub refdes: Option<RefDes>,
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
    /// reaches every unit. A value of `"@R1.2"` means the net that pin already
    /// carries, whatever it is called. A copied KiCad name such as
    /// `"Net-(R1-Pad2)"` is normalized to the same pin reference.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub pins: IndexMap<String, String>,
    /// Decoupling sugar: cap value → count, expanded across this part's rails.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub decouple: IndexMap<String, u32>,
}

/// Live-sheet facts needed to audit a placement payload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExistingSheet {
    /// Existing named nets and the number of live sheet pins on each one.
    pub net_pins: BTreeMap<String, usize>,
    /// Reference designators already present on the sheet.
    pub refs: BTreeSet<RefDes>,
    /// References `reserve_refs` promised another caller. They are free to be
    /// named explicitly — that is what a reservation is for — but no designator
    /// this payload mints for itself may land on one.
    pub reserved: BTreeSet<RefDes>,
}

impl ExistingSheet {
    /// Every reference a minted designator has to step over.
    fn occupied(&self) -> BTreeSet<RefDes> {
        self.refs.union(&self.reserved).cloned().collect()
    }
}

/// An explicit reference designator that is already occupied.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DuplicateRef {
    #[serde(rename = "ref")]
    pub refdes: RefDes,
    pub next_free: RefDes,
}

/// A new pin whose named net would have no other pin after placement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DanglingPin {
    #[serde(rename = "ref")]
    pub refdes: RefDes,
    pub pin: String,
    pub net: NetName,
    /// Pins the net would carry in total — payload and live sheet together.
    /// One is the dangling pin itself; a caller reading `1` knows the net is
    /// new or has been emptied, not that it mistyped a busy net's name.
    pub pins_on_net: usize,
    /// Whether the live sheet already carries this net at all.
    pub on_sheet: bool,
}

/// A part that cannot be drawn: its `lib_id` names no symbol, or one of its pin
/// keys names no pin of that symbol.
///
/// It is left OUT of the design rather than refusing the payload it arrived in —
/// the other forty parts of a block are still a circuit, and a net that only
/// touched this part simply stays open and is reported as dangling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Unplaced {
    #[serde(rename = "ref")]
    pub refdes: RefDes,
    pub part: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub did_you_mean: Vec<String>,
}

/// Decoupling sugar omitted because the part's supply and return rails were not
/// uniquely identifiable. The authored part remains valid and is still placed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecoupleUnresolved {
    #[serde(rename = "ref")]
    pub refdes: RefDes,
    pub why: String,
    pub how: String,
}

/// A requested connection dropped because the library declares that pin NC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NcOverride {
    #[serde(rename = "ref")]
    pub refdes: RefDes,
    pub pin: String,
    pub requested_net: NetName,
}

/// Findings that make a bulk-create payload electrically incomplete.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PayloadAudit {
    pub dangling: Vec<DanglingPin>,
    pub duplicate_refs: Vec<DuplicateRef>,
    pub did_you_mean: BTreeMap<NetName, NetName>,
    pub unknown_pins: Vec<String>,
    /// Lowering errors — unknown lib_ids, pin conflicts, missing prefixes —
    /// reported alongside the audit so one refusal names every fault.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_errors: Vec<String>,
    /// Nets whose pin count could not be trusted because a part naming them was
    /// dropped for a missing reference prefix. Their dangling reports may clear
    /// on their own once the lib_id is fixed.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub unreliable_nets: BTreeSet<NetName>,
    /// Symbol/footprint assignments rejected before any placement work begins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub footprint_mismatch: Vec<FootprintMismatch>,
    /// Parts left out of the design because nothing could resolve them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unplaced: Vec<Unplaced>,
    /// Requested decouplers that need explicit supply and ground nets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decouple_unresolved: Vec<DecoupleUnresolved>,
    /// Requested nets replaced by explicit no-connects on library NC pins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nc_overridden: Vec<NcOverride>,
}

/// One symbol/footprint assignment whose electrical pins do not agree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FootprintMismatch {
    #[serde(rename = "ref")]
    pub refdes: RefDes,
    pub symbol: String,
    pub footprint: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_pads: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_pins: Vec<String>,
    pub message: String,
    pub suggestion: Option<String>,
}

impl PayloadAudit {
    /// Whether the payload may proceed to placement.
    ///
    /// Dangling pins do not block it. A net with one pin is unfinished work, not a
    /// malformed payload: the placement commits and [`crate::lint`]'s
    /// `single-pin-net` error — which `place_parts` returns in the same response —
    /// is what holds the board back until it is closed. Refusing a whole 50-part
    /// payload for it only forces the caller to resend everything.
    pub fn is_valid(&self) -> bool {
        self.input_errors.is_empty()
            && self.duplicate_refs.is_empty()
            && self.unknown_pins.is_empty()
            && self.footprint_mismatch.is_empty()
    }

    /// Whether anything at all is worth telling the caller about.
    pub fn is_clean(&self) -> bool {
        self.is_valid() && self.dangling.is_empty() && self.input_errors.is_empty()
    }

    /// Nets that would carry a single pin — reported, never fatal.
    pub fn dangling_nets(&self) -> BTreeSet<NetName> {
        self.dangling.iter().map(|d| d.net.clone()).collect()
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

    /// The engine-facing IR after dropping hints that name no available part.
    pub fn into_layout_ir_for(self, available: &BTreeSet<String>) -> (LayoutIr, Vec<String>) {
        let mut ir = self.into_layout_ir();
        let mut warnings = Vec::new();
        ir.place.retain(|reference, _| {
            let keep = available.contains(reference);
            if !keep {
                warnings.push(format!(
                    "dropped intent.place.{reference}: no arrangeable part has that reference"
                ));
            }
            keep
        });
        ir.mirror.retain(|reference| {
            let keep = available.contains(reference);
            if !keep {
                warnings.push(format!(
                    "dropped intent.mirror entry `{reference}`: no arrangeable part has that reference"
                ));
            }
            keep
        });
        ir.relations = ir
            .relations
            .into_iter()
            .enumerate()
            .filter_map(|(index, relation)| {
                let missing = relation
                    .refdes()
                    .into_iter()
                    .filter(|reference| !available.contains(*reference))
                    .map(str::to_string)
                    .collect::<BTreeSet<_>>();
                if missing.is_empty() {
                    Some(relation)
                } else {
                    warnings.push(format!(
                        "dropped intent.relations[{index}]: unknown arrangeable reference(s): {}",
                        missing.into_iter().collect::<Vec<_>>().join(", ")
                    ));
                    None
                }
            })
            .collect();
        (ir, warnings)
    }
}

/// The sheet a payload without an explicit `block` fills.
pub use sch_model::result::DEFAULT_BLOCK;

/// Lower the input to a [`Design`]: pin keys resolved to physical pin numbers
/// against the symbol table, `decouple` expanded into [`Origin::Synthesized`]
/// caps, unconnected signal pins marked no-connect, net attributes derived.
///
/// Diagnostics carry unknown parts and pins. The payload audit reports duplicate
/// references and new pins whose nets would have no other pin across the payload
/// and existing sheet.
pub fn into_design(
    input: &PlacePartsInput,
    provider: &SymbolTable,
    existing: &ExistingSheet,
) -> (Design, Diagnostics, PayloadAudit) {
    let mut input = input.clone();
    let duplicate_refs = resolve_references(&mut input, provider, existing);
    let nc_overridden = override_library_no_connects(&mut input, provider);
    let mut diags = Diagnostics::default();
    let mut design = Design {
        name: input.name.clone(),
        ..Design::default()
    };
    let default_block = input.block.as_deref().unwrap_or(DEFAULT_BLOCK);
    // A part nothing can resolve leaves before lowering, so the `decouple` caps it
    // would have grown never appear and the placement never has to draw it.
    let (dropped, unplaced) = unresolvable(&input, provider);
    for spec in input
        .parts
        .iter()
        .enumerate()
        .filter(|(index, _)| !dropped.contains(index))
        .map(|(_, spec)| spec)
    {
        let Some(refdes) = spec.refdes.as_ref() else {
            diags.push(Diagnostic::error(
                "missing-reference-prefix",
                format!(
                    "`{}` has no library Reference field from which to assign a designator",
                    spec.part
                ),
            ));
            continue;
        };
        let name = spec.block.as_deref().unwrap_or(default_block);
        let comp = component(spec, refdes, provider, &mut diags);
        let block = design.blocks.entry(name.to_string()).or_default();
        if block.components.insert(refdes.clone(), comp).is_some() {
            diags.push(Diagnostic::error(
                "duplicate-ref",
                format!("`{refdes}` is declared twice — one of them is lost"),
            ));
        }
    }
    if design.blocks.is_empty() {
        design
            .blocks
            .insert(default_block.to_string(), Block::default());
    }
    for (name, doc) in &input.blocks {
        match design.blocks.get_mut(name) {
            Some(block) => {
                block.title = doc.title.clone();
                block.note = doc.note.clone();
            }
            None => diags.push(Diagnostic::warning(
                "unknown-block",
                format!("`blocks` names region `{name}`, which no part joins — ignored"),
            )),
        }
    }
    for (name, tree) in &input.layout {
        match design.blocks.get_mut(name) {
            Some(block) => {
                for error in tree_faults(name, tree, block) {
                    diags.push(error);
                }
                block.layout = Some(tree.clone());
            }
            // A layout hint for a region nobody joined is a hint about nothing,
            // not a broken circuit — it is dropped and said so.
            None => diags.push(Diagnostic::warning(
                "unknown-block",
                format!("`layout` names region `{name}`, which no part joins — ignored"),
            )),
        }
    }
    let mut decouple_unresolved = Vec::new();
    expand_decouple(
        &input,
        default_block,
        &mut design,
        provider,
        &mut diags,
        &mut decouple_unresolved,
    );
    decouple::renumber(&mut design);
    pins::mark_unused_no_connect(&mut design, provider);
    nets::derive_attrs(&mut design);
    let mut audit = audit_payload(&input, &design, provider, existing, &diags);
    audit.duplicate_refs = duplicate_refs;
    audit.unplaced = unplaced;
    audit.decouple_unresolved = decouple_unresolved;
    audit.nc_overridden = nc_overridden;
    (design, diags, audit)
}

fn override_library_no_connects(
    input: &mut PlacePartsInput,
    provider: &SymbolTable,
) -> Vec<NcOverride> {
    let mut overridden = Vec::new();
    for part in &mut input.parts {
        let (Some(refdes), Some(meta)) = (part.refdes.as_ref(), provider.symbol(&part.part)) else {
            continue;
        };
        let requested = std::mem::take(&mut part.pins);
        for (key, requested_net) in requested {
            if is_no_connect_name(&requested_net) {
                part.pins.insert(key, requested_net);
                continue;
            }
            let resolved = pins::resolve(&meta, &key);
            let no_connects = resolved
                .iter()
                .filter(|pin| pin.etype == PinType::NoConnect)
                .collect::<Vec<_>>();
            if no_connects.is_empty() {
                part.pins.insert(key, requested_net);
                continue;
            }
            overridden.extend(no_connects.into_iter().map(|pin| NcOverride {
                refdes: refdes.clone(),
                pin: pin.number.clone(),
                requested_net: requested_net.clone(),
            }));
            if resolved.iter().all(|pin| pin.etype == PinType::NoConnect) {
                part.pins.insert(key, "nc".to_string());
                continue;
            }
            for pin in resolved {
                let net = match pin.etype {
                    PinType::NoConnect => "nc".to_string(),
                    _ => requested_net.clone(),
                };
                part.pins.insert(pin.number.clone(), net);
            }
        }
    }
    overridden.sort_by(|left, right| {
        left.refdes
            .cmp(&right.refdes)
            .then_with(|| left.pin.cmp(&right.pin))
            .then_with(|| left.requested_net.cmp(&right.requested_net))
    });
    overridden.dedup();
    overridden
}

fn resolve_references(
    input: &mut PlacePartsInput,
    provider: &SymbolTable,
    existing: &ExistingSheet,
) -> Vec<DuplicateRef> {
    assign_references(input, provider, existing);
    let mut occupied = existing.occupied();
    let mut counts = BTreeMap::<RefDes, usize>::new();
    for refdes in input.parts.iter().filter_map(|part| part.refdes.as_ref()) {
        *counts.entry(refdes.clone()).or_default() += 1;
        occupied.insert(refdes.clone());
    }
    counts
        .into_iter()
        .filter(|(refdes, count)| *count > 1 || existing.refs.contains(refdes))
        .map(|(refdes, _)| DuplicateRef {
            next_free: next_free_ref(refdes_prefix(&refdes), &occupied),
            refdes,
        })
        .collect()
}

/// Fill omitted references with the same deterministic designators lowering uses.
pub fn assign_references(
    input: &mut PlacePartsInput,
    provider: &SymbolTable,
    existing: &ExistingSheet,
) {
    let mut occupied = existing.occupied();
    for refdes in input.parts.iter().filter_map(|part| part.refdes.as_ref()) {
        occupied.insert(refdes.clone());
    }

    for part in &mut input.parts {
        if part.refdes.is_some() {
            continue;
        }
        let Some(reference) = provider
            .symbol(&part.part)
            .and_then(|symbol| symbol.reference)
        else {
            continue;
        };
        let prefix = reference.trim_end_matches(['?', '*']);
        if prefix.is_empty() {
            continue;
        }
        let refdes = next_free_ref(prefix, &occupied);
        occupied.insert(refdes.clone());
        part.refdes = Some(refdes);
    }
}

fn refdes_prefix(refdes: &str) -> &str {
    refdes.trim_end_matches(|ch: char| ch.is_ascii_digit())
}

fn next_free_ref(prefix: &str, occupied: &BTreeSet<RefDes>) -> RefDes {
    let mut number = 1;
    loop {
        let candidate = format!("{prefix}{number}");
        if !occupied.contains(&candidate) {
            return candidate;
        }
        number += 1;
    }
}

/// `nc`, `NC`, `NC_RTS`, `NC3`, `N/C` — the conventional ways a payload declares a pin
/// deliberately unconnected. Such a net is not dangling: the realiser draws it as a
/// no-connect marker rather than a wire.
fn is_no_connect_name(net: &str) -> bool {
    let upper = net.to_ascii_uppercase();
    upper == "NC"
        || upper == "N/C"
        || upper
            .strip_prefix("NC_")
            .is_some_and(|rest| !rest.is_empty())
        || upper
            .strip_prefix("NC")
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

fn audit_payload(
    input: &PlacePartsInput,
    design: &Design,
    provider: &SymbolTable,
    existing: &ExistingSheet,
    lowering: &Diagnostics,
) -> PayloadAudit {
    let mut pin_counts = existing.net_pins.clone();
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

    let mut audit = PayloadAudit {
        // A part dropped for want of a reference prefix takes its pins' net counts with
        // it, so its nets' dangling verdicts are guesses until its lib_id is fixed.
        unreliable_nets: input
            .parts
            .iter()
            .filter(|spec| spec.refdes.is_none())
            .flat_map(|spec| spec.pins.values().cloned())
            .collect(),
        ..PayloadAudit::default()
    };
    for spec in &input.parts {
        let Some(refdes) = spec.refdes.as_ref() else {
            continue;
        };
        let Some(meta) = provider.symbol(&spec.part) else {
            continue;
        };
        for (pin, net) in &spec.pins {
            if is_no_connect_name(net)
                || is_power_net(net)
                || input
                    .intent
                    .as_ref()
                    .is_some_and(|intent| intent.ports.contains_key(net))
                || pins::resolve(&meta, pin).is_empty()
            {
                continue;
            }
            let pins_on_net = pin_counts.get(net).copied().unwrap_or_default();
            if pins_on_net < 2 {
                audit.dangling.push(DanglingPin {
                    refdes: refdes.clone(),
                    pin: pin.clone(),
                    net: net.clone(),
                    pins_on_net,
                    on_sheet: existing.net_pins.contains_key(net),
                });
                if !existing.net_pins.contains_key(net)
                    && let Some(candidate) =
                        closest_net_name(net, existing.net_pins.keys().map(String::as_str))
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

/// The parts of `input` that cannot be drawn, with the repair for each.
///
/// A missing reference prefix belongs here too: the lowering has no designator to
/// file the part under, so it was already being dropped — silently, as a
/// whole-payload error rather than as one part's.
fn unresolvable(
    input: &PlacePartsInput,
    provider: &SymbolTable,
) -> (BTreeSet<usize>, Vec<Unplaced>) {
    let mut dropped = BTreeSet::new();
    let mut out = Vec::new();
    for (index, spec) in input.parts.iter().enumerate() {
        let Some(refdes) = spec.refdes.clone() else {
            let near = provider.suggest(&spec.part);
            dropped.insert(index);
            out.push(Unplaced {
                refdes: format!("unassigned {}", spec.part),
                part: spec.part.clone(),
                reason: format!(
                    "`{}` has no library Reference field from which to assign a designator; \
                     give this part an explicit `ref`",
                    spec.part
                ),
                did_you_mean: near,
            });
            continue;
        };
        let Some(meta) = provider.symbol(&spec.part) else {
            let (reason, did_you_mean) = authored::unknown_part_details(&spec.part, provider);
            dropped.insert(index);
            out.push(Unplaced {
                reason: format!("{refdes}: {reason}"),
                part: spec.part.clone(),
                did_you_mean,
                refdes,
            });
            continue;
        };
        let unknown: Vec<&String> = spec
            .pins
            .keys()
            .filter(|key| pins::resolve(&meta, key).is_empty())
            .collect();
        if let Some(key) = unknown.first() {
            dropped.insert(index);
            out.push(Unplaced {
                reason: format!(
                    "pin key{} {} not found on {refdes} ({})",
                    if unknown.len() > 1 { "s" } else { "" },
                    unknown
                        .iter()
                        .map(|key| format!("`{key}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    spec.part
                ),
                did_you_mean: pins::ranked_suggestions(&meta, key, 8),
                refdes,
                part: spec.part.clone(),
            });
        }
    }
    (dropped, out)
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

fn component(
    spec: &PartSpec,
    refdes: &str,
    provider: &SymbolTable,
    diags: &mut Diagnostics,
) -> Component {
    let mut comp = Component {
        part: spec.part.clone(),
        value: spec.value.clone(),
        footprint: spec.footprint.clone(),
        dnp: spec.dnp,
        props: spec.props.clone(),
        ..Default::default()
    };
    let Some(meta) = provider.symbol(&spec.part) else {
        diags.push(authored::unknown_part(refdes, &spec.part, provider));
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
            diags.push(authored::unknown_pin(refdes, &spec.part, &meta, key));
            continue;
        }
        for number in numbers {
            if let Some(prev) = claimed.insert(number.clone(), key)
                && prev != key
            {
                diags.push(authored::pin_conflict(refdes, &number, prev, key));
            }
            comp.pins.insert(number, target(net));
        }
    }
    comp
}

fn target(net: &str) -> PinTarget {
    if is_no_connect_name(net) {
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
    unresolved: &mut Vec<DecoupleUnresolved>,
) {
    for spec in &input.parts {
        if spec.decouple.is_empty() {
            continue;
        }
        let Some(refdes) = spec.refdes.as_ref() else {
            continue;
        };
        let block = spec.block.as_deref().unwrap_or(default_block);
        let Some(comp) = design
            .blocks
            .get(block)
            .and_then(|contents| contents.components.get(refdes))
            .cloned()
        else {
            let why = "part was not lowered, so its supply and ground pins are unavailable";
            unresolved.push(DecoupleUnresolved {
                refdes: refdes.clone(),
                why: why.to_string(),
                how: "fix the part library ID or add the decoupling capacitors explicitly"
                    .to_string(),
            });
            diags.push(Diagnostic::warning(
                "decouple-unplaced",
                format!("{refdes}: decouple ignored because the {why}"),
            ));
            continue;
        };
        match decouple::rails(refdes, &comp, provider) {
            Ok(rails) => {
                let caps = decouple::expand(refdes, &spec.decouple, &rails);
                let Some(block) = design.blocks.get_mut(block) else {
                    unresolved.push(DecoupleUnresolved {
                        refdes: refdes.clone(),
                        why: format!("placement region `{block}` disappeared during lowering"),
                        how: "add the decoupling capacitors explicitly".to_string(),
                    });
                    diags.push(Diagnostic::warning(
                        "decouple-missing-block",
                        format!(
                            "{refdes}: decouple ignored because placement region `{block}` is unavailable"
                        ),
                    ));
                    continue;
                };
                for (key, cap) in caps {
                    block.components.insert(key, cap);
                }
            }
            Err(diag) => {
                unresolved.push(DecoupleUnresolved {
                    refdes: refdes.clone(),
                    why: diag
                        .message
                        .strip_prefix(&format!("{refdes}: decouple "))
                        .unwrap_or(&diag.message)
                        .to_string(),
                    how:
                        "add the decoupling capacitors explicitly with their supply and ground nets"
                            .to_string(),
                });
                diags.push(diag);
            }
        }
    }
}

/// JSON Schema for the tool's `input_schema`. Deliberately terse: the LLM needs
/// the shape and the rules that are not obvious (`"nc"`, that a pin key may be a
/// name or a number, and that anything left out is a no-connect).
/// The `intent.relations` schema: every accepted entry shape, with an example.
///
/// Split out because the whole payload schema is one `json!` literal and the
/// macro's recursion limit is real; it also keeps the grammar in one readable place.
fn relations_schema() -> Value {
    json!({
    "type": "array",
    "description":
        "Relative placement. `b` and `anchor` may name a part already \
         on the sheet. Example: \
         {\"kind\":\"group\",\"name\":\"leds\",\"members\":[\"R3\",\"D1\"],\
         \"side\":\"right\",\"anchor\":\"U1\"}",
    "items": {
        "type": "object",
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["left_of", "right_of", "above", "below",
                         "group", "align"]
            },
            "a": {"type": "string"},
            "b": {"type": "string"},
            "name": {"type": "string"},
            "members": {
                "type": "array",
                "items": {"type": "string"},
                "minItems": 1
            },
            "side": {
                "description":
                    "An edge, or an [edge, anchor] pair, or \
                     {side, anchor}.",
                "anyOf": [
                    {"type": "string",
                     "enum": ["left", "right", "top", "bottom"]},
                    {"type": "array", "minItems": 2, "maxItems": 2},
                    {"type": "object"}
                ]
            },
            "anchor": {"type": "string"},
            "axis": {
                "type": "string",
                "enum": ["horizontal", "vertical"]
            }
        },
        "required": ["kind"]
    }})
}

/// What is wrong with a region's layout tree: a leaf naming a part that is not in the
/// region, or the same part placed twice. Both would silently lose a part off the drawing,
/// so they refuse the payload rather than surprise the author.
fn tree_faults(name: &str, tree: &Tree, block: &Block) -> Vec<Diagnostic> {
    let mut seen: BTreeSet<(String, u8)> = BTreeSet::new();
    let mut out = Vec::new();
    for (refdes, unit) in tree.keys() {
        if !block.components.contains_key(&refdes) {
            let near = closest_ref(&refdes, block.components.keys().map(String::as_str));
            let hint = near.map_or(String::new(), |r| format!(" (did you mean `{r}`?)"));
            out.push(Diagnostic::error(
                "layout-unknown-part",
                format!("`layout.{name}` places `{refdes}`, which is not a part of that region{hint}"),
            ));
        } else if !seen.insert((refdes.clone(), unit)) {
            out.push(Diagnostic::error(
                "layout-duplicate-part",
                format!("`layout.{name}` places `{refdes}` twice; every part gets one place"),
            ));
        }
    }
    out
}

/// The region member a mistyped refdes most likely meant.
fn closest_ref<'a>(refdes: &str, candidates: impl Iterator<Item = &'a str>) -> Option<String> {
    candidates
        .map(|c| (strsim::normalized_levenshtein(refdes, c), c))
        .filter(|(score, _)| *score >= 0.6)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, c)| c.to_owned())
}

/// The layout tree a region is drawn from: nested rows and columns of parts.
///
/// The schema is recursive by `$ref`, and its description carries the composition rules —
/// they are what separates a readable block from a merely correct one.
fn layout_tree_schema() -> Value {
    json!({
        "$ref": "#/$defs/node",
        "$defs": {
            "node": {
                "type": "object",
                "description":
                    "One of: {part} for a part, {row:[...]} for a left-to-right signal path, \
                     {col:[...]} for what hangs off a node. RULES: a row is ONE signal path \
                     (neighbours in a row must share a net, so they get a straight wire) — \
                     never put unrelated parts side by side. Anything hanging off a node (a \
                     shunt cap to GND, a pull-up, a bias resistor) goes in a col with the \
                     series part it attaches to. Around an IC: \
                     {row:[{col:[input-side parts]}, {part:IC}, {col:[output-side parts]}]}. \
                     Decoupling caps: a row of caps right after the IC. Two parts that meet \
                     only through a rail (GND, +3V3) need no adjacency — power symbols join \
                     them. Symmetric halves (H-bridge, differential pair, dual channel) are \
                     two mirrored cols side by side in one row. Keep a block to 3-12 parts \
                     and give every part a place. Gaps: 4-6 in a passive chain, 6-8 around \
                     an IC and between sub-rows; keep blocks COMPACT — empty space, long \
                     wires and parts far from what they connect to all read badly.",
                "properties": {
                    "part": {"type": "string", "description": "Refdes of a part in this region."},
                    "unit": {"type": "integer", "description": "Unit of a multi-unit symbol; one leaf per unit."},
                    "rot": {
                        "type": "integer", "enum": [0, 90, 180, 270],
                        "description":
                            "Only when the default looks wrong. 0 stands a 2-pin part up, \
                             90 lays it along the row. By default series passives lie along \
                             their row, a part touching a rail stands with GND down and the \
                             supply up, and a connector at a row end faces the circuit."
                    },
                    "mirror": {"type": "boolean", "description": "Flip the symbol left-to-right."},
                    "row": {"type": "array", "items": {"$ref": "#/$defs/node"}, "minItems": 1},
                    "col": {"type": "array", "items": {"$ref": "#/$defs/node"}, "minItems": 1},
                    "gap": {"type": "number", "description": "Grid units between children (1 unit = 1.27 mm, an 0603 resistor is 6 units). Default 8."},
                    "align": {"type": "string", "enum": ["center", "start", "end"]}
                },
                "additionalProperties": false
            }
        }
    })
}

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
                    "required": ["part"],
                    "additionalProperties": false,
                    "properties": {
                        "ref": {
                            "type": "string",
                            "description": "Optional refdes, e.g. U1. Omit to assign the lowest unused designator from the symbol library."
                        },
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
                                 signal pin left out becomes a no-connect. Every named signal net must \
                                 land on at least two pins across these parts and the existing sheet; \
                                 power rails and nets declared under intent.ports may be terminal. \
                                 Write \"@R1.2\" to join whatever net that existing pin is on, which \
                                 is the stable form of a copied KiCad-derived name such as \
                                 \"Net-(R1-Pad2)\".",
                            "additionalProperties": {"type": "string"}
                        },
                        "decouple": {
                            "type": "object",
                            "description":
                                "Capacitor value -> count. Uses this part's power-input pins and \
                                 adds at least one cap per distinct supply net to its nearest ground.",
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
                    "Region -> its layout TREE: how that region is drawn. This is the layout; \
                     compose one for every region.",
                "additionalProperties": layout_tree_schema()
            },
            "blocks": {
                "type": "object",
                "description":
                    "Region -> {title?, note?}: the caption drawn on that region's frame \
                     and one line of explanation under it. Write a `note` wherever a human \
                     would say why — a switching frequency, a sizing choice, which side a \
                     pull-up belongs on.",
                "additionalProperties": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string"},
                        "note": {"type": "string"}
                    },
                    "additionalProperties": false
                }
            },
            "engine": {
                "type": "string",
                "enum": ["flex"],
                "description":
                    "Placement engine override. Only worth setting after a placement-engine \
                     failure; the default is chosen from the sheet's size."
            },
            "intent": {
                "type": "object",
                "description": "Optional layout intent. Hints only; the solver owns all geometry.",
                "additionalProperties": false,
                "properties": {
                    "flow": {"enum": ["lr", "tb"], "description": "Global signal-flow direction."},
                    "rails": {
                        "type": "object",
                        "description": "Net -> sheet side: nets drawn as spanning rails. Left/right are mapped to the nearest supported horizontal band.",
                        "additionalProperties": {"enum": ["left", "right", "top", "bottom"]}
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
                    "relations": relations_schema()
                }
            }
        }
    })
}

#[cfg(test)]
mod no_connect_names {
    use crate::PinTarget;

    #[test]
    fn conventional_no_connect_names_lower_to_no_connects() {
        for name in ["nc", "NC", "NC_RTS", "nc_cts", "NC3", "N/C"] {
            assert!(super::is_no_connect_name(name), "{name}");
            assert_eq!(super::target(name), PinTarget::NoConnect, "{name}");
        }
        for name in ["NCS", "SYNC", "ENC1", "GND", "NC_"] {
            assert!(!super::is_no_connect_name(name), "{name}");
            assert_eq!(super::target(name), PinTarget::Net(name.into()), "{name}");
        }
    }
}
