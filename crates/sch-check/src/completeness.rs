//! Advisory checks for support circuitry expected around powered and external interfaces.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use circuit_graph::netclass::{is_connector_like, is_ground, is_power_net};
use circuit_graph::{CircuitGraph, NetKind, Node, Pin};
use serde::Serialize;

use crate::{Block, Component, Design, PinTarget, PinType, SymbolMeta, SymbolTable};

/// One deterministic, non-blocking completeness finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Gap {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refdes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net: Option<String>,
    pub suggestion: String,
}

#[derive(Clone)]
struct ResolvedPin {
    number: String,
    name: String,
    net: String,
    etype: PinType,
}

#[derive(Default)]
struct MatchedIdioms {
    decoupling_caps: BTreeSet<String>,
    i2c_pullup_nets: BTreeSet<String>,
    led_indicator: bool,
}

/// Audit support circuitry whose absence is unambiguous from connectivity and symbol metadata.
pub fn audit(design: &Design, symbols: &SymbolTable) -> Vec<Gap> {
    let mut gaps = Vec::new();
    let mut matched = MatchedIdioms::default();

    for block in design.blocks.values() {
        let graph = circuit_graph(block, design, symbols);
        for found in circuit_graph::find(&graph, &circuit_graph::library::DECOUPLING) {
            matched
                .decoupling_caps
                .extend(found.bindings["cap"].clone());
        }
        for found in circuit_graph::find(&graph, &circuit_graph::library::I2C_PULLUP) {
            for role in ["res_a", "res_b"] {
                for refdes in &found.bindings[role] {
                    if let Some(component) = block.components.get(refdes) {
                        matched.i2c_pullup_nets.extend(
                            component_nets(component)
                                .into_iter()
                                .filter(|net| i2c_kind(net).is_some()),
                        );
                    }
                }
            }
        }
        matched.led_indicator |= circuit_graph::find(
            &graph,
            &circuit_graph::library::LED_INDICATOR,
        )
        .iter()
        .any(|found| {
            block
                .components
                .get(&found.anchor)
                .is_some_and(|component| component.part.to_ascii_uppercase().contains("LED"))
        });
    }

    for block in design.blocks.values() {
        audit_bypass(block, symbols, &matched, &mut gaps);
        audit_control_straps(block, symbols, &mut gaps);
    }
    audit_buses(design, &matched, &mut gaps);
    audit_bus_power_support(design, symbols, &matched, &mut gaps);
    audit_connector_protection(design, symbols, &mut gaps);

    gaps.sort_by(|a, b| {
        (&a.kind, &a.refdes, &a.net, &a.suggestion).cmp(&(
            &b.kind,
            &b.refdes,
            &b.net,
            &b.suggestion,
        ))
    });
    gaps.dedup();
    gaps
}

fn circuit_graph(block: &Block, design: &Design, symbols: &SymbolTable) -> CircuitGraph {
    let nodes = block
        .components
        .iter()
        .filter(|(_, component)| !component.dnp)
        .map(|(refdes, component)| {
            let meta = symbols.symbol(&component.part);
            let pins = match meta.as_ref() {
                Some(meta) => resolved_pins(component, meta)
                    .into_iter()
                    .map(|pin| Pin {
                        number: pin.number,
                        name: pin.name,
                        net: Some(pin.net),
                    })
                    .collect(),
                None => connected_pins(component)
                    .map(|(key, net)| Pin {
                        number: key.to_string(),
                        name: key.to_string(),
                        net: Some(net.to_string()),
                    })
                    .collect(),
            };
            Node {
                refdes: refdes.clone(),
                lib_id: component.part.clone(),
                value: component.value.clone().unwrap_or_default(),
                pins,
            }
        })
        .collect();
    CircuitGraph::new(nodes, |net| {
        if is_ground(net) {
            NetKind::Ground
        } else if design.nets.get(net).is_some_and(|attrs| attrs.power) || is_power_net(net) {
            NetKind::Power
        } else {
            NetKind::Signal
        }
    })
}

