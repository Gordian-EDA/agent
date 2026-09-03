//! Net classification: every net on the sheet is a ground, a supply, or a signal.
//! Pure connectivity + naming; the one semantic input every later phase reads.

use std::collections::BTreeMap;

use circuit_graph::netclass::{is_ground, is_power_net};
use sch_model::ir::LayoutIr;
use sch_model::item::Incidence;

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

/// Reserved width of a net-name label: the realiser's stub lead plus the width
/// the renderer actually strokes the name at. One definition — nine call sites
/// that must reserve what the drawing uses, no more and no less.
pub(crate) fn label_text_width(net: &str) -> f64 {
    2.54 + sch_model::text::text_width(net)
}
