//! `lock_parts` / `unlock_parts`: the one thing on a board no helper overrules.
//!
//! A lock is KiCad's own footprint `locked` flag, so KiCad honours it too, plus
//! a `locked_reason` the agent can read back: `mechanical` for a position the
//! physical world fixed, `agent` for a pose worth keeping, `user` for a lock
//! made outside these tools.

use anyhow::Result;
use serde_json::{Value, json};

use gordian_runtime::AgentRuntime;
use kicad_board::Annotation;

use crate::board::guard::{Edit, Guard};
use crate::staging::{LockReason, lock_reason};

/// The references a lock tool was asked to act on.
fn requested_refs(input: &Value) -> std::result::Result<Vec<String>, String> {
    let refs: Vec<String> = input
        .get("refs")
        .and_then(Value::as_array)
        .ok_or("refs must be an array of footprint references")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "every entry in refs must be a string".to_owned())
        })
        .collect::<std::result::Result<_, String>>()?;
    if refs.is_empty() {
        return Err("refs named no footprints".to_owned());
    }
    Ok(refs)
}

fn unknown_refs(board: &kicad_board::BoardSnapshot, refs: &[String]) -> Vec<String> {
    let known: std::collections::BTreeSet<&str> = board
        .imported
        .parts
        .iter()
        .map(|part| part.reference.as_str())
        .collect();
    refs.iter()
        .filter(|reference| !known.contains(reference.as_str()))
        .cloned()
        .collect()
}

/// Write the lock flag and its reason for every named part.
fn apply(
    ctx: &AgentRuntime,
    tool: &'static str,
    refs: Vec<String>,
    reason: Option<LockReason>,
) -> Result<Value> {
    let path = ctx.pcb_path();
    let annotations: Vec<Annotation> = refs
        .iter()
        .map(|reference| match reason {
            Some(reason) => Annotation::new(reference.clone())
                .locked(true)
                .set(kicad_board::LOCKED_REASON, reason.as_str()),
            None => Annotation::new(reference.clone())
                .locked(false)
                .clear(kicad_board::LOCKED_REASON),
        })
        .collect();
    let gate = match Guard::open(ctx, Edit::new(tool, std::slice::from_ref(&path))) {
        Ok(gate) => gate,
        Err(refusal) => return Ok(refusal),
    };
    let written = std::fs::read_to_string(&path)
        .map_err(|error| format!("could not read the board: {error}"))
        .and_then(|text| kicad_board::patch_annotations(&text, &annotations))
        .and_then(|text| {
            std::fs::write(&path, text)
                .map_err(|error| format!("could not write the board: {error}"))
        });
    if let Err(error) = written {
        return Ok(gate.rollback(ctx, json!({ "error": format!("{tool}: {error}") })));
    }
    Ok(gate.commit(
        ctx,
        json!({
            "ok": true,
            "locked": reason.is_some(),
            "refs": refs,
            "locked_reason": reason.map(LockReason::as_str),
        }),
    ))
}

/// Lock footprints so no helper moves them.
#[tracing::instrument(skip_all, fields(project = %ctx.project_dir().display()))]
pub fn lock_parts(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let board = match crate::active_board(ctx) {
        Ok(board) => board,
        Err(error) => return Ok(json!({ "error": error })),
    };
    let refs = match requested_refs(&input) {
        Ok(refs) => refs,
        Err(error) => return Ok(json!({ "error": error })),
    };
    let unknown = unknown_refs(&board, &refs);
    if !unknown.is_empty() {
        return Ok(json!({
            "error": format!("lock_parts: {} is not on this board", unknown.join(", ")),
            "code": "unknown_refs",
            "unknown": unknown,
        }));
    }
    let reason = match input.get("reason").and_then(Value::as_str) {
        None => LockReason::Agent,
        Some(value) => match LockReason::parse(value) {
            Some(reason) => reason,
            None => {
                return Ok(json!({
                    "error": format!(
                        "lock_parts: unknown reason `{value}` — use mechanical, agent or user"
                    ),
                }));
            }
        },
    };
    apply(ctx, "lock_parts", refs, Some(reason))
}

/// Release locks so placement may move these parts again.
#[tracing::instrument(skip_all, fields(project = %ctx.project_dir().display()))]
pub fn unlock_parts(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let board = match crate::active_board(ctx) {
        Ok(board) => board,
        Err(error) => return Ok(json!({ "error": error })),
    };
    let refs = match requested_refs(&input) {
        Ok(refs) => refs,
        Err(error) => return Ok(json!({ "error": error })),
    };
    let unknown = unknown_refs(&board, &refs);
    if !unknown.is_empty() {
        return Ok(json!({
            "error": format!("unlock_parts: {} is not on this board", unknown.join(", ")),
            "code": "unknown_refs",
            "unknown": unknown,
        }));
    }
    let already: Vec<&str> = board
        .imported
        .parts
        .iter()
        .filter(|part| refs.iter().any(|reference| reference == &part.reference))
        .filter(|part| lock_reason(part).is_none())
        .map(|part| part.reference.as_str())
        .collect();
    let mut result = apply(ctx, "unlock_parts", refs, None)?;
    if !already.is_empty()
        && let Some(object) = result.as_object_mut()
    {
        object.insert("already_unlocked".to_owned(), json!(already));
    }
    Ok(result)
}

/// Whichever of `refs` a locked footprint protects, and why. Every helper that
/// moves parts asks this before it moves them.
pub(crate) fn locked_among<'a>(
    board: &'a kicad_board::BoardSnapshot,
    refs: impl IntoIterator<Item = &'a str>,
) -> Vec<(&'a str, LockReason)> {
    let wanted: std::collections::BTreeSet<&str> = refs.into_iter().collect();
    board
        .imported
        .parts
        .iter()
        .filter(|part| wanted.contains(part.reference.as_str()))
        .filter_map(|part| lock_reason(part).map(|reason| (part.reference.as_str(), reason)))
        .collect()
}

/// The refusal a helper owes a caller who asked it to move a locked part.
pub(crate) fn locked_refusal(tool: &str, locked: &[(&str, LockReason)]) -> Value {
    let named = locked
        .iter()
        .map(|(reference, reason)| format!("{reference} ({})", reason.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    json!({
        "error": format!(
            "{tool} refused: {named} {} locked, and a lock is the one thing no helper \
             overrules; nothing was moved",
            if locked.len() == 1 { "is" } else { "are" }
        ),
        "code": "parts_locked",
        "locked": locked
            .iter()
            .map(|(reference, reason)| json!({ "ref": reference, "locked_reason": reason.as_str() }))
            .collect::<Vec<_>>(),
        "note": "Call unlock_parts({\"refs\": [...]}) if the lock should go, then try again.",
    })
}
