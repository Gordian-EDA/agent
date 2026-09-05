//! Nets whose NAMES say they are one signal while the drawing keeps them apart.
//!
//! `SWCLK` and `SWCLK_TCK` are the same wire under two spellings — the MCU's pin
//! name and the debug header's. When both exist as separate nets and nothing
//! bridges them, the sheet is missing the connection between them, and the names
//! are the only place that shows. Readable net names made this visible: while a
//! net was called `N_J5_SWCLK_TCK` there was no claim to contradict.

use std::collections::{BTreeMap, BTreeSet};

use circuit_graph::netclass::is_power_net;

use crate::model::{Design, NetName, PinTarget};

/// A part with at least this many pins is an IC or a header — something whose
/// pin a `connect` fix can name as one end of the missing wire. Passives and
/// discretes below it say nothing about which end of the signal they are.
const ANCHOR_PART_PINS: usize = 4;

/// A part wider than this is not a series element. Resistors, ESD diodes, and
/// small buffers pass a signal through; an MCU is where signals END.
const SERIES_PART_PINS: usize = 8;

/// How many series parts one signal may be drawn through before its two ends
/// stop being the same signal.
const SERIES_PARTS: usize = 4;

/// Two nets one signal was drawn as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearTwin {
    pub a: NetName,
    pub b: NetName,
    /// The `REF.PIN` pair a `connect` would join, when each net has exactly one
    /// pin on a part big enough to identify the signal's end.
    pub join: Option<(String, String)>,
}

/// Every pair of nets whose names claim one signal that the drawing splits.
///
/// A pair is reported only when nothing on the sheet already carries the signal
/// between them: `USB_DP` reaches `USB_DP_MCU` through an ESD diode and then a
/// series resistor, and a path like that is what makes the two names legitimate.
/// Rails are left out entirely — supplies are spelt a dozen ways on purpose.
pub fn near_twins(d: &Design) -> Vec<NearTwin> {
    let index = NetPins::of(d);
    let names: Vec<&str> = index
        .pins
        .keys()
        .map(String::as_str)
        .filter(|net| speaks_for_a_signal(net, d))
        .collect();

    let mut out = Vec::new();
    let mut paired: BTreeSet<&str> = BTreeSet::new();
    for (i, a) in names.iter().enumerate() {
        if paired.contains(a) {
            continue;
        }
        // One net, one report: the best partner by did-you-mean distance, per
        // CLAUDE.md's rule that `strsim` ranks a single best suggestion only.
        let best = names[i + 1..]
            .iter()
            .filter(|b| !paired.contains(*b) && says_same_signal(a, b))
            .filter(|b| !index.carries_between(a, b))
            .min_by_key(|b| strsim::levenshtein(a, b));
        let Some(b) = best else { continue };
        paired.insert(a);
        paired.insert(b);
        out.push(NearTwin {
            a: (*a).to_string(),
            b: (*b).to_string(),
            join: index.anchor(a).zip(index.anchor(b)),
        });
    }
    out
}

/// Whether a net's name is a claim about a signal at all.
///
/// A machine-made name (`N_J5_SWCLK_TCK`, `Net-(U1-NRST)`, `D2_K`) names a pin,
/// not a signal, so two of them reading alike claims nothing. A rail is spelt
/// many ways by design.
fn speaks_for_a_signal(net: &str, d: &Design) -> bool {
    sch_doc::netname::machine_parts(net).is_none() && !is_rail(net, d)
}

/// Whether two names spell one signal.
fn says_same_signal(a: &str, b: &str) -> bool {
    qualifies(a, b) || qualifies(b, a) || abbreviates(a, b) || abbreviates(b, a)
}

/// `SWCLK` → `SWCLK_TCK`: the same name carrying an alternate-function
/// qualifier. A purely numeric tail is excluded — `USB_D` and `USB_D_2` are a
/// name and its de-duplication counter, which is how two DIFFERENT nets end up
/// looking alike, and `VCAP_1`/`VCAP_2` are two real pins of one part.
fn qualifies(stem: &str, long: &str) -> bool {
    stem.len() >= 3
        && long
            .strip_prefix(stem)
            .and_then(|tail| tail.strip_prefix('_'))
            .is_some_and(|tail| !tail.is_empty() && !tail.bytes().all(|b| b.is_ascii_digit()))
}

/// `NRST` → `RESET`: the same word, one side written without its vowels.
///
/// Hardware abbreviates by dropping vowels (`RESET`/`RST`, `CLOCK`/`CLK`), so
/// the short form is the long one's consonants in order and nothing else. Both
/// sides lose an active-low marker first, which is why `NRST` reaches `RST`.
/// Requiring the dropped letters to be vowels is what keeps `MISO`/`MOSI` and
/// `SCK`/`SCLK` apart — neither is the other with vowels removed.
fn abbreviates(short: &str, long: &str) -> bool {
    let (short, long) = (signal_word(short), signal_word(long));
    short.len() >= 3 && short.len() < long.len() && vowelless(&long) == short
}

