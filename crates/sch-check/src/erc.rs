//! Deterministic electrical-rule checks — the structural + exact-math layer UNDER the LLM review
//! ensemble (`agent::review`). The reviewer is strong on judgment but weak on arithmetic (it
//! consistently missed a feedback-divider value error in the recall harness); these checks compute
//! the numbers exactly where the netlist makes them unambiguous, so the two layers are complementary:
//! deterministic where the netlist is unambiguous, LLM lenses where judgment is needed.
//!
//! Two families:
//! * **Quantitative** — values/voltages must parse and the result be clearly out of range
//!   (LED current, feedback-divider ratio).
//! * **Topological** — the rules a senior reviewer runs first, from netlist shape alone: missing
//!   decoupling on a large IC, an I2C/open-drain net with no pull-up, a floating control input, an
//!   undriven rail, two outputs shorted together (plus dangling parts, crystal load caps, LED
//!   polarity). Each reuses the decoupling-idiom IC→cap / rail grouping where it can.
//!
//! FP-averse is the governing constraint: every check fires ONLY on an unambiguous defect — a false
//! positive would make the agent "fix" a correct design, which is worse than a miss. Each topological
//! check therefore leans conservative (high IC-pin threshold for decoupling, exact `SDA`/`SCL` token
//! match, name-driven input/output vocabularies, a power-flag gate for undriven rails) and is
//! calibrated against the known-good `crates/sch-floorplan/tests/fixtures/validation/*` corpus, where
//! it finds only real fixture defects (two lack I2C pull-ups; `stm32f4-buck` shorts L1). The
//! per-check rustdoc
//! states the heuristic and its known limits.
//!
//! The name-driven rules resolve each pin key through the symbol table, so they read a
//! number-keyed design — what an extractor or a tool call produces — exactly as they read an
//! authored one.

use crate::model::*;
use crate::{SymbolMeta, SymbolTable};
use std::collections::HashMap;

/// Common regulator/converter feedback reference voltages, for divider-ratio checks.
const VREFS: &[f64] = &[0.6, 0.765, 0.8, 1.0, 1.182, 1.21, 1.225, 1.24, 1.25];
const LED_VF: f64 = 1.8; // typical forward drop; deliberately low so the current estimate is generous

/// Parse a component value like `10k`, `1.5k`, `330R`, `4R7`, `22pF`, `100nF`, `4.7uF`, `2M2`
/// into a base-unit number (ohms / farads / henries). `None` if it isn't a plain numeric value.
pub fn parse_value(s: &str) -> Option<f64> {
    let s = s.trim().trim_end_matches(['F', 'H', 'Ω']).trim();
    if let Ok(n) = s.parse::<f64>() {
        return Some(n);
    }
    const SCALES: &[(char, f64)] = &[
        ('p', 1e-12),
        ('n', 1e-9),
        ('u', 1e-6),
        ('µ', 1e-6),
        ('m', 1e-3),
        ('R', 1.0),
        ('r', 1.0),
        ('k', 1e3),
        ('K', 1e3),
        ('M', 1e6),
        ('G', 1e9),
    ];
    for &(c, scale) in SCALES {
        if let Some(idx) = s.find(c) {
            let (a, b) = (&s[..idx], &s[idx + c.len_utf8()..]);
            let b = b.trim();
            let num = if b.is_empty() {
                a.to_string()
            } else {
                format!("{a}.{b}")
            };
            if let Ok(n) = num.parse::<f64>() {
                return Some(n * scale);
            }
        }
    }
    None
}

/// Parse a power-rail net name into volts when it is unambiguous: `3V3`→3.3, `5V`→5, `1V8`→1.8,
/// `12V`→12, `3.3V`→3.3, `+5V`→5, `GND`/`VSS`→0. Ambiguous names (`VOUT`, `V12`, `VCC`) → `None`.
pub fn rail_voltage(net: &str) -> Option<f64> {
    let n = net.trim().trim_start_matches('+').to_uppercase();
    if circuit_graph::netclass::is_ground(&n) {
        return Some(0.0);
    }
    // Common net-label spelling with a leading voltage-domain marker:
    // V3V3 / V1V8. Do not generalize V5 or VIN5, which remain ambiguous.
    if let Some(rest) = n.strip_prefix('V')
        && rest.contains('V')
        && let Some(volts) = rail_voltage(rest)
    {
        return Some(volts);
    }
    // d.dV (e.g. 3.3V); else dVd (e.g. 3V3, 1V8 — V is the decimal point) is handled below.
    if let Some(p) = n.strip_suffix('V')
        && let Ok(v) = p.parse::<f64>()
    {
        return Some(v);
    }
    if let Some(vpos) = n.find('V') {
        let (a, b) = (&n[..vpos], &n[vpos + 1..]);
        if !a.is_empty()
            && a.chars().all(|c| c.is_ascii_digit())
            && b.chars().all(|c| c.is_ascii_digit())
        {
            let s = if b.is_empty() {
                a.to_string()
            } else {
                format!("{a}.{b}")
            };
            if let Ok(v) = s.parse::<f64>() {
                return Some(v);
            }
        }
    }
    // SoC/FPGA core-rail convention: V<2 digits> = deci-volts (V12=1.2, V33=3.3, V25=2.5, V18=1.8).
    // Bounded to low-voltage core rails (0.8-6 V), where the V-prefix naming is unambiguous; a true
    // 12 V rail is conventionally "+12V"/"12V" (handled above), not "V12".
    if let Some(d) = n.strip_prefix('V') {
        let b = d.as_bytes();
        if b.len() == 2 && b.iter().all(u8::is_ascii_digit) {
            let v = (b[0] - b'0') as f64 + (b[1] - b'0') as f64 / 10.0;
            if (0.8..=6.0).contains(&v) {
                return Some(v);
            }
        }
    }
    None
}

