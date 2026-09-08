//! Routing rules for boards that carry no project file.
//!
//! A board with copper is its own evidence (thinnest track, most common via); an empty one gets
//! KiCad's built-in constraints widened by the room its pad pitch leaves. Ported from
//! `pcbagent/route/rules.py`, minus the pad-hugging power-island geometry the pipeline does not use.

use std::collections::BTreeMap;

use crate::geom::{dist, polygon_area, BBox};
use crate::model::{Board, Footprint, Pad, Rules};

/// What an empty board gets: KiCad's own built-in constraints, which any fabricator pools.
pub const EMPTY_TRACK_WIDTH: f64 = 0.2;
pub const EMPTY_CLEARANCE: f64 = 0.2;

/// Manufacturability limits, not KiCad defaults: DRC is checked against the inferred rule, so a
/// board routed to 0.13 mm can be reworked at 0.13 mm.
const TRACK_BOUNDS: (f64, f64) = (0.1, 0.5);
const CLEARANCE_BOUNDS: (f64, f64) = (0.1, 0.5);
const VIA_SIZE_BOUNDS: (f64, f64) = (0.4, 1.0);
const VIA_DRILL_BOUNDS: (f64, f64) = (0.2, 0.6);

const MIN_TRACKS_TO_MEASURE: usize = 5;

fn clamp(v: f64, b: (f64, f64)) -> f64 {
    (v.clamp(b.0, b.1) * 1000.0).round() / 1000.0
}

/// The rules to route with. Copper the board already carries only ever NARROWS the width rule:
/// the rule is a DRC minimum, and widening it would turn a later fine escape into an error.
pub fn infer_rules(board: &Board) -> Rules {
    let mut r = board.design_rules();
    let tracks = board.tracks();
    let vias = board.vias();
    if tracks.len() >= MIN_TRACKS_TO_MEASURE {
        let measured = tracks.iter().map(|t| t.width).fold(f64::INFINITY, f64::min);
        r.track_width = r.track_width.min(measured);
        r.clearance = r.clearance.min(measured);
    }
    if !vias.is_empty() {
        let mut counts: BTreeMap<(u64, u64), (usize, f64, f64)> = BTreeMap::new();
        for v in &vias {
            let key = ((v.size * 1000.0) as u64, (v.drill * 1000.0) as u64);
            let e = counts.entry(key).or_insert((0, v.size, v.drill));
            e.0 += 1;
        }
        if let Some((_, size, drill)) = counts.values().copied().max_by_key(|e| e.0) {
            r.via_size = size;
            r.via_drill = drill;
        }
    }
    r.track_width = clamp(r.track_width, TRACK_BOUNDS);
    r.clearance = clamp(r.clearance, CLEARANCE_BOUNDS);
    r.via_size = clamp(r.via_size, VIA_SIZE_BOUNDS);
    r.via_drill = clamp(r.via_drill, VIA_DRILL_BOUNDS);
    if r.via_drill > r.via_size - 0.2 {
        r.via_drill = clamp(r.via_size - 0.2, VIA_DRILL_BOUNDS);
    }
    r
}

// ---- net names ------------------------------------------------------------------
// Hand-rolled matchers standing in for rules.py's regexes; the crate takes no regex dependency.

/// A net without its sheet path: KiCad names a hierarchical net `/power/+3V3`.
pub fn bare_net_name(name: &str) -> &str {
    name.trim().rsplit('/').next().unwrap_or("").trim()
}

const SIGNAL_SUFFIXES: &[&str] = &[
    "EN", "PG", "PGOOD", "FB", "OK", "SNS", "SENSE", "FLAG", "RST", "INT", "ADJ", "SET", "MON",
    "DET", "CTRL", "TRIP", "REF", "DIV",
];

/// A rail name wearing a signal's job on the end — `VOUT_FB`, `3V3_EN` — names the pin that
/// watches the rail, not the rail. Those carry no current and want no copper.
fn has_signal_suffix(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    let Some(cut) = up.rfind(['_', '-']) else {
        return false;
    };
    if cut == 0 {
        return false;
    }
    let tail = &up[cut + 1..];
    let stem = tail.trim_end_matches(|c: char| c.is_ascii_digit());
    !stem.is_empty() && SIGNAL_SUFFIXES.contains(&stem)
}