fn audit_bypass(
    block: &Block,
    symbols: &SymbolTable,
    matched: &MatchedIdioms,
    gaps: &mut Vec<Gap>,
) {
    for (refdes, component) in &block.components {
        if component.dnp || !is_ic(component) {
            continue;
        }
        let Some(meta) = symbols.symbol(&component.part) else {
            continue;
        };
        let mut pins_by_net: BTreeMap<String, Vec<ResolvedPin>> = BTreeMap::new();
        for pin in resolved_pins(component, &meta) {
            if pin.etype == PinType::PowerInput && !is_ground(&pin.net) && !is_ground(&pin.name) {
                pins_by_net.entry(pin.net.clone()).or_default().push(pin);
            }
        }
        for (net, pins) in pins_by_net {
            let mut caps: BTreeSet<&str> = matched
                .decoupling_caps
                .iter()
                .filter_map(|candidate| {
                    block
                        .components
                        .get_key_value(candidate)
                        .filter(|(_, cap)| is_bypass_cap(cap, &net))
                        .map(|(key, _)| key.as_str())
                })
                .collect();
            caps.extend(
                block
                    .components
                    .iter()
                    .filter(|(_, cap)| is_bypass_cap(cap, &net))
                    .map(|(candidate, _)| candidate.as_str()),
            );
            for pin in pins.iter().skip(caps.len()) {
                gaps.push(Gap {
                    kind: "bypass".into(),
                    refdes: Some(refdes.clone()),
                    net: Some(net.clone()),
                    suggestion: format!("add 100nF between {refdes}.{} and GND", display_pin(pin)),
                });
            }
        }
    }
}

fn audit_control_straps(block: &Block, symbols: &SymbolTable, gaps: &mut Vec<Gap>) {
    for (refdes, component) in &block.components {
        if component.dnp || !is_ic(component) {
            continue;
        }
        let Some(meta) = symbols.symbol(&component.part) else {
            continue;
        };
        for pin in resolved_pins(component, &meta) {
            if !is_control_pin(&pin.name) || is_power_net(&pin.net) {
                continue;
            }
            if has_pull(&pin.net, block.components.values()) {
                continue;
            }
            gaps.push(Gap {
                kind: "control_pull".into(),
                refdes: Some(refdes.clone()),
                net: Some(pin.net.clone()),
                suggestion: format!(
                    "add a pull-up or pull-down on {refdes}.{} ({})",
                    display_pin(&pin),
                    pin.net
                ),
            });
        }
    }
}

fn audit_buses(design: &Design, matched: &MatchedIdioms, gaps: &mut Vec<Gap>) {
    let components: Vec<&Component> = design
        .blocks
        .values()
        .flat_map(|block| block.components.values())
        .filter(|component| !component.dnp)
        .collect();
    let nets = all_nets(design);

    let mut can_pairs: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
    for net in &nets {
        if let Some((family, high)) = can_kind(net) {
            let pair = can_pairs.entry(family).or_default();
            if high {
                pair.0.push(net.clone());
            } else {
                pair.1.push(net.clone());
            }
        }
    }
    for (_, (highs, lows)) in can_pairs {
        if highs.is_empty() || lows.is_empty() {
            continue;
        }
        if !highs.iter().any(|high| {
            lows.iter()
                .any(|low| has_resistive_path(high, low, &components))
        }) {
            let high = &highs[0];
            let low = &lows[0];
            gaps.push(Gap {
                kind: "can_termination".into(),
                refdes: None,
                net: Some(format!("{high}/{low}")),
                suggestion: format!(
                    "add split 120Ω termination between {high} and {low} (two 60Ω resistors and a switch/jumper)"
                ),
            });
        }
    }

    for net in nets {
        if i2c_kind(&net).is_some()
            && !matched.i2c_pullup_nets.contains(&net)
            && !has_pull_to_power(&net, &components)
        {
            gaps.push(Gap {
                kind: "i2c_pullup".into(),
                refdes: None,
                net: Some(net.clone()),
                suggestion: format!("add 4.7k pull-up from {net} to the bus supply"),
            });
        }
        let series_kind = if is_usb_data(&net) {
            Some("usb_series")
        } else if is_spi_cs(&net) {
            Some("spi_cs_series")
        } else {
            None
        };
        if let Some(kind) = series_kind
            && !has_series_resistor(&net, &components)
        {
            gaps.push(Gap {
                kind: kind.into(),
                refdes: None,
                net: Some(net.clone()),
                suggestion: format!("add a series resistor in {net} near its controller"),
            });
        }
    }
}

