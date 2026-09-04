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
    /// Pin number → net name; `"nc"` means the pin stays unconnected.
    #[serde(default)]
    pub pins: BTreeMap<String, String>,
}

/// The netlist a request says the design must reproduce.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(from = "Document")]
pub struct ReferenceNetlist {
    pub parts: Vec<ReferencePart>,
}

/// Both shapes a hand-written or exported reference comes in: the bare array of
/// parts, and the `{title, parts}` document the dataset extractor writes.
#[derive(Deserialize)]
#[serde(untagged)]
enum Document {
    Parts(Vec<ReferencePart>),
    Titled { parts: Vec<ReferencePart> },
}

impl From<Document> for ReferenceNetlist {
    fn from(document: Document) -> Self {
        let parts = match document {
            Document::Parts(parts) | Document::Titled { parts } => parts,
        };
        Self { parts }
    }
}

impl ReferenceNetlist {
    /// Parse the JSON the request carries.
    pub fn parse(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }
}

/// A pin the design does not put where the reference does.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MisNettedPin {
    /// `U1.3`.
    pub pin: String,
    /// The reference net, or `null` when the reference leaves the pin unconnected
    /// or never names it.
    pub expected_net: Option<String>,
    /// The net the design puts it on, or `null` when it is unconnected or the
    /// placed symbol has no such pin.
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

/// Where a pin sits, as the two sides state it.
///
/// The three states are what keeps "both unconnected" apart from "the reference
/// wires this and the design does not": collapsing either onto `None` would call
/// an entirely unwired signal faithful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Site<'a> {
    Unconnected,
    Net(&'a str),
    /// A reference net no delivered net stands for, or a pin the placed symbol
    /// does not have. Never equal to anything, including itself.
    Nowhere,
}

impl Site<'_> {
    fn agrees_with(self, other: Self) -> bool {
        match (self, other) {
            (Site::Unconnected, Site::Unconnected) => true,
            (Site::Net(left), Site::Net(right)) => left == right,
            _ => false,
        }
    }

    fn net(self) -> Option<String> {
        match self {
            Site::Net(net) => Some(net.to_owned()),
            _ => None,
        }
    }
}

/// `(refdes, pin number)` → where the design puts that pin.
type DeliveredPins = BTreeMap<(String, String), Option<String>>;