const POWER_PREFIXES: &[&str] = &[
    "GND", "AGND", "DGND", "PGND", "VSS", "VCC", "VDD", "VBUS", "VIN", "VOUT", "VBAT", "VSYS",
    "VMOT", "VDC", "VPP", "PWR",
];
const POWER_EXACT: &[&str] = &["GROUND", "POWER", "RAIL"];

fn is_word(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `3V3`, `5V`, `12V0`, `3.3V`, `V33` — a rail spelled as a voltage.
fn is_voltage_name(up: &str) -> bool {
    let b = up.as_bytes();
    if b.is_empty() {
        return false;
    }
    let mut i = 0;
    if b[0] == b'V' {
        i = 1;
        if i >= b.len() || !b[i].is_ascii_digit() {
            return false;
        }
    } else if !b[0].is_ascii_digit() {
        return false;
    }
    let mut seen_digit = false;
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
        seen_digit |= b[i].is_ascii_digit();
        i += 1;
    }
    if !seen_digit {
        return false;
    }
    if b[0] != b'V' {
        // a leading number must be followed by the volt marker
        if i >= b.len() || b[i] != b'V' {
            return false;
        }
        i += 1;
    }
    is_word(&up[i..])
}

fn matches_power_name(name: &str) -> bool {
    let up = name.trim_start_matches(['+', '-']).to_ascii_uppercase();
    if POWER_EXACT.contains(&up.as_str()) {
        return true;
    }
    if POWER_PREFIXES
        .iter()
        .any(|p| up.starts_with(p) && is_word(&up[p.len()..]))
    {
        return true;
    }
    is_voltage_name(&up)
}

const SIGNAL_NAMES: &[&str] = &[
    "CLK", "CLOCK", "XTAL", "OSC", "SCL", "SDA", "SCK", "SDI", "SDO", "MISO", "MOSI", "RX", "TX",
    "SRX", "STX", "URX", "UTX", "USB", "DP", "DM", "DN", "CS", "CE", "RESET", "RST", "NRST", "EN",
    "INT", "IRQ", "SWD", "SWC", "SWO", "TDI", "TDO", "TCK", "TMS", "MCLK", "BCLK", "LRCK", "ROW",
    "COL",
];

/// A net a big pad count would otherwise sweep up: an auto-named net, a bus line, a port pin.
fn matches_signal_name(name: &str) -> bool {
    if name.starts_with("Net-(") || name.starts_with("unconnected-") {
        return true;
    }
    if let Some(rest) = name.strip_prefix("N$") {
        if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    let up = name.to_ascii_uppercase();
    if SIGNAL_NAMES
        .iter()
        .any(|p| up.starts_with(p) && is_word(&up[p.len()..]))
    {
        return true;
    }
    // a port pin: one to three letters then one to three digits (PA0, PB12, D3)
    let letters = up.chars().take_while(|c| c.is_ascii_alphabetic()).count();
    let digits = up[letters..].chars().take_while(|c| c.is_ascii_digit()).count();
    (1..=3).contains(&letters) && (1..=3).contains(&digits) && letters + digits == up.len()
}

/// Ground answers to more names than one capture tool's default: `SupplyGND`, `GND_A`, `VSSA`.
/// Matched as a substring, but a name ending in a signal's job is the pin that watches ground.
pub fn is_ground_name(name: &str) -> bool {
    let bare = bare_net_name(name);
    if bare.is_empty() || has_signal_suffix(bare) {
        return false;
    }
    let up = bare.to_ascii_uppercase();
    ["GND", "GROUND", "VSS", "EARTH"].iter().any(|k| up.contains(k))
}

/// Is this net a supply rail (or ground) rather than a signal? By name first, then by fan-out.
pub fn is_power_net(name: &str, pads: usize) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    let bare = bare_net_name(name);
    if has_signal_suffix(bare) {
        return false;
    }
    if matches_power_name(bare) || is_ground_name(name) {
        return true;
    }
    pads > BIG_NET_PADS && !(matches_signal_name(name) || matches_signal_name(bare))
}

