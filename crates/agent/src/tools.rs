//! The six-tool registry the agent drives (spec §10).
//!
//! Each tool is a thin, deterministic wrapper over logic that already lives in
//! `circuit-lang`, `kicad-bridge`, and `sch-engine`. The registry exposes:
//!
//! - [`Tools::defs`] — the JSON-Schema [`ToolDef`]s handed to the LLM.
//! - [`Tools::run`] — dispatch a tool by name with a JSON input, returning JSON
//!   the model reads back. Results are structured for **self-repair**: failures
//!   carry diagnostic strings and "did you mean" suggestions rather than just an
//!   error flag, so the model can correct itself on the next turn.
//!
//! ## The six tools
//!
//! | name | input | output |
//! |---|---|---|
//! | `search_symbols` | `{query, limit?}` | `{hits: [{lib_id, pin_count}]}` |
//! | `get_symbol_info` | `{lib_id}` | `{lib_id, pins: [{number,name,type,unit}]}` or `{error, suggestions}` |
//! | `get_design` | `{}` | `{yaml}` (lifted) or `{yaml: "", note}` |
//! | `validate_design` | `{yaml}` | `{ok, diagnostics, errors, warnings}` |
//! | `apply_design` | `{yaml, commit?}` | dry-run diff, or (commit) `{written, erc}` |
//! | `run_erc` | `{}` | `{errors, warnings, violations: [...]}` |
//!
//! ## `apply_design`: dry-run vs commit
//!
//! `apply_design` is **dry-run by default** (`commit` absent or `false`). It
//! compiles the YAML, and on success renders the reconciled schematic against
//! the current `.kicad_sch` (if any), then returns a structured diff
//! (`added`/`removed`/`changed` refdes + a net-count delta) and the rendered
//! text length — **without writing anything**. The human apply-gate lives in the
//! agent loop (Task 3); only once it approves does the loop re-call with
//! `commit: true`, which writes the file, snapshots the prior, and runs ERC,
//! returning `{written: true, erc: {errors, warnings}}`.
//!
//! ## Symbol-index caching
//!
//! `search_symbols` is backed by [`SymbolIndex`], whose `build` scans every
//! installed `.kicad_sym` (~0.5 s). The index is built **once per `ToolCtx`**
//! and cached in a [`OnceCell`]; subsequent searches reuse it.

use std::cell::OnceCell;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use circuit_lang::model::{Component, Design, PinTarget};
use circuit_lang::{SymbolProvider, compile};
use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;
use kicad_bridge::search::SymbolIndex;
use kicad_bridge::snapshot::SnapshotStore;
use sch_engine::{emit_design_reconciled, lift::lift};

use crate::llm::ToolDef;

/// Default number of symbol-search hits returned when `limit` is omitted.
const DEFAULT_SEARCH_LIMIT: usize = 8;

/// Shared state every tool runs against: the detected KiCAD environment, the
/// project directory and its `.kicad_sch`, a symbol provider, a snapshot store,
/// and a lazily-built (then cached) symbol index.
///
/// The provider and index are interior-mutable / cached, so `run` takes
/// `&ToolCtx` — tools never need exclusive access.
pub struct ToolCtx {
    env: KicadEnv,
    /// Project directory holding the schematic and `.auto-pcb/history`.
    project_dir: PathBuf,
    /// Path to the project's `.kicad_sch` (may not exist yet).
    sch_path: PathBuf,
    /// Symbol provider over the installed libraries (memoizes lookups).
    provider: RealSymbolProvider,
    /// Per-write history / undo store.
    snapshots: SnapshotStore,
    /// Cross-library name index, built on first `search_symbols` and reused.
    index: OnceCell<SymbolIndex>,
    /// Keeps a test tempdir alive for the ctx's lifetime; `None` for real ctxs.
    _tempdir: Option<tempfile::TempDir>,
}

impl ToolCtx {
    /// Build a context for an existing project directory.
    ///
    /// `project_dir` must exist; `<project_dir>/<name>.kicad_sch` is the file the
    /// tools read/write. The schematic itself need not exist yet.
    pub fn new(env: KicadEnv, project_dir: PathBuf, sch_path: PathBuf) -> Result<Self> {
        let snapshots = SnapshotStore::for_project(&project_dir)
            .with_context(|| format!("opening snapshot store in {}", project_dir.display()))?;
        let provider = RealSymbolProvider::new(env.clone());
        Ok(Self {
            env,
            project_dir,
            sch_path,
            provider,
            snapshots,
            index: OnceCell::new(),
            _tempdir: None,
        })
    }