/// A name reduced to the word it spells: upper case, active-low markers and
/// separators gone.
fn signal_word(net: &str) -> String {
    let upper: String = net
        .to_ascii_uppercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    let stripped = upper
        .strip_suffix('N')
        .or_else(|| upper.strip_prefix('N'))
        .filter(|rest| rest.len() >= 3);
    stripped.map_or(upper.clone(), str::to_string)
}

fn vowelless(word: &str) -> String {
    word.chars().filter(|c| !"AEIOU".contains(*c)).collect()
}

/// The pin/part incidence of one design, indexed both ways.
struct NetPins {
    /// Net → the `(refdes, pin, part width)` it holds.
    pins: BTreeMap<String, Vec<(String, String, usize)>>,
    /// Net → the parts on it, rails excluded: every part touches a supply, and
    /// a path through one is not a signal path.
    parts: BTreeMap<String, BTreeSet<String>>,
    /// Part → the non-rail nets it sits on.
    nets: BTreeMap<String, BTreeSet<String>>,
    width: BTreeMap<String, usize>,
}

impl NetPins {
    fn of(d: &Design) -> NetPins {
        let mut out = NetPins {
            pins: BTreeMap::new(),
            parts: BTreeMap::new(),
            nets: BTreeMap::new(),
            width: BTreeMap::new(),
        };
        for block in d.blocks.values() {
            for (refdes, comp) in &block.components {
                let all = comp.pins.iter().chain(comp.units.values().flatten());
                let width = comp.pins.len() + comp.units.values().map(|u| u.len()).sum::<usize>();
                out.width.insert(refdes.clone(), width);
                for (key, target) in all {
                    let PinTarget::Net(net) = target else {
                        continue;
                    };
                    out.pins.entry(net.clone()).or_default().push((
                        refdes.clone(),
                        key.clone(),
                        width,
                    ));
                    if is_rail(net, d) {
                        continue;
                    }
                    out.parts
                        .entry(net.clone())
                        .or_default()
                        .insert(refdes.clone());
                    out.nets
                        .entry(refdes.clone())
                        .or_default()
                        .insert(net.clone());
                }
            }
        }
        out
    }

    /// Whether the sheet already carries the signal from one net to the other.
    ///
    /// One part sitting on both settles it whatever that part is. Beyond that
    /// the path has to read as a SERIES chain — `USB_DP` through an ESD diode,
    /// then through a series resistor, to `USB_DP_MCU` — so every part it steps
    /// through must be narrow enough to be one. Stepping through the MCU would
    /// join every signal on the sheet to every other and the question would stop
    /// meaning anything; [`SERIES_PART_PINS`] is what rules that out.
    fn carries_between(&self, a: &str, b: &str) -> bool {
        let (Some(from), Some(to)) = (self.parts.get(a), self.parts.get(b)) else {
            return false;
        };
        if from.intersection(to).next().is_some() {
            return true;
        }
        let mut seen: BTreeSet<&str> = [a].into_iter().collect();
        let mut front: Vec<&str> = vec![a];
        for _ in 0..SERIES_PARTS {
            let mut next = Vec::new();
            for net in front {
                let series =
                    self.parts.get(net).into_iter().flatten().filter(|part| {
                        self.width.get(*part).copied().unwrap_or(0) <= SERIES_PART_PINS
                    });
                for reached in series.flat_map(|part| self.nets.get(part).into_iter().flatten()) {
                    if reached == b {
                        return true;
                    }
                    if seen.insert(reached) {
                        next.push(reached.as_str());
                    }
                }
            }
            front = next;
        }
        false
    }

    /// The net's one pin on a part big enough to name the signal's end, if it
    /// has exactly one.
    fn anchor(&self, net: &str) -> Option<String> {
        let mut wide = self
            .pins
            .get(net)?
            .iter()
            .filter(|(_, _, width)| *width >= ANCHOR_PART_PINS);
        let (refdes, pin, _) = wide.next()?;
        wide.next().is_none().then(|| format!("{refdes}.{pin}"))
    }
}

