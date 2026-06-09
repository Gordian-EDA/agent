# Plan 1: Workspace + `circuit-lang` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restructure the repo into a cargo workspace and build `circuit-lang` — the pure (no I/O) crate that parses, desugars, lints, and canonically re-emits the circuit markup language defined in spec §5.

**Architecture:** Hand-walked YAML 1.2 tree (`saphyr` `MarkedYaml`, spans for diagnostics) → strict **surface AST** (sugar still present) → **desugar pass** (needs a `SymbolProvider` trait, mocked in tests) → **kernel model** → lints → deterministic canonical emitter. Spec: `docs/superpowers/specs/2026-06-09-kicad-copilot-agent-design.md` §5–6. This is plan 1 of 4 (2: kicad-bridge, 3: sch-engine, 4: agent+TUI).

**Tech Stack:** Rust edition 2024, `saphyr` (YAML 1.2 + spans), `indexmap` (deterministic ordering), `strsim` (did-you-mean), `thiserror`.

**Conventions for every task:** run commands from repo root. All `circuit-lang` code is pure — any task adding file/network I/O to this crate is a bug.

---

### Task 1: Cargo workspace restructure

**Files:**
- Modify: `Cargo.toml` (root → virtual workspace manifest)
- Create: `crates/autopcb/Cargo.toml`, `crates/autopcb/src/main.rs`
- Create: `crates/circuit-lang/Cargo.toml`, `crates/circuit-lang/src/lib.rs`
- Delete: `src/main.rs`

- [ ] **Step 1: Write the workspace manifest** — replace root `Cargo.toml` entirely:

```toml
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.package]
version = "0.1.0"
edition = "2024"

[workspace.dependencies]
saphyr = "0.0.6"
indexmap = "2"
strsim = "0.11"
thiserror = "2"
```

(If `cargo add saphyr --dry-run` shows a newer 0.x, pin that instead; same for others.)

- [ ] **Step 2: Create the two crates**

`crates/circuit-lang/Cargo.toml`:
```toml
[package]
name = "circuit-lang"
version.workspace = true
edition.workspace = true

[dependencies]
saphyr.workspace = true
indexmap.workspace = true
strsim.workspace = true
thiserror.workspace = true
```

`crates/circuit-lang/src/lib.rs`:
```rust
//! circuit-lang: parse, desugar, lint, and canonically emit the
//! auto-pcb circuit markup language (spec §5). Pure — no I/O.
```

`crates/autopcb/Cargo.toml`:
```toml
[package]
name = "autopcb"
version.workspace = true
edition.workspace = true

[dependencies]
circuit-lang = { path = "../circuit-lang" }
```

`crates/autopcb/src/main.rs`:
```rust
fn main() {
    println!("auto-pcb {}", env!("CARGO_PKG_VERSION"));
}
```

Then `git rm src/main.rs` (the old hello-world root binary).

- [ ] **Step 3: Verify the workspace builds**

