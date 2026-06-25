//! The schematic + PCB tool registry the agent drives.
//!
//! Each tool is a thin, deterministic wrapper over logic that already lives in
//! `circuit-lang`, `kicad-sexpr`/`kicad-cli`, and `sch-floorplan`/`sch-io`. The registry
//! exposes two free functions, both driven directly by the [`crate::Agent`] loop:
//!
//! - [`tool_defs`] — the JSON-Schema [`ToolDef`]s handed to the LLM.
//! - [`run_tool`] — dispatch a tool by name with a JSON input, returning JSON the
//!   model reads back. Results are structured for **self-repair**: failures carry
//!   diagnostic strings and "did you mean" suggestions rather than just an error
//!   flag, so the model can correct itself on the next turn.
//!
//! The schematic side covers `search_symbols` / `get_symbol_info` / `get_design`
//! / `validate_design` / `apply_design` / `review_design` / `run_erc` /
//! `project_info` / `read_schematic` / `render_schematic` / `create_design` /
//! `edit_design`; the PCB side (in [`crate::tools_pcb`]) covers the footprint
//! search/info, `derive_board`, and the place/route/export/interactive flow.
//!
//! ## `apply_design`: dry-run vs commit
//!
//! `apply_design` is **dry-run by default** (`commit` absent or `false`). It
//! compiles the YAML, and on success renders the reconciled schematic against
//! the current `.kicad_sch` (if any), then returns a structured diff
//! (`added`/`removed`/`changed` refdes + a net-count delta) and the rendered
//! text length — **without writing anything**. The human apply-gate lives in the
//! [`crate::Agent`] loop (the preview → approve → commit choreography over a
//! [`crate::ToolEffect::Gated`] tool); only once it approves does the loop re-call
//! with `commit: true`, which writes
//! the file, snapshots the prior, and runs ERC, returning
//! `{written: true, erc: {errors, warnings}}`.
//!
//! ## Symbol-index caching
//!
//! `search_symbols` is backed by [`SymbolIndex`], whose `build` scans every
//! installed `.kicad_sym` (~0.5 s). The index is built **once per `PcbToolCtx`**
//! and cached in a [`OnceLock`]; subsequent searches reuse it.
//!
//! ## Threading
//!
//! [`PcbToolCtx`] is `Send + Sync` (asserted below) so the agent loop can run
//! tool calls on `spawn_blocking` threads — keeping a single-threaded UI
//! responsive while a tool compiles, renders, or shells out to `kicad-cli`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use circuit_lang::model::{Component, Design, PinTarget};
use circuit_lang::compile;
use kicad_cli::cli::KicadCli;
use kicad_cli::env::KicadEnv;
use kicad_sexpr::footlib::FootprintIndex;
use kicad_symbol::SymbolTable;
use kicad_symbol::search::SymbolIndex;
use crate::history::SnapshotStore;

use sch_floorplan::floorplan::{infer_ir, LayoutIr};
use sch_io::read::lift;

use crate::ToolDef;

/// Default number of symbol-search hits returned when `limit` is omitted.
const DEFAULT_SEARCH_LIMIT: usize = 8;

/// Shared state every tool runs against: the detected KiCAD environment, the
/// project directory and its `.kicad_sch`, a symbol provider, a snapshot store,
/// and a lazily-built (then cached) symbol index.
///
/// The provider and index are interior-mutable / cached, so `run` takes
/// `&PcbToolCtx` — tools never need exclusive access.
pub struct PcbToolCtx {
    env: KicadEnv,
    /// Project directory holding the schematic and `.gordian/history`.
    project_dir: PathBuf,
    /// Path to the project's `.kicad_sch` (may not exist yet).
    sch_path: PathBuf,
    /// Symbol provider over the installed libraries (memoizes lookups).
    provider: SymbolTable,
    /// Per-write history / undo store.
    snapshots: SnapshotStore,
    /// Cross-library name index, built on first `search_symbols` and reused.
    index: OnceLock<SymbolIndex>,
    /// Cross-library footprint index, built on first `search_footprints` /
    /// `get_footprint_info` / `derive_board` and reused. Scanning every
    /// `.pretty` library is expensive, so (like `index`) it is built once.
    footprint_index: OnceLock<FootprintIndex>,
    /// Test override: when set, the footprint index is built from this directory
    /// of `.pretty` libraries (the vendored fixtures) instead of the installed
    /// KiCAD footprint share dir, so footprint tests run without KiCAD libs.
    footprint_dir_override: Option<PathBuf>,
    /// Project-local persistent state directory `.gordian/`.
    workspace: crate::workspace::Workspace,
    /// The live KiCAD IPC session for interactive board editing, launched lazily
    /// by `open_board` and reused by the geometry tools (`move_part`,
    /// `route_track`, …). `Mutex` so `PcbToolCtx` stays `Send + Sync`.
    kicad: std::sync::Mutex<Option<kicad_ipc::Session>>,
    /// Keeps a test tempdir alive for the ctx's lifetime; `None` for real ctxs.
    _tempdir: Option<tempfile::TempDir>,
}

