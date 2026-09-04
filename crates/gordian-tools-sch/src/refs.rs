//! Naming things on a sheet: `"R1.1"` / `"U1.VDD"` for a pin, `[x, y]` for a
//! bare point, and the net a pin currently sits on.

use geom::Point2;
use kicad_symbol::SymbolTable;
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
///
/// A leading [`NET_OF_PIN`] is accepted and ignored. Tool results spell a pin's net
/// as `@R1.2`, so the model reads that form back and writes it wherever a pin goes;
/// as an ENDPOINT the net and the pin are the same place, and refusing it only cost
/// a request.
pub(crate) fn pin(doc: &SchDoc, spec: &str) -> Result<PlacedPin, String> {
    let spec = spec.strip_prefix(NET_OF_PIN).unwrap_or(spec);
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

/// Ranked complete pin addresses for an unresolved caller-facing pin key.
pub(crate) fn pin_suggestions(
    doc: &SchDoc,
    spec: &str,
    symbol_dir: std::path::PathBuf,
) -> Vec<String> {
    let spec = spec.strip_prefix(NET_OF_PIN).unwrap_or(spec);
    let Some((refdes, key)) = spec.rsplit_once('.') else {
        return Vec::new();
    };
    let Some(symbol) = doc.symbol_by_ref(refdes) else {
        return Vec::new();
    };
    let table = SymbolTable::from_symbol_dir(symbol_dir);
    let Some(meta) = table.symbol(&symbol.lib_id) else {
        return Vec::new();
    };
    sch_check::pins::ranked_suggestions(&meta, key, 8)
        .into_iter()
        .map(|pin| format!("{refdes}.{pin}"))
        .collect()
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

/// Whether a name is present either in extracted connectivity or on a power symbol.
pub(crate) fn net_exists(doc: &SchDoc, netlist: &Netlist, name: &str) -> bool {
    netlist.nets.iter().any(|net| net.name == name)
        || doc.symbols().any(|symbol| {
            symbol.lib_id.starts_with("power:")
                && symbol.lib_id != "power:PWR_FLAG"
                && match symbol.value() {
                    "" => symbol.lib_id.strip_prefix("power:") == Some(name),
                    value => value == name,
                }
        })
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

/// The stable pin address encoded by a KiCad-derived net name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DerivedNetRef {
    /// Physical-number address used internally, including the `@` prefix.
    pub spec: String,
    /// Human-readable name address reported back to the caller.
    pub reported: String,
}

/// Resolve `Net-(U1-BAT)`, `Net-(J1-Pad1)`, and numbered suffix variants.
pub(crate) fn derived_net_ref(doc: &SchDoc, name: &str) -> Result<Option<DerivedNetRef>, String> {
    let Some(close) = name.rfind(')') else {
        return Ok(None);
    };
    let base = &name[..=close];
    let suffix = &name[close + 1..];
    if !base.starts_with("Net-(")
        || !(suffix.is_empty()
            || suffix.strip_prefix('_').is_some_and(|digits| {
                !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
            }))
    {
        return Ok(None);
    }
    for pin in placed_pins(doc) {
        let unit = derived_unit_letter(&pin);
        let key = match pin.name.as_str() {
            "" | "~" => format!("Pad{}", pin.number),
            pin_name if pin_name == pin.number => format!("Pad{}", pin.number),
            pin_name => sch_doc::unescape(pin_name),
        };
        if base == format!("Net-({}{}-{key})", pin.refdes, unit) {
            let reported_key = key.strip_prefix("Pad").unwrap_or(&key);
            return Ok(Some(DerivedNetRef {
                spec: format!("@{}.{}", pin.refdes, pin.number),
                reported: format!("@{}.{reported_key}", pin.refdes),
            }));
        }
    }
    let encoded = base
        .strip_prefix("Net-(")
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or(base)
        .replacen('-', ".", 1);
    Err(format!(
        "derived net `{name}` names pin `{encoded}`, but that pin does not exist on the sheet"
    ))
}

fn derived_unit_letter(pin: &PlacedPin) -> String {
    if !pin.multi_unit {
        return String::new();
    }
    let mut index = pin.unit.max(1) - 1;
    let mut out = String::new();
    loop {
        out.insert(0, char::from(b'A' + (index % 26) as u8));
        if index < 26 {
            return out;
        }
        index = index / 26 - 1;
    }
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
pub(crate) fn net_of_pin(doc: &SchDoc, netlist: &Netlist, spec: &str) -> Result<PinNet, String> {
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
