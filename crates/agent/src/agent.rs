//! The agent loop with a human apply-gate.
//!
//! [`Agent::run_turn`] drives one user turn: it builds the system prompt (the
//! circuit-YAML language spec + workflow doctrine + real-library guidance), then
//! repeatedly calls the [`LlmClient`], executing each tool the model requests and
//! feeding the structured result back, until the model returns a final text (or a
//! safety iteration cap is hit).
//!
//! ## Design state is tool-pulled
//!
//! The whole design is **never** dumped into the system prompt. The model fetches
//! it on demand via `get_design`, keeps context small, and self-repairs off the
//! structured diagnostics that the tools return.
//!
//! ## The apply-gate (dry-run → approve → commit)
//!
//! `apply_design` is the only write. When the model asks to commit
//! (`commit: true`), the loop does **not** write immediately. It:
//!
//! 1. Re-runs `apply_design` as a **dry-run** (`commit: false`) to obtain the
//!    structured diff without touching disk.
//! 2. Hands that diff to [`Approvals::approve`].
//! 3. On approval, re-runs `apply_design` with `commit: true` (the real write +
//!    snapshot + ERC) and marks the turn `applied`.
//! 4. On rejection, feeds a "user rejected" tool result back to the model so it
//!    can react, and writes nothing.
//!
//! [`AutoApprove`] is the headless test/automation implementation; the TUI
//! supplies an interactive one later.

use anyhow::Result;
use serde_json::{Value, json};

use crate::llm::{ContentBlock, LlmClient, Message, Role};
use crate::tools::{ToolCtx, Tools};

/// Safety cap on LLM round-trips per turn. Generous enough for
/// search → info → validate → apply self-repair, bounded so a misbehaving model
/// can't loop forever.
const MAX_ITERATIONS: usize = 12;

/// The human apply-gate. The loop calls [`Approvals::approve`] with the dry-run
/// diff before any `apply_design` write; returning `false` cancels the write.
pub trait Approvals {
    /// Decide whether to commit the proposed change, given the dry-run diff
    /// (the `apply_design` dry-run JSON: `{ok, would_write, diff, rendered_len}`).
    fn approve(&mut self, diff: &Value) -> bool;
}

/// A non-interactive [`Approvals`] that always answers the same way. Used by
/// tests and headless automation (`autopcb agent`).
pub struct AutoApprove {
    answer: bool,
}

impl AutoApprove {
    /// Always approve.
    pub fn yes() -> Self {
        Self { answer: true }
    }

    /// Always reject.
    pub fn no() -> Self {
        Self { answer: false }
    }
}

impl Approvals for AutoApprove {
    fn approve(&mut self, _diff: &Value) -> bool {
        self.answer
    }
}

/// The result of one [`Agent::run_turn`].
#[derive(Clone, Debug)]
pub struct TurnOutcome {
    /// Whether an approved `apply_design` write actually committed this turn.
    pub applied: bool,
    /// The model's final text reply.
    pub final_text: String,
    /// How many tool calls the loop executed (counts each model-requested call;
    /// the dry-run probe before an approved commit is internal and not counted).
    pub tool_calls_made: usize,
}

/// An agent session over one project. Holds the LLM client, the tool context
/// (KiCAD env + project paths + symbol provider + snapshot store), and the tool
/// registry.
pub struct Agent {
    client: Box<dyn LlmClient>,
    ctx: ToolCtx,
    tools: Tools,
}

impl Agent {
    /// Build an agent over a project's [`ToolCtx`].
    pub fn new(client: Box<dyn LlmClient>, ctx: ToolCtx) -> Self {
        Self {
            client,
            ctx,
            tools: Tools::new(),
        }
    }

    /// The tool context (project paths, KiCAD env). Exposed for callers that
    /// want to inspect the `.kicad_sch` path after a turn.
    pub fn ctx(&self) -> &ToolCtx {
        &self.ctx
    }