struct Item<'a> {
    refdes: &'a str,
    comp: &'a Component,
    nets: Vec<&'a str>,
    /// Connected pins with their keys resolved against the symbol.
    pins: Vec<Pin<'a>>,
}

/// One connected pin, seen both ways: as the model wrote it (`key` — a number on
/// an extracted design, a name on an authored one) and as the symbol names it.
/// The name-based rules below match either, so they read the same circuit the
/// same way whichever front end built it.
struct Pin<'a> {
    key: &'a str,
    name: &'a str,
    net: &'a str,
}

impl Pin<'_> {
    fn is(&self, pred: impl Fn(&str) -> bool) -> bool {
        pred(self.name) || pred(self.key)
    }
}

impl<'a> Item<'a> {
    /// The net on the first pin satisfying `pred`, **by name before key**: a
    /// numeric key must never win over the pin the symbol actually names, or a
    /// part whose pin 2 is not its anode would answer the anode question.
    fn pin_net(&self, pred: impl Fn(&str) -> bool) -> Option<&'a str> {
        self.pins
            .iter()
            .find(|p| pred(p.name))
            .or_else(|| self.pins.iter().find(|p| pred(p.key)))
            .map(|p| p.net)
    }
}

fn nets_of(c: &Component) -> Vec<&str> {
    let mut v = Vec::new();
    for t in c
        .pins
        .values()
        .chain(c.units.values().flatten().map(|(_, t)| t))
    {
        if let PinTarget::Net(n) = t {
            v.push(n.as_str());
        }
    }
    v
}

/// The net at the *other* end of a 2-net part from `net`.
fn far<'a>(r: &Item<'a>, net: &str) -> &'a str {
    if r.nets[0] == net {
        r.nets[1]
    } else {
        r.nets[0]
    }
}

fn is_resistor(c: &Component) -> bool {
    c.part.contains(":R") || c.part.ends_with("Device:R") || c.part == "R"
}
fn is_led(c: &Component) -> bool {
    c.part.to_uppercase().contains("LED")
}
fn is_cap(c: &Component) -> bool {
    c.part.contains(":C") || c.part == "C"
}
fn is_passive(c: &Component) -> bool {
    is_resistor(c) || is_cap(c) || c.part.contains(":L")
}
fn on_gnd(it: &Item) -> bool {
    it.nets.iter().any(|n| rail_voltage(n) == Some(0.0))
}
fn is_diode(c: &Component) -> bool {
    let part = c.part.to_ascii_uppercase();
    part.contains("LED")
        || part.contains("DIODE")
        || part.ends_with(":D")
        || part.contains(":D_")
}
fn is_connector(c: &Component) -> bool {
    c.part.to_uppercase().contains("CONNECTOR")
}
/// A `power:*` library part — a net-flag symbol that *declares/sources* a rail
/// (`power:+3V3`, `power:VCC`, `power:GND`). Treated as a rail source.
fn is_power_symbol(c: &Component) -> bool {
    c.part.to_ascii_lowercase().starts_with("power:")
}
fn is_regulator(c: &Component) -> bool {
    let p = c.part.to_uppercase();
    p.contains("REGULATOR") || p.contains("DCDC") || p.contains("DC-DC")
}

/// Total resolved pins on a component (the flat map plus all unit maps).
fn pin_count(c: &Component) -> usize {
    c.pins.len() + c.units.values().map(|u| u.len()).sum::<usize>()
}

/// The connected pins of a component, each key resolved against `meta` when the
/// symbol is known. `NoConnect` pins are dropped.
fn pins_of<'a>(c: &'a Component, meta: Option<&'a SymbolMeta>) -> Vec<Pin<'a>> {
    c.pins
        .iter()
        .chain(c.units.values().flatten())
        .filter_map(|(k, t)| match t {
            PinTarget::Net(n) => Some((k.as_str(), n.as_str())),
            PinTarget::NoConnect => None,
        })
        .map(|(key, net)| {
            let name = meta
                .and_then(|m| {
                    crate::pins::resolve(m, key)
                        .first()
                        .map(|p| p.name.as_str())
                })
                .unwrap_or(key);
            Pin { key, name, net }
        })
        .collect()
}

