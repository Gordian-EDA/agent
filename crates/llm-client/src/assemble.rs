//! Incremental tool-call assembly from a streaming response.
//!
//! Streaming backends deliver a tool call in fragments, and the three wire
//! dialects fragment it differently. [`ToolCallAssembler`] folds any of them
//! into finished [`ToolCall`]s:
//!
//! - **OpenAI** index-keyed partial-arg strings: each chunk carries a slot
//!   `index`, an optional `id`/`name` on the first chunk for that slot, and a
//!   `function.arguments` STRING fragment to concatenate. The joined string is
//!   parsed as JSON at the end ([`ToolCallAssembler::openai_delta`]).
//! - **Anthropic** `input_json_delta`: a `content_block_start` opens a `tool_use`
//!   block (with its `id`/`name`) at a content index, then `partial_json`
//!   fragments concatenate into the input JSON string
//!   ([`ToolCallAssembler::anthropic_start`] + [`ToolCallAssembler::anthropic_json_delta`]).
//! - **Gateway** whole-object args: a single delta carries the COMPLETE
//!   arguments as an already-structured object (Anthropic models proxied through
//!   the respan gateway return tool args as an object, not a string)
//!   ([`ToolCallAssembler::openai_delta`] handles this too — an object-valued
//!   `arguments` is taken whole).

use serde_json::Value;

use crate::types::ToolCall;

/// One in-progress tool call: its identity plus either a string being
/// concatenated (OpenAI / Anthropic partial JSON) or a whole object already
/// supplied (gateway).
#[derive(Default)]
struct Pending {
    id: String,
    name: String,
    /// Accumulated argument-string fragments (OpenAI `arguments` / Anthropic
    /// `partial_json`). Parsed as JSON when the call is finished.
    arg_str: String,
    /// A whole, already-structured argument object (gateway form). When set, it
    /// wins over `arg_str`.
    arg_obj: Option<Value>,
}

/// Folds streaming tool-call deltas (any of the three dialects) into finished
/// [`ToolCall`]s. Slots are keyed by the wire's own index so interleaved chunks
/// for different calls never cross-contaminate.
#[derive(Default)]
pub struct ToolCallAssembler {
    /// slot index → in-progress call, in first-seen order.
    slots: Vec<(usize, Pending)>,
}

impl ToolCallAssembler {
    /// A fresh assembler.
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(&mut self, index: usize) -> &mut Pending {
        if let Some(pos) = self.slots.iter().position(|(i, _)| *i == index) {
            return &mut self.slots[pos].1;
        }
        self.slots.push((index, Pending::default()));
        &mut self.slots.last_mut().unwrap().1
    }

    /// Fold one OpenAI-style `tool_calls[]` delta object (which carries an
    /// `index`, optional `id`, and an optional `function.{name,arguments}`).
    /// `arguments` may be a STRING fragment to concatenate or — in the gateway
    /// form — a whole OBJECT taken as-is.
    pub fn openai_delta(&mut self, delta: &Value) {
        let index = delta.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        let slot = self.slot(index);
        if let Some(id) = delta.get("id").and_then(Value::as_str)
            && !id.is_empty()
        {
            slot.id = id.to_string();
        }
        if let Some(func) = delta.get("function") {
            if let Some(name) = func.get("name").and_then(Value::as_str)
                && !name.is_empty()
            {
                slot.name = name.to_string();
            }
            match func.get("arguments") {
                Some(Value::String(s)) => slot.arg_str.push_str(s),
                Some(other) if !other.is_null() => slot.arg_obj = Some(other.clone()),
                _ => {}
            }
        }
    }

    /// Open an Anthropic `tool_use` content block (from a `content_block_start`
    /// event) at content `index`, carrying its `id` and `name`.
    pub fn anthropic_start(&mut self, index: usize, id: &str, name: &str) {
        let slot = self.slot(index);
        slot.id = id.to_string();
        slot.name = name.to_string();
    }

    /// Fold one Anthropic `input_json_delta` (`partial_json` fragment) for the
    /// `tool_use` block at content `index`.
    pub fn anthropic_json_delta(&mut self, index: usize, partial_json: &str) {
        self.slot(index).arg_str.push_str(partial_json);
    }