fn audit_connector_protection(design: &Design, symbols: &SymbolTable, gaps: &mut Vec<Gap>) {
    let components: Vec<&Component> = design
        .blocks
        .values()
        .flat_map(|block| block.components.values())
        .filter(|component| !component.dnp)
        .collect();
    for block in design.blocks.values() {
        for (refdes, component) in &block.components {
            if component.dnp || !is_connector_like(&component.part) {
                continue;
            }
            let Some(meta) = symbols.symbol(&component.part) else {
                continue;
            };
            for pin in resolved_pins(component, &meta) {
                if !is_bus_net(&pin.net) {
                    continue;
                }
                let protected = has_protection(&pin.net, &components, symbols);
                let filtered = has_signal_shunt_cap(&pin.net, &components);
                if matches!(pin.etype, PinType::PowerInput | PinType::PowerOutput)
                    || is_power_net(&pin.net)
                    || protected && (!is_uart_signal(&pin.net) || filtered)
                {
                    continue;
                }
                let support = match (protected, is_uart_signal(&pin.net), filtered) {
                    (false, true, false) => "bidirectional TVS/ESD protection and a 100pF shunt capacitor",
                    (false, _, _) => "bidirectional TVS/ESD protection",
                    (true, true, false) => "a 100pF shunt capacitor",
                    (true, _, _) => continue,
                };
                gaps.push(Gap {
                    kind: "connector_protection".into(),
                    refdes: Some(refdes.clone()),
                    net: Some(pin.net.clone()),
                    suggestion: format!(
                        "add {support} from {refdes}.{} ({}) to GND at the connector",
                        display_pin(&pin),
                        pin.net
                    ),
                });
            }
        }
    }
}

fn audit_bus_power_support(
    design: &Design,
    symbols: &SymbolTable,
    matched: &MatchedIdioms,
    gaps: &mut Vec<Gap>,
) {
    if !all_nets(design).iter().any(|net| is_bus_net(net)) {
        return;
    }
    let components: Vec<&Component> = design
        .blocks
        .values()
        .flat_map(|block| block.components.values())
        .filter(|component| !component.dnp)
        .collect();
    let mut rails = BTreeSet::new();
    for component in components.iter().copied().filter(|component| is_ic(component)) {
        let Some(meta) = symbols.symbol(&component.part) else {
            continue;
        };
        rails.extend(
            resolved_pins(component, &meta)
                .into_iter()
                .filter(|pin| {
                    pin.etype == PinType::PowerInput
                        && !is_ground(&pin.net)
                        && is_supply_input(pin)
                })
                .map(|pin| pin.net),
        );
    }
    for rail in rails {
        if !components
            .iter()
            .copied()
            .any(|component| is_bulk_cap(component, &rail))
        {
            gaps.push(Gap {
                kind: "bulk_capacitor".into(),
                refdes: None,
                net: Some(rail.clone()),
                suggestion: format!("add 4.7uF between {rail} and GND near the powered block"),
            });
        }
        let has_entry = components.iter().copied().any(|component| {
            is_connector_like(&component.part) && component_nets(component).contains(&rail)
        });
        if has_entry && !has_protection(&rail, &components, symbols) {
            gaps.push(Gap {
                kind: "power_entry_protection".into(),
                refdes: None,
                net: Some(rail.clone()),
                suggestion: format!(
                    "add a bidirectional supply TVS from {rail} to GND at its connector"
                ),
            });
        } else if !has_entry && !has_power_output(&rail, &components, symbols) {
            gaps.push(Gap {
                kind: "power_entry".into(),
                refdes: None,
                net: Some(rail.clone()),
                suggestion: format!(
                    "add a 2-pin power connector feeding {rail} through a resettable fuse and reverse-polarity diode, with a TVS from the protected rail to GND"
                ),
            });
        }
    }
    if !matched.led_indicator {
        gaps.push(Gap {
            kind: "bus_indicator".into(),
            refdes: None,
            net: None,
            suggestion: "add a power/status LED with its own current-limiting resistor".into(),
        });
    }
}

