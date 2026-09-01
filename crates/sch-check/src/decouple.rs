//! `decouple` sugar: "give this IC N caps of value V across its rails".
//!
//! Model-level, not syntax — the bulk-create tool
//! expand it the same way — so it lives with the model. The caps are ordinary
//! [`Component`]s tagged [`Origin::Synthesized`], which is what lets a round trip
//! re-collapse them onto their parent.

use indexmap::IndexMap;

use crate::model::*;
use crate::{Diagnostic, SymbolTable};

/// The single supply/return net pair a component's caps go across.
pub struct Rails {
    pub vdd: NetName,
    pub gnd: NetName,
}

/// Find the rails to decouple: exactly one `VDD*`/`VCC*` net and one
/// `VSS*`/`GND*` net across the component's pins. Pin-map keys may be numbers,
/// so keys are resolved to pin NAMES through the symbol table first (an unknown
/// symbol falls back to the raw key). Anything ambiguous is an error — the
/// author must write those caps explicitly.
pub fn rails(refdes: &str, comp: &Component, provider: &SymbolTable) -> Result<Rails, Diagnostic> {
    let meta = provider.symbol(&comp.part);
    let pin_name = |key: &str| -> String {
        let Some(meta) = &meta else {
            return key.to_string();
        };
        crate::pins::resolve(meta, key)
            .first()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| key.to_string())
    };
    let matching = |prefixes: &[&str]| -> Vec<NetName> {
        let mut nets: Vec<NetName> = comp
            .pins
            .iter()
            .chain(comp.units.values().flatten())
            .filter(|(k, _)| {
                let k = pin_name(k).to_ascii_uppercase();
                prefixes.iter().any(|p| k.starts_with(p))
            })
            .filter_map(|(_, t)| match t {
                PinTarget::Net(n) => Some(n.clone()),
                PinTarget::NoConnect => None,
            })
            .collect();
        nets.sort();
        nets.dedup();
        nets
    };
    let vdd = matching(&["VDD", "VCC"]);
    let gnd = matching(&["VSS", "GND"]);
    if vdd.len() != 1 || gnd.len() != 1 {
        return Err(Diagnostic::error(
            "decouple-ambiguous",
            format!(
                "{refdes}: decouple needs exactly one VDD*/VCC* net and one \
                 VSS*/GND* net (found {vdd:?} / {gnd:?}) — write the caps explicitly"
            ),
        ));
    }
    Ok(Rails {
        vdd: vdd[0].clone(),
        gnd: gnd[0].clone(),
    })
}

/// The caps `values` (value → count) asks for, keyed `__dec_<parent>_<n>`.
///
/// Values are expanded in sorted order so a canonical round trip assigns the
/// same `Origin::Synthesized { index }` however the author ordered them.
pub fn expand(
    refdes: &str,
    values: &IndexMap<String, u32>,
    rails: &Rails,
) -> Vec<(RefDes, Component)> {
    let mut entries: Vec<(&String, &u32)> = values.iter().collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    let mut out = Vec::new();
    let mut index = 0u32;
    for (value, count) in entries {
        for _ in 0..*count {
            index += 1;
            let mut cap = Component {
                part: "Device:C".into(),
                value: Some(value.clone()),
                // A synthesized decoupler is a physical part the board needs a
                // footprint for and the author never sees it to assign one.
                footprint: Some("Capacitor_SMD:C_0402_1005Metric".into()),
                origin: Origin::Synthesized {
                    parent: refdes.to_string(),
                    role: "decouple".into(),
                    index,
                },
                ..Default::default()
            };
            cap.pins
                .insert("1".into(), PinTarget::Net(rails.vdd.clone()));
            cap.pins
                .insert("2".into(), PinTarget::Net(rails.gnd.clone()));
            out.push((format!("__dec_{refdes}_{index}"), cap));
        }
    }
    out
}

/// Give every synthesized decoupling cap a real `C<n>` refdes, continuing after
/// the highest authored `C`.
///
/// The `__dec_*` key is what re-collapses the caps onto their parent, but it is
/// also what the writer stamps as the KiCAD refdes — so rename late, keeping
/// each cap's `Origin` (identity lives there, not in the name). Deterministic
/// (block then component order, one global counter), so the round trip stays a
/// fixpoint.
pub fn renumber(d: &mut Design) {
    let is_synth =
        |c: &Component| matches!(&c.origin, Origin::Synthesized { role, .. } if role == "decouple");
    let mut next = 1 + d
        .blocks
        .values()
        .flat_map(|b| b.components.keys())
        .filter_map(|k| k.strip_prefix('C').and_then(|n| n.parse::<u32>().ok()))
        .max()
        .unwrap_or(0);
    for block in d.blocks.values_mut() {
        let synth: Vec<RefDes> = block
            .components
            .iter()
            .filter(|(_, c)| is_synth(c))
            .map(|(k, _)| k.clone())
            .collect();
        for old in synth {
            if let Some(comp) = block.components.shift_remove(&old) {
                block.components.insert(format!("C{next}"), comp);
                next += 1;
            }
        }
    }
}
