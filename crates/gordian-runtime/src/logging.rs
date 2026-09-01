//! Process-wide tracing setup for Gordian frontends.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{Event, Subscriber};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{EnvFilter, filter_fn};
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

/// Target whose stderr events are emitted as a bare agent transcript.
pub const EVENTS_TARGET: &str = "gordian::events";

/// Keeps the non-blocking file writer alive and flushes it when dropped.
#[must_use = "the logging guard must be held until process exit"]
pub struct LogGuard {
    _file_guard: WorkerGuard,
    path: PathBuf,
}

impl LogGuard {
    /// Returns the file receiving the complete process log.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Installs file and compact stderr logging for a headless command.
pub fn init(project_dir: &Path, thread_id: &str) -> LogGuard {
    init_with_stderr(project_dir, thread_id, true)
}

/// Installs file logging without stderr output for an active terminal UI.
pub fn init_file_only(project_dir: &Path, thread_id: &str) -> LogGuard {
    init_with_stderr(project_dir, thread_id, false)
}

fn init_with_stderr(project_dir: &Path, thread_id: &str, stderr_enabled: bool) -> LogGuard {
    let logs_dir = project_dir.join(".gordian/logs");
    std::fs::create_dir_all(&logs_dir).expect("create Gordian log directory");
    let filename = format!(
        "{}-{}.log",
        utc_timestamp(),
        safe_filename_component(thread_id)
    );
    let path = logs_dir.join(&filename);
    let appender = tracing_appender::rolling::never(&logs_dir, filename);
    let (file_writer, file_guard) = tracing_appender::non_blocking(appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_target(true)
        .with_filter(env_filter("debug"));

    let stderr_layer = stderr_enabled.then(|| {
        tracing_subscriber::fmt::layer()
            .compact()
            .with_writer(std::io::stderr)
            .with_filter(env_filter("info"))
            .with_filter(filter_fn(|metadata| metadata.target() != EVENTS_TARGET))
    });
    let events_layer = stderr_enabled.then(|| {
        tracing_subscriber::fmt::layer()
            .event_format(BareEventFormatter)
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .with_filter(env_filter("info"))
            .with_filter(filter_fn(|metadata| metadata.target() == EVENTS_TARGET))
    });

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .with(events_layer)
        .try_init()
        .expect("Gordian tracing subscriber must only be initialized once");

    LogGuard {
        _file_guard: file_guard,
        path,
    }
}

fn env_filter(default_level: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level))
}

fn safe_filename_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn utc_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_secs()
}

struct BareEventFormatter;

impl<S, N> FormatEvent<S, N> for BareEventFormatter
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        context
            .field_format()
            .format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_identifier_is_safe_as_a_filename_component() {
        assert_eq!(
            safe_filename_component("quality/case one"),
            "quality_case_one"
        );
        assert_eq!(safe_filename_component("thread-01_a.b"), "thread-01_a.b");
    }
}