    /// Detect a real KiCAD installation and build a context over a fresh
    /// temporary project (no schematic yet). Returns `None` when no KiCAD is
    /// found, so tests SKIP gracefully off the project's test environment.
    pub fn detect_for_test() -> Option<Self> {
        let env = KicadEnv::detect()?;
        let tempdir = tempfile::tempdir().ok()?;
        let project_dir = tempdir.path().to_path_buf();
        let sch_path = project_dir.join("project.kicad_sch");
        let snapshots = SnapshotStore::for_project(&project_dir).ok()?;
        let provider = RealSymbolProvider::new(env.clone());
        Some(Self {
            env,
            project_dir,
            sch_path,
            provider,
            snapshots,
            index: OnceCell::new(),
            _tempdir: Some(tempdir),
        })
    }

    /// The project's `.kicad_sch` path (may not exist).
    pub fn sch_path(&self) -> &Path {
        &self.sch_path
    }

    /// The project directory.
    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    /// The detected KiCAD environment.
    pub fn env(&self) -> &KicadEnv {
        &self.env
    }

    /// The symbol provider over the installed libraries.
    pub fn provider(&self) -> &RealSymbolProvider {
        &self.provider
    }

    /// The snapshot / undo store for this project.
    pub fn snapshots(&self) -> &SnapshotStore {
        &self.snapshots
    }

    /// The cross-library symbol index, built once and cached.
    ///
    /// Building scans every installed `.kicad_sym` (~0.5 s); the result is
    /// memoized so repeated `search_symbols` calls are cheap.
    fn index(&self) -> Result<&SymbolIndex> {
        if let Some(idx) = self.index.get() {
            return Ok(idx);
        }
        let idx = SymbolIndex::build(&self.env).context("building symbol index")?;
        // `set` only fails if another thread raced us; dispatch is single-threaded.
        let _ = self.index.set(idx);
        Ok(self.index.get().expect("index just set"))
    }
}

/// The tool registry. Stateless — all state lives in [`ToolCtx`].
#[derive(Default)]
pub struct Tools;

impl Tools {
    pub fn new() -> Self {
        Self
    }

    /// The JSON-Schema definitions for every tool, in a stable order.
    pub fn defs(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "search_symbols".into(),
                description: "Search every installed KiCAD symbol library by name \
                    and return the best matches as fully-qualified `Lib:Name` ids \
                    with their pin counts. Use this to find the real lib_id for a \
                    part before placing it — never guess a lib_id."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string",
                            "description": "Part name or fragment, e.g. \"STM32H743VI\" or \"USB-C receptacle\"." },
                        "limit": { "type": "integer",
                            "description": "Max hits to return (default 8).", "minimum": 1 }
                    },
                    "required": ["query"]
                }),
            },
            ToolDef {
                name: "get_symbol_info".into(),
                description: "Return the full pin table (number, name, electrical \
                    type, unit) for a fully-qualified `Lib:Name` symbol. If the \
                    lib_id is unknown, returns an error with the closest known \
                    suggestions."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "lib_id": { "type": "string",
                            "description": "Fully-qualified symbol id, e.g. \"Device:R\" or \"MCU_ST_STM32H7:STM32H743VITx\"." }
                    },
                    "required": ["lib_id"]
                }),
            },
            ToolDef {
                name: "get_design".into(),
                description: "Return the current schematic lifted back into \
                    canonical circuit-YAML. If no schematic exists yet, returns an \
                    empty yaml with a note."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            ToolDef {
                name: "validate_design".into(),
                description: "Compile circuit-YAML against the real symbol \
                    libraries WITHOUT writing anything. Returns ok plus every \
                    diagnostic (errors and warnings) as human-readable strings — \
                    use the diagnostics to self-repair the YAML."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "yaml": { "type": "string", "description": "The circuit-YAML source to validate." }
                    },
                    "required": ["yaml"]
                }),
            },
            ToolDef {
                name: "apply_design".into(),
                description: "Compile circuit-YAML and render the reconciled \
                    schematic. By default (commit omitted/false) this is a DRY RUN: \
                    it returns a diff (added/removed/changed refdes + net delta) \
                    and does NOT write. With commit=true it writes the .kicad_sch, \
                    snapshots the prior, runs ERC, and returns the ERC counts. \
                    Compilation errors are returned as diagnostics with ok=false."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "yaml": { "type": "string", "description": "The circuit-YAML source to apply." },
                        "commit": { "type": "boolean",
                            "description": "Write the schematic (true) or dry-run and only return the diff (false, default)." }
                    },
                    "required": ["yaml"]
                }),
            },
            ToolDef {
                name: "run_erc".into(),
                description: "Run KiCAD's Electrical Rules Check on the current \
                    schematic and return the error/warning counts plus the \
                    violations (severity, type, description). Errors if no \
                    schematic exists yet."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
        ]
    }

    /// Dispatch a tool by name. `input` is the model-supplied JSON arguments;
    /// the returned `Value` is fed back to the model.
    pub fn run(&self, name: &str, input: Value, ctx: &ToolCtx) -> Result<Value> {
        match name {
            "search_symbols" => search_symbols(input, ctx),
            "get_symbol_info" => get_symbol_info(input, ctx),
            "get_design" => get_design(ctx),
            "validate_design" => validate_design(input, ctx),
            "apply_design" => apply_design(input, ctx),
            "run_erc" => run_erc(ctx),
            other => bail!("unknown tool: {other}"),
        }
    }
}

