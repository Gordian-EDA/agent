//! circuit-lang: parse, desugar, and canonically emit the gordian circuit
//! markup language. The model it produces and the checks that judge it live in
//! [`sch_check`]. Pure — no I/O.

pub mod canon;
pub mod desugar;
pub mod parse;
pub mod surface;
mod yaml;

use sch_check::{Design, Diagnostics, SymbolTable};

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
        diagnostics.extend(sch_check::authored::lint(&d, provider));
        diagnostics.extend(sch_check::lint::lint(&d, provider));
        d
    });
    CompileResult {
        design: design.filter(|_| !diagnostics.has_errors()),
        diagnostics,
    }
}
