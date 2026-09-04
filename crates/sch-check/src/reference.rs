//! The reference netlist a request pins the design to, and how the design compares.
//!
//! When the user hands over a netlist and forbids additions, "is it right" is not
//! a judgement call: every named part must exist, nothing else may, and every pin
//! must sit with exactly the pins the reference puts it with. Net *names* are the
//! author's to choose, so nets are compared by the pins they carry, not by name.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{Design, PinTarget};

/// One part of a reference netlist.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ReferencePart {
    #[serde(rename = "ref")]
    pub refdes: String,
    pub lib_id: String,
    #[serde(default)]
    pub value: Option<String>,
    /// Pin number → net name; `"nc"` means the pin stays unconnected.
    #[serde(default)]
    pub pins: BTreeMap<String, String>,
}

/// The netlist a request says the design must reproduce.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct ReferenceNetlist {
    pub parts: Vec<ReferencePart>,
}

impl ReferenceNetlist {
    /// Parse the JSON array the request carries.
    pub fn parse(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }
}

/// A pin the design does not put where the reference does.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MisNettedPin {
    /// `U1.3`.
    pub pin: String,
    /// The reference net, or `null` when the reference leaves the pin unconnected.
    pub expected_net: Option<String>,
    /// The net the design puts it on, or `null` when it is unconnected.
    pub actual_net: Option<String>,
}

/// How faithfully the design reproduces a reference netlist.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Fidelity {
    /// Nothing missing, nothing extra, every pin where the reference puts it.
    pub matches: bool,
    pub reference_parts: usize,
    /// Parts the reference names that the design does not have.
    pub parts_missing: Vec<String>,
    /// Parts the design has that the reference never named.
    pub parts_extra: Vec<String>,
    pub pins_mis_netted_total: usize,
    /// The first [`MAX_REPORTED_PINS`] mis-netted pins.
    pub pins_mis_netted: Vec<MisNettedPin>,
}

/// How many mis-netted pins one report names before it truncates.
pub const MAX_REPORTED_PINS: usize = 40;

/// Compare a design against the netlist it was asked to reproduce.
///
/// Power symbols (`#PWR`, `#FLG`) are the author's way of drawing a rail, never
/// parts of the reference, so they are excluded from the part comparison and
/// from every net's pin set.
pub fn compare(reference: &ReferenceNetlist, design: &Design) -> Fidelity {
    let expected_refs: BTreeSet<&str> = reference
        .parts
        .iter()
        .map(|part| part.refdes.as_str())
        .collect();
    let delivered = delivered_pins(design);
    let delivered_refs = delivered_refs(design);

    let anchors = anchor_nets(reference, &delivered, &delivered_refs);
    let mut mis_netted = Vec::new();
    for part in &reference.parts {
        if !delivered_refs.contains(part.refdes.as_str()) {
            continue;
        }
        for (number, net) in &part.pins {
            let key = (part.refdes.clone(), number.clone());
            let Some(actual) = delivered.get(&key) else {
                continue;
            };
            let expected = (!is_nc(net)).then_some(net.as_str());
            let want = expected
                .and_then(|net| anchors.get(net))
                .map(String::as_str);
            if want == actual.as_deref() {
                continue;
            }
            mis_netted.push(MisNettedPin {
                pin: format!("{}.{number}", part.refdes),
                expected_net: expected.map(str::to_owned),
                actual_net: actual.clone(),
            });
        }
    }

    let parts_missing = difference(&expected_refs, &delivered_refs);
    let parts_extra = difference(&delivered_refs, &expected_refs);
    let pins_mis_netted_total = mis_netted.len();
    mis_netted.truncate(MAX_REPORTED_PINS);
    Fidelity {
        matches: parts_missing.is_empty() && parts_extra.is_empty() && pins_mis_netted_total == 0,
        reference_parts: reference.parts.len(),
        parts_missing,
        parts_extra,
        pins_mis_netted_total,
        pins_mis_netted: mis_netted,
    }
}

/// Every part the design draws, power symbols excluded.
fn delivered_refs(design: &Design) -> BTreeSet<&str> {
    design
        .blocks
        .values()
        .flat_map(|block| block.components.keys())
        .map(String::as_str)
        .filter(|refdes| !is_power_symbol(refdes))
        .collect()
}

