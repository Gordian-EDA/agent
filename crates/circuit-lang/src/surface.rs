//! Surface AST: what the parser produces. Sugar is still present;
//! pin targets are raw strings (may be net names, pin-refs, or `nc`).

use crate::diag::Span;
use indexmap::IndexMap;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SurfaceDesign {
    pub name: Option<String>,
    pub description: Option<String>,
    /// `rails:` sugar, with spans for diagnostics.
    pub rails: Vec<(String, Span)>,
    pub blocks: IndexMap<String, SurfaceBlock>,
    pub nets: IndexMap<String, SurfaceNet>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SurfaceBlock {
    pub note: Option<String>,
    pub layout: crate::model::LayoutHint,
    pub components: IndexMap<String, SurfaceComponent>,
    pub span: Option<Span>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SurfaceComponent {
    pub part: String, // raw — may be an alias like "R"
    pub value: Option<String>,
    pub footprint: Option<String>,
    pub dnp: bool,
    pub props: IndexMap<String, String>,
    /// raw pin target strings: net name | pin-ref ("U1.PB6") | "nc"
    pub pins: IndexMap<String, (String, Span)>,
    pub units: IndexMap<String, IndexMap<String, (String, Span)>>,
    /// `between:` sugar (exactly two raw targets).
    pub between: Option<((String, Span), (String, Span))>,
    /// `decouple:` sugar — value -> count, e.g. {"100nF": 10}.
    pub decouple: IndexMap<String, u32>,
    pub span: Option<Span>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SurfaceNet {
    pub power: bool,
    pub class: Option<String>,
    pub span: Option<Span>,
}
