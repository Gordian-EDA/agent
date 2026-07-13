//! Independent review of the committed design, in TWO complementary planes:
//!
//! - the NETLIST plane ([`review_netlist`]) — electrical-CORRECTNESS faults that
//!   pass ERC and look clean (pin-function mis-wires, wrong values, missing
//!   essential parts, voltage-domain and topology errors); the netlist analog of
//!   `tools/schematic_critic.py`.
//! - the LAYOUT plane ([`review_layout`]) — VISION readability faults only the
//!   *render* shows (decoupling-cap placement, sprawl, crossings, silk overlap,
//!   routing detours) — the 7/10 quality ceiling the netlist pass is blind to.
//!   This is the in-loop port of the standalone VLM critics
//!   `tools/schematic_critic.py` / `tools/pcb_critic.py`.
//!
//! Both share the [`crate::review`](mod@crate::review) MECHANICS
//! (the diverse-lens ensemble, verdict parsing, defect dedup) — the netlist pass
//! over a text subject, the layout pass over a rendered image — and both return
//! the same `(score, high-confidence defects)` shape the review→fix loop feeds
//! back. Each runs as a FRESH [`Provider::complete`] call (no conversation history
//! → unbiased; the generating model can't rationalise its own slips). The netlist
//! pass also unions in the deterministic exact-math ERC.

use crate::config::ReviewConfig;
use crate::{Binary, Provider};
use anyhow::Result;
use std::collections::BTreeMap;

const NETLIST_REVIEW_SYSTEM: &str = r#"You are a senior electronics engineer reviewing a circuit-YAML NETLIST, not layout.

The netlist is circuit-YAML:
- each component has a refdes, `part`, optional `value`, and a `pins` map;
- sugar forms are electrical facts too: `between: [A, B]`, `positive`/`negative`,
  `power: NET`, `label:global`, and `decouple: {100nF: N}` means N bypass caps exist.

Find only high-confidence electrical design faults that can pass ERC: wrong pin
function, wrong value/ratio, missing essential support part, voltage-domain error,
reversed polarity, or broken feedback/bias/topology. Do not report style,
layout, optional protection, or guesses; a correct design scores 9-10.

Treat every topology or operating mode explicitly named in the intent as a contract:
trace its active-device functional pins and passive paths rather than accepting a
same-order or superficially similar substitute. For timer/filter/feedback blocks,
verify timing, cutoff, or gain from the actual topology and values.

Ground every defect in exact netlist evidence. Quote the component field(s) or
pin/net/value assignments that prove the fault in `evidence`. Do NOT infer missing
individual IC power pins from package pin numbers, a rendered image, or wording like
"appears" / "not shown"; if the YAML maps a repeated power pin name such as VDD, VSS,
VDDA, VSSA, or VBAT to a rail, treat that netlist fact as intentional unless the
netlist explicitly exposes an unpowered pin. Do not claim a pin number/function
unless the netlist itself contains that numbered pin or exact pin name.

Reason briefly by IC/net, then emit only a JSON verdict after `FINAL_JSON:`:
{"score":0-10,"summary":"one line","defects":[{"severity":"critical|major|minor","confidence":"high|medium|low","refdes":"U1","issue":"short","why":"electrical reason","evidence":"exact netlist field(s) proving the fault"}]}"#;

/// Diverse review LENSES, unioned. A ground-truth recall sweep (tools/recall_harness.py, 25 injected
/// defects) showed repeated SAME-prompt sampling is flat (it can't recover a *consistent* miss), while
/// DIVERSE lenses each catch different fault classes and lift recall (80%→84%, and the clear-defect
/// rate to ~95%). Empty string = the general pass.
pub const LENSES: &[&str] = &[
    "",
    "power, regulation and analog faults: for EVERY resistor divider feeding a regulator feedback or \
     reference pin, COMPUTE the resulting output voltage from the resistor values and verify it matches \
     the intended rail; also bias/reference networks, voltage-domain part supply ranges, and \
     current-limit / gain resistor values; for every intent-required timer, filter, or feedback block, \
     trace resolved functional pins and passive paths, enforce any named topology, and compute its \
     timing/cutoff/gain from the actual circuit",
    "digital interfaces and clocking: SPI/I2C/UART/ISP bus signals on the correct device pins, \
     crystal/oscillator pin placement, reset/boot/enable/chip-select straps, direction and address pins",
];

