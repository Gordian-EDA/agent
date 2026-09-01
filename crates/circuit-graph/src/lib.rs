//! `circuit-graph` — an attributed circuit graph plus a generic, extensible idiom
//! matcher.
//!
//! A schematic is a hypergraph: components (nodes) joined by nets (hyperedges over
//! pins). Recurring sub-circuits — a crystal with its load caps, a decoupling bank,
//! an RC filter — are small **idioms** worth recognising so a layout engine can
//! co-place them. This crate models the graph ([`CircuitGraph`]) and matches it
//! against a **library of declarative patterns** ([`library`]) with an attributed
//! subgraph-similarity algorithm ([`matcher`]).
//!
//! Design goals:
//! - **Pure & testable.** No KiCAD, geometry, or I/O — just data in, matches out.
//!   The host (`sch-floorplan`) adapts its own structures into [`CircuitGraph`].
//! - **Extensible.** A new idiom is one [`pattern::Pattern`] value; the matcher is
//!   generic and never changes.
//! - **Robust.** Optional roles/edges yield a graph-similarity *score* so a
//!   near-miss degrades gracefully instead of vanishing.

pub mod graph;
pub mod library;
pub mod matcher;
pub mod netclass;
pub mod pattern;
pub mod value;

pub use graph::{CircuitGraph, NetKind, Node, Pin};
pub use matcher::{Match, find, find_all};
pub use pattern::{Edge, Mult, NetMatch, NodePred, Pattern, Role, Target};

#[cfg(test)]
mod tests {
    use super::*;

    /// A node helper for tests.
    fn node(refdes: &str, lib: &str, value: &str, pins: &[(&str, &str)]) -> Node {
        Node {
            refdes: refdes.into(),
            lib_id: lib.into(),
            value: value.into(),
            pins: pins
                .iter()
                .enumerate()
                .map(|(i, (name, net))| Pin {
                    number: (i + 1).to_string(),
                    name: (*name).into(),
                    net: (!net.is_empty()).then(|| net.to_string()),
                })
                .collect(),
        }
    }

    fn kind_of(net: &str) -> NetKind {
        match net {
            "GND" | "VSS" => NetKind::Ground,
            "+3V3" | "VCC" | "5V" => NetKind::Power,
            _ => NetKind::Signal,
        }
    }

    /// A minimal STM32-like graph: MCU with a crystal + 2 load caps and a 3-cap
    /// decoupling bank on +3V3.
    fn stm32_graph() -> CircuitGraph {
        let nodes = vec![
            node(
                "U1",
                "MCU_ST_STM32F1:STM32F103C8Tx",
                "",
                &[
                    ("VDD", "+3V3"),
                    ("VSS", "GND"),
                    ("OSC_IN", "XTAL1"),
                    ("OSC_OUT", "XTAL2"),
                    ("PA0", "SIG"),
                ],
            ),
            node(
                "Y1",
                "Device:Crystal",
                "8MHz",
                &[("1", "XTAL1"), ("2", "XTAL2")],
            ),
            node("C1", "Device:C", "22pF", &[("1", "XTAL1"), ("2", "GND")]),
            node("C2", "Device:C", "22pF", &[("1", "XTAL2"), ("2", "GND")]),
            node("C3", "Device:C", "100nF", &[("1", "+3V3"), ("2", "GND")]),
            node("C4", "Device:C", "100nF", &[("1", "+3V3"), ("2", "GND")]),
            node("C5", "Device:C", "100nF", &[("1", "+3V3"), ("2", "GND")]),
        ];
        CircuitGraph::new(nodes, kind_of)
    }

    #[test]
    fn matches_crystal_cluster() {
        let g = stm32_graph();
        let ms = find(&g, &library::CRYSTAL);
        assert_eq!(ms.len(), 1, "exactly one crystal cluster");
        let m = &ms[0];
        assert_eq!(m.anchor, "U1");
        assert!((m.score - 1.0).abs() < 1e-9, "full crystal scores 1.0");
        let mut caps: Vec<String> = m.bindings["cap_a"].clone();
        caps.extend(m.bindings["cap_b"].clone());
        caps.sort();
        assert_eq!(caps, vec!["C1".to_string(), "C2".to_string()]);
        assert_eq!(m.bindings["crystal"], vec!["Y1".to_string()]);
    }