fn pin_net_alias<'a>(it: &Item<'a>, aliases: &[&str]) -> Option<&'a str> {
    it.pin_net(|s| aliases.iter().any(|a| s.eq_ignore_ascii_case(a)))
}

/// A 2-pin decoupling/bypass cap bridging `rail` and a ground net — the unit the
/// decoupling idiom co-places beside its anchor IC.
fn is_bypass_cap_on(it: &Item, rail: &str) -> bool {
    is_cap(it.comp) && it.comp.pins.len() == 2 && it.nets.contains(&rail) && on_gnd(it)
}

/// Does `net` carry a pull-up — a 2-pin resistor from `net` to a *positive* rail?
fn has_pullup_to_rail(net: &str, items: &[Item], net_items: &HashMap<&str, Vec<usize>>) -> bool {
    net_items.get(net).into_iter().flatten().any(|&ri| {
        let r = &items[ri];
        is_resistor(r.comp)
            && r.nets.len() == 2
            && rail_voltage(far(r, net)).is_some_and(|v| v > 0.0)
    })
}

/// Run all deterministic quantitative checks, returning defect lines (same `- REFDES: ...` shape the
/// LLM review emits, so the agent's run_turn_reviewed can union them).
pub fn erc_checks(d: &Design, provider: &SymbolTable) -> Vec<String> {
    defects(d, provider).into_iter().map(|df| df.line).collect()
}

/// A deterministic electrical finding and whether it blocks.
pub struct Defect {
    pub blocking: bool,
    pub line: String,
}

/// The checks above, split by what a finding means. A *blocking* defect is one the
/// netlist itself proves wrong — a 2-pin part that cannot conduct, two push-pull
/// outputs on one node, a fixed-pinout optocoupler reversed: no placement or value
/// choice makes it work, so the turn may not end on one. The rest read intent from
/// topology (a diode that looks reversed, a missing bypass cap, an unusual divider);
/// a design may legitimately answer for those, and a gate the author cannot clear
/// costs more than the defect.
pub fn defects(d: &Design, provider: &SymbolTable) -> Vec<Defect> {
    let mut blocking = Vec::new();
    let mut advisory = Vec::new();
    run_checks(d, provider, &mut blocking, &mut advisory);
    let mark = |lines: Vec<String>, blocking: bool| {
        lines
            .into_iter()
            .map(move |line| Defect { blocking, line })
    };
    mark(blocking, true).chain(mark(advisory, false)).collect()
}

fn run_checks(
    d: &Design,
    provider: &SymbolTable,
    blocking: &mut Vec<String>,
    advisory: &mut Vec<String>,
) {
    let lib_ids: std::collections::BTreeSet<&str> = d
        .blocks
        .values()
        .flat_map(|b| b.components.values())
        .map(|c| c.part.as_str())
        .collect();
    let symbols: HashMap<&str, SymbolMeta> = lib_ids
        .into_iter()
        .filter_map(|id| provider.symbol(id).map(|m| (id, m)))
        .collect();
    let items: Vec<Item> = d
        .blocks
        .values()
        .flat_map(|b| b.components.iter())
        .filter(|(_, c)| !c.dnp)
        .map(|(rd, c)| Item {
            refdes: rd.as_str(),
            comp: c,
            nets: nets_of(c),
            pins: pins_of(c, symbols.get(c.part.as_str())),
        })
        .collect();
    // net -> indices into items
    let mut net_items: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        for &n in &it.nets {
            net_items.entry(n).or_default().push(i);
        }
    }
    check_phototransistor_optocoupler_polarity(&items, blocking);
    check_led_indicator_polarity(&items, &net_items, blocking);
    check_output_short(&items, &net_items, blocking);
    check_dangling(&items, blocking);

    check_led_current(&items, &net_items, advisory);
    check_fb_divider(&items, &net_items, advisory);
    check_555_timing_topology(&items, &net_items, advisory);
    check_crystal(&items, &net_items, advisory);
    check_missing_decoupling(&items, &net_items, advisory);
    check_missing_pullup(&items, &net_items, advisory);
    check_floating_input(&items, &net_items, advisory);
    check_undriven_rail(&items, &net_items, advisory);
}

