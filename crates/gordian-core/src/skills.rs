//! Skill starters: the verified part list, pin map and ready-to-build design
//! JSON of a circuit close to what was asked for.
//!
//! A matching starter is what turns a Blue Pill into a one-shot — the model
//! adapts a design that already builds instead of composing one from a blank
//! sheet — so the block rides with the opening message, framed as a starting
//! point rather than as a reference.

/// How many starters ride with a prompt.
const TOP_K: usize = 2;

/// The instruction that frames the starters.
const FRAMING: &str = "If one of these skills covers the request, `build` ITS DESIGN JSON AS IT STANDS first - \
    that layout already scores 9 with the reviewer - and change only where the request differs from it. Every lib id, \
    pin key, footprint and net in a skill is verified against the installed libraries, so do not look those parts up \
    again with `search_symbols` or `symbol_info`, do not add parts the skill does not have, and copy its pin keys \
    character for character. Then fix only what the checks and the review report. Do not start from a blank sheet \
    when a skill covers the circuit.";

/// The context block appended to the opening user message, or empty when nothing
/// in the library matches.
pub fn prompt_block(prompt: &str) -> String {
    let block = gordian_skills::prompt_block(&gordian_skills::select(prompt, TOP_K));
    if block.is_empty() {
        String::new()
    } else {
        format!("{block}\n{FRAMING}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blue_pill_request_finds_its_starter() {
        let block = prompt_block("Design an STM32F103C8T6 'Blue Pill' development board");
        if block.is_empty() {
            return; // no skill library installed on this machine
        }
        assert!(block.contains("# Relevant design skills"));
        assert!(block.contains(FRAMING));
    }
}
