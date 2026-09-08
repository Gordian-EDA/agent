//! The design loop: prompt -> tool calls -> a checked, reviewed `.kicad_sch`.
//!
//! A port of `schagent/agent.py`. The model never computes coordinates: it
//! searches the symbol library, writes a netlist plus layout trees, and `build`
//! lays out, compiles and checks the sheet. `erc`, `render` and `review` are the
//! feedback it acts on, and `finish` is gated on all three.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gordian_llm::{
    Binary, ChatMessage, ContentPart, MessageContent, Provider, StreamEnd, ToolCall, ToolResponse,
    completed_text, token_usage,
};
use gordian_runtime::logging::EVENTS_TARGET;
use serde_json::{Value, json};

use crate::critic::{self, Review};
use crate::engines::sch;
use crate::prompt::system_prompt;
use crate::render;
use crate::tools::tool_defs;

/// How the loop is bounded: builds, wall clock and whether the polish pass runs.
#[derive(Clone, Debug)]
pub struct Budget {
    /// Hard ceiling on `build` calls.
    pub max_builds: usize,
    /// Wall clock for the whole run, schematic and PCB together.
    pub total: Duration,
    /// Share of `total` the design loop may take before it is told to finish.
    pub loop_share: f64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_builds: 6,
            total: Duration::from_secs(180),
            loop_share: 0.6,
        }
    }
}

impl Budget {
    /// When the design loop must have stopped asking for more builds.
    fn loop_deadline(&self) -> Duration {
        self.total.mul_f64(self.loop_share)
    }

    /// When the loop is abandoned mid-conversation, whatever the model is doing.
    fn hard_deadline(&self) -> Duration {
        self.total.mul_f64((self.loop_share + 1.0) / 2.0)
    }
}

/// Token and request totals over one run.
#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
}

impl Usage {
    fn add(&mut self, end: &StreamEnd) -> (u64, u64, u64) {
        let (input, output, cache_write, cache_read) = token_usage(end);
        self.requests += 1;
        self.input += input;
        self.output += output;
        self.cache_write += cache_write;
        self.cache_read += cache_read;
        (input, output, cache_read)
    }
}

/// What the run produced.
pub struct Outcome {
    /// The delivered sheet, when one was built.
    pub sheet: Option<PathBuf>,
    /// The design JSON behind [`Self::sheet`].
    pub design: Option<Value>,
    /// The best review of a clean build, when one was graded.
    pub review: Option<Review>,
    /// KiCad ERC violation lines on the delivered sheet.
    pub erc: Vec<String>,
    /// Checker issues on the LAST build; a delivered sheet is a clean one, so
    /// this is what the model was still looking at when the clock ran out.
    pub issues: Vec<String>,
    /// Which work-directory build [`Self::sheet`] is a copy of. A downstream
    /// stage started early from a build can be reused when this names it.
    pub source: Option<PathBuf>,
    pub builds: usize,
    pub summary: String,
    pub usage: Usage,
    pub seconds: f64,
}

/// Told the sheet of every clean build, so a deterministic downstream stage can
/// start while the model is still running ERC and the review on it.
pub type OnCleanBuild<'a> = &'a (dyn Fn(&Path) + Sync);

/// One design session over a project directory.
pub struct Agent<'a> {
    client: &'a dyn Provider,
    lib: sch::Library,
    kicad_cli: PathBuf,
    project_dir: PathBuf,
    out_sch: PathBuf,
    work_dir: PathBuf,
    budget: Budget,
    log: std::fs::File,

    builds: usize,
    last_build: Option<PathBuf>,
    last_ok_build: Option<PathBuf>,
    last_design: Option<Value>,
    last_raw: Value,
    last_report: Option<sch::BuildReport>,
    erc_of: Option<PathBuf>,
    erc_lines: Vec<String>,
    erc_errors: usize,
    rendered_of: Option<PathBuf>,
    sheet: Option<render::Sheet>,
    reviewed_of: Option<PathBuf>,
    review: Option<Review>,
    best: Option<(f64, PathBuf, Value)>,
    on_clean_build: Option<OnCleanBuild<'a>>,
    usage: Usage,
    started: Instant,

    base: Option<sch::Design>,
    base_json: Value,
    base_sheet: Option<render::Sheet>,
    base_nets: BTreeMap<String, BTreeSet<String>>,
    baseline_issues: BTreeSet<String>,
}

