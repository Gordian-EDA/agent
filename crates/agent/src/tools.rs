//! The eleven-tool registry the agent drives (spec §10).
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
//! ## The eleven tools
//!
//! | name | input | output |
//! |---|---|---|
//! | `search_symbols` | `{query, limit?}` | `{hits: [{lib_id, pin_count}]}` |
//! | `get_symbol_info` | `{lib_id}` | `{lib_id, pins: [{number,name,type,unit}]}` or `{error, suggestions}` |
//! | `get_design` | `{}` | `{yaml, source, stale?, note?}` |
//! | `validate_design` | `{yaml}` | `{ok, diagnostics, errors, warnings}` |
//! | `apply_design` | `{yaml?, commit?, ...}` | dry-run diff, or (commit) `{written, path, erc}` |
//! | `run_erc` | `{}` | `{errors, warnings, violations: [...]}` |
//! | `project_info` | `{}` | `{project_dir, sch_path, sch_exists, snapshots, cwd}` |
//! | `read_schematic` | `{path}` | `{path, yaml, note?}` or `{error}` |
//! | `render_schematic` | `{}` | `{ok, png_path, note}` (+image attached) or `{error}` |
//! | `create_design` | `{yaml, overwrite?}` | compile report + `{draft_written}` or `{error}` |
//! | `edit_design` | `{old_string, new_string, replace_all?}` | compile report + `{replacements}` or `{error}` |
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
//! and cached in a [`OnceLock`]; subsequent searches reuse it.
//!
//! ## Threading
//!
//! [`ToolCtx`] is `Send + Sync` (asserted below) so the agent loop can run
//! tool calls on `spawn_blocking` threads — keeping a single-threaded UI
//! responsive while a tool compiles, renders, or shells out to `kicad-cli`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
    index: OnceLock<SymbolIndex>,
    /// Project-local persistent state directory `.autopcb/`.
    workspace: crate::workspace::Workspace,
    /// Keeps a test tempdir alive for the ctx's lifetime; `None` for real ctxs.
    _tempdir: Option<tempfile::TempDir>,
}

/// Tool execution happens on blocking threads; the context must cross them.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ToolCtx>();
};

impl ToolCtx {
    /// Build a context for an existing project directory.
    ///
    /// `project_dir` must exist; `<project_dir>/<name>.kicad_sch` is the file the
    /// tools read/write. The schematic itself need not exist yet.
    pub fn new(env: KicadEnv, project_dir: PathBuf, sch_path: PathBuf) -> Result<Self> {
        let snapshots = SnapshotStore::for_project(&project_dir)
            .with_context(|| format!("opening snapshot store in {}", project_dir.display()))?;
        let workspace = crate::workspace::Workspace::for_project(&project_dir)
            .with_context(|| format!("opening .autopcb workspace in {}", project_dir.display()))?;
        let provider = RealSymbolProvider::new(env.clone());
        Ok(Self {
            env,
            project_dir,
            sch_path,
            provider,
            snapshots,
            index: OnceLock::new(),
            workspace,
            _tempdir: None,
        })
    }

