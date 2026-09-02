//! Naming things on a sheet: `"R1.1"` / `"U1.VDD"` for a pin, `[x, y]` for a
//! bare point, and the net a pin currently sits on.

use geom::Point2;
use sch_doc::{Netlist, PinRef, PlacedPin, SchDoc, placed_pins};
use serde_json::Value;

/// One end of a connection the model asked for.
pub(crate) enum Target {
    Pin(PlacedPin),
    Point(Point2),
}

impl Target {
    pub fn at(&self) -> Point2 {
        match self {
            Target::Pin(pin) => pin.at,
            Target::Point(p) => *p,
        }
    }

    pub fn owner(&self) -> Option<&str> {
        match self {
            Target::Pin(pin) => Some(&pin.refdes),
            Target::Point(_) => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Target::Pin(pin) => format!("{}.{}", pin.refdes, pin.number),
            Target::Point(p) => format!("({:.2},{:.2})", p.x, p.y),
        }
    }
}

/// Resolve `"R1.1"`, `"U1.VDD"` or `[x, y]` against the sheet.
pub(crate) fn target(doc: &SchDoc, value: &Value) -> Result<Target, String> {
    if let Some(spec) = value.as_str() {
        return pin(doc, spec).map(Target::Pin);
    }
    if let Some(pair) = value.as_array()
        && let (Some(x), Some(y)) = (
            pair.first().and_then(Value::as_f64),
            pair.get(1).and_then(Value::as_f64),
        )
    {
        return Ok(Target::Point(Point2::new(x, y)));
    }
    Err(format!(
        "expected a pin like \"U1.VDD\" or a point [x, y], got {value}"
    ))
}