fn is_rail(net: &str, d: &Design) -> bool {
    is_power_net(net) || d.nets.get(net).is_some_and(|attrs| attrs.power)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Block, Component};
    use indexmap::IndexMap;

    fn design(parts: &[(&str, &[(&str, &str)])]) -> Design {
        let mut components: IndexMap<String, Component> = IndexMap::new();
        for (refdes, pins) in parts {
            components.insert(
                (*refdes).to_string(),
                Component {
                    part: "Device:R".to_string(),
                    pins: pins
                        .iter()
                        .map(|(k, net)| ((*k).to_string(), PinTarget::Net((*net).to_string())))
                        .collect(),
                    ..Component::default()
                },
            );
        }
        let mut blocks = IndexMap::new();
        blocks.insert(
            "main".to_string(),
            Block {
                components,
                ..Block::default()
            },
        );
        Design {
            blocks,
            ..Design::default()
        }
    }

    fn pairs(d: &Design) -> Vec<(String, String)> {
        near_twins(d)
            .into_iter()
            .map(|twin| (twin.a, twin.b))
            .collect()
    }

    /// A ten-pin debug header whose pins nothing joins to the MCU that names the
    /// same four signals.
    fn unjoined_debug_header() -> Design {
        design(&[
            (
                "U1",
                &[
                    ("7", "NRST"),
                    ("49", "SWCLK"),
                    ("46", "SWDIO"),
                    ("55", "SWO"),
                    ("1", "VDD"),
                ],
            ),
            (
                "J5",
                &[
                    ("2", "RESET"),
                    ("4", "SWCLK_TCK"),
                    ("6", "SWDIO_TMS"),
                    ("8", "SWO_TDO"),
                    ("10", "GND"),
                ],
            ),
        ])
    }

    #[test]
    fn a_qualified_pin_name_and_its_stem_are_one_signal() {
        let found = pairs(&unjoined_debug_header());
        assert!(
            found.contains(&("SWCLK".into(), "SWCLK_TCK".into())),
            "{found:?}"
        );
        assert!(
            found.contains(&("SWDIO".into(), "SWDIO_TMS".into())),
            "{found:?}"
        );
        assert!(
            found.contains(&("SWO".into(), "SWO_TDO".into())),
            "{found:?}"
        );
    }

    #[test]
    fn a_vowelless_abbreviation_and_its_word_are_one_signal() {
        let found = pairs(&unjoined_debug_header());
        assert!(
            found.contains(&("NRST".into(), "RESET".into())),
            "{found:?}"
        );
    }

    #[test]
    fn the_fix_names_one_pin_on_each_side() {
        let twins = near_twins(&unjoined_debug_header());
        let reset = twins.iter().find(|t| t.a == "NRST").unwrap();
        assert_eq!(reset.join, Some(("U1.7".into(), "J5.2".into())));
    }

    #[test]
    fn a_series_part_between_them_is_why_they_are_two_nets() {
        let d = design(&[
            (
                "U1",
                &[("1", "USB_DP_MCU"), ("2", "A"), ("3", "B"), ("4", "C")],
            ),
            ("R1", &[("1", "USB_DP_MCU"), ("2", "USB_DP")]),
            ("J1", &[("1", "USB_DP"), ("2", "D"), ("3", "E"), ("4", "F")]),
        ]);
        assert_eq!(pairs(&d), Vec::new());
    }

    #[test]
    fn a_two_hop_series_path_is_still_one_signal() {
        let d = design(&[
            ("J1", &[("1", "USB_DP"), ("2", "A"), ("3", "B"), ("4", "C")]),
            (
                "U3",
                &[
                    ("1", "USB_DP"),
                    ("2", "USB_DP_PROT"),
                    ("3", "D"),
                    ("4", "E"),
                ],
            ),
            ("R7", &[("1", "USB_DP_PROT"), ("2", "USB_DP_MCU")]),
            (
                "U1",
                &[("44", "USB_DP_MCU"), ("1", "F"), ("2", "G"), ("3", "H")],
            ),
        ]);
        assert_eq!(pairs(&d), Vec::new());
    }

    #[test]
    fn a_family_broken_in_the_middle_is_reported_once_per_gap() {
        let d = design(&[
            ("J1", &[("1", "USB_DP"), ("2", "A"), ("3", "B"), ("4", "C")]),
            (
                "U3",
                &[
                    ("1", "USB_DP"),
                    ("2", "USB_DP_PROT"),
                    ("3", "D"),
                    ("4", "E"),
                ],
            ),
            (
                "U1",
                &[("44", "USB_DP_MCU"), ("1", "F"), ("2", "G"), ("3", "H")],
            ),
        ]);
        assert_eq!(
            pairs(&d),
            vec![("USB_DP".to_string(), "USB_DP_MCU".to_string())]
        );
    }

    #[test]
    fn a_de_duplication_counter_is_not_a_qualifier() {
        let d = design(&[(
            "J1",
            &[
                ("1", "USB_D"),
                ("2", "USB_D_2"),
                ("3", "VCAP_1"),
                ("4", "VCAP_2"),
            ],
        )]);
        assert_eq!(pairs(&d), Vec::new());
    }

    #[test]
    fn nets_one_character_apart_are_not_twins() {
        let d = design(&[(
            "U1",
            &[
                ("1", "USART3_RX"),
                ("2", "USART3_TX"),
                ("3", "BUCK_IN"),
                ("4", "BUCK_EN"),
                ("5", "MISO"),
                ("6", "MOSI"),
                ("7", "HSE_IN"),
                ("8", "HSE_OUT"),
            ],
        )]);
        assert_eq!(pairs(&d), Vec::new());
    }

    #[test]
    fn machine_names_and_rails_claim_nothing() {
        let d = design(&[
            (
                "U1",
                &[
                    ("1", "N_J5_RESET"),
                    ("2", "N_J5_RESET_2"),
                    ("3", "X"),
                    ("4", "Y"),
                ],
            ),
            (
                "J5",
                &[("1", "GND"), ("2", "GND_ANALOG"), ("3", "Z"), ("4", "W")],
            ),
        ]);
        assert_eq!(pairs(&d), Vec::new());
    }
}