    /// Build a context for a real project directory using the project's
    /// conventional schematic name `design.kicad_sch`.
    ///
    /// `project_dir` is created if it does not exist. The schematic itself need
    /// not exist yet — the agent's first `apply_design(commit:true)` writes it.
    /// This is the constructor the headless `autopcb agent` subcommand uses.
    pub fn for_project(env: KicadEnv, project_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&project_dir)
            .with_context(|| format!("creating project dir {}", project_dir.display()))?;
        let sch_path = project_dir.join("design.kicad_sch");
        Self::new(env, project_dir, sch_path)
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
        let workspace = crate::workspace::Workspace::for_project(&project_dir).ok()?;
        let provider = RealSymbolProvider::new(env.clone());
        Some(Self {
            env,
            project_dir,
            sch_path,
            provider,
            snapshots,
            index: OnceLock::new(),
            workspace,
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

    /// The project's `.autopcb/` persistent state.
    pub fn workspace(&self) -> &crate::workspace::Workspace {
        &self.workspace
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
                description: "Return the working draft (circuit-YAML) when one \
                    exists, seeding it from the current schematic if needed. If \
                    a draft already exists, returns it (source=draft) and flags \
                    stale=true when the .kicad_sch changed out-of-band since the \
                    draft was seeded. If no draft exists, lifts the schematic \
                    (source=lifted), seeds the draft so edit_design is immediately \
                    usable, and returns the lifted YAML. If no schematic exists \
                    yet, returns an empty yaml with a note."
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
                    Compilation errors are returned as diagnostics with ok=false. \
                    If yaml is omitted, applies the current draft (see \
                    create_design/edit_design)."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "yaml": { "type": "string", "description": "The circuit-YAML source to apply. If omitted, the current draft is used." },
                        "commit": { "type": "boolean",
                            "description": "Write the schematic (true) or dry-run and only return the diff (false, default)." }
                    }
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
            ToolDef {
                name: "project_info".into(),
                description: "Return the current project's paths and state: the \
                    project directory, the schematic path the tools read/write, \
                    whether that file exists yet, how many undo snapshots there \
                    are, and the process working directory. Use this when the \
                    user asks where files live or whether you can see their \
                    schematic."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            ToolDef {
                name: "read_schematic".into(),
                description: "Read ANY .kicad_sch file on disk and return it \
                    lifted to circuit-YAML. The path may be absolute, start \
                    with ~, or be relative to the project directory. Use this \
                    to inspect a schematic the user references by path; it does \
                    not change which file the project edits."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string",
                            "description": "Path to a .kicad_sch file, e.g. \"/home/me/boards/x.kicad_sch\" or \"~/boards/x.kicad_sch\"." }
                    },
                    "required": ["path"]
                }),
            },
            ToolDef {
                name: "render_schematic".into(),
                description: "Render the current schematic to a PNG image and \
                    return it so you can SEE the sheet. Use after apply_design \
                    to inspect layout quality: overlapping text, crowding, \
                    confusing arrangement. The PNG is also saved under \
                    .autopcb/renders/."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            ToolDef {
                name: "create_design".into(),
                description: "Create the working draft (circuit-YAML) from \
                    scratch. The draft is the document edit_design patches and \
                    apply_design (with no yaml argument) applies. Fails if a \
                    draft already exists unless overwrite=true. Returns compile \
                    diagnostics for the new draft."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "yaml": { "type": "string", "description": "The full circuit-YAML draft content." },
                        "overwrite": { "type": "boolean", "description": "Replace an existing draft (default false)." }
                    },
                    "required": ["yaml"]
                }),
            },
            ToolDef {
                name: "edit_design".into(),
                description: "Patch the working draft by exact string \
                    replacement: old_string must occur exactly once (or pass \
                    replace_all=true). Far cheaper and safer than resending the \
                    whole document. Returns compile diagnostics for the edited \
                    draft so you get immediate validation feedback."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "old_string": { "type": "string", "description": "Exact text to find in the draft." },
                        "new_string": { "type": "string", "description": "Replacement text." },
                        "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)." }
                    },
                    "required": ["old_string", "new_string"]
                }),
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
            "project_info" => project_info(ctx),
            "read_schematic" => read_schematic(input, ctx),
            "render_schematic" => render_schematic(ctx),
            "create_design" => create_design(input, ctx),
            "edit_design" => edit_design(input, ctx),
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

fn current_sch_text(ctx: &ToolCtx) -> Option<String> {
    std::fs::read_to_string(&ctx.sch_path).ok()
}