const QUICK_LENSES: &[&str] = &[
    "check power/regulation math, polarity, essential support parts, feedback/bias topology, digital pin functions, clocks, resets, enables, straps, and interface direction; for every intent-required timer, filter, or feedback block, trace resolved functional pins and passive paths, enforce any named topology, and verify timing/cutoff/gain from the actual circuit",
];

fn netlist_lenses(config: &ReviewConfig) -> &'static [&'static str] {
    if config.ensemble {
        LENSES
    } else {
        QUICK_LENSES
    }
}

/// Review a netlist and return `(actionable score, union of high-confidence
/// critical/major defect lines)` — ready to feed back as a fix turn. Thin domain
/// wrapper over [`crate::review()`](fn@crate::review) with this module's
/// netlist system prompt and the configured lens set.
pub async fn review_netlist(
    client: &dyn Provider,
    intent: &str,
    netlist: &str,
    config: &ReviewConfig,
) -> Result<(f64, Vec<String>)> {
    crate::review::review_with_retry(
        client,
        NETLIST_REVIEW_SYSTEM,
        netlist_lenses(config),
        intent,
        netlist,
        config.retry_json,
    )
    .await
}

/// Add deterministic facts from the compiled design and KiCAD symbol table to the
/// subject the LLM sees. This keeps the reviewer anchored to actual pin/function/net
/// data for active parts, package power pins, and synthesized support parts.
pub(crate) fn annotate_netlist_for_review(
    netlist: &str,
    design: &circuit_lang::Design,
    provider: &circuit_lang::SymbolTable,
) -> String {
    let decoupling = decoupling_by_parent(design);
    let mut lines = vec![
        "=== SYMBOL GROUND TRUTH (from compiled circuit + KiCAD libraries) ===".to_string(),
        "Trust these facts over package-memory guesses.".to_string(),
    ];

    for block in design.blocks.values() {
        for (refdes, comp) in &block.components {
            if matches!(comp.origin, circuit_lang::model::Origin::Synthesized { .. }) {
                continue;
            }
            let power = power_pin_facts(comp, provider);
            let explicit = explicit_ic_pin_facts(comp, provider);
            let decouple = decoupling.get(refdes.as_str());
            if power.is_empty() && explicit.is_empty() && decouple.is_none() {
                continue;
            }

            lines.push(format!("{refdes} {}:", comp.part));
            if !power.is_empty() {
                lines.push(format!("  power pins: {}", power.join(", ")));
            }
            if !explicit.is_empty() {
                lines.push(format!("  resolved explicit pins: {}", explicit.join(", ")));
            }
            if let Some(values) = decouple {
                let entries = values
                    .iter()
                    .map(|(value, count)| format!("{value}: {count}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!("  decouple: {{{entries}}}"));
            }
        }
    }

    if lines.len() == 2 {
        lines.push(
            "No symbol-backed active-pin, power-pin, or synthesized-decoupling facts.".to_string(),
        );
    }
    lines.push("=== END SYMBOL GROUND TRUTH ===".to_string());
    lines.push(String::new());
    lines.push("Netlist:".to_string());
    lines.push(netlist.trim().to_string());
    lines.join("\n")
}

/// Catch symbol-backed rail contradictions that ordinary KiCAD ERC cannot see.
///
/// Several protection and connector symbols deliberately declare every pin as
/// passive, so ERC permits mistakes such as wiring a pin named `VBUS` to ground.
/// Keep this deliberately narrow: only exact, conventional supply/ground
/// function names and unambiguous voltage/ground net names participate.
pub(crate) fn symbol_pin_rail_checks(
    design: &circuit_lang::Design,
    provider: &circuit_lang::SymbolTable,
) -> Vec<String> {
    let mut findings = Vec::new();
    for block in design.blocks.values() {
        for (refdes, comp) in &block.components {
            let Some(meta) = provider.symbol(&comp.part) else {
                continue;
            };
            for (key, target) in comp.pins.iter().chain(comp.units.values().flatten()) {
                let circuit_lang::model::PinTarget::Net(net) = target else {
                    continue;
                };
                let by_number: Vec<_> = meta.pins.iter().filter(|pin| pin.number == *key).collect();
                let hits = if by_number.is_empty() {
                    meta.pins.iter().filter(|pin| pin.name == *key).collect()
                } else {
                    by_number
                };
                for pin in hits {
                    let voltage = circuit_lang::erc::rail_voltage(net);
                    if is_positive_supply_function(&pin.name) && voltage == Some(0.0) {
                        findings.push(format!(
                            "- {refdes}: symbol pin {}/{} is tied to ground net {net} — a positive supply pin cannot be grounded",
                            pin.number, pin.name
                        ));
                    } else if is_ground_function(&pin.name)
                        && voltage.is_some_and(|volts| volts > 0.0)
                    {
                        findings.push(format!(
                            "- {refdes}: symbol pin {}/{} is tied to positive rail {net} — a ground pin cannot be powered",
                            pin.number, pin.name
                        ));
                    }
                }
            }
        }
    }
    findings.sort();
    findings.dedup();
    findings
}

fn is_positive_supply_function(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_uppercase().as_str(),
        "VBUS" | "VBAT" | "VCC" | "VDD" | "VDDA" | "VDDD" | "AVDD" | "DVDD" | "PVDD"
    )
}

fn is_ground_function(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_uppercase().as_str(),
        "GND" | "VSS" | "VSSA" | "VSSD" | "AGND" | "DGND" | "PGND"
    )
}

