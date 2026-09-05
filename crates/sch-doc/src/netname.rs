//! Readable names for nets a tool has to name.
//!
//! A schematic a human wrote calls the net at an MCU's `PB6` pin `PB6`, not
//! `N_U2_42`. Every place the engine has to invent a name — a net straddling a
//! re-arrange, a net lifted under KiCAD's own `Net-(U3-PD0)` derivation, a wire
//! the agent asked for without naming — mints through [`Namer`] so they all read
//! the same way.

use std::collections::{BTreeMap, BTreeSet};

/// A pin a nameless net could be named after.
#[derive(Debug, Clone, Copy)]
pub struct Anchor<'a> {
    pub refdes: &'a str,
    pub number: &'a str,
    /// The pin's name in the symbol definition — `PB6`, `NRST`, `~` for a passive.
    pub pin_name: &'a str,
    /// How many pins the owning symbol has. An MCU names a net better than the
    /// header it runs to, and a header better than a decoupling cap.
    pub symbol_pins: usize,
}

/// Whether a net name was derived by a tool rather than written by an author.
///
/// These are recomputed from the net's own pins, so writing one down as a label
/// freezes a name that stops being true the moment a pin moves.
pub fn is_derived(net: &str) -> bool {
    net.starts_with("Net-(")
        || net.starts_with("unconnected-(")
        || net
            .strip_prefix("N$")
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// The `(refdes, pin name)` KiCAD encoded into a `Net-(U1-NRST)` derivation.
pub fn derived_parts(net: &str) -> Option<(&str, &str)> {
    let inner = net.strip_prefix("Net-(")?.strip_suffix(')')?;
    let (refdes, pin) = inner.split_once('-')?;
    (!refdes.is_empty() && !pin.is_empty()).then_some((refdes, pin))
}

/// The refdes and pin id a `N_<REF>_<PIN>` tool mint spells out, when the name
/// has that shape: `N_C13_PAD2` names pin `PAD2` of `C13`.
///
/// A name of this shape reads as machine spill wherever it came from — our own
/// earlier minting, or a model imitating it — so it is renamed like a derivation.
/// `N_LED` and `N_CV` are left alone: their head is no designator, so a reader
/// wrote them.
pub fn mint_parts(net: &str) -> Option<(&str, &str)> {
    let (refdes, pin) = net.strip_prefix("N_")?.split_once('_')?;
    (is_refdes_shaped(refdes) && !pin.is_empty()).then_some((refdes, pin))
}

/// One to three letters then digits — `U1`, `C13`, `SW2`.
fn is_refdes_shaped(text: &str) -> bool {
    let digits = text.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let letters = text.len() - digits.len();
    (1..=3).contains(&letters)
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
}

/// The `(refdes, pin)` any machine-made name spells out, whoever made it.
pub fn machine_parts(net: &str) -> Option<(&str, &str)> {
    derived_parts(net).or_else(|| mint_parts(net))
}

/// Mints net names and remembers which pin each one stands for, so the same net
/// asked for twice gets the same name and two different nets never share one.
#[derive(Debug, Default)]
pub struct Namer {
    /// Name → the pins the net under that name is known to hold. An empty set is
    /// an opaque claim: the name is taken, by nothing this can recognise.
    held: BTreeMap<String, BTreeSet<(String, String)>>,
}

impl Namer {
    pub fn new() -> Namer {
        Namer::default()
    }

    /// Record that `name` is already in use, by the net holding `pins`.
    pub fn hold<I, S>(&mut self, name: &str, pins: I)
    where
        I: IntoIterator<Item = (S, S)>,
        S: Into<String>,
    {
        let entry = self.held.entry(name.to_string()).or_default();
        entry.extend(pins.into_iter().map(|(r, n)| (r.into(), n.into())));
    }

    /// Note every name on the sheet that nothing may be minted over.
    pub fn hold_all<S: AsRef<str>>(&mut self, names: impl IntoIterator<Item = S>) {
        for name in names {
            self.held.entry(name.as_ref().to_string()).or_default();
        }
    }

    /// A readable name for the net touching `anchors`.
    ///
    /// The most specific pin names it: its bare pin name when that reads
    /// unambiguously on this sheet, else `REF_PINNAME`, else the `N_REF_PAD`
    /// form — which carries no meaning but is always available.
    ///
    /// A name the sheet ALREADY uses for a net holding one of these pins wins over
    /// all of them: that is this same net, met again, and re-minting it would fork
    /// the drawing rather than name it.
    pub fn mint(&mut self, anchors: &[Anchor<'_>]) -> String {
        let mut ranked: Vec<&Anchor<'_>> = anchors.iter().collect();
        ranked.sort_by_key(|a| {
            (
                std::cmp::Reverse(readable_pin_name(a.pin_name, a.number).is_some()),
                std::cmp::Reverse(a.symbol_pins),
                a.refdes,
                a.number,
            )
        });
        let candidates: Vec<(String, (String, String))> = ranked
            .iter()
            .flat_map(|anchor| {
                let key = (anchor.refdes.to_string(), anchor.number.to_string());
                names_for(anchor).into_iter().map(move |name| (name, key.clone()))
            })
            .collect();
        let Some((fallback, fallback_key)) = candidates.last().cloned() else {
            return "N_UNNAMED".to_string();
        };
        let mine = candidates
            .iter()
            .find(|(name, key)| self.held.get(name).is_some_and(|pins| pins.contains(key)));
        let (name, key) = match mine {
            Some(found) => found.clone(),
            None => candidates
                .iter()
                .find(|(name, _)| !self.held.contains_key(name))
                .cloned()
                .unwrap_or_else(|| {
                    let name = (2..)
                        .map(|n| format!("{fallback}_{n}"))
                        .find(|name| !self.held.contains_key(name))
                        .expect("the suffix space is unbounded");
                    (name, fallback_key)
                }),
        };
        self.held.entry(name.clone()).or_default().insert(key);
        name
    }
}

/// The names one pin offers its net, best first.
fn names_for(anchor: &Anchor<'_>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(pin) = readable_pin_name(anchor.pin_name, anchor.number) {
        if !looks_like_rail(&pin) {
            out.push(pin.clone());
        }
        out.push(format!("{}_{}", identifier(anchor.refdes), pin));
    }
    out.push(format!(
        "N_{}_{}",
        identifier(anchor.refdes),
        identifier(anchor.number)
    ));
    out
}

/// A pin name a reader would recognise as the net's identity.
///
/// A passive's `~`, a connector's `Pin_3` and a transistor's single-letter `G`
/// name nothing; `PB6`, `NRST`, `OSC_IN` and `VOUT` do.
fn readable_pin_name(name: &str, number: &str) -> Option<String> {
    let name = name.trim();
    if name.len() < 2
        || name == number
        || !name.starts_with(|c: char| c.is_ascii_alphabetic())
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        || name.eq_ignore_ascii_case("nc")
        || is_placeholder(name)
    {
        return None;
    }
    Some(name.to_string())
}

/// `Pin_3`, `PIN12`, `PAD7` — a generic symbol's stand-in for a pin number.
fn is_placeholder(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    for stem in ["pin_", "pin", "pad_", "pad", "p"] {
        if let Some(rest) = lower.strip_prefix(stem)
            && !rest.is_empty()
            && rest.bytes().all(|b| b.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

/// Whether a bare pin name would read as a supply rail. Rails are drawn with
/// their own symbols, so minting one from a pin would fork the supply.
fn looks_like_rail(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const RAILS: [&str; 10] = [
        "GND", "VSS", "VDD", "VCC", "VEE", "AGND", "DGND", "VBAT", "VIN", "VBUS",
    ];
    RAILS.contains(&upper.as_str())
        || upper.starts_with("+")
        || (upper.starts_with('V') && upper[1..].bytes().all(|b| b.is_ascii_digit()))
}

fn identifier(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin<'a>(refdes: &'a str, number: &'a str, name: &'a str, pins: usize) -> Anchor<'a> {
        Anchor { refdes, number, pin_name: name, symbol_pins: pins }
    }

    #[test]
    fn an_mcu_pin_names_the_net_it_shares_with_a_header() {
        let mut namer = Namer::new();
        let name = namer.mint(&[
            pin("J1", "4", "Pin_4", 10),
            pin("U2", "42", "PB6", 48),
        ]);
        assert_eq!(name, "PB6");
    }

    #[test]
    fn a_repeated_pin_name_is_qualified_by_its_part() {
        let mut namer = Namer::new();
        assert_eq!(namer.mint(&[pin("U1", "1", "OUT", 8)]), "OUT");
        assert_eq!(namer.mint(&[pin("U2", "7", "OUT", 8)]), "U2_OUT");
    }

    #[test]
    fn the_same_net_asked_for_twice_keeps_its_name() {
        let mut namer = Namer::new();
        assert_eq!(namer.mint(&[pin("U1", "1", "NRST", 48)]), "NRST");
        assert_eq!(namer.mint(&[pin("U1", "1", "NRST", 48)]), "NRST");
    }

    #[test]
    fn passives_alone_fall_back_to_the_pad_form() {
        let mut namer = Namer::new();
        assert_eq!(namer.mint(&[pin("C13", "2", "~", 2), pin("R4", "1", "~", 2)]), "N_C13_2");
    }

    #[test]
    fn a_rail_pin_name_is_qualified_rather_than_forking_the_supply() {
        let mut namer = Namer::new();
        assert_eq!(namer.mint(&[pin("U1", "8", "VDD", 8)]), "U1_VDD");
    }

    #[test]
    fn a_net_the_sheet_already_named_after_one_of_its_pins_is_met_again() {
        let mut namer = Namer::new();
        namer.hold("N_P3_1", [("P3", "1"), ("R9", "1")]);
        // A later call mentions only C7, which joins that same net.
        assert_eq!(namer.mint(&[pin("C7", "1", "~", 2), pin("P3", "1", "~", 2)]), "N_P3_1");
    }

    #[test]
    fn a_name_the_sheet_already_uses_is_not_stolen() {
        let mut namer = Namer::new();
        namer.hold_all(["SCL"]);
        assert_eq!(namer.mint(&[pin("U1", "9", "SCL", 28)]), "U1_SCL");
    }

    #[test]
    fn derived_names_are_recognised_and_parsed() {
        assert!(is_derived("Net-(U1-NRST)"));
        assert!(is_derived("N$14"));
        assert!(!is_derived("N$PWR"));
        assert!(!is_derived("SCL"));
        assert_eq!(derived_parts("Net-(U1-NRST)"), Some(("U1", "NRST")));
    }

    #[test]
    fn mint_shaped_names_are_recognised_but_authored_ones_are_left() {
        assert_eq!(mint_parts("N_C13_PAD2"), Some(("C13", "PAD2")));
        assert_eq!(mint_parts("N_J5_RESET"), Some(("J5", "RESET")));
        assert_eq!(mint_parts("N_LED"), None);
        assert_eq!(mint_parts("N_Q_BASE"), None);
        assert_eq!(machine_parts("Net-(R84-Pad1)"), Some(("R84", "Pad1")));
    }
}
