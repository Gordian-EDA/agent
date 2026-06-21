//! Deterministic, quantitative electrical checks — the exact-math layer UNDER the LLM review
//! ensemble (`agent::review`). The reviewer is strong on judgment but weak on arithmetic (it
//! consistently missed a feedback-divider value error in the recall harness); these checks compute
//! the numbers exactly where the netlist makes them unambiguous, so the two layers are complementary:
//! deterministic where math is exact, LLM lenses where judgment is needed.
//!
//! FP-averse: every check only fires when the relevant values/voltages parse unambiguously AND the
//! result is clearly out of range — a false deterministic defect would trigger a needless fix turn.

use crate::model::*;
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
        ('p', 1e-12), ('n', 1e-9), ('u', 1e-6), ('µ', 1e-6), ('m', 1e-3),
        ('R', 1.0), ('r', 1.0), ('k', 1e3), ('K', 1e3), ('M', 1e6), ('G', 1e9),
    ];
    for &(c, scale) in SCALES {
        if let Some(idx) = s.find(c) {
            let (a, b) = (&s[..idx], &s[idx + c.len_utf8()..]);
            let b = b.trim();
            let num = if b.is_empty() { a.to_string() } else { format!("{a}.{b}") };
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
    if n == "GND" || n.starts_with("GND") || n == "VSS" || n.starts_with("AGND") || n.starts_with("DGND") {
        return Some(0.0);
    }
    // d.dV  e.g. 3.3V
    if let Some(p) = n.strip_suffix('V') {
        if let Ok(v) = p.parse::<f64>() {
            return Some(v);
        }
        // dVd  e.g. 3V3, 1V8 — the V is the decimal point
    }
    if let Some(vpos) = n.find('V') {
        let (a, b) = (&n[..vpos], &n[vpos + 1..]);
        if !a.is_empty() && a.chars().all(|c| c.is_ascii_digit()) && b.chars().all(|c| c.is_ascii_digit()) {
            let s = if b.is_empty() { a.to_string() } else { format!("{a}.{b}") };
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
}

fn nets_of(c: &Component) -> Vec<&str> {
    let mut v = Vec::new();
    for t in c.pins.values().chain(c.units.values().flatten().map(|(_, t)| t)) {
        if let PinTarget::Net(n) = t {
            v.push(n.as_str());
        }
    }
    v
}

/// The net at the *other* end of a 2-net part from `net`.
fn far<'a>(r: &Item<'a>, net: &str) -> &'a str {
    if r.nets[0] == net { r.nets[1] } else { r.nets[0] }
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

/// Run all deterministic quantitative checks, returning defect lines (same `- REFDES: ...` shape the
/// LLM review emits, so the agent's run_turn_reviewed can union them).
pub fn erc_checks(d: &Design) -> Vec<String> {
    let items: Vec<Item> = d
        .blocks
        .values()
        .flat_map(|b| b.components.iter())
        .filter(|(_, c)| !c.dnp)
        .map(|(rd, c)| Item { refdes: rd.as_str(), comp: c, nets: nets_of(c) })
        .collect();
    // net -> indices into items
    let mut net_items: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        for &n in &it.nets {
            net_items.entry(n).or_default().push(i);
        }
    }
    let mut out = Vec::new();
    check_led_current(&items, &net_items, &mut out);
    check_fb_divider(&items, &net_items, &mut out);
    check_dangling(&items, &mut out);
    check_crystal(&items, &net_items, &mut out);
    out
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
                let r_far = if r.nets[0] == junction { r.nets[1] } else { r.nets[0] };
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
        let is_fb = up.ends_with("FB") || up.ends_with("_FB") || up.contains("FEEDBACK") || up.contains("VSENSE");
        if !is_fb {
            continue;
        }
        let rs: Vec<&Item> = idxs.iter().map(|&i| &items[i]).filter(|it| is_resistor(it.comp) && it.nets.len() == 2).collect();
        if rs.len() != 2 {
            continue;
        }
        // classify: one R goes to GND (Rbot), the other to an output rail (Rtop).
        let (mut rtop, mut rbot, mut target) = (None, None, None);
        for r in &rs {
            let f = far(r, net);
            match rail_voltage(f) {
                Some(v) if v == 0.0 => rbot = r.comp.value.as_deref().and_then(parse_value),
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
        let ok = VREFS.iter().any(|&vr| ((vout(vr) - tgt) / tgt).abs() < 0.20);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desugar::desugar;
    use crate::parse::parse_str;
    use crate::provider::MockSymbolProvider;

    fn design(src: &str) -> Design {
        let p = MockSymbolProvider::with_basics();
        let (s, _) = parse_str(src);
        let (d, _) = desugar(&s.unwrap(), &p);
        d
    }

    #[test]
    fn value_parser() {
        let approx = |a: Option<f64>, b: f64| a.is_some_and(|x| (x - b).abs() <= b.abs() * 1e-9);
        assert!(approx(parse_value("10k"), 10_000.0));
        assert!(approx(parse_value("1.5k"), 1500.0));
        assert!(approx(parse_value("330R"), 330.0));
        assert!(approx(parse_value("4R7"), 4.7));
        assert!(approx(parse_value("22pF"), 22e-12));
        assert!(approx(parse_value("100nF"), 100e-9));
        assert!(approx(parse_value("4.7uF"), 4.7e-6));
        assert!(approx(parse_value("2M2"), 2.2e6));
        assert!(approx(parse_value("100"), 100.0));
        assert_eq!(parse_value("notavalue"), None);
    }

    #[test]
    fn rail_voltage_parser() {
        assert_eq!(rail_voltage("3V3"), Some(3.3));
        assert_eq!(rail_voltage("+5V"), Some(5.0));
        assert_eq!(rail_voltage("1V8"), Some(1.8));
        assert_eq!(rail_voltage("12V"), Some(12.0));
        assert_eq!(rail_voltage("3.3V"), Some(3.3));
        assert_eq!(rail_voltage("GND"), Some(0.0));
        assert_eq!(rail_voltage("V12"), Some(1.2)); // SoC core-rail convention
        assert_eq!(rail_voltage("V33"), Some(3.3));
        assert_eq!(rail_voltage("VOUT"), None); // ambiguous → skip
        assert_eq!(rail_voltage("VCC"), None);
        assert_eq!(rail_voltage("V5"), None); // single digit → ambiguous
    }

    #[test]
    fn wrong_v12_divider_flagged() {
        // The recall harness's proven LLM miss: a 1.2 V (V12) core rail whose FB divider is 100x off.
        let d = design("
version: 1
blocks:
  main:
    components:
      R5: {part: Device:R, value: 150k, pins: {1: V12, 2: V12_FB}}
      R6: {part: Device:R, value: 10k, pins: {1: V12_FB, 2: GND}}
");
        assert!(erc_checks(&d).iter().any(|s| s.contains("feedback-divider")), "{:?}", erc_checks(&d));
    }

    #[test]
    fn led_overcurrent_flagged() {
        // 5V rail, 22R series, red LED → (5-1.8)/22 ≈ 145 mA: excessive.
        let d = design("
version: 1
blocks:
  main:
    components:
      D1: {part: Device:LED, pins: {1: LED_A, 2: GND}}
      R1: {part: Device:R, value: 22R, pins: {1: '+5V', 2: LED_A}}
");
        let out = erc_checks(&d);
        assert!(out.iter().any(|s| s.contains("D1") && s.contains("excessive")), "{out:?}");
    }

    #[test]
    fn sane_led_not_flagged() {
        // 5V, 330R → ~10 mA: fine.
        let d = design("
version: 1
blocks:
  main:
    components:
      D1: {part: Device:LED, pins: {1: LED_A, 2: GND}}
      R1: {part: Device:R, value: 330R, pins: {1: '+5V', 2: LED_A}}
");
        assert!(erc_checks(&d).is_empty(), "{:?}", erc_checks(&d));
    }

    #[test]
    fn wrong_fb_divider_flagged() {
        // 3V3 rail; correct ~ Rtop 22k / Rbot 10k with Vref 0.8 → 0.8*(1+2.2)=2.56 (not 3.3, but
        // within none?) — use a clearly-wrong ratio: Rtop 220k / Rbot 10k → 0.8*23=18 V, way off 3.3.
        let d = design("
version: 1
blocks:
  main:
    components:
      U1: {part: Device:R, value: 220k, pins: {1: '3V3', 2: VFB}}
      R2: {part: Device:R, value: 10k, pins: {1: VFB, 2: GND}}
");
        let out = erc_checks(&d);
        assert!(out.iter().any(|s| s.contains("feedback-divider")), "{out:?}");
    }

    #[test]
    fn correct_fb_divider_not_flagged() {
        // 3V3, Rtop 31.6k / Rbot 10k, Vref 0.8 → 0.8*(1+3.16)=3.33 ≈ 3.3 ✓
        let d = design("
version: 1
blocks:
  main:
    components:
      R1: {part: Device:R, value: 31.6k, pins: {1: '3V3', 2: VFB}}
      R2: {part: Device:R, value: 10k, pins: {1: VFB, 2: GND}}
");
        assert!(erc_checks(&d).is_empty(), "{:?}", erc_checks(&d));
    }

    #[test]
    fn dangling_pin_flagged() {
        let d = design("
version: 1
blocks:
  main:
    components:
      C1: {part: Device:C, value: 100nF, pins: {1: SIG, 2: nc}}
");
        assert!(erc_checks(&d).iter().any(|s| s.contains("C1") && s.contains("unconnected")), "{:?}", erc_checks(&d));
    }

    #[test]
    fn shorted_part_flagged() {
        let d = design("
version: 1
blocks:
  main:
    components:
      R1: {part: Device:R, value: 10k, pins: {1: A, 2: A}}
");
        assert!(erc_checks(&d).iter().any(|s| s.contains("R1") && s.contains("shorted")), "{:?}", erc_checks(&d));
    }

    #[test]
    fn crystal_without_load_caps_flagged() {
        let d = design("
version: 1
blocks:
  main:
    components:
      Y1: {part: Device:Crystal, value: 8MHz, pins: {1: OSC1, 2: OSC2}}
");
        assert!(erc_checks(&d).iter().any(|s| s.contains("Y1") && s.contains("load cap")), "{:?}", erc_checks(&d));
    }

    #[test]
    fn crystal_with_load_caps_ok() {
        let d = design("
version: 1
blocks:
  main:
    components:
      Y1: {part: Device:Crystal, value: 8MHz, pins: {1: OSC1, 2: OSC2}}
      C1: {part: Device:C, value: 22pF, pins: {1: OSC1, 2: GND}}
      C2: {part: Device:C, value: 22pF, pins: {1: OSC2, 2: GND}}
");
        assert!(erc_checks(&d).is_empty(), "{:?}", erc_checks(&d));
    }
}