/// Catch a reversed output transistor on optocouplers whose numeric pinout is fixed and
/// unambiguous. PC817/LTV-817-family parts use 3=emitter and 4=collector. Grounding the
/// collector while exposing the emitter as the output reverses the intended NPN current
/// path; KiCad's symbols type both pins as anonymous passive pins, so ordinary ERC cannot
/// distinguish this from the correct emitter-to-ground connection.
///
/// Keep this deliberately narrow: emitter-follower optocoupler circuits are valid, and
/// unknown optocoupler families may use another pin order. We therefore only diagnose the
/// known 817 family and only the impossible positive-domain shape (C on 0 V, E off 0 V).
fn check_phototransistor_optocoupler_polarity(items: &[Item], out: &mut Vec<String>) {
    for it in items {
        let part = it.comp.part.to_ascii_uppercase();
        if !(part.contains("PC817") || part.contains("LTV-817") || part.contains("LTV817")) {
            continue;
        }
        let emitter = pin_net_alias(it, &["3", "E", "EMITTER"]);
        let collector = pin_net_alias(it, &["4", "C", "COLLECTOR"]);
        if let (Some(emitter), Some(collector)) = (emitter, collector)
            && rail_voltage(collector) == Some(0.0)
            && rail_voltage(emitter) != Some(0.0)
        {
            out.push(format!(
                "- {}: {} output transistor is BACKWARDS — pin 4 collector is on {collector} while pin 3 emitter is on {emitter}; this family requires emitter to ground and collector to the pulled-up output",
                it.refdes, it.comp.part
            ));
        }
    }
}

/// A 555 timer whose discharge transistor is wired directly onto the same
/// capacitor node as both trigger and threshold cannot implement the standard
/// astable/monostable timing path: DIS must reach that capacitor through a
/// timing resistor. Restrict this to unambiguous single-555 Timer symbols and a
/// grounded capacitor on the shared node, so unrelated numbered ICs and legal
/// 555 modes stay silent.
fn check_555_timing_topology(
    items: &[Item],
    net_items: &HashMap<&str, Vec<usize>>,
    out: &mut Vec<String>,
) {
    for timer in items {
        let part = timer.comp.part.to_uppercase();
        if !part.starts_with("TIMER:") || !part.contains("555") || part.contains("556") {
            continue;
        }
        let (Some(trigger), Some(threshold), Some(discharge)) = (
            pin_net_alias(timer, &["2", "TR", "TRIG", "TRIGGER"]),
            pin_net_alias(timer, &["6", "THR", "THRESH", "THRESHOLD"]),
            pin_net_alias(timer, &["7", "DIS", "DISCH", "DISCHARGE"]),
        ) else {
            continue;
        };
        if trigger != threshold || trigger != discharge {
            continue;
        }
        let has_timing_cap = net_items
            .get(trigger)
            .into_iter()
            .flatten()
            .any(|&i| is_cap(items[i].comp) && on_gnd(&items[i]));
        if has_timing_cap {
            out.push(format!(
                "- {}: 555 DIS, TR, and THR all tie directly to timing-capacitor net {trigger} — DIS must reach that node through the timing resistor, not short directly onto it",
                timer.refdes
            ));
        }
    }
}

/// Minimum pin count for the [`check_missing_decoupling`] anchor. Set high (caps the
/// check to MCUs / FPGAs / large mixed-signal ICs) so simple ≤14-pin parts — a 555,
/// an 8-pin op-amp, a level translator — are NOT required to carry a local bypass
/// cap; those are the borderline cases where demanding decoupling produces noise.
const DECOUPLE_MIN_PINS: usize = 16;

/// Reject the reversed form of the LED-indicator idiom: an LED anode on ground whose
/// cathode reaches a known positive rail through its current-limiting resistor.
/// Only LED-named library symbols participate, so signal clamps and flyback diodes
/// remain outside this rule even when they share the same rail-facing topology.
fn check_led_indicator_polarity(
    items: &[Item],
    net_items: &HashMap<&str, Vec<usize>>,
    out: &mut Vec<String>,
) {
    for led in items {
        if !is_led(led.comp) {
            continue;
        }
        let Some(anode) = pin_net_alias(led, &["2", "A", "+"]) else {
            continue;
        };
        let Some(cathode) = pin_net_alias(led, &["1", "K", "-"]) else {
            continue;
        };
        if rail_voltage(anode) != Some(0.0) {
            continue;
        }
        let rail_resistor = net_items
            .get(cathode)
            .into_iter()
            .flatten()
            .map(|&index| &items[index])
            .find_map(|resistor| {
                if !is_resistor(resistor.comp) || resistor.nets.len() != 2 {
                    return None;
                }
                let rail = far(resistor, cathode);
                rail_voltage(rail)
                    .is_some_and(|voltage| voltage > 0.0)
                    .then_some((resistor.refdes, rail))
            });
        if let Some((resistor, rail)) = rail_resistor {
            out.push(format!(
                "- {}: LED is reversed — its cathode reaches {rail} through {resistor} while its anode is on {anode}; swap {}: anode should face {rail} through {resistor}, and cathode should face GND",
                led.refdes, led.refdes
            ));
        }
    }
}