const BIG_NET_PADS: usize = 8;
const POWER_FACTOR: f64 = 2.5;
const POWER_MIN: f64 = 0.5;
const POWER_MAX: f64 = 1.0;
const POWER_RATIO_MAX: f64 = 1.8;
/// A track may exceed the narrowest pad it lands on by this much.
const PAD_WIDTH_MARGIN: f64 = 0.1;

/// The width a rail gets on a board with nothing to measure, necked down to what its finest pad
/// can take so a rail never lands wider than a pad it must reach.
pub fn power_width(signal_width: f64, pads: &[&Pad]) -> f64 {
    let top = POWER_MAX.max(POWER_RATIO_MAX * signal_width);
    let mut w = (POWER_FACTOR * signal_width).max(POWER_MIN).min(top);
    let cap = pads
        .iter()
        .map(|p| p.size.0.min(p.size.1))
        .filter(|d| *d > 0.0)
        .fold(f64::INFINITY, f64::min);
    if cap.is_finite() {
        w = w.min(cap + PAD_WIDTH_MARGIN);
    }
    ((w.max(signal_width)) * 1000.0).round() / 1000.0
}

// ---- widths that follow the room on the board -----------------------------------
// A human's signal width follows the finest pad pitch on the board. The policy only ever WIDENS
// the empty-board default, and never overrides a width the board states itself.

const PITCH_WIDTH: &[(f64, f64)] = &[(1.30, 0.20), (2.80, 0.50), (f64::INFINITY, 0.80)];
const CROWDED_PARTS_PER_CM2: f64 = 3.0;
const POLICY_MAX: f64 = 1.0;

/// The tightest centre-to-centre distance between two pads of one footprint on different nets:
/// the narrowest channel any track on this board has to live in.
pub fn finest_pad_pitch(board: &Board) -> Option<f64> {
    let mut best: Option<f64> = None;
    for f in board.footprints() {
        let pads: Vec<&Pad> = f
            .pads
            .iter()
            .filter(|p| !p.copper_layers().is_empty() && p.size.0.min(p.size.1) > 0.0)
            .collect();
        for (i, a) in pads.iter().enumerate() {
            for b in &pads[i + 1..] {
                if a.net_id != 0 && a.net_id == b.net_id {
                    continue;
                }
                let d = dist(a.pos, b.pos);
                if d > 1e-6 && best.is_none_or(|x| d < x) {
                    best = Some(d);
                }
            }
        }
    }
    best
}

fn board_area(board: &Board) -> Option<f64> {
    if let Some(poly) = board.outline_polygon() {
        let a = polygon_area(&poly).abs();
        if a > 0.0 {
            return Some(a);
        }
    }
    let bb = board.outline_bbox()?;
    let a = bb.w().max(0.0) * bb.h().max(0.0);
    (a > 0.0).then_some(a)
}

/// The signal width the room on this board allows. Never finer than the empty-board default.
pub fn room_width(board: &Board) -> f64 {
    let Some(pitch) = finest_pad_pitch(board) else {
        return EMPTY_TRACK_WIDTH;
    };
    let mut w = PITCH_WIDTH
        .iter()
        .find(|(p, _)| pitch < *p)
        .map(|(_, v)| *v)
        .unwrap_or(EMPTY_TRACK_WIDTH);
    if let Some(area) = board_area(board) {
        let n = board.footprints().iter().filter(|f| !f.is_dnp()).count() as f64;
        if n / (area / 100.0) >= CROWDED_PARTS_PER_CM2 {
            w = EMPTY_TRACK_WIDTH; // packed this tight and the room is gone whatever the pitch says
        }
    }
    w.max(EMPTY_TRACK_WIDTH).min(POLICY_MAX)
}

/// A signal width the board is itself evidence for, or `None` when there is only silence to fill.
fn stated_signal_width(board: &Board, rules: Option<&Rules>) -> Option<f64> {
    if let Some(r) = rules {
        let w = r.track_width;
        if w <= TRACK_BOUNDS.1 + 1e-9 && (w - EMPTY_TRACK_WIDTH).abs() > 1e-9 {
            return Some(w);
        }
    }
    let tracks = board.tracks();
    if tracks.len() >= MIN_TRACKS_TO_MEASURE {
        return Some(tracks.iter().map(|t| t.width).fold(f64::INFINITY, f64::min));
    }
    None
}

