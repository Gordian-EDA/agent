use std::fmt;

/// 1-based source position (from saphyr markers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// One finding. `code` is a stable machine-readable id; `suggestion`
/// is the "did you mean" payload the agent uses to self-repair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub span: Option<Span>,
    pub suggestion: Option<String>,
}

impl Diagnostic {
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            code,
            message: message.into(),
            span: None,
            suggestion: None,
        }
    }
    pub fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            code,
            message: message.into(),
            span: None,
            suggestion: None,
        }
    }
    pub fn with_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }
    pub fn with_suggestion(mut self, s: impl Into<String>) -> Self {
        self.suggestion = Some(s.into());
        self
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sev = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(f, "{sev}[{}]", self.code)?;
        if let Some(s) = self.span {
            write!(f, " at {}:{}", s.line, s.col)?;
        }
        write!(f, ": {}", self.message)?;
        if let Some(sug) = &self.suggestion {
            write!(f, " (did you mean `{sug}`?)")?;
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Diagnostics(pub Vec<Diagnostic>);

impl Diagnostics {
    pub fn push(&mut self, d: Diagnostic) {
        self.0.push(d);
    }
    pub fn has_errors(&self) -> bool {
        self.0.iter().any(|d| d.severity == Severity::Error)
    }
    pub fn extend(&mut self, other: Diagnostics) {
        self.0.extend(other.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_renders_with_span_and_suggestion() {
        let d = Diagnostic::error("unknown-key", "unknown key `decuople`")
            .with_span(Span { line: 7, col: 5 })
            .with_suggestion("decouple");
        assert_eq!(
            d.to_string(),
            "error[unknown-key] at 7:5: unknown key `decuople` (did you mean `decouple`?)"
        );
    }

    #[test]
    fn diagnostics_collection_tracks_errors() {
        let mut ds = Diagnostics::default();
        assert!(!ds.has_errors());
        ds.push(Diagnostic::warning(
            "near-name",
            "nets `X` and `Y` differ by one character",
        ));
        assert!(!ds.has_errors());
        ds.push(Diagnostic::error(
            "pin-conflict",
            "pin `1` of R1 mapped twice",
        ));
        assert!(ds.has_errors());
        assert_eq!(ds.0.len(), 2);
    }
}
