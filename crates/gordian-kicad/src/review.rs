//! Independent electrical-CORRECTNESS review of a committed netlist — the netlist analog of the
//! layout critic (`tools/schematic_critic.py`). The review MECHANICS (the diverse-lens ensemble,
//! verdict parsing, defect dedup) live in [`gordian_core::review`]; THIS module supplies the
//! domain: the netlist [`REVIEW_SYSTEM`] prompt and the diverse [`LENSES`], and unions in the
//! deterministic exact-math ERC. Run as a FRESH [`Provider::complete`] call (no conversation
//! history → unbiased; the generating model rationalises its own slips). Catches faults that pass
//! ERC and look clean: pin-function mis-wires, voltage-domain part-selection errors,
//! missing-essential-part and topology errors.

use anyhow::Result;
use gordian_core::Provider;

pub const REVIEW_SYSTEM: &str = r#"You are a senior electronics design engineer performing a NETLIST review (NOT a layout review).

You are given a circuit's intended function and its netlist in circuit-YAML (components with a refdes, a `part:` KiCAD lib_id, an optional `value:`, and pin->net maps; `between:[A,B]` = a 2-pin part across nets A,B; `positive/negative` = a polarized part; `power:NET` = a rail symbol; `label:global` = an exposed port).

Review ONLY for ELECTRICAL-DESIGN CORRECTNESS — faults a netlist can have while still passing ERC (connectivity) and looking clean:
1. PIN-FUNCTION mis-wires: a net wired to the WRONG pin for its function. Use your knowledge of the SPECIFIC part's pinout (e.g. SPI/ISP MISO/MOSI/SCK on the wrong MCU pin; a regulator FB pin not seeing the feedback divider; enable/boot/reset tied wrong).
2. WRONG VALUES: a resistor/cap value wrong by ~an order of magnitude for its role (1M I2C pull-up; 10nF "bulk" cap; a feedback divider whose ratio gives the wrong output voltage).
3. MISSING ESSENTIAL parts (cannot function without): crystal with no load caps; regulator with no output cap; an IC powered with NO decoupling at all.
4. VOLTAGE-DOMAIN / part-selection: a part operated outside its supply range (e.g. a 5V-only transceiver on a 3.3V rail).
5. TOPOLOGY errors: feedback/bias/reference wired wrong; reversed polarity; a missing return path.

Do NOT report layout, naming/style, nice-to-have protection, or anything you are not confident is a real electrical fault. A correct design SHOULD score 9-10 with few/no defects; do NOT invent defects.

REASON step by step FIRST (per IC, state its key pins from your knowledge of that exact part, then trace the critical nets), THEN emit, after a line `FINAL_JSON:`, a JSON object:
{"score": 0-10, "summary": "one line", "defects": [{"severity": "critical|major|minor", "confidence": "high|medium|low", "refdes": "U1", "issue": "short", "why": "the electrical reason"}]}"#;

/// Diverse review LENSES, unioned. A ground-truth recall sweep (tools/recall_harness.py, 25 injected
/// defects) showed repeated SAME-prompt sampling is flat (it can't recover a *consistent* miss), while
/// DIVERSE lenses each catch different fault classes and lift recall (80%→84%, and the clear-defect
/// rate to ~95%). Empty string = the general pass.
pub const LENSES: &[&str] = &[
    "",
    "power, regulation and analog faults: for EVERY resistor divider feeding a regulator feedback or \
     reference pin, COMPUTE the resulting output voltage from the resistor values and verify it matches \
     the intended rail; also bias/reference networks, voltage-domain part supply ranges, and \
     current-limit / gain resistor values",
    "digital interfaces and clocking: SPI/I2C/UART/ISP bus signals on the correct device pins, \
     crystal/oscillator pin placement, reset/boot/enable/chip-select straps, direction and address pins",
];

/// Review a netlist with the diverse-lens ENSEMBLE and return `(lowest score, union of
/// high-confidence critical/major defect lines)` — ready to feed back as a fix turn. Thin domain
/// wrapper over [`gordian_core::review`] with this module's [`REVIEW_SYSTEM`] + [`LENSES`].
pub async fn review_netlist(
    client: &dyn Provider,
    intent: &str,
    netlist: &str,
) -> Result<(f64, Vec<String>)> {
    gordian_core::review(client, REVIEW_SYSTEM, LENSES, intent, netlist).await
}

/// Two defect lines are "the same" if they target the same refdes — so a union (across lenses, or
/// with the deterministic ERC layer) doesn't feed the agent two phrasings of one fault. Re-exported
/// from [`gordian_core::review::same_defect`] so the ERC-union sites here read locally.
pub use gordian_core::review::same_defect;