/// What to route this board's signals at: what it says, else what it has room for.
pub fn policy_signal_width(board: &Board, rules: Option<&Rules>) -> f64 {
    stated_signal_width(board, rules).unwrap_or_else(|| room_width(board))
}

/// The width to draw a signal track at. With a `board` the policy fills the board's silence;
/// without one a stated width above the routing bound is a legacy rail width and is ignored.
pub fn signal_track_width(rules: &Rules, board: Option<&Board>) -> f64 {
    match board {
        Some(b) => policy_signal_width(b, Some(rules)),
        None => {
            if rules.track_width > TRACK_BOUNDS.1 + 1e-9 {
                EMPTY_TRACK_WIDTH
            } else {
                rules.track_width
            }
        }
    }
}

/// Track width per net name: rails wide, signals the policy width.
pub fn infer_net_widths(board: &Board, rules: &Rules) -> BTreeMap<String, f64> {
    let signal = policy_signal_width(board, Some(rules));
    let pads = board.pads_by_net();
    let mut out = BTreeMap::new();
    for net in board.nets() {
        if net.id == 0 || net.name.is_empty() {
            continue;
        }
        let net_pads: Vec<&Pad> = pads
            .get(&net.id)
            .map(|v| v.iter().map(|(_, p)| p).collect())
            .unwrap_or_default();
        let w = if is_power_net(&net.name, net_pads.len()) {
            power_width(signal, &net_pads)
        } else {
            signal
        };
        out.insert(net.name.clone(), (w * 1000.0).round() / 1000.0);
    }
    out
}

/// The widths worth handing the router: only nets wider than the signal rule.
pub fn router_net_widths(board: &Board, rules: &Rules) -> BTreeMap<String, f64> {
    let signal = policy_signal_width(board, Some(rules));
    infer_net_widths(board, rules)
        .into_iter()
        .filter(|(_, w)| *w > signal + 1e-9)
        .collect()
}

// ---- fine-pitch escapes ---------------------------------------------------------
// A pin field tighter than the board's own signal lane cannot fit a track at the board gauge: the
// pad cannot be left without breaking clearance to its neighbour. `fine_escape` reads the finer
// gauge such nets need off the board itself; the DSN writes it as a Specctra class.

const FINE_FLOOR_MM: f64 = 0.05;
/// Two neighbours flank a pad when their directions span more than 120 degrees.
const FINE_OPPOSITE: f64 = -0.5;
/// A two-pad part has no pin field to escape from.
const FINE_MIN_PADS: usize = 3;

#[derive(Debug, Clone)]
pub struct FineEscape {
    pub nets: Vec<String>,
    pub width: f64,
    pub clearance: f64,
    pub gap: f64,
    pub parts: Vec<String>,
}

fn rect_gap(a: &BBox, b: &BBox) -> f64 {
    let dx = (a.x0 - b.x1).max(0.0).max(b.x0 - a.x1);
    let dy = (a.y0 - b.y1).max(0.0).max(b.y0 - a.y1);
    dx.hypot(dy)
}

fn layers_meet(a: &Pad, b: &Pad) -> bool {
    let la = a.copper_layers();
    b.copper_layers().iter().any(|l| la.contains(l))
}

