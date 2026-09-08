//! The cockpit's own progress event, and the translation from the core's.
//!
//! The transcript, the working row and the status bar are written against this
//! enum: an assistant reply, a tool card opening and closing, a provider
//! invocation, a render to preview. `gordian_core` reports the same moments in
//! its own vocabulary ([`gordian_core::AgentEvent`]), so [`translate`] is the
//! single place the two meet — everything downstream stays unaware of which
//! core produced it.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

/// A monotonic identity for tool cards, so a card can be matched to its result.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// One moment of a run, as the cockpit understands it.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    /// An incremental chunk of assistant text, streamed live as the model
    /// produces it. Concatenating a turn's `AssistantDelta`s reconstructs the
    /// text later finalized in [`AgentEvent::AssistantText`].
    AssistantDelta(String),
    /// The model produced assistant text (interleaved with tool calls or final).
    AssistantText(String),
    /// A tool call is about to run.
    ToolStarted { name: String, args: Value, seq: u64 },
    /// A tool call finished; `summary` is a short one-line digest for a card.
    /// `image_path` carries the on-disk PNG a render produced (if any), so the
    /// transcript can offer it inline; `None` for every non-render tool.
    ToolFinished {
        name: String,
        summary: String,
        image_path: Option<String>,
        elapsed_ms: u64,
        result: Value,
    },
    /// One provider invocation, including failed requests with zero tokens.
    ProviderRequest {
        request: u64,
        input_tokens: u64,
        output_tokens: u64,
        cache_write_tokens: u64,
        cache_read_tokens: u64,
        latency_ms: u64,
    },
    /// An ordered warning or error that belongs in the live transcript.
    Diagnostic {
        level: &'static str,
        target: String,
        message: String,
    },
    /// Progress with no further structure — a plain line for the transcript.
    Note(String),
    /// Provider invocation and token usage. `input_tokens` includes
    /// `cache_write_tokens` and `cache_read_tokens`, letting consumers bill
    /// cached prefixes correctly.
    Usage {
        provider_requests: u64,
        input_tokens: u64,
        output_tokens: u64,
        cache_write_tokens: u64,
        cache_read_tokens: u64,
    },
    /// The conversation history was replaced with a summary pair.
    Compacted {
        messages_before: usize,
        messages_after: usize,
    },
    /// The turn finished.
    TurnDone,
    /// An independent review pass over the committed work started.
    ReviewStarted { round: usize },
    /// That review pass completed.
    Reviewed {
        round: usize,
        score: f64,
        defects: Vec<String>,
    },
}

/// Map one core event onto the cockpit's.
///
/// Two shapes need a decision rather than a rename. A `Usage` report is one
/// provider round trip, so it becomes both the ledger entry and the request
/// counter the HUD reads. A `Render` has no tool call of its own in the new
/// core, so it is presented as the render tool that would have produced it —
/// which is what attaches the PNG to a transcript row the user can click.
pub fn translate(event: gordian_core::AgentEvent) -> AgentEvent {
    use gordian_core::AgentEvent as Core;
    match event {
        Core::Assistant(text) => AgentEvent::AssistantText(text),
        Core::ToolCall { name, args } => AgentEvent::ToolStarted {
            name,
            args: Value::String(args),
            seq: SEQ.fetch_add(1, Ordering::Relaxed),
        },
        Core::ToolResult {
            name,
            seconds,
            summary,
        } => AgentEvent::ToolFinished {
            name,
            image_path: None,
            elapsed_ms: (seconds * 1000.0) as u64,
            result: json!({"summary": summary}),
            summary,
        },
        Core::Usage {
            request,
            input,
            output,
            cache_write,
            cached,
            seconds,
        } => {
            let _ = (request, seconds);
            AgentEvent::Usage {
                provider_requests: 1,
                input_tokens: input,
                output_tokens: output,
                cache_write_tokens: cache_write,
                cache_read_tokens: cached,
            }
        }
        Core::Review {
            score,
            mean,
            samples,
            defects,
        } => AgentEvent::Note(format!(
            "design review: {score:.0}/10 (mean {mean:.2} over {} samples) — {}",
            samples.len(),
            match defects {
                0 => "no functional defects".to_string(),
                n => format!("{n} defect(s)"),
            }
        )),
        Core::Render { label, path } => {
            let display = path.display().to_string();
            AgentEvent::ToolFinished {
                name: render_tool_for(&path).to_string(),
                summary: label,
                image_path: Some(display),
                elapsed_ms: 0,
                result: Value::Null,
            }
        }
        Core::Note(line) => AgentEvent::Note(line),
        Core::Done => AgentEvent::TurnDone,
    }
}

/// Which render tool a written PNG stands for. The transcript only offers an
/// inline preview for these two names, and the board's renders are the ones
/// that name a side.
fn render_tool_for(path: &Path) -> &'static str {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.contains("board") || name.contains("pcb") {
        "render_board"
    } else {
        "render_schematic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_written_png_becomes_a_clickable_render_card() {
        let event = translate(gordian_core::AgentEvent::Render {
            label: "v3 grid".into(),
            path: PathBuf::from("/tmp/p/v3_grid.png"),
        });
        let AgentEvent::ToolFinished {
            name,
            summary,
            image_path,
            ..
        } = event
        else {
            panic!("a render is a tool card");
        };
        assert_eq!(name, "render_schematic");
        assert_eq!(summary, "v3 grid");
        assert_eq!(image_path.as_deref(), Some("/tmp/p/v3_grid.png"));
    }

    #[test]
    fn the_boards_renders_are_the_board_tools() {
        assert_eq!(
            render_tool_for(Path::new("/tmp/board-front.png")),
            "render_board"
        );
        assert_eq!(
            render_tool_for(Path::new("/tmp/schematic.png")),
            "render_schematic"
        );
    }

    #[test]
    fn one_usage_report_is_one_provider_request() {
        let event = translate(gordian_core::AgentEvent::Usage {
            request: 4,
            input: 900,
            output: 120,
            cache_write: 30,
            cached: 800,
            seconds: 2.0,
        });
        let AgentEvent::Usage {
            provider_requests,
            input_tokens,
            cache_read_tokens,
            ..
        } = event
        else {
            panic!("usage stays usage");
        };
        assert_eq!((provider_requests, input_tokens, cache_read_tokens), (1, 900, 800));
    }

    #[test]
    fn a_graded_build_reads_as_a_review_line() {
        let AgentEvent::Note(line) = translate(gordian_core::AgentEvent::Review {
            score: 8.0,
            mean: 7.75,
            samples: vec![8.0, 7.5],
            defects: 2,
        }) else {
            panic!("a review is a transcript note");
        };
        assert_eq!(
            line,
            "design review: 8/10 (mean 7.75 over 2 samples) — 2 defect(s)"
        );
    }
}
