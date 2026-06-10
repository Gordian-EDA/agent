# Schematic Readability — Slice 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement Phase 1 (render tool + image plumbing), the `.autopcb` draft workspace, and Phase 2 (engine layout primitives + layout-rev reconciliation) of `docs/superpowers/specs/2026-06-10-schematic-readability-design.md`.

**Architecture:** Part A (Tasks 1–6) extends the `agent` crate: tool results gain image blocks, a `render_schematic` tool rasterizes the schematic via `kicad-cli sch export svg` + the `resvg` crate, and a persistent `.autopcb/` draft enables `create_design`/`edit_design` anchored editing. Part B (Tasks 7–15) extends `sch-engine`: power symbols + wires replace power-net label spam, signal labels get stubs and orientation, passives orient to their rail role, placement becomes bbox-aware with decoupling banks, blocks get frames/titles, a deterministic layout lint reports collisions, and `ap_layout_rev` hashing lets hint changes trigger re-placement without clobbering user drags. The two parts are independent and can be executed in either order (Part B has higher visual impact).

**Tech Stack:** Rust 2024 workspace, KiCAD 10 `kicad-cli`, `resvg` (pure-Rust SVG→PNG), `base64`, AWS Bedrock Converse API.

**Conventions for every task:** run tests with `cargo test -p <crate>` from the repo root. Many tests SKIP gracefully when no KiCAD install is detected (`KicadEnv::detect()` / `ToolCtx::detect_for_test()` returning `None`) — on this machine KiCAD 10.0.3 IS installed, so they run for real. Never run two cargo commands concurrently (one shared target dir).

---

## Part A — Agent surface (Phase 1 + draft workspace)

### Task 1: Image content in tool results (`llm.rs`)

Bedrock Converse tool results may carry image blocks: `{"image": {"format": "png", "source": {"bytes": "<base64>"}}}`. Add an `ImageData` type and extend `ContentBlock::ToolResult` with an `images` field.

**Files:**
- Modify: `crates/agent/src/llm.rs` (ContentBlock enum ~line 48, `content_block_to_wire` ~line 258, tests)
- Modify: `crates/agent/src/agent.rs` (ToolResult construction at ~line 415, ~line 638, test at ~line 889 — add `images: Vec::new()`)

- [ ] **Step 1: Write the failing test** (in `llm.rs` `mod tests`)

```rust
    #[test]
    fn tool_result_with_image_maps_to_converse_image_block() {
        let c = client();
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu_9".to_string(),
                content: "{\"ok\":true}".to_string(),
                images: vec![ImageData {
                    format: "png".to_string(),
                    base64: "aGVsbG8=".to_string(),
                }],
            }],
        }];
        let body = c.build_request("sys", &messages, &[]);

        let content = &body["messages"][0]["content"][0]["toolResult"]["content"];
        assert_eq!(content[0]["text"], "{\"ok\":true}");
        assert_eq!(content[1]["image"]["format"], "png");
        assert_eq!(content[1]["image"]["source"]["bytes"], "aGVsbG8=");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p agent tool_result_with_image -- --nocapture`
Expected: COMPILE ERROR — `ImageData` not defined, `ToolResult` has no field `images`.

- [ ] **Step 3: Implement**

In `llm.rs`, above `ContentBlock`:

```rust
/// An image attached to a tool result, already base64-encoded for the wire.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageData {
    /// Converse wire format identifier: "png", "jpeg", "gif", or "webp".
    pub format: String,
    /// Base64-encoded image bytes.
    pub base64: String,
}
```

Extend the variant:

```rust
    /// The result of running a tool, fed back to the model.
    ToolResult {
        tool_use_id: String,
        content: String,
        /// Images attached to the result (rendered schematics). Empty for
        /// text-only results.
        images: Vec<ImageData>,
    },
```

Rewrite the wire mapping arm in `content_block_to_wire`:

```rust
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            images,
        } => {
            let mut blocks = vec![json!({ "text": content })];
            for img in images {
                blocks.push(json!({
                    "image": {
                        "format": img.format,
                        "source": { "bytes": img.base64 },
                    }
                }));
            }
            json!({
                "toolResult": {
                    "toolUseId": tool_use_id,
                    "content": blocks,
                }
            })
        }
```

In `agent.rs`, add `images: Vec::new(),` to every `ContentBlock::ToolResult { … }` construction (the run-loop at ~415, `repair_history`'s synthesized results at ~638, and the test fixture at ~889). The match at ~233 uses `{ content, .. }` and needs no change. Update the existing `assistant_tool_use_and_user_tool_result_map_to_wire` test in `llm.rs` the same way.

- [ ] **Step 4: Run the full crate test suite**

Run: `cargo test -p agent`
Expected: PASS (including the new test).

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/llm.rs crates/agent/src/agent.rs
git commit -m "feat(agent): image content blocks in tool results (Converse wire)"
```

---

### Task 2: `KicadCli::export_svg` (`kicad-bridge`)

**Files:**
- Modify: `crates/kicad-bridge/src/cli.rs`
- Test: `crates/kicad-bridge/tests/export_svg.rs` (create)

- [ ] **Step 1: Write the failing test**

```rust
//! `kicad-cli sch export svg` wrapper — exercised against the checked-in
//! bluepill fixture. SKIPs when no KiCAD environment is detected.

use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use std::path::Path;

#[test]
fn exports_svg_for_bluepill_fixture() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let sch = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/validation/bluepill.kicad_sch");
    assert!(sch.is_file(), "fixture missing: {}", sch.display());

    let out = tempfile::tempdir().unwrap();
    let svg_path = KicadCli::new(&env)
        .export_svg(&sch, out.path())
        .expect("svg export");

    let svg = std::fs::read_to_string(&svg_path).unwrap();
    assert!(svg.contains("<svg"), "not an SVG: {}", svg_path.display());
    assert!(svg.len() > 1000, "suspiciously small SVG ({} bytes)", svg.len());
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p kicad-bridge --test export_svg`
Expected: COMPILE ERROR — no method `export_svg`.

- [ ] **Step 3: Implement** (in `cli.rs`, after `netlist`)

```rust
    /// Run `kicad-cli sch export svg` on `schematic`, writing into `out_dir`.
    ///
    /// KiCAD names the output `<schematic stem>.svg` inside `out_dir`; the
    /// resolved path is returned. Returns `Err` on execution failure (binary
    /// missing, schematic failed to load) or if the expected file was not
    /// produced.
    pub fn export_svg(&self, schematic: &Path, out_dir: &Path) -> io::Result<std::path::PathBuf> {
        std::fs::create_dir_all(out_dir)?;
        let output = Command::new(&self.cli_path)
            .args(["sch", "export", "svg"])
            .arg("--output")
            .arg(out_dir)
            .arg(schematic)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("kicad-cli sch export svg failed: {}", stderr.trim()),
            ));
        }
        let stem = schematic
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "schematic has no stem"))?;
        let svg = out_dir.join(format!("{stem}.svg"));
        if !svg.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("expected SVG not produced at {}", svg.display()),
            ));
        }
        Ok(svg)
    }
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p kicad-bridge --test export_svg`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/kicad-bridge/src/cli.rs crates/kicad-bridge/tests/export_svg.rs
git commit -m "feat(kicad-bridge): kicad-cli sch export svg wrapper"
```

---

### Task 3: SVG → PNG rasterizer (`agent/src/render.rs`)

kicad-cli 10 cannot export PNG for schematics, so rasterize the SVG with the pure-Rust `resvg`. KiCAD strokes text as polylines in SVG plots, so no font database setup is needed. (Known risk from the spec: if rendered text ever comes out blank, the fallback is shelling out to `rsvg-convert`, which is installed.)

**Files:**
- Modify: `Cargo.toml` (workspace deps), `crates/agent/Cargo.toml`, `crates/agent/src/lib.rs` (add `pub mod render;`)
- Create: `crates/agent/src/render.rs`

- [ ] **Step 1: Add dependencies**

Workspace `Cargo.toml` `[workspace.dependencies]`:

```toml
resvg = { version = "0.45", default-features = false }
base64 = "0.22"
```

`crates/agent/Cargo.toml` `[dependencies]`: add `resvg = { workspace = true }` and `base64 = { workspace = true }`.

- [ ] **Step 2: Write the failing test** (inline `mod tests` in the new `render.rs` — write the file with the test and a `todo!()` body first if you want a strict red step, or proceed directly; the meaningful red gate here is the test, not the module skeleton)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A 100x50 red rectangle. Rasterized at max_px=200 the long edge must be
    /// 200 px and the pixel data non-trivial.
    const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50">
        <rect x="10" y="10" width="80" height="30" fill="red"/></svg>"##;

    #[test]
    fn rasterizes_svg_to_scaled_png() {
        let png = svg_to_png(SVG, 200).expect("render");
        // PNG magic bytes.
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert!(png.len() > 100, "PNG too small: {} bytes", png.len());
    }

    #[test]
    fn rejects_malformed_svg() {
        assert!(svg_to_png("not svg at all", 200).is_err());
    }
}
```

- [ ] **Step 3: Implement**

```rust
//! Rasterize a `kicad-cli`-exported SVG into a PNG the LLM can see.
//!
//! Pure-Rust via `resvg` — no system rasterizer needed. KiCAD plots text as
//! stroked polylines, so an empty fontdb renders correctly. The long edge is
//! capped at `max_px` (callers pass ~1600: under Bedrock's request limits and
//! near Claude's 1568 px vision sweet spot).

use anyhow::{Context, Result};

/// Render `svg` to PNG bytes, scaling so the long edge is `max_px` pixels.
pub fn svg_to_png(svg: &str, max_px: u32) -> Result<Vec<u8>> {
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_str(svg, &opt).context("parsing SVG")?;
    let size = tree.size();
    let scale = max_px as f32 / size.width().max(size.height());
    let w = ((size.width() * scale).ceil() as u32).max(1);
    let h = ((size.height() * scale).ceil() as u32).max(1);

    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(w, h).context("allocating pixmap")?;
    // KiCAD SVGs assume a paper-white background; resvg default is transparent.
    pixmap.fill(resvg::tiny_skia::Color::WHITE);
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().context("encoding PNG")
}
```

Add `pub mod render;` to `crates/agent/src/lib.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p agent render`
Expected: both tests PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/agent/Cargo.toml crates/agent/src/lib.rs crates/agent/src/render.rs Cargo.lock
git commit -m "feat(agent): resvg-based SVG->PNG rasterizer"
```

---

### Task 4: `.autopcb/` workspace module

**Files:**
- Create: `crates/agent/src/workspace.rs`
- Modify: `crates/agent/src/lib.rs` (add `pub mod workspace;`), `crates/agent/src/tools.rs` (`ToolCtx` gains a `workspace` field + accessor; construct in `new` / `detect_for_test`)