/// Tool execution happens on blocking threads; the context must cross them.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PcbToolCtx>();
};

impl PcbToolCtx {
    /// Build a context for an existing project directory.
    ///
    /// `project_dir` must exist; `<project_dir>/<name>.kicad_sch` is the file the
    /// tools read/write. The schematic itself need not exist yet.
    pub fn new(env: KicadEnv, project_dir: PathBuf, sch_path: PathBuf) -> Result<Self> {
        let snapshots = SnapshotStore::for_project(&project_dir)
            .with_context(|| format!("opening snapshot store in {}", project_dir.display()))?;
        let workspace = crate::workspace::Workspace::for_project(&project_dir)
            .with_context(|| format!("opening .gordian workspace in {}", project_dir.display()))?;
        let provider = SymbolTable::from_env(&env);
        Ok(Self {
            env,
            project_dir,
            sch_path,
            provider,
            snapshots,
            index: OnceLock::new(),
            footprint_index: OnceLock::new(),
            footprint_dir_override: None,
            workspace,
            kicad: std::sync::Mutex::new(None),
            _tempdir: None,
        })
    }

    /// Build a context for a real project directory using the project's
    /// conventional schematic name `design.kicad_sch`.
    ///
    /// `project_dir` is created if it does not exist. The schematic itself need
    /// not exist yet — the agent's first `apply_design(commit:true)` writes it.
    /// This is the constructor the headless `gordian agent` subcommand uses.
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
        let provider = SymbolTable::from_env(&env);
        Some(Self {
            env,
            project_dir,
            sch_path,
            provider,
            snapshots,
            index: OnceLock::new(),
            footprint_index: OnceLock::new(),
            footprint_dir_override: None,
            workspace,
            kicad: std::sync::Mutex::new(None),
            _tempdir: Some(tempdir),
        })
    }

    /// Build a context over a fresh temporary project whose **footprint index**
    /// is sourced from `footprint_dir` (a directory of `.pretty` libraries)
    /// instead of an installed KiCAD share dir. This is the footprint-tool test
    /// entry point: it needs no KiCAD installation, so the PCB tools can be
    /// exercised against the vendored fixtures on any machine.
    ///
    /// The symbol-side fields still point at a (possibly absent) real KiCAD env
    /// via [`KicadEnv::detect`]; tests that only touch footprint tools never
    /// reach them. Returns `None` only if a tempdir or the workspace cannot be
    /// created.
    pub fn with_footprint_dir_for_test(footprint_dir: PathBuf) -> Option<Self> {
        // Symbol-side env is a placeholder (footprint tests never touch it);
        // the footprint index is built from `footprint_dir`, not from `env`.
        let env = KicadEnv::detect()
            .unwrap_or_else(|| KicadEnv::with_symbol_dir(PathBuf::from("/nonexistent")));
        let tempdir = tempfile::tempdir().ok()?;
        let project_dir = tempdir.path().to_path_buf();
        let sch_path = project_dir.join("project.kicad_sch");
        let snapshots = SnapshotStore::for_project(&project_dir).ok()?;
        let workspace = crate::workspace::Workspace::for_project(&project_dir).ok()?;
        let provider = SymbolTable::from_env(&env);
        Some(Self {
            env,
            project_dir,
            sch_path,
            provider,
            snapshots,
            index: OnceLock::new(),
            footprint_index: OnceLock::new(),
            footprint_dir_override: Some(footprint_dir),
            workspace,
            kicad: std::sync::Mutex::new(None),
            _tempdir: Some(tempdir),
        })
    }

    /// The Layout IR for `design`: the connectivity-driven frame inferred from
    /// the netlist (rails, anchor order, satellite placement, ports, mirror).
    /// Pure and cheap, so it is just recomputed each call (no cache — a stale
    /// fingerprint would silently ignore `layout:`/`power:` edits).
    fn layout_for(&self, design: &Design) -> LayoutIr {
        infer_ir(&self.env, design)
    }

    /// The project's `.kicad_sch` path (may not exist).
    pub fn sch_path(&self) -> &Path {
        &self.sch_path
    }

    /// The project's `.kicad_pcb` path: the schematic path with a `.kicad_pcb`
    /// extension (`design.kicad_sch` → `design.kicad_pcb`), the board the PCB
    /// tools export to. May not exist yet.
    pub fn pcb_path(&self) -> PathBuf {
        self.sch_path.with_extension("kicad_pcb")
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
    pub fn provider(&self) -> &SymbolTable {
        &self.provider
    }

    /// The snapshot / undo store for this project.
    pub fn snapshots(&self) -> &SnapshotStore {
        &self.snapshots
    }

    /// The project's `.gordian/` persistent state.
    pub fn workspace(&self) -> &crate::workspace::Workspace {
        &self.workspace
    }

    /// The live KiCAD IPC session (guarded). `open_board` installs one;
    /// interactive geometry tools take `guard.as_mut()`.
    pub(crate) fn kicad(&self) -> std::sync::MutexGuard<'_, Option<kicad_ipc::Session>> {
        self.kicad.lock().expect("kicad session mutex poisoned")
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

    /// The cross-library footprint index, built once and cached.
    ///
    /// Building scans every installed `.pretty` library (155 of them) — far
    /// dearer than the symbol scan — so the result is memoized like
    /// [`Self::index`]. When a footprint-dir override is set (the test
    /// constructor), the index is built from that directory of `.pretty`
    /// libraries instead of the installed KiCAD footprint share dir.
    pub(crate) fn footprint_index(&self) -> Result<&FootprintIndex> {
        if let Some(idx) = self.footprint_index.get() {
            return Ok(idx);
        }
        let idx = match &self.footprint_dir_override {
            Some(dir) => FootprintIndex::build_from_dir(dir)
                .with_context(|| format!("building footprint index from {}", dir.display()))?,
            None => FootprintIndex::build(&self.env).context("building footprint index")?,
        };
        let _ = self.footprint_index.set(idx);
        Ok(self.footprint_index.get().expect("footprint index just set"))
    }
}

