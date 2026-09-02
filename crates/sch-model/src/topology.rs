//! Which anchor a satellite belongs to, read off connectivity alone.

use std::collections::{BTreeMap, BTreeSet};

use crate::ir::Band;
use crate::item::{Incidence, Item};

/// The single anchor pin a satellite taps, as (anchor index, pin number, net), or
/// None if it touches zero or several anchor pins.
pub fn anchor_tap(
    items: &[Item],
    inc: &Incidence,
    anchors: &[usize],
    si: usize,
    rails: &BTreeMap<String, Band>,
) -> Option<(usize, String, String)> {
    // LOCALITY PRINCIPLE: a power RAIL (GND/V+) reaches nearly every anchor on the
    // board, so it carries no positional information — the part's home is decided by
    // its SIGNAL legs alone. (This is the same rule `order_anchors` uses when it
    // excludes ground/weak-weights power from its adjacency graph.) Splitting hits by
    // rail-ness lets a per-pin pull-down/pull-up/sense divider — whose other leg is a
    // shared rail — still flank the one IC pin its signal leg taps.
    let mut sig_hits = Vec::new();
    let mut rail_hits = Vec::new();
    for (_, _, net) in &items[si].pins {
        let Some(net) = net else { continue };
        for (j, num) in inc.get(net).into_iter().flatten() {
            if anchors.contains(j) {
                let hit = (*j, num.clone(), net.clone());
                if rails.contains_key(net) {
                    rail_hits.push(hit);
                } else {
                    sig_hits.push(hit);
                }
            }
        }
    }
    // A satellite whose SIGNAL pin taps exactly one anchor flanks THAT anchor, even
    // when its other leg is a rail shared with other anchors. Two distinct signal
    // anchors is a genuine inter-IC series element: ambiguous, place generically.
    let sig_anchors: BTreeSet<usize> = sig_hits.iter().map(|h| h.0).collect();
    if sig_anchors.len() == 1 {
        return Some(sig_hits.into_iter().next().unwrap());
    }
    if !sig_anchors.is_empty() {
        return None;
    }
    // No signal tap reaches an anchor. A part that nonetheless carries a signal net
    // (a coax / DC-block cap whose signal exits elsewhere) is NOT made adjacent by a
    // bare rail tap — let it place elsewhere. Only a PURE decoupler (every net a
    // rail) flanks the supply pin of the single anchor its rail legs land on.
    let has_signal = items[si]
        .pins
        .iter()
        .filter_map(|(_, _, n)| n.as_deref())
        .any(|n| !rails.contains_key(n));
    if has_signal {
        return None;
    }
    let rail_anchors: BTreeSet<usize> = rail_hits.iter().map(|h| h.0).collect();
    if rail_anchors.len() != 1 {
        return None;
    }
    rail_hits.into_iter().next()
}