fn decoupling_by_parent(design: &circuit_lang::Design) -> BTreeMap<&str, BTreeMap<&str, usize>> {
    let mut out: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
    for block in design.blocks.values() {
        for comp in block.components.values() {
            if let circuit_lang::model::Origin::Synthesized { parent, role, .. } = &comp.origin
                && role == "decouple"
            {
                let value = comp.value.as_deref().unwrap_or("?");
                *out.entry(parent.as_str())
                    .or_default()
                    .entry(value)
                    .or_default() += 1;
            }
        }
    }
    out
}

fn power_pin_facts(
    comp: &circuit_lang::model::Component,
    provider: &circuit_lang::SymbolTable,
) -> Vec<String> {
    let Some(meta) = provider.symbol(&comp.part) else {
        return Vec::new();
    };
    let mut covered: BTreeMap<&str, (&str, &circuit_lang::model::PinTarget)> = BTreeMap::new();
    let all_pins = comp.pins.iter().chain(comp.units.values().flatten());
    for (key, target) in all_pins {
        let by_number: Vec<_> = meta.pins.iter().filter(|p| p.number == *key).collect();
        let hits = if by_number.is_empty() {
            meta.pins.iter().filter(|p| p.name == *key).collect()
        } else {
            by_number
        };
        for pin in hits {
            covered.insert(pin.number.as_str(), (key.as_str(), target));
        }
    }

    meta.pins
        .iter()
        .filter(|pin| pin.etype == circuit_lang::PinType::PowerInput)
        .map(|pin| {
            let target = covered
                .get(pin.number.as_str())
                .map(|(_, target)| pin_target_text(target))
                .unwrap_or_else(|| "unconnected".to_string());
            format!("{}/{} -> {}", pin.number, pin.name, target)
        })
        .collect::<Vec<_>>()
}

/// Resolve every author-mapped non-power-input pin on an active multi-pin part to
/// the KiCAD library's physical number, function name, direction, and target net.
///
/// The committed-schematic lift keys every pin by number, which is electrically
/// exact but strips the function names the reviewer needs for topology reasoning.
/// Keep the annotation compact and IC-focused: passive/connective symbols never
/// enter because they have no active pin type, and power-input facts already ride
/// in [`power_pin_facts`] (including required pins absent from the YAML).
fn explicit_ic_pin_facts(
    comp: &circuit_lang::model::Component,
    provider: &circuit_lang::SymbolTable,
) -> Vec<String> {
    use circuit_lang::{PinDir, PinType};

    let Some(meta) = provider.symbol(&comp.part) else {
        return Vec::new();
    };
    let has_semantic_rail_pin = meta
        .pins
        .iter()
        .any(|pin| is_positive_supply_function(&pin.name) || is_ground_function(&pin.name));
    if meta.pins.len() <= 2
        || (meta.pins.iter().all(|pin| pin.etype == PinType::Passive) && !has_semantic_rail_pin)
    {
        return Vec::new();
    }

    let mut facts = Vec::new();
    for (key, target) in comp.pins.iter().chain(comp.units.values().flatten()) {
        let by_number: Vec<_> = meta.pins.iter().filter(|pin| pin.number == *key).collect();
        let hits = if by_number.is_empty() {
            meta.pins.iter().filter(|pin| pin.name == *key).collect()
        } else {
            by_number
        };
        for pin in hits {
            if pin.etype == PinType::PowerInput {
                continue; // already reported above, including missing required inputs
            }
            let direction = match pin.dir {
                PinDir::In => " [in]",
                PinDir::Out => " [out]",
                PinDir::Bidir => " [bidir]",
                PinDir::Passive => " [passive]",
                PinDir::Power => " [power]",
                PinDir::Unknown => "",
            };
            facts.push(format!(
                "{}/{}{direction} -> {}",
                pin.number,
                pin.name,
                pin_target_text(target)
            ));
        }
    }
    facts.sort();
    facts.dedup();
    facts
}

