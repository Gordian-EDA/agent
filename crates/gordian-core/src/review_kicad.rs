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

"Flow-through protection" is a concrete topology: every protected signal must
enter and leave the protector on separate pins and separate connector-side and
downstream-side nets. A single shunt/ESD pin attached to a net that directly joins
both sides is not flow-through protection, even when it provides valid ESD clamping.

Ground every defect in exact netlist evidence. Quote the component field(s) or
pin/net/value assignments that prove the fault in `evidence`. Do NOT infer missing
individual IC power pins from package pin numbers, a rendered image, or wording like
"appears" / "not shown"; if the YAML maps a repeated power pin name such as VDD, VSS,
VDDA, VSSA, or VBAT to a rail, treat that netlist fact as intentional unless the
netlist explicitly exposes an unpowered pin. Do not claim a pin number/function
unless the netlist itself contains that numbered pin or exact pin name.

For a series diode, conventional current flows from anode (A) to cathode (K).
KiCad `Device:D` is pin 1=K and pin 2=A, so raw positive input on pin 2/A and the
protected output on pin 1/K is correct reverse-polarity protection; do not reverse it.

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
    "check power/regulation math, polarity, essential support parts, feedback/bias topology, digital pin functions, clocks, resets, enables, straps, interface direction, and intent-required flow-through protection (distinct in/out pins and nets per protected signal); for every intent-required timer, filter, or feedback block, trace resolved functional pins and passive paths, enforce any named topology, and verify timing/cutoff/gain from the actual circuit",
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