Run: `cargo build && cargo run -p autopcb`
Expected: builds clean; prints `auto-pcb 0.1.0`.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "refactor: restructure into cargo workspace (circuit-lang, autopcb)"
```

---

### Task 2: Diagnostics module

**Files:**
- Create: `crates/circuit-lang/src/diag.rs`
- Modify: `crates/circuit-lang/src/lib.rs` (add `pub mod diag;`)

Diagnostics are the LLM self-repair channel (spec §6): structured, span-bearing, suggestion-bearing.

- [ ] **Step 1: Write the failing test** — at the bottom of `crates/circuit-lang/src/diag.rs`:

```rust
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
        ds.push(Diagnostic::warning("single-pin-net", "net `X` has only one pin"));
        assert!(!ds.has_errors());
        ds.push(Diagnostic::error("pin-conflict", "pin `1` of R1 mapped twice"));
        assert!(ds.has_errors());
        assert_eq!(ds.0.len(), 2);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p circuit-lang`
Expected: FAIL to compile — `Diagnostic` not found.

- [ ] **Step 3: Implement** — top of `crates/circuit-lang/src/diag.rs`:

```rust
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
        Self { severity: Severity::Error, code, message: message.into(), span: None, suggestion: None }
    }
    pub fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self { severity: Severity::Warning, code, message: message.into(), span: None, suggestion: None }
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
```

And in `lib.rs` add: `pub mod diag;`

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p circuit-lang`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/circuit-lang && git commit -m "feat(circuit-lang): diagnostics with spans and suggestions"
```

---

### Task 3: Kernel + surface model types

**Files:**
- Create: `crates/circuit-lang/src/model.rs` (kernel — what the reconciler sees)
- Create: `crates/circuit-lang/src/surface.rs` (surface AST — sugar still present)
- Modify: `crates/circuit-lang/src/lib.rs`

Pure data definitions; the test just exercises construction + equality (used later by idempotence tests).

- [ ] **Step 1: Write the failing test** — bottom of `crates/circuit-lang/src/model.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn design_equality_is_structural() {
        let mut a = Design::default();
        a.blocks.insert("main".into(), Block::default());
        let mut b = Design::default();
        b.blocks.insert("main".into(), Block::default());
        assert_eq!(a, b);
        b.nets.insert("GND".into(), NetAttrs { power: true, class: None });
        assert_ne!(a, b);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p circuit-lang`
Expected: FAIL to compile — `Design` not found.

- [ ] **Step 3: Implement the kernel model** — `crates/circuit-lang/src/model.rs`:

```rust
use indexmap::IndexMap;

pub type RefDes = String;
pub type NetName = String;
pub type BlockName = String;

/// Kernel design — post-desugar. This is the ONLY thing the
/// validator, reconciler, and lift operate on (spec §5 layering).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Design {
    pub name: Option<String>,
    pub description: Option<String>,
    pub blocks: IndexMap<BlockName, Block>,
    pub nets: IndexMap<NetName, NetAttrs>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Block {
    pub note: Option<String>,
    pub layout: LayoutHint,
    pub components: IndexMap<RefDes, Component>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LayoutHint {
    pub edge: Option<Edge>,
    pub near: Option<BlockName>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub part: String, // full lib_id after alias desugar, e.g. "Device:R"
    pub value: Option<String>,
    pub footprint: Option<String>,
    pub dnp: bool,
    pub props: IndexMap<String, String>,
    /// Component-level pin map; resolves across units (spec §5.2).
    pub pins: IndexMap<String, PinTarget>,
    /// Multi-unit parts: unit letter -> pin map.
    pub units: IndexMap<String, IndexMap<String, PinTarget>>,
    pub origin: Origin,
}

impl Default for Component {
    fn default() -> Self {
        Self {
            part: String::new(),
            value: None,
            footprint: None,
            dnp: false,
            props: IndexMap::new(),
            pins: IndexMap::new(),
            units: IndexMap::new(),
            origin: Origin::Authored,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinTarget {
    Net(NetName),
    NoConnect,
}

/// Identity for reconciliation (spec §7): authored components match by
/// refdes; sugar-synthesized ones by (parent, role, index) — carried
/// into the sch file as ap_parent/ap_role/ap_index properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Authored,
    Synthesized { parent: RefDes, role: String, index: u32 },
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NetAttrs {
    pub power: bool,
    pub class: Option<String>,
}
```

- [ ] **Step 4: Implement the surface AST** — `crates/circuit-lang/src/surface.rs`:

```rust
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
```

In `lib.rs` add:
```rust
pub mod model;
pub mod surface;
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p circuit-lang`
Expected: all tests pass (3 total so far).

- [ ] **Step 6: Commit**

```bash
git add crates/circuit-lang && git commit -m "feat(circuit-lang): kernel model and surface AST types"
```

---

### Task 4: YAML adapter + strict parser → surface AST

**Files:**
- Create: `crates/circuit-lang/src/yaml.rs` (thin saphyr adapter — the ONLY module touching saphyr)
- Create: `crates/circuit-lang/src/parse.rs`
- Modify: `crates/circuit-lang/src/lib.rs`

The adapter isolates saphyr's API behind our own `Node` type so version churn touches one file. **Note:** saphyr 0.0.x type names (`MarkedYaml`, `YamlData::{Mapping,Sequence,Value}`, `Scalar`) may differ slightly per version — adapt names in `yaml.rs` only; the `Node` interface below is fixed.

- [ ] **Step 1: Write the failing tests** — bottom of `crates/circuit-lang/src/parse.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::Severity;

    const MINIMAL: &str = "
version: 1
blocks:
  main:
    components:
      R1: {part: R, value: 4.7k, pins: {1: A, 2: GND}}
";

    #[test]
    fn parses_minimal_design() {
        let (d, diags) = parse_str(MINIMAL);
        assert!(!diags.has_errors(), "{:?}", diags);
        let d = d.unwrap();
        let r1 = &d.blocks["main"].components["R1"];
        assert_eq!(r1.part, "R");
        assert_eq!(r1.value.as_deref(), Some("4.7k")); // YAML 1.2: stays a string
        assert_eq!(r1.pins["1"].0, "A");
        assert_eq!(r1.pins["2"].0, "GND");
    }

    #[test]
    fn unknown_key_errors_with_suggestion() {
        let src = "
version: 1
blocks:
  main:
    components:
      U1: {part: X:Y, decuople: {100nF: 2}}
";
        let (_, diags) = parse_str(src);
        let e = diags.0.iter().find(|d| d.code == "unknown-key").unwrap();
        assert_eq!(e.severity, Severity::Error);
        assert_eq!(e.suggestion.as_deref(), Some("decouple"));
        assert!(e.span.is_some());
    }

    #[test]
    fn version_must_be_1() {
        let (_, diags) = parse_str("version: 2\nblocks: {main: {components: {}}}");
        assert!(diags.0.iter().any(|d| d.code == "bad-version"));
        let (_, diags) = parse_str("blocks: {main: {components: {}}}");
        assert!(diags.0.iter().any(|d| d.code == "bad-version"));
    }

    #[test]
    fn parses_sugar_and_full_component_fields() {
        let src = "
version: 1
name: t
rails: [3V3, GND]
blocks:
  main:
    note: power section
    layout: {edge: top}
    components:
      C1: {part: C, value: 10uF, between: [VBUS, GND], dnp: true,
           footprint: Capacitor_SMD:C_0603_1608Metric, props: {MPN: GRM188}}
      U3:
        part: Amplifier_Operational:LM358
        units:
          A: {pins: {'+': X, '-': Y, OUT: Z}}
        pins: {V+: 3V3, V-: GND}
      U1: {part: M:CPU, decouple: {100nF: 4}, pins: {VDD: 3V3, PA0: nc}}
nets:
  X: {class: analog}
";
        let (d, diags) = parse_str(src);
        assert!(!diags.has_errors(), "{:?}", diags);
        let d = d.unwrap();
        assert_eq!(d.rails.len(), 2);
        let b = &d.blocks["main"];
        assert_eq!(b.layout.edge, Some(crate::model::Edge::Top));
        let c1 = &b.components["C1"];
        assert!(c1.dnp);
        assert_eq!(c1.between.as_ref().unwrap().0 .0, "VBUS");
        assert_eq!(c1.props["MPN"], "GRM188");
        let u3 = &b.components["U3"];
        assert_eq!(u3.units["A"]["OUT"].0, "Z");
        assert_eq!(u3.pins["V+"].0, "3V3");
        let u1 = &b.components["U1"];
        assert_eq!(u1.decouple["100nF"], 4);
        assert_eq!(u1.pins["PA0"].0, "nc");
        assert_eq!(d.nets["X"].class.as_deref(), Some("analog"));
    }

    #[test]
    fn bad_refdes_and_net_names_rejected() {
        let src = "
version: 1
blocks:
  main:
    components:
      lowercase1: {part: R, pins: {1: 'MY NET'}}
";
        let (_, diags) = parse_str(src);
        assert!(diags.0.iter().any(|d| d.code == "bad-refdes"));
        assert!(diags.0.iter().any(|d| d.code == "bad-net-name")); // space
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p circuit-lang`
Expected: FAIL to compile — `parse_str` not found.

- [ ] **Step 3: Implement the adapter** — `crates/circuit-lang/src/yaml.rs`:

```rust
//! Thin adapter over saphyr: converts its marked tree into our `Node`.
//! The ONLY module allowed to import saphyr.

use crate::diag::{Diagnostic, Diagnostics, Span};
use saphyr::{LoadableYamlNode, MarkedYaml, YamlData};

#[derive(Debug, Clone)]
pub enum Node {
    /// All scalars are surfaced as their literal string form (pin
    /// numbers, values like `4.7k`, `NO` — spec §5.3.7).
    Scalar(String, Span),
    Seq(Vec<Node>, Span),
    /// Key order preserved; keys are scalars-as-strings with spans.
    Map(Vec<((String, Span), Node)>, Span),
    Null(Span),
}

impl Node {
    pub fn span(&self) -> Span {
        match self {
            Node::Scalar(_, s) | Node::Seq(_, s) | Node::Map(_, s) | Node::Null(s) => *s,
        }
    }
}

pub fn load(src: &str) -> Result<Node, Diagnostics> {
    let docs = MarkedYaml::load_from_str(src).map_err(|e| {
        let mut ds = Diagnostics::default();
        ds.push(Diagnostic::error("yaml-syntax", e.to_string()));
        ds
    })?;
    let doc = docs.into_iter().next().ok_or_else(|| {
        let mut ds = Diagnostics::default();
        ds.push(Diagnostic::error("yaml-syntax", "empty document"));
        ds
    })?;
    Ok(convert(doc))
}

fn mark_span(node: &MarkedYaml) -> Span {
    // saphyr markers are 1-based line, 0-based col (verify per version)
    Span { line: node.span.start.line(), col: node.span.start.col() + 1 }
}

fn scalar_string(data: &YamlData<MarkedYaml>) -> Option<String> {
    match data {
        YamlData::Value(s) => Some(s.to_string()), // Scalar's Display = literal form
        _ => None,
    }
}

fn convert(node: MarkedYaml) -> Node {
    let span = mark_span(&node);
    match node.data {
        YamlData::Mapping(m) => {
            let mut entries = Vec::new();
            for (k, v) in m {
                let kspan = mark_span(&k);
                let key = scalar_string(&k.data).unwrap_or_default();
                entries.push(((key, kspan), convert(v)));
            }
            Node::Map(entries, span)
        }
        YamlData::Sequence(s) => Node::Seq(s.into_iter().map(convert).collect(), span),
        ref d if scalar_string(d).is_some() => {
            Node::Scalar(scalar_string(&node.data).unwrap(), span)
        }
        _ => Node::Null(span),
    }
}
```

(If the installed saphyr names differ — e.g. `Hash` instead of `Mapping`, or scalar variants instead of `Value(Scalar)` — adjust **only this file** until `cargo test` compiles; the `Scalar`-to-literal-string rule is the invariant to preserve: `4.7k`, `NO`, `1`, `true` must all come out as their source strings.)

- [ ] **Step 4: Implement the parser** — `crates/circuit-lang/src/parse.rs`:

```rust
//! Strict walker: yaml::Node -> SurfaceDesign. Unknown keys are errors
//! with did-you-mean suggestions (spec §5.3.6).

use crate::diag::{Diagnostic, Diagnostics, Span};
use crate::model::{Edge, LayoutHint};
use crate::surface::*;
use crate::yaml::{self, Node};
use indexmap::IndexMap;

pub fn parse_str(src: &str) -> (Option<SurfaceDesign>, Diagnostics) {
    let mut diags = Diagnostics::default();
    let root = match yaml::load(src) {
        Ok(n) => n,
        Err(ds) => return (None, ds),
    };
    let mut p = Parser { diags: &mut diags };
    let design = p.design(&root);
    (design, diags)
}

struct Parser<'a> {
    diags: &'a mut Diagnostics,
}

fn suggest(key: &str, allowed: &[&str]) -> Option<String> {
    allowed
        .iter()
        .map(|a| (strsim::levenshtein(key, a), *a))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, a)| a.to_string())
}

impl Parser<'_> {
    fn err(&mut self, code: &'static str, msg: String, span: Span) {
        self.diags.push(Diagnostic::error(code, msg).with_span(span));
    }

    /// Strict map access: every key must be in `allowed`.
    fn check_keys(&mut self, map: &[((String, Span), Node)], allowed: &[&str], ctx: &str) {
        for ((k, kspan), _) in map {
            if !allowed.contains(&k.as_str()) {
                let mut d = Diagnostic::error(
                    "unknown-key",
                    format!("unknown key `{k}` in {ctx}"),
                )
                .with_span(*kspan);
                if let Some(s) = suggest(k, allowed) {
                    d = d.with_suggestion(s);
                }
                self.diags.push(d);
            }
        }
    }

    fn get<'n>(map: &'n [((String, Span), Node)], key: &str) -> Option<&'n Node> {
        map.iter().find(|((k, _), _)| k == key).map(|(_, v)| v)
    }

    fn scalar(&mut self, n: &Node, ctx: &str) -> Option<String> {
        match n {
            Node::Scalar(s, _) => Some(s.clone()),
            _ => {
                self.err("expected-scalar", format!("expected a scalar for {ctx}"), n.span());
                None
            }
        }
    }

    fn map_node<'n>(&mut self, n: &'n Node, ctx: &str) -> Option<&'n [((String, Span), Node)]> {
        match n {
            Node::Map(m, _) => Some(m),
            _ => {
                self.err("expected-map", format!("expected a mapping for {ctx}"), n.span());
                None
            }
        }
    }

    fn design(&mut self, root: &Node) -> Option<SurfaceDesign> {
        let map = self.map_node(root, "top level")?;
        self.check_keys(
            map,
            &["version", "name", "description", "rails", "blocks", "nets"],
            "top level",
        );
        // version: required, == 1
        match Self::get(map, "version").and_then(|n| match n {
            Node::Scalar(s, _) => Some(s.clone()),
            _ => None,
        }) {
            Some(v) if v == "1" => {}
            _ => self.diags.push(Diagnostic::error(
                "bad-version",
                "`version: 1` is required at top level",
            )),
        }

        let mut d = SurfaceDesign::default();
        d.name = Self::get(map, "name").and_then(|n| self.scalar(n, "name"));
        d.description = Self::get(map, "description").and_then(|n| self.scalar(n, "description"));

        if let Some(Node::Seq(items, _)) = Self::get(map, "rails") {
            for it in items {
                if let Some(s) = self.scalar(it, "rails entry") {
                    self.check_net_name(&s, it.span());
                    d.rails.push((s, it.span()));
                }
            }
        } else if let Some(n) = Self::get(map, "rails") {
            self.err("expected-seq", "`rails` must be a list of net names".into(), n.span());
        }

        match Self::get(map, "blocks") {
            Some(n) => {
                if let Some(bm) = self.map_node(n, "blocks") {
                    for ((bname, bspan), bnode) in bm {
                        if !bname.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                            self.err("bad-block-name",
                                format!("block `{bname}` must be lower_snake"), *bspan);
                        }
                        if let Some(b) = self.block(bnode) {
                            d.blocks.insert(bname.clone(), b);
                        }
                    }
                }
                if d.blocks.is_empty() {
                    self.diags.push(Diagnostic::error("no-blocks", "at least one block is required"));
                }
            }
            None => self.diags.push(Diagnostic::error("no-blocks", "`blocks:` is required")),
        }

        if let Some(n) = Self::get(map, "nets") {
            if let Some(nm) = self.map_node(n, "nets") {
                for ((net, nspan), nnode) in nm {
                    self.check_net_name(net, *nspan);
                    d.nets.insert(net.clone(), self.net_attrs(nnode));
                }
            }
        }
        Some(d)
    }

    fn check_net_name(&mut self, name: &str, span: Span) {
        if name.contains(' ') || name.contains('/') {
            self.err("bad-net-name",
                format!("net `{name}`: spaces forbidden, `/` reserved for hierarchy"), span);
        }
    }

    fn net_attrs(&mut self, n: &Node) -> SurfaceNet {
        let mut out = SurfaceNet { span: Some(n.span()), ..Default::default() };
        if let Some(m) = self.map_node(n, "net attributes") {
            self.check_keys(m, &["power", "class"], "net attributes");
            if let Some(p) = Self::get(m, "power").and_then(|v| self.scalar(v, "power")) {
                out.power = p == "true";
            }
            out.class = Self::get(m, "class").and_then(|v| self.scalar(v, "class"));
        }
        out
    }

    fn block(&mut self, n: &Node) -> Option<SurfaceBlock> {
        let m = self.map_node(n, "block")?;
        self.check_keys(m, &["note", "layout", "components"], "block");
        let mut b = SurfaceBlock { span: Some(n.span()), ..Default::default() };
        b.note = Self::get(m, "note").and_then(|v| self.scalar(v, "note"));
        if let Some(l) = Self::get(m, "layout") {
            b.layout = self.layout(l);
        }
        if let Some(cn) = Self::get(m, "components") {
            if let Some(cm) = self.map_node(cn, "components") {
                for ((refdes, rspan), cnode) in cm {
                    let ok = refdes.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                        && refdes.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                        && refdes.chars().last().is_some_and(|c| c.is_ascii_digit());
                    if !ok {
                        self.err("bad-refdes",
                            format!("`{refdes}` is not a valid refdes (expected e.g. U1, R10)"), *rspan);
                    }
                    if let Some(c) = self.component(cnode) {
                        b.components.insert(refdes.clone(), c);
                    }
                }
            }
        }
        Some(b)
    }

    fn layout(&mut self, n: &Node) -> LayoutHint {
        let mut h = LayoutHint::default();
        if let Some(m) = self.map_node(n, "layout") {
            self.check_keys(m, &["edge", "near"], "layout");
            if let Some(e) = Self::get(m, "edge").and_then(|v| self.scalar(v, "edge")) {
                h.edge = match e.as_str() {
                    "left" => Some(Edge::Left),
                    "right" => Some(Edge::Right),
                    "top" => Some(Edge::Top),
                    "bottom" => Some(Edge::Bottom),
                    other => {
                        self.err("bad-edge",
                            format!("`{other}` is not an edge (left|right|top|bottom)"), n.span());
                        None
                    }
                };
            }
            h.near = Self::get(m, "near").and_then(|v| self.scalar(v, "near"));
        }
        h
    }

    fn pin_map(&mut self, n: &Node, out: &mut IndexMap<String, (String, Span)>) {
        if let Some(m) = self.map_node(n, "pins") {
            for ((pin, pspan), v) in m {
                if let Some(t) = self.scalar(v, "pin target") {
                    if t != "nc" && !t.eq_ignore_ascii_case("nc") && !t.contains('.') {
                        self.check_net_name(&t, v.span());
                    }
                    if out.insert(pin.clone(), (t, v.span())).is_some() {
                        self.err("pin-conflict",
                            format!("pin `{pin}` mapped more than once"), *pspan);
                    }
                }
            }
        }
    }

    fn component(&mut self, n: &Node) -> Option<SurfaceComponent> {
        let m = self.map_node(n, "component")?;
        self.check_keys(
            m,
            &["part", "value", "footprint", "dnp", "props", "pins", "units", "between", "decouple"],
            "component",
        );
        let mut c = SurfaceComponent { span: Some(n.span()), ..Default::default() };
        match Self::get(m, "part").and_then(|v| self.scalar(v, "part")) {
            Some(p) => c.part = p,
            None => self.err("missing-part", "`part:` is required".into(), n.span()),
        }
        c.value = Self::get(m, "value").and_then(|v| self.scalar(v, "value"));
        c.footprint = Self::get(m, "footprint").and_then(|v| self.scalar(v, "footprint"));
        if let Some(d) = Self::get(m, "dnp").and_then(|v| self.scalar(v, "dnp")) {
            c.dnp = d == "true";
        }
        if let Some(Node::Map(pm, _)) = Self::get(m, "props") {
            for ((k, _), v) in pm {
                if let Some(s) = self.scalar(v, "prop value") {
                    c.props.insert(k.clone(), s);
                }
            }
        }
        if let Some(pn) = Self::get(m, "pins") {
            let mut pins = IndexMap::new();
            self.pin_map(pn, &mut pins);
            c.pins = pins;
        }
        if let Some(un) = Self::get(m, "units") {
            if let Some(um) = self.map_node(un, "units") {
                for ((uname, _), unode) in um {
                    if let Some(uim) = self.map_node(unode, "unit") {
                        self.check_keys(uim, &["pins"], "unit");
                        let mut pins = IndexMap::new();
                        if let Some(pn) = Self::get(uim, "pins") {
                            self.pin_map(pn, &mut pins);
                        }
                        c.units.insert(uname.clone(), pins);
                    }
                }
            }
        }
        if let Some(bn) = Self::get(m, "between") {
            match bn {
                Node::Seq(items, _) if items.len() == 2 => {
                    let a = self.scalar(&items[0], "between[0]");
                    let b = self.scalar(&items[1], "between[1]");
                    if let (Some(a), Some(b)) = (a, b) {
                        c.between = Some(((a, items[0].span()), (b, items[1].span())));
                    }
                }
                _ => self.err("bad-between",
                    "`between` must be a 2-element list".into(), bn.span()),
            }
        }
        if let Some(dn) = Self::get(m, "decouple") {
            if let Some(dm) = self.map_node(dn, "decouple") {
                for ((val, vspan), cnt) in dm {
                    match self.scalar(cnt, "decouple count").and_then(|s| s.parse::<u32>().ok()) {
                        Some(k) if k >= 1 => {
                            c.decouple.insert(val.clone(), k);
                        }
                        _ => self.err("bad-decouple",
                            format!("decouple count for `{val}` must be a positive integer"), *vspan),
                    }
                }
            }
        }
        Some(c)
    }
}
```

In `lib.rs` add:
```rust
pub mod parse;
mod yaml;
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p circuit-lang`
Expected: all tests pass. If `yaml.rs` fails to compile against the installed saphyr, fix names in that file only (see Step 3 note).

- [ ] **Step 6: Commit**

```bash
git add crates/circuit-lang && git commit -m "feat(circuit-lang): strict YAML parser to surface AST with did-you-mean"
```

---

### Task 5: `SymbolProvider` trait + mock

**Files:**
- Create: `crates/circuit-lang/src/provider.rs`
- Modify: `crates/circuit-lang/src/lib.rs` (add `pub mod provider;`)

Desugar (`between`) and lints need symbol metadata, but circuit-lang is pure — so the data is injected. `kicad-bridge` implements this trait against real libraries in Plan 2. The mock ships in the crate (not `#[cfg(test)]`) because later crates' tests reuse it.