fn pin_target_text(target: &circuit_lang::model::PinTarget) -> String {
    match target {
        circuit_lang::model::PinTarget::Net(net) => net.clone(),
        circuit_lang::model::PinTarget::NoConnect => "nc".to_string(),
    }
}

// ── LAYOUT (vision) critic ───────────────────────────────────────────────────

/// Which rendered artifact the layout critic is looking at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutKind {
    /// A rendered `.kicad_sch`.
    Schematic,
    /// A rendered PCB plot.
    Board,
}

/// Diverse LAYOUT lenses, unioned — the vision analog of [`LENSES`]. Each pass
/// scrutinises a different readability axis the others under-weight (the empty
/// lens is the general sweep); a union across them lifts recall on the exact
/// defect families the 7/10 ceiling is made of.
pub const LAYOUT_LENSES: &[&str] = &[
    "",
    "PLACEMENT and grouping: decoupling caps must hug their IC's power pin; a part \
     that belongs beside another but sits far away; connectors/edge parts stranded \
     in the interior; sprawl (long wires + big empty gaps) versus a tight grouping",
    "ROUTING and reading: avoidable wire/trace crossings and dog-legs where a \
     straight run fits, congestion a small rearrangement would untangle, and silk / \
     refdes / value TEXT colliding with a wire, a body, or other text",
];

const QUICK_LAYOUT_LENSES: &[&str] = &[
    "check the visible worst layout issues only: related-part grouping, connector placement, board use, route/wire directness, congestion, and text/silkscreen legibility",
];

fn layout_lenses(config: &ReviewConfig) -> &'static [&'static str] {
    if config.ensemble {
        LAYOUT_LENSES
    } else {
        QUICK_LAYOUT_LENSES
    }
}

/// The prompt text that rides ALONGSIDE the rendered image: the design intent plus
/// the "reason first, then FINAL_JSON" instruction. The image itself is attached as
/// a vision block by [`crate::review_image`].
fn layout_prompt(intent: &str, kind: LayoutKind) -> String {
    let what = match kind {
        LayoutKind::Schematic => "rendered schematic",
        LayoutKind::Board => "rendered PCB layout",
    };
    format!(
        "Audit this {what} for layout quality. Intended circuit: {intent}. Reason \
         first (trace each candidate defect to its evidence), then emit the \
         FINAL_JSON verdict."
    )
}

const SCHEMATIC_LAYOUT_SYSTEM: &str = r#"Review one rendered KiCAD schematic for visual/layout readability, not electrical correctness.

Use image evidence only. Do not report wire-through-body or dangling-pin: engine
ground truth says every pin is connected and zero wires cross component bodies.
Judge avoidable text overlap, orientation, dog-legs, crossings/congestion,
spacing/sprawl, and confusing placement. Report `text-overlap` only when text
actually collides, merges, or obscures another label/wire/body; merely close but
readable labels are minor spacing/readability notes and must not gate a fix loop.
Minor issues alone score >=8; a real major scores 5-7; critical
wrong-reading/unreadable issues score <=4.

Reason briefly, tracing each candidate defect to visible evidence. Then emit
strict JSON after `FINAL_JSON:` with:
{"score":0-10,"summary":"one sentence","defects":[{"severity":"critical|major|minor","confidence":"high|medium|low","category":"wire-through-body|dangling-pin|text-overlap|orientation|off-spine-leg|wire-crossing|congestion|spacing|other","location":"refdes/region","description":"concrete observation","verification":"visible evidence"}]}"#;