    /// Drive one user turn to completion.
    ///
    /// Loops: call the model → run any requested tools (gating `apply_design`
    /// commits through `approvals`) → feed results back → repeat, until the model
    /// returns a final text with no pending tool calls, or [`MAX_ITERATIONS`] is
    /// reached.
    pub async fn run_turn(
        &mut self,
        user_msg: &str,
        approvals: &mut dyn Approvals,
    ) -> Result<TurnOutcome> {
        let system = system_prompt();
        let defs = self.tools.defs();

        let mut messages: Vec<Message> = vec![Message::user(user_msg)];
        let mut applied = false;
        let mut tool_calls_made = 0usize;
        let mut final_text = String::new();

        for _ in 0..MAX_ITERATIONS {
            let completion = self.client.complete(&system, &messages, &defs).await?;

            // Record the assistant turn (text + any tool_use blocks) verbatim so
            // the next request carries a faithful transcript.
            let mut assistant_blocks: Vec<ContentBlock> = Vec::new();
            if !completion.text.is_empty() {
                assistant_blocks.push(ContentBlock::Text(completion.text.clone()));
            }
            for call in &completion.tool_calls {
                assistant_blocks.push(ContentBlock::ToolUse {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                });
            }
            messages.push(Message {
                role: Role::Assistant,
                content: assistant_blocks,
            });

            // No tool calls → the model is done; return its text.
            if completion.tool_calls.is_empty() {
                final_text = completion.text;
                return Ok(TurnOutcome {
                    applied,
                    final_text,
                    tool_calls_made,
                });
            }

            // Run every requested tool and collect the results into one user
            // message (Converse requires all tool results in a single turn).
            let mut result_blocks: Vec<ContentBlock> = Vec::new();
            for call in &completion.tool_calls {
                tool_calls_made += 1;
                let content = self.run_tool_call(call, approvals, &mut applied);
                result_blocks.push(ContentBlock::ToolResult {
                    tool_use_id: call.id.clone(),
                    content,
                });
            }
            messages.push(Message {
                role: Role::User,
                content: result_blocks,
            });

            // Carry any text the model emitted alongside its tool calls so a turn
            // that ends without a trailing text-only completion still has a reply.
            if !completion.text.is_empty() {
                final_text = completion.text;
            }
        }

        // Hit the iteration cap without a clean finish.
        if final_text.is_empty() {
            final_text = "(agent reached its iteration limit without a final answer)".to_string();
        }
        Ok(TurnOutcome {
            applied,
            final_text,
            tool_calls_made,
        })
    }

    /// Execute one tool call, returning the JSON-stringified result to feed back
    /// to the model. `apply_design` commits are routed through the apply-gate;
    /// every other tool runs directly. Tool errors are surfaced as a structured
    /// `{error: ...}` result (not propagated) so the model can self-repair.
    fn run_tool_call(
        &self,
        call: &crate::llm::ToolCall,
        approvals: &mut dyn Approvals,
        applied: &mut bool,
    ) -> String {
        let result = if call.name == "apply_design" && wants_commit(&call.input) {
            self.gated_apply(&call.input, approvals, applied)
        } else {
            self.tools.run(&call.name, call.input.clone(), &self.ctx)
        };

        match result {
            Ok(value) => value.to_string(),
            Err(e) => json!({ "error": e.to_string() }).to_string(),
        }
    }

    /// The apply-gate: dry-run to get the diff, ask for approval, and only then
    /// commit. On rejection nothing is written and the model is told.
    fn gated_apply(
        &self,
        input: &Value,
        approvals: &mut dyn Approvals,
        applied: &mut bool,
    ) -> Result<Value> {
        // 1. Dry-run (commit:false) to get the diff WITHOUT writing.
        let mut dry_input = input.clone();
        dry_input["commit"] = json!(false);
        let dry = self.tools.run("apply_design", dry_input, &self.ctx)?;

        // If the YAML doesn't even compile, there is nothing to approve — return
        // the diagnostics straight back so the model self-repairs.
        if dry.get("ok").and_then(Value::as_bool) != Some(true) {
            return Ok(dry);
        }

        // 2. Human apply-gate on the dry-run diff.
        if !approvals.approve(&dry) {
            return Ok(json!({
                "ok": true,
                "written": false,
                "rejected": true,
                "note": "user rejected the proposed change; nothing was written",
            }));
        }

        // 3. Approved → commit (writes + snapshots + ERC).
        let mut commit_input = input.clone();
        commit_input["commit"] = json!(true);
        let committed = self.tools.run("apply_design", commit_input, &self.ctx)?;
        if committed.get("written").and_then(Value::as_bool) == Some(true) {
            *applied = true;
        }
        Ok(committed)
    }
}

