//! The run's progress stream: one bare stderr transcript for the CLI, and the
//! same moments as typed values for an interactive frontend.
//!
//! Every stage reports through [`emit`]. The line it logs is the transcript
//! grammar the quality harness parses (`tool ->`, `tool <-`, `usage:`,
//! `review N/10`); a frontend that has called [`subscribe`] additionally
//! receives the structured [`AgentEvent`], including the two moments that carry
//! no line at all — a written PNG and the end of the run.

use std::path::PathBuf;
use std::sync::RwLock;

use gordian_runtime::logging::EVENTS_TARGET;

/// One reportable moment in a run.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    /// Prose the model produced alongside its tool calls.
    Assistant(String),
    /// A tool is about to run, with its arguments (large payloads by size).
    ToolCall { name: String, args: String },
    /// That tool returned, with a one-line digest of what it said.
    ToolResult {
        name: String,
        seconds: f64,
        summary: String,
    },
    /// One provider round trip and what it cost.
    Usage {
        request: u64,
        /// Prompt tokens, cache reads and writes included.
        input: u64,
        output: u64,
        cache_write: u64,
        /// Prompt tokens served from the cache.
        cached: u64,
        seconds: f64,
    },
    /// The visual critic graded the current build.
    Review {
        score: f64,
        mean: f64,
        samples: Vec<f64>,
        defects: usize,
    },
    /// A PNG the run just wrote: a sheet render, or the routed board.
    Render { label: String, path: PathBuf },
    /// Progress with no further structure — skills, the board, compose, the report.
    Note(String),
    /// The run is over and `report.json` is written.
    Done,
}

impl AgentEvent {
    /// The bare transcript line, or `None` for an event that only a frontend
    /// consumes and that must not change what the CLI prints.
    pub fn line(&self) -> Option<String> {
        Some(match self {
            Self::Assistant(text) => format!("assistant: {text}"),
            Self::ToolCall { name, args } => format!("tool -> {name} {args}"),
            Self::ToolResult {
                name,
                seconds,
                summary,
            } => format!("tool <- {name} (elapsed {seconds:.1}s): {summary}"),
            Self::Usage {
                request,
                input,
                output,
                cached,
                seconds,
                ..
            } => format!(
                "usage: request #{request} in={input} out={output} cached={cached} \
                 latency={seconds:.1}s"
            ),
            Self::Review {
                score,
                mean,
                samples,
                defects,
            } => format!("review {score:.0}/10 mean {mean:.2} samples {samples:?} — {defects} defect(s)"),
            Self::Note(line) => line.clone(),
            Self::Render { .. } | Self::Done => return None,
        })
    }
}

/// Where structured events go while a frontend is listening.
type Sink = Box<dyn Fn(AgentEvent) + Send + Sync>;

static SINK: RwLock<Option<Sink>> = RwLock::new(None);

/// Send every later [`AgentEvent`] to `sink` as well as to the transcript.
/// One sink at a time: a second call replaces the first.
pub fn subscribe(sink: impl Fn(AgentEvent) + Send + Sync + 'static) {
    *lock() = Some(Box::new(sink));
}

/// Stop delivering structured events.
pub fn unsubscribe() {
    *lock() = None;
}

/// Report one moment: its transcript line, then the subscriber.
pub fn emit(event: AgentEvent) {
    if let Some(line) = event.line() {
        tracing::info!(target: EVENTS_TARGET, "{line}");
    }
    let sink = lock();
    if let Some(sink) = sink.as_ref() {
        sink(event);
    }
}

/// A progress line with no further structure.
pub fn event(line: impl AsRef<str>) {
    emit(AgentEvent::Note(line.as_ref().to_string()));
}

/// A poisoned sink lock only means a subscriber panicked; the run keeps
/// reporting rather than take the process down with it.
fn lock() -> std::sync::RwLockWriteGuard<'static, Option<Sink>> {
    SINK.write().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// The transcript grammar the quality harness parses is the one `line`
    /// produces.
    #[test]
    fn the_lines_keep_the_transcript_grammar() {
        assert_eq!(
            AgentEvent::ToolCall {
                name: "build".into(),
                args: "{}".into()
            }
            .line()
            .unwrap(),
            "tool -> build {}"
        );
        assert_eq!(
            AgentEvent::ToolResult {
                name: "erc".into(),
                seconds: 1.25,
                summary: "clean".into()
            }
            .line()
            .unwrap(),
            "tool <- erc (elapsed 1.2s): clean"
        );
        assert!(
            AgentEvent::Review {
                score: 8.0,
                mean: 7.8,
                samples: vec![8.0],
                defects: 2,
            }
            .line()
            .unwrap()
            .starts_with("review 8/10 mean 7.80")
        );
    }

    /// A render and the end of the run reach a frontend without adding a line
    /// to what the CLI prints.
    #[test]
    fn structural_events_print_nothing() {
        assert!(
            AgentEvent::Render {
                label: "v1".into(),
                path: PathBuf::from("/tmp/v1.png")
            }
            .line()
            .is_none()
        );
        assert!(AgentEvent::Done.line().is_none());
    }

    #[test]
    fn a_subscriber_sees_events_until_it_unsubscribes() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        subscribe(move |e| sink.lock().unwrap().push(format!("{e:?}")));
        event("hello");
        unsubscribe();
        event("unheard");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].contains("hello"));
    }
}