/// Pull a required string field out of the input, with a clear error.
fn require_str(input: &Value, key: &str) -> Result<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing required string field `{key}`"))
}

// ── 1. search_symbols ──────────────────────────────────────────────────────

fn search_symbols(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let query = require_str(&input, "query")?;
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_SEARCH_LIMIT);

    let hits: Vec<Value> = ctx
        .index()?
        .search(&query, limit)
        .into_iter()
        .map(|h| json!({ "lib_id": h.lib_id, "pin_count": h.pin_count }))
        .collect();

    Ok(json!({ "hits": hits }))
}

// ── 2. get_symbol_info ─────────────────────────────────────────────────────

fn get_symbol_info(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let lib_id = require_str(&input, "lib_id")?;

    match ctx.provider.symbol(&lib_id) {
        Some(meta) => {
            let pins: Vec<Value> = meta
                .pins
                .iter()
                .map(|p| {
                    json!({
                        "number": p.number,
                        "name": p.name,
                        "type": pin_type_str(p.etype),
                        "unit": p.unit,
                    })
                })
                .collect();
            Ok(json!({ "lib_id": lib_id, "pins": pins }))
        }
        None => {
            let suggestions = ctx.provider.suggest(&lib_id);
            Ok(json!({
                "error": format!("unknown symbol `{lib_id}`"),
                "suggestions": suggestions,
            }))
        }
    }
}

/// Render a [`circuit_lang::PinType`] as a stable lowercase string for the LLM.
fn pin_type_str(t: circuit_lang::PinType) -> &'static str {
    use circuit_lang::PinType::*;
    match t {
        PowerInput => "power_input",
        PowerOutput => "power_output",
        Passive => "passive",
        Other => "other",
    }
}

// ── 3. get_design ──────────────────────────────────────────────────────────

fn get_design(ctx: &ToolCtx) -> Result<Value> {
    if !ctx.sch_path.exists() {
        return Ok(json!({ "yaml": "", "note": "no schematic yet" }));
    }
    let yaml = lift(&ctx.env, &ctx.sch_path)
        .with_context(|| format!("lifting {}", ctx.sch_path.display()))?;
    Ok(json!({ "yaml": yaml }))
}

// ── 4. validate_design ─────────────────────────────────────────────────────

fn validate_design(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let yaml = require_str(&input, "yaml")?;
    let result = compile(&yaml, &ctx.provider);
    Ok(compile_report(&result.diagnostics))
}

/// Build the `{ok, diagnostics, errors, warnings}` report a compile yields.
fn compile_report(diags: &circuit_lang::Diagnostics) -> Value {
    use circuit_lang::Severity;
    let strings: Vec<String> = diags.0.iter().map(|d| d.to_string()).collect();
    let errors = diags
        .0
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warnings = diags
        .0
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count();
    json!({
        "ok": errors == 0,
        "diagnostics": strings,
        "errors": errors,
        "warnings": warnings,
    })
}

// ── 5. apply_design ────────────────────────────────────────────────────────

