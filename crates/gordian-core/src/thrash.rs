//! Breaks edit loops: the same mutation re-applied until the sheet is wrecked.
//!
//! Failing tools are already self-limiting — the model sees the error and moves
//! on. The pathology this guard exists for is the opposite: every call *succeeds*
//! and the model still undoes its own work, because the check it is chasing has
//! no repair (`fix: null`) or its suggested repair does not clear the finding.
//! Three loops, all with `status: ok`, each caught by its own rule here:
//!
//! - **wiring** — `label {net: "+3V3", pin: "U2.2"}` issued 28 times with
//!   identical arguments; `delete_wires` and `connect` alternating on
//!   `D1.1, D1.2, R6.2` nine times while the checker kept reporting
//!   `led-polarity at D1 → fix: null`. Keyed by the exact set of pins touched,
//!   with the net names collapsed away — that is what makes the delete and the
//!   connect that undoes it share a budget, while wiring a 48-pin MCU pin by pin
//!   does not.
//! - **existence** — `remove_symbols {refs: ["D2"]}`, `place_parts` putting D2
//!   back, `remove_symbols {refs: ["D2"]}` again. The re-add names a whole block
//!   (`R4, D2`) so an exact-set key misses it; counting *presence flips per ref*
//!   does not. The initial placement seeds presence and is not a flip.
//! - **furniture purge** — `#FLG1`, then four `#PWR_GND_*`, then five more
//!   `#PWR_*`, then `#PWR_GND_4`: each removal names a fresh set, so every
//!   per-set key was fresh and the whole rail furniture of the sheet went. The
//!   `#`-prefixed symbols KiCAD generates share one budget regardless of which
//!   ones are named.

use std::collections::{HashMap, HashSet};

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

#[derive(Default)]
pub(crate) struct ThrashGuard {
    wiring: HashMap<Vec<String>, usize>,
    present: HashSet<String>,
    flips: HashMap<String, usize>,
    furniture_removals: usize,
}

impl ThrashGuard {
    /// Record one mutating call and, when it is the third turn of a loop, return
    /// the intervention to hand back *instead of* running it.
    pub(crate) fn intervene(&mut self, tool: &str, args: &Value) -> Option<Value> {
        let targets = targets_of(args);
        if targets.is_empty() {
            return None;
        }
        match family(tool)? {
            Family::Wiring => self.wiring(tool, targets),
            Family::Add => self.population(tool, targets, true),
            Family::Remove => self.population(tool, targets, false),
        }
    }

    fn wiring(&mut self, tool: &str, targets: Vec<String>) -> Option<Value> {
        let count = self.wiring.entry(targets.clone()).or_default();
        *count += 1;
        (*count >= LIMIT).then(|| {
            intervention(
                tool,
                format!("has edited {} {count} times", targets.join(", ")),
                "Change what those pins connect to, accept the current wiring and say what is \
                 wrong with it, or move on to another part of the design.",
            )
        })
    }

    /// Population edits are budgeted per ref, by how often the ref's presence on
    /// the sheet has been flipped. A ref seen for the first time only seeds its
    /// state, so building the sheet costs nothing; deleting and restoring one
    /// part costs two. Power furniture shares a single budget because KiCAD
    /// mints a fresh reference for every flag and rail symbol, which would give
    /// each round of a purge a key of its own.
    fn population(&mut self, tool: &str, targets: Vec<String>, adding: bool) -> Option<Value> {
        if !adding && targets.iter().any(|target| is_furniture(target)) {
            self.furniture_removals += 1;
            if self.furniture_removals >= LIMIT {
                return Some(intervention(
                    tool,
                    format!(
                        "has removed power symbols and flags {} times",
                        self.furniture_removals
                    ),
                    "The rails are furniture, not the defect. Leave them and report the rule \
                     you cannot satisfy.",
                ));
            }
        }
        let mut worst = 0;
        let mut flipped = Vec::new();
        for target in targets.iter().filter(|target| !is_furniture(target)) {
            let known = self.flips.contains_key(target);
            let flips = self.flips.entry(target.clone()).or_default();
            let changed = known && self.present.contains(target) != adding;
            if adding {
                self.present.insert(target.clone());
            } else {
                self.present.remove(target);
            }
            if !changed {
                continue;
            }
            *flips += 1;
            worst = worst.max(*flips);
            flipped.push(target.clone());
        }
        // A restore always runs: refusing it would strand whatever the previous
        // removal took off the sheet, which is the wreck this guard prevents.
        (worst >= LIMIT && !adding).then(|| {
            intervention(
                tool,
                format!("has added and removed {} {worst} times", flipped.join(", ")),
                "The part belongs to the request. Keep it, wire it as best you can, and report \
                 what is still wrong instead of deleting it.",
            )
        })
    }
}