    #[test]
    fn matches_decoupling_bank() {
        let g = stm32_graph();
        let ms = find(&g, &library::DECOUPLING);
        assert_eq!(ms.len(), 1, "one decoupling bank");
        let m = &ms[0];
        assert_eq!(m.anchor, "U1");
        let mut caps = m.bindings["cap"].clone();
        caps.sort();
        // C3/C4/C5 are the rail-to-rail caps; the crystal load caps (to a signal
        // net, not power) must NOT be swept in.
        assert_eq!(
            caps,
            vec!["C3".to_string(), "C4".to_string(), "C5".to_string()]
        );
    }

    #[test]
    fn decoupling_anchors_to_the_ic_not_a_power_connector() {
        // U1 (IC) + J1 (a power/SWD connector, ≥3 pins) both sit on +3V3 and GND with
        // the bypass caps. The bank must bind to the IC it decouples, NOT the connector
        // (which is a supply entry) — else the connector steals the anchor and placement
        // drops the whole bank, scattering it.
        let nodes = vec![
            node(
                "U1",
                "MCU:X",
                "",
                &[("1", "+3V3"), ("2", "GND"), ("3", "SIG")],
            ),
            node(
                "J1",
                "Connector_Generic:Conn_01x04",
                "",
                &[("1", "+3V3"), ("2", "GND"), ("3", "SWDIO"), ("4", "SWCLK")],
            ),
            node("C1", "Device:C", "100nF", &[("1", "+3V3"), ("2", "GND")]),
            node("C2", "Device:C", "100nF", &[("1", "+3V3"), ("2", "GND")]),
            node("C3", "Device:C", "100nF", &[("1", "+3V3"), ("2", "GND")]),
        ];
        let g = CircuitGraph::new(nodes, kind_of);
        let ms = find(&g, &library::DECOUPLING);
        assert_eq!(ms.len(), 1, "exactly one bank (not one per multi-pin part)");
        assert_eq!(ms[0].anchor, "U1", "anchored to the IC, not the connector");
    }

    #[test]
    fn find_all_resolves_contention_no_double_claim() {
        let g = stm32_graph();
        let ms = find_all(&g, &library::active_library());
        // crystal + decoupling, disjoint member sets.
        let crystal = ms.iter().find(|m| m.pattern == "crystal").unwrap();
        let deco = ms.iter().find(|m| m.pattern == "decoupling").unwrap();
        let cm: std::collections::BTreeSet<_> = crystal.members("anchor").into_iter().collect();
        let dm: std::collections::BTreeSet<_> = deco.members("anchor").into_iter().collect();
        assert!(cm.is_disjoint(&dm), "no component claimed by two idioms");
    }

    #[test]
    fn no_idiom_on_plain_divider() {
        // R-R-C divider/filter: no crystal, fewer than 3 rail caps.
        let nodes = vec![
            node("R1", "Device:R", "10k", &[("1", "VIN"), ("2", "MID")]),
            node("R2", "Device:R", "10k", &[("1", "MID"), ("2", "GND")]),
            node("C1", "Device:C", "100nF", &[("1", "MID"), ("2", "GND")]),
        ];
        let g = CircuitGraph::new(nodes, kind_of);
        assert!(find(&g, &library::CRYSTAL).is_empty());
        assert!(find(&g, &library::DECOUPLING).is_empty());
    }

    #[test]
    fn single_cap_crystal_rejected_at_full_min_score() {
        // Drop C2: only one load cap. With min_score 1.0 (both caps required) the
        // crystal pattern must not match.
        let nodes = vec![
            node(
                "U1",
                "MCU:X",
                "",
                &[("1", "+3V3"), ("2", "GND"), ("3", "XTAL1"), ("4", "XTAL2")],
            ),
            node(
                "Y1",
                "Device:Crystal",
                "8MHz",
                &[("1", "XTAL1"), ("2", "XTAL2")],
            ),
            node("C1", "Device:C", "22pF", &[("1", "XTAL1"), ("2", "GND")]),
        ];
        let g = CircuitGraph::new(nodes, kind_of);
        assert!(
            find(&g, &library::CRYSTAL).is_empty(),
            "one-cap crystal fails full match"
        );
    }

    #[test]
    fn extended_library_patterns_match_their_shapes() {
        // RC low-pass: R in series to a node shunted by C to GND.
        let rc = vec![
            node("R1", "Device:R", "1k", &[("1", "IN"), ("2", "OUT")]),
            node("C1", "Device:C", "100nF", &[("1", "OUT"), ("2", "GND")]),
        ];
        let g = CircuitGraph::new(rc, kind_of);
        assert_eq!(
            find(&g, &library::RC_LOWPASS).len(),
            1,
            "rc_lowpass matches"
        );

        // LED indicator: LED in series with a resistor.
        let led = vec![
            node("D1", "Device:LED", "", &[("1", "NODE"), ("2", "GND")]),
            node("R1", "Device:R", "330", &[("1", "+3V3"), ("2", "NODE")]),
        ];
        let g = CircuitGraph::new(led, kind_of);
        assert_eq!(
            find(&g, &library::LED_INDICATOR).len(),
            1,
            "led_indicator matches"
        );
    }