/// A 2-terminal passive that can't conduct: a pin left unconnected, or both pins on one net.
fn check_dangling(items: &[Item], out: &mut Vec<String>) {
    for it in items {
        if !is_passive(it.comp) || it.comp.pins.len() != 2 {
            continue;
        }
        if it.nets.len() < 2 {
            out.push(format!(
                "- {}: a 2-pin part has a pin left unconnected — it cannot conduct (does nothing)",
                it.refdes
            ));
        } else if it.nets[0] == it.nets[1] {
            out.push(format!(
                "- {}: both pins are on the same net ({}) — the part is shorted out",
                it.refdes, it.nets[0]
            ));
        }
    }
}

/// A 2-pin crystal whose pin has no load capacitor to ground — unreliable / no oscillation.
fn check_crystal(items: &[Item], net_items: &HashMap<&str, Vec<usize>>, out: &mut Vec<String>) {
    for it in items {
        if !it.comp.part.contains("Crystal") || it.nets.len() != 2 {
            continue;
        }
        for &n in &it.nets {
            let has_load = net_items
                .get(n)
                .into_iter()
                .flatten()
                .any(|&ci| is_cap(items[ci].comp) && on_gnd(&items[ci]));
            if !has_load {
                out.push(format!(
                    "- {}: crystal pin on net {} has no load capacitor to ground — unreliable oscillation",
                    it.refdes, n
                ));
                break;
            }
        }
    }
}

/// LED + series resistor between a rail and ground: I = (Vrail - Vf)/R. Flag clear overcurrent
/// (>50 mA) or a current too low to light it (<0.1 mA), only when both the rail voltage and the
/// resistor value parse.
fn check_led_current(items: &[Item], net_items: &HashMap<&str, Vec<usize>>, out: &mut Vec<String>) {
    for led in items.iter().filter(|it| is_led(it.comp)) {
        if led.nets.len() != 2 {
            continue;
        }
        for (k, &junction) in led.nets.iter().enumerate() {
            let led_far = led.nets[1 - k];
            for &ri in net_items.get(junction).into_iter().flatten() {
                let r = &items[ri];
                if !is_resistor(r.comp) || r.nets.len() != 2 {
                    continue;
                }
                let r_far = if r.nets[0] == junction {
                    r.nets[1]
                } else {
                    r.nets[0]
                };
                let (Some(rval), Some(va), Some(vb)) = (
                    r.comp.value.as_deref().and_then(parse_value),
                    rail_voltage(led_far),
                    rail_voltage(r_far),
                ) else {
                    continue;
                };
                let vrail = va.max(vb);
                if vrail <= LED_VF || rval <= 0.0 {
                    continue;
                }
                let i_ma = (vrail - LED_VF) / rval * 1000.0;
                if i_ma > 50.0 {
                    out.push(format!(
                        "- {}: LED current ~{:.0} mA is excessive — R{} = {} on a {:.1} V rail (I=(V-Vf)/R); \
                         expect ~1-20 mA, use a larger series resistor",
                        led.refdes, i_ma, r.refdes, r.comp.value.as_deref().unwrap_or("?"), vrail
                    ));
                } else if i_ma < 0.1 {
                    out.push(format!(
                        "- {}: LED current ~{:.3} mA is too low to light it — R{} = {} on a {:.1} V rail is far too large",
                        led.refdes, i_ma, r.refdes, r.comp.value.as_deref().unwrap_or("?"), vrail
                    ));
                }
            }
        }
    }
}

/// Regulator feedback divider: a net whose name marks it as feedback (`*FB`, `*FEEDBACK`, `*VSENSE`)
/// carrying exactly two resistors — one to an output rail of a clearly-named voltage, one to ground.
/// Vout = Vref·(1 + Rtop/Rbot); flag if NO common Vref lands within 20 % of the rail's named voltage.
fn check_fb_divider(items: &[Item], net_items: &HashMap<&str, Vec<usize>>, out: &mut Vec<String>) {
    for (&net, idxs) in net_items {
        let up = net.to_uppercase();
        let is_fb = up.ends_with("FB")
            || up.ends_with("_FB")
            || up.contains("FEEDBACK")
            || up.contains("VSENSE");
        if !is_fb {
            continue;
        }
        let rs: Vec<&Item> = idxs
            .iter()
            .map(|&i| &items[i])
            .filter(|it| is_resistor(it.comp) && it.nets.len() == 2)
            .collect();
        if rs.len() != 2 {
            continue;
        }
        // classify: one R goes to GND (Rbot), the other to an output rail (Rtop).
        let (mut rtop, mut rbot, mut target) = (None, None, None);
        for r in &rs {
            let f = far(r, net);
            match rail_voltage(f) {
                Some(v) if v <= 0.0 => rbot = r.comp.value.as_deref().and_then(parse_value),
                Some(v) => {
                    rtop = r.comp.value.as_deref().and_then(parse_value);
                    target = Some(v);
                }
                None => {}
            }
        }
        let (Some(rt), Some(rb), Some(tgt)) = (rtop, rbot, target) else {
            continue;
        };
        if rb <= 0.0 || tgt <= 0.0 {
            continue;
        }
        let vout = |vref: f64| vref * (1.0 + rt / rb);
        let ok = VREFS
            .iter()
            .any(|&vr| ((vout(vr) - tgt) / tgt).abs() < 0.20);
        if !ok {
            let lo = vout(*VREFS.first().unwrap());
            let hi = vout(*VREFS.last().unwrap());
            let rd = rs.iter().map(|r| r.refdes).collect::<Vec<_>>().join("/");
            out.push(format!(
                "- {}: feedback-divider ratio is wrong for a {:.2} V rail — Rtop/Rbot = {:.0}/{:.0} gives \
                 ~{:.1}-{:.1} V across standard references, not {:.2} V",
                rd, tgt, rt, rb, lo, hi, tgt
            ));
        }
    }
}