/// Whether an `apply_design` input intends to write (`commit: true`).
fn wants_commit(input: &Value) -> bool {
    input.get("commit").and_then(Value::as_bool) == Some(true)
}

/// The system prompt: the circuit-YAML language spec (kernel + sugar), the
/// workflow doctrine, and the real-library guidance from validation.
///
/// Kept as a single embedded string (no design state) — the model pulls the
/// design on demand via `get_design`.
fn system_prompt() -> String {
    SYSTEM_PROMPT.to_string()
}

const SYSTEM_PROMPT: &str = r#"You are an expert KiCAD schematic copilot. You design and edit electronic
schematics by emitting a small declarative YAML language ("circuit-YAML") and
driving a fixed set of tools. You never hand-edit the .kicad_sch directly; the
tools compile your YAML to a real KiCAD schematic.

# circuit-YAML language

A design is ONE YAML document with this shape:

  version: 1                 # required, always 1
  name: my_board             # optional design name
  rails: [3V3, GND, VBUS]    # optional: declare power/ground nets (see Sugar)
  blocks:                    # required: a partition of all components
    main:                    # block name, lower_snake_case
      components:
        R1: { ... }          # refdes -> component
      layout: { edge: top }  # optional placement hint (top/bottom/left/right)
  nets:                      # optional: per-net class hints (rarely needed)
    I2C1_SDA: { class: signal }

## Components (the kernel)

Each component is keyed by its refdes and has:

  U1:
    part: MCU_ST_STM32H7:STM32H743VITx   # REQUIRED: full KiCAD lib_id "Lib:Name"
    value: 10k                           # optional component value
    footprint: Package_QFP:LQFP-100      # optional
    dnp: true                            # optional do-not-populate flag
    pins:                                # map pin -> net name (or `nc`)
      VDD: 3V3
      VSS: GND
      PA0: USB_DM
      "48": VCAP1                        # pin NUMBER as a quoted key (see below)

- `part:` MUST be a real, fully-qualified lib_id like `Device:R` or
  `MCU_ST_STM32H7:STM32H743VITx`. Find it with `search_symbols` first — never
  guess or invent a lib_id. The five short aliases `R`, `C`, `L`, `D`, `LED`
  expand to `Device:R`/`Device:C`/`Device:L`/`Device:D`/`Device:LED`; everything
  else must be a real `Lib:Name`.
- `pins:` maps a pin KEY to a net name. The key may be the pin's NAME (e.g. `VDD`,
  `PA0`) or, when names are ambiguous or stacked (multiple pins share a name like
  the STM32 `VCAP`/`VSS`), the pin NUMBER as a quoted string (e.g. `"48"`).
  Prefer numbers when a name is not unique. Use `get_symbol_info` to read the
  real pin names/numbers/types for a part.
- The reserved net `nc` (case-insensitive) places a no-connect on a pin. You do
  NOT need to list every pin: any unmentioned pin is auto-no-connected — EXCEPT
  power-INPUT pins, which MUST be connected to a net or compilation fails loudly.
  So always wire VDD/VSS/VDDA/etc.

## Naming rules (hard unless noted)

- refdes: strictly `[A-Z]+[0-9]+` — uppercase letters then digits, e.g. `R1`,
  `U2`, `J1`. Use PLAIN refdes; do NOT use descriptive names like `C_VCAP1` or
  `R_PULLUP` (they are rejected). Just `C1`, `R3`, etc.