- [ ] **Step 1: Write the failing tests** (inline in `workspace.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_self_ignoring_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::for_project(dir.path()).unwrap();
        assert!(dir.path().join(".autopcb/renders").is_dir());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".autopcb/.gitignore")).unwrap(),
            "*\n"
        );
        assert!(ws.read_draft().is_none());
    }

    #[test]
    fn draft_roundtrip_and_staleness_hash() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::for_project(dir.path()).unwrap();

        ws.write_draft("version: 1\n", Some("sch contents v1")).unwrap();
        assert_eq!(ws.read_draft().as_deref(), Some("version: 1\n"));
        // Same sch text -> not stale; different -> stale.
        assert!(!ws.draft_is_stale(Some("sch contents v1")));
        assert!(ws.draft_is_stale(Some("sch contents v2")));
        // Draft seeded with no schematic on disk: stale only once a sch appears.
        ws.write_draft("version: 1\n", None).unwrap();
        assert!(!ws.draft_is_stale(None));
        assert!(ws.draft_is_stale(Some("anything")));
    }

    #[test]
    fn render_paths_increment() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::for_project(dir.path()).unwrap();
        let a = ws.next_render_path().unwrap();
        std::fs::write(&a, b"x").unwrap();
        let b = ws.next_render_path().unwrap();
        assert!(a.ends_with("render-001.png"), "{}", a.display());
        assert!(b.ends_with("render-002.png"), "{}", b.display());
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p agent workspace`
Expected: COMPILE ERROR — module/type missing.

- [ ] **Step 3: Implement**

```rust
//! Project-local persistent state: `<project>/.autopcb/`.
//!
//! Holds the working draft (`draft.circuit.yaml` — the document `edit_design`
//! patches and `apply_design` applies), `draft.meta.json` (the content hash of
//! the `.kicad_sch` the draft was seeded from, for staleness detection), and
//! `renders/` (PNGs from `render_schematic`). The directory ships its own
//! `.gitignore` containing `*` so it never pollutes the user's repo. The
//! `session/` subdirectory is reserved for a future resume feature.

use std::io;
use std::path::{Path, PathBuf};

pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Open (creating if needed) the `.autopcb/` directory under `project_dir`.
    pub fn for_project(project_dir: &Path) -> io::Result<Self> {
        let root = project_dir.join(".autopcb");
        std::fs::create_dir_all(root.join("renders"))?;
        let gi = root.join(".gitignore");
        if !gi.exists() {
            std::fs::write(&gi, "*\n")?;
        }
        Ok(Self { root })
    }

    pub fn draft_path(&self) -> PathBuf {
        self.root.join("draft.circuit.yaml")
    }

    /// The current draft text, if a draft exists.
    pub fn read_draft(&self) -> Option<String> {
        std::fs::read_to_string(self.draft_path()).ok()
    }

    /// Write the draft and record which schematic text it was seeded from
    /// (`None` when no schematic exists yet).
    pub fn write_draft(&self, yaml: &str, sch_text: Option<&str>) -> io::Result<()> {
        std::fs::write(self.draft_path(), yaml)?;
        let meta = serde_json::json!({
            "seeded_from_sch_hash": sch_text.map(fnv1a64),
        });
        std::fs::write(self.root.join("draft.meta.json"), meta.to_string())
    }

    /// True when the on-disk schematic no longer matches what the draft was
    /// seeded from (the user edited it in KiCAD out-of-band).
    pub fn draft_is_stale(&self, current_sch_text: Option<&str>) -> bool {
        let Ok(meta) = std::fs::read_to_string(self.root.join("draft.meta.json")) else {
            return false; // no meta -> nothing to compare against
        };
        let recorded: Option<u64> = serde_json::from_str::<serde_json::Value>(&meta)
            .ok()
            .and_then(|v| v.get("seeded_from_sch_hash").cloned())
            .and_then(|v| v.as_u64());
        recorded != current_sch_text.map(fnv1a64)
    }

    /// The next free `renders/render-NNN.png` path.
    pub fn next_render_path(&self) -> io::Result<PathBuf> {
        let dir = self.root.join("renders");
        for n in 1..10_000u32 {
            let p = dir.join(format!("render-{n:03}.png"));
            if !p.exists() {
                return Ok(p);
            }
        }
        Err(io::Error::other("renders/ directory is full"))
    }
}

/// FNV-1a 64-bit — tiny, dependency-free content hash for staleness checks.
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
```

In `tools.rs`: add `workspace: crate::workspace::Workspace` to `ToolCtx`, construct it with `Workspace::for_project(&project_dir)?` in `ToolCtx::new` (and via `Workspace::for_project(&project_dir).ok()?` in `detect_for_test`), and expose:

```rust
    /// The project's `.autopcb/` persistent state.
    pub fn workspace(&self) -> &crate::workspace::Workspace {
        &self.workspace
    }
```

Add `pub mod workspace;` to `lib.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p agent`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/workspace.rs crates/agent/src/lib.rs crates/agent/src/tools.rs
git commit -m "feat(agent): .autopcb project workspace (draft, meta, renders)"
```

---

### Task 5: `render_schematic` tool + image attachment in the loop

Tools keep returning `Value`. Convention: a tool result containing the key `"_image_path"` (a PNG on disk) gets that file loaded, base64-encoded, attached as an `ImageData`, and the key stripped before the JSON goes back to the model.

**Files:**
- Modify: `crates/agent/src/tools.rs` (new tool def + dispatch + impl), `crates/agent/src/agent.rs` (`run_tool_call` returns images; `tool_summary` arm)
- Test: `crates/agent/tests/tools.rs` (extend)

- [ ] **Step 1: Write the failing test** (append to `crates/agent/tests/tools.rs`)

```rust
#[test]
fn render_schematic_returns_png_and_image_path() {
    let Some(ctx) = agent::tools::ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let tools = agent::tools::Tools::new();

    // No schematic yet -> structured error, no crash.
    let out = tools
        .run("render_schematic", serde_json::json!({}), &ctx)
        .unwrap();
    assert!(out.get("error").is_some());

    // Write a minimal schematic via apply_design, then render it.
    let yaml = "
version: 1
name: t
rails: [GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
";
    let applied = tools
        .run(
            "apply_design",
            serde_json::json!({ "yaml": yaml, "commit": true }),
            &ctx,
        )
        .unwrap();
    assert_eq!(applied["written"], serde_json::json!(true));

    let out = tools
        .run("render_schematic", serde_json::json!({}), &ctx)
        .unwrap();
    let png_path = out["_image_path"].as_str().expect("image path");
    let bytes = std::fs::read(png_path).unwrap();
    assert_eq!(&bytes[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    assert!(png_path.contains(".autopcb/renders/render-001.png"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p agent --test tools render_schematic`
Expected: FAIL — `unknown tool: render_schematic`.

- [ ] **Step 3: Implement the tool** (in `tools.rs`)

Add to `Tools::defs()`:

```rust
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
```

Dispatch arm: `"render_schematic" => render_schematic(ctx),`. Implementation:

```rust
// ── 9. render_schematic ────────────────────────────────────────────────────

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
    let png = crate::render::svg_to_png(&svg, RENDER_MAX_PX)?;
    let path = ctx.workspace.next_render_path()?;
    std::fs::write(&path, &png)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(json!({
        "ok": true,
        "png_path": path.display().to_string(),
        "note": "image attached; also saved to png_path for the user to open",
        IMAGE_PATH_KEY: path.display().to_string(),
    }))
}
```

- [ ] **Step 4: Attach images in the agent loop** (in `agent.rs`)

Change `run_tool_call` to return `(String, Vec<ImageData>)`:

```rust
    async fn run_tool_call(
        &self,
        call: &crate::llm::ToolCall,
        approvals: &mut dyn Approvals,
        applied: &mut bool,
        events: Events<'_>,
    ) -> (String, Vec<ImageData>) {
        let result = if call.name == "apply_design" && wants_commit(&call.input) {
            self.gated_apply(&call.input, approvals, applied, events)
                .await
        } else {
            self.run_tool_blocking(&call.name, call.input.clone()).await
        };

        match result {
            Ok(mut value) => {
                let images = take_images(&mut value);
                (value.to_string(), images)
            }
            Err(e) => (json!({ "error": e.to_string() }).to_string(), Vec::new()),
        }
    }
```

Add the helper (file scope, near `tool_summary`):

```rust
/// Pull a `_image_path` out of a tool result: load + base64 the PNG, strip the
/// key so the model's text view stays clean. An unreadable file degrades to
/// "no image" rather than failing the tool call.
fn take_images(value: &mut Value) -> Vec<ImageData> {
    use base64::Engine as _;
    let Some(path) = value
        .get(crate::tools::IMAGE_PATH_KEY)
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Vec::new();
    };
    if let Some(obj) = value.as_object_mut() {
        obj.remove(crate::tools::IMAGE_PATH_KEY);
    }
    match std::fs::read(&path) {
        Ok(bytes) => vec![ImageData {
            format: "png".to_string(),
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        }],
        Err(_) => Vec::new(),
    }
}
```

Update the call site in `run_turn` (~line 405) to destructure and pass images through:

```rust
                let (content, images) = self
                    .run_tool_call(call, approvals, &mut applied, events)
                    .await;
                // …
                result_blocks.push(ContentBlock::ToolResult {
                    tool_use_id: call.id.clone(),
                    content,
                    images,
                });
```

(`tool_summary` is called with `&content` — unchanged.) Add a `tool_summary` arm: `"render_schematic" => "rendered schematic to PNG".to_string(),`. Import `ImageData` in `agent.rs`'s `use crate::llm::…` list.

- [ ] **Step 5: Run, then commit**

Run: `cargo test -p agent`
Expected: PASS (Task-5 test renders for real on this machine).

```bash
git add crates/agent/src/tools.rs crates/agent/src/agent.rs crates/agent/tests/tools.rs
git commit -m "feat(agent): render_schematic tool with vision feedback plumbing"
```

---

### Task 6: Draft editing tools (`create_design`, `edit_design`, seeded `get_design`, draft-default `apply_design`)

**Files:**
- Modify: `crates/agent/src/tools.rs`
- Test: `crates/agent/tests/tools.rs` (extend)

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn draft_lifecycle_create_edit_apply() {
    let Some(ctx) = agent::tools::ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let tools = agent::tools::Tools::new();
    let yaml = "
version: 1
name: t
rails: [GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
";

    // edit before create -> structured error.
    let out = tools.run("edit_design",
        serde_json::json!({"old_string": "x", "new_string": "y"}), &ctx).unwrap();
    assert!(out["error"].as_str().unwrap().contains("no draft"));

    // create seeds the draft and validates it.
    let out = tools.run("create_design", serde_json::json!({"yaml": yaml}), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true));
    // create again without overwrite -> error; with overwrite -> ok.
    let out = tools.run("create_design", serde_json::json!({"yaml": yaml}), &ctx).unwrap();
    assert!(out["error"].as_str().unwrap().contains("draft already exists"));

    // Anchored edit: ambiguity and uniqueness rules.
    let out = tools.run("edit_design",
        serde_json::json!({"old_string": "NOT-PRESENT", "new_string": "y"}), &ctx).unwrap();
    assert!(out["error"].as_str().unwrap().contains("not found"));
    let out = tools.run("edit_design",
        serde_json::json!({"old_string": "value: 1k", "new_string": "value: 4.7k"}), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true));
    assert_eq!(out["replacements"], serde_json::json!(1));

    // apply_design with NO yaml applies the draft.
    let out = tools.run("apply_design", serde_json::json!({"commit": true}), &ctx).unwrap();
    assert_eq!(out["written"], serde_json::json!(true));

    // get_design now prefers the draft and reports its source.
    let out = tools.run("get_design", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["source"], serde_json::json!("draft"));
    assert!(out["yaml"].as_str().unwrap().contains("4.7k"));
}

#[test]
fn get_design_seeds_draft_from_lift_and_flags_staleness() {
    let Some(ctx) = agent::tools::ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let tools = agent::tools::Tools::new();
    let yaml = "
version: 1
name: t
rails: [GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
";
    // Write a schematic with explicit yaml (no draft involved).
    tools.run("apply_design",
        serde_json::json!({"yaml": yaml, "commit": true}), &ctx).unwrap();

    // get_design lifts AND seeds the draft.
    let out = tools.run("get_design", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["source"], serde_json::json!("lifted"));
    let out2 = tools.run("get_design", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out2["source"], serde_json::json!("draft"));
    assert_eq!(out2.get("stale"), None);

    // Out-of-band sch edit -> staleness surfaces.
    let sch = std::fs::read_to_string(ctx.sch_path()).unwrap();
    std::fs::write(ctx.sch_path(), format!("{sch}\n")).unwrap();
    let out3 = tools.run("get_design", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out3["stale"], serde_json::json!(true));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p agent --test tools draft`
Expected: FAIL — `unknown tool: create_design` / `edit_design`.

- [ ] **Step 3: Implement**

Tool defs (append to `defs()`; also EDIT the `apply_design` def: remove `"required": ["yaml"]` and update its description to "…If `yaml` is omitted, applies the current draft (see create_design/edit_design)."; update `get_design`'s description to mention it returns the draft when one exists and otherwise lifts + seeds it):

```rust
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
```

Dispatch arms: `"create_design" => create_design(input, ctx),` and `"edit_design" => edit_design(input, ctx),`.

```rust
// ── 10. create_design / edit_design ────────────────────────────────────────

fn current_sch_text(ctx: &ToolCtx) -> Option<String> {
    std::fs::read_to_string(&ctx.sch_path).ok()
}

fn create_design(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let yaml = require_str(&input, "yaml")?;
    let overwrite = input.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
    if ctx.workspace.read_draft().is_some() && !overwrite {
        return Ok(json!({
            "error": "a draft already exists — pass overwrite=true to replace it, \
                      or use edit_design to modify it",
        }));
    }
    ctx.workspace
        .write_draft(&yaml, current_sch_text(ctx).as_deref())?;
    let mut report = compile_report(&compile(&yaml, &ctx.provider).diagnostics);
    report["draft_written"] = json!(true);
    Ok(report)
}

fn edit_design(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let old = require_str(&input, "old_string")?;
    let new = require_str(&input, "new_string")?;
    let replace_all = input.get("replace_all").and_then(Value::as_bool).unwrap_or(false);

    let Some(draft) = ctx.workspace.read_draft() else {
        return Ok(json!({
            "error": "no draft exists — call get_design (seeds a draft from the \
                      current schematic) or create_design first",
        }));
    };
    let count = draft.matches(&old).count();
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
        draft.replace(&old, &new)
    } else {
        draft.replacen(&old, &new, 1)
    };
    ctx.workspace
        .write_draft(&edited, current_sch_text(ctx).as_deref())?;

    let mut report = compile_report(&compile(&edited, &ctx.provider).diagnostics);
    report["replacements"] = json!(if replace_all { count } else { 1 });
    Ok(report)
}
```

Rewrite `get_design`:

```rust
fn get_design(ctx: &ToolCtx) -> Result<Value> {
    if let Some(draft) = ctx.workspace.read_draft() {
        let mut out = json!({ "yaml": draft, "source": "draft" });
        if ctx.workspace.draft_is_stale(current_sch_text(ctx).as_deref()) {
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
    ctx.workspace
        .write_draft(&yaml, current_sch_text(ctx).as_deref())?;
    Ok(json!({ "yaml": yaml, "source": "lifted",
               "note": "draft seeded from the schematic; use edit_design for changes" }))
}
```

In `apply_design`, replace the first line with draft fallback + staleness warning, and refresh the draft meta after a commit:

```rust
    let explicit_yaml = input.get("yaml").and_then(Value::as_str).map(str::to_string);
    let yaml = match explicit_yaml {
        Some(y) => y,
        None => match ctx.workspace.read_draft() {
            Some(d) => d,
            None => {
                return Ok(json!({
                    "error": "no yaml given and no draft exists — pass yaml, or \
                              create a draft via get_design/create_design",
                }));
            }
        },
    };
    let stale = input.get("yaml").is_none()
        && ctx.workspace.draft_is_stale(current_sch_text(ctx).as_deref());
```

…and just before the final commit `Ok(json!({ "ok": true, "written": true, … }))`, re-record the meta and surface the warning:

```rust
    // The schematic just changed under us legitimately; re-seed the meta hash
    // so the draft is no longer considered stale.
    ctx.workspace
        .write_draft(&yaml, current_sch_text(ctx).as_deref())?;
```

Add `"stale_draft_warning": stale,` to both the dry-run and commit result JSON (omit/false when not stale is fine — keep it always present for simplicity).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p agent`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/tools.rs crates/agent/tests/tools.rs
git commit -m "feat(agent): persistent draft + create_design/edit_design anchored editing"
```

---

## Part B — Engine layout primitives (Phase 2)

### Task 7: SPIKE — power symbols drive nets by Value (the riskiest assumption)

Verify with real KiCAD: (a) a stock `power:GND` symbol placed pin-coincident with a component pin puts that pin on global net `GND` (no leading `/`); (b) a donor power symbol (`power:VCC`) with its **Value overridden** to a custom rail name drives a net with that custom name; (c) `#`-prefixed power refs are excluded from the netlist component list; (d) a `PWR_FLAG` placed pin-coincident at the same point joins the net (this matters because local labels do NOT merge with global power nets — the flag can no longer be attached by label on power nets).

**Files:**
- Modify: `crates/sch-engine/src/emit.rs` (`add_power_symbol` method; hide `Reference` for `#`-prefixed refs)
- Test: `crates/sch-engine/tests/power_symbol_spike.rs` (create)

- [ ] **Step 1: Write the failing test**

```rust
//! SPIKE (spec "day-one"): KiCAD power symbols drive nets by their Value.
//! If this fails, STOP and re-design Phase 2's power-symbol approach.

use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use sch_engine::emit::SchematicWriter;

#[test]
fn power_symbol_value_names_the_net() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };

    let mut w = SchematicWriter::new();
    // R1 vertical at (127, 63.5): pin 1 endpoint (127, 59.69), pin 2 (127, 67.31).
    w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0).unwrap();
    // Stock GND at R1 pin 2 (graphic extends down; connection point = origin).
    w.add_power_symbol(&env, "power:GND", "#PWR01", "GND", [127.0, 67.31], 0.0).unwrap();
    // Donor power:VCC renamed to a custom rail at R1 pin 1.
    w.add_power_symbol(&env, "power:VCC", "#PWR02", "RAIL_CUSTOM", [127.0, 59.69], 0.0).unwrap();
    // PWR_FLAG pin-coincident with the GND attach point (label-free attachment).
    w.add_power_flag_at(&env, "#FLG01", [127.0, 67.31]).unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let sch = tmp.path().join("spike.kicad_sch");
    std::fs::write(&sch, w.finish()).unwrap();

    let nl = KicadCli::new(&env).netlist(&sch).expect("netlist");

    // (c) hidden refs are netlist-excluded.
    assert!(nl.components.iter().all(|c| !c.reference.starts_with('#')));
    assert_eq!(nl.components.len(), 1, "only R1: {:?}", nl.components);

    // (a)+(b) net names come from the power symbols' Values, globally (no '/').
    let net_of = |refdes: &str, pin: &str| {
        nl.nets
            .iter()
            .find(|n| n.nodes.contains(&(refdes.to_string(), pin.to_string())))
            .map(|n| n.name.clone())
            .unwrap_or_default()
    };
    assert_eq!(net_of("R1", "2"), "GND");
    assert_eq!(net_of("R1", "1"), "RAIL_CUSTOM");

    // (d) the coincident PWR_FLAG drives GND -> zero ERC errors on that net.
    let erc = KicadCli::new(&env).erc(&sch).expect("erc");
    let gnd_errors: Vec<_> = erc
        .violations
        .iter()
        .filter(|v| v.severity == "error" && v.kind == "power_pin_not_driven")
        .filter(|v| v.items.iter().any(|i| i.description.contains("GND")))
        .collect();
    assert!(gnd_errors.is_empty(), "GND undriven despite PWR_FLAG: {gnd_errors:?}");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p sch-engine --test power_symbol_spike`
Expected: COMPILE ERROR — `add_power_symbol` / `add_power_flag_at` missing.

- [ ] **Step 3: Implement the two writer methods** (in `emit.rs`, near `add_power_flag`)

```rust
    /// Place a power symbol (graphic power port) whose **Value names the net**.
    ///
    /// KiCAD derives a power port's global net from the symbol's Value field,
    /// so a stock `power:GND` drives `GND` and any donor symbol with an
    /// overridden Value drives that custom rail. The single pin of every
    /// `power:` symbol sits at the symbol origin, so `at` IS the connection
    /// point. `refdes` must be `#`-prefixed (hidden, netlist-excluded).
    pub fn add_power_symbol(
        &mut self,
        env: &KicadEnv,
        lib_id: &str,
        refdes: &str,
        net: &str,
        at: [f64; 2],
        angle: f64,
    ) -> io::Result<()> {
        self.add_symbol(env, lib_id, refdes, net, at, angle)
    }

    /// Place a `PWR_FLAG` whose pin is **pin-coincident** with `at`.
    ///
    /// Power nets are joined by global power ports, and a *local* label does
    /// not merge with a global net — so the flag attaches by position, not by
    /// label: its pin (at the symbol origin) lands exactly on an existing
    /// power-port connection point.
    pub fn add_power_flag_at(
        &mut self,
        env: &KicadEnv,
        refdes: &str,
        at: [f64; 2],
    ) -> io::Result<()> {
        self.add_symbol(env, "power:PWR_FLAG", refdes, "PWR_FLAG", at, 0.0)
    }
```

In `render_instance`, hide the Reference for hidden refs and make the Value the visible net name for power ports (centered under/over per KiCAD defaults is overkill — visible left-justified is fine for now):

```rust
    let hidden_ref = inst.refdes.starts_with('#');
    // … in the Reference property block, replace the effects line with:
    if hidden_ref {
        s.push_str("\t\t\t(effects (font (size 1.27 1.27)) (justify left) (hide yes))\n");
    } else {
        s.push_str("\t\t\t(effects (font (size 1.27 1.27)) (justify left))\n");
    }
```

Also hide the Value for `PWR_FLAG` instances (`inst.value == "PWR_FLAG"`) the same way — the flag's text is pure noise (it currently prints `#FLG01 PWR_FLAG` on the sheet).

- [ ] **Step 4: Run the spike**

Run: `cargo test -p sch-engine --test power_symbol_spike -- --nocapture`
Expected: PASS. **If `RAIL_CUSTOM` comes back wrong (e.g. as `VCC`), STOP: this is the spec's identified spike risk — report the actual netlist output and re-plan the custom-rail approach before continuing.**

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src/emit.rs crates/sch-engine/tests/power_symbol_spike.rs
git commit -m "feat(sch-engine): power symbol + coincident PWR_FLAG primitives (spike verified)"
```

---

### Task 8: Wires; power symbols replace power-net labels in emission

**Files:**
- Modify: `crates/sch-engine/src/emit.rs` (Wire struct + `add_wire` + `pin_directions` + render), `crates/sch-engine/src/reconcile.rs` (power-aware `emit_pin`, flag placement at attach points)
- Test: `crates/sch-engine/tests/power_symbols_emit.rs` (create)

- [ ] **Step 1: Write the failing test**

```rust
//! Power nets render as power symbols + stub wires, not text labels.

use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

fn compile(env: &KicadEnv, src: &str) -> circuit_lang::Design {
    let provider = RealSymbolProvider::new(env.clone());
    let result = circuit_lang::compile(src, &provider);
    assert!(!result.diagnostics.has_errors(), "{:?}", result.diagnostics);
    result.design.unwrap()
}

#[test]
fn power_nets_use_power_symbols_not_labels() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let design = compile(&env, "
version: 1
name: t
rails: [3V3, GND]
blocks:
  a:
    components:
      C1: {part: Device:C, value: 100nF, between: [3V3, GND]}
      R1: {part: Device:R, value: 10k, between: [SIG, GND]}
");
    let out = sch_engine::emit_design(&env, &design).unwrap();
    let text = &out.sch;

    // No power-net text labels; SIG keeps its label.
    assert!(!text.contains("(label \"GND\""), "GND must not be a label");
    assert!(!text.contains("(label \"3V3\""), "3V3 must not be a label");
    assert!(text.contains("(label \"SIG\""), "signal nets keep labels");
    // Power symbols and wires present.
    assert!(text.contains("power:GND"));
    assert!(text.contains("power:+3V3"), "3V3 must map to the stock +3V3 symbol");
    assert!(text.contains("(wire"));

    // Connectivity ground truth via netlist.
    let tmp = tempfile::tempdir().unwrap();
    let sch = tmp.path().join("t.kicad_sch");
    std::fs::write(&sch, text).unwrap();
    let nl = KicadCli::new(&env).netlist(&sch).unwrap();
    let net_of = |r: &str, p: &str| {
        nl.nets.iter()
            .find(|n| n.nodes.contains(&(r.to_string(), p.to_string())))
            .map(|n| n.name.clone()).unwrap_or_default()
    };
    assert_eq!(net_of("C1", "1"), "3V3");
    assert_eq!(net_of("C1", "2"), "GND");
    assert_eq!(net_of("R1", "2"), "GND");

    // ERC stays clean (flags attach pin-coincident now).
    let erc = KicadCli::new(&env).erc(&sch).unwrap();
    assert_eq!(erc.error_count(), 0, "{:?}", erc.violations);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p sch-engine --test power_symbols_emit`
Expected: COMPILE ERROR first (`out.sch` — `emit_design` still returns `String`; for THIS task keep `String` and write `let text = &out;` — the `EmitOutput` change comes in Task 13. Adjust the test accordingly: `let text = &out;`). Then FAIL on the `(label "GND")` assertion.

- [ ] **Step 3: Implement wires + directions in `emit.rs`**

```rust
/// One `(wire …)` segment between two grid-snapped sheet points.
struct Wire {
    a: [f64; 2],
    b: [f64; 2],
    /// Stable key for the wire uuid (content-derived from the endpoints).
    uuid_key: String,
}

/// A pin's outward direction on the sheet, quantized to the four axes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    East,
    West,
    North,
    South,
}

impl Dir {
    /// Sheet-space unit vector (sheet Y grows downward, so North is -y).
    pub fn vec(self) -> [f64; 2] {
        match self {
            Dir::East => [1.0, 0.0],
            Dir::West => [-1.0, 0.0],
            Dir::North => [0.0, -1.0],
            Dir::South => [0.0, 1.0],
        }
    }
}
```

Writer additions (`wires: Vec<Wire>` field; render in `finish` after labels, sorted by `uuid_key`):

```rust
    /// Add a wire segment between two sheet points (snapped).
    pub fn add_wire(&mut self, a: [f64; 2], b: [f64; 2]) {
        let a = snap_point(a);
        let b = snap_point(b);
        if a == b {
            return;
        }
        let uuid_key = format!("{}:{}:{}:{}", a[0], a[1], b[0], b[1]);
        self.wires.push(Wire { a, b, uuid_key });
    }

    /// Resolve a pin to its endpoint(s) AND outward direction(s) on the sheet.
    ///
    /// A pin's local `angle` points from the connection point INTO the body, so
    /// outward is `angle + 180°`, transformed exactly like the endpoint itself
    /// (mirror -> instance rotation -> sheet Y-flip) and quantized to an axis.
    pub fn pin_dirs(
        &self,
        env: &KicadEnv,
        refdes: &str,
        pin: &str,
    ) -> io::Result<Vec<([f64; 2], Dir)>> {
        let inst = self
            .instances
            .iter()
            .find(|i| i.refdes == refdes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no symbol {refdes:?}")))?;
        let (inst_at, inst_angle, inst_mirror) = (inst.at, inst.angle, inst.mirror);
        let geom = SymbolGeometry::load(env, &inst.lib_id)?;

        let matches: Vec<&PinGeom> = {
            let by_number: Vec<&PinGeom> = geom.pins.iter().filter(|p| p.number == pin).collect();
            if !by_number.is_empty() {
                by_number
            } else {
                geom.pins.iter().filter(|p| p.name == pin).collect()
            }
        };
        if matches.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no pin {pin:?} on {}", inst.lib_id),
            ));
        }
        Ok(matches
            .into_iter()
            .map(|pg| {
                let ep = pin_endpoint(pg, inst_at, inst_angle, inst_mirror);
                // Outward in symbol space:
                let theta = (pg.angle + 180.0).to_radians();
                let (mut dx, dy) = (theta.cos(), theta.sin());
                if inst_mirror {
                    dx = -dx;
                }
                let phi = inst_angle.to_radians();
                let (s, c) = phi.sin_cos();
                let rx = dx * c - dy * s;
                let ry = dx * s + dy * c;
                // Sheet flip: the sheet-space y component is -ry.
                let sy = -ry;
                let dir = if rx.abs() >= sy.abs() {
                    if rx >= 0.0 { Dir::East } else { Dir::West }
                } else if sy >= 0.0 {
                    Dir::South
                } else {
                    Dir::North
                };
                (ep, dir)
            })
            .collect())
    }
```

Wire rendering (in `finish`, after the label loop):

```rust
        let mut wires = self.wires;
        wires.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        for wire in &wires {
            let uuid = stable_uuid("wire", &wire.uuid_key);
            let _ = writeln!(
                out,
                "\t(wire\n\t\t(pts\n\t\t\t(xy {} {}) (xy {} {})\n\t\t)\n\t\t(stroke (width 0) (type default))\n\t\t(uuid \"{uuid}\")\n\t)",
                fmt_coord(wire.a[0]), fmt_coord(wire.a[1]),
                fmt_coord(wire.b[0]), fmt_coord(wire.b[1]),
            );
        }
```

Unit tests for the quantizer (in `emit.rs` `mod tests`):

```rust
    #[test]
    fn pin_outward_directions_quantize_per_rotation() {
        // Device:R pin 1: local at (0, 3.81), pin angle 270 (line runs down into
        // the body), so outward is up (North on the sheet) at instance angle 0.
        // Verified against the four rotations.
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0).unwrap();
        w.add_symbol(&env, "Device:R", "R2", "1k", [101.6, 63.5], 90.0).unwrap();
        let d1 = w.pin_dirs(&env, "R1", "1").unwrap();
        assert_eq!(d1[0].1, Dir::North);
        let d2 = w.pin_dirs(&env, "R2", "1").unwrap();
        assert_eq!(d2[0].1, Dir::West);
    }
```

- [ ] **Step 4: Implement power-aware pin emission in `reconcile.rs`**

Add constants and helpers:

```rust
/// Stub wire length from a pin to its label / power riser, in mm (3 grid units).
const STUB_MM: f64 = 3.81;
/// Vertical riser from a horizontal stub to a power symbol, in mm.
const RISER_MM: f64 = 2.54;

/// Ground-ish rails point down; everything else points up.
fn is_ground(net: &str) -> bool {
    let n = net.to_ascii_uppercase();
    n.contains("GND") || n.starts_with("VSS")
}

/// Choose the power-symbol lib_id for a rail. Exact `power:` match first, then
/// common aliases, then a donor whose Value is overridden to the rail name
/// (verified by the Task 7 spike).
fn power_lib_id(net: &str, provider: &RealSymbolProvider) -> String {
    use circuit_lang::SymbolProvider as _;
    let exact = format!("power:{net}");
    if provider.symbol(&exact).is_some() {
        return exact;
    }
    let alias = match net {
        "3V3" => Some("power:+3V3"),
        "5V" => Some("power:+5V"),
        "12V" => Some("power:+12V"),
        _ => None,
    };
    if let Some(a) = alias
        && provider.symbol(a).is_some()
    {
        return a.to_string();
    }
    if is_ground(net) { "power:GND".into() } else { "power:VCC".into() }
}
```

Replace `emit_pin` with a power-aware version (and update both call sites to pass the new arguments):

```rust
/// Emit one pin's connectivity. Signal nets: stub wire + oriented label
/// (Task 9 adds orientation; until then `add_pin_label` directly). Power nets:
/// stub wire (+ riser for horizontal pins) + power symbol whose Value is the
/// net. Records the first power attach point per net for PWR_FLAG placement.
#[allow(clippy::too_many_arguments)]
fn emit_pin(
    w: &mut SchematicWriter,
    env: &KicadEnv,
    provider: &RealSymbolProvider,
    refdes: &str,
    pin: &str,
    target: &PinTarget,
    power_nets: &std::collections::BTreeSet<String>,
    used_nets: &mut std::collections::BTreeSet<String>,
    power_attach: &mut std::collections::BTreeMap<String, [f64; 2]>,
) -> io::Result<()> {
    match target {
        PinTarget::Net(net) => {
            used_nets.insert(net.clone());
            if power_nets.contains(net) {
                emit_power_pin(w, env, provider, refdes, pin, net, power_attach)
            } else {
                w.add_pin_label(env, refdes, pin, net)
            }
        }
        PinTarget::NoConnect => w.add_no_connect(env, refdes, pin),
    }
}

fn emit_power_pin(
    w: &mut SchematicWriter,
    env: &KicadEnv,
    provider: &RealSymbolProvider,
    refdes: &str,
    pin: &str,
    net: &str,
    power_attach: &mut std::collections::BTreeMap<String, [f64; 2]>,
) -> io::Result<()> {
    use crate::emit::Dir;
    let lib_id = power_lib_id(net, provider);
    let down = is_ground(net);

    for (idx, (ep, dir)) in w.pin_dirs(env, refdes, pin)?.into_iter().enumerate() {
        let v = dir.vec();
        let stub_end = [ep[0] + v[0] * STUB_MM, ep[1] + v[1] * STUB_MM];
        w.add_wire(ep, stub_end);

        // Vertical pins attach the symbol straight on the stub; horizontal pins
        // take a short riser toward the conventional side (GND down, rails up).
        let (attach, angle) = match dir {
            Dir::North | Dir::South => {
                // Symbol flips when the pin points against convention.
                let conventional = (down && dir == Dir::South) || (!down && dir == Dir::North);
                (stub_end, if conventional { 0.0 } else { 180.0 })
            }
            Dir::East | Dir::West => {
                let dy = if down { RISER_MM } else { -RISER_MM };
                let attach = [stub_end[0], stub_end[1] + dy];
                w.add_wire(stub_end, attach);
                (attach, 0.0)
            }
        };
        let pref = format!("#PWR_{refdes}_{pin}_{idx}");
        w.add_power_symbol(env, &lib_id, &pref, net, attach, angle)?;
        power_attach.entry(net.to_string()).or_insert(attach);
    }
    Ok(())
}
```

In `emit_design_reconciled`: build `let power_nets: BTreeSet<String> = design.nets.iter().filter(|(_, a)| a.power).map(|(n, _)| n.clone()).collect();` and `let mut power_attach = BTreeMap::new();` before the block loop; thread both into the `emit_pin` calls. Then change the flag loop: a net present in `power_attach` gets `w.add_power_flag_at(env, &refdes, power_attach[net])?;` (pin-coincident, drives the global net); a needs-flag net **not** in `power_attach` (undeclared power-input net, still label-connected) keeps today's column + `add_power_flag` label behavior.

- [ ] **Step 5: Run, then commit**

Run: `cargo test -p sch-engine`
Expected: PASS, including the existing `bluepill_emit`/`emit_connected` integration tests (their nets still connect; bluepill ERC stays at 0 errors). If `bluepill_emit` asserts on exact text containing power-net labels, update those assertions to the new power-symbol form.

```bash
git add crates/sch-engine/src
git commit -m "feat(sch-engine): power symbols + stub wires replace power-net labels"
```

---

### Task 9: Oriented labels on stub wires for signal nets

**Files:**
- Modify: `crates/sch-engine/src/emit.rs` (PinLabel gains a `Dir`; oriented `render_label`; new `add_signal_label`), `crates/sch-engine/src/reconcile.rs` (signal path calls `add_signal_label`)
- Test: extend `crates/sch-engine/tests/power_symbols_emit.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn signal_labels_sit_on_stubs_and_orient_away_from_the_body() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let design = compile(&env, "
version: 1
name: t
rails: [GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 10k, between: [SIG, GND]}
      R2: {part: Device:R, value: 10k, between: [SIG, GND]}
");
    let out = sch_engine::emit_design(&env, &design).unwrap();
    let text = &out; // adjust to out.sch after Task 13

    // Labels exist with a non-zero rotation possibility and connectivity holds.
    assert!(text.contains("(label \"SIG\""));
    let tmp = tempfile::tempdir().unwrap();
    let sch = tmp.path().join("t.kicad_sch");
    std::fs::write(&sch, text).unwrap();
    let nl = kicad_bridge::cli::KicadCli::new(&env).netlist(&sch).unwrap();
    let sig = nl.nets.iter().find(|n| n.name.ends_with("SIG")).expect("SIG net");
    assert_eq!(sig.nodes.len(), 2, "both R pins join SIG through their stubs: {sig:?}");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p sch-engine --test power_symbols_emit signal_labels`
Expected: FAIL or PASS-by-luck on connectivity but without stubs — the real red gate is the unit test below; add it too (in `emit.rs` tests):

```rust
    #[test]
    fn label_orientation_per_direction() {
        let mk = |dir| PinLabel {
            net: "X".into(),
            at: [0.0, 0.0],
            uuid_key: "k".into(),
            dir,
        };
        assert!(render_label(&mk(Dir::East)).contains("(at 0 0 0)"));
        assert!(render_label(&mk(Dir::East)).contains("justify left"));
        assert!(render_label(&mk(Dir::West)).contains("(at 0 0 180)"));
        assert!(render_label(&mk(Dir::West)).contains("justify right"));
        assert!(render_label(&mk(Dir::North)).contains("(at 0 0 90)"));
        assert!(render_label(&mk(Dir::South)).contains("(at 0 0 270)"));
    }
```

- [ ] **Step 3: Implement**

`PinLabel` gains `dir: Dir` (default `Dir::East` for the legacy `add_pin_label` path, which stays for PWR_FLAG/no-stub uses). New writer method:

```rust
    /// Signal-net connectivity with breathing room: a stub wire out of the pin
    /// and the net label at the stub's far end, oriented along the stub so the
    /// text reads away from the symbol body.
    pub fn add_signal_label(
        &mut self,
        env: &KicadEnv,
        refdes: &str,
        pin: &str,
        net: &str,
    ) -> io::Result<()> {
        const STUB_MM: f64 = 3.81;
        for (idx, (ep, dir)) in self.pin_dirs(env, refdes, pin)?.into_iter().enumerate() {
            let v = dir.vec();
            let end = [ep[0] + v[0] * STUB_MM, ep[1] + v[1] * STUB_MM];
            self.add_wire(ep, end);
            self.labels.push(PinLabel {
                net: net.to_string(),
                at: snap_point(end),
                uuid_key: format!("{refdes}:{pin}:{net}:{idx}"),
                dir,
            });
        }
        Ok(())
    }
```

`render_label` derives angle/justify from `dir` (KiCAD reads E/W horizontally and N/S bottom-up/top-down; the **visual correctness of this mapping must be eyeballed via the Task 15 render harness** — it is the one convention here not machine-checked):

```rust
fn render_label(label: &PinLabel) -> String {
    let x = fmt_coord(label.at[0]);
    let y = fmt_coord(label.at[1]);
    let net = escape_sexpr_string(&label.net);
    let uuid = stable_uuid("label", &label.uuid_key);
    let (angle, justify) = match label.dir {
        Dir::East => (0, "left"),
        Dir::West => (180, "right"),
        Dir::North => (90, "left"),
        Dir::South => (270, "right"),
    };

    let mut s = String::new();
    let _ = writeln!(s, "\t(label \"{net}\"");
    let _ = writeln!(s, "\t\t(at {x} {y} {angle})");
    let _ = writeln!(
        s,
        "\t\t(effects (font (size 1.27 1.27)) (justify {justify} bottom))"
    );
    let _ = writeln!(s, "\t\t(uuid \"{uuid}\")");
    s.push_str("\t)\n");
    s
}
```

In `reconcile.rs::emit_pin`, the signal branch becomes `w.add_signal_label(env, refdes, pin, net)`.

- [ ] **Step 4: Run all engine tests**

Run: `cargo test -p sch-engine`
Expected: PASS (connectivity tests prove labels-on-stub-ends still join nets).

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src crates/sch-engine/tests
git commit -m "feat(sch-engine): stub wires + direction-oriented signal labels"
```

---

### Task 10: Rail-span passive orientation (`layout_role`)

**Files:**
- Modify: `crates/circuit-lang/src/model.rs` (LayoutRole + Component field), `crates/circuit-lang/src/desugar.rs` (set the role), `crates/sch-engine/src/reconcile.rs` (initial angle from role)
- Test: `crates/circuit-lang/tests/` inline in desugar tests + extend `power_symbols_emit.rs`

- [ ] **Step 1: Model + desugar with failing test**

`model.rs`:

```rust
/// Placement-relevant role inferred at desugar time (spec Phase 2 item 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutRole {
    /// A two-pin passive strung between a power rail and ground: drawn
    /// vertical, rail-side pin up, ground-side pin down.
    RailSpan,
}
```

`Component` gains `pub layout_role: Option<LayoutRole>,` (add `layout_role: None` to the `Default` impl).

Desugar rule (place where `between:` / decouple caps build their pin maps; the exact insertion point is wherever the desugared `Component` is constructed): a component qualifies when it has exactly two pins, both `PinTarget::Net`, and **at least one** target is a declared power net:

```rust
/// True when `net` is declared power in the surface design's rails/nets.
fn is_power_net(net: &str, power_nets: &std::collections::BTreeSet<String>) -> bool {
    power_nets.contains(net)
}

fn infer_layout_role(
    pins: &indexmap::IndexMap<String, PinTarget>,
    power_nets: &std::collections::BTreeSet<String>,
) -> Option<LayoutRole> {
    if pins.len() != 2 {
        return None;
    }
    let nets: Vec<&str> = pins
        .values()
        .filter_map(|t| match t {
            PinTarget::Net(n) => Some(n.as_str()),
            PinTarget::NoConnect => None,
        })
        .collect();
    if nets.len() == 2 && nets.iter().any(|n| is_power_net(n, power_nets)) {
        Some(LayoutRole::RailSpan)
    } else {
        None
    }
}
```

Failing test (in `desugar.rs` tests or the crate's existing test style):

```rust
    #[test]
    fn between_rails_infers_rail_span_role() {
        let provider = crate::MockSymbolProvider::with_basics();
        let result = crate::compile(
            "
version: 1
name: t
rails: [3V3, GND]
blocks:
  a:
    components:
      C1: {part: C, value: 100nF, between: [3V3, GND]}
      R1: {part: R, value: 10k, between: [A, B]}
",
            &provider,
        );
        let d = result.design.unwrap();
        use crate::model::LayoutRole;
        assert_eq!(d.blocks["a"].components["C1"].layout_role, Some(LayoutRole::RailSpan));
        assert_eq!(d.blocks["a"].components["R1"].layout_role, None);
    }
```

- [ ] **Step 2: Run, implement, re-run**

Run: `cargo test -p circuit-lang between_rails_infers` → COMPILE ERROR → implement → PASS. Decouple-cap synthesis sets `layout_role: Some(LayoutRole::RailSpan)` directly on the synthesized caps (they are by construction rail↔GND).

- [ ] **Step 3: Use the role at initial placement** (`reconcile.rs`)

In `emit_design_reconciled`'s per-component placement, replace the hardcoded `0.0` initial angle:

```rust
                None => (
                    auto.positions.get(refdes).copied().unwrap_or([0.0, 0.0]),
                    initial_angle(comp),
                    None,
                ),
```

```rust
/// Initial orientation for a freshly placed component. RailSpan passives stand
/// vertical with the ground-side pin down; KiCAD's Device:R / Device:C bodies
/// are already vertical at angle 0 with pin "1" on top, so the only decision is
/// whether to flip: pin "1" tied to ground -> 180°.
fn initial_angle(comp: &circuit_lang::model::Component) -> f64 {
    use circuit_lang::model::LayoutRole;
    if comp.layout_role != Some(LayoutRole::RailSpan) {
        return 0.0;
    }
    match comp.pins.get("1") {
        Some(PinTarget::Net(n)) if is_ground(n) => 180.0,
        _ => 0.0,
    }
}
```

Engine test (extend `power_symbols_emit.rs`):

```rust
#[test]
fn rail_span_passive_flips_when_pin1_is_ground() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let design = compile(&env, "
version: 1
name: t
rails: [3V3, GND]
blocks:
  a:
    components:
      C1: {part: Device:C, value: 100nF, between: [3V3, GND]}
      C2: {part: Device:C, value: 100nF, between: [GND, 3V3]}
");
    let text = sch_engine::emit_design(&env, &design).unwrap(); // .sch after Task 13
    // C1: pin1=3V3 -> angle 0. C2: pin1=GND -> flipped 180.
    let c2_block = text.split("(property \"Reference\" \"C2\"").next().unwrap();
    let c2_at = c2_block.rsplit("(symbol\n").next().unwrap();
    assert!(c2_at.contains(" 180)"), "C2 must be flipped: …{}", &c2_at[..200.min(c2_at.len())]);
}
```

(If string-slicing proves brittle, assert via `kiutils_kicad::SchematicFile` instead: read the temp file, find the symbol with reference `C2`, assert `angle == Some(180.0)` — kiutils already exposes `at`/`angle`/`reference` as used in `reconcile.rs::parse_prior`.)

- [ ] **Step 4: Run everything**

Run: `cargo test -p circuit-lang && cargo test -p sch-engine`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/circuit-lang/src crates/sch-engine/src crates/sch-engine/tests
git commit -m "feat: rail-span layout role orients passives vertically"
```

---

### Task 11: Bbox-aware cells + decoupling-bank rows in `place()`

**Files:**
- Modify: `crates/kicad-bridge/src/geometry.rs` (`approx_size`), `crates/sch-engine/src/place.rs` (SizeMap parameter, per-cluster envelopes, bank rows), `crates/sch-engine/src/reconcile.rs` + `crates/sch-engine/src/lib.rs` (build + pass the SizeMap)
- Test: `place.rs` inline tests + `geometry.rs` test

- [ ] **Step 1: `SymbolGeometry::approx_size` with failing test**

```rust
    /// Approximate body extents `[width, height]` in mm, derived from pin
    /// connection points (pins bound the drawn body closely for almost every
    /// KiCAD symbol). Floors at 5.08 mm and pads 2.54 mm per side so even a
    /// bare two-pin passive gets a sane footprint.
    pub fn approx_size(&self) -> [f64; 2] {
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for p in &self.pins {
            min_x = min_x.min(p.at[0]);
            max_x = max_x.max(p.at[0]);
            min_y = min_y.min(p.at[1]);
            max_y = max_y.max(p.at[1]);
        }
        [
            (max_x - min_x).max(5.08) + 5.08,
            (max_y - min_y).max(5.08) + 5.08,
        ]
    }
```

Test (kicad-bridge `tests/geometry.rs`, SKIP-gated like its peers): `Device:R` ≈ `[10.16, 12.7]` (pins ±3.81 in y); an MCU symbol must come back much larger than a passive.

```rust
#[test]
fn approx_size_scales_with_symbol() {
    let Some(env) = KicadEnv::detect() else { return };
    let r = SymbolGeometry::load(&env, "Device:R").unwrap().approx_size();
    assert!(r[1] > r[0], "R is taller than wide: {r:?}");
    assert!(r[1] <= 15.0, "passive stays small: {r:?}");
}
```

- [ ] **Step 2: Re-shape `place()`**

New signature and constants (this REPLACES the fixed `CELL_MM` layout; keep `CELL_MM` as the no-size fallback):

```rust
pub type SizeMap = indexmap::IndexMap<RefDes, [f64; 2]>;

/// Clearance added around a symbol's bbox for stubs, labels, and fields, mm.
const CLEARANCE_MM: f64 = 15.24;

/// Snap a length up to the 2.54 mm placement grid.
fn snap_up(v: f64) -> f64 {
    (v / 2.54).ceil() * 2.54
}

/// A component's cell extents: bbox + clearance, or the legacy fallback.
fn cell_of(refdes: &RefDes, sizes: &SizeMap) -> [f64; 2] {
    match sizes.get(refdes) {
        Some(s) => [snap_up(s[0] + CLEARANCE_MM), snap_up(s[1] + CLEARANCE_MM)],
        None => [CELL_MM, CELL_MM],
    }
}

pub fn place(design: &Design, sizes: &SizeMap) -> Layout {
```

Cluster layout becomes envelope-based: a multi-cell cluster (parent + decouple caps) is the parent's cell with a **single row of cap cells to its right, top-aligned** (the decoupling bank); a singleton is just its cell. Replace `layout_block_cells` with:

```rust
/// One cluster laid out: per-refdes offsets (cell centers) relative to the
/// cluster's top-left corner, plus the cluster envelope [w, h].
fn layout_cluster(cluster: &Cluster, sizes: &SizeMap) -> (Vec<(RefDes, [f64; 2])>, [f64; 2]) {
    let parent_cell = cell_of(&cluster[0], sizes);
    let mut offsets = vec![(cluster[0].clone(), [parent_cell[0] / 2.0, parent_cell[1] / 2.0])];
    let mut x = parent_cell[0];
    let mut h = parent_cell[1];
    for child in &cluster[1..] {
        let c = cell_of(child, sizes);
        offsets.push((child.clone(), [x + c[0] / 2.0, c[1] / 2.0]));
        x += c[0];
        h = h.max(c[1]);
    }
    (offsets, [x, h])
}

/// Lay a block's clusters into rows of envelopes. Rows wrap at the block's
/// target width (~square in total area). Returns per-refdes positions relative
/// to the block origin plus the block envelope [w, h].
fn layout_block(block: &Block, sizes: &SizeMap) -> (Vec<(RefDes, [f64; 2])>, [f64; 2]) {
    let clusters = block_clusters(block);
    let laid: Vec<_> = clusters.iter().map(|c| layout_cluster(c, sizes)).collect();
    let total_area: f64 = laid.iter().map(|(_, e)| e[0] * e[1]).sum();
    let target_w = total_area.sqrt().max(laid.iter().map(|(_, e)| e[0]).fold(0.0, f64::max));

    let mut out = Vec::new();
    let (mut x, mut y, mut row_h, mut max_w) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for (offsets, env) in &laid {
        if x > 0.0 && x + env[0] > target_w {
            y += row_h;
            x = 0.0;
            row_h = 0.0;
        }
        for (refdes, off) in offsets {
            out.push((refdes.clone(), [x + off[0], y + off[1]]));
        }
        x += env[0];
        row_h = row_h.max(env[1]);
        max_w = max_w.max(x);
    }
    (out, [max_w, y + row_h])
}
```

`place()` keeps its band logic but works in mm envelopes instead of cell counts: band x-advance uses the widest block envelope in the band; per block, positions are `snap_point([x0 + rel_x, y0 + rel_y])` and the band y-cursor advances by the block envelope height + `BLOCK_GAP_MM`.

- [ ] **Step 3: Update callers + tests**

- `reconcile.rs::emit_design_reconciled`: build the SizeMap before placing:

```rust
    let mut sizes = place::SizeMap::new();
    for block in design.blocks.values() {
        for (refdes, comp) in &block.components {
            if let Ok(g) = kicad_bridge::geometry::SymbolGeometry::load(env, &comp.part) {
                sizes.insert(refdes.clone(), g.approx_size());
            }
        }
    }
    let auto = place::place(design, &sizes);
```

- `place.rs` tests: call `place(&design, &SizeMap::new())` (fallback cells keep the existing invariant tests meaningful). The `cluster_bound` adjacency test changes shape: with bank rows the farthest cap is `Σ child widths` away horizontally — update the bound to `CELL_MM * cluster_len as f64` (still excludes the old 50 mm row-wrap pathology for the fallback cell size, and the row layout keeps caps strictly adjacent). Add one new test:

```rust
    #[test]
    fn decouple_caps_form_a_row_beside_parent() {
        // (reuse the MockSymbolProvider design from the existing decouple test)
        let layout = place(&design, &SizeMap::new());
        let u1 = layout.positions["U1"];
        let mut cap_pos: Vec<[f64; 2]> = cap_keys.iter().map(|c| layout.positions[*c]).collect();
        cap_pos.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap());
        for p in &cap_pos {
            assert!(p[0] > u1[0], "caps sit to the right of the parent");
        }
        // All caps share one row (same y).
        assert!(cap_pos.windows(2).all(|w| (w[0][1] - w[1][1]).abs() < 1e-6));
    }
```

- [ ] **Step 4: Run everything**

Run: `cargo test -p kicad-bridge && cargo test -p sch-engine`
Expected: PASS (bluepill integration still ERC-clean; positions changed, invariants hold).

- [ ] **Step 5: Commit**

```bash
git add crates/kicad-bridge/src crates/kicad-bridge/tests crates/sch-engine/src
git commit -m "feat(sch-engine): bbox-aware placement cells + decoupling bank rows"
```

---

### Task 12: Block frames, titles, and field placement

**Files:**
- Modify: `crates/sch-engine/src/emit.rs` (`add_text`, `add_rect`, Instance `half_extents`, field offsets), `crates/sch-engine/src/reconcile.rs` (frame emission per block)
- Test: extend `power_symbols_emit.rs`

- [ ] **Step 1: Failing test**

```rust
#[test]
fn blocks_get_title_text_and_frame() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let design = compile(&env, "
version: 1
name: t
rails: [GND]
blocks:
  power_supply:
    components:
      R1: {part: Device:R, value: 1k, between: [A, GND]}
");
    let text = sch_engine::emit_design(&env, &design).unwrap(); // .sch after Task 13
    assert!(text.contains("(text \"power_supply\""), "block title text");
    assert!(text.contains("(rectangle"), "block frame");
}
```

- [ ] **Step 2: Writer primitives** (`emit.rs`)

```rust
/// Free-standing sheet text (block titles / annotations).
struct SheetText {
    text: String,
    at: [f64; 2],
    /// Font size (mm); titles 2.54, annotations 1.27.
    size: f64,
    bold: bool,
    uuid_key: String,
}

/// A graphic rectangle (block frame).
struct SheetRect {
    start: [f64; 2],
    end: [f64; 2],
    uuid_key: String,
}
```

```rust
    /// Add free-standing text to the sheet.
    pub fn add_text(&mut self, text: &str, at: [f64; 2], size: f64, bold: bool, key: &str) {
        self.texts.push(SheetText {
            text: text.to_string(),
            at: snap_point(at),
            size,
            bold,
            uuid_key: key.to_string(),
        });
    }

    /// Add a graphic rectangle (no fill, dashed) to the sheet.
    pub fn add_rect(&mut self, start: [f64; 2], end: [f64; 2], key: &str) {
        self.rects.push(SheetRect {
            start: snap_point(start),
            end: snap_point(end),
            uuid_key: key.to_string(),
        });
    }
```

Render (in `finish`, after wires; both sorted by `uuid_key`):

```rust
        let mut texts = self.texts;
        texts.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        let mut rects = self.rects;
        rects.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        for t in &texts {
            let body = escape_sexpr_string(&t.text);
            let uuid = stable_uuid("text", &t.uuid_key);
            let weight = if t.bold { " bold" } else { "" };
            let _ = writeln!(
                out,
                "\t(text \"{body}\"\n\t\t(exclude_from_sim no)\n\t\t(at {} {} 0)\n\t\t(effects (font (size {sz} {sz}){weight}) (justify left bottom))\n\t\t(uuid \"{uuid}\")\n\t)",
                fmt_coord(t.at[0]), fmt_coord(t.at[1]), sz = t.size,
            );
        }
        for r in &rects {
            let uuid = stable_uuid("rect", &r.uuid_key);
            let _ = writeln!(
                out,
                "\t(rectangle\n\t\t(start {} {})\n\t\t(end {} {})\n\t\t(stroke (width 0.1524) (type dash))\n\t\t(fill (type none))\n\t\t(uuid \"{uuid}\")\n\t)",
                fmt_coord(r.start[0]), fmt_coord(r.start[1]),
                fmt_coord(r.end[0]), fmt_coord(r.end[1]),
            );
        }
```

**Field placement:** `Instance` gains `half_extents: [f64; 2]`, captured in `add_symbol_full` from the loaded geometry (`approx_size()` halved; the geometry is already loaded there for `lib_symbols` registration — load it unconditionally now and cache the size). In `render_instance`, replace the fixed `+2.54` offsets so fields clear the body:

```rust
    let ref_x = fmt_coord(x + inst.half_extents[0] + 1.27);
    let ref_y = fmt_coord(y - 1.27);
    let val_x = fmt_coord(x + inst.half_extents[0] + 1.27);
    let val_y = fmt_coord(y + 1.27);
```

- [ ] **Step 3: Frame emission** (`reconcile.rs`, after all components of all blocks are placed — collect per-block min/max over each member's position ± its half-extents from the SizeMap)

```rust
    // Block frames + titles: drawn around the *current* positions (preserved or
    // auto), regenerated every emit like all decoration.
    const FRAME_PAD_MM: f64 = 7.62;
    for (block_name, block) in &design.blocks {
        let mut bounds: Option<[f64; 4]> = None; // min_x, min_y, max_x, max_y
        for (refdes, comp) in &block.components {
            let identity = Identity::of(refdes, &comp.origin);
            let Some(at) = prior_map
                .get(&identity)
                .map(|p| p.at)
                .or_else(|| auto.positions.get(refdes).copied())
            else {
                continue;
            };
            let half = sizes
                .get(refdes)
                .map(|s| [s[0] / 2.0, s[1] / 2.0])
                .unwrap_or([12.7, 12.7]);
            let b = bounds.get_or_insert([f64::MAX, f64::MAX, f64::MIN, f64::MIN]);
            b[0] = b[0].min(at[0] - half[0]);
            b[1] = b[1].min(at[1] - half[1]);
            b[2] = b[2].max(at[0] + half[0]);
            b[3] = b[3].max(at[1] + half[1]);
        }
        let Some(b) = bounds else { continue };
        let start = [b[0] - FRAME_PAD_MM, b[1] - FRAME_PAD_MM];
        let end = [b[2] + FRAME_PAD_MM, b[3] + FRAME_PAD_MM];
        w.add_rect(start, end, &format!("frame:{block_name}"));
        w.add_text(
            block_name,
            [start[0], start[1] - 1.27],
            2.54,
            true,
            &format!("title:{block_name}"),
        );
        if let Some(note) = &block.note {
            w.add_text(
                note,
                [start[0], end[1] + 3.81],
                1.27,
                false,
                &format!("note:{block_name}"),
            );
        }
    }
```

(This requires the `sizes` map from Task 11 to be in scope — it is, same function.)

- [ ] **Step 4: Run, check bluepill still ERC-clean**

Run: `cargo test -p sch-engine`
Expected: PASS. Graphic text/rectangles are ERC-inert; `parse_prior` ignores non-symbol nodes (kiutils only iterates `symbols`).

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src crates/sch-engine/tests
git commit -m "feat(sch-engine): block frames, titles, notes, and clear field placement"
```

---

### Task 13: Layout lint + `EmitOutput`

**Files:**
- Modify: `crates/sch-engine/src/emit.rs` (`layout_warnings()`), `crates/sch-engine/src/reconcile.rs` + `crates/sch-engine/src/lib.rs` (return `EmitOutput`), `crates/agent/src/tools.rs` (surface `layout_warnings` in `apply_design`), all engine tests using `emit_design*` (`.sch` field)
- Test: `emit.rs` inline

- [ ] **Step 1: Failing test**

```rust
    #[test]
    fn layout_lint_flags_overlapping_text() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        // Two symbols stacked nearly on top of each other -> collision.
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0).unwrap();
        w.add_symbol(&env, "Device:R", "R2", "2k", [127.0, 64.77], 0.0).unwrap();
        let warnings = w.layout_warnings();
        assert!(
            warnings.iter().any(|s| s.contains("R1") && s.contains("R2")),
            "expected an R1/R2 overlap warning, got {warnings:?}"
        );

        // Far apart -> clean.
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0).unwrap();
        w.add_symbol(&env, "Device:R", "R2", "2k", [177.8, 63.5], 0.0).unwrap();
        assert!(w.layout_warnings().is_empty());
    }
```

- [ ] **Step 2: Implement the lint** (in `emit.rs`)

```rust
/// An axis-aligned bbox: [min_x, min_y, max_x, max_y].
type BBox = [f64; 4];

fn boxes_overlap(a: &BBox, b: &BBox) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

impl SchematicWriter {
    /// Deterministic readability lint over everything placed so far: symbol
    /// bodies (from their half-extents) and label text (estimated at 1.1 mm
    /// per character along the label's direction, 1.6 mm tall). Returns one
    /// human-readable warning per overlapping pair. Power symbols and wires
    /// are exempt (they legitimately touch the pins they serve).
    pub fn layout_warnings(&self) -> Vec<String> {
        let mut items: Vec<(String, BBox)> = Vec::new();
        for inst in &self.instances {
            if inst.refdes.starts_with('#') {
                continue;
            }
            let h = inst.half_extents;
            items.push((
                format!("symbol {}", inst.refdes),
                [inst.at[0] - h[0], inst.at[1] - h[1], inst.at[0] + h[0], inst.at[1] + h[1]],
            ));
        }
        for label in &self.labels {
            let len = label.net.chars().count() as f64 * 1.1;
            let b = match label.dir {
                Dir::East => [label.at[0], label.at[1] - 1.6, label.at[0] + len, label.at[1]],
                Dir::West => [label.at[0] - len, label.at[1] - 1.6, label.at[0], label.at[1]],
                Dir::North => [label.at[0] - 1.6, label.at[1] - len, label.at[0], label.at[1]],
                Dir::South => [label.at[0], label.at[1], label.at[0] + 1.6, label.at[1] + len],
            };
            items.push((format!("label \"{}\" at {:?}", label.net, label.at), b));
        }
        let mut warnings = Vec::new();
        for i in 0..items.len() {
            for j in (i + 1)..items.len() {
                if boxes_overlap(&items[i].1, &items[j].1) {
                    warnings.push(format!("{} overlaps {}", items[i].0, items[j].0));
                }
            }
        }
        warnings
    }
}
```

- [ ] **Step 3: `EmitOutput` plumbing**

In `reconcile.rs`:

```rust
/// The rendered schematic plus deterministic readability findings.
pub struct EmitOutput {
    pub sch: String,
    pub layout_warnings: Vec<String>,
}
```

`emit_design_reconciled` computes `let layout_warnings = w.layout_warnings();` just before `w.finish()` and returns `Ok(EmitOutput { sch: w.finish(), layout_warnings })`. `lib.rs::emit_design` returns `io::Result<EmitOutput>`. Update every caller: engine tests (`.sch` / `&out.sch`, including the Task 8–12 tests written with the `// .sch after Task 13` notes), and `tools.rs::apply_design`:

```rust
    let emitted = emit_design_reconciled(&ctx.env, &design, prior_text.as_deref())
        .context("rendering reconciled schematic")?;
    let rendered = emitted.sch;
    // … include in BOTH the dry-run and commit results:
    "layout_warnings": emitted.layout_warnings,
```

- [ ] **Step 4: Run workspace-wide**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine crates/agent/src/tools.rs
git commit -m "feat(sch-engine): deterministic layout lint surfaced through apply_design"
```

---

### Task 14: `ap_layout_rev` + `relayout` — hint changes re-place, drags survive

**Files:**
- Modify: `crates/sch-engine/src/reconcile.rs` (rev computation, PriorPlacement field, preservation gate, `Relayout` parameter), `crates/sch-engine/src/lib.rs`, `crates/agent/src/tools.rs` (`relayout` input)
- Test: `crates/sch-engine/tests/layout_rev.rs` (create)

- [ ] **Step 1: Write the failing test**

```rust
//! Layout-revision reconciliation: hint changes re-place exactly the affected
//! block; user drags survive everywhere else; relayout=all starts fresh.

use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;
use sch_engine::reconcile::{Relayout, emit_design_reconciled};

fn compile(env: &KicadEnv, src: &str) -> circuit_lang::Design {
    let provider = RealSymbolProvider::new(env.clone());
    let r = circuit_lang::compile(src, &provider);
    assert!(!r.diagnostics.has_errors(), "{:?}", r.diagnostics);
    r.design.unwrap()
}

/// Read R1's position out of a rendered schematic via kiutils.
fn pos_of(sch_text: &str, refdes: &str) -> [f64; 2] {
    let tmp = tempfile::Builder::new().suffix(".kicad_sch").tempfile().unwrap();
    std::fs::write(tmp.path(), sch_text).unwrap();
    let doc = kiutils_kicad::SchematicFile::read(tmp.path()).unwrap();
    doc.ast().symbols.iter()
        .find(|s| s.reference.as_deref() == Some(refdes))
        .and_then(|s| s.at)
        .expect("symbol with position")
}

const SRC_A: &str = "
version: 1
name: t
rails: [GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
  b:
    components:
      R2: {part: Device:R, value: 1k, between: [N2, GND]}
";
// Same design, but block `a` re-hinted to the right edge.
const SRC_B: &str = "
version: 1
name: t
rails: [GND]
blocks:
  a:
    layout: {edge: right}
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
  b:
    components:
      R2: {part: Device:R, value: 1k, between: [N2, GND]}
";

#[test]
fn hint_change_replaces_only_that_block_and_drags_survive_elsewhere() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let d_a = compile(&env, SRC_A);
    let first = emit_design_reconciled(&env, &d_a, None, &Relayout::None).unwrap().sch;

    // Simulate user drags: move BOTH R1 and R2 to recognizable spots.
    let dragged = first
        .replace(
            &format!("(at {} {} 0)", pos_of(&first, "R1")[0], pos_of(&first, "R1")[1]),
            "(at 254 127 0)",
        )
        .replace(
            &format!("(at {} {} 0)", pos_of(&first, "R2")[0], pos_of(&first, "R2")[1]),
            "(at 254 152.4 0)",
        );

    // Re-emit SAME design: both drags preserved.
    let same = emit_design_reconciled(&env, &d_a, Some(&dragged), &Relayout::None).unwrap().sch;
    assert_eq!(pos_of(&same, "R1"), [254.0, 127.0]);
    assert_eq!(pos_of(&same, "R2"), [254.0, 152.4]);

    // Re-emit with block `a` re-hinted: R1 re-placed, R2's drag survives.
    let d_b = compile(&env, SRC_B);
    let rehinted = emit_design_reconciled(&env, &d_b, Some(&dragged), &Relayout::None).unwrap().sch;
    assert_ne!(pos_of(&rehinted, "R1"), [254.0, 127.0], "hint change must re-place R1");
    assert_eq!(pos_of(&rehinted, "R2"), [254.0, 152.4], "untouched block keeps the drag");

    // relayout=all discards every prior position.
    let fresh = emit_design_reconciled(&env, &d_a, Some(&dragged), &Relayout::All).unwrap().sch;
    assert_ne!(pos_of(&fresh, "R1"), [254.0, 127.0]);
    assert_ne!(pos_of(&fresh, "R2"), [254.0, 152.4]);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p sch-engine --test layout_rev`
Expected: COMPILE ERROR — `Relayout` missing / wrong arity.

- [ ] **Step 3: Implement** (`reconcile.rs`)

```rust
/// Property key recording the layout-revision a component was placed under.
pub const AP_LAYOUT_REV: &str = "ap_layout_rev";

/// Which prior placements to discard on re-emit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Relayout {
    /// Honor every surviving prior placement whose layout-rev still matches.
    #[default]
    None,
    /// Discard all prior placements; the placer lays out everything fresh.
    All,
    /// Discard prior placements only for the named blocks.
    Blocks(std::collections::BTreeSet<String>),
}

/// The layout revision of one component: a content hash of its
/// placement-relevant inputs (spec "Reconciliation & lift impact"). Membership
/// is deliberately NOT hashed — adding a neighbor must not blow away drags.
fn layout_rev(block: &circuit_lang::model::Block, comp: &circuit_lang::model::Component) -> String {
    let desc = format!(
        "edge={:?}|near={:?}|role={:?}",
        block.layout.edge, block.layout.near, comp.layout_role,
    );
    crate::ids::stable_uuid("layout_rev", &desc)
}
```

- `PriorPlacement` gains `pub layout_rev: Option<String>,`; `parse_prior` fills it from the symbol's `AP_LAYOUT_REV` property (the `prop` closure already reads properties).
- `ap_properties` gains the rev: change its signature to `ap_properties(block_name: &str, origin: &Origin, rev: &str)` and push `(AP_LAYOUT_REV.to_string(), rev.to_string())`.
- `emit_design_reconciled` gains the parameter: `pub fn emit_design_reconciled(env: &KicadEnv, design: &Design, prior: Option<&str>, relayout: &Relayout) -> io::Result<EmitOutput>`. The preservation gate becomes:

```rust
            let rev = layout_rev(block, comp);
            let block_relayout = match relayout {
                Relayout::All => true,
                Relayout::Blocks(names) => names.contains(block_name),
                Relayout::None => false,
            };
            let prior_ok = |p: &PriorPlacement| {
                // An old file with no recorded rev preserves (explicit relayout
                // is the migration path); a recorded rev must match.
                p.layout_rev.as_deref().is_none_or(|r| r == rev)
            };
            let (at, angle, uuid) = match prior_map.get(&identity) {
                Some(p) if !block_relayout && prior_ok(p) => {
                    (snap_point(p.at), p.angle, p.uuid.clone())
                }
                _ => (
                    auto.positions.get(refdes).copied().unwrap_or([0.0, 0.0]),
                    initial_angle(comp),
                    None,
                ),
            };
            let extra = ap_properties(block_name, &comp.origin, &rev);
```

- `lib.rs::emit_design` passes `&Relayout::None`. `tools.rs::apply_design` parses the input:

```rust
    let relayout = match input.get("relayout") {
        Some(Value::String(s)) if s == "all" => sch_engine::reconcile::Relayout::All,
        Some(Value::Array(items)) => sch_engine::reconcile::Relayout::Blocks(
            items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        ),
        _ => sch_engine::reconcile::Relayout::None,
    };
```

…threaded into `emit_design_reconciled`, plus the schema/description addition on the `apply_design` ToolDef:

```rust
                        "relayout": {
                            "description": "Discard preserved positions and re-place: \"all\", or a list of block names.",
                            "anyOf": [
                                { "type": "string", "enum": ["all"] },
                                { "type": "array", "items": { "type": "string" } }
                            ]
                        }
```

Also count re-placed components per block while emitting and include `"relayout_blocks": {...}` in the apply_design diff JSON so the apply-gate can show "block `a`: layout changed, N components re-placed" (a `BTreeMap<String, usize>` incremented whenever the `_ =>` placement arm fires for a component that DID have a prior identity match).

- [ ] **Step 4: Run workspace-wide**

Run: `cargo test --workspace`
Expected: PASS (gated-apply / agent tests compile against the new arity via `tools.rs` only — the agent crate never calls `emit_design_reconciled` directly except there).

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine crates/agent/src/tools.rs
git commit -m "feat(sch-engine): ap_layout_rev reconciliation + relayout escape hatch"
```

---

### Task 15: Integration oracle + render harness

**Files:**
- Modify: `crates/sch-engine/tests/bluepill_emit.rs` (zero-collision + ERC oracle)
- Create: `crates/agent/examples/render_validation.rs`

- [ ] **Step 1: Strengthen the bluepill oracle**

In the existing `bluepill_emit.rs` test (after the ERC assertion), add:

```rust
    // Phase 2 readability oracle: the deterministic layout lint must be clean.
    assert!(
        out.layout_warnings.is_empty(),
        "layout collisions on bluepill:\n{}",
        out.layout_warnings.join("\n")
    );
```

Run: `cargo test -p sch-engine --test bluepill_emit`. **If collisions remain, fix the responsible primitive (usually clearance constants in `place.rs` or label estimates in `layout_warnings`) until the oracle passes — this is the acceptance gate for Part B.**

- [ ] **Step 2: Render harness example**

```rust
//! Render every docs/validation schematic to PNG for eyeball regression checks.
//!
//! Usage: cargo run -p agent --example render_validation

use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;

fn main() -> anyhow::Result<()> {
    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let dir = std::path::Path::new("docs/validation");
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("kicad_sch") {
            continue;
        }
        let tmp = tempfile::tempdir()?;
        let svg_path = KicadCli::new(&env).export_svg(&path, tmp.path())?;
        let svg = std::fs::read_to_string(&svg_path)?;
        let png = agent::render::svg_to_png(&svg, 1600)?;
        let out = path.with_extension("png");
        std::fs::write(&out, png)?;
        println!("rendered {}", out.display());
    }
    Ok(())
}
```

Add `docs/validation/*.png` to `.gitignore`.

- [ ] **Step 3: The eyeball check (manual, REQUIRED)**

Regenerate the bluepill schematic from its YAML through the new engine and render it:

```bash
cargo test -p sch-engine --test bluepill_emit   # writes its output; or use the agent CLI
cargo run -p agent --example render_validation
```

Open the PNG(s). Verify against the spec's quality bar: power symbols up/GND down, labels reading away from bodies, decoupling caps in a bank row, block frames with titles, **no overlapping text**, and — specifically — that the Task 9 label angle/justify convention reads correctly (this is the one convention not machine-verified). Fix and iterate before committing.

- [ ] **Step 4: Full workspace test + commit**

Run: `cargo test --workspace`
Expected: PASS.

```bash
git add crates/sch-engine/tests/bluepill_emit.rs crates/agent/examples/render_validation.rs .gitignore
git commit -m "test: bluepill zero-collision oracle + validation render harness"
```

---

## Done criteria (maps to spec)

- Phase 1: `render_schematic` returns a PNG image block the model can see (Task 1–5); dev render harness exists (Task 15).
- Draft workspace: `.autopcb/` with draft + meta + renders + self-gitignore (Task 4); `create_design`/`edit_design`/seeded `get_design`/draft-default `apply_design` with staleness surfacing (Task 6).
- Phase 2: power symbols + Value-rename spike (7, 8), wire stubs + oriented labels (8, 9), passive orientation (10), decoupling banks + bbox cells (11), frames/titles/fields (12), layout lint through `apply_design` (13), `ap_layout_rev` + `relayout` with per-block re-place reporting (14), zero-collision bluepill oracle (15).
- Out of scope here (Phases 3–4 and deferred Phase-1 extras): DSL `sheet:`/`idiom:`/`place:` hints, `title:` surface field, idiom registry, vision critique loop, layout-only auto-approve in the TUI, and `render_schematic`'s per-block `region` crop (spec allows it to land later).