/// Deterministic checks for explicit, machine-verifiable intent contracts that
/// a small semantic reviewer can otherwise overlook. These do not guess part
/// suitability; they only reject objectively absent structures or malformed
/// library identifiers named by the request.
pub(crate) fn intent_contract_checks(intent: &str, design: &circuit_lang::Design) -> Vec<String> {
    let request = intent.to_ascii_lowercase();
    let all_components = design
        .blocks
        .values()
        .flat_map(|block| block.components.iter())
        .collect::<Vec<_>>();
    let components = all_components
        .iter()
        .copied()
        .filter(|(_, component)| matches!(component.origin, circuit_lang::model::Origin::Authored))
        .collect::<Vec<_>>();
    let mut defects = Vec::new();

    if request.contains("common-mode choke") || request.contains("common mode choke") {
        let has_choke = components.iter().any(|(_, component)| {
            component_text(component).contains("choke")
                || component_text(component).contains("commonmode")
                || component_text(component).contains("common_mode")
        });
        if !has_choke {
            defects.push(
                "intent contract: the request requires a CAN common-mode choke, but no authored component part/value identifies a choke; separate generic inductors do not satisfy this contract"
                    .into(),
            );
        }
    }

    if requires_unconditionally(&request, "status") {
        let has_status_net = design.nets.keys().any(|net| {
            let net = net.to_ascii_lowercase();
            net.contains("status") || net.contains("fault") || net.contains("error") || net == "err"
        });
        if !has_status_net {
            defects.push(
                "intent contract: a status signal/header was requested, but the design has no STATUS, FAULT, ERROR, or ERR net; labeling an unrelated reference or logic net as status is not a status output"
                    .into(),
            );
        }
    }

    if request.contains("power-good") || request.contains("power good") {
        let has_power_good = design.nets.keys().any(|net| {
            let net = net.to_ascii_lowercase();
            net.contains("pgood") || net.contains("power_good") || net == "pg"
        });
        if !has_power_good {
            defects.push(
                "intent contract: power-good was requested, but the design has no PG, PGOOD, or POWER_GOOD net"
                    .into(),
            );
        }
    }

    if request.contains("i2c")
        && (request.contains("pull-up")
            || request.contains("pull up")
            || request.contains("pullup"))
        && !has_i2c_pullups(&components)
    {
        defects.push(
            "intent contract: explicit I2C pull-ups were requested, but the design does not have separate resistors pulling both SDA and SCL to a positive supply rail"
                .into(),
        );
    }

    if (request.contains("selectable") || request.contains("configurable"))
        && request.contains("address")
        && !has_selectable_address_strap(&components)
    {
        defects.push(
            "intent contract: a selectable address strap was requested, but no jumper/switch/strap component is connected to an address net"
                .into(),
        );
    }

    if request.contains("decoupl") && !has_decoupling(&all_components) {
        defects.push(
            "intent contract: decoupling was explicitly requested, but no capacitor is connected between a positive supply rail and ground"
                .into(),
        );
    }

    if request.contains("test point") || request.contains("testpoint") {
        let requested_nets = requested_test_point_nets(&request);
        let present_nets = test_point_nets(&components);
        let missing = requested_nets
            .iter()
            .filter(|required| {
                !present_nets
                    .iter()
                    .any(|present| canonical_net_matches(present, required))
            })
            .copied()
            .collect::<Vec<_>>();
        if present_nets.is_empty() || !missing.is_empty() {
            let detail = if missing.is_empty() {
                "no authored test-point component is present".to_string()
            } else {
                format!(
                    "test points are missing for named nets {}",
                    missing.join(", ")
                )
            };
            defects.push(format!(
                "intent contract: named test points were requested, but {detail}"
            ));
        }
    }

    if request.contains("regulator")
        && !components.iter().any(|(_, component)| {
            let text = component_text(component);
            text.contains("regulator") || text.contains("buck") || text.contains("ldo")
        })
    {
        defects.push(
            "intent contract: a regulator was requested, but no authored regulator, buck, or LDO component is present"
                .into(),
        );
    }

    if (request.contains("power led")
        || request.contains("power-good led")
        || request.contains("power good led")
        || request.contains("power indicator"))
        && !has_power_led(&components)
    {
        defects.push(
            "intent contract: a power LED was requested, but no LED and series resistor form a path between a positive supply rail and ground"
                .into(),
        );
    }

    if (request.contains("reverse-polarity") || request.contains("reverse polarity"))
        && !has_reverse_protection(&components)
    {
        defects.push(
            "intent contract: reverse-polarity protection was requested, but no authored diode or MOSFET protection element is present between distinct non-ground nets"
                .into(),
        );
    }

    if request.contains("fuse")
        && !components
            .iter()
            .any(|(_, component)| component_text(component).contains("fuse"))
    {
        defects.push(
            "intent contract: an input fuse was requested, but no authored fuse is present".into(),
        );
    }

    if request.contains("tvs")
        && !components
            .iter()
            .any(|(_, component)| component_text(component).contains("tvs"))
    {
        defects.push("intent contract: input TVS protection was requested, but no authored TVS component is present".into());
    }

    if request.contains("input protection")
        && !components.iter().any(|(_, component)| {
            let text = component_text(component);
            text.contains("tvs")
                || text.contains("fuse")
                || text.contains("protection")
                || component.part.ends_with(":D")
                || text.contains("mosfet")
        })
    {
        defects.push(
            "intent contract: input protection was requested, but no authored fuse, TVS, diode, MOSFET, or protection component is present"
                .into(),
        );
    }

    if request.contains("connector") {
        let connector_count = components
            .iter()
            .filter(|(_, component)| component.part.to_ascii_lowercase().contains("connector"))
            .count();
        let required = if (request.contains("input") && request.contains("output"))
            || request.contains("connectors")
        {
            2
        } else {
            1
        };
        if connector_count < required {
            defects.push(format!(
                "intent contract: the request requires {required} connector(s), but only {connector_count} authored connector component(s) are present"
            ));
        }
    }

    if request.contains("split termination") && !has_split_termination(&components) {
        defects.push(
            "intent contract: split termination was requested, but the design does not contain two approximately 60-ohm resistors sharing a center net with a capacitor from that center net to ground"
                .into(),
        );
    }

    if request.contains("exact") && request.contains("footprint") {
        let invalid = components
            .iter()
            .filter(|(_, component)| !component.part.starts_with("power:"))
            .filter_map(|(refdes, component)| match component.footprint.as_deref() {
                Some(footprint) if footprint.split_once(':').is_some() => None,
                Some(footprint) => Some(format!("{refdes}={footprint}")),
                None => Some(format!("{refdes}=missing")),
            })
            .collect::<Vec<_>>();
        if !invalid.is_empty() {
            defects.push(format!(
                "intent contract: exact library-qualified footprints were requested, but these authored components lack a `Library:Footprint` identifier: {}",
                invalid.join(", ")
            ));
        }
    }

    defects
}