/// `(band, gap)` for a pad flanked by two foreign pads, or `None` when it is not flanked: a pad
/// with a neighbour on only one side is not a fine-pitch escape — the track leaves the other way.
fn escape_band(pad: &Pad, others: &[&Pad]) -> Option<(f64, f64)> {
    let bx = pad.bbox();
    let mut near: Vec<(f64, f64, (f64, f64))> = Vec::new();
    for q in others {
        if std::ptr::eq(*q, pad) || q.size.0.min(q.size.1) <= 0.0 {
            continue;
        }
        if pad.net_id != 0 && pad.net_id == q.net_id {
            continue;
        }
        if !layers_meet(pad, q) {
            continue;
        }
        let d = dist(pad.pos, q.pos);
        if d <= 1e-9 {
            continue;
        }
        near.push((
            d,
            rect_gap(&bx, &q.bbox()),
            ((q.pos.0 - pad.pos.0) / d, (q.pos.1 - pad.pos.1) / d),
        ));
    }
    near.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let (_, g1, u1) = *near.first()?;
    for (_, g2, u2) in &near[1..] {
        if u1.0 * u2.0 + u1.1 * u2.1 <= FINE_OPPOSITE {
            let across = u1.0.abs() * (bx.x1 - bx.x0) + u1.1.abs() * (bx.y1 - bx.y0);
            return Some((g1 + across + g2, g1.min(*g2)));
        }
    }
    None
}

/// The gauge this board's fine-pitch escapes need, or `None` when it has none.
pub fn fine_escape(board: &Board, rules: &Rules) -> Option<FineEscape> {
    let width = policy_signal_width(board, Some(rules));
    let clearance = rules.clearance;
    // The threshold is the signal LANE (width plus clearance both sides), not the net's own width:
    // measured per net, an ordinary 0402 would count as fine pitch.
    let need = width + 2.0 * clearance;
    let mut nets: Vec<String> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    let mut gaps: Vec<f64> = Vec::new();
    let fps: Vec<Footprint> = board.footprints();
    for f in &fps {
        let pads: Vec<&Pad> = f
            .pads
            .iter()
            .filter(|p| !p.copper_layers().is_empty() && p.size.0.min(p.size.1) > 0.0)
            .collect();
        if pads.len() < FINE_MIN_PADS {
            continue;
        }
        let mut hit = false;
        for p in &pads {
            let Some((_, gap)) = escape_band(p, &pads) else {
                continue;
            };
            if gap >= need - 1e-9 {
                continue;
            }
            hit = true;
            gaps.push(gap);
            let name = if p.net_name.is_empty() {
                board
                    .nets()
                    .into_iter()
                    .find(|n| n.id == p.net_id)
                    .map(|n| n.name)
                    .unwrap_or_default()
            } else {
                p.net_name.clone()
            };
            if !name.is_empty() && !nets.contains(&name) {
                nets.push(name);
            }
        }
        if hit {
            parts.push(f.ref_.clone());
        }
    }
    if nets.is_empty() {
        return None;
    }
    nets.sort();
    parts.sort();
    let w = width.clamp(TRACK_BOUNDS.0, width);
    let c = clearance.max(FINE_FLOOR_MM.min(clearance)).min(clearance);
    Some(FineEscape {
        nets,
        width: (w * 1000.0).round() / 1000.0,
        clearance: (c * 10000.0).round() / 10000.0,
        gap: gaps.iter().cloned().fold(f64::INFINITY, f64::min),
        parts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_the_blue_pill_net_names() {
        for rail in ["GND", "+3V3", "+5V", "VSSA", "VCC", "3.3V", "/power/+3V3"] {
            assert!(is_power_net(rail, 2), "{rail} should read as a rail");
        }
        for sig in ["PA0", "PB12", "PC13", "SWDIO", "USB_DM", "NRST", "OSC_IN"] {
            assert!(!is_power_net(sig, 3), "{sig} should read as a signal");
        }
        assert!(is_ground_name("GND") && is_ground_name("VSSA") && is_ground_name("SupplyGND"));
        assert!(!is_ground_name("GND_SENSE"));
        assert!(!is_power_net("VOUT_FB", 2));
    }

    #[test]
    fn a_rail_is_wider_than_a_signal_but_never_wider_than_its_pad() {
        assert_eq!(power_width(0.2, &[]), 0.5);
        let pad = Pad {
            number: "1".into(),
            kind: "smd".into(),
            shape: "rect".into(),
            pos: (0.0, 0.0),
            rot: 0.0,
            size: (1.5, 0.3),
            drill: None,
            layers: vec!["F.Cu".into()],
            net_id: 1,
            net_name: "GND".into(),
            roundrect_ratio: None,
            node_key: (0, 0),
            custom_points: vec![],
        };
        assert_eq!(power_width(0.2, &[&pad]), 0.4);
    }
}