/// KiCAD mints `#PWR*` rail symbols and `#FLG*` power flags itself, with a fresh
/// reference each time, so their names never repeat across a purge.
fn is_furniture(target: &str) -> bool {
    target.starts_with('#')
}

enum Family {
    Wiring,
    Add,
    Remove,
}

fn family(tool: &str) -> Option<Family> {
    Some(match tool {
        "connect" | "no_connect" | "delete_wires" => Family::Wiring,
        "place_parts" => Family::Add,
        "remove_symbols" => Family::Remove,
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
/// pin carries.
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

fn intervention(tool: &str, loop_description: String, demand: &str) -> Value {
    json!({
        "error": "edit loop refused",
        "note": format!(
            "This turn {loop_description} and the sheet keeps returning to the same state, so \
             `{tool}` was NOT applied. Repeating it will keep being refused. {demand}"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(refs: &[&str]) -> Value {
        json!({"parts": refs.iter().map(|r| json!({"ref": r})).collect::<Vec<_>>()})
    }

    #[test]
    fn a_third_identical_edit_is_refused_and_the_first_two_are_not() {
        let mut guard = ThrashGuard::default();
        let args = json!({"net": "+3V3", "from": "U2.2"});
        assert!(guard.intervene("connect", &args).is_none());
        assert!(guard.intervene("connect", &args).is_none());
        let refusal = guard.intervene("connect", &args).expect("third is refused");
        assert_eq!(refusal["error"], "edit loop refused");
        assert!(refusal["note"].as_str().unwrap().contains("U2.2"));
    }

    /// The blue-pill wiring loop: `delete_wires` and `connect` alternate over one
    /// pin set while only the net names change. They must share a key.
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
            let args = json!({"from": pin, "net": "GND"});
            assert!(guard.intervene("connect", &args).is_none(), "{pin}");
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

    /// v4 blue-pill #45/#46/#48: the re-add names the whole `LEDS` block, so its
    /// argument set never matches the removal's. Presence flips per ref do.
    #[test]
    fn a_removal_and_a_block_shaped_restore_share_one_budget() {
        let mut guard = ThrashGuard::default();
        assert!(
            guard
                .intervene("place_parts", &place(&["R4", "D2"]))
                .is_none()
        );
        assert!(
            guard
                .intervene("remove_symbols", &json!({"refs": ["D2"]}))
                .is_none()
        );
        assert!(
            guard
                .intervene("place_parts", &place(&["R4", "D2"]))
                .is_none()
        );
        let refusal = guard
            .intervene("remove_symbols", &json!({"refs": ["D2"]}))
            .expect("the second removal of a restored part is refused");
        assert!(refusal["note"].as_str().unwrap().contains("D2"));
    }

    /// Building a sheet block by block, and re-placing a block that is already
    /// there, must cost nothing.
    #[test]
    fn placing_the_same_block_repeatedly_is_never_a_loop() {
        let mut guard = ThrashGuard::default();
        for _ in 0..6 {
            assert!(
                guard
                    .intervene("place_parts", &place(&["U2", "C4", "C5", "R1"]))
                    .is_none()
            );
        }
        assert!(
            guard
                .intervene("remove_symbols", &json!({"refs": ["C4"]}))
                .is_none(),
            "a first removal is still allowed after any number of placements"
        );
    }

    /// v4 blue-pill #64/#65/#70/#100/#117/#125: every purge named a fresh set of
    /// KiCAD-minted references, so all of them shared no key at all.
    #[test]
    fn power_furniture_shares_one_budget_whatever_it_is_called() {
        let mut guard = ThrashGuard::default();
        assert!(
            guard
                .intervene("remove_symbols", &json!({"refs": ["#FLG1"]}))
                .is_none()
        );
        assert!(
            guard
                .intervene(
                    "remove_symbols",
                    &json!({"refs": ["#PWR_GND_0_4", "#PWR_GND_0_5", "#PWR_GND_0_2"]})
                )
                .is_none()
        );
        let refusal = guard
            .intervene("remove_symbols", &json!({"refs": ["#PWR_+5V_2"]}))
            .expect("the third purge is refused");
        assert!(
            refusal["note"]
                .as_str()
                .unwrap()
                .contains("power symbols and flags")
        );
    }

    /// Attaching rails is how a sheet gets built; only removing them is a purge.
    #[test]
    fn adding_power_furniture_is_never_refused() {
        let mut guard = ThrashGuard::default();
        for pin in ["U1.1", "U2.8", "U3.4", "C1.2", "C2.2", "R1.1"] {
            assert!(
                guard
                    .intervene("connect", &json!({"net": "GND", "from": pin}))
                    .is_none(),
                "{pin}"
            );
        }
    }
}