/// Compare a design against the netlist it was asked to reproduce.
///
/// Power symbols are the author's way of drawing a rail, never parts of the
/// reference, so they are excluded from the part comparison and from every net's
/// pin set.
pub fn compare(reference: &ReferenceNetlist, design: &Design) -> Fidelity {
    let expected_refs: BTreeSet<&str> = reference
        .parts
        .iter()
        .map(|part| part.refdes.as_str())
        .collect();
    let delivered = delivered_pins(design);
    let delivered_refs = delivered_refs(design);
    let shared: BTreeSet<&str> = expected_refs
        .intersection(&delivered_refs)
        .copied()
        .collect();

    let anchors = anchor_nets(reference, &delivered, &shared);
    let mut mis_netted = mis_netted_pins(reference, &delivered, &shared, &anchors);
    mis_netted.extend(wired_pins_the_reference_never_names(
        reference, &delivered, &shared,
    ));
    mis_netted.sort_by(|left, right| left.pin.cmp(&right.pin));

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

/// Every reference pin the design does not put where the reference does.
fn mis_netted_pins(
    reference: &ReferenceNetlist,
    delivered: &DeliveredPins,
    shared: &BTreeSet<&str>,
    anchors: &BTreeMap<String, String>,
) -> Vec<MisNettedPin> {
    let mut mis_netted = Vec::new();
    for part in reference
        .parts
        .iter()
        .filter(|part| shared.contains(part.refdes.as_str()))
    {
        for (number, net) in &part.pins {
            let expected = if is_nc(net) {
                Site::Unconnected
            } else {
                anchors.get(net).map_or(Site::Nowhere, |net| Site::Net(net))
            };
            let actual = match delivered.get(&(part.refdes.clone(), number.clone())) {
                None => Site::Nowhere,
                Some(None) => Site::Unconnected,
                Some(Some(net)) => Site::Net(net),
            };
            if expected.agrees_with(actual) {
                continue;
            }
            mis_netted.push(MisNettedPin {
                pin: format!("{}.{number}", part.refdes),
                expected_net: (!is_nc(net)).then(|| net.clone()),
                actual_net: actual.net(),
            });
        }
    }
    mis_netted
}

/// Pins the design wires on a reference part that the reference never lists —
/// the placed symbol is not the one the netlist describes.
fn wired_pins_the_reference_never_names(
    reference: &ReferenceNetlist,
    delivered: &DeliveredPins,
    shared: &BTreeSet<&str>,
) -> Vec<MisNettedPin> {
    let named: BTreeSet<(&str, &str)> = reference
        .parts
        .iter()
        .flat_map(|part| {
            part.pins
                .keys()
                .map(move |number| (part.refdes.as_str(), number.as_str()))
        })
        .collect();
    delivered
        .iter()
        .filter(|((refdes, number), net)| {
            net.is_some()
                && shared.contains(refdes.as_str())
                && !named.contains(&(refdes.as_str(), number.as_str()))
        })
        .map(|((refdes, number), net)| MisNettedPin {
            pin: format!("{refdes}.{number}"),
            expected_net: None,
            actual_net: net.clone(),
        })
        .collect()
}

/// Every part the design draws, power symbols excluded.
fn delivered_refs(design: &Design) -> BTreeSet<&str> {
    design
        .blocks
        .values()
        .flat_map(|block| block.components.iter())
        .filter(|(refdes, component)| !is_power_symbol(refdes, &component.part))
        .map(|(refdes, _)| refdes.as_str())
        .collect()
}

/// `(refdes, pin number)` → the net it carries, `None` when no-connect. Power
/// symbols never appear; a multi-unit part's units are one part's pins.
fn delivered_pins(design: &Design) -> DeliveredPins {
    let mut pins = BTreeMap::new();
    for block in design.blocks.values() {
        for (refdes, component) in &block.components {
            if is_power_symbol(refdes, &component.part) {
                continue;
            }
            let all = component
                .pins
                .iter()
                .chain(component.units.values().flatten());
            for (number, target) in all {
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

/// Pair each reference net with the delivered net that carries most of its pins.
///
/// Greedily, highest overlap first, and one to one: two reference nets shorted
/// onto a single delivered net must not both come out "as expected" — only the
/// one that owns the most of those pins keeps it, and the other is left
/// unanchored so every pin on it is reported.
fn anchor_nets(
    reference: &ReferenceNetlist,
    delivered: &DeliveredPins,
    shared: &BTreeSet<&str>,
) -> BTreeMap<String, String> {
    let mut overlaps: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for part in reference
        .parts
        .iter()
        .filter(|part| shared.contains(part.refdes.as_str()))
    {
        for (number, net) in part.pins.iter().filter(|(_, net)| !is_nc(net)) {
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

/// A rail or flag symbol: drawing, not a part of the circuit.
fn is_power_symbol(refdes: &str, part: &str) -> bool {
    refdes.starts_with('#') || refdes.is_empty() || part.starts_with("power:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Block, Component};

    const NETLIST: &str = r#"{"title": "T", "parts": [
      {"ref":"R1","lib_id":"Device:R","value":"1k","pins":{"1":"IN","2":"OUT"}},
      {"ref":"C1","lib_id":"Device:C","value":"100n","pins":{"1":"OUT","2":"GND"}},
      {"ref":"U1","lib_id":"Device:Opamp","pins":{"1":"OUT","2":"GND","3":"nc"}}
    ]}"#;

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

    fn reference() -> ReferenceNetlist {
        ReferenceNetlist::parse(NETLIST).unwrap()
    }

    /// The dataset extractor writes `{title, parts}`; a hand-written reference is
    /// often the bare array. Both are the same netlist.
    #[test]
    fn both_document_shapes_parse_to_the_same_netlist() {
        let bare = r#"[{"ref":"R1","lib_id":"Device:R","pins":{"1":"IN"}}]"#;
        let titled =
            r#"{"title":"T","parts":[{"ref":"R1","lib_id":"Device:R","pins":{"1":"IN"}}]}"#;
        assert_eq!(
            ReferenceNetlist::parse(bare).unwrap(),
            ReferenceNetlist::parse(titled).unwrap()
        );
    }

    /// Every `dataset-*` case is measured against one of these files, so the
    /// parser has to read the real thing, not a shape invented for a test.
    #[test]
    fn the_dataset_cases_reference_netlists_parse() {
        let cases = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../quality/cases");
        let mut read = 0;
        for case in std::fs::read_dir(&cases).expect("quality cases").flatten() {
            let path = case.path().join("input/netlist.json");
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let netlist = ReferenceNetlist::parse(&text)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            assert!(!netlist.parts.is_empty(), "{}", path.display());
            read += 1;
        }
        assert!(read >= 8, "expected the dataset cases, read {read}");
    }

    #[test]
    fn a_faithful_design_matches_whatever_the_nets_are_called() {
        let fidelity = compare(&reference(), &faithful());
        assert!(fidelity.matches, "{fidelity:?}");
        assert_eq!(fidelity.reference_parts, 3);

        let renamed = design(&[
            ("R1", &[("1", Some("N$7")), ("2", Some("N$8"))]),
            ("C1", &[("1", Some("N$8")), ("2", Some("GND"))]),
            ("U1", &[("1", Some("N$8")), ("2", Some("GND")), ("3", None)]),
        ]);
        assert!(compare(&reference(), &renamed).matches);
    }

    #[test]
    fn an_invented_part_is_extra() {
        let mut with_extra = faithful();
        with_extra.blocks["main"]
            .components
            .insert("D1".to_owned(), Component::default());
        let fidelity = compare(&reference(), &with_extra);
        assert!(!fidelity.matches);
        assert_eq!(fidelity.parts_extra, ["D1"]);
        assert!(fidelity.parts_missing.is_empty());
    }

    #[test]
    fn a_dropped_part_is_missing() {
        let mut short = faithful();
        short.blocks["main"].components.shift_remove("C1");
        let fidelity = compare(&reference(), &short);
        assert_eq!(fidelity.parts_missing, ["C1"]);
        assert_eq!(fidelity.pins_mis_netted_total, 0);
    }

    #[test]
    fn a_pin_on_the_wrong_net_is_named() {
        let wrong = design(&[
            ("R1", &[("1", Some("IN")), ("2", Some("GND"))]),
            ("C1", &[("1", Some("OUT")), ("2", Some("GND"))]),
            ("U1", &[("1", Some("OUT")), ("2", Some("GND")), ("3", None)]),
        ]);
        let fidelity = compare(&reference(), &wrong);
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
        let shorted = design(&[
            ("R1", &[("1", Some("OUT")), ("2", Some("OUT"))]),
            ("C1", &[("1", Some("OUT")), ("2", Some("GND"))]),
            ("U1", &[("1", Some("OUT")), ("2", Some("GND")), ("3", None)]),
        ]);
        assert_eq!(
            compare(&reference(), &shorted).pins_mis_netted,
            [MisNettedPin {
                pin: "R1.1".to_owned(),
                expected_net: Some("IN".to_owned()),
                actual_net: Some("OUT".to_owned()),
            }]
        );
    }

    #[test]
    fn a_wired_no_connect_pin_is_reported() {
        let wired = design(&[
            ("R1", &[("1", Some("IN")), ("2", Some("OUT"))]),
            ("C1", &[("1", Some("OUT")), ("2", Some("GND"))]),
            (
                "U1",
                &[("1", Some("OUT")), ("2", Some("GND")), ("3", Some("IN"))],
            ),
        ]);
        let fidelity = compare(&reference(), &wired);
        assert_eq!(fidelity.pins_mis_netted.len(), 1);
        assert_eq!(fidelity.pins_mis_netted[0].pin, "U1.3");
        assert_eq!(fidelity.pins_mis_netted[0].expected_net, None);
    }

    /// A signal the design never wired at all: both ends read "unconnected", and
    /// only the three-state comparison keeps that from looking faithful.
    #[test]
    fn an_entirely_unwired_reference_net_is_reported() {
        let unwired = design(&[
            ("R1", &[("1", None), ("2", Some("OUT"))]),
            ("C1", &[("1", Some("OUT")), ("2", Some("GND"))]),
            ("U1", &[("1", Some("OUT")), ("2", Some("GND")), ("3", None)]),
        ]);
        let fidelity = compare(&reference(), &unwired);
        assert!(!fidelity.matches, "{fidelity:?}");
        assert_eq!(
            fidelity.pins_mis_netted,
            [MisNettedPin {
                pin: "R1.1".to_owned(),
                expected_net: Some("IN".to_owned()),
                actual_net: None,
            }]
        );
    }

    /// The placed symbol is not the one the netlist describes: a reference pin it
    /// does not have, and a pin of its own the reference never names.
    #[test]
    fn a_symbol_with_the_wrong_pins_is_reported() {
        let wrong_symbol = design(&[
            ("R1", &[("1", Some("IN")), ("2", Some("OUT"))]),
            ("C1", &[("1", Some("OUT")), ("2", Some("GND"))]),
            (
                "U1",
                &[("1", Some("OUT")), ("2", Some("GND")), ("8", Some("GND"))],
            ),
        ]);
        let fidelity = compare(&reference(), &wrong_symbol);
        assert_eq!(
            fidelity.pins_mis_netted,
            [
                MisNettedPin {
                    pin: "U1.3".to_owned(),
                    expected_net: None,
                    actual_net: None,
                },
                MisNettedPin {
                    pin: "U1.8".to_owned(),
                    expected_net: None,
                    actual_net: Some("GND".to_owned()),
                },
            ]
        );
    }

    /// A multi-unit part's units are one part's pins, as they are everywhere else
    /// in this crate.
    #[test]
    fn a_multi_unit_parts_units_are_compared() {
        let mut split = faithful();
        let component = &mut split.blocks["main"].components["U1"];
        component.pins.shift_remove("2");
        component.units.insert(
            "B".to_owned(),
            [("2".to_owned(), PinTarget::Net("GND".to_owned()))]
                .into_iter()
                .collect(),
        );
        assert!(compare(&reference(), &split).matches);
    }

    #[test]
    fn the_reported_pins_are_capped_but_the_total_is_not() {
        let pins: Vec<(String, String)> = (0..MAX_REPORTED_PINS + 5)
            .map(|n| (n.to_string(), format!("N{n}")))
            .collect();
        let reference = ReferenceNetlist {
            parts: vec![ReferencePart {
                refdes: "J1".to_owned(),
                pins: pins.iter().cloned().collect(),
            }],
        };
        let pin_specs: Vec<(&str, Option<&str>)> = pins
            .iter()
            .map(|(number, _)| (number.as_str(), None))
            .collect();
        let unwired = design(&[("J1", &pin_specs)]);
        let fidelity = compare(&reference, &unwired);
        assert_eq!(fidelity.pins_mis_netted_total, MAX_REPORTED_PINS + 5);
        assert_eq!(fidelity.pins_mis_netted.len(), MAX_REPORTED_PINS);
    }

    #[test]
    fn an_empty_reference_and_an_empty_design_agree() {
        let empty = ReferenceNetlist { parts: Vec::new() };
        assert!(compare(&empty, &Design::default()).matches);
    }
}
