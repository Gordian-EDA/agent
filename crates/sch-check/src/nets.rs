//! Net attributes derived from the parts on a net.

use crate::model::*;

/// Mark the nets a power symbol drives as `power` and the nets a global label
/// drives as `port`.
///
/// Both are model-level facts: dropping a `power:GND` symbol on a net is how any
/// front end — text, tool call, or live sheet — says "this is a rail", and a
/// `label:global` is how it says "this leaves the sheet". Checkers exempt both
/// from the one-pin-net rule, so this must run before [`crate::lint`].
pub fn derive_attrs(d: &mut Design) {
    let mut power: Vec<NetName> = Vec::new();
    let mut ports: Vec<NetName> = Vec::new();
    for block in d.blocks.values() {
        for comp in block.components.values() {
            let sink = if comp.part.starts_with("power:") {
                &mut power
            } else if comp.part == "label:global" {
                &mut ports
            } else {
                continue;
            };
            for target in comp.pins.values() {
                if let PinTarget::Net(net) = target {
                    sink.push(net.clone());
                }
            }
        }
    }
    for net in power {
        d.nets.entry(net).or_default().power = true;
    }
    for net in ports {
        d.nets.entry(net).or_default().port = true;
    }
    for attrs in d.nets.values_mut() {
        if attrs.class.as_deref() == Some("power") {
            attrs.power = true;
        }
    }
}