const PCB_LAYOUT_SYSTEM: &str = r#"Review one rendered KiCAD PCB plot for placement/routing quality, not electrical correctness.

DRC ground truth says zero shorts, clearance violations, and unconnected items;
do not report those. Judge related-part grouping, connector edge placement,
board utilisation, routing directness/neatness, via economy, and silkscreen
legibility. Different copper colors are different layers. Minor issues alone
score >=8; a real major scores 5-7; critical unusable/broken-looking issues
score <=4.

Reason briefly from visible evidence and name a concrete better alternative for
each defect. Then emit strict JSON after `FINAL_JSON:` with:
{"score":0-10,"summary":"one sentence","defects":[{"severity":"critical|major|minor","confidence":"high|medium|low","category":"placement|board-utilisation|routing-directness|routing-neatness|via-economy|silkscreen|other","location":"refdes/region","description":"concrete observation","verification":"visible evidence"}]}"#;

/// Run the VISION layout critic over a rendered design `image` and return
/// `(actionable score, union of high-confidence critical/major layout defect
/// lines)` — the SAME shape [`review_netlist`] returns, so the review→fix loop
/// folds layout defects in beside the netlist ones. `kind` picks the schematic or
/// board layout prompt. A flaky/empty vision response degrades to `(0.0, [])`
/// inside [`crate::review_image`].
pub async fn review_layout(
    client: &dyn Provider,
    intent: &str,
    image: Binary,
    kind: LayoutKind,
    config: &ReviewConfig,
) -> Result<(f64, Vec<String>)> {
    let system = match kind {
        LayoutKind::Schematic => SCHEMATIC_LAYOUT_SYSTEM,
        LayoutKind::Board => PCB_LAYOUT_SYSTEM,
    };
    let prompt = layout_prompt(intent, kind);
    crate::review::review_image_with_retry(
        client,
        system,
        layout_lenses(config),
        &prompt,
        image,
        config.retry_json,
    )
    .await
}

