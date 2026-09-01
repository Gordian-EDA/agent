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