/// **missing-decoupling**: a large powered IC with no local bypass/decoupling cap on
/// its supply. The anchor is a component with ≥ [`DECOUPLE_MIN_PINS`] pins that is
/// neither a connector, a passive, a diode, nor a `power:*` flag — i.e. an MCU / FPGA /
/// large mixed-signal chip. For each *positive* rail the anchor sits on, we look for a
/// 2-pin cap that bridges that same rail and ground (the decoupling-idiom IC→cap
/// grouping reused from the layout library: a bypass cap is a rail↔GND cap on a power
/// net that also reaches the anchor). If a powered rail has none, we flag the IC.
///
/// FP-averse:
/// * The high default pin threshold (16) excludes the ambiguous small-IC cases — a
///   555, an 8-pin op-amp, a logic gate — where requiring local decoupling is opinion,
///   not rule. It targets the chips where a senior reviewer *always* expects it.
/// * Only rails whose voltage parses unambiguously (`rail_voltage > 0`) are checked, so
///   a chip on an un-named/derived supply node is never faulted.
/// * Grouping is by NET, not by source block, so a cap declared in a separate power
///   block still counts for the IC it shares the rail with.
///
/// Known limits: doesn't judge cap *count* or value (one 100 nF satisfies a 100-ball
/// FPGA here); a chip whose only supply is a non-parseable net name is skipped.
fn check_missing_decoupling(
    items: &[Item],
    net_items: &HashMap<&str, Vec<usize>>,
    out: &mut Vec<String>,
) {
    for ic in items {
        let c = ic.comp;
        if pin_count(c) < DECOUPLE_MIN_PINS
            || is_connector(c)
            || is_passive(c)
            || is_diode(c)
            || is_power_symbol(c)
        {
            continue;
        }
        let mut powered_rails: Vec<&str> = ic
            .nets
            .iter()
            .copied()
            .filter(|n| rail_voltage(n).is_some_and(|v| v > 0.0))
            .collect();
        powered_rails.sort();
        powered_rails.dedup();
        let undecoupled: Vec<&str> = powered_rails
            .into_iter()
            .filter(|&rail| {
                !net_items
                    .get(rail)
                    .into_iter()
                    .flatten()
                    .any(|&ci| is_bypass_cap_on(&items[ci], rail))
            })
            .collect();
        if !undecoupled.is_empty() {
            out.push(format!(
                "- {}: powered IC ({} pins) has no decoupling/bypass capacitor on rail {} — \
                 add a local rail-to-GND bypass cap (e.g. 100nF) close to the supply pins",
                ic.refdes,
                pin_count(c),
                undecoupled.join("/")
            ));
        }
    }
}