fn apply_design(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let yaml = require_str(&input, "yaml")?;
    let commit = input
        .get("commit")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Compile first; never render or write a design with errors.
    let result = compile(&yaml, &ctx.provider);
    let Some(design) = result.design else {
        let mut report = compile_report(&result.diagnostics);
        // `ok` is already false here (errors > 0), but be explicit for the LLM.
        report["ok"] = json!(false);
        return Ok(report);
    };

    // Prior schematic text (for reconciliation) and prior design (for the diff).
    let prior_text = std::fs::read_to_string(&ctx.sch_path).ok();
    let prior_design = if ctx.sch_path.exists() {
        let prior_yaml = lift(&ctx.env, &ctx.sch_path)
            .with_context(|| format!("lifting prior {}", ctx.sch_path.display()))?;
        compile(&prior_yaml, &ctx.provider).design
    } else {
        None
    };

    let rendered = emit_design_reconciled(&ctx.env, &design, prior_text.as_deref())
        .context("rendering reconciled schematic")?;
    let diff = design_diff(prior_design.as_ref(), &design);

    if !commit {
        return Ok(json!({
            "ok": true,
            "would_write": true,
            "diff": diff,
            "rendered_len": rendered.len(),
        }));
    }

    // Commit path: snapshot the prior (if any), write, then ERC.
    if ctx.sch_path.exists() {
        ctx.snapshots
            .snapshot(&ctx.sch_path)
            .with_context(|| format!("snapshotting {}", ctx.sch_path.display()))?;
    }
    std::fs::write(&ctx.sch_path, &rendered)
        .with_context(|| format!("writing {}", ctx.sch_path.display()))?;

    let erc = KicadCli::new(&ctx.env)
        .erc(&ctx.sch_path)
        .with_context(|| format!("running ERC on {}", ctx.sch_path.display()))?;

    Ok(json!({
        "ok": true,
        "written": true,
        "diff": diff,
        "erc": { "errors": erc.error_count(), "warnings": erc.warning_count() },
    }))
}

/// A per-refdes signature used to detect a *changed* component across a re-apply.
///
/// Two components with the same refdes but a different signature are "changed".
/// The signature folds in the fields that the netlist round-trip can carry —
/// part id, value, footprint, dnp, and the connectivity (component-level and
/// per-unit pin maps) — but deliberately ignores placement, which is not part of
/// the kernel model. Pins/units are gathered into sorted maps so iteration order
/// never spuriously flips the signature.
fn component_signature(c: &Component) -> String {
    use std::collections::BTreeMap;

    fn pin_targets(pins: &indexmap::IndexMap<String, PinTarget>) -> BTreeMap<&str, String> {
        pins.iter()
            .map(|(k, t)| {
                let v = match t {
                    PinTarget::Net(n) => n.clone(),
                    PinTarget::NoConnect => "nc".to_string(),
                };
                (k.as_str(), v)
            })
            .collect()
    }

    let pins = pin_targets(&c.pins);
    let units: BTreeMap<&str, BTreeMap<&str, String>> = c
        .units
        .iter()
        .map(|(u, m)| (u.as_str(), pin_targets(m)))
        .collect();

    format!(
        "{}|{}|{}|{}|{:?}|{:?}",
        c.part,
        c.value.as_deref().unwrap_or(""),
        c.footprint.as_deref().unwrap_or(""),
        c.dnp,
        pins,
        units,
    )
}

/// Structured diff between a prior design (possibly `None` for a fresh project)
/// and the new one: which refdes were added, removed, or changed, plus the net
/// count before/after. Refdes are gathered across all blocks.
fn design_diff(prior: Option<&Design>, new: &Design) -> Value {
    use std::collections::BTreeMap;

    fn components(d: &Design) -> BTreeMap<String, &Component> {
        let mut m = BTreeMap::new();
        for block in d.blocks.values() {
            for (refdes, c) in &block.components {
                m.insert(refdes.clone(), c);
            }
        }
        m
    }

    let new_comps = components(new);
    let prior_comps = prior.map(components).unwrap_or_default();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();

    for (refdes, c) in &new_comps {
        match prior_comps.get(refdes) {
            None => added.push(refdes.clone()),
            Some(old) if component_signature(old) != component_signature(c) => {
                changed.push(refdes.clone());
            }
            Some(_) => {}
        }
    }
    for refdes in prior_comps.keys() {
        if !new_comps.contains_key(refdes) {
            removed.push(refdes.clone());
        }
    }

    let nets_before = prior.map(|d| d.nets.len()).unwrap_or(0);
    json!({
        "added": added,
        "removed": removed,
        "changed": changed,
        "nets_before": nets_before,
        "nets_after": new.nets.len(),
    })
}

// ── 6. run_erc ─────────────────────────────────────────────────────────────

fn run_erc(ctx: &ToolCtx) -> Result<Value> {
    if !ctx.sch_path.exists() {
        bail!(
            "no schematic to check at {} — apply a design first",
            ctx.sch_path.display()
        );
    }
    let report = KicadCli::new(&ctx.env)
        .erc(&ctx.sch_path)
        .with_context(|| format!("running ERC on {}", ctx.sch_path.display()))?;

    let violations: Vec<Value> = report
        .violations
        .iter()
        .map(|v| {
            json!({
                "severity": v.severity,
                "type": v.kind,
                "description": v.description,
            })
        })
        .collect();

    Ok(json!({
        "errors": report.error_count(),
        "warnings": report.warning_count(),
        "violations": violations,
    }))
}