/// Resolve a `"<ref>.<pin number or name>"` pin reference.
pub(crate) fn pin(doc: &SchDoc, spec: &str) -> Result<PlacedPin, String> {
    let (refdes, key) = spec
        .rsplit_once('.')
        .ok_or_else(|| format!("`{spec}` is not a pin reference; write it as \"U1.VDD\""))?;
    let pins = placed_pins(doc);
    let owned: Vec<&PlacedPin> = pins.iter().filter(|p| p.refdes == refdes).collect();
    if owned.is_empty() {
        return Err(format!(
            "no symbol `{refdes}` on the sheet, or its library definition is missing"
        ));
    }
    if let Some(found) = owned.iter().find(|p| p.number == key) {
        return Ok((*found).clone());
    }
    let named: Vec<&&PlacedPin> = owned
        .iter()
        .filter(|p| p.name.eq_ignore_ascii_case(key))
        .collect();
    match named.as_slice() {
        [one] => Ok((**one).clone()),
        [] => Err(format!(
            "`{refdes}` has no pin `{key}`; it has {}",
            summarize(&owned)
        )),
        many => Err(format!(
            "`{refdes}` has {} pins named `{key}` ({}); use the pin number",
            many.len(),
            many.iter()
                .map(|p| p.number.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// A short `1=A 2=K` listing of a symbol's pins for an error message.
fn summarize(pins: &[&PlacedPin]) -> String {
    pins.iter()
        .map(|p| match p.name.as_str() {
            "" | "~" => p.number.clone(),
            name => format!("{}={name}", p.number),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every placed unit of a part, in unit order, as `(unit, uuid)`.
///
/// A reference designator names a *part*, and the halves of a dual op-amp or a
/// dual triode are one part: `U1` is both. Tools address them through this so
/// a value or a library swap lands on all of them at once, the way KiCAD does.
pub(crate) fn units(doc: &SchDoc, refdes: &str) -> Vec<(u32, String)> {
    let mut units: Vec<(u32, String)> = doc
        .symbols()
        .filter(|s| s.refdes() == refdes)
        .map(|s| (s.unit, s.uuid.clone()))
        .collect();
    units.sort_by_key(|(unit, _)| *unit);
    units
}

/// The net a pin currently sits on, if it is on one.
pub(crate) fn net_of<'a>(netlist: &'a Netlist, refdes: &str, number: &str) -> Option<&'a str> {
    netlist
        .nets
        .iter()
        .find(|net| {
            net.pins
                .iter()
                .any(|p| p.refdes == refdes && p.pin == number)
        })
        .map(|net| net.name.as_str())
}

/// The pins an edit has just left dangling, `R4.1` style.
///
/// Breaking a net to insert a part in series loosens every *other* pin that
/// was on it, and a caller who reconnects only the two ends it named has
/// silently deleted a branch. Saying so is how the model finds out.
pub(crate) fn newly_loose(before: &Netlist, after: &Netlist) -> Vec<String> {
    after
        .unconnected
        .iter()
        .filter(|pin| {
            !before
                .unconnected
                .iter()
                .any(|was| was.refdes == pin.refdes && was.pin == pin.pin)
        })
        .map(label)
        .collect()
}

/// Every net a set of parts has a pin on.
pub(crate) fn nets_touching(netlist: &Netlist, refs: &[String]) -> Vec<String> {
    let mut out: Vec<String> = netlist
        .nets
        .iter()
        .filter(|net| net.pins.iter().any(|p| refs.contains(&p.refdes)))
        .map(|net| net.name.clone())
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// A wire's two ends. A `(wire)` with fewer than two points is malformed and
/// has none, so callers skip it rather than index into it.
pub(crate) fn ends(wire: &sch_doc::Wire) -> Option<(Point2, Point2)> {
    Some((*wire.points.first()?, *wire.points.last()?)).filter(|_| wire.points.len() >= 2)
}

/// `R1.1`, the way every tool result spells a pin.
pub(crate) fn label(pin: &PinRef) -> String {
    format!("{}.{}", pin.refdes, pin.pin)
}

/// Why a name KiCAD generated for an unnamed net cannot be reused as a label.
///
/// `read_schematic` shows those names — `Net-(U1B-G)` — and they read like an
/// identity anything may join. They are not: they are derived from the net's
/// own pins each time connectivity is extracted. Writing a label with that text
/// creates a *second* net, and KiCAD silently disambiguates the original to
/// `…_1`, severing a signal path that every guard still calls unchanged.
pub(crate) fn derived_name_refusal(netlist: &Netlist, net: &str) -> Option<String> {
    let auto = netlist
        .nets
        .iter()
        .find(|candidate| candidate.name == net && candidate.source == sch_doc::NetSource::Auto)?;
    let pins = auto
        .pins
        .iter()
        .take(4)
        .map(label)
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "refused: `{net}` is the name KiCAD generates for an unnamed net ({pins}), not a label \
         anything can join — naming a new node `{net}` forks it and renames the original to \
         `{net}_1`. Write \"@{first}\" to join that pin's net whatever it is called, or name the \
         net first with `label({{pin: \"{first}\", net: \"…\"}})` and use the name you gave it.",
        first = auto.pins.first().map(label).unwrap_or_default()
    ))
}

/// Prefix that turns a pin reference into a *net* reference: `"@P3.1"` means
/// "whatever net P3 pin 1 is on".
pub(crate) const NET_OF_PIN: char = '@';

/// A stable label for the net at `refdes.number`, minted when KiCAD's own name
/// for it is generated and therefore unusable as an identity.
fn minted_net_name(refdes: &str, number: &str) -> String {
    let sanitize = |s: &str| {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
    };
    format!("N_{}_{}", sanitize(refdes), sanitize(number))
}

/// What `"@R1.2"` resolves to: the net that pin already carries, or a name to give it.
pub(crate) enum PinNet {
    /// The pin sits on a net that already has a usable name.
    Named(String),
    /// The pin's net has no usable name; label the pin with this to mint one.
    Mint {
        refdes: String,
        number: String,
        net: String,
    },
}

impl PinNet {
    pub fn net(&self) -> &str {
        match self {
            PinNet::Named(net) => net,
            PinNet::Mint { net, .. } => net,
        }
    }
}

/// Resolve a `"@<ref>.<pin>"` net reference against the sheet.
///
/// A generated name like `Net-(P3-Pad1)` is not an identity a caller can join —
/// it is recomputed from the net's own pins — so there was no way to say "connect
/// to whatever P3 pin 1 is on". This is that way. When the net has no usable name
/// the caller labels the pin with [`PinNet::Mint`]'s name first, which makes the
/// identity real before anything joins it.
pub(crate) fn net_of_pin(
    doc: &SchDoc,
    netlist: &Netlist,
    spec: &str,
) -> Result<PinNet, String> {
    let spec = spec.strip_prefix(NET_OF_PIN).unwrap_or(spec);
    let pin = pin(doc, spec)?;
    let usable = netlist.nets.iter().find(|net| {
        net.source != sch_doc::NetSource::Auto
            && net
                .pins
                .iter()
                .any(|p| p.refdes == pin.refdes && p.pin == pin.number)
    });
    Ok(match usable {
        Some(net) => PinNet::Named(net.name.clone()),
        None => PinNet::Mint {
            net: minted_net_name(&pin.refdes, &pin.number),
            refdes: pin.refdes,
            number: pin.number,
        },
    })
}

/// Resolve `net` when it is written as `"@R1.2"`, else return it unchanged.
///
/// Unlike the `place_parts` path this never mints a name: a caller naming a net
/// here is already giving it an identity, so joining an unnamed one is a no-op
/// it should be told about rather than have guessed for it.
pub(crate) fn net_of_pin_name(
    doc: &SchDoc,
    netlist: &Netlist,
    net: &str,
) -> Result<String, String> {
    if !net.starts_with(NET_OF_PIN) {
        return Ok(net.to_string());
    }
    match net_of_pin(doc, netlist, net)? {
        PinNet::Named(found) => Ok(found),
        PinNet::Mint { refdes, number, .. } => Err(format!(
            "`{net}` names no existing net: {refdes}.{number} is not on a named net yet. \
             Label it first, or connect straight to the pin."
        )),
    }
}