impl<'a> Agent<'a> {
    /// Open a session. `project_dir` holding a `design.kicad_sch` puts the run in
    /// edit mode, where `build` takes a patch against that sheet.
    pub fn new(
        client: &'a dyn Provider,
        symbol_dir: &Path,
        kicad_cli: &Path,
        project_dir: &Path,
        sch_name: &str,
        budget: Budget,
    ) -> Result<Self> {
        let work_dir = project_dir.join(".gordian/work");
        std::fs::create_dir_all(&work_dir)
            .with_context(|| format!("creating {}", work_dir.display()))?;
        let out_sch = project_dir.join(sch_name);
        let lib = sch::Library::load(symbol_dir).context("indexing the KiCad symbol libraries")?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(work_dir.join("log.jsonl"))?;
        Ok(Self {
            client,
            lib,
            kicad_cli: kicad_cli.to_path_buf(),
            project_dir: project_dir.to_path_buf(),
            out_sch,
            work_dir,
            budget,
            log,
            builds: 0,
            last_build: None,
            last_ok_build: None,
            last_design: None,
            last_raw: Value::Null,
            last_report: None,
            erc_of: None,
            erc_lines: Vec::new(),
            erc_errors: 0,
            rendered_of: None,
            sheet: None,
            reviewed_of: None,
            review: None,
            best: None,
            on_clean_build: None,
            usage: Usage::default(),
            started: Instant::now(),
            base: None,
            base_json: Value::Null,
            base_sheet: None,
            base_nets: BTreeMap::new(),
            baseline_issues: BTreeSet::new(),
        })
    }

    /// Call `hook` with each clean build's sheet, before ERC and the review are
    /// run on it, so the board stage always works on the newest netlist and a
    /// later build does not push the whole board into the last seconds of the run.
    pub fn on_clean_build(&mut self, hook: OnCleanBuild<'a>) {
        self.on_clean_build = Some(hook);
    }

    /// Whether the project already carries a sheet, which puts `build` in patch mode.
    pub fn edit_mode(&self) -> bool {
        self.base.is_some()
    }

    /// Load the existing sheet so `build` patches it instead of replacing it.
    ///
    /// The model is shown that sheet as raw JSON with an id on every element and
    /// a `touches` list on every wire, plus its render, and the issues the sheet
    /// already had are subtracted from every later build report.
    pub fn open_existing(&mut self) -> Result<()> {
        if !self.out_sch.is_file() {
            return Ok(());
        }
        let base = sch::extract(&self.out_sch)?;
        let mut json = base.to_json(true);
        sch::check::annotate_wires(&mut json, Some(&base));
        self.base_json = json;
        if let Ok(report) = sch::build_patch(
            &self.lib,
            &base,
            &json!({}),
            &self.work_dir.join("v0.kicad_sch"),
        ) {
            self.baseline_issues = report.issues.iter().cloned().collect();
            self.base_nets = report.netlist.clone();
        }
        let stem = self.work_dir.join("current");
        self.base_sheet = render::sheet(
            &self.kicad_cli,
            &self.out_sch,
            &stem.with_extension("png"),
            &self.work_dir.join("current_grid.png"),
        )
        .ok();
        self.base = Some(base);
        Ok(())
    }