fn connected_pins(component: &Component) -> impl Iterator<Item = (&str, &str)> {
    component
        .pins
        .iter()
        .chain(component.units.values().flatten())
        .filter_map(|(key, target)| match target {
            PinTarget::Net(net) => Some((key.as_str(), net.as_str())),
            PinTarget::NoConnect => None,
        })
}

fn resolved_pins(component: &Component, meta: &SymbolMeta) -> Vec<ResolvedPin> {
    connected_pins(component)
        .flat_map(|(key, net)| {
            crate::pins::resolve(meta, key)
                .into_iter()
                .map(move |pin| ResolvedPin {
                    number: pin.number.clone(),
                    name: pin.name.clone(),
                    net: net.to_string(),
                    etype: pin.etype,
                })
        })
        .collect()
}

fn display_pin(pin: &ResolvedPin) -> &str {
    if pin.name == "~" || pin.name.is_empty() {
        &pin.number
    } else {
        &pin.name
    }
}

fn component_nets(component: &Component) -> BTreeSet<String> {
    connected_pins(component)
        .map(|(_, net)| net.to_string())
        .collect()
}

fn all_nets(design: &Design) -> BTreeSet<String> {
    design
        .nets
        .keys()
        .cloned()
        .chain(
            design
                .blocks
                .values()
                .flat_map(|block| block.components.values())
                .flat_map(component_nets),
        )
        .collect()
}

fn is_ic(component: &Component) -> bool {
    !is_connector_like(&component.part)
        && !is_resistor(component)
        && !is_capacitor(component)
        && !component.part.to_ascii_lowercase().starts_with("power:")
}

fn is_resistor(component: &Component) -> bool {
    let part = component.part.to_ascii_uppercase();
    part == "R" || part.ends_with(":R") || part.contains(":R_")
}

fn is_capacitor(component: &Component) -> bool {
    let part = component.part.to_ascii_uppercase();
    part == "C" || part.ends_with(":C") || part.contains(":C_") || part.ends_with(":CP")
}

fn is_switch(component: &Component) -> bool {
    let part = component.part.to_ascii_uppercase();
    part.contains("SWITCH") || part.contains(":SW_") || part.contains("JUMPER")
}

fn is_bypass_cap(component: &Component, rail: &str) -> bool {
    if component.dnp || !is_capacitor(component) {
        return false;
    }
    let nets = component_nets(component);
    nets.contains(rail) && nets.iter().any(|net| is_ground(net))
}

fn far_net(component: &Component, net: &str) -> Option<String> {
    let nets = component_nets(component);
    (nets.len() == 2 && nets.contains(net))
        .then(|| nets.into_iter().find(|candidate| candidate != net))
        .flatten()
}

fn has_pull<'a>(net: &str, components: impl Iterator<Item = &'a Component>) -> bool {
    components
        .filter(|component| is_resistor(component))
        .any(|resistor| far_net(resistor, net).is_some_and(|far| is_power_net(&far)))
}

fn has_pull_to_power(net: &str, components: &[&Component]) -> bool {
    components
        .iter()
        .copied()
        .filter(|component| is_resistor(component))
        .any(|resistor| {
            far_net(resistor, net).is_some_and(|far| is_power_net(&far) && !is_ground(&far))
        })
}

fn has_series_resistor(net: &str, components: &[&Component]) -> bool {
    components
        .iter()
        .copied()
        .filter(|component| is_resistor(component))
        .any(|resistor| far_net(resistor, net).is_some_and(|far| !is_power_net(&far) && far != net))
}

fn has_protection(net: &str, components: &[&Component], symbols: &SymbolTable) -> bool {
    components.iter().copied().any(|component| {
        let part = component.part.to_ascii_uppercase();
        let metadata = symbols.symbol(&component.part);
        let description = metadata
            .as_ref()
            .and_then(|meta| meta.description.as_deref())
            .unwrap_or_default()
            .to_ascii_uppercase();
        let keywords = metadata
            .as_ref()
            .and_then(|meta| meta.keywords.as_deref())
            .unwrap_or_default()
            .to_ascii_uppercase();
        (part.contains("TVS")
            || part.contains("ESD")
            || part.contains("TRANSIL")
            || part.contains("VARISTOR")
            || description.contains("TVS")
            || description.contains("TRANSIENT VOLTAGE")
            || keywords.contains("TRANSIL")
            || keywords.contains("TRANSIENT VOLTAGE"))
            && component_nets(component).contains(net)
    })
}