fn requires_unconditionally(request: &str, term: &str) -> bool {
    request
        .split(['.', ';', '\n'])
        .filter(|clause| clause.contains(term))
        .any(|clause| {
            ![
                "if supported",
                "if available",
                "when supported",
                "when available",
            ]
            .iter()
            .any(|conditional| clause.contains(conditional))
        })
}

fn component_text(component: &circuit_lang::model::Component) -> String {
    format!(
        "{} {}",
        component.part,
        component.value.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase()
    .replace(['-', ' '], "")
}

fn component_nets(component: &circuit_lang::model::Component) -> Vec<&str> {
    component
        .pins
        .values()
        .filter_map(|target| match target {
            circuit_lang::model::PinTarget::Net(net) => Some(net.as_str()),
            circuit_lang::model::PinTarget::NoConnect => None,
        })
        .collect()
}

fn is_ground_net(net: &str) -> bool {
    net.eq_ignore_ascii_case("gnd") || net.to_ascii_lowercase().ends_with("gnd")
}

fn is_positive_rail_net(net: &str) -> bool {
    circuit_lang::erc::rail_voltage(net).is_some_and(|volts| volts > 0.0)
        || matches!(
            net.trim().to_ascii_uppercase().as_str(),
            "VCC" | "VDD" | "VBUS" | "VBAT" | "VIN" | "VOUT"
        )
}

fn has_i2c_pullups(components: &[(&String, &circuit_lang::model::Component)]) -> bool {
    ["sda", "scl"].iter().all(|signal| {
        components.iter().any(|(_, component)| {
            if !component.part.ends_with(":R") {
                return false;
            }
            let nets = component_nets(component);
            nets.iter()
                .any(|net| net.to_ascii_lowercase().contains(signal))
                && nets.iter().any(|net| is_positive_rail_net(net))
        })
    })
}

fn has_selectable_address_strap(components: &[(&String, &circuit_lang::model::Component)]) -> bool {
    components.iter().any(|(refdes, component)| {
        let text = component_text(component);
        let selectable = refdes.to_ascii_uppercase().starts_with("JP")
            || refdes.to_ascii_uppercase().starts_with("SW")
            || text.contains("jumper")
            || text.contains("solderjumper")
            || text.contains("switch")
            || text.contains("strap");
        selectable
            && component_nets(component).iter().any(|net| {
                let net = net.to_ascii_lowercase();
                net.contains("addr")
                    || net.contains("address")
                    || matches!(net.as_str(), "a0" | "a1" | "a2" | "sdo")
            })
    })
}

fn requested_test_point_nets(request: &str) -> Vec<&'static str> {
    let Some((index, marker_len)) = request
        .find("test points")
        .map(|index| (index, "test points".len()))
        .or_else(|| {
            request
                .find("testpoints")
                .map(|index| (index, "testpoints".len()))
        })
        .or_else(|| {
            request
                .find("test point")
                .map(|index| (index, "test point".len()))
        })
        .or_else(|| {
            request
                .find("testpoint")
                .map(|index| (index, "testpoint".len()))
        })
    else {
        return Vec::new();
    };
    let suffix = &request[index + marker_len..];
    let clause = suffix.split(['.', ';', '\n']).next().unwrap_or(suffix);
    let clause = clause.trim_start();
    let named = if let Some(named) = clause.strip_prefix("for ") {
        named
    } else if let Some(named) = clause.strip_prefix("on ") {
        named
    } else if let Some(named) = clause.strip_prefix(':') {
        named.trim_start()
    } else {
        return Vec::new();
    };
    let tokens = named
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '+' | '.' | '_')))
        .map(|token| token.trim_matches('_'))
        .collect::<Vec<_>>();
    let aliases: &[(&str, &[&str])] = &[
        ("5V", &["5v", "+5v"]),
        ("3V3", &["3v3", "3.3v", "+3v3", "+3.3v"]),
        ("GND", &["gnd"]),
        ("SDA", &["sda"]),
        ("SCL", &["scl"]),
        ("VCC", &["vcc"]),
        ("VDD", &["vdd"]),
        ("VIN", &["vin"]),
        ("VOUT", &["vout"]),
        ("INT", &["int", "irq"]),
    ];
    aliases
        .iter()
        .filter(|(_, names)| names.iter().any(|name| tokens.contains(name)))
        .map(|(canonical, _)| *canonical)
        .collect()
}