    /// Run the loop to a finish, a build ceiling or the loop deadline.
    pub async fn run(&mut self, prompt: &str, skills: &str) -> Result<Outcome> {
        self.started = Instant::now();
        let system = system_prompt(self.edit_mode());
        let defs = tool_defs(self.edit_mode());
        let mut messages = vec![ChatMessage::user(self.opening_message(prompt, skills))];
        let mut summary = String::new();
        let mut nudged = false;

        loop {
            let request_started = Instant::now();
            let end = self.client.complete(&system, &messages, &defs).await?;
            let (input, output, cached) = self.usage.add(&end);
            event(format!(
                "usage: request #{} in={input} out={output} cached={cached} latency={:.1}s",
                self.usage.requests,
                request_started.elapsed().as_secs_f64()
            ));
            let text = completed_text(&end);
            if !text.trim().is_empty() {
                event(format!("assistant: {}", one_line(&text, 2000)));
            }
            let calls = end.captured_into_tool_calls().unwrap_or_default();
            if calls.is_empty() {
                if self.last_build.is_none() && !nudged {
                    nudged = true;
                    messages.push(ChatMessage::assistant(text));
                    messages.push(ChatMessage::user(
                        "Continue: use the tools. Call `build` with the design JSON.",
                    ));
                    continue;
                }
                summary = text;
                break;
            }
            messages.push(ChatMessage::from(calls.clone()));

            let mut images: Vec<(String, Binary)> = Vec::new();
            let mut finished = None;
            for call in &calls {
                let started = Instant::now();
                event(format!("tool -> {} {}", call.fn_name, call_args(call)));
                let (out, image) = self.dispatch(call, &mut finished).await;
                event(format!(
                    "tool <- {} (elapsed {:.1}s): {}",
                    call.fn_name,
                    started.elapsed().as_secs_f64(),
                    one_line(&out, 400)
                ));
                self.record(&call.fn_name, &out);
                messages.push(ChatMessage::from(ToolResponse::from_tool_call(
                    call,
                    out.clone(),
                )));
                if let Some(png) = image {
                    images.push((
                        format!("Render of build {} (grid units on the axes):", self.builds),
                        png,
                    ));
                }
            }
            for (caption, png) in images {
                messages.push(ChatMessage::user(MessageContent::from_parts(vec![
                    ContentPart::from_text(caption),
                    ContentPart::Binary(png),
                ])));
            }
            if let Some(text) = finished {
                summary = text;
                break;
            }
            let elapsed = self.started.elapsed();
            if elapsed > self.budget.hard_deadline() {
                event("schematic: wall clock reached, delivering the best build so far");
                break;
            }
            if elapsed > self.budget.loop_deadline() {
                messages.push(ChatMessage::user(
                    "[time budget reached] Stop polishing. If `erc` still reports an ERROR, fix \
                     that one thing and build again - nothing else. Otherwise call `finish` now \
                     with a one-paragraph summary of what the sheet contains and what is still \
                     imperfect.",
                ));
            }
        }

        self.deliver(summary)
    }

    /// The first user message: the request, any matching skill starters, and in
    /// edit mode the current sheet as raw JSON plus its render.
    fn opening_message(&self, prompt: &str, skills: &str) -> MessageContent {
        let mut parts = vec![ContentPart::from_text(prompt.to_string())];
        if !skills.trim().is_empty() {
            parts.push(ContentPart::from_text(skills.to_string()));
        }
        if let Some(base) = self.base.as_ref() {
            parts.push(ContentPart::from_text(format!(
                "CURRENT DESIGN (raw JSON with element ids, absolute grid units, paper {}):\n{}",
                base.paper, self.base_json
            )));
            if !self.baseline_issues.is_empty() {
                parts.push(ContentPart::from_text(format!(
                    "Pre-existing issues in this design (ignored by the checker):\n- {}",
                    self.baseline_issues
                        .iter()
                        .take(30)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n- ")
                )));
            }
            if let Some(sheet) = self.base_sheet.as_ref() {
                parts.push(ContentPart::from_text(
                    "Current render of the schematic:".to_string(),
                ));
                parts.push(ContentPart::Binary(binary(&sheet.grid)));
            }
        }
        MessageContent::from_parts(parts)
    }

    async fn dispatch(
        &mut self,
        call: &ToolCall,
        finished: &mut Option<String>,
    ) -> (String, Option<Binary>) {
        let args = &call.fn_arguments;
        match call.fn_name.as_str() {
            "search_symbols" => (self.t_search(args["query"].as_str().unwrap_or("")), None),
            "symbol_info" => (
                self.t_symbol_info(
                    args["lib_id"].as_str().unwrap_or(""),
                    args.get("unit").and_then(Value::as_u64).map(|u| u as u32),
                ),
                None,
            ),
            "build" => {
                let design = args
                    .get("design")
                    .or_else(|| args.get("patch"))
                    .cloned()
                    .unwrap_or_else(|| args.clone());
                (self.t_build(&design), None)
            }
            "erc" => (self.t_erc(), None),
            "render" => self.t_render(),
            "review" => self.t_review().await,
            "finish" => (self.t_finish(args, finished), None),
            other => (format!("unknown tool {other}"), None),
        }
    }

