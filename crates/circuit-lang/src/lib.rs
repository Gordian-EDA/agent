//! circuit-lang: parse, desugar, lint, and canonically emit the
//! gordian circuit markup language. Pure — no I/O.

pub mod canon;
pub mod desugar;
pub mod diag;
pub mod erc;
pub mod lint;
pub mod model;
pub mod parse;
pub mod provider;
pub mod surface;
mod yaml;

pub use diag::{Diagnostic, Diagnostics, Severity, Span};
pub use model::Design;
pub use provider::{PinDir, PinMeta, PinType, SymbolMeta, SymbolTable, find_pin};

pub struct CompileResult {
    /// Some only when there are no errors (warnings allowed).
    pub design: Option<Design>,
    pub diagnostics: Diagnostics,
}

/// Full gauntlet, pure half: parse -> desugar -> lint.
pub fn compile(src: &str, provider: &SymbolTable) -> CompileResult {
    let (surface, mut diagnostics) = parse::parse_str(src);
    let design = surface.map(|s| {
        let (d, ds) = desugar::desugar(&s, provider);
        diagnostics.extend(ds);
        diagnostics.extend(lint::lint(&d, provider));
        d
    });
    CompileResult {
        design: design.filter(|_| !diagnostics.has_errors()),
        diagnostics,
    }
}