/// **missing-pullup**: an I2C-style open-drain bus net (named `SDA`/`SCL`, with the
/// usual decorations — `I2C1_SDA`, `SDA0`, `SCL_3V3`) carrying ≥2 connections but with
/// no pull-up resistor to a positive rail. Open-drain buses can't idle high without an
/// external pull-up, so this is a real functional defect.
///
/// FP-averse:
/// * Net-name heuristic only fires on the unambiguous `SDA`/`SCL` token — separated by
///   non-alphanumerics so `PSDA`/`MISCL` don't match — not on the broad `I2C`, because
///   a power/ground or label net could incidentally contain it.
/// * Requires the net to actually be used by ≥2 pins; a single-pin off-sheet port stub
///   is left to the `single-pin-net` lint.
/// * A pull-up is specifically a 2-pin resistor to a *positive* rail ([`has_pullup_to_rail`]),
///   so a series/termination resistor to another signal doesn't count and, conversely,
///   isn't mistaken for the bus needing one.
///
/// Known limits: open-drain pins NOT on an SDA/SCL-named net (a bare `~{INT}` or a
/// generic open-collector output) aren't detected — pin electrical type isn't carried
/// on the kernel `Design`, so the check stays conservative and name-driven.
fn check_missing_pullup(
    items: &[Item],
    net_items: &HashMap<&str, Vec<usize>>,
    out: &mut Vec<String>,
) {
    let is_i2c = |net: &str| {
        let u = net.to_uppercase();
        ["SDA", "SCL"].iter().any(|tok| {
            u.match_indices(tok).any(|(i, _)| {
                let before = u[..i].chars().next_back();
                let after = u[i + tok.len()..].chars().next();
                let edge = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric());
                edge(before) && edge(after)
            })
        })
    };
    let mut nets: Vec<&str> = net_items.keys().copied().filter(|n| is_i2c(n)).collect();
    nets.sort();
    for net in nets {
        let degree = net_items.get(net).map_or(0, |v| v.len());
        if degree < 2 {
            continue;
        }
        if !has_pullup_to_rail(net, items, net_items) {
            out.push(format!(
                "- {net}: I2C/open-drain net has no pull-up resistor to a rail — an open-drain bus \
                 cannot idle high; add a pull-up (typically 2.2k-10k to the bus rail)"
            ));
        }
    }
}

/// **floating-input**: a net that touches exactly ONE pin (truly unconnected) on a
/// multi-pin IC, where the pin name reads as an INPUT/control function. A logic input
/// left floating picks up noise and latches randomly, so it must be driven, pulled, or
/// explicitly marked no-connect.
///
/// FP-averse — this is the easiest check to make noisy, so it is deliberately narrow:
/// * Net degree must be exactly 1. Explicit `NoConnect` pins are already dropped by
///   [`pins_of`]/`nets_of`, so an intentional NC never reaches here.
/// * The owner must be a real IC (≥8 pins, not connector/passive/power) — a dangling
///   2-pin passive is the `check_dangling` case, and a single-pin header/port pin is an
///   intentional board I/O, not a floating input.
/// * The pin NAME must match a conservative input/enable vocabulary (`EN`, `CE`, `OE`,
///   `RST`/`RESET`, `nRST`, `CS`, `IN`, `A0`…); a bare numbered pin or an output/IO pin
///   is NOT flagged, because we can't prove a numbered pin is an input without pin types.
/// * Rails (`rail_voltage` parses) are skipped — a control pin tied straight to a rail
///   is driven.
///
/// Known limits: only catches a floating input on a *named* control pin of a large IC;
/// floating bidirectional/IO pins and floating numbered pins are intentionally missed
/// to stay false-positive-free without pin electrical types.
fn check_floating_input(
    items: &[Item],
    net_items: &HashMap<&str, Vec<usize>>,
    out: &mut Vec<String>,
) {
    // Conservative control/enable-input vocabulary. A token matches when the pin name
    // IS the token or extends it by ≤2 chars (e.g. `EN`, `EN1`, `CE0`, `nRST`) — never a
    // long arbitrary name that merely starts with these letters.
    let looks_input = |key: &str| {
        let k = key
            .trim_start_matches(['~', '{', '/', '!', '#'])
            .to_uppercase();
        const TOKENS: &[&str] = &["EN", "CE", "OE", "CS", "RST", "RESET", "NRST", "MR", "SHDN"];
        TOKENS
            .iter()
            .any(|t| k.starts_with(t) && k.len() <= t.len() + 2)
    };
    let mut hits: Vec<String> = Vec::new();
    for ic in items {
        let c = ic.comp;
        if pin_count(c) < 8 || is_connector(c) || is_passive(c) || is_power_symbol(c) {
            continue;
        }
        for pin in &ic.pins {
            // Report the pin the reader recognizes — its symbol NAME — whichever
            // way the design keyed it.
            let (key, net) = (pin.name, pin.net);
            if rail_voltage(net).is_some() || !pin.is(looks_input) {
                continue;
            }
            if net_items.get(net).map_or(0, |v| v.len()) == 1 {
                hits.push(format!(
                    "- {}: input/control pin {} (net {}) is left floating — no driver and no pull \
                     resistor; drive it, add a pull-up/down, or mark it no-connect",
                    ic.refdes, key, net
                ));
            }
        }
    }
    hits.sort();
    out.extend(hits);
}