/// The JSON-Schema definitions for every tool, in a stable order. The
/// [`crate::Agent`] loop hands these to the model.
pub fn tool_defs() -> Vec<ToolDef> {
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
                name: "review_design".into(),
                description: "Get an INDEPENDENT electrical-correctness review of the \
                    current design. A FRESH reviewer (no memory of your work, so it \
                    won't rationalise your choices) plus a deterministic exact-math ERC \
                    audit the netlist for FUNCTIONAL faults that pass ERC and look clean \
                    but are electrically wrong: pin-function mis-wires (a bus signal on \
                    the wrong device pin), a part on the wrong voltage rail, a feedback \
                    divider set for the wrong output voltage, reversed polarity, a missing \
                    essential part (crystal load caps, regulator output cap). Returns a \
                    score (0-10) and a list of high-confidence defects. STRONGLY \
                    RECOMMENDED once your design is complete (before you finish): call it, \
                    fix any defects with edit_design, then re-check. It reviews the current \
                    draft, so you can run it before committing."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "intent": { "type": "string",
                            "description": "What the circuit is supposed to do (the design goal), for the reviewer's context. Be specific about rails, key parts, and interfaces." }
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
                name: "find_similar_designs".into(),
                description: "Study REAL professional KiCAD schematics that match your \
                    design intent, returned as circuit-YAML you can emulate. Given a \
                    one-line intent (e.g. \"STM32 board with USB and a 3V3 regulator\"), \
                    this ranks a corpus of human-authored designs and returns the top \
                    matches — each with its description, origin repo, and (when it lifts \
                    cleanly) its full circuit-YAML — so you can copy professional patterns: \
                    how to PARTITION into blocks, place decoupling, wire idioms (crystal + \
                    load caps, regulator in/out caps), and which parts pros actually use. \
                    Call this BEFORE authoring a new design to ground yourself in real \
                    references. Some human schematics won't lift to YAML (exotic libs / \
                    hierarchy); those still return their description, and lift_success_rate \
                    reports how many produced YAML. Returns empty when no reference corpus \
                    is installed — that is fine, just design from first principles."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "intent": { "type": "string",
                            "description": "One-line description of what you want to design, \
                                e.g. \"STM32 microcontroller board with USB and 3V3 regulator\"." },
                        "k": { "type": "integer",
                            "description": "How many references to return (default 3).", "minimum": 1 }
                    },
                    "required": ["intent"]
                }),
            },
            ToolDef {
                name: "render_schematic".into(),
                description: "Render the current schematic to a PNG image and \
                    return it so you can SEE the sheet. Use after apply_design \
                    to inspect layout quality: overlapping text, crowding, \
                    confusing arrangement. The PNG is also saved under \
                    .gordian/renders/."
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
            // ── PCB tools (slice 5) ─────────────────────────────────────────
            ToolDef {
                name: "search_footprints".into(),
                description: "Search every installed KiCAD footprint library by \
                    name and return the best matches as fully-qualified \
                    `Lib:Name` ids with their pad counts. Use this to find the \
                    real footprint lib_id for a part before putting it on a board \
                    — NEVER guess a footprint lib_id. The pad count is the number \
                    of pads you must assign nets to in derive_board."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string",
                            "description": "Footprint name or fragment, e.g. \"R_0603\", \"SOT-23\", \"PinHeader 1x02 2.54\"." },
                        "limit": { "type": "integer",
                            "description": "Max hits to return (default 8).", "minimum": 1 }
                    },
                    "required": ["query"]
                }),
            },
            ToolDef {
                name: "get_footprint_info".into(),
                description: "Return the pad NUMBER list (use these to build the pad_nets \
                    map for derive_board) plus a compact shape summary — pad_count, \
                    min_pitch_mm, pad dimensions, pad technologies, the courtyard rectangle, \
                    and the bounding box — for a fully-qualified `Lib:Name` footprint. \
                    (Per-pad coordinates are summarized, not listed: the engine places pads, \
                    not you.) If the lib_id is unknown, returns an error with the closest \
                    known suggestions — never guess the id."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "lib_id": { "type": "string",
                            "description": "Fully-qualified footprint id, e.g. \"Resistor_SMD:R_0603_1608Metric\" or \"Package_TO_SOT_SMD:SOT-23\"." }
                    },
                    "required": ["lib_id"]
                }),
            },
            ToolDef {
                name: "assign_footprint".into(),
                description: "Set a part's footprint in the board draft (fills a part whose \
                    schematic symbol carried no footprint). The footprint must have every pad the \
                    part nets (checked). Find lib_ids with search_footprints — never guess. Run \
                    after derive_board, before place_board."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "reference": { "type": "string", "description": "Part reference, e.g. \"U1\"." },
                        "footprint": { "type": "string", "description": "Footprint lib_id, e.g. \"Package_SO:SOIC-8_3.9x4.9mm_P1.27mm\"." }
                    },
                    "required": ["reference", "footprint"]
                }),
            },
            ToolDef {
                name: "open_board".into(),
                description: "Open the exported .kicad_pcb in a LIVE headless KiCAD for INTERACTIVE \
                    editing over IPC. After this you edit the REAL board directly — move_part, \
                    route_track, set_net_width — with board_state to read it and render_board to see \
                    it. Requires an exported board (derive_board -> [assign_footprint] -> place_board \
                    -> route_board -> export_board). Returns the board state."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            ToolDef {
                name: "board_state".into(),
                description: "Read the live (open) board: every part's reference + position (mm), the \
                    track count, and the net list. Inspect before/after an interactive edit. Requires open_board."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            ToolDef {
                name: "move_part".into(),
                description: "Move a part to (x, y) mm (optional rotation degrees) on the live board — \
                    direct geometry control for thermal / decoupling / length-match placement. Requires open_board."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "reference": { "type": "string", "description": "Part reference, e.g. \"U1\"." },
                        "x": { "type": "number", "description": "X position (mm)." },
                        "y": { "type": "number", "description": "Y position (mm)." },
                        "rotation": { "type": "number", "description": "Optional rotation (degrees)." }
                    },
                    "required": ["reference", "x", "y"]
                }),
            },
            ToolDef {
                name: "route_track".into(),
                description: "Route a straight copper track on the live board: start/end as [x,y] mm, \
                    width mm, a copper layer (F.Cu/B.Cu/In1.Cu/...), optionally on a net. WIDTH is the \
                    engineering lever — fat copper for power/high current, thin for signals. Requires open_board."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "start": { "type": "array", "items": {"type":"number"}, "description": "[x, y] mm." },
                        "end": { "type": "array", "items": {"type":"number"}, "description": "[x, y] mm." },
                        "width": { "type": "number", "description": "Track width (mm). Default 0.2." },
                        "layer": { "type": "string", "description": "Copper layer: F.Cu, B.Cu, In1.Cu, ... Default F.Cu." },
                        "net": { "type": "string", "description": "Optional net name to assign." }
                    },
                    "required": ["start", "end"]
                }),
            },
            ToolDef {
                name: "set_net_width".into(),
                description: "Define (or update) a net class with a track width + clearance (mm) and \
                    assign nets to it — the idiomatic \"wide copper for power\" lever (e.g. widen \
                    GND/VCC/VIN). Requires open_board. (Per-track widths are also settable via route_track.)"
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Net class name, e.g. \"Power\"." },
                        "width": { "type": "number", "description": "Track width (mm). Default 0.5." },
                        "clearance": { "type": "number", "description": "Clearance (mm). Default 0.2." },
                        "nets": { "type": "array", "items": {"type":"string"}, "description": "Net names, e.g. [\"GND\",\"VCC\"]." }
                    },
                    "required": ["name", "nets"]
                }),
            },
            ToolDef {
                name: "derive_board".into(),
                description: "Seed the board from the committed schematic: reads the parts + \
                    netlist (pin->pad is KiCAD's) and builds the board draft — one part per \
                    component with its pad->net map and the footprint taken from the symbol. \
                    Requires a committed .kicad_sch (run apply_design first). `missing_footprints` \
                    lists parts whose symbol had no footprint — set each with assign_footprint. \
                    Then place_board -> route_board -> export_board -> open_board to refine \
                    interactively. Optional `bounds` seeds the outline (mm); `rules.layers` the \
                    copper layer count; overwrite=true replaces an existing draft."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "bounds": {
                            "type": "object",
                            "description": "Optional: seed the board outline (mm, y-down).",
                            "properties": {
                                "min_x": { "type": "number" }, "max_x": { "type": "number" },
                                "min_y": { "type": "number" }, "max_y": { "type": "number" }
                            }
                        },
                        "rules": { "type": "object",
                            "description": "Optional: {layers: 2|4} seeds the copper layer count." },
                        "overwrite": { "type": "boolean", "description": "Replace an existing board draft." }
                    }
                }),
            },
            ToolDef {
                name: "get_board".into(),
                description: "Return the current board draft (parts as \
                    reference/footprint/lock + a pad_count — the full per-pad net map you \
                    passed to derive_board is summarized, not echoed) plus a derived \
                    summary: part count, net count, the per-net pin counts, the keepout \
                    count, and whether the board has been placed / routed yet. Use this to \
                    inspect board state before placing or routing, or to confirm a \
                    derive_board / triage edit took effect."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            ToolDef {
                name: "place_board".into(),
                description: "Place the current board: turn every part's footprint \
                    + design rules + any locked positions into a placement problem, \
                    apply the stored placement hints, and run the deterministic \
                    placer. The placement is persisted to the board draft (route_board \
                    and render_board read it). Returns legal (true iff no courtyard \
                    overlap and all parts in bounds), the HPWL wirelength metric, how \
                    many overlaps the legalizer resolved / parts it clamped, and the \
                    per-part positions [{reference, x, y, rotation}]. NOTE: keepouts do \
                    NOT affect placement in v1 — they only block ROUTING (route_board). \
                    Run derive_board first; an unplaceable (too-tight) board returns \
                    legal=false with a note on how to relax it."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            ToolDef {
                name: "route_board".into(),
                description: "Route the placed board: build the routing problem from \
                    the placement, add the keepouts as blocking obstacles, and run the \
                    auto-router (detailed pipeline with a naive fallback). Requires a \
                    placement — run place_board first (else a recoverable error). The \
                    full solution is persisted for export; the result returns: router \
                    (\"detailed\"/\"naive\"), failed nets [{connection, reason}] with \
                    stage provenance (global:/assign:/cell:/finisher:), metrics \
                    (wirelength, vias, traces), and lint_summary (DRC violation counts \
                    by kind — EXPECTED ZERO; a non-zero count sets engine_bug=true and \
                    is an engine fault, not a board you can fix). When nets fail, a \
                    congestion report (iterations + edge hotspots) is included to guide \
                    triage — re-seed with a bigger outline or more layers (derive_board \
                    overwrite=true), or refine interactively after open_board."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            ToolDef {
                name: "autoroute".into(),
                description: "Auto-route the exported board with the FREEROUTING autorouter — the \
                    heavy-duty assist for dense boards (BGA/QFP fan-out) the in-house route_board \
                    can't escape. Routes from scratch at the board's design rules, writes the \
                    routed copper back to the .kicad_pcb, and reports copper DRC + unconnected \
                    counts. Requires an exported (placed) board (derive_board → place_board → \
                    export_board → autoroute). Use this instead of route_board when route_board \
                    leaves many nets failed on a dense board; then open_board to inspect/refine."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            ToolDef {
                name: "render_board".into(),
                description: "Render the board to a PNG image and attach it so you can \
                    SEE the board. Call this AFTER place_board to inspect part positions \
                    and AFTER route_board to inspect the copper. Two views: \
                    view=\"placed\" shows part courtyards + pads coloured by net + region \
                    hints (dashed blue) + keepout/obstacle rectangles in dark grey; \
                    view=\"routed\" shows the full copper + vias + failed-net highlights. \
                    Colour key (routed view): red = top-layer trace, blue = bottom-layer \
                    trace, orange cross = failed net endpoint (route that net differently). \
                    When view is omitted the default is \"routed\" if route.json exists, \
                    \"placed\" otherwise. Requires place_board (placed view) or route_board \
                    (routed view); missing state returns a recoverable error. The PNG is \
                    also saved under .gordian/renders/."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "view": {
                            "type": "string",
                            "enum": ["placed", "routed"],
                            "description": "Which view to render: \"placed\" (part positions, \
                                courtyards, pads, region hints) or \"routed\" (full copper, \
                                vias, failed-net highlights). Omit for auto (routed if routed, \
                                else placed)."
                        }
                    }
                }),
            },
            ToolDef {
                name: "export_board".into(),
                description: "Export the placed + routed board to a .kicad_pcb file. \
                    Synthesizes the board from the engine placement (footprints + \
                    per-pad nets + a board outline) and splices the routed copper onto \
                    it. Requires a placed AND routed board — run place_board then \
                    route_board first (else a recoverable error). When a KiCAD 8+ CLI \
                    is available it runs `kicad-cli pcb drc` and returns the counts \
                    (copper_violations, unconnected_items; lib_footprint_mismatch \
                    warnings are a tolerated library-bookkeeping carve-out, not a copper \
                    fault); otherwise DRC is skipped with a note. Default output path is \
                    the project's <stem>.kicad_pcb next to the schematic."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Output .kicad_pcb path. Omit to write the \
                                project's default <stem>.kicad_pcb next to the schematic."
                        }
                    }
                }),
            },
            ToolDef {
                name: "export_fab".into(),
                description: "Bundle the routed board into a manufacturable FAB deliverable: \
                    Gerbers (one *.gbr per copper/mask/silk/edge layer), an Excellon drill set \
                    (separate plated/non-plated files + drill maps), a CSV pick-and-place \
                    (component positions), and — when the project has a schematic — a grouped \
                    BOM CSV. Everything lands in a single fab/ directory you hand to a board \
                    house. Call this LAST, AFTER export_board has written the .kicad_pcb \
                    (place_board → route_board → export_board → export_fab); if no board file \
                    exists it returns a recoverable error pointing at export_board. Returns the \
                    fab directory and the produced file list."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Input .kicad_pcb to bundle. Omit to use the \
                                project's default <stem>.kicad_pcb (what export_board wrote)."
                        },
                        "out_dir": {
                            "type": "string",
                            "description": "Output directory for the bundle. Omit for the \
                                project's default fab/ directory."
                        }
                    }
                }),
            },
    ]
}