fn get_design(ctx: &ToolCtx) -> Result<Value> {
    if let Some(draft) = ctx.workspace().read_draft() {
        let mut out = json!({ "yaml": draft, "source": "draft" });
        if ctx.workspace().draft_is_stale(current_sch_text(ctx).as_deref()) {
            out["stale"] = json!(true);
            out["note"] = json!(
                "the .kicad_sch changed since this draft was seeded (user edit \
                 in KiCAD?) — call read_schematic on the project schematic to \
                 see the current state, then reconcile your draft deliberately"
            );
        }
        return Ok(out);
    }
    if !ctx.sch_path.exists() {
        return Ok(json!({ "yaml": "", "note": "no schematic yet" }));
    }
    let yaml = lift(&ctx.env, &ctx.sch_path)
        .with_context(|| format!("lifting {}", ctx.sch_path.display()))?;
    // Seed the draft so edit_design is immediately usable.
    ctx.workspace()
        .write_draft(&yaml, current_sch_text(ctx).as_deref())?;
    Ok(json!({ "yaml": yaml, "source": "lifted",
               "note": "draft seeded from the schematic; use edit_design for changes" }))
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
    let explicit_yaml = input.get("yaml").and_then(Value::as_str).map(str::to_string);
    let yaml = match explicit_yaml.clone() {
        Some(y) => y,
        None => match ctx.workspace().read_draft() {
            Some(d) => d,
            None => {
                return Ok(json!({
                    "error": "no yaml given and no draft exists — pass yaml, or \
                              create a draft via get_design/create_design",
                }));
            }
        },
    };
    let stale = explicit_yaml.is_none()
        && ctx.workspace().draft_is_stale(current_sch_text(ctx).as_deref());

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
            "stale_draft_warning": stale,
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

    // Record the hash of the just-written schematic (current_sch_text reads the
    // file we wrote above) so the applied draft is no longer flagged stale.
    // No-op when no draft exists (an explicit-yaml apply must not create one).
    if ctx.workspace().read_draft().is_some() {
        ctx.workspace()
            .write_draft(&yaml, current_sch_text(ctx).as_deref())?;
    }

    Ok(json!({
        "ok": true,
        "written": true,
        "path": ctx.sch_path.display().to_string(),
        "stale_draft_warning": stale,
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

// ── 6. project_info ────────────────────────────────────────────────────────

fn project_info(ctx: &ToolCtx) -> Result<Value> {
    let snapshots = ctx
        .snapshots
        .list(&ctx.sch_path)
        .map(|v| v.len())
        .unwrap_or(0);
    Ok(json!({
        "project_dir": ctx.project_dir.display().to_string(),
        "sch_path": ctx.sch_path.display().to_string(),
        "sch_exists": ctx.sch_path.exists(),
        "snapshots": snapshots,
        "cwd": std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    }))
}

// ── 7. read_schematic ──────────────────────────────────────────────────────

fn read_schematic(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let raw = require_str(&input, "path")?;
    let path = resolve_user_path(&raw, &ctx.project_dir);

    if !path.is_file() {
        return Ok(json!({
            "error": format!("no file at `{}`", path.display()),
            "note": "the path may be absolute, start with ~, or be relative to the project dir",
        }));
    }
    if path.extension().and_then(|e| e.to_str()) != Some("kicad_sch") {
        return Ok(json!({
            "error": format!("`{}` is not a .kicad_sch schematic", path.display()),
        }));
    }

    let yaml = match lift(&ctx.env, &path) {
        Ok(yaml) => yaml,
        Err(e) => {
            return Ok(json!({
                "error": format!("could not lift `{}`: {e}", path.display()),
            }));
        }
    };

    let mut out = json!({
        "path": path.display().to_string(),
        "yaml": yaml,
    });
    if same_file(&path, &ctx.sch_path) {
        out["note"] =
            json!("this IS the project's current schematic (the one apply_design writes)");
    }
    Ok(out)
}

/// Resolve a user-supplied path: expand a leading `~`, and anchor relative
/// paths at the project directory (the agent's natural working root).
fn resolve_user_path(raw: &str, project_dir: &Path) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else {
        project_dir.join(p)
    }
}

/// Whether two paths name the same existing file (canonicalized comparison;
/// falls back to literal equality when either cannot be canonicalized).
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

// ── 8. run_erc ─────────────────────────────────────────────────────────────

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

// ── 9. create_design / edit_design ────────────────────────────────────────

fn create_design(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let yaml = require_str(&input, "yaml")?;
    let overwrite = input.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
    if ctx.workspace().read_draft().is_some() && !overwrite {
        return Ok(json!({
            "error": "a draft already exists — pass overwrite=true to replace it, \
                      or use edit_design to modify it",
        }));
    }
    ctx.workspace()
        .write_draft(&yaml, current_sch_text(ctx).as_deref())?;
    let mut report = compile_report(&compile(&yaml, &ctx.provider).diagnostics);
    report["draft_written"] = json!(true);
    Ok(report)
}

fn edit_design(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let old = require_str(&input, "old_string")?;
    let new = require_str(&input, "new_string")?;
    let replace_all = input.get("replace_all").and_then(Value::as_bool).unwrap_or(false);

    let Some(draft) = ctx.workspace().read_draft() else {
        return Ok(json!({
            "error": "no draft exists — call get_design (seeds a draft from the \
                      current schematic) or create_design first",
        }));
    };
    let count = draft.matches(&*old).count();
    if count == 0 {
        return Ok(json!({
            "error": format!("old_string not found in the draft (it must match \
                              exactly, including whitespace): {old:?}"),
        }));
    }
    if count > 1 && !replace_all {
        return Ok(json!({
            "error": format!("old_string matches {count} times — make it more \
                              specific or pass replace_all=true"),
        }));
    }
    let edited = if replace_all {
        draft.replace(&*old, &*new)
    } else {
        draft.replacen(&*old, &*new, 1)
    };
    ctx.workspace()
        .write_draft(&edited, current_sch_text(ctx).as_deref())?;

    let mut report = compile_report(&compile(&edited, &ctx.provider).diagnostics);
    report["replacements"] = json!(if replace_all { count } else { 1 });
    Ok(report)
}

// ── 10. render_schematic ────────────────────────────────────────────────────

/// Result key carrying a PNG path for the agent loop to attach as an image
/// block (and strip from the JSON the model sees as text).
pub const IMAGE_PATH_KEY: &str = "_image_path";

/// Long-edge pixel cap for rendered schematics (Claude vision sweet spot).
const RENDER_MAX_PX: u32 = 1600;

fn render_schematic(ctx: &ToolCtx) -> Result<Value> {
    if !ctx.sch_path.exists() {
        return Ok(json!({
            "error": "no schematic yet — apply a design first",
        }));
    }
    let tmp = tempfile::tempdir().context("creating temp dir for svg export")?;
    let svg_path = KicadCli::new(&ctx.env)
        .export_svg(&ctx.sch_path, tmp.path())
        .context("exporting schematic SVG")?;
    let svg = std::fs::read_to_string(&svg_path)?;
    // `svg` is now fully in memory; `tmp` may safely drop at end of scope.
    let png = crate::render::svg_to_png(&svg, RENDER_MAX_PX)?;
    let path = ctx.workspace().next_render_path()?;
    std::fs::write(&path, &png)
        .with_context(|| format!("writing {}", path.display()))?;
    let mut obj = json!({
        "ok": true,
        "png_path": path.display().to_string(),
        "note": "image attached; also saved to png_path for the user to open",
    });
    obj[IMAGE_PATH_KEY] = json!(path.display().to_string());
    Ok(obj)
}