fn test_point_nets(components: &[(&String, &circuit_lang::model::Component)]) -> Vec<String> {
    components
        .iter()
        .filter(|(refdes, component)| {
            refdes.to_ascii_uppercase().starts_with("TP")
                || component_text(component).contains("testpoint")
        })
        .flat_map(|(_, component)| component_nets(component))
        .map(ToOwned::to_owned)
        .collect()
}

fn canonical_net_matches(net: &str, canonical: &str) -> bool {
    let normalized = net
        .trim()
        .trim_start_matches('+')
        .to_ascii_uppercase()
        .replace('.', "");
    match canonical {
        "5V" => normalized == "5V",
        "3V3" => normalized == "3V3" || normalized == "33V",
        "INT" => normalized == "INT" || normalized == "IRQ",
        other => normalized == other,
    }
}

fn has_decoupling(components: &[(&String, &circuit_lang::model::Component)]) -> bool {
    components.iter().any(|(_, component)| {
        if !component.part.ends_with(":C") {
            return false;
        }
        let nets = component_nets(component);
        nets.iter().any(|net| is_ground_net(net))
            && nets.iter().any(|net| is_positive_rail_net(net))
    })
}

fn has_power_led(components: &[(&String, &circuit_lang::model::Component)]) -> bool {
    components.iter().any(|(_, led)| {
        if !component_text(led).contains("led") {
            return false;
        }
        let led_nets = component_nets(led);
        components.iter().any(|(_, resistor)| {
            if !resistor.part.ends_with(":R") {
                return false;
            }
            let resistor_nets = component_nets(resistor);
            if !led_nets.iter().any(|net| resistor_nets.contains(net)) {
                return false;
            }
            led_nets
                .iter()
                .chain(resistor_nets.iter())
                .any(|net| is_ground_net(net))
                && led_nets
                    .iter()
                    .chain(resistor_nets.iter())
                    .any(|net| is_positive_rail_net(net))
        })
    })
}

fn has_reverse_protection(components: &[(&String, &circuit_lang::model::Component)]) -> bool {
    components.iter().any(|(_, component)| {
        let text = component_text(component);
        let candidate = (component.part.ends_with(":D")
            || component.part.contains(":Q_")
            || text.contains("diode")
            || text.contains("mosfet"))
            && !text.contains("led")
            && !text.contains("tvs");
        let non_ground_nets = component_nets(component)
            .into_iter()
            .filter(|net| !is_ground_net(net))
            .collect::<Vec<_>>();
        candidate
            && non_ground_nets.len() >= 2
            && non_ground_nets.windows(2).any(|pair| pair[0] != pair[1])
    })
}