fn has_signal_shunt_cap(net: &str, components: &[&Component]) -> bool {
    components.iter().copied().any(|component| {
        is_capacitor(component)
            && component_nets(component).contains(net)
            && component_nets(component).iter().any(|other| is_ground(other))
            && component
                .value
                .as_deref()
                .and_then(crate::erc::parse_value)
                .is_some_and(|value| value <= 10e-9)
    })
}

fn is_bulk_cap(component: &Component, rail: &str) -> bool {
    is_bypass_cap(component, rail)
        && component
            .value
            .as_deref()
            .and_then(|value| value.split_whitespace().next())
            .and_then(crate::erc::parse_value)
            .is_some_and(|value| value >= 1e-6)
}

fn has_power_output(rail: &str, components: &[&Component], symbols: &SymbolTable) -> bool {
    components
        .iter()
        .copied()
        .filter(|component| is_ic(component))
        .any(|component| {
            symbols.symbol(&component.part).is_some_and(|meta| {
                resolved_pins(component, &meta)
                    .iter()
                    .any(|pin| pin.etype == PinType::PowerOutput && pin.net == rail)
            })
        })
}

fn has_resistive_path(from: &str, to: &str, components: &[&Component]) -> bool {
    let mut edges: BTreeMap<String, Vec<(String, bool)>> = BTreeMap::new();
    for component in components {
        let resistor = is_resistor(component);
        if !resistor && !is_switch(component) {
            continue;
        }
        let nets: Vec<String> = component_nets(component).into_iter().collect();
        if let [a, b] = nets.as_slice() {
            edges
                .entry(a.clone())
                .or_default()
                .push((b.clone(), resistor));
            edges
                .entry(b.clone())
                .or_default()
                .push((a.clone(), resistor));
        }
    }
    let mut queue = VecDeque::from([(from.to_string(), false)]);
    let mut seen = BTreeSet::new();
    while let Some((net, has_resistor)) = queue.pop_front() {
        if net == to && has_resistor {
            return true;
        }
        if !seen.insert((net.clone(), has_resistor)) {
            continue;
        }
        for (next, edge_is_resistor) in edges.get(&net).into_iter().flatten() {
            queue.push_back((next.clone(), has_resistor || *edge_is_resistor));
        }
    }
    false
}

fn normalized(name: &str) -> String {
    name.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_uppercase())
        .collect()
}

fn can_kind(net: &str) -> Option<(String, bool)> {
    let name = normalized(net);
    let (family, high) = if let Some(family) = name.strip_suffix('H') {
        (family, true)
    } else if let Some(family) = name.strip_suffix('L') {
        (family, false)
    } else {
        return None;
    };
    family
        .starts_with("CAN")
        .then(|| (family.to_string(), high))
}

fn i2c_kind(net: &str) -> Option<&'static str> {
    let name = normalized(net);
    if name == "SDA" || name.starts_with("I2C") && name.ends_with("SDA") {
        Some("SDA")
    } else if name == "SCL" || name.starts_with("I2C") && name.ends_with("SCL") {
        Some("SCL")
    } else {
        None
    }
}

fn is_usb_data(net: &str) -> bool {
    let upper = net.to_ascii_uppercase();
    upper.contains("USB")
        && (upper.ends_with("D+")
            || upper.ends_with("D-")
            || upper.ends_with("_DP")
            || upper.ends_with("_DM")
            || upper.ends_with("DPLUS")
            || upper.ends_with("DMINUS"))
}

fn is_spi_cs(net: &str) -> bool {
    let name = normalized(net);
    name.starts_with("SPI") && (name.ends_with("CS") || name.ends_with("NSS"))
}

fn is_uart_signal(net: &str) -> bool {
    let name = normalized(net);
    matches!(name.as_str(), "TX" | "TXD" | "RX" | "RXD")
        || ["CAN", "UART", "USART"].iter().any(|prefix| {
            name.starts_with(prefix)
                && ["TX", "TXD", "RX", "RXD"]
                    .iter()
                    .any(|suffix| name.ends_with(suffix))
        })
}