    /// Finish: every slot becomes a [`ToolCall`], in first-seen order. A whole
    /// object (gateway) wins; otherwise the concatenated string is parsed (an
    /// empty string becomes `{}`, matching a no-argument call). A parse failure
    /// degrades to `Value::Null` so a truncated call surfaces rather than panics.
    pub fn finish(self) -> Vec<ToolCall> {
        self.slots
            .into_iter()
            .map(|(_, p)| {
                let input = match p.arg_obj {
                    Some(obj) => obj,
                    None => {
                        if p.arg_str.trim().is_empty() {
                            Value::Object(Default::default())
                        } else {
                            serde_json::from_str(&p.arg_str).unwrap_or(Value::Null)
                        }
                    }
                };
                ToolCall { id: p.id, name: p.name, input }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_index_keyed_partial_arg_strings() {
        // One call streamed across several chunks: id+name on the first, then
        // arguments string fragments concatenate.
        let mut a = ToolCallAssembler::new();
        a.openai_delta(&json!({
            "index": 0, "id": "call_1",
            "function": { "name": "search_symbols", "arguments": "" }
        }));
        a.openai_delta(&json!({ "index": 0, "function": { "arguments": "{\"query\":" } }));
        a.openai_delta(&json!({ "index": 0, "function": { "arguments": "\"STM32\"}" } }));
        let calls = a.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "search_symbols");
        assert_eq!(calls[0].input["query"], "STM32");
    }

    #[test]
    fn openai_two_interleaved_calls_keyed_by_index() {
        let mut a = ToolCallAssembler::new();
        a.openai_delta(&json!({ "index": 0, "id": "c0", "function": { "name": "f0", "arguments": "{\"a\":" } }));
        a.openai_delta(&json!({ "index": 1, "id": "c1", "function": { "name": "f1", "arguments": "{\"b\":" } }));
        a.openai_delta(&json!({ "index": 0, "function": { "arguments": "1}" } }));
        a.openai_delta(&json!({ "index": 1, "function": { "arguments": "2}" } }));
        let calls = a.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!((calls[0].name.as_str(), calls[0].input["a"].as_i64()), ("f0", Some(1)));
        assert_eq!((calls[1].name.as_str(), calls[1].input["b"].as_i64()), ("f1", Some(2)));
    }

    #[test]
    fn anthropic_input_json_delta() {
        // content_block_start opens the tool_use block, then partial_json
        // fragments build the input.
        let mut a = ToolCallAssembler::new();
        a.anthropic_start(1, "toolu_1", "create_design");
        a.anthropic_json_delta(1, "{\"yaml\":");
        a.anthropic_json_delta(1, "\"version: 1\"}");
        let calls = a.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].name, "create_design");
        assert_eq!(calls[0].input["yaml"], "version: 1");
    }

    #[test]
    fn anthropic_empty_input_becomes_empty_object() {
        // A no-argument tool: the block opens but no partial_json arrives.
        let mut a = ToolCallAssembler::new();
        a.anthropic_start(0, "toolu_x", "get_design");
        let calls = a.finish();
        assert_eq!(calls[0].input, json!({}), "no args ⇒ empty object, not null");
    }

    #[test]
    fn gateway_whole_object_args() {
        // The respan gateway returns a single delta with the WHOLE arguments as
        // a structured object, not a string.
        let mut a = ToolCallAssembler::new();
        a.openai_delta(&json!({
            "index": 0, "id": "call_xyz",
            "function": { "name": "create_design", "arguments": { "yaml": "version: 1" } }
        }));
        let calls = a.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "create_design");
        assert_eq!(calls[0].input["yaml"], "version: 1");
    }

    #[test]
    fn truncated_args_degrade_to_null_not_panic() {
        let mut a = ToolCallAssembler::new();
        a.openai_delta(&json!({ "index": 0, "id": "c", "function": { "name": "f", "arguments": "{\"yaml\": \"trun" } }));
        let calls = a.finish();
        assert_eq!(calls[0].input, Value::Null, "an unparsable fragment is Null, surfaced not crashed");
    }
}