/// Two defect lines are "the same" if they target the same refdes — so a union (across lenses, or
/// with the deterministic ERC layer) doesn't feed the agent two phrasings of one fault. Re-exported
/// from [`crate::review::same_defect`] so the ERC-union sites here read locally.
pub use crate::review::same_defect;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_subject_includes_symbol_ground_truth_and_decoupling() {
        let mut provider = circuit_lang::SymbolTable::with_basics();
        provider.mock_add(
            "M:STM32",
            vec![
                ("1", "VDD", circuit_lang::PinType::PowerInput, 1),
                ("9", "VDD", circuit_lang::PinType::PowerInput, 1),
                ("24", "VDD", circuit_lang::PinType::PowerInput, 1),
                ("7", "NRST", circuit_lang::PinType::Other, 1),
                ("8", "VSS", circuit_lang::PinType::PowerInput, 1),
                ("23", "VSS", circuit_lang::PinType::PowerInput, 1),
            ],
        );
        let netlist = r#"
version: 1
blocks:
  main:
    components:
      U1: {part: M:STM32, decouple: {100nF: 2}, pins: {VDD: VDD, VSS: GND, NRST: NRST}}
"#;
        let compiled = circuit_lang::compile(netlist, &provider);
        let design = compiled.design.expect("test design compiles");

        let subject = annotate_netlist_for_review(netlist, &design, &provider);

        assert!(subject.contains("SYMBOL GROUND TRUTH"));
        assert!(subject.contains("U1 M:STM32"));
        assert!(subject.contains("1/VDD -> VDD"));
        assert!(subject.contains("9/VDD -> VDD"));
        assert!(subject.contains("24/VDD -> VDD"));
        assert!(subject.contains("8/VSS -> GND"));
        assert!(subject.contains("23/VSS -> GND"));
        assert!(subject.contains("7/NRST -> NRST"));
        assert!(subject.contains("decouple: {100nF: 2}"));
        assert!(subject.contains("Netlist:\n"));
        assert!(subject.contains(netlist.trim()));
    }

    #[test]
    fn review_subject_resolves_numeric_active_pin_functions() {
        let mut provider = circuit_lang::SymbolTable::with_basics();
        provider.mock_add(
            "M:DUAL_OPAMP",
            vec![
                ("1", "~", circuit_lang::PinType::Other, 1),
                ("2", "-", circuit_lang::PinType::Other, 1),
                ("3", "+", circuit_lang::PinType::Other, 1),
                ("4", "V-", circuit_lang::PinType::PowerInput, 3),
                ("5", "+", circuit_lang::PinType::Other, 2),
                ("6", "-", circuit_lang::PinType::Other, 2),
                ("7", "~", circuit_lang::PinType::Other, 2),
                ("8", "V+", circuit_lang::PinType::PowerInput, 3),
            ],
        );
        let netlist = r#"
version: 1
blocks:
  main:
    components:
      U1: {part: M:DUAL_OPAMP, pins: {"1": VVG, "2": VVG, "3": VVG_SENSE, "4": GND, "5": V2, "6": VOUT, "7": VOUT, "8": +5V}}
"#;
        let compiled = circuit_lang::compile(netlist, &provider);
        let design = compiled.design.expect("test design compiles");

        let subject = annotate_netlist_for_review(netlist, &design, &provider);

        assert!(subject.contains("resolved explicit pins:"), "{subject}");
        assert!(subject.contains("5/+ -> V2"), "{subject}");
        assert!(subject.contains("6/- -> VOUT"), "{subject}");
        assert!(subject.contains("7/~ -> VOUT"), "{subject}");
        assert!(subject.contains("4/V- -> GND"), "{subject}");
        assert!(subject.contains("8/V+ -> +5V"), "{subject}");
    }

    #[test]
    fn passive_protection_supply_pin_is_annotated_and_cannot_be_grounded() {
        let mut provider = circuit_lang::SymbolTable::with_basics();
        provider.mock_add(
            "Protection:USB_ESD",
            vec![
                ("1", "I/O1", circuit_lang::PinType::Passive, 1),
                ("2", "GND", circuit_lang::PinType::Passive, 1),
                ("3", "I/O2", circuit_lang::PinType::Passive, 1),
                ("4", "I/O2", circuit_lang::PinType::Passive, 1),
                ("5", "VBUS", circuit_lang::PinType::Passive, 1),
                ("6", "I/O1", circuit_lang::PinType::Passive, 1),
            ],
        );
        let netlist = r#"
version: 1
blocks:
  main:
    components:
      U2:
        part: Protection:USB_ESD
        pins: {"1": DP_IN, "2": GND, "3": DM_IN, "4": DM_OUT, "5": GND, "6": DP_OUT}
"#;
        let compiled = circuit_lang::compile(netlist, &provider);
        let design = compiled.design.expect("test design compiles");

        let subject = annotate_netlist_for_review(netlist, &design, &provider);
        assert!(subject.contains("5/VBUS [passive] -> GND"), "{subject}");

        assert_eq!(
            symbol_pin_rail_checks(&design, &provider),
            vec![
                "- U2: symbol pin 5/VBUS is tied to ground net GND — a positive supply pin cannot be grounded"
            ]
        );
    }

    #[test]
    fn symbol_pin_rail_check_accepts_matching_supply_and_ground_rails() {
        let mut provider = circuit_lang::SymbolTable::with_basics();
        provider.mock_add(
            "M:POWERED",
            vec![
                ("1", "VDD", circuit_lang::PinType::PowerInput, 1),
                ("2", "VSS", circuit_lang::PinType::PowerInput, 1),
                ("3", "OUT", circuit_lang::PinType::Other, 1),
            ],
        );
        let netlist = r#"
version: 1
blocks:
  main:
    components:
      U1: {part: M:POWERED, pins: {VDD: 3V3, VSS: GND, OUT: SIG}}
"#;
        let compiled = circuit_lang::compile(netlist, &provider);
        let design = compiled.design.expect("test design compiles");

        assert!(symbol_pin_rail_checks(&design, &provider).is_empty());
    }

    #[test]
    fn netlist_review_requires_named_topology_and_actual_transfer_checks() {
        assert!(NETLIST_REVIEW_SYSTEM.contains("explicitly named in the intent"));
        assert!(NETLIST_REVIEW_SYSTEM.contains("actual topology and values"));
        for lenses in [QUICK_LENSES, LENSES] {
            assert!(
                lenses.iter().any(|lens| {
                    lens.contains("intent-required timer, filter, or feedback block")
                        && lens.contains("enforce any named topology")
                        && lens.contains("actual circuit")
                }),
                "review path is missing the functional-topology trace: {lenses:?}"
            );
        }
    }
}