/// `(refdes, pin number)` → the net it carries, `None` when no-connect.
/// Power symbols never appear.
fn delivered_pins(design: &Design) -> BTreeMap<(String, String), Option<String>> {
    let mut pins = BTreeMap::new();
    for block in design.blocks.values() {
        for (refdes, component) in &block.components {
            if is_power_symbol(refdes) {
                continue;
            }
            for (number, target) in &component.pins {
                let net = match target {
                    PinTarget::Net(net) => Some(net.clone()),
                    PinTarget::NoConnect => None,
                };
                pins.insert((refdes.clone(), number.clone()), net);
            }
        }
    }
    pins
}

/// Pair each reference net with the delivered net that carries most of its pins,
/// one to one.
///
/// The matching has to be injective: two reference nets shorted onto one
/// delivered net must not both come out "as expected" — only the net that owns
/// the most of those pins keeps it, and the other's pins are reported.
fn anchor_nets(
    reference: &ReferenceNetlist,
    delivered: &BTreeMap<(String, String), Option<String>>,
    delivered_refs: &BTreeSet<&str>,
) -> BTreeMap<String, String> {
    let mut overlaps: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for part in &reference.parts {
        if !delivered_refs.contains(part.refdes.as_str()) {
            continue;
        }
        for (number, net) in &part.pins {
            if is_nc(net) {
                continue;
            }
            let key = (part.refdes.clone(), number.clone());
            if let Some(Some(actual)) = delivered.get(&key) {
                *overlaps.entry((net.as_str(), actual.as_str())).or_default() += 1;
            }
        }
    }
    let mut ranked: Vec<_> = overlaps
        .into_iter()
        .map(|((expected, actual), count)| (count, expected, actual))
        .collect();
    ranked.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.cmp(right.1))
            .then_with(|| left.2.cmp(right.2))
    });

    let mut anchors = BTreeMap::new();
    let mut taken: BTreeSet<&str> = BTreeSet::new();
    for (_, expected, actual) in ranked {
        if anchors.contains_key(expected) || taken.contains(actual) {
            continue;
        }
        anchors.insert(expected.to_owned(), actual.to_owned());
        taken.insert(actual);
    }
    anchors
}

fn difference(left: &BTreeSet<&str>, right: &BTreeSet<&str>) -> Vec<String> {
    left.difference(right).map(|s| (*s).to_owned()).collect()
}

fn is_nc(net: &str) -> bool {
    net.eq_ignore_ascii_case("nc")
}