/// Dispatch a tool by name (synchronous). `input` is the model-supplied JSON
/// arguments; the returned `Value` is fed back to the model. The [`crate::Agent`]
/// loop off-loads this onto the blocking pool.
pub fn run_tool(name: &str, input: Value, ctx: &PcbToolCtx) -> Result<Value> {
    match name {
        "search_symbols" => search_symbols(input, ctx),
            "get_symbol_info" => get_symbol_info(input, ctx),
            "get_design" => get_design(ctx),
            "validate_design" => validate_design(input, ctx),
            "apply_design" => apply_design(input, ctx),
            "run_erc" => run_erc(ctx),
            "project_info" => project_info(ctx),
            "read_schematic" => read_schematic(input, ctx),
            "find_similar_designs" => find_similar_designs(input, ctx),
            "render_schematic" => render_schematic(ctx),
            "create_design" => create_design(input, ctx),
            "edit_design" => edit_design(input, ctx),
            "search_footprints" => crate::tools_pcb::search_footprints(input, ctx),
            "get_footprint_info" => crate::tools_pcb::get_footprint_info(input, ctx),
            "derive_board" => crate::tools_pcb::derive_board(input, ctx),
            "assign_footprint" => crate::tools_pcb::assign_footprint(input, ctx),
            "get_board" => crate::tools_pcb::get_board(ctx),
            "place_board" => crate::tools_pcb::place_board(input, ctx),
            "route_board" => crate::tools_pcb::route_board(input, ctx),
            "autoroute" => crate::tools_pcb::autoroute(input, ctx),
            "export_board" => crate::tools_pcb::export_board(input, ctx),
            "export_fab" => crate::tools_pcb::export_fab(input, ctx),
            "open_board" => crate::tools_pcb::open_board(input, ctx),
            "board_state" => crate::tools_pcb::board_state(ctx),
            "move_part" => crate::tools_pcb::move_part(input, ctx),
            "route_track" => crate::tools_pcb::route_track(input, ctx),
            "set_net_width" => crate::tools_pcb::set_net_width(input, ctx),
            "render_board" => crate::tools_pcb::render_board(input, ctx),
            other => bail!("unknown tool: {other}"),
        }
}