fn has_split_termination(components: &[(&String, &circuit_lang::model::Component)]) -> bool {
    let legs = components
        .iter()
        .filter(|(_, component)| component.part.ends_with(":R"))
        .filter(|(_, component)| {
            component
                .value
                .as_deref()
                .and_then(circuit_lang::erc::parse_value)
                .is_some_and(|ohms| (55.0..=65.0).contains(&ohms))
        })
        .map(|(_, component)| component_nets(component))
        .filter(|nets| nets.len() == 2)
        .collect::<Vec<_>>();

    for (index, left) in legs.iter().enumerate() {
        for right in &legs[index + 1..] {
            let Some(center) = left
                .iter()
                .find(|net| right.contains(net) && !net.eq_ignore_ascii_case("gnd"))
            else {
                continue;
            };
            let center_is_bypassed = components.iter().any(|(_, component)| {
                if !component.part.ends_with(":C") {
                    return false;
                }
                let nets = component_nets(component);
                nets.iter().any(|net| net == center)
                    && nets.iter().any(|net| net.eq_ignore_ascii_case("gnd"))
            });
            if center_is_bypassed {
                return true;
            }
        }
    }
    false
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
            let explicit = explicit_pin_facts(comp, provider);
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
            let protection_part = is_protection_part(&comp.part, &meta);
            let mut flow_pins: BTreeMap<String, FlowChannelPins<'_>> = BTreeMap::new();
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
                    if protection_part
                        && let Some((channel, role)) = flow_through_pin_role(&pin.name)
                    {
                        let entry = flow_pins.entry(channel).or_default();
                        match role {
                            FlowRole::Input => entry.inputs.push((pin.name.as_str(), net.as_str())),
                            FlowRole::Output => {
                                entry.outputs.push((pin.name.as_str(), net.as_str()))
                            }
                        }
                    }
                }
            }
            for (_channel, pins) in flow_pins {
                for (input_name, input_net) in &pins.inputs {
                    for (output_name, output_net) in &pins.outputs {
                        if input_net == output_net {
                            findings.push(format!(
                                "- {refdes}: protection pins {input_name} and {output_name} are both tied to {input_net} — flow-through input and output must use distinct nets or the protected channel is bypassed"
                            ));
                        }
                    }
                }
            }
        }
    }
    findings.sort();
    findings.dedup();
    findings
}

fn is_protection_part(part: &str, meta: &circuit_lang::SymbolMeta) -> bool {
    let mut text = part.to_ascii_lowercase();
    if let Some(description) = &meta.description {
        text.push(' ');
        text.push_str(&description.to_ascii_lowercase());
    }
    if let Some(keywords) = &meta.keywords {
        text.push(' ');
        text.push_str(&keywords.to_ascii_lowercase());
    }
    text.contains("protection") || text.contains("esd") || text.contains("protector")
}

#[derive(Clone, Copy)]
enum FlowRole {
    Input,
    Output,
}

#[derive(Default)]
struct FlowChannelPins<'a> {
    inputs: Vec<(&'a str, &'a str)>,
    outputs: Vec<(&'a str, &'a str)>,
}