    // ---- tools ---------------------------------------------------------

    fn t_search(&self, query: &str) -> String {
        let hits = self.lib.search(query, 12);
        if hits.is_empty() {
            return "no matches. Try a shorter query, a different part number variant, or a generic \
                    symbol (Device:*, Connector_Generic:*)."
                .to_string();
        }
        hits.iter()
            .map(|h| {
                format!(
                    "{}  ({} pins)  {}",
                    h.lib_id,
                    h.pins,
                    h.description.chars().take(110).collect::<String>()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn t_symbol_info(&self, lib_id: &str, unit: Option<u32>) -> String {
        match self.lib.info_text(lib_id, unit) {
            Ok(text) => text,
            Err(_) => {
                let near = self
                    .lib
                    .search(lib_id, 5)
                    .into_iter()
                    .map(|h| h.lib_id)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("unknown symbol {lib_id:?}. Did you mean: {near}")
            }
        }
    }

    fn t_build(&mut self, design: &Value) -> String {
        if self.builds >= self.budget.max_builds {
            return "Build limit reached. Call finish now.".to_string();
        }
        self.builds += 1;
        let n = self.builds;
        let sch_path = self.work_dir.join(format!("v{n}.kicad_sch"));
        let _ = std::fs::write(
            self.work_dir.join(format!("v{n}.json")),
            serde_json::to_string_pretty(design).unwrap_or_default(),
        );
        let built = match self.base.as_ref() {
            Some(base) => sch::build_patch(&self.lib, base, design, &sch_path),
            None => sch::build(&self.lib, design, &sch_path),
        };
        let mut report = match built {
            Ok(report) => report,
            Err(error) => return format!("error: build failed: {error:#}"),
        };
        let _ = std::fs::write(
            self.work_dir.join(format!("v{n}_raw.json")),
            serde_json::to_string_pretty(&report.raw).unwrap_or_default(),
        );
        let pre: Vec<String> = report
            .issues
            .iter()
            .filter(|i| self.baseline_issues.contains(*i))
            .cloned()
            .collect();
        report.issues.retain(|i| !self.baseline_issues.contains(i));
        let parts_now = part_ids(&report.raw);
        if let Some(base) = self.base.as_ref() {
            let was: BTreeSet<String> = base.parts.iter().map(|p| p.id.clone()).collect();
            report
                .issues
                .extend(stranded(&was, &report.netlist, &parts_now));
        }
        let ok = report.issues.is_empty();

        self.last_build = Some(sch_path.clone());
        self.last_design = Some(design.clone());
        self.last_raw = report.raw.clone();
        if ok {
            self.last_ok_build = Some(sch_path.clone());
            if let Some(hook) = self.on_clean_build {
                hook(&sch_path);
            }
        }

        let mut out = vec![format!(
            "BUILD {n}: {}",
            if ok {
                "OK (no hard issues)".to_string()
            } else {
                format!("{} ISSUES", report.issues.len())
            }
        )];
        out.extend(report.notes.iter().cloned());
        if !pre.is_empty() {
            out.push(format!(
                "({} pre-existing issues of the original design are ignored)",
                pre.len()
            ));
        }
        if !report.issues.is_empty() {
            out.push("ISSUES (must fix):".to_string());
            out.extend(report.issues.iter().take(80).map(|i| format!("  - {i}")));
        }
        if !report.warnings.is_empty() {
            out.push("WARNINGS (review):".to_string());
            out.extend(report.warnings.iter().take(40).map(|w| format!("  - {w}")));
        }
        let netlist = report.netlist_text(400);
        if !netlist.is_empty() {
            out.push(format!("NETLIST:\n{netlist}"));
        }
        if self.base.is_some() {
            let diff = net_diff(&self.base_nets, &report.netlist, &parts_now);
            out.push(format!(
                "NET CHANGES vs original:\n{}",
                if diff.is_empty() { "  none" } else { &diff }
            ));
        }
        out.push(format!(
            "({} builds left){}",
            self.budget.max_builds - n,
            if ok {
                "; next: erc, review"
            } else {
                " - fix the ISSUES and build again"
            }
        ));
        self.last_report = Some(report);
        out.join("\n")
    }

    fn t_erc(&mut self) -> String {
        let Some(build) = self.last_build.clone() else {
            return "Nothing built yet.".to_string();
        };
        let lines = sch::run_erc(&self.kicad_cli, &build)
            .unwrap_or_else(|e| vec![format!("[error] ERC could not run: {e:#}")]);
        self.erc_errors = lines.iter().filter(|l| l.starts_with("[error]")).count();
        self.erc_lines = lines.clone();
        self.erc_of = Some(build);
        if lines.is_empty() {
            "KICAD ERC: clean".to_string()
        } else {
            format!(
                "KICAD ERC:\n{}",
                lines
                    .iter()
                    .take(60)
                    .map(|l| format!("  - {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }
    }

    fn t_render(&mut self) -> (String, Option<Binary>) {
        let Some(build) = self.last_build.clone() else {
            return ("Nothing built yet.".to_string(), None);
        };
        if self.rendered_of.as_ref() != Some(&build) {
            let stem = build.with_extension("");
            let clean = stem.with_extension("png");
            let grid = stem.with_file_name(format!(
                "{}_grid.png",
                stem.file_name().unwrap_or_default().to_string_lossy()
            ));
            match render::sheet(&self.kicad_cli, &build, &clean, &grid) {
                Ok(sheet) => {
                    self.sheet = Some(sheet);
                    self.rendered_of = Some(build);
                }
                Err(error) => return (format!("error: render failed: {error:#}"), None),
            }
        }
        let png = self.sheet.as_ref().map(|s| binary(&s.grid));
        (
            "Rendered sheet attached (blue coordinate grid in grid units).".to_string(),
            png,
        )
    }

    async fn t_review(&mut self) -> (String, Option<Binary>) {
        let (message, image) = self.t_render();
        if image.is_none() {
            return (message, None);
        }
        let Some(sheet) = self.sheet.as_ref() else {
            return (message, image);
        };
        let clean = sheet.clean.clone();
        let engine_clean = self
            .last_report
            .as_ref()
            .is_some_and(|r| r.issues.is_empty() && r.warnings.is_empty());
        let parts = part_summary(&self.last_raw);
        match critic::review(self.client, &clean, &parts, engine_clean).await {
            Ok(Some(review)) => {
                event(review.event());
                let text = review.text();
                self.reviewed_of = self.last_build.clone();
                if self.last_build == self.last_ok_build
                    && let (Some(build), Some(design)) =
                        (self.last_build.clone(), self.last_design.clone())
                    && self
                        .best
                        .as_ref()
                        .is_none_or(|(best, _, _)| review.mean > *best)
                {
                    self.best = Some((review.mean, build, design));
                }
                self.review = Some(review);
                (
                    format!("VISUAL REVIEW by an independent reviewer:\n{text}"),
                    image,
                )
            }
            _ => ("review unavailable".to_string(), image),
        }
    }

    fn t_finish(&mut self, args: &Value, finished: &mut Option<String>) -> String {
        let state = FinishState {
            built: self.last_build.is_some(),
            clean: self.last_ok_build == self.last_build,
            erc_ran: self.erc_of == self.last_build,
            erc_errors: self.erc_errors,
            reviewed: self.reviewed_of == self.last_build,
            score: self.review.as_ref().map(|r| r.mean),
            forced: args
                .get("force")
                .and_then(Value::as_bool)
                .unwrap_or_default()
                || self.builds >= self.budget.max_builds
                || self.started.elapsed() > self.budget.hard_deadline(),
            time_pressed: self.started.elapsed() > self.budget.loop_deadline(),
        };
        let missing = state.missing();
        if missing.is_empty() {
            *finished = Some(
                args.get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            );
            "ok".to_string()
        } else {
            format!(
                "Not finished: {}. (finish with force=true if you judge the remaining points acceptable)",
                missing.join("; ")
            )
        }
    }

    // ---- delivery ------------------------------------------------------

    /// Copy the best clean build to the project schematic and report on it.
    fn deliver(&mut self, summary: String) -> Result<Outcome> {
        let chosen = self
            .best
            .as_ref()
            .map(|(_, path, _)| path.clone())
            .or_else(|| self.last_ok_build.clone())
            .or_else(|| self.last_build.clone());
        let design = self
            .best
            .as_ref()
            .map(|(_, _, design)| design.clone())
            .or_else(|| self.last_design.clone());
        if let Some(path) = chosen.as_ref() {
            std::fs::copy(path, &self.out_sch)
                .with_context(|| format!("delivering {}", self.out_sch.display()))?;
        }

        Ok(Outcome {
            sheet: chosen.as_ref().map(|_| self.out_sch.clone()),
            source: chosen,
            design,
            review: self.review.take(),
            erc: self.erc_lines.clone(),
            issues: self
                .last_report
                .as_ref()
                .map(|r| r.issues.clone())
                .unwrap_or_default(),
            builds: self.builds,
            summary,
            usage: self.usage,
            seconds: self.started.elapsed().as_secs_f64(),
        })
    }

    /// The library index, so the composition pass can rebuild without reloading it.
    pub fn library(&self) -> &sch::Library {
        &self.lib
    }

    /// Where intermediate versions are written.
    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// The delivered sheet's path inside the project.
    pub fn out_sch(&self) -> &Path {
        &self.out_sch
    }

    /// The project directory this session writes into.
    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    /// The best clean build's mean review score, when one was graded.
    pub fn best_score(&self) -> Option<f64> {
        self.best.as_ref().map(|(score, _, _)| *score)
    }

    fn record(&mut self, tool: &str, out: &str) {
        use std::io::Write;
        let line = json!({"tool": tool, "out": out.chars().take(4000).collect::<String>()});
        let _ = writeln!(self.log, "{line}");
    }
}

/// What `finish` is gated on: a clean last build, an ERC without errors and a
/// review of at least 8.
///
/// Time relaxes the gate in two steps, because the two halves are not worth the
/// same. Past the loop deadline only the review SCORE is waived — a point of
/// composition is cosmetic. Correctness is waived only by [`Self::forced`]: the
/// model's own `force`, the build ceiling, or the hard deadline.
#[derive(Clone, Copy, Debug, Default)]
struct FinishState {
    built: bool,
    clean: bool,
    erc_ran: bool,
    erc_errors: usize,
    reviewed: bool,
    score: Option<f64>,
    forced: bool,
    time_pressed: bool,
}

impl FinishState {
    /// What still stands in the way, empty when the sheet may be delivered.
    fn missing(self) -> Vec<String> {
        if !self.built {
            return vec!["nothing has been built yet".to_string()];
        }
        if self.forced {
            return Vec::new();
        }
        let mut missing = Vec::new();
        if !self.clean {
            missing.push("the last build still has ISSUES".to_string());
        }
        if !self.erc_ran {
            missing.push("run erc on the last build".to_string());
        } else if self.erc_errors > 0 {
            missing.push(format!("ERC reported {} error(s)", self.erc_errors));
        }
        if self.time_pressed {
            return missing;
        }
        if !self.reviewed {
            missing.push("run review on the last build".to_string());
        } else if let Some(score) = self.score
            && score < 8.0
        {
            missing.push(format!(
                "the review scored {score:.1}/10 - address its defects and build again"
            ));
        }
        missing
    }
}

/// Human-readable differences between two netlists; `N$` autonames and pins of
/// removed parts are ignored.
/// The references of every part in a laid-out raw design.
fn part_ids(raw: &Value) -> BTreeSet<String> {
    raw.get("parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Edit-mode issues about parts the patch added and did not attach.
///
/// A patch adds circuitry *to* a sheet, so a new part that shares no net with
/// anything that was already there is floating next to the drawing rather than
/// wired into it, and a pin alone on a net is a wire that leads nowhere. Both
/// pass every check a fresh design is judged by — the added block is internally
/// consistent — so edit mode has to say it here.
fn stranded(
    was: &BTreeSet<String>,
    netlist: &BTreeMap<String, BTreeSet<String>>,
    parts_now: &BTreeSet<String>,
) -> Vec<String> {
    let owner = |pin: &str| pin.split('.').next().unwrap_or("").to_string();
    let added: BTreeSet<&String> = parts_now.iter().filter(|id| !was.contains(*id)).collect();
    let mut issues = Vec::new();
    for (net, pins) in netlist {
        if pins.len() == 1
            && let Some(pin) = pins.iter().next()
            && added.contains(&owner(pin))
        {
            issues.push(format!(
                "{pin} is alone on net {net}: the new part's pin connects to nothing. Wire it, or \
                 join the existing net by putting a label with the SAME name on one of its wires."
            ));
        }
    }
    for part in &added {
        let reaches = netlist.values().any(|pins| {
            pins.iter().any(|p| owner(p) == **part) && pins.iter().any(|p| was.contains(&owner(p)))
        });
        if !reaches {
            issues.push(format!(
                "the added part {part} shares no net with any part of the original design - it is \
                 drawn beside the sheet, not connected into it"
            ));
        }
    }
    issues
}

fn net_diff(
    old: &BTreeMap<String, BTreeSet<String>>,
    new: &BTreeMap<String, BTreeSet<String>>,
    parts_now: &BTreeSet<String>,
) -> String {
    let mut lines = Vec::new();
    let names: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    for name in names {
        if name.starts_with("N$") {
            continue;
        }
        let empty = BTreeSet::new();
        let before: BTreeSet<String> = old
            .get(name)
            .unwrap_or(&empty)
            .iter()
            .filter(|m| parts_now.contains(m.split('.').next().unwrap_or("")))
            .cloned()
            .collect();
        let after = new.get(name).unwrap_or(&empty).clone();
        if before == after {
            continue;
        }
        if before.is_empty() {
            lines.push(format!("  + new net {name}: {}", join(&after)));
        } else if after.is_empty() {
            lines.push(format!(
                "  - net {name} disappeared (had {})",
                join(&before)
            ));
        } else {
            let lost: BTreeSet<_> = before.difference(&after).cloned().collect();
            let gained: BTreeSet<_> = after.difference(&before).cloned().collect();
            let mut change = format!("  ~ {name}: ");
            if !lost.is_empty() {
                change.push_str(&format!("lost {} ", join(&lost)));
            }
            if !gained.is_empty() {
                change.push_str(&format!("gained {}", join(&gained)));
            }
            lines.push(change);
        }
    }
    lines.join("\n")
}

fn join(items: &BTreeSet<String>) -> String {
    items.iter().cloned().collect::<Vec<_>>().join(" ")
}

/// `R1=1k, U1=NE555` — what the critic is told is on the sheet.
pub fn part_summary(raw: &Value) -> String {
    raw.get("parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .map(|p| {
                    let id = p.get("id").and_then(Value::as_str).unwrap_or("?");
                    let value = p
                        .get("value")
                        .and_then(Value::as_str)
                        .filter(|v| !v.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            p.get("lib")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .rsplit(':')
                                .next()
                                .unwrap_or("")
                                .to_string()
                        });
                    format!("{id}={value}")
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn binary(png: &[u8]) -> Binary {
    use base64::Engine;
    Binary::from_base64(
        "image/png",
        base64::engine::general_purpose::STANDARD.encode(png),
        None,
    )
}

/// A tool call's arguments for the transcript. Always a complete JSON object —
/// a design is thousands of lines, so a big payload is reported by size rather
/// than clipped into something unparsable.
fn call_args(call: &ToolCall) -> String {
    let text = call.fn_arguments.to_string();
    if text.len() <= 200 {
        text
    } else {
        json!({"bytes": text.len()}).to_string()
    }
}

/// One transcript line: newlines collapsed, clipped to `limit` characters.
fn one_line(text: &str, limit: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        flat
    } else {
        flat.chars().take(limit).collect::<String>() + "..."
    }
}

/// A bare line on the agent's stderr transcript.
pub fn event(line: impl AsRef<str>) {
    tracing::info!(target: EVENTS_TARGET, "{}", line.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nets(pairs: &[(&str, &[&str])]) -> BTreeMap<String, BTreeSet<String>> {
        pairs
            .iter()
            .map(|(net, pins)| {
                (
                    (*net).to_string(),
                    pins.iter().map(|p| (*p).to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn the_net_diff_names_what_a_patch_changed() {
        let before = nets(&[("VCC", &["U1.1", "C1.1"]), ("N$1", &["R9.1"])]);
        let after = nets(&[("VCC", &["U1.1"]), ("VOUT", &["U2.3"]), ("N$1", &[])]);
        let parts = ["U1", "C1", "U2", "R9"]
            .iter()
            .map(|p| (*p).to_string())
            .collect();

        let diff = net_diff(&before, &after, &parts);

        assert!(diff.contains("~ VCC: lost C1.1"), "{diff}");
        assert!(diff.contains("+ new net VOUT: U2.3"), "{diff}");
        assert!(!diff.contains("N$1"), "autonamed nets are noise");
    }

    #[test]
    fn a_removed_part_s_pins_do_not_read_as_a_change() {
        let before = nets(&[("VCC", &["U1.1", "C1.1"])]);
        let after = nets(&[("VCC", &["U1.1"])]);
        let parts = ["U1"].iter().map(|p| (*p).to_string()).collect();

        assert_eq!(net_diff(&before, &after, &parts), "");
    }

    #[test]
    fn the_part_summary_falls_back_to_the_symbol_name() {
        let raw =
            json!({"parts": [{"id": "U1", "lib": "Timer:NE555P"}, {"id": "R1", "value": "1k"}]});
        assert_eq!(part_summary(&raw), "U1=NE555P, R1=1k");
    }

    #[test]
    fn a_transcript_line_is_flat_and_clipped() {
        assert_eq!(one_line("a\n  b\tc", 100), "a b c");
        assert_eq!(one_line("abcdef", 3), "abc...");
    }

    /// The transcript is parsed by the quality harness, whose `tool ->` pattern
    /// only matches a complete JSON object — so a big design is summarised, never
    /// cut in half.
    #[test]
    fn tool_call_arguments_stay_a_whole_json_object() {
        let call = |args: Value| ToolCall {
            call_id: "1".into(),
            fn_name: "build".into(),
            fn_arguments: args,
            thought_signatures: None,
        };
        assert_eq!(call_args(&call(json!({"query": "R"}))), r#"{"query":"R"}"#);

        let big = call_args(&call(json!({"design": "x".repeat(500)})));
        assert!(big.starts_with('{') && big.ends_with('}'), "{big}");
        assert!(big.contains("bytes"));
    }

    #[test]
    fn finish_is_refused_until_the_sheet_is_clean_erc_free_and_reviewed_well() {
        let ready = FinishState {
            built: true,
            clean: true,
            erc_ran: true,
            erc_errors: 0,
            reviewed: true,
            score: Some(8.5),
            forced: false,
            time_pressed: false,
        };
        assert!(ready.missing().is_empty());

        assert_eq!(
            FinishState::default().missing(),
            ["nothing has been built yet"]
        );
        assert_eq!(
            FinishState {
                clean: false,
                ..ready
            }
            .missing(),
            ["the last build still has ISSUES"]
        );
        assert_eq!(
            FinishState {
                erc_ran: false,
                ..ready
            }
            .missing(),
            ["run erc on the last build"]
        );
        assert_eq!(
            FinishState {
                erc_errors: 2,
                ..ready
            }
            .missing(),
            ["ERC reported 2 error(s)"]
        );
        assert_eq!(
            FinishState {
                reviewed: false,
                ..ready
            }
            .missing(),
            ["run review on the last build"]
        );
        assert_eq!(
            FinishState {
                score: Some(6.0),
                ..ready
            }
            .missing(),
            ["the review scored 6.0/10 - address its defects and build again"]
        );
    }

    /// Force clears every gate but the one that has no sheet to deliver.
    #[test]
    fn force_delivers_a_built_sheet_and_nothing_else() {
        let forced = FinishState {
            built: true,
            forced: true,
            ..FinishState::default()
        };
        assert!(forced.missing().is_empty());
        assert!(
            !FinishState {
                built: false,
                ..forced
            }
            .missing()
            .is_empty()
        );
    }

    #[test]
    fn the_loop_deadline_is_a_share_of_the_total_budget() {
        let budget = Budget {
            total: Duration::from_secs(180),
            loop_share: 0.6,
            ..Budget::default()
        };
        assert_eq!(budget.loop_deadline(), Duration::from_secs(108));
        assert_eq!(budget.hard_deadline(), Duration::from_secs(144));
    }
}