/// Pull a required string field out of the input, with a clear error.
pub(crate) fn require_str(input: &Value, key: &str) -> Result<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing required string field `{key}`"))
}

// ── 1. search_symbols ──────────────────────────────────────────────────────

fn search_symbols(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
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

fn get_symbol_info(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
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

fn current_sch_text(ctx: &PcbToolCtx) -> Option<String> {
    std::fs::read_to_string(&ctx.sch_path).ok()
}

fn get_design(ctx: &PcbToolCtx) -> Result<Value> {
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

fn validate_design(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
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

fn apply_design(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
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

    // Prior design (for the diff), lifted from the existing schematic.
    let prior_design = if ctx.sch_path.exists() {
        let prior_yaml = lift(&ctx.env, &ctx.sch_path)
            .with_context(|| format!("lifting prior {}", ctx.sch_path.display()))?;
        compile(&prior_yaml, &ctx.provider).design
    } else {
        None
    };

    // The floorplan engine re-lays-out from scratch via the connectivity-driven
    // inferred IR; the human-style layout always re-flows the whole sheet.
    let ir = ctx.layout_for(&design);
    // Production uses the locality-aware ANNEAL search (strictly ≥ greedy via the
    // candidate pick) so generated boards get the premium placement, not the
    // env-defaulted greedy free tier.
    let emitted = sch_floorplan::floorplan::emit_strategy(&ctx.env, &design, &ir, Box::new(anneal_place::Anneal))
        .context("rendering schematic")?;
    let rendered = emitted.sch;
    let diff = design_diff(prior_design.as_ref(), &design);

    // Idioms the engine recognized + co-placed (crystal, decoupling, …), surfaced so
    // the LLM can confirm the layout matched its intent — detection is automatic from
    // the netlist, no new authoring syntax.
    let detected_idioms = serde_json::to_value(&emitted.detected_idioms).unwrap_or(json!([]));

    if !commit {
        return Ok(json!({
            "ok": true,
            "would_write": true,
            "stale_draft_warning": stale,
            "diff": diff,
            "rendered_len": rendered.len(),
            "layout_warnings": emitted.layout_warnings,
            "wire_through_body": emitted.crossings.body + emitted.crossings.ic,
            "detected_idioms": detected_idioms,
        }));
    }

    // Commit path: snapshot the prior (if any), write, then ERC.
    if ctx.sch_path.exists() {
        ctx.snapshots
            .snapshot(&ctx.sch_path)
            .with_context(|| format!("snapshotting {}", ctx.sch_path.display()))?;
    }
    // ANY multi-block design ships as ONE COMPOSED sheet: each functional block is laid out
    // independently (8-9 each), then the block regions are tiled onto a single enlarged page
    // as labeled bounding boxes — per-block independent layout is the RULE, not a dense-only
    // special case. `multisheet::refine_blocks` first normalizes the blocks (split
    // over-crammed, merge tiny) so even a 2-block design lays out per-block with no
    // cross-border global SA; cross-block nets join via matching global labels on the one
    // sheet. Only a single-block design takes the plain single-sheet emit. The composed
    // .kicad_sch is written at ctx.sch_path; downstream render/ERC operate on it.
    let n_blocks = design.blocks.values().filter(|b| !b.components.is_empty()).count();
    let multisheet = n_blocks >= 2;
    if multisheet {
        let dir = ctx.sch_path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let root = crate::multisheet::compose_single_sheet(&ctx.env, &design, dir)
            .context("composing single-sheet schematic")?;
        if root != ctx.sch_path {
            std::fs::rename(&root, &ctx.sch_path).with_context(|| {
                format!("placing composed sheet at {}", ctx.sch_path.display())
            })?;
        }
    } else {
        std::fs::write(&ctx.sch_path, &rendered)
            .with_context(|| format!("writing {}", ctx.sch_path.display()))?;
    }

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
        "layout_warnings": emitted.layout_warnings,
        "wire_through_body": emitted.crossings.body + emitted.crossings.ic,
        "detected_idioms": detected_idioms,
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

fn project_info(ctx: &PcbToolCtx) -> Result<Value> {
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

fn read_schematic(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
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

// ── find_similar_designs (retrieval-augmented references) ───────────────────

/// Rank the corpus of real human schematics against `intent` and return the
/// top-`k` as circuit-YAML the model can emulate. Absent-safe: with no corpus
/// installed it returns `{matches: [], note: ...}` rather than erroring.
fn find_similar_designs(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
    use crate::retrieval::{Corpus, DEFAULT_K};

    let intent = require_str(&input, "intent")?;
    let k = input
        .get("k")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_K)
        .max(1);

    let corpus = Corpus::discover();
    if corpus.is_empty() {
        return Ok(json!({
            "matches": [],
            "note": "no reference corpus installed — design from first principles \
                     (set GORDIAN_CORPUS_DIR to a dataset of *.kicad_sch + *.json to \
                     enable references)",
        }));
    }

    let report = corpus.find_similar(&ctx.env, &intent, k);
    let matches: Vec<Value> = report
        .references
        .iter()
        .map(|r| {
            let mut m = json!({
                "id": r.meta.id,
                "repo": r.meta.repo,
                "description": r.meta.description,
                "score": r.score,
            });
            match (&r.yaml, &r.lift_error) {
                (Some(yaml), _) => m["yaml"] = json!(yaml),
                (None, Some(err)) => m["lift_error"] = json!(err),
                (None, None) => {}
            }
            m
        })
        .collect();

    Ok(json!({
        "matches": matches,
        "corpus_size": corpus.len(),
        "ranked_total": report.ranked_total,
        "lifted_ok": report.successes,
        "lift_success_rate": report.lift_success_rate(),
        "note": "circuit-YAML references from real human designs — study their block \
                 partition, decoupling, and idioms; do NOT copy verbatim. Entries with \
                 only a description + lift_error could not be lifted.",
    }))
}

// ── 8. run_erc ─────────────────────────────────────────────────────────────

fn run_erc(ctx: &PcbToolCtx) -> Result<Value> {
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

fn create_design(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
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

fn edit_design(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
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
        draft.replace(&*old, &new)
    } else {
        draft.replacen(&*old, &new, 1)
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

/// Long-edge pixel cap for rendered schematics / board renders (Claude vision sweet spot).
pub(crate) const RENDER_MAX_PX: u32 = 1600;

fn render_schematic(ctx: &PcbToolCtx) -> Result<Value> {
    if !ctx.sch_path.exists() {
        return Ok(json!({
            "error": "no schematic yet — apply a design first",
        }));
    }
    let png = crate::render::schematic_png(&ctx.env, &ctx.sch_path)?;
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
