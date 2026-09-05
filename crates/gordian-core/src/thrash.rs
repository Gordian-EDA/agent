//! Breaks edit loops: the same kind of mutation re-applied to the same targets.
//!
//! Failing tools are already self-limiting — the model sees the error and moves
//! on. The pathology this guard exists for is the opposite: every call *succeeds*
//! and the model still undoes its own work, because the check it is chasing has
//! no repair (`fix: null`) or its suggested repair does not clear the finding.
//! Observed shapes, all with `status: ok`:
//!
//! - `label {net: "+3V3", pin: "U2.2"}` issued 28 times with identical arguments;
//! - `delete_wires` and `connect` alternating on `D1.1, D1.2, R6.2` nine times
//!   while `check_schematic` kept reporting `led-polarity at D1 → fix: null`;
//! - `remove_symbols {refs: ["U4", "C12", "C13"]}` three times, the last of which
//!   left the requested USB-serial bridge off the finished sheet.
//!
//! Keying on the tool name alone would miss the alternation and keying on the
//! full arguments would miss it too (the net names change every cycle). So the
//! key is the *edit family* (wiring, population) plus the exact set of refs and pins touched: the
//! delete and the connect that fight over one pin share a key, while wiring a
//! 48-pin MCU pin by pin does not.

use std::collections::HashMap;

use serde_json::{Value, json};

/// Applications of one key that pass before the guard starts refusing. Two
/// rounds of "edit, look, edit again" is ordinary repair; a third is a loop.
const LIMIT: usize = 3;

/// Argument keys whose string leaves name a symbol or a pin. `net` is
/// deliberately absent: the net is what an oscillation renames each cycle, and
/// collapsing it is what makes delete/connect pairs share a key.
const TARGET_KEYS: [&str; 9] = [
    "ref", "refs", "pin", "pins", "from", "to", "part", "parts", "uuids",
];

/// Per-subturn tally of edits by (family, targets).
#[derive(Default)]
pub(crate) struct ThrashGuard {
    seen: HashMap<(&'static str, Vec<String>), usize>,
}

impl ThrashGuard {
    /// Record one mutating call and, once its key reaches [`LIMIT`], return the
    /// intervention to hand back *instead of* running it.
    pub(crate) fn intervene(&mut self, tool: &str, args: &Value) -> Option<Value> {
        let family = edit_family(tool)?;
        let targets = targets_of(args);
        if targets.is_empty() {
            return None;
        }
        let count = self.seen.entry((family, targets.clone())).or_default();
        *count += 1;
        (*count >= LIMIT && refusable(tool)).then(|| intervention(tool, *count, &targets))
    }
}

/// Whether refusing this tool is safe. Refusing a call that puts parts back
/// would strand whatever the previous cycle deleted — exactly the wreck this
/// guard exists to prevent — so the additive tools always run. They still count
/// toward their key, which is what makes the *next* removal the refused one.
fn refusable(tool: &str) -> bool {
    !matches!(tool, "place_parts" | "add_parts" | "add_symbols")
}

/// The class of edit a mutating tool performs. Tools in one family fight over
/// the same state, so an alternation between them is the loop to catch.
fn edit_family(tool: &str) -> Option<&'static str> {
    Some(match tool {
        "connect" | "label" | "no_connect" | "add_power" | "delete_wires" | "delete_labels"
        | "rewire" => "wiring",
        "place_parts" | "add_parts" | "add_symbols" | "remove_symbols" | "remove_region" => {
            "population"
        }
        // Layout and field edits neither strand parts nor change connectivity,
        // and re-arranging one block after touching another is ordinary work.
        _ => return None,
    })
}

/// The sorted, deduplicated set of refs and pins named anywhere in `args`.
fn targets_of(args: &Value) -> Vec<String> {
    let mut targets = Vec::new();
    collect(args, false, &mut targets);
    targets.sort_unstable();
    targets.dedup();
    targets
}

