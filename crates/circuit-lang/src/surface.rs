//! Surface AST: what the parser produces. Sugar is still present;
//! pin targets are raw strings (may be net names, pin-refs, or `nc`).

use crate::diag::Span;
use indexmap::IndexMap;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SurfaceDesign {
    pub name: Option<String>,
    pub description: Option<String>,
    /// `power:` — the power/ground nets, with spans for diagnostics. The single
    /// way to declare power (there is no longer a per-net `power: true`).
    pub power: Vec<(String, Span)>,
    pub blocks: IndexMap<String, SurfaceBlock>,
    pub nets: IndexMap<String, SurfaceNet>,
    /// Lint codes suppressed via top-level `lint: {allow: [...]}`.
    pub lint_allow: Vec<String>,
    /// Top-level `layout:` placement grid — rows of cells, each a block/refdes
    /// name (with a span for diagnostics) or `None` for a `~` hole.
    pub layout: Vec<Vec<(Option<String>, Span)>>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SurfaceBlock {
    pub note: Option<String>,
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
    /// `between:` sugar (exactly two raw targets) — SYMMETRIC 2-pin parts.
    pub between: Option<((String, Span), (String, Span))>,
    /// `positive:`/`negative:` sugar — POLARIZED 2-pin parts. `positive` wires the
    /// anode (`A`/`+`) pin, `negative` the cathode (`K`/`-`).
    pub positive: Option<(String, Span)>,
    pub negative: Option<(String, Span)>,
    /// `decouple:` sugar — value -> count, e.g. {"100nF": 10}.
    pub decouple: IndexMap<String, u32>,
    pub span: Option<Span>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SurfaceNet {
    pub class: Option<String>,
    pub span: Option<Span>,
}