/// Resolve conventional paired channel functions such as `CH1In`/`CH1Out`.
/// KiCad 9's TPD2S017 symbol spells CH2 input `CH2Int`; accept that known
/// trailing-`t` variant only when a non-empty channel prefix remains.
fn flow_through_pin_role(name: &str) -> Option<(String, FlowRole)> {
    let normalized: String = name
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_uppercase())
        .collect();
    if let Some(channel) = normalized.strip_suffix("OUT").filter(|s| !s.is_empty()) {
        return Some((channel.to_string(), FlowRole::Output));
    }
    if let Some(channel) = normalized.strip_suffix("IN").filter(|s| !s.is_empty()) {
        return Some((channel.to_string(), FlowRole::Input));
    }
    normalized
        .strip_suffix("INT")
        .filter(|s| !s.is_empty())
        .map(|channel| (channel.to_string(), FlowRole::Input))
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
/// Keep the annotation compact and function-focused: ordinary passive/connective
/// symbols stay out, while polarized two-pin parts retain the A/K or +/- facts
/// needed to prevent unsafe polarity guesses. Power-input facts already ride in
/// [`power_pin_facts`] (including required pins absent from the YAML).
fn explicit_pin_facts(
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
    let is_polarized_two_pin = meta.pins.len() == 2
        && meta.pins.iter().any(|pin| {
            matches!(
                pin.name.trim().to_ascii_uppercase().as_str(),
                "A" | "K" | "ANODE" | "CATHODE" | "+" | "-"
            )
        });
    if (meta.pins.len() <= 2 && !is_polarized_two_pin)
        || (meta.pins.iter().all(|pin| pin.etype == PinType::Passive)
            && !has_semantic_rail_pin
            && !is_polarized_two_pin)
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
    fn review_subject_resolves_numeric_diode_polarity() {
        let mut provider = circuit_lang::SymbolTable::with_basics();
        provider.mock_add(
            "Device:D",
            vec![
                ("1", "K", circuit_lang::PinType::Passive, 1),
                ("2", "A", circuit_lang::PinType::Passive, 1),
            ],
        );
        let netlist = r#"
version: 1
blocks:
  main:
    components:
      D1: {part: Device:D, pins: {"1": VPROT, "2": VIN}}
"#;
        let compiled = circuit_lang::compile(netlist, &provider);
        let design = compiled.design.expect("test design compiles");

        let subject = annotate_netlist_for_review(netlist, &design, &provider);

        assert!(subject.contains("D1 Device:D"), "{subject}");
        assert!(subject.contains("1/K [passive] -> VPROT"), "{subject}");
        assert!(subject.contains("2/A [passive] -> VIN"), "{subject}");
        assert!(NETLIST_REVIEW_SYSTEM.contains("input on pin 2/A"));
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
    fn protection_flow_through_inputs_and_outputs_require_distinct_nets() {
        let mut provider = circuit_lang::SymbolTable::with_basics();
        provider.mock_add(
            "Power_Protection:TPD2S017",
            vec![
                ("1", "CH1Out", circuit_lang::PinType::Passive, 1),
                ("2", "GND", circuit_lang::PinType::PowerInput, 1),
                ("3", "CH1In", circuit_lang::PinType::Passive, 1),
                ("4", "CH2Int", circuit_lang::PinType::Passive, 1),
                ("5", "VCC", circuit_lang::PinType::PowerInput, 1),
                ("6", "CH2Out", circuit_lang::PinType::Passive, 1),
            ],
        );
        let netlist = r#"
version: 1
blocks:
  main:
    components:
      U2:
        part: Power_Protection:TPD2S017
        pins: {"1": DPLUS, "2": GND, "3": DPLUS, "4": DMINUS, "5": VBUS, "6": DMINUS}
"#;
        let design = circuit_lang::compile(netlist, &provider)
            .design
            .expect("test design compiles");

        assert_eq!(
            symbol_pin_rail_checks(&design, &provider),
            vec![
                "- U2: protection pins CH1In and CH1Out are both tied to DPLUS — flow-through input and output must use distinct nets or the protected channel is bypassed",
                "- U2: protection pins CH2Int and CH2Out are both tied to DMINUS — flow-through input and output must use distinct nets or the protected channel is bypassed",
            ]
        );
    }

    #[test]
    fn protection_flow_through_accepts_separate_connector_and_device_nets() {
        let mut provider = circuit_lang::SymbolTable::with_basics();
        provider.mock_add(
            "Power_Protection:TPD2S017",
            vec![
                ("1", "CH1Out", circuit_lang::PinType::Passive, 1),
                ("2", "GND", circuit_lang::PinType::PowerInput, 1),
                ("3", "CH1In", circuit_lang::PinType::Passive, 1),
                ("4", "CH2Int", circuit_lang::PinType::Passive, 1),
                ("5", "VCC", circuit_lang::PinType::PowerInput, 1),
                ("6", "CH2Out", circuit_lang::PinType::Passive, 1),
            ],
        );
        let netlist = r#"
version: 1
blocks:
  main:
    components:
      U2:
        part: Power_Protection:TPD2S017
        pins: {"1": DPLUS_OUT, "2": GND, "3": DPLUS_IN, "4": DMINUS_IN, "5": VBUS, "6": DMINUS_OUT}
"#;
        let design = circuit_lang::compile(netlist, &provider)
            .design
            .expect("test design compiles");

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

    #[test]
    fn netlist_review_defines_flow_through_as_distinct_pins_and_nets() {
        assert!(NETLIST_REVIEW_SYSTEM.contains("Flow-through protection"));
        assert!(NETLIST_REVIEW_SYSTEM.contains("separate pins and separate connector-side"));
        assert!(NETLIST_REVIEW_SYSTEM.contains("single shunt/ESD pin"));
        assert!(
            QUICK_LENSES
                .iter()
                .any(|lens| lens.contains("distinct in/out pins and nets"))
        );
    }

    #[test]
    fn intent_contracts_reject_superficial_can_substitutes() {
        let provider = circuit_lang::SymbolTable::with_basics();
        let netlist = r#"
version: 1
blocks:
  main:
    components:
      L1: {part: Device:L, value: 47uH, footprint: Inductor_SMD_L_0603, between: [CANH, CANH_INT]}
      L2: {part: Device:L, value: 47uH, footprint: Inductor_SMD:L_0603, between: [CANL, CANL_INT]}
      R5: {part: Device:R, value: 60, footprint: Resistor_SMD:R_0603, between: [CANH_INT, MID]}
      R6: {part: Device:R, value: 60, footprint: Resistor_SMD:R_0603, between: [MID, CANL_INT]}
nets:
  CANH: {}
  CANL: {}
  CANH_INT: {}
  CANL_INT: {}
  MID: {}
  VREF: {}
"#;
        let design = circuit_lang::compile(netlist, &provider)
            .design
            .expect("fixture compiles");
        let defects = intent_contract_checks(
            "include a common-mode choke, TX/RX/status header, split termination, and exact footprints",
            &design,
        );
        let joined = defects.join("\n");
        assert!(joined.contains("common-mode choke"), "{joined}");
        assert!(joined.contains("status signal"), "{joined}");
        assert!(joined.contains("center net to ground"), "{joined}");
        assert!(joined.contains("L1=Inductor_SMD_L_0603"), "{joined}");
    }

    #[test]
    fn intent_contracts_accept_concrete_can_structures() {
        let provider = circuit_lang::SymbolTable::with_basics();
        let netlist = r#"
version: 1
blocks:
  main:
    components:
      L1: {part: Device:L, value: CAN_common-mode_choke, footprint: Filter:Choke_CommonMode, between: [CANH, CANH_INT]}
      R5: {part: Device:R, value: 60, footprint: Resistor_SMD:R_0603, between: [CANH_INT, MID]}
      R6: {part: Device:R, value: 60, footprint: Resistor_SMD:R_0603, between: [MID, CANL_INT]}
      C1: {part: Device:C, value: 4.7nF, footprint: Capacitor_SMD:C_0603, between: [MID, GND]}
nets:
  CANH: {}
  CANH_INT: {}
  CANL_INT: {}
  MID: {}
  GND: {}
  STATUS: {}
"#;
        let design = circuit_lang::compile(netlist, &provider)
            .design
            .expect("fixture compiles");
        assert!(
            intent_contract_checks(
                "include a common-mode choke, status header, split termination, and exact footprints",
                &design,
            )
            .is_empty()
        );
    }

    #[test]
    fn conditional_status_does_not_create_a_false_contract() {
        let provider = circuit_lang::SymbolTable::with_basics();
        let netlist = r#"
version: 1
blocks:
  main:
    components:
      R1: {part: Device:R, value: 10k, between: [SIG, GND]}
"#;
        let design = circuit_lang::compile(netlist, &provider)
            .design
            .expect("fixture compiles");

        let defects = intent_contract_checks(
            "Expose a status output if supported/available by the selected device",
            &design,
        );

        assert!(defects.is_empty(), "{}", defects.join("\n"));
        assert!(
            intent_contract_checks("Expose a status output", &design)
                .join("\n")
                .contains("status signal")
        );
        assert!(
            intent_contract_checks("µC status if available", &design).is_empty(),
            "conditional UTF-8 request must neither panic nor create a contract"
        );
    }

    #[test]
    fn named_test_points_require_each_explicit_canonical_net() {
        let provider = circuit_lang::SymbolTable::with_basics();
        let netlist = r#"
version: 1
blocks:
  main:
    components:
      TP1: {part: Device:R, value: testpoint, between: [SDA, GND]}
      TP2: {part: Device:R, value: testpoint, between: [3V3, GND]}
"#;
        let design = circuit_lang::compile(netlist, &provider)
            .design
            .expect("fixture compiles");
        let defects = intent_contract_checks(
            "Provide named test points for SDA, SCL, 3V3, and GND.",
            &design,
        )
        .join("\n");

        assert!(defects.contains("missing for named nets SCL"), "{defects}");
    }

    #[test]
    fn intent_contracts_reject_missing_explicit_board_essentials() {
        let provider = circuit_lang::SymbolTable::with_basics();
        let netlist = r#"
version: 1
blocks:
  main:
    components:
      R1: {part: Device:R, value: 10k, between: [SIG, GND]}
"#;
        let design = circuit_lang::compile(netlist, &provider)
            .design
            .expect("fixture compiles");
        let defects = intent_contract_checks(
            "Include I2C pull-ups, a selectable address strap, mandatory decoupling, named test points, a regulator, a power LED, reverse-polarity protection, input TVS and fuse protection, and input/output connectors",
            &design,
        )
        .join("\n");

        for expected in [
            "I2C pull-ups",
            "selectable address strap",
            "decoupling was explicitly requested",
            "named test points",
            "a regulator was requested",
            "a power LED was requested",
            "reverse-polarity protection",
            "input fuse",
            "input TVS",
            "requires 2 connector(s)",
        ] {
            assert!(
                defects.contains(expected),
                "missing {expected:?}: {defects}"
            );
        }
    }

    #[test]
    fn intent_contracts_accept_explicit_board_essentials() {
        let mut provider = circuit_lang::SymbolTable::with_basics();
        provider.mock_add(
            "Connector:Conn_01x02_Pin",
            vec![
                ("1", "Pin_1", circuit_lang::PinType::Passive, 1),
                ("2", "Pin_2", circuit_lang::PinType::Passive, 1),
            ],
        );
        for part in ["Device:D_TVS", "Device:Fuse"] {
            provider.mock_add(
                part,
                vec![
                    ("1", "1", circuit_lang::PinType::Passive, 1),
                    ("2", "2", circuit_lang::PinType::Passive, 1),
                ],
            );
        }
        let netlist = r#"
version: 1
blocks:
  main:
    components:
      R1: {part: Device:R, value: 4.7k, between: [SDA, 3V3]}
      R2: {part: Device:R, value: 4.7k, between: [SCL, 3V3]}
      JP1: {part: Device:R, value: address_strap_jumper, between: [ADDR, GND]}
      C1: {part: Device:C, value: 100nF, between: [3V3, GND]}
      TP1: {part: Device:R, value: testpoint, between: [SDA, GND]}
      TP2: {part: Device:R, value: testpoint, between: [SCL, GND]}
      U1: {part: Device:R, value: buck_regulator, between: [VIN, 3V3]}
      D1: {part: Device:D, value: reverse_diode, positive: VIN_RAW, negative: VIN}
      D2: {part: Device:D_TVS, value: input_TVS, between: [VIN, GND]}
      F1: {part: Device:Fuse, between: [VIN_CONN, VIN_RAW]}
      D3: {part: Device:LED, positive: LED_A, negative: GND}
      R3: {part: Device:R, value: 1k, between: [3V3, LED_A]}
      J1: {part: Connector:Conn_01x02_Pin, between: [VIN_CONN, GND]}
      J2: {part: Connector:Conn_01x02_Pin, between: [3V3, GND]}
"#;
        let compiled = circuit_lang::compile(netlist, &provider);
        let design = compiled
            .design
            .unwrap_or_else(|| panic!("fixture compiles: {:?}", compiled.diagnostics));
        let defects = intent_contract_checks(
            "Include I2C pull-ups, a selectable address strap, mandatory decoupling, named test points for SDA and SCL; include a regulator, a power LED, reverse-polarity protection, input TVS and fuse protection, and input/output connectors",
            &design,
        );

        assert!(defects.is_empty(), "{}", defects.join("\n"));
    }

    #[test]
    fn bme_sdo_jumper_satisfies_selectable_address_contract() {
        let mut provider = circuit_lang::SymbolTable::with_basics();
        provider.mock_add(
            "Connector:Conn_01x02_Pin",
            vec![
                ("1", "Pin_1", circuit_lang::PinType::Passive, 1),
                ("2", "Pin_2", circuit_lang::PinType::Passive, 1),
            ],
        );
        let design = circuit_lang::compile(
            r#"
version: 1
blocks:
  main:
    components:
      JP1: {part: Connector:Conn_01x02_Pin, pins: {1: SDO, 2: GND}}
"#,
            &provider,
        )
        .design
        .expect("fixture compiles");

        assert!(intent_contract_checks("include a selectable address strap", &design).is_empty());
    }
}