- net names: UPPER_SNAKE, no spaces, `/` reserved. (A lowercase letter is only a
  warning, but prefer UPPER_SNAKE.)
- block names: lower_snake_case.
- Every refdes is globally unique across all blocks. Blocks are grouping +
  placement only — no electrical meaning.

## Sugar (shorthands the compiler expands)

- `rails: [3V3, GND]` — declares these nets as power/ground rails. Use it so the
  schematic gets proper power symbols.
- `between: [NET_A, NET_B]` — for a 2-pin part, wires its two pins to these nets
  in pin-number order. Replaces an explicit `pins:` map:
      R1: { part: R, value: 10k, between: [VBUS, GND] }
  Note: on POLARIZED parts (D/LED/CP) `between` warns about orientation.
- `decouple: { 100nF: 10, 4.7uF: 2 }` — on an IC, synthesizes that many
  decoupling caps of each value across the IC's power/ground. The caps are
  generated for you; never list them individually.

# Tools and workflow (follow this order)

1. `get_design()` — lift the CURRENT schematic back to circuit-YAML. ALWAYS call
   this first when editing an existing design so you build on it (don't recreate
   from scratch and don't clobber the user's work).
2. `search_symbols(query)` — find the real `Lib:Name` lib_id for any part BEFORE
   you reference it. KiCAD 10 renamed many symbols (e.g.
   `USB_C_Receptacle_USB2.0` is now `USB_C_Receptacle_USB2.0_16P`), so do not
   trust remembered names — search.
3. `get_symbol_info(lib_id)` — read a part's real pin table (number, name,
   electrical type, unit) so you wire the right pins, especially for stacked
   power pins where you must key by number.
4. `validate_design(yaml)` — compile your YAML WITHOUT writing. Read the
   diagnostics and self-repair until it reports `ok: true` and 0 errors.
5. `apply_design(yaml, commit:false)` — preview: returns the structured diff
   (added/removed/changed refdes, net delta) WITHOUT writing. Inspect it.
6. `apply_design(yaml, commit:true)` — propose the WRITE. A human must approve
   the diff before it lands; on approval it writes the .kicad_sch, snapshots the
   prior, and runs ERC, returning the ERC counts. On rejection nothing is written
   — explain or revise.
7. `run_erc()` — re-run KiCAD's Electrical Rules Check on the current schematic.

Doctrine: search before you reference a part; read pins with get_symbol_info;
validate before you apply; preview (commit:false) before you commit (commit:true).
Aim for ERC-clean designs. When you are finished, reply with a short plain-text
summary of what you did — no tool call.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_approve_yes_and_no() {
        let diff = json!({ "diff": { "added": ["R1"] } });
        assert!(AutoApprove::yes().approve(&diff));
        assert!(!AutoApprove::no().approve(&diff));
    }

    #[test]
    fn wants_commit_detects_true_only() {
        assert!(wants_commit(&json!({ "yaml": "x", "commit": true })));
        assert!(!wants_commit(&json!({ "yaml": "x", "commit": false })));
        assert!(!wants_commit(&json!({ "yaml": "x" })));
    }

    #[test]
    fn system_prompt_covers_kernel_sugar_and_workflow() {
        let p = system_prompt();
        // Kernel + naming rules.
        assert!(p.contains("part:"));
        assert!(p.contains("[A-Z]+[0-9]+"));
        assert!(p.contains("power-INPUT"));
        // Sugar forms.
        assert!(p.contains("rails:"));
        assert!(p.contains("between:"));
        assert!(p.contains("decouple:"));
        // Workflow doctrine + real-lib guidance.
        assert!(p.contains("search_symbols"));
        assert!(p.contains("validate_design"));
        assert!(p.contains("apply_design"));
        assert!(p.contains("commit:false"));
        assert!(p.contains("C_VCAP1")); // plain-refdes guidance
    }
}