fn is_bus_net(net: &str) -> bool {
    can_kind(net).is_some()
        || i2c_kind(net).is_some()
        || is_usb_data(net)
        || is_spi_cs(net)
        || is_uart_signal(net)
}

fn is_control_pin(name: &str) -> bool {
    let name = normalized(name);
    matches!(
        name.as_str(),
        "EN" | "ENABLE" | "NRST" | "NRESET" | "RESET" | "RESETN" | "RST" | "RSTN"
    ) || name.starts_with("BOOT")
}

fn is_supply_input(pin: &ResolvedPin) -> bool {
    let name = normalized(&pin.name);
    is_power_net(&pin.net)
        || matches!(
            name.as_str(),
            "VIN"
                | "VCC"
                | "VDD"
                | "VDDA"
                | "VDDD"
                | "AVCC"
                | "AVDD"
                | "VBAT"
                | "VBUS"
                | "VS"
                | "VMOT"
        )
        || name.starts_with("VCC")
        || name.starts_with("VDD")
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use super::*;
    use crate::{Block, Component, PinTarget};

    fn component(part: &str, pins: &[(&str, &str)]) -> Component {
        Component {
            part: part.into(),
            pins: pins
                .iter()
                .map(|(pin, net)| ((*pin).into(), PinTarget::Net((*net).into())))
                .collect(),
            ..Component::default()
        }
    }

    fn design(parts: &[(&str, Component)]) -> Design {
        Design {
            blocks: IndexMap::from([(
                "main".into(),
                Block {
                    components: parts
                        .iter()
                        .map(|(refdes, component)| ((*refdes).into(), component.clone()))
                        .collect(),
                    ..Block::default()
                },
            )]),
            ..Design::default()
        }
    }

    fn symbols() -> SymbolTable {
        let mut symbols = SymbolTable::with_basics();
        symbols.mock_add(
            "MCU:Small",
            vec![
                ("1", "VDD", PinType::PowerInput, 1),
                ("2", "GND", PinType::PowerInput, 1),
                ("3", "GPIO", PinType::Other, 1),
            ],
        );
        symbols.mock_add(
            "Interface_CAN_LIN:CAN",
            vec![
                ("1", "VCC", PinType::PowerInput, 1),
                ("2", "GND", PinType::PowerInput, 1),
                ("3", "CANH", PinType::Other, 1),
                ("4", "CANL", PinType::Other, 1),
            ],
        );
        symbols
    }

    #[test]
    fn mcu_without_bypass_has_gap() {
        let design = design(&[(
            "U1",
            component("MCU:Small", &[("1", "+3V3"), ("2", "GND"), ("3", "IO")]),
        )]);

        let gaps = audit(&design, &symbols());

        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].kind, "bypass");
        assert_eq!(gaps[0].refdes.as_deref(), Some("U1"));
    }

    #[test]
    fn mcu_with_100nf_bypass_has_no_gap() {
        let design = design(&[
            (
                "U1",
                component("MCU:Small", &[("1", "+3V3"), ("2", "GND"), ("3", "IO")]),
            ),
            (
                "C1",
                Component {
                    value: Some("100nF".into()),
                    ..component("Device:C", &[("1", "+3V3"), ("2", "GND")])
                },
            ),
        ]);

        assert!(audit(&design, &symbols()).is_empty());
    }

    #[test]
    fn can_transceiver_without_termination_has_gap() {
        let design = design(&[
            (
                "U1",
                component(
                    "Interface_CAN_LIN:CAN",
                    &[("1", "+3V3"), ("2", "GND"), ("3", "CAN_H"), ("4", "CAN_L")],
                ),
            ),
            (
                "C1",
                Component {
                    value: Some("100nF".into()),
                    ..component("Device:C", &[("1", "+3V3"), ("2", "GND")])
                },
            ),
        ]);

        let gaps = audit(&design, &symbols());

        let termination = gaps
            .iter()
            .find(|gap| gap.kind == "can_termination")
            .expect("CAN termination gap");
        assert_eq!(termination.net.as_deref(), Some("CAN_H/CAN_L"));
    }
}
