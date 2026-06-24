//! Co-placement pair detection: which 2-pad parts should hug which anchor.
//!
//! Decoupling caps share BOTH nets with a ≥3-pad anchor (bypass a power rail);
//! series taps sit on a 2-pin net off a dense package (a breakout element). The
//! two sets are disjoint by construction, and feed both the fan-out fast-path and
//! the annealer's cohesion term.

use super::model::PlaceProblem;

/// Detect decoupling co-placement pairs `(cap_idx, ic_idx)`: a 2-pad part whose
/// BOTH pad nets also appear on a larger (≥3-pad) part is its decoupling cap and
/// should hug that IC/regulator. The smaller-index qualifying anchor wins
/// (deterministic). A part wired to two unrelated nets (e.g. a divider resistor)
/// finds no single anchor with both nets, so this fires only for real bypass caps.
/// Public so the agent surface can suggest a `surround` hint for a decoupling-heavy IC.
pub fn decoupling_pairs(problem: &PlaceProblem) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    for (si, small) in problem.parts.iter().enumerate() {
        if small.pads.len() != 2 {
            continue;
        }
        let nets: Vec<&str> = small.pads.iter().filter_map(|p| p.net.as_deref()).collect();
        if nets.len() != 2 || nets[0] == nets[1] {
            continue;
        }
        for (ai, anc) in problem.parts.iter().enumerate() {
            if ai == si || anc.pads.len() < 3 {
                continue;
            }
            let anc_nets: std::collections::BTreeSet<&str> =
                anc.pads.iter().filter_map(|p| p.net.as_deref()).collect();
            if anc_nets.contains(nets[0]) && anc_nets.contains(nets[1]) {
                pairs.push((si, ai));
                break;
            }
        }
    }
    pairs
}

/// Series co-placement earns its keep only where escape congestion is real: a DENSE
/// package (QFP/BGA/QFN — many pads on tight pitch) whose signal pads must thread
/// limited channels to break out. A small anchor (SOIC-8, SOT-223) has trivial escape,
/// so pulling a series part to it just perturbs an already-clean layout — it cost
/// power-stage a net in the full-harness sweep. Gate the anchor on pad count.
const SERIES_ANCHOR_MIN_PADS: usize = 16;

/// Detect series co-placement pairs `(part_idx, anchor_idx)`: a 2-pad part with a
/// pad on a **2-pin net** whose other pin belongs to a dense (≥[`SERIES_ANCHOR_MIN_PADS`]
/// -pad) anchor — a series element hanging directly off one anchor pin (the classic
/// BGA/IC signal
/// breakout: ball → series R → header). Co-placing it next to that anchor pad keeps
/// the congested escape hop short, so the breakout actually routes. The 2-pin-net
/// test is what makes this safe: a divider resistor's nets are high-fanout power
/// rails (≥3 pins), so it never matches — this fires only for true series taps.
/// When both pads qualify (R between two ICs), the LARGER anchor wins (the dense
/// package whose escape congestion matters most). Disjoint from [`decoupling_pairs`]
/// (whose caps share BOTH nets with one anchor, i.e. high-fanout power).
pub fn series_pairs(problem: &PlaceProblem) -> Vec<(usize, usize)> {
    // net name → the part indices with a pad on it (one entry per pad).
    let mut net_pins: std::collections::HashMap<&str, Vec<usize>> =
        std::collections::HashMap::new();
    for (pi, part) in problem.parts.iter().enumerate() {
        for pad in &part.pads {
            if let Some(n) = pad.net.as_deref() {
                net_pins.entry(n).or_default().push(pi);
            }
        }
    }
    let mut pairs = Vec::new();
    for (si, small) in problem.parts.iter().enumerate() {
        if small.pads.len() != 2 {
            continue;
        }
        let mut best: Option<usize> = None;
        let mut best_pads = 0usize;
        for pad in &small.pads {
            let Some(net) = pad.net.as_deref() else { continue };
            let pins = &net_pins[net];
            // 2-pin net: exactly this part's pad + one other pin.
            if pins.len() != 2 {
                continue;
            }
            if let Some(&anchor) = pins.iter().find(|&&p| p != si) {
                let np = problem.parts[anchor].pads.len();
                if np >= SERIES_ANCHOR_MIN_PADS && np > best_pads {
                    best_pads = np;
                    best = Some(anchor);
                }
            }
        }
        if let Some(anchor) = best {
            pairs.push((si, anchor));
        }
    }
    pairs
}

/// Order series parts by the ANGLE of their connected `ic` pad around the IC
/// centre. Ringing them in this order makes each IC→part escape route radially
/// (short, parallel, NON-crossing) instead of spaghetti — the key to neat fan-out
/// on a board whose signal pins each tap a series element (the routing-neatness
/// lever). `parts` are part indices (e.g. from [`series_pairs`] anchored at `ic`).
pub fn series_fanout_order(problem: &PlaceProblem, ic: usize, parts: &[usize]) -> Vec<String> {
    let mut with_angle: Vec<(f64, String)> = parts
        .iter()
        .filter_map(|&p| {
            let p_nets: std::collections::BTreeSet<&str> =
                problem.parts[p].pads.iter().filter_map(|pp| pp.net.as_deref()).collect();
            // The IC pad sharing this part's 2-pin net → its angle around the IC.
            problem.parts[ic].pads.iter().find_map(|pad| {
                let n = pad.net.as_deref()?;
                p_nets.contains(n).then(|| {
                    (pad.offset.y.atan2(pad.offset.x), problem.parts[p].reference.clone())
                })
            })
        })
        .collect();
    with_angle.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    with_angle.into_iter().map(|(_, r)| r).collect()
}

/// Co-placement pairs the SA cohesion honours: decoupling caps (hug their IC) plus
/// series taps (hug their dense anchor). A part can appear once — [`decoupling_pairs`]
/// and [`series_pairs`] are disjoint by construction (both-nets-shared vs 2-pin-net).
pub(crate) fn coplacement_pairs(problem: &PlaceProblem) -> Vec<(usize, usize)> {
    let mut pairs = decoupling_pairs(problem);
    pairs.extend(series_pairs(problem));
    pairs
}
