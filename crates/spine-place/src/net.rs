//! Net classification: every net on the sheet is a ground, a supply, or a signal.
//! Pure connectivity + naming; the one semantic input every later phase reads.

use std::collections::BTreeMap;

use sch_place::ir::LayoutIr;
use sch_place::item::Incidence;
use sch_place::netclass::{is_ground, is_power_net};

/// The three net classes the grammar distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetClass {
    Ground,
    Supply,
    Signal,
}

impl NetClass {
    pub fn is_rail(self) -> bool {
        !matches!(self, NetClass::Signal)
    }
}

/// Classify every net in the incidence. Rails declared by the IR win; otherwise
/// name heuristics (`GND`/`VSS`… ground, `VCC`/`3V3`… supply).
pub fn classify_nets(inc: &Incidence, ir: &LayoutIr) -> BTreeMap<String, NetClass> {
    inc.keys()
        .map(|net| {
            let class = if is_ground(net) {
                NetClass::Ground
            } else if ir.rails.contains_key(net) || is_power_net(net) {
                NetClass::Supply
            } else {
                NetClass::Signal
            };
            (net.clone(), class)
        })
        .collect()
}

/// Reserved width of a net-name label: the realizer's stub lead plus its
/// per-character glyph advance. One definition — nine call sites once drifted
/// against the renderer in lockstep.
pub(crate) fn label_text_width(net: &str) -> f64 {
    2.54 + 1.4 * net.chars().count() as f64
}