- [ ] **Step 1: Write the failing test** — bottom of `crates/circuit-lang/src/provider.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_provider_serves_symbols_and_suggestions() {
        let p = MockSymbolProvider::with_basics(); // includes Device:R, Device:C
        let r = p.symbol("Device:R").unwrap();
        assert_eq!(r.pins.len(), 2);
        assert!(p.symbol("Device:Q").is_none());
        assert_eq!(p.suggest("Device:r"), vec!["Device:R".to_string()]);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p circuit-lang`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** — `crates/circuit-lang/src/provider.rs`:

```rust
//! Symbol metadata injection point. Implemented by kicad-bridge
//! against real .kicad_sym libraries; mocked here for tests.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinType {
    PowerInput,
    PowerOutput,
    Passive,
    Other,
}

#[derive(Debug, Clone)]
pub struct PinMeta {
    pub number: String,
    pub name: String,
    pub etype: PinType,
    pub unit: u8, // 1-based; 1 for single-unit symbols
}

#[derive(Debug, Clone, Default)]
pub struct SymbolMeta {
    pub pins: Vec<PinMeta>,
}

pub trait SymbolProvider {
    fn symbol(&self, lib_id: &str) -> Option<&SymbolMeta>;
    /// Closest known lib_ids for an unknown one (for diagnostics).
    fn suggest(&self, lib_id: &str) -> Vec<String>;
}

#[derive(Default)]
pub struct MockSymbolProvider {
    symbols: HashMap<String, SymbolMeta>,
}

impl MockSymbolProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, lib_id: &str, pins: Vec<(&str, &str, PinType, u8)>) -> &mut Self {
        let pins = pins
            .into_iter()
            .map(|(number, name, etype, unit)| PinMeta {
                number: number.into(),
                name: name.into(),
                etype,
                unit,
            })
            .collect();
        self.symbols.insert(lib_id.into(), SymbolMeta { pins });
        self
    }

    /// Device:R / Device:C / Device:D / Device:LED — enough for most tests.
    pub fn with_basics() -> Self {
        use PinType::*;
        let mut p = Self::new();
        for id in ["Device:R", "Device:C", "Device:L"] {
            p.add(id, vec![("1", "~", Passive, 1), ("2", "~", Passive, 1)]);
        }
        p.add("Device:D", vec![("1", "K", Passive, 1), ("2", "A", Passive, 1)]);
        p.add("Device:LED", vec![("1", "K", Passive, 1), ("2", "A", Passive, 1)]);
        p
    }
}

impl SymbolProvider for MockSymbolProvider {
    fn symbol(&self, lib_id: &str) -> Option<&SymbolMeta> {
        self.symbols.get(lib_id)
    }
    fn suggest(&self, lib_id: &str) -> Vec<String> {
        let mut hits: Vec<(usize, &String)> = self
            .symbols
            .keys()
            .map(|k| (strsim::levenshtein(&lib_id.to_lowercase(), &k.to_lowercase()), k))
            .filter(|(d, _)| *d <= 3)
            .collect();
        hits.sort();
        hits.into_iter().take(3).map(|(_, k)| k.clone()).collect()
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p circuit-lang`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/circuit-lang && git commit -m "feat(circuit-lang): SymbolProvider trait with mock implementation"
```

---

### Task 6: Desugar skeleton — part aliases, rails, `nc`, plain nets

**Files:**
- Create: `crates/circuit-lang/src/desugar.rs`
- Modify: `crates/circuit-lang/src/lib.rs` (add `pub mod desugar;`)

The desugar pipeline order (fixed; later tasks slot into it): **aliases → rails → between → pin resolution → decouple**. This task builds the skeleton with aliases, rails, and a simple pin resolution (no pin-refs yet — Task 8 replaces it).

- [ ] **Step 1: Write the failing test** — bottom of `crates/circuit-lang/src/desugar.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PinTarget;
    use crate::parse::parse_str;
    use crate::provider::MockSymbolProvider;

    pub(crate) fn run(src: &str) -> (crate::model::Design, crate::diag::Diagnostics) {
        let (s, mut diags) = parse_str(src);
        let (d, ds) = desugar(&s.expect("parse failed"), &MockSymbolProvider::with_basics());
        diags.extend(ds);
        (d, diags)
    }

    #[test]
    fn aliases_rails_and_nc() {
        let (d, diags) = run("
version: 1
rails: [3V3, GND]
blocks:
  main:
    components:
      R1: {part: R, pins: {1: 3V3, 2: OUT}}
      U1: {part: M:X, pins: {EN: nc}}
");
        assert!(!diags.has_errors(), "{:?}", diags);
        assert!(d.nets["3V3"].power);
        assert!(d.nets["GND"].power);
        let main = &d.blocks["main"];
        assert_eq!(main.components["R1"].part, "Device:R");
        assert_eq!(main.components["U1"].part, "M:X"); // full lib_id passthrough
        assert_eq!(main.components["R1"].pins["1"], PinTarget::Net("3V3".into()));
        assert_eq!(main.components["U1"].pins["EN"], PinTarget::NoConnect);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p circuit-lang`
Expected: FAIL to compile — `desugar` not found.

- [ ] **Step 3: Implement** — `crates/circuit-lang/src/desugar.rs`:

```rust
//! Sugar -> kernel lowering (spec §5.5). The reconciler and lints see
//! only the output of this pass.

use crate::diag::{Diagnostic, Diagnostics};
use crate::model::*;
use crate::provider::SymbolProvider;
use crate::surface::*;
use indexmap::IndexMap;

/// Closed alias table (spec §5.5) — exactly these five.
fn alias(part: &str) -> String {
    match part {
        "R" => "Device:R".into(),
        "C" => "Device:C".into(),
        "L" => "Device:L".into(),
        "D" => "Device:D".into(),
        "LED" => "Device:LED".into(),
        other => other.into(),
    }
}

pub fn desugar(s: &SurfaceDesign, provider: &dyn SymbolProvider) -> (Design, Diagnostics) {
    let mut diags = Diagnostics::default();
    let mut d = Design {
        name: s.name.clone(),
        description: s.description.clone(),
        ..Default::default()
    };

    // rails -> power net attrs
    for (rail, _span) in &s.rails {
        d.nets.entry(rail.clone()).or_default().power = true;
    }
    for (net, attrs) in &s.nets {
        let e = d.nets.entry(net.clone()).or_default();
        e.power |= attrs.power;
        e.class = attrs.class.clone();
    }

    // surface components -> kernel components (pins still raw, resolved below)
    let mut raw_pins: Vec<RawPin> = Vec::new();
    for (bname, sb) in &s.blocks {
        let mut block = Block {
            note: sb.note.clone(),
            layout: sb.layout.clone(),
            components: IndexMap::new(),
        };
        for (refdes, sc) in &sb.components {
            let mut sc = sc.clone();
            apply_between(refdes, &mut sc, provider, &mut diags); // Task 7
            let comp = Component {
                part: alias(&sc.part),
                value: sc.value.clone(),
                footprint: sc.footprint.clone(),
                dnp: sc.dnp,
                props: sc.props.clone(),
                origin: Origin::Authored,
                ..Default::default()
            };
            for (pin, (target, span)) in &sc.pins {
                raw_pins.push(RawPin {
                    block: bname.clone(),
                    refdes: refdes.clone(),
                    unit: None,
                    pin: pin.clone(),
                    target: target.clone(),
                    span: *span,
                });
            }
            for (unit, pins) in &sc.units {
                for (pin, (target, span)) in pins {
                    raw_pins.push(RawPin {
                        block: bname.clone(),
                        refdes: refdes.clone(),
                        unit: Some(unit.clone()),
                        pin: pin.clone(),
                        target: target.clone(),
                        span: *span,
                    });
                }
            }
            block.components.insert(refdes.clone(), comp);
        }
        d.blocks.insert(bname.clone(), block);
    }

    resolve_pins(&mut d, raw_pins, &mut diags);
    synth_decouple(&mut d, s, &mut diags); // Task 8

    (d, diags)
}

struct RawPin {
    block: String,
    refdes: String,
    unit: Option<String>,
    pin: String,
    target: String,
    span: crate::diag::Span,
}

/// Task 7 fills this in. No-op until then.
fn apply_between(
    _refdes: &str,
    _sc: &mut SurfaceComponent,
    _provider: &dyn SymbolProvider,
    _diags: &mut Diagnostics,
) {
}

/// Task 8 replaces this with pin-ref-aware resolution. For now:
/// `nc` -> NoConnect, anything else -> a net name.
fn resolve_pins(d: &mut Design, raw: Vec<RawPin>, _diags: &mut Diagnostics) {
    for rp in raw {
        let target = if rp.target.eq_ignore_ascii_case("nc") {
            PinTarget::NoConnect
        } else {
            PinTarget::Net(rp.target.clone())
        };
        write_pin(d, &rp, target);
    }
}

fn write_pin(d: &mut Design, rp: &RawPin, target: PinTarget) {
    let comp = d
        .blocks
        .get_mut(&rp.block)
        .and_then(|b| b.components.get_mut(&rp.refdes))
        .expect("raw pin refers to existing component");
    match &rp.unit {
        Some(u) => {
            comp.units.entry(u.clone()).or_default().insert(rp.pin.clone(), target);
        }
        None => {
            comp.pins.insert(rp.pin.clone(), target);
        }
    }
}

/// Task 8 fills this in. No-op until then.
fn synth_decouple(_d: &mut Design, _s: &SurfaceDesign, _diags: &mut Diagnostics) {}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p circuit-lang`
Expected: all pass (compiler will warn about unused params in the stubs — fine for now).

- [ ] **Step 5: Commit**

```bash
git add crates/circuit-lang && git commit -m "feat(circuit-lang): desugar skeleton with aliases, rails, nc"
```

---

### Task 7: `between` desugar

**Files:**
- Modify: `crates/circuit-lang/src/desugar.rs` (replace the `apply_between` stub; add tests)

- [ ] **Step 1: Write the failing tests** — add inside `mod tests` in `desugar.rs`:

```rust
    #[test]
    fn between_desugars_in_pin_number_order() {
        let (d, diags) = run("
version: 1
blocks:
  main:
    components:
      C1: {part: C, value: 10uF, between: [VBUS, GND]}
");
        assert!(!diags.has_errors(), "{:?}", diags);
        let c1 = &d.blocks["main"].components["C1"];
        assert_eq!(c1.pins["1"], PinTarget::Net("VBUS".into()));
        assert_eq!(c1.pins["2"], PinTarget::Net("GND".into()));
    }

    #[test]
    fn between_on_polarized_part_warns() {
        let (_, diags) = run("
version: 1
blocks:
  main:
    components:
      D1: {part: LED, between: [STATUS, GND]}
");
        assert!(!diags.has_errors());
        assert!(diags.0.iter().any(|d| d.code == "between-polarized"));
    }

    #[test]
    fn between_on_unknown_or_non_2pin_symbol_errors() {
        let (_, diags) = run("
version: 1
blocks:
  main:
    components:
      X1: {part: Nope:Nada, between: [A, B]}
");
        assert!(diags.0.iter().any(|d| d.code == "between-unknown-symbol"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p circuit-lang`
Expected: the three new tests FAIL (stub does nothing — pins empty / no diagnostics).

- [ ] **Step 3: Replace the `apply_between` stub:**

```rust
fn apply_between(
    refdes: &str,
    sc: &mut SurfaceComponent,
    provider: &dyn SymbolProvider,
    diags: &mut Diagnostics,
) {
    let Some(((a, aspan), (b, bspan))) = sc.between.take() else { return };
    let part = alias(&sc.part);
    let Some(meta) = provider.symbol(&part) else {
        let mut d = Diagnostic::error(
            "between-unknown-symbol",
            format!("{refdes}: cannot desugar `between` — unknown symbol `{part}`"),
        )
        .with_span(aspan);
        if let Some(s) = provider.suggest(&part).into_iter().next() {
            d = d.with_suggestion(s);
        }
        diags.push(d);
        return;
    };
    if meta.pins.len() != 2 {
        diags.push(
            Diagnostic::error(
                "between-arity",
                format!(
                    "{refdes}: `between` needs a 2-pin symbol; `{part}` has {} pins",
                    meta.pins.len()
                ),
            )
            .with_span(aspan),
        );
        return;
    }
    // Polarized-part lint (spec §5.5): warn, suggest named pins.
    let polarized = matches!(part.as_str(), "Device:D" | "Device:LED" | "Device:CP")
        || meta.pins.iter().any(|p| p.name == "A" || p.name == "K");
    if polarized {
        diags.push(
            Diagnostic::warning(
                "between-polarized",
                format!(
                    "{refdes}: `{part}` is polarized; `between` maps pin order ({}, {}) — \
                     prefer named pins {{{}: …, {}: …}}",
                    meta.pins[0].number, meta.pins[1].number, meta.pins[0].name, meta.pins[1].name
                ),
            )
            .with_span(aspan),
        );
    }
    for (pin, target, span) in [
        (meta.pins[0].number.clone(), a, aspan),
        (meta.pins[1].number.clone(), b, bspan),
    ] {
        if sc.pins.insert(pin.clone(), (target, span)).is_some() {
            diags.push(
                Diagnostic::error(
                    "pin-conflict",
                    format!("{refdes}: pin `{pin}` set by both `between` and `pins`"),
                )
                .with_span(span),
            );
        }
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p circuit-lang`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/circuit-lang && git commit -m "feat(circuit-lang): between desugar with polarity warning"
```

---

### Task 8: Pin-ref resolution + `decouple` synthesis

**Files:**
- Modify: `crates/circuit-lang/src/desugar.rs` (replace `resolve_pins` and `synth_decouple` stubs; add tests)

Pin-refs (`J1.CC1` as a net designator) resolve by connected components: pins joined by refs share a net; a group with a named net uses it; a group without one gets the deterministic name `N_<REF>_<PIN>` (lexicographically smallest member). A pin-ref to a previously unmapped pin **creates** that pin's mapping.

- [ ] **Step 1: Write the failing tests** — add inside `mod tests`:

```rust
    #[test]
    fn pin_ref_joins_existing_net() {
        let (d, diags) = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:X, pins: {PB6: I2C_SCL}}
      J2: {part: M:Conn, pins: {3: U1.PB6}}
");
        assert!(!diags.has_errors(), "{:?}", diags);
        assert_eq!(
            d.blocks["main"].components["J2"].pins["3"],
            PinTarget::Net("I2C_SCL".into())
        );
    }

    #[test]
    fn pin_ref_to_unmapped_pin_synthesizes_net_on_both_sides() {
        let (d, diags) = run("
version: 1
blocks:
  main:
    components:
      J1: {part: M:Usb, pins: {VBUS: VBUS}}
      R1: {part: R, between: [J1.CC1, GND]}
");
        assert!(!diags.has_errors(), "{:?}", diags);
        let main = &d.blocks["main"];
        assert_eq!(main.components["R1"].pins["1"], PinTarget::Net("N_J1_CC1".into()));
        assert_eq!(main.components["J1"].pins["CC1"], PinTarget::Net("N_J1_CC1".into()));
    }

    #[test]
    fn pin_ref_to_unknown_component_errors() {
        let (_, diags) = run("
version: 1
blocks:
  main:
    components:
      R1: {part: R, pins: {1: U9.PA0, 2: GND}}
");
        assert!(diags.0.iter().any(|d| d.code == "bad-pin-ref"));
    }

    #[test]
    fn decouple_synthesizes_tagged_caps() {
        let (d, diags) = run("
version: 1
rails: [3V3, GND]
blocks:
  mcu:
    components:
      U1: {part: M:CPU, decouple: {100nF: 2, 4.7uF: 1},
           pins: {VDD: 3V3, VSS: GND}}
");
        assert!(!diags.has_errors(), "{:?}", diags);
        let mcu = &d.blocks["mcu"];
        let caps: Vec<_> = mcu.components.iter()
            .filter(|(_, c)| matches!(c.origin, crate::model::Origin::Synthesized { .. }))
            .collect();
        assert_eq!(caps.len(), 3);
        let (key, c) = &caps[0];
        assert_eq!(*key, "__dec_U1_1");
        assert_eq!(c.part, "Device:C");
        assert_eq!(c.value.as_deref(), Some("100nF"));
        assert_eq!(c.pins["1"], PinTarget::Net("3V3".into()));
        assert_eq!(c.pins["2"], PinTarget::Net("GND".into()));
        assert_eq!(
            c.origin,
            crate::model::Origin::Synthesized { parent: "U1".into(), role: "decouple".into(), index: 1 }
        );
    }

    #[test]
    fn decouple_with_ambiguous_rails_errors() {
        let (_, diags) = run("
version: 1
blocks:
  mcu:
    components:
      U1: {part: M:CPU, decouple: {100nF: 1}, pins: {VDD: 3V3, VDDA: AVDD, VSS: GND}}
");
        assert!(diags.0.iter().any(|d| d.code == "decouple-ambiguous"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p circuit-lang`
Expected: new tests FAIL (stubs).

- [ ] **Step 3: Replace `resolve_pins` and `synth_decouple`:**

```rust
fn sanitize(pin: &str) -> String {
    pin.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

fn resolve_pins(d: &mut Design, raw: Vec<RawPin>, diags: &mut Diagnostics) {
    // refdes -> block (for pin-ref targets and on-demand pin creation)
    let comp_block: std::collections::HashMap<String, String> = d
        .blocks
        .iter()
        .flat_map(|(b, bl)| bl.components.keys().map(move |r| (r.clone(), b.clone())))
        .collect();

    // Union-find over pin nodes keyed by (refdes, pin).
    let mut nodes: Vec<(String, String)> = Vec::new();
    let mut index: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    let mut parent: Vec<usize> = Vec::new();
    let mut node = |r: &str, p: &str, nodes: &mut Vec<(String, String)>,
                    index: &mut std::collections::HashMap<(String, String), usize>,
                    parent: &mut Vec<usize>| -> usize {
        *index.entry((r.to_string(), p.to_string())).or_insert_with(|| {
            nodes.push((r.to_string(), p.to_string()));
            parent.push(nodes.len() - 1);
            nodes.len() - 1
        })
    };
    fn find(parent: &mut Vec<usize>, mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }

    let mut named: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let mut placement: Vec<(RawPin, usize)> = Vec::new(); // node idx per raw pin
    let mut extra_nodes: Vec<usize> = Vec::new(); // pin-ref targets (may be unmapped)

    for rp in raw {
        if rp.target.eq_ignore_ascii_case("nc") {
            write_pin(d, &rp, PinTarget::NoConnect);
            continue;
        }
        let i = node(&rp.refdes, &rp.pin, &mut nodes, &mut index, &mut parent);
        // pin-ref? "<REFDES>.<pin>" where REFDES exists
        let is_ref = rp.target.split_once('.').is_some_and(|(r, _)| comp_block.contains_key(r));
        if rp.target.contains('.') && !is_ref {
            diags.push(
                Diagnostic::error(
                    "bad-pin-ref",
                    format!(
                        "{}.{}: target `{}` looks like a pin-ref but no such component exists",
                        rp.refdes, rp.pin, rp.target
                    ),
                )
                .with_span(rp.span),
            );
            continue;
        }
        if is_ref {
            let (tr, tp) = rp.target.split_once('.').unwrap();
            let j = node(tr, tp, &mut nodes, &mut index, &mut parent);
            let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
            parent[ri] = rj;
            extra_nodes.push(j);
        } else {
            let root = find(&mut parent, i);
            named.insert(root, rp.target.clone());
        }
        placement.push((rp, i));
    }

    // consolidate names after all unions
    let mut group_name: std::collections::HashMap<usize, String> =
        std::collections::HashMap::new();
    for (i, name) in &named {
        let root = find(&mut parent, *i);
        if let Some(prev) = group_name.insert(root, name.clone()) {
            if &prev != name {
                diags.push(Diagnostic::error(
                    "net-conflict",
                    format!("nets `{prev}` and `{name}` joined by pin-refs"),
                ));
            }
        }
    }
    // unnamed groups: N_<smallest member>
    for i in 0..nodes.len() {
        let root = find(&mut parent, i);
        group_name.entry(root).or_insert_with(|| {
            let mut members: Vec<String> = (0..nodes.len())
                .filter(|&j| find(&mut parent, j) == root)
                .map(|j| format!("{}_{}", nodes[j].0, sanitize(&nodes[j].1)))
                .collect();
            members.sort();
            format!("N_{}", members[0])
        });
    }

    for (rp, i) in placement {
        let root = find(&mut parent, i);
        write_pin(d, &rp, PinTarget::Net(group_name[&root].clone()));
    }
    // pin-ref targets that had no own mapping: create one on the component
    for j in extra_nodes {
        let (r, p) = nodes[j].clone();
        let block = comp_block[&r].clone();
        let comp = d.blocks.get_mut(&block).unwrap().components.get_mut(&r).unwrap();
        let already = comp.pins.contains_key(&p)
            || comp.units.values().any(|u| u.contains_key(&p));
        if !already {
            let root = find(&mut parent, j);
            comp.pins.insert(p, PinTarget::Net(group_name[&root].clone()));
        }
    }
}

fn synth_decouple(d: &mut Design, s: &SurfaceDesign, diags: &mut Diagnostics) {
    for (bname, sb) in &s.blocks {
        for (refdes, sc) in &sb.components {
            if sc.decouple.is_empty() {
                continue;
            }
            let comp = &d.blocks[bname].components[refdes];
            let rail = |prefixes: &[&str]| -> Vec<NetName> {
                let mut nets: Vec<NetName> = comp
                    .pins
                    .iter()
                    .chain(comp.units.values().flatten())
                    .filter(|(k, _)| {
                        let k = k.to_ascii_uppercase();
                        prefixes.iter().any(|p| k.starts_with(p))
                    })
                    .filter_map(|(_, t)| match t {
                        PinTarget::Net(n) => Some(n.clone()),
                        PinTarget::NoConnect => None,
                    })
                    .collect();
                nets.sort();
                nets.dedup();
                nets
            };
            let vdd = rail(&["VDD", "VCC"]);
            let gnd = rail(&["VSS", "GND"]);
            if vdd.len() != 1 || gnd.len() != 1 {
                diags.push(Diagnostic::error(
                    "decouple-ambiguous",
                    format!(
                        "{refdes}: decouple needs exactly one VDD*/VCC* net and one \
                         VSS*/GND* net (found {vdd:?} / {gnd:?}) — write the caps explicitly"
                    ),
                ));
                continue;
            }
            let (vdd, gnd) = (vdd[0].clone(), gnd[0].clone());
            let mut idx = 0u32;
            let mut synths = Vec::new();
            for (value, count) in &sc.decouple {
                for _ in 0..*count {
                    idx += 1;
                    let mut c = Component {
                        part: "Device:C".into(),
                        value: Some(value.clone()),
                        origin: Origin::Synthesized {
                            parent: refdes.clone(),
                            role: "decouple".into(),
                            index: idx,
                        },
                        ..Default::default()
                    };
                    c.pins.insert("1".into(), PinTarget::Net(vdd.clone()));
                    c.pins.insert("2".into(), PinTarget::Net(gnd.clone()));
                    synths.push((format!("__dec_{refdes}_{idx}"), c));
                }
            }
            let block = d.blocks.get_mut(bname).unwrap();
            for (key, c) in synths {
                block.components.insert(key, c);
            }
        }
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p circuit-lang`
Expected: all pass (including all earlier tests — the simple-resolution test from Task 6 must still pass).

- [ ] **Step 5: Commit**

```bash
git add crates/circuit-lang && git commit -m "feat(circuit-lang): pin-ref resolution and decouple synthesis"
```

---

### Task 9: Lints

**Files:**
- Create: `crates/circuit-lang/src/lint.rs`
- Modify: `crates/circuit-lang/src/lib.rs` (add `pub mod lint;`)

Implements spec §6's semantic checks: unknown-part (+suggest), unknown-pin (+did-you-mean), power-input-connected, physical pin-conflict, single-pin-net, near-name, unreferenced-declared-net.

- [ ] **Step 1: Write the failing tests** — bottom of `crates/circuit-lang/src/lint.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::desugar::desugar;
    use crate::parse::parse_str;
    use crate::provider::{MockSymbolProvider, PinType};

    fn provider() -> MockSymbolProvider {
        use PinType::*;
        let mut p = MockSymbolProvider::with_basics();
        p.add("M:CPU", vec![
            ("1", "VDD", PowerInput, 1), ("2", "VDD", PowerInput, 1), // stacked
            ("3", "VSS", PowerInput, 1),
            ("4", "PB6", Other, 1), ("5", "PB7", Other, 1),
        ]);
        p
    }

    fn run(src: &str) -> crate::diag::Diagnostics {
        let p = provider();
        let (s, mut diags) = parse_str(src);
        let (d, ds) = desugar(&s.unwrap(), &p);
        diags.extend(ds);
        diags.extend(lint(&d, &p));
        diags
    }

    #[test]
    fn unknown_part_and_pin_get_suggestions() {
        let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: V, VSS: G, PB66: X, PB7: X}}
      U2: {part: M:CPX, pins: {}}
");
        let pin = diags.0.iter().find(|d| d.code == "unknown-pin").unwrap();
        assert_eq!(pin.suggestion.as_deref(), Some("PB6"));
        let part = diags.0.iter().find(|d| d.code == "unknown-part").unwrap();
        assert_eq!(part.suggestion.as_deref(), Some("M:CPU"));
    }

    #[test]
    fn unconnected_power_input_is_an_error() {
        let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: 3V3, PB6: X, PB7: X}}
"); // VSS missing
        let e = diags.0.iter().find(|d| d.code == "power-pin-unconnected").unwrap();
        assert!(e.message.contains("VSS"));
    }

    #[test]
    fn warnings_single_pin_near_name_unreferenced() {
        let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: 3V3, VSS: GND, PB6: I2C_SDA, PB7: I2C1_SDA}}
      R1: {part: R, between: [I2C_SDA, 3V3]}
nets:
  UNUSED: {class: x}
");
        assert!(diags.0.iter().any(|d| d.code == "single-pin-net")); // I2C1_SDA
        assert!(diags.0.iter().any(|d| d.code == "near-name"));      // I2C_SDA vs I2C1_SDA
        assert!(diags.0.iter().any(|d| d.code == "unreferenced-net"));
        assert!(!diags.has_errors());
    }

    #[test]
    fn stacked_power_name_counts_as_connected() {
        let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: 3V3, VSS: GND, PB6: A, PB7: B}}
      R1: {part: R, between: [A, B]}
");
        assert!(!diags.has_errors(), "{:?}", diags); // VDD name covers pins 1 AND 2
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p circuit-lang`
Expected: FAIL to compile — `lint` not found.

- [ ] **Step 3: Implement** — `crates/circuit-lang/src/lint.rs`:

```rust
//! Semantic lints over the kernel model (spec §6).

use crate::diag::{Diagnostic, Diagnostics};
use crate::model::*;
use crate::provider::{PinType, SymbolProvider};

pub fn lint(d: &Design, provider: &dyn SymbolProvider) -> Diagnostics {
    let mut diags = Diagnostics::default();
    let mut net_pins: indexmap::IndexMap<&str, Vec<String>> = indexmap::IndexMap::new();

    for (_bname, block) in &d.blocks {
        for (refdes, comp) in block.components.iter() {
            let all_pins = comp.pins.iter().chain(comp.units.values().flatten());
            for (key, target) in all_pins.clone() {
                if let PinTarget::Net(n) = target {
                    net_pins.entry(n.as_str()).or_default().push(format!("{refdes}.{key}"));
                }
            }

            let Some(meta) = provider.symbol(&comp.part) else {
                let mut e = Diagnostic::error(
                    "unknown-part",
                    format!("{refdes}: symbol `{}` not found in any library", comp.part),
                );
                if let Some(s) = provider.suggest(&comp.part).into_iter().next() {
                    e = e.with_suggestion(s);
                }
                diags.push(e);
                continue; // pin checks impossible without the symbol
            };

            // Resolve each map key to physical pins: exact number, else name.
            let mut covered: std::collections::HashMap<&str, &str> = Default::default(); // number -> key
            for (key, _) in all_pins.clone() {
                let by_number: Vec<&crate::provider::PinMeta> =
                    meta.pins.iter().filter(|p| p.number == *key).collect();
                let hits = if by_number.is_empty() {
                    meta.pins.iter().filter(|p| p.name == *key).collect()
                } else {
                    by_number
                };
                if hits.is_empty() {
                    let names: Vec<&str> = meta
                        .pins
                        .iter()
                        .flat_map(|p| [p.name.as_str(), p.number.as_str()])
                        .collect();
                    let mut e = Diagnostic::error(
                        "unknown-pin",
                        format!("pin `{key}` not found on {refdes} ({})", comp.part),
                    );
                    if let Some(s) = names
                        .iter()
                        .map(|n| (strsim::levenshtein(key, n), *n))
                        .filter(|(d, _)| *d <= 2)
                        .min_by_key(|(d, _)| *d)
                    {
                        e = e.with_suggestion(s.1);
                    }
                    diags.push(e);
                }
                for p in hits {
                    if let Some(prev) = covered.insert(&p.number, key) {
                        if prev != key.as_str() {
                            diags.push(Diagnostic::error(
                                "pin-conflict",
                                format!(
                                    "{refdes}: physical pin {} claimed by both `{prev}` and `{key}`",
                                    p.number
                                ),
                            ));
                        }
                    }
                }
            }

            // Every power-input pin must be covered AND on a net.
            for p in meta.pins.iter().filter(|p| p.etype == PinType::PowerInput) {
                let on_net = covered.get(p.number.as_str()).is_some_and(|key| {
                    comp.pins
                        .get(*key)
                        .or_else(|| comp.units.values().find_map(|u| u.get(*key)))
                        .is_some_and(|t| matches!(t, PinTarget::Net(_)))
                });
                if !on_net {
                    diags.push(Diagnostic::error(
                        "power-pin-unconnected",
                        format!(
                            "{refdes}: power-input pin {} ({}) is not connected to a net",
                            p.number, p.name
                        ),
                    ));
                }
            }
        }
    }

    for (net, pins) in &net_pins {
        if pins.len() == 1 && !d.nets.get(*net).map(|a| a.power).unwrap_or(false) {
            diags.push(Diagnostic::warning(
                "single-pin-net",
                format!("net `{net}` has only one pin ({}) — typo?", pins[0]),
            ));
        }
    }
    let names: Vec<&str> = net_pins.keys().copied().collect();
    for (i, a) in names.iter().enumerate() {
        for b in &names[i + 1..] {
            if strsim::levenshtein(a, b) == 1 {
                diags.push(Diagnostic::warning(
                    "near-name",
                    format!("nets `{a}` and `{b}` differ by one character — intentional?"),
                ));
            }
        }
    }
    for (net, _) in &d.nets {
        if !net_pins.contains_key(net.as_str()) {
            diags.push(Diagnostic::warning(
                "unreferenced-net",
                format!("net `{net}` is declared in `nets:` but no pin references it"),
            ));
        }
    }
    diags
}
```

Also in `desugar.rs`, make `mod tests`'s `run` helper `pub(crate)` if not already (the lint tests reuse the pattern but define their own — no change needed if compile is clean).

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p circuit-lang`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/circuit-lang && git commit -m "feat(circuit-lang): semantic lints with self-repair suggestions"
```

---

### Task 10: Canonical emitter + idempotence

**Files:**
- Create: `crates/circuit-lang/src/canon.rs`
- Modify: `crates/circuit-lang/src/lib.rs` (add `pub mod canon;`)

Canonical form (spec §5.5): kernel-only, **except** role-tagged synthesized components re-sugar (`decouple:` on the parent; `__dec_*` entries omitted). Ordering: blocks in design order, components by natural refdes sort (`U2` < `U10`), pin keys natural-sorted, nets alphabetical. Deviation from spec noted: spec says "pins in symbol pin order" — that needs a provider; circuit-lang canon uses natural key sort for provider-free determinism, and the engine's lift (Plan 3) may reorder. Same state ⇒ byte-identical output.

- [ ] **Step 1: Write the failing tests** — bottom of `crates/circuit-lang/src/canon.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::desugar::desugar;
    use crate::parse::parse_str;
    use crate::provider::MockSymbolProvider;

    const SRC: &str = "
version: 1
name: t
rails: [3V3, GND]
blocks:
  mcu:
    layout: {edge: right}
    components:
      U1: {part: M:CPU, decouple: {100nF: 2}, pins: {VDD: 3V3, VSS: GND, PB6: SCL}}
      R7: {part: R, value: 4.7k, between: [SCL, 3V3]}
";

    fn compile(src: &str) -> crate::model::Design {
        let p = MockSymbolProvider::with_basics();
        let (s, diags) = parse_str(src);
        assert!(!diags.has_errors(), "{diags:?}");
        // M:CPU unknown to provider — between only needs Device:R; ok here
        let (d, ds) = desugar(&s.unwrap(), &p);
        assert!(!ds.has_errors(), "{ds:?}");
        d
    }

    #[test]
    fn canonical_resugars_decouple_and_is_idempotent() {
        let d1 = compile(SRC);
        let out1 = to_canonical_yaml(&d1);
        assert!(out1.contains("decouple: {100nF: 2}"));
        assert!(!out1.contains("__dec_"));
        let d2 = compile(&out1);
        assert_eq!(d1, d2, "canonical round-trip must preserve the kernel model");
        assert_eq!(out1, to_canonical_yaml(&d2), "canonical emit must be a fixpoint");
    }

    #[test]
    fn natural_refdes_ordering() {
        assert!(natural_lt("U2", "U10"));
        assert!(natural_lt("C9", "C12"));
        assert!(!natural_lt("R10", "R2"));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p circuit-lang`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** — `crates/circuit-lang/src/canon.rs`:

```rust
//! Deterministic canonical YAML emission of the kernel model.

use crate::model::*;
use indexmap::IndexMap;
use std::fmt::Write;

/// Quote a YAML scalar only when needed.
fn q(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars().next().unwrap().is_ascii_alphanumeric()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "_.+:~/-".contains(c));
    if safe { s.to_string() } else { format!("'{}'", s.replace('\'', "''")) }
}

/// Natural sort: alpha prefix, then numeric suffix (U2 < U10).
pub fn natural_lt(a: &str, b: &str) -> bool {
    fn split(s: &str) -> (&str, u64) {
        let i = s.find(|c: char| c.is_ascii_digit()).unwrap_or(s.len());
        (&s[..i], s[i..].parse().unwrap_or(0))
    }
    split(a) < split(b)
}

fn sorted<'a, V>(m: &'a IndexMap<String, V>) -> Vec<(&'a String, &'a V)> {
    let mut v: Vec<_> = m.iter().collect();
    v.sort_by(|(a, _), (b, _)| {
        if natural_lt(a, b) { std::cmp::Ordering::Less } else if natural_lt(b, a) { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Equal }
    });
    v
}

fn pin_map_inline(pins: &IndexMap<String, PinTarget>) -> String {
    let parts: Vec<String> = sorted(pins)
        .into_iter()
        .map(|(k, t)| match t {
            PinTarget::Net(n) => format!("{}: {}", q(k), q(n)),
            PinTarget::NoConnect => format!("{}: nc", q(k)),
        })
        .collect();
    format!("{{{}}}", parts.join(", "))
}

pub fn to_canonical_yaml(d: &Design) -> String {
    let mut o = String::new();
    o.push_str("version: 1\n");
    if let Some(n) = &d.name {
        writeln!(o, "name: {}", q(n)).unwrap();
    }
    if let Some(desc) = &d.description {
        writeln!(o, "description: {}", q(desc)).unwrap();
    }
    o.push_str("blocks:\n");
    for (bname, block) in &d.blocks {
        writeln!(o, "  {}:", q(bname)).unwrap();
        if let Some(note) = &block.note {
            writeln!(o, "    note: {}", q(note)).unwrap();
        }
        let mut hints = Vec::new();
        if let Some(e) = block.layout.edge {
            hints.push(format!(
                "edge: {}",
                match e { Edge::Left => "left", Edge::Right => "right", Edge::Top => "top", Edge::Bottom => "bottom" }
            ));
        }
        if let Some(nb) = &block.layout.near {
            hints.push(format!("near: {}", q(nb)));
        }
        if !hints.is_empty() {
            writeln!(o, "    layout: {{{}}}", hints.join(", ")).unwrap();
        }
        o.push_str("    components:\n");

        // Re-sugar: collect decouple synths per parent (value -> count).
        let mut decouple: IndexMap<&str, IndexMap<&str, u32>> = IndexMap::new();
        for (_, c) in block.components.iter() {
            if let Origin::Synthesized { parent, role, .. } = &c.origin {
                if role == "decouple" {
                    *decouple
                        .entry(parent.as_str())
                        .or_default()
                        .entry(c.value.as_deref().unwrap_or("?"))
                        .or_default() += 1;
                }
            }
        }

        for (refdes, c) in sorted(&block.components) {
            if matches!(c.origin, Origin::Synthesized { .. }) {
                continue; // re-sugared onto parent
            }
            let mut fields = vec![format!("part: {}", q(&c.part))];
            if let Some(v) = &c.value {
                fields.push(format!("value: {}", q(v)));
            }
            if let Some(fpr) = &c.footprint {
                fields.push(format!("footprint: {}", q(fpr)));
            }
            if c.dnp {
                fields.push("dnp: true".into());
            }
            if !c.props.is_empty() {
                let ps: Vec<String> =
                    sorted(&c.props).into_iter().map(|(k, v)| format!("{}: {}", q(k), q(v))).collect();
                fields.push(format!("props: {{{}}}", ps.join(", ")));
            }
            if let Some(dec) = decouple.get(refdes.as_str()) {
                let ds: Vec<String> = dec.iter().map(|(v, n)| format!("{}: {}", q(v), n)).collect();
                fields.push(format!("decouple: {{{}}}", ds.join(", ")));
            }
            if !c.pins.is_empty() {
                fields.push(format!("pins: {}", pin_map_inline(&c.pins)));
            }
            if c.units.is_empty() {
                writeln!(o, "      {}: {{{}}}", q(refdes), fields.join(", ")).unwrap();
            } else {
                writeln!(o, "      {}:", q(refdes)).unwrap();
                for f in &fields {
                    let (k, v) = f.split_once(": ").unwrap();
                    writeln!(o, "        {k}: {v}").unwrap();
                }
                o.push_str("        units:\n");
                for (u, pins) in sorted(&c.units) {
                    writeln!(o, "          {}: {{pins: {}}}", q(u), pin_map_inline(pins)).unwrap();
                }
            }
        }
    }
    if !d.nets.is_empty() {
        let mut nets: Vec<_> = d.nets.iter().collect();
        nets.sort_by(|(a, _), (b, _)| a.cmp(b));
        let mut wrote_header = false;
        for (net, attrs) in nets {
            let mut fields = Vec::new();
            if attrs.power {
                fields.push("power: true".to_string());
            }
            if let Some(c) = &attrs.class {
                fields.push(format!("class: {}", q(c)));
            }
            if fields.is_empty() {
                continue; // nets exist by reference; attribute-free entries add nothing
            }
            if !wrote_header {
                o.push_str("nets:\n");
                wrote_header = true;
            }
            writeln!(o, "  {}: {{{}}}", q(net), fields.join(", ")).unwrap();
        }
    }
    o
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p circuit-lang`
Expected: all pass. If the idempotence test fails, diff `out1` vs `to_canonical_yaml(&d2)` — the usual culprit is ordering or quoting instability; fix in `canon.rs` until fixpoint holds.

- [ ] **Step 5: Commit**

```bash
git add crates/circuit-lang && git commit -m "feat(circuit-lang): deterministic canonical emitter with decouple re-sugar"
```

---

### Task 11: `compile()` facade + bluepill fixture (acceptance)

**Files:**
- Modify: `crates/circuit-lang/src/lib.rs` (facade)
- Create: `crates/circuit-lang/tests/fixtures/bluepill-h7.circuit.yaml`
- Create: `crates/circuit-lang/tests/bluepill.rs`

- [ ] **Step 1: Write the facade** — append to `crates/circuit-lang/src/lib.rs`:

```rust
pub use diag::{Diagnostic, Diagnostics, Severity, Span};
pub use model::Design;
pub use provider::{MockSymbolProvider, PinMeta, PinType, SymbolMeta, SymbolProvider};

pub struct CompileResult {
    /// Some only when there are no errors (warnings allowed).
    pub design: Option<Design>,
    pub diagnostics: Diagnostics,
}

/// Full gauntlet, pure half: parse -> desugar -> lint (spec §6).
pub fn compile(src: &str, provider: &dyn SymbolProvider) -> CompileResult {
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
```

- [ ] **Step 2: Write the fixture** — `crates/circuit-lang/tests/fixtures/bluepill-h7.circuit.yaml` (the spec §5 running example):

```yaml
version: 1
name: bluepill-h7
rails: [3V3, GND, VBUS]

blocks:
  power:
    layout: {edge: top}
    components:
      U2: {part: Regulator_Linear:AMS1117-3.3, pins: {VI: VBUS, VO: 3V3, GND: GND}}
      C9: {part: C, value: 10uF, between: [VBUS, GND]}
      C10: {part: C, value: 22uF, between: [3V3, GND]}

  usb:
    layout: {edge: left}
    components:
      J1:
        part: Connector:USB_C_Receptacle_USB2.0
        pins: {VBUS: VBUS, GND: GND, SHIELD: GND,
               DP1: USB_DP, DN1: USB_DM, DP2: USB_DP, DN2: USB_DM}
      R1: {part: R, value: 5.1k, between: [J1.CC1, GND]}
      R2: {part: R, value: 5.1k, between: [J1.CC2, GND]}

  mcu:
    components:
      U1:
        part: MCU_ST_STM32H7:STM32H743VITx
        decouple: {100nF: 10, 4.7uF: 2}
        pins:
          VDD: 3V3
          VSS: GND
          PA11: USB_DM
          PA12: USB_DP
          PB6: I2C1_SCL
          PB7: I2C1_SDA
          NRST: NRST
      R3: {part: R, value: 4.7k, between: [I2C1_SDA, 3V3]}
      R4: {part: R, value: 4.7k, between: [I2C1_SCL, 3V3]}

  headers:
    layout: {edge: right}
    components:
      J2:
        part: Connector_Generic:Conn_01x10
        pins: {1: 3V3, 2: GND, 3: U1.PB6, 4: U1.PB7, 5: NRST}

nets:
  I2C1_SDA: {class: i2c}
  I2C1_SCL: {class: i2c}
```

- [ ] **Step 3: Write the acceptance test** — `crates/circuit-lang/tests/bluepill.rs`:

```rust
use circuit_lang::{compile, MockSymbolProvider, PinType};

fn provider() -> MockSymbolProvider {
    use PinType::*;
    let mut p = MockSymbolProvider::with_basics();
    p.add("Regulator_Linear:AMS1117-3.3", vec![
        ("1", "GND", PowerInput, 1), ("2", "VO", PowerOutput, 1), ("3", "VI", PowerInput, 1),
    ]);
    p.add("Connector:USB_C_Receptacle_USB2.0", vec![
        ("A1", "GND", Passive, 1), ("A4", "VBUS", Passive, 1), ("A5", "CC1", Passive, 1),
        ("B5", "CC2", Passive, 1), ("A6", "DP1", Passive, 1), ("A7", "DN1", Passive, 1),
        ("B6", "DP2", Passive, 1), ("B7", "DN2", Passive, 1), ("S1", "SHIELD", Passive, 1),
    ]);
    p.add("MCU_ST_STM32H7:STM32H743VITx", vec![
        ("17", "VDD", PowerInput, 1), ("39", "VDD", PowerInput, 1),
        ("16", "VSS", PowerInput, 1), ("38", "VSS", PowerInput, 1),
        ("70", "PA11", Other, 1), ("71", "PA12", Other, 1),
        ("92", "PB6", Other, 1), ("93", "PB7", Other, 1), ("14", "NRST", Other, 1),
    ]);
    p.add("Connector_Generic:Conn_01x10", vec![
        ("1", "Pin_1", Passive, 1), ("2", "Pin_2", Passive, 1), ("3", "Pin_3", Passive, 1),
        ("4", "Pin_4", Passive, 1), ("5", "Pin_5", Passive, 1), ("6", "Pin_6", Passive, 1),
        ("7", "Pin_7", Passive, 1), ("8", "Pin_8", Passive, 1), ("9", "Pin_9", Passive, 1),
        ("10", "Pin_10", Passive, 1),
    ]);
    p
}

#[test]
fn bluepill_compiles_clean() {
    let src = include_str!("fixtures/bluepill-h7.circuit.yaml");
    let r = compile(src, &provider());
    let errors: Vec<_> = r.diagnostics.0.iter()
        .filter(|d| d.severity == circuit_lang::Severity::Error).collect();
    assert!(errors.is_empty(), "{errors:#?}");
    let d = r.design.unwrap();

    // 12 decoupling caps synthesized, tagged to U1
    let mcu = &d.blocks["mcu"];
    let caps = mcu.components.values()
        .filter(|c| matches!(&c.origin,
            circuit_lang::model::Origin::Synthesized { parent, role, .. }
                if parent == "U1" && role == "decouple"))
        .count();
    assert_eq!(caps, 12);

    // pin-refs: J2.3 joined I2C1_SCL via U1.PB6; CC pulldowns synthesized nets
    use circuit_lang::model::PinTarget;
    assert_eq!(d.blocks["headers"].components["J2"].pins["3"],
        PinTarget::Net("I2C1_SCL".into()));
    assert_eq!(d.blocks["usb"].components["R1"].pins["1"],
        PinTarget::Net("N_J1_CC1".into()));

    // canonical fixpoint on the real design
    let canon1 = circuit_lang::canon::to_canonical_yaml(&d);
    let r2 = compile(&canon1, &provider());
    let d2 = r2.design.unwrap();
    assert_eq!(d, d2);
    assert_eq!(canon1, circuit_lang::canon::to_canonical_yaml(&d2));
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p circuit-lang`
Expected: all unit tests + the bluepill acceptance test pass. This test failing usually means a desugar-order or naming bug — the assertions name the exact contract violated.

- [ ] **Step 5: Run the full workspace check**

Run: `cargo test && cargo clippy --workspace -- -D warnings && cargo fmt --check`
Expected: clean. Fix any clippy/fmt findings.

- [ ] **Step 6: Commit**

```bash
git add crates/circuit-lang && git commit -m "feat(circuit-lang): compile facade and bluepill-h7 acceptance fixture"
```

---

## Plan 1 complete — definition of done

- `cargo test` green; clippy/fmt clean.
- `circuit-lang` has zero I/O dependencies (check: `Cargo.toml` deps are exactly saphyr/indexmap/strsim/thiserror).
- The bluepill fixture compiles to a kernel `Design` with 12 tagged caps, resolved pin-refs, and a canonical-emit fixpoint.

**Out of scope here (next plans):** real symbol libraries (Plan 2 implements `SymbolProvider` over `/usr/share/kicad/symbols`), `.kicad_sch` emission/reconciliation (Plan 3), agent/TUI (Plan 4).