/// **undriven-rail**: a positive supply rail that parts *consume* but nothing *sources*.
/// A source is a `power:*` flag declaring the rail, a connector pin (external supply
/// entry), or a regulator that also touches a different rail (output of a converter).
/// A rail with consumers but no source is a wiring gap — the parts have no power.
///
/// FP-averse:
/// * **Gated** on the design actually using the `power:*` flag convention: if NO power
///   symbol appears anywhere, the input is treated as a fragment and the check stays
///   silent (a bare two-resistor divider snippet referencing `+5V` is not faulted).
/// * Only rails whose voltage parses unambiguously and is > 0 are considered; ground and
///   derived/oddly-named nodes are skipped.
/// * Three independent, structural source signals (power flag / connector / regulator),
///   so a rail sourced in *any* conventional way is never flagged. Every known-good
///   design declares each rail with a `power:*` symbol, so the check is silent on them.
/// * A regulator counts as a source for a rail only when it also touches a *second*
///   distinct rail (its input) — a regulator that merely consumes a rail isn't mistaken
///   for sourcing it.
///
/// Known limits: a rail sourced only by a transistor/ideal-switch with no parseable
/// second rail, or by an off-sheet supply with no on-sheet flag, may be missed.
fn check_undriven_rail(
    items: &[Item],
    net_items: &HashMap<&str, Vec<usize>>,
    out: &mut Vec<String>,
) {
    if !items.iter().any(|it| is_power_symbol(it.comp)) {
        return; // no power-flag convention in use → treat as a fragment, stay silent
    }
    let mut rails: Vec<&str> = net_items
        .keys()
        .copied()
        .filter(|n| rail_voltage(n).is_some_and(|v| v > 0.0))
        .collect();
    rails.sort();
    for rail in rails {
        let consumers = net_items.get(rail).map_or(0, |v| v.len());
        if consumers == 0 {
            continue;
        }
        let sourced = net_items.get(rail).into_iter().flatten().any(|&i| {
            let it = &items[i];
            let c = it.comp;
            is_power_symbol(c)
                || is_connector(c)
                || (is_regulator(c)
                    && it
                        .nets
                        .iter()
                        .any(|&n| n != rail && rail_voltage(n).is_some_and(|v| v > 0.0)))
        });
        if !sourced {
            out.push(format!(
                "- {rail}: rail is consumed by parts but nothing sources it — no supply/regulator \
                 output, connector, or power flag drives this net"
            ));
        }
    }
}

/// **output-short**: two or more parts whose OUTPUT pins are tied to the same net — a
/// driver conflict (two push-pull devices fighting to set the node). An output pin is
/// recognised by its author-written pin NAME reading as a dedicated output: a regulator
/// supply output (`VO`, `VOUT`) or a generic `OUT`-named pin.
///
/// FP-averse:
/// * Output detection is by an unambiguous output-pin-NAME vocabulary, NOT by part kind:
///   two regulators that merely share an *input* rail (a `VI`/`VIN` net) are never
///   flagged, because only their `VO` pins count.
/// * Open-drain / wired-or is explicitly excluded: any I2C-named net and any pin whose
///   name carries an open-collector/open-drain marker (`OD`, `OC`) is skipped — those are
///   *meant* to share a node.
/// * Ground and the supply rails (`rail_voltage` parses) are excluded — many power-output
///   pins legitimately tie to the same rail (that is how a rail is fed); a "short" there
///   is the wrong frame and is covered by [`check_undriven_rail`] instead.
/// * Distinct refdes only, so one part's two aliases of the same output pin don't
///   self-trigger.
///
/// Known limits: outputs authored by pin *number* (no name) can't be recognised without
/// pin electrical types — a numbered-pin output clash is missed. The check favours the
/// unambiguous named-output short (two regulator `VO`s / two `OUT`s on one node).
fn check_output_short(
    items: &[Item],
    net_items: &HashMap<&str, Vec<usize>>,
    out: &mut Vec<String>,
) {
    let is_output_name = |key: &str| {
        let k = key.trim_start_matches(['~', '{', '/']).to_uppercase();
        if k.contains("OD") || k.contains("OC") {
            return false; // open-drain / open-collector: wired-or is legal
        }
        k == "VO" || k.starts_with("VOUT") || k.starts_with("OUT")
    };
    let is_i2c_net = |net: &str| {
        let u = net.to_uppercase();
        u.contains("SDA") || u.contains("SCL") || u.contains("I2C")
    };
    let mut nets: Vec<&str> = net_items.keys().copied().collect();
    nets.sort();
    for net in nets {
        if rail_voltage(net).is_some() || is_i2c_net(net) {
            continue; // rails (incl. GND) and open-drain buses are legitimately multi-driver
        }
        let mut drivers: Vec<&str> = net_items[net]
            .iter()
            .map(|&i| &items[i])
            .filter(|it| it.pins.iter().any(|p| p.net == net && p.is(is_output_name)))
            .map(|it| it.refdes)
            .collect();
        drivers.sort();
        drivers.dedup();
        if drivers.len() >= 2 {
            out.push(format!(
                "- {net}: multiple outputs ({}) tie to this net — driver conflict; only one \
                 push-pull output may drive a node (use open-drain + pull-up for a shared bus)",
                drivers.join("/")
            ));
        }
    }
}