/// Collect the string leaves reached through a [`TARGET_KEYS`] name. An object
/// re-dispatches on its own keys whatever it was reached through, so the
/// net-per-pin maps of `place_parts` (`pins: {"1": "+3V3"}`) contribute nothing.
/// Library ids are filtered out by their `Lib:Name` colon, which no refdes or
/// pin carries — that is what lets a `place_parts` restore and the
/// `remove_symbols` it undoes share one key.
fn collect(value: &Value, named: bool, out: &mut Vec<String>) {
    match value {
        Value::String(text) if named && !text.is_empty() && !text.contains(':') => {
            out.push(text.clone())
        }
        Value::Array(items) => items.iter().for_each(|item| collect(item, named, out)),
        Value::Object(fields) => {
            for (key, field) in fields {
                collect(field, TARGET_KEYS.contains(&key.as_str()), out);
            }
        }
        _ => {}
    }
}

fn intervention(tool: &str, count: usize, targets: &[String]) -> Value {
    json!({
        "error": "edit loop refused",
        "note": format!(
            "This turn has already edited {} {count} times and the sheet keeps returning to the \
             same state, so `{tool}` was NOT applied. Repeating it will keep being refused. \
             Change what those pins connect to, accept the current wiring and say what is wrong \
             with it, or move on to another part of the design.",
            targets.join(", ")
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_third_identical_edit_is_refused_and_the_first_two_are_not() {
        let mut guard = ThrashGuard::default();
        let args = json!({"net": "+3V3", "pin": "U2.2"});
        assert!(guard.intervene("label", &args).is_none());
        assert!(guard.intervene("label", &args).is_none());
        let refusal = guard.intervene("label", &args).expect("third is refused");
        assert_eq!(refusal["error"], "edit loop refused");
        assert!(refusal["note"].as_str().unwrap().contains("U2.2"));
    }

    /// The blue-pill loop: `delete_wires` and `connect` alternate over one pin
    /// set while only the net names change. They must share a key.
    #[test]
    fn delete_and_connect_on_the_same_pins_share_one_budget() {
        let mut guard = ThrashGuard::default();
        let pins = json!({"pins": ["D1.1", "D1.2", "R6.2"]});
        let wire = json!({"pairs": [
            {"from": "D1.1", "net": "GND"},
            {"from": "D1.2", "net": "PWR_LED_A"},
            {"from": "R6.2", "net": "PWR_LED_A"}
        ]});
        assert!(guard.intervene("delete_wires", &pins).is_none());
        assert!(guard.intervene("connect", &wire).is_none());
        assert!(guard.intervene("delete_wires", &pins).is_some());
    }

    #[test]
    fn different_pins_keep_their_own_budgets() {
        let mut guard = ThrashGuard::default();
        for pin in ["U2.1", "U2.2", "U2.3", "U2.4", "U2.5", "U2.6"] {
            let args = json!({"pin": pin, "net": "GND"});
            assert!(guard.intervene("label", &args).is_none(), "{pin}");
        }
    }

    /// `pins` is a pin list in `delete_wires` and a pin-to-net map in
    /// `place_parts`; only the former names targets.
    #[test]
    fn nets_and_library_ids_are_not_targets() {
        assert_eq!(
            targets_of(&json!({"parts": [
                {"ref": "R1", "part": "Device:R", "pins": {"1": "+3V3", "2": "GND"}}
            ]})),
            vec!["R1".to_string()]
        );
    }

    #[test]
    fn reads_and_targetless_calls_are_never_refused() {
        let mut guard = ThrashGuard::default();
        for _ in 0..8 {
            assert!(
                guard
                    .intervene("check_schematic", &json!({"detail": true}))
                    .is_none()
            );
            assert!(
                guard
                    .intervene(
                        "arrange",
                        &json!({"layout": {"MCU": {"row": [{"part": "U2"}]}}})
                    )
                    .is_none()
            );
        }
    }

    /// The arduino loop: `remove_symbols` and the `place_parts` that restores the
    /// same parts are one population fight, so the third removal never runs.
    #[test]
    fn removing_and_restoring_the_same_parts_is_one_loop() {
        let mut guard = ThrashGuard::default();
        let remove = json!({"refs": ["U4", "C12", "C13"]});
        let restore = json!({
            "block": "usb_serial_restore",
            "layout": {"usb_serial_restore": {"row": [
                {"part": "U4"}, {"part": "C12"}, {"part": "C13"}
            ]}}
        });
        assert!(guard.intervene("remove_symbols", &remove).is_none());
        assert!(guard.intervene("place_parts", &restore).is_none());
        assert!(
            guard.intervene("place_parts", &restore).is_none(),
            "a restore is never the refused call"
        );
        assert!(guard.intervene("remove_symbols", &remove).is_some());
    }
}