fn is_power_symbol(refdes: &str) -> bool {
    refdes.starts_with('#')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Block, Component};

    const NETLIST: &str = r#"[
      {"ref":"R1","lib_id":"Device:R","value":"1k","pins":{"1":"IN","2":"OUT"}},
      {"ref":"C1","lib_id":"Device:C","value":"100n","pins":{"1":"OUT","2":"GND"}},
      {"ref":"U1","lib_id":"Device:Opamp","pins":{"1":"OUT","2":"GND","3":"nc"}}
    ]"#;

    /// One part: its refdes and each pin number's net, `None` for no-connect.
    type Part<'a> = (&'a str, &'a [(&'a str, Option<&'a str>)]);

    fn design(parts: &[Part<'_>]) -> Design {
        let mut block = Block::default();
        for (refdes, pins) in parts {
            let mut component = Component::default();
            for (number, net) in *pins {
                let target = match net {
                    Some(net) => PinTarget::Net((*net).to_owned()),
                    None => PinTarget::NoConnect,
                };
                component.pins.insert((*number).to_owned(), target);
            }
            block.components.insert((*refdes).to_owned(), component);
        }
        let mut design = Design::default();
        design.blocks.insert("main".to_owned(), block);
        design
    }

    fn faithful() -> Design {
        design(&[
            ("R1", &[("1", Some("IN")), ("2", Some("OUT"))]),
            ("C1", &[("1", Some("OUT")), ("2", Some("GND"))]),
            ("U1", &[("1", Some("OUT")), ("2", Some("GND")), ("3", None)]),
            ("#PWR01", &[("1", Some("GND"))]),
        ])
    }

    #[test]
    fn a_faithful_design_matches_whatever_the_nets_are_called() {
        let reference = ReferenceNetlist::parse(NETLIST).unwrap();
        let fidelity = compare(&reference, &faithful());
        assert!(fidelity.matches, "{fidelity:?}");
        assert_eq!(fidelity.reference_parts, 3);

        let renamed = design(&[
            ("R1", &[("1", Some("N$7")), ("2", Some("N$8"))]),
            ("C1", &[("1", Some("N$8")), ("2", Some("GND"))]),
            ("U1", &[("1", Some("N$8")), ("2", Some("GND")), ("3", None)]),
        ]);
        assert!(compare(&reference, &renamed).matches);
    }

    #[test]
    fn an_invented_part_is_extra() {
        let reference = ReferenceNetlist::parse(NETLIST).unwrap();
        let mut with_extra = faithful();
        with_extra.blocks["main"]
            .components
            .insert("D1".to_owned(), Component::default());
        let fidelity = compare(&reference, &with_extra);
        assert!(!fidelity.matches);
        assert_eq!(fidelity.parts_extra, ["D1"]);
        assert!(fidelity.parts_missing.is_empty());
    }

    #[test]
    fn a_dropped_part_is_missing() {
        let reference = ReferenceNetlist::parse(NETLIST).unwrap();
        let mut short = faithful();
        short.blocks["main"].components.shift_remove("C1");
        let fidelity = compare(&reference, &short);
        assert_eq!(fidelity.parts_missing, ["C1"]);
        assert_eq!(fidelity.pins_mis_netted_total, 0);
    }

    #[test]
    fn a_pin_on_the_wrong_net_is_named() {
        let reference = ReferenceNetlist::parse(NETLIST).unwrap();
        let wrong = design(&[
            ("R1", &[("1", Some("IN")), ("2", Some("GND"))]),
            ("C1", &[("1", Some("OUT")), ("2", Some("GND"))]),
            ("U1", &[("1", Some("OUT")), ("2", Some("GND")), ("3", None)]),
        ]);
        let fidelity = compare(&reference, &wrong);
        assert!(!fidelity.matches);
        assert!(fidelity.parts_extra.is_empty());
        assert_eq!(
            fidelity.pins_mis_netted,
            [MisNettedPin {
                pin: "R1.2".to_owned(),
                expected_net: Some("OUT".to_owned()),
                actual_net: Some("GND".to_owned()),
            }]
        );
    }

    /// Two reference nets shorted onto one delivered net: the net that owns
    /// fewer of the pins loses the anchor, so the short is reported.
    #[test]
    fn a_short_between_two_reference_nets_is_reported() {
        let reference = ReferenceNetlist::parse(NETLIST).unwrap();
        let shorted = design(&[
            ("R1", &[("1", Some("OUT")), ("2", Some("OUT"))]),
            ("C1", &[("1", Some("OUT")), ("2", Some("GND"))]),
            ("U1", &[("1", Some("OUT")), ("2", Some("GND")), ("3", None)]),
        ]);
        let fidelity = compare(&reference, &shorted);
        assert_eq!(
            fidelity.pins_mis_netted,
            [MisNettedPin {
                pin: "R1.1".to_owned(),
                expected_net: Some("IN".to_owned()),
                actual_net: Some("OUT".to_owned()),
            }]
        );
    }

    #[test]
    fn a_wired_no_connect_pin_is_reported() {
        let reference = ReferenceNetlist::parse(NETLIST).unwrap();
        let wired = design(&[
            ("R1", &[("1", Some("IN")), ("2", Some("OUT"))]),
            ("C1", &[("1", Some("OUT")), ("2", Some("GND"))]),
            (
                "U1",
                &[("1", Some("OUT")), ("2", Some("GND")), ("3", Some("IN"))],
            ),
        ]);
        let fidelity = compare(&reference, &wired);
        assert_eq!(fidelity.pins_mis_netted.len(), 1);
        assert_eq!(fidelity.pins_mis_netted[0].pin, "U1.3");
        assert_eq!(fidelity.pins_mis_netted[0].expected_net, None);
    }
}