    #[test]
    fn led_indicator_pairs_the_series_resistor_not_a_supply_sharing_one() {
        // A GPIO LED: anode on +3V3, cathode → series R2 → GND. A reset pull-up R1
        // (3V3 → NRST) merely SHARES the +3V3 rail with the LED. The idiom must bind
        // the resistor on the LED's junction (signal) net, not the rail-sharing one.
        let nodes = vec![
            node("D1", "Device:LED", "", &[("1", "D1K"), ("2", "+3V3")]),
            node("R1", "Device:R", "10k", &[("1", "+3V3"), ("2", "NRST")]),
            node("R2", "Device:R", "1k", &[("1", "D1K"), ("2", "GND")]),
        ];
        let g = CircuitGraph::new(nodes, kind_of);
        let ms = find(&g, &library::LED_INDICATOR);
        assert_eq!(ms.len(), 1, "one LED indicator");
        assert_eq!(
            ms[0].bindings["res"],
            vec!["R2".to_string()],
            "pairs the SERIES resistor R2"
        );
    }

    #[test]
    fn led_indicator_does_not_match_a_clamp_diode() {
        let nodes = vec![
            node("D1", "Device:D", "", &[("1", "SIGNAL"), ("2", "GND")]),
            node("R1", "Device:R", "10k", &[("1", "+3V3"), ("2", "SIGNAL")]),
        ];
        let graph = CircuitGraph::new(nodes, kind_of);

        assert!(find(&graph, &library::LED_INDICATOR).is_empty());
    }

    #[test]
    fn led_indicator_does_not_match_an_led_driver_ic() {
        let nodes = vec![
            node(
                "U1",
                "Driver_LED:Example",
                "",
                &[("1", "DRIVE"), ("2", "GND"), ("3", "ENABLE")],
            ),
            node("R1", "Device:R", "1k", &[("1", "+12V"), ("2", "DRIVE")]),
        ];
        let graph = CircuitGraph::new(nodes, kind_of);

        assert!(find(&graph, &library::LED_INDICATOR).is_empty());
    }

    #[test]
    fn i2c_pullup_matches_small_sensor_not_large_mcu() {
        // A small I2C sensor (8 pins) with SDA/SCL pull-ups to +3V3 → one match.
        let sensor = vec![
            node(
                "U1",
                "Sensor:BME280",
                "",
                &[
                    ("SCK", "SCL"),
                    ("SDI", "SDA"),
                    ("VDD", "+3V3"),
                    ("GND", "GND"),
                    ("SDO", "AD"),
                    ("CSB", "+3V3"),
                    ("VDDIO", "+3V3"),
                    ("GND2", "GND"),
                ],
            ),
            node("R1", "Device:R", "4.7k", &[("1", "+3V3"), ("2", "SDA")]),
            node("R2", "Device:R", "4.7k", &[("1", "+3V3"), ("2", "SCL")]),
        ];
        let g = CircuitGraph::new(sensor, kind_of);
        let ms = find(&g, &library::I2C_PULLUP);
        assert_eq!(ms.len(), 1, "I2C pull-ups match a small sensor");
        assert_eq!(ms[0].anchor, "U1");

        // The SAME pull-up shape on a 17-pin MCU must NOT match (PinsAtMost(16)) — EN/BOOT
        // control pull-ups are not an I2C bus pair and must not be frozen as a cluster.
        let mut pins: Vec<(&str, &str)> = vec![
            ("EN", "ENN"),
            ("IO0", "BOOT"),
            ("VDD", "+3V3"),
            ("GND", "GND"),
        ];
        for n in [
            "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m",
        ] {
            pins.push((n, n));
        }
        let mcu = vec![
            node("U1", "RF:ESP32", "", &pins),
            node("R1", "Device:R", "10k", &[("1", "+3V3"), ("2", "ENN")]),
            node("R2", "Device:R", "10k", &[("1", "+3V3"), ("2", "BOOT")]),
        ];
        let g2 = CircuitGraph::new(mcu, kind_of);
        assert!(
            find(&g2, &library::I2C_PULLUP).is_empty(),
            "control pull-ups on a large MCU don't match"
        );
    }
}
