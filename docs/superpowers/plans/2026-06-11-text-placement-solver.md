# Text & Field Placement Solver Implementation Plan (Aesthetics Slice 1 of 5)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate text collisions in emitted schematics: net labels never sit on pin names, refdes/value fields never sit on neighbors, adjacent power-rail values never merge.

**Architecture:** A pure greedy candidate-placement solver (`textplace.rs`, no I/O — unit-testable without KiCAD) chooses collision-free positions for movable text (stub net labels, refdes/value fields, power-symbol values) against an obstacle set (symbol bodies, **pin name/number text**, wires, fixed labels). Glue in `emit.rs` builds obstacles/movables and applies results inside `finish()`; `reconcile.rs` runs the same pass before linting. The overlap lint is extended to see pin text and fields, keeping the strict fixture oracle honest.

**Spec:** `docs/superpowers/specs/2026-06-11-schematic-aesthetics-design.md` (section 4). Slices 2–5 (router, placement, corpus, polish) get separate plans.

**Tech stack:** Rust, existing `sch-engine` + `kicad-bridge` crates. No new dependencies.

**Test scoping (per project memory):** run `cargo test -p sch-engine` (or narrower) — never the whole workspace. Never run cargo concurrently with a subagent that also runs cargo.

**Context for the engineer — current behavior being fixed:**
- `render_instance` (emit.rs:1115) places Reference/Value at a fixed offset right of the body; rotated symbols and dense neighbors collide (`62R15` artifact in docs/validation/uart-level-translator.png).
- `retract_colliding_stubs` (emit.rs:756) resets a retracted label's `dir` to `Dir::East`; on a west-side IC pin the text then extends east, across the pin line, over the pin name (`N_RST` over `RST` in docs/validation/555-blinker.png).
- Pin name/number text is invisible to the lint (emit.rs:1228) and to placement.
- Power-symbol Value (the rail name) renders at the fixed right-of-body offset; two adjacent rails merge (`VCCDVCC3V3` artifact).
- `layout_warnings_excluding` bboxes symbol bodies angle-blind: a 90°-rotated body keeps its unrotated extents.

---

### Task 1: Pure solver core — `choose()`

**Files:**
- Create: `crates/sch-engine/src/textplace.rs`
- Modify: `crates/sch-engine/src/lib.rs` (add `mod textplace;` next to the other `mod` lines)

- [ ] **Step 1: Write the failing tests**

Create `crates/sch-engine/src/textplace.rs` with module doc, types, an unimplemented `choose`, and tests:

```rust
//! Text placement solver: choose collision-free positions for movable text.
//!
//! Pure geometry — no I/O, no KiCAD environment — so the core is unit-testable
//! and deterministic. `emit.rs` builds [`Obstacle`]s and [`Movable`]s from
//! writer state, calls [`choose`], and applies the returned candidate indices.
//!
//! The solver is greedy: movables are processed in the order given (callers
//! pass a deterministic order — most-constrained first), each takes its first
//! candidate that collides with nothing, and the chosen box becomes an
//! obstacle for everything after it. Greedy is enough here because candidate
//! lists are short and ordered by convention (the first candidate is the
//! KiCAD-conventional spot); a global optimizer would buy little and cost
//! determinism scrutiny.

/// An axis-aligned bbox: `[min_x, min_y, max_x, max_y]` (sheet mm, y down).
pub(crate) type BBox = [f64; 4];

/// Whether two boxes overlap (open intervals: edge-touching is NOT overlap,
/// matching the lint's `boxes_overlap` so solver and oracle agree).
pub(crate) fn boxes_overlap(a: &BBox, b: &BBox) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

/// Fixed geometry a movable must not collide with.
pub(crate) enum ObKind {
    /// A symbol body, exempted for text OWNED by that refdes (a label on its
    /// own pin endpoint legitimately sits inside its symbol's generous bbox).
    OwnExempt(String),
    /// Never exempted: pin text, wires, fixed labels, no-connects.
    Hard,
}

pub(crate) struct Obstacle {
    pub bbox: BBox,
    pub kind: ObKind,
}

/// One piece of movable text with its candidate boxes in preference order.
pub(crate) struct Movable {
    /// Owning refdes, matched against [`ObKind::OwnExempt`].
    pub owner: Option<String>,
    /// Candidate bboxes, best-first. Never empty.
    pub candidates: Vec<BBox>,
}

/// For each movable (in order), the index of the first candidate that collides
/// with no obstacle (minus own-body exemptions) and no previously chosen box.
/// Falls back to candidate 0 when none is free (caller's lint then flags it —
/// visible degradation, per spec).
pub(crate) fn choose(obstacles: &[Obstacle], movables: &[Movable]) -> Vec<usize> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hard(b: BBox) -> Obstacle {
        Obstacle { bbox: b, kind: ObKind::Hard }
    }

    #[test]
    fn picks_first_free_candidate() {
        let obstacles = vec![hard([0.0, 0.0, 10.0, 10.0])];
        let m = Movable {
            owner: None,
            candidates: vec![[5.0, 5.0, 8.0, 8.0], [12.0, 0.0, 15.0, 3.0]],
        };
        assert_eq!(choose(&obstacles, &[m]), vec![1]);
    }

    #[test]
    fn falls_back_to_candidate_zero_when_all_collide() {
        let obstacles = vec![hard([0.0, 0.0, 20.0, 20.0])];
        let m = Movable {
            owner: None,
            candidates: vec![[1.0, 1.0, 2.0, 2.0], [3.0, 3.0, 4.0, 4.0]],
        };
        assert_eq!(choose(&obstacles, &[m]), vec![0]);
    }

    #[test]
    fn own_body_is_exempt_but_hard_is_not() {
        let obstacles = vec![
            Obstacle { bbox: [0.0, 0.0, 10.0, 10.0], kind: ObKind::OwnExempt("R1".into()) },
            hard([0.0, 0.0, 4.0, 4.0]),
        ];
        // Candidate 0 overlaps both; only the body is exempt for R1, so the
        // hard obstacle still rejects it. Candidate 1 overlaps the body only
        // -> exempt -> chosen.
        let m = Movable {
            owner: Some("R1".into()),
            candidates: vec![[1.0, 1.0, 3.0, 3.0], [5.0, 5.0, 9.0, 9.0]],
        };
        assert_eq!(choose(&obstacles, &[m]), vec![1]);
        // A different owner gets no exemption anywhere -> all collide -> 0.
        let m2 = Movable {
            owner: Some("R2".into()),
            candidates: vec![[1.0, 1.0, 3.0, 3.0], [5.0, 5.0, 9.0, 9.0]],
        };
        assert_eq!(choose(&obstacles, &[m2]), vec![0]);
    }

    #[test]
    fn chosen_boxes_block_later_movables() {
        let a = Movable { owner: None, candidates: vec![[0.0, 0.0, 5.0, 5.0]] };
        let b = Movable {
            owner: None,
            candidates: vec![[1.0, 1.0, 4.0, 4.0], [10.0, 10.0, 12.0, 12.0]],
        };
        assert_eq!(choose(&[], &[a, b]), vec![0, 1]);
    }

    #[test]
    fn edge_touching_is_not_collision() {
        let obstacles = vec![hard([0.0, 0.0, 10.0, 10.0])];
        let m = Movable { owner: None, candidates: vec![[10.0, 0.0, 14.0, 4.0]] };
        assert_eq!(choose(&obstacles, &[m]), vec![0]);
    }
}
```

Add to `crates/sch-engine/src/lib.rs`: `mod textplace;`

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test -p sch-engine --lib textplace`
Expected: panics at `todo!()` (5 failures).

- [ ] **Step 3: Implement `choose`**

Replace the `todo!()` body:

```rust
pub(crate) fn choose(obstacles: &[Obstacle], movables: &[Movable]) -> Vec<usize> {
    let mut placed: Vec<BBox> = Vec::new();
    let mut out = Vec::with_capacity(movables.len());
    for m in movables {
        let free = |b: &BBox| {
            obstacles.iter().all(|o| match &o.kind {
                ObKind::OwnExempt(r) if Some(r) == m.owner.as_ref() => true,
                _ => !boxes_overlap(b, &o.bbox),
            }) && placed.iter().all(|p| !boxes_overlap(b, p))
        };
        let idx = m.candidates.iter().position(free).unwrap_or(0);
        placed.push(m.candidates[idx]);
        out.push(idx);
    }
    out
}
```

(Adjust the closure to take `&BBox` matching `position`'s `&&BBox` — use `|b: &&BBox|` or `.position(|c| free(c))`.)

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo test -p sch-engine --lib textplace`
Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src/textplace.rs crates/sch-engine/src/lib.rs
git commit -m "feat(sch-engine): greedy candidate solver core for text placement"
```

---

### Task 2: Text geometry — label boxes, pin-text boxes

**Files:**
- Modify: `crates/sch-engine/src/textplace.rs`
- Modify: `crates/sch-engine/src/emit.rs` (make `transform_offset` reachable — it is already `pub(crate)`, just `use` it)

Shared text metrics: 1.1 mm per char, 1.6 mm tall (the lint's existing model), with a 0.4 mm standoff from the anchor so text anchored ON a wire (KiCAD draws label text floating above its anchor point) does not count as colliding with that wire.

- [ ] **Step 1: Write the failing tests**

Append to `textplace.rs`:

```rust
use kicad_bridge::geometry::PinGeom;

use crate::emit::Dir;
use crate::emit::transform_offset;

/// Estimated width of rendered text (mm): 1.1 mm/char at the 1.27 font.
pub(crate) fn text_width(s: &str) -> f64 {
    s.chars().count() as f64 * 1.1
}

/// Bbox of a net label anchored at `at` reading along `dir`.
///
/// KiCAD renders label text floating 0.4 mm off the anchor on the side away
/// from the wire, so the box is offset by 0.4 mm perpendicular to the reading
/// direction — a label sitting ON its own wire does not collide with it.
pub(crate) fn label_box(at: [f64; 2], dir: Dir, width: f64) -> BBox {
    todo!()
}

/// Bboxes (sheet space) of a pin's rendered NAME and NUMBER text for a placed
/// instance. Empty for unnamed pins (`"~"`) — only the number box is returned
/// then. The name starts just past the pin's body end and extends INTO the
/// body along the pin direction; the number straddles the pin line midpoint.
pub(crate) fn pin_text_boxes(
    pin: &PinGeom,
    inst_at: [f64; 2],
    inst_angle: f64,
    inst_mirror: bool,
) -> Vec<BBox> {
    todo!()
}

/// Sheet bbox from two transformed corner points (normalizes min/max).
pub(crate) fn rect_from_corners(a: [f64; 2], b: [f64; 2]) -> BBox {
    [a[0].min(b[0]), a[1].min(b[1]), a[0].max(b[0]), a[1].max(b[1])]
}

/// Instance half-extents with the body rotation applied: 90/270 swaps w/h.
pub(crate) fn rotated_half_extents(h: [f64; 2], angle: f64) -> [f64; 2] {
    todo!()
}

/// Thin obstacle box around a wire segment (inflated 0.13 mm).
pub(crate) fn wire_box(a: [f64; 2], b: [f64; 2]) -> BBox {
    [
        a[0].min(b[0]) - 0.13,
        a[1].min(b[1]) - 0.13,
        a[0].max(b[0]) + 0.13,
        a[1].max(b[1]) + 0.13,
    ]
}
```

And tests (inside `mod tests`):

```rust
    fn bbox_close(got: BBox, want: BBox) {
        for i in 0..4 {
            assert!((got[i] - want[i]).abs() < 1e-6, "got {got:?}, want {want:?}");
        }
    }

    #[test]
    fn label_box_per_direction() {
        // East: text extends +x from the anchor, floats 0.4 above (-y).
        bbox_close(label_box([10.0, 20.0], Dir::East, 5.5), [10.0, 17.6, 15.5, 19.6]);
        // West: extends -x.
        bbox_close(label_box([10.0, 20.0], Dir::West, 5.5), [4.5, 17.6, 10.0, 19.6]);
        // North: vertical text extending -y, floating 0.4 to the -x side.
        bbox_close(label_box([10.0, 20.0], Dir::North, 5.5), [8.0, 14.5, 9.6, 20.0]);
        // South: extending +y, floating to the +x side.
        bbox_close(label_box([10.0, 20.0], Dir::South, 5.5), [10.4, 20.0, 12.0, 25.5]);
    }

    #[test]
    fn pin_name_box_extends_into_body() {
        // A west-side IC pin: connection at local (-10.16, 2.54), angle 0
        // (pointing east INTO the body), length 2.54, name "RST" (3 chars).
        // Body end (local) = (-10.16 + 2.54, 2.54) = (-7.62, 2.54).
        // Name spans local x in [-7.112, -3.812] (0.508 offset + 3.3 width),
        // local y in [1.74, 3.34] (±0.8). Sheet (inst at (100,100), angle 0,
        // y-flip): x in [92.888, 96.188], y in [96.66, 98.26].
        let pin = PinGeom {
            number: "4".into(),
            name: "RST".into(),
            at: [-10.16, 2.54],
            angle: 0.0,
            length: 2.54,
            unit: 1,
        };
        let boxes = pin_text_boxes(&pin, [100.0, 100.0], 0.0, false);
        assert_eq!(boxes.len(), 2, "named pin -> name box + number box");
        bbox_close(boxes[0], [92.888, 96.66, 96.188, 98.26]);
        // Number box straddles the pin line midpoint, local (-8.89, 2.54):
        // sheet (91.11, 97.46) ± (1.1, 0.8).
        bbox_close(boxes[1], [90.01, 96.66, 92.21, 98.26]);
    }

    #[test]
    fn unnamed_pin_has_only_number_box() {
        let pin = PinGeom {
            number: "1".into(),
            name: "~".into(),
            at: [0.0, 3.81],
            angle: 270.0,
            length: 1.27,
            unit: 1,
        };
        let boxes = pin_text_boxes(&pin, [50.0, 50.0], 0.0, false);
        assert_eq!(boxes.len(), 1);
    }

    #[test]
    fn rotated_half_extents_swaps_at_90() {
        assert_eq!(rotated_half_extents([3.0, 1.0], 0.0), [3.0, 1.0]);
        assert_eq!(rotated_half_extents([3.0, 1.0], 90.0), [1.0, 3.0]);
        assert_eq!(rotated_half_extents([3.0, 1.0], 180.0), [3.0, 1.0]);
        assert_eq!(rotated_half_extents([3.0, 1.0], 270.0), [1.0, 3.0]);
    }
```

- [ ] **Step 2: Run tests, verify the new ones fail**

Run: `cargo test -p sch-engine --lib textplace`
Expected: Task-1 tests pass; new tests panic at `todo!()`.

- [ ] **Step 3: Implement**

```rust
pub(crate) fn label_box(at: [f64; 2], dir: Dir, width: f64) -> BBox {
    const H: f64 = 1.6; // text height
    const OFF: f64 = 0.4; // standoff from the anchor/wire
    let [x, y] = at;
    match dir {
        Dir::East => [x, y - OFF - H, x + width, y - OFF],
        Dir::West => [x - width, y - OFF - H, x, y - OFF],
        Dir::North => [x - OFF - H, y - width, x - OFF, y],
        Dir::South => [x + OFF, y, x + OFF + H, y + width],
    }
}

pub(crate) fn pin_text_boxes(
    pin: &PinGeom,
    inst_at: [f64; 2],
    inst_angle: f64,
    inst_mirror: bool,
) -> Vec<BBox> {
    const NAME_OFFSET: f64 = 0.508; // KiCAD default pin-name offset
    let theta = pin.angle.to_radians();
    let u = [theta.cos(), theta.sin()]; // local: from tip INTO the body
    let p = [-u[1], u[0]]; // perpendicular
    let to_sheet = |local: [f64; 2]| {
        let off = transform_offset(local, inst_angle, inst_mirror);
        [inst_at[0] + off[0], inst_at[1] + off[1]]
    };
    let mut boxes = Vec::new();
    if pin.name != "~" {
        let w = text_width(&pin.name);
        let start = [
            pin.at[0] + (pin.length + NAME_OFFSET) * u[0],
            pin.at[1] + (pin.length + NAME_OFFSET) * u[1],
        ];
        let end = [start[0] + w * u[0], start[1] + w * u[1]];
        let c1 = [start[0] - 0.8 * p[0], start[1] - 0.8 * p[1]];
        let c2 = [end[0] + 0.8 * p[0], end[1] + 0.8 * p[1]];
        boxes.push(rect_from_corners(to_sheet(c1), to_sheet(c2)));
    }
    // Number text straddles the pin line midpoint.
    let mid = [
        pin.at[0] + 0.5 * pin.length * u[0],
        pin.at[1] + 0.5 * pin.length * u[1],
    ];
    let c1 = [mid[0] - 1.1 * u[0] - 0.8 * p[0], mid[1] - 1.1 * u[1] - 0.8 * p[1]];
    let c2 = [mid[0] + 1.1 * u[0] + 0.8 * p[0], mid[1] + 1.1 * u[1] + 0.8 * p[1]];
    boxes.push(rect_from_corners(to_sheet(c1), to_sheet(c2)));
    boxes
}

pub(crate) fn rotated_half_extents(h: [f64; 2], angle: f64) -> [f64; 2] {
    let a = angle.rem_euclid(360.0);
    if (a - 90.0).abs() < 1e-9 || (a - 270.0).abs() < 1e-9 {
        [h[1], h[0]]
    } else {
        h
    }
}
```

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo test -p sch-engine --lib textplace`
Expected: all pass. If the `pin_name_box_extends_into_body` expectations are off
by transform details, re-derive by hand from `transform_offset` (mirror →
rotate → y-flip) — fix the EXPECTATION only if your hand derivation agrees
with the code's transform semantics (emit.rs:967 and its doc comment).

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src/textplace.rs
git commit -m "feat(sch-engine): text/pin-text bbox geometry for placement solver"
```

---

### Task 3: Writer plumbing — solved field positions, pin cache

**Files:**
- Modify: `crates/sch-engine/src/emit.rs`
- Test: `crates/sch-engine/tests/textplace_emit.rs` (create)

- [ ] **Step 1: Write the failing test**

Create `crates/sch-engine/tests/textplace_emit.rs`:

```rust
//! Integration tests for solved text positions in emitted documents.

use kicad_bridge::env::KicadEnv;
use sch_engine::SchematicWriter;

fn detect_env() -> Option<KicadEnv> {
    match KicadEnv::detect() {
        Some(env) => Some(env),
        None => {
            eprintln!("SKIP: no KiCAD environment detected");
            None
        }
    }
}

/// Two resistors placed so close horizontally that R1's legacy right-of-body
/// fields would sit inside R2's body. The solver must move R1's fields
/// elsewhere (any non-right candidate), so the legacy Reference position
/// must NOT appear in the output.
#[test]
fn fields_dodge_neighbor_body() {
    let Some(env) = detect_env() else { return };
    let mut w = SchematicWriter::new();
    // Device:R approx size [10.16, 12.7] -> half extents [5.08, 6.35].
    // Legacy ref position for R1 at (100, 100): x = 100+5.08+1.27 = 106.35.
    // R2 at (110, 100): body spans x in [104.92, 115.08] -> covers 106.35.
    w.add_symbol(&env, "Device:R", "R1", "1k", [100.0, 100.0], 0.0).unwrap();
    w.add_symbol(&env, "Device:R", "R2", "2k", [110.0, 100.0], 0.0).unwrap();
    let sch = w.finish();
    let r1_prop = sch
        .split("(property \"Reference\" \"R1\"")
        .nth(1)
        .expect("R1 Reference property present");
    assert!(
        !r1_prop.trim_start().starts_with("(at 106.35"),
        "R1 Reference must move off the legacy right-of-body spot:\n{sch}"
    );
}

/// With no neighbors, the first (conventional) candidate is chosen and the
/// output keeps the legacy right-of-body field placement byte-for-byte.
#[test]
fn lone_symbol_keeps_conventional_fields() {
    let Some(env) = detect_env() else { return };
    let mut w = SchematicWriter::new();
    w.add_symbol(&env, "Device:R", "R1", "1k", [100.0, 100.0], 0.0).unwrap();
    let sch = w.finish();
    assert!(
        sch.contains("(property \"Reference\" \"R1\"\n\t\t\t(at 106.35 98.73 0)"),
        "lone symbol keeps conventional field spot:\n{sch}"
    );
}
```

Note the conventional Reference y: instances render `ref_y = y - 1.27` →
`98.73`. If the existing legacy output differs, read the actual emitted
output once and pin the test to the TRUE legacy values before proceeding.

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo test -p sch-engine --test textplace_emit`
Expected: `fields_dodge_neighbor_body` FAILS (fields don't move yet);
`lone_symbol_keeps_conventional_fields` PASSES (documents the baseline).

- [ ] **Step 3: Add plumbing to emit.rs**

3a. New types near `Instance`:

```rust
/// Horizontal text justification for a solved field position. `Center` is
/// rendered by omitting the justify token (KiCAD's default is centered).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Justify {
    Left,
    Right,
    Center,
}

/// A solved field text anchor.
#[derive(Clone, Copy)]
pub(crate) struct TextPos {
    pub at: [f64; 2],
    pub justify: Justify,
}
```

3b. `Instance` gains two fields (and `Default`-free construction sites updated):

```rust
    /// Solver-assigned Reference/Value positions. `None` -> legacy fixed
    /// right-of-body offsets (kept for hidden fields and as fallback).
    ref_pos: Option<TextPos>,
    val_pos: Option<TextPos>,
```

Set `ref_pos: None, val_pos: None` in `add_symbol_full`'s `Instance { … }`.

3c. Cache pin geometry per lib_id. Add to `SchematicWriter`:

```rust
    /// Pin geometry per lib_id, cached at first load, for pin-text obstacles.
    sym_pins: BTreeMap<String, Vec<kicad_bridge::geometry::PinGeom>>,
```

In `add_symbol_full`'s `if !self.lib_symbols.contains_key(lib_id)` branch,
before moving `raw_definition`:

```rust
            self.sym_pins.insert(lib_id.to_string(), geom.pins.clone());
```

3d. `render_instance` honors solved positions. Replace the fixed
`ref_x/ref_y/val_x/val_y` block with:

```rust
    let (ref_at, ref_j) = match inst.ref_pos {
        Some(p) => (p.at, p.justify),
        None => (
            [x + inst.half_extents[0] + 1.27, y - 1.27],
            Justify::Left,
        ),
    };
    let (val_at, val_j) = match inst.val_pos {
        Some(p) => (p.at, p.justify),
        None => (
            [x + inst.half_extents[0] + 1.27, y + 1.27],
            Justify::Left,
        ),
    };
```

and render with a helper:

```rust
fn justify_token(j: Justify) -> &'static str {
    match j {
        Justify::Left => " (justify left)",
        Justify::Right => " (justify right)",
        Justify::Center => "",
    }
}
```

so the property effects lines become (preserving the hide variants):

```rust
    let _ = writeln!(s, "\t\t(property \"Reference\" \"{refdes}\"");
    let _ = writeln!(s, "\t\t\t(at {} {} 0)", fmt_coord(ref_at[0]), fmt_coord(ref_at[1]));
    if hide_ref {
        let _ = writeln!(s, "\t\t\t(effects (font (size 1.27 1.27)){} (hide yes))", justify_token(ref_j));
    } else {
        let _ = writeln!(s, "\t\t\t(effects (font (size 1.27 1.27)){})", justify_token(ref_j));
    }
    s.push_str("\t\t)\n");
```

(and the symmetric Value block with `val_at`/`val_j`).

3e. Minimal solver glue so the new test passes — add to `SchematicWriter`
(full obstacle model lands in Task 5; this step wires fields only):

```rust
    /// Assign collision-free Reference/Value positions (see textplace.rs).
    /// Idempotent: recomputes every assignment from scratch each call.
    pub(crate) fn solve_text_positions(&mut self) {
        use crate::textplace::{
            choose, label_box, rotated_half_extents, text_width, wire_box, BBox, Movable,
            ObKind, Obstacle,
        };
        let mut obstacles: Vec<Obstacle> = Vec::new();
        for inst in &self.instances {
            let h = rotated_half_extents(inst.half_extents, inst.angle);
            obstacles.push(Obstacle {
                bbox: [
                    inst.at[0] - h[0],
                    inst.at[1] - h[1],
                    inst.at[0] + h[0],
                    inst.at[1] + h[1],
                ],
                kind: ObKind::OwnExempt(inst.refdes.clone()),
            });
        }
        for w in &self.wires {
            obstacles.push(Obstacle { bbox: wire_box(w.a, w.b), kind: ObKind::Hard });
        }
        for l in &self.labels {
            obstacles.push(Obstacle {
                bbox: label_box(l.at, l.dir, text_width(&l.net)),
                kind: ObKind::Hard,
            });
        }

        // Field movables: one per visible-field instance, deterministic refdes
        // order. Each candidate is the UNION box of the Reference+Value pair;
        // the per-candidate anchor pair is kept in a parallel vec for apply.
        let mut order: Vec<usize> = (0..self.instances.len())
            .filter(|&i| !self.instances[i].refdes.starts_with('#'))
            .collect();
        order.sort_by(|&a, &b| self.instances[a].refdes.cmp(&self.instances[b].refdes));

        let mut movables: Vec<Movable> = Vec::new();
        let mut apply: Vec<(usize, Vec<(TextPos, TextPos)>)> = Vec::new();
        for &i in &order {
            let inst = &self.instances[i];
            let h = rotated_half_extents(inst.half_extents, inst.angle);
            let (cx, cy) = (inst.at[0], inst.at[1]);
            let (minx, miny, maxx, maxy) = (cx - h[0], cy - h[1], cx + h[0], cy + h[1]);
            let rw = text_width(&inst.refdes);
            let vw = text_width(&inst.value);
            let wmax = rw.max(vw);
            // Each candidate: (ref anchor, val anchor, union bbox). Text is
            // bottom-anchored, 1.6 tall, with the 0.4 standoff baked into the
            // line offsets (not the box) for fields.
            let right = (
                TextPos { at: [maxx + 1.27, cy - 1.27], justify: Justify::Left },
                TextPos { at: [maxx + 1.27, cy + 1.27], justify: Justify::Left },
                [maxx + 1.27, cy - 2.87, maxx + 1.27 + wmax, cy + 1.27] as BBox,
            );
            let left = (
                TextPos { at: [minx - 1.27, cy - 1.27], justify: Justify::Right },
                TextPos { at: [minx - 1.27, cy + 1.27], justify: Justify::Right },
                [minx - 1.27 - wmax, cy - 2.87, minx - 1.27, cy + 1.27],
            );
            let above = (
                TextPos { at: [cx, miny - 3.18], justify: Justify::Center },
                TextPos { at: [cx, miny - 0.64], justify: Justify::Center },
                [cx - wmax / 2.0, miny - 4.78, cx + wmax / 2.0, miny - 0.64],
            );
            let below = (
                TextPos { at: [cx, maxy + 2.24], justify: Justify::Center },
                TextPos { at: [cx, maxy + 4.78], justify: Justify::Center },
                [cx - wmax / 2.0, maxy + 0.64, cx + wmax / 2.0, maxy + 4.78],
            );
            // Wide bodies (rotated passives) prefer above/below; tall prefer
            // right/left (the KiCAD convention).
            let cands = if h[0] > h[1] {
                vec![above, below, right, left]
            } else {
                vec![right, left, above, below]
            };
            movables.push(Movable {
                owner: Some(inst.refdes.clone()),
                candidates: cands.iter().map(|c| c.2).collect(),
            });
            apply.push((i, cands.into_iter().map(|c| (c.0, c.1)).collect()));
        }

        let picks = choose(&obstacles, &movables);
        for ((i, cands), pick) in apply.into_iter().zip(picks) {
            let (r, v) = cands[pick];
            self.instances[i].ref_pos = Some(r);
            self.instances[i].val_pos = Some(v);
        }
    }
```

Call it in `finish()` right after retraction:

```rust
        self.retract_colliding_stubs();
        self.solve_text_positions();
```

NOTE: the `right` candidate for an UNROTATED symbol must equal the legacy
position so `lone_symbol_keeps_conventional_fields` stays green: legacy is
`x + half_extents[0] + 1.27` which equals `maxx + 1.27`. Verify and adjust
the test's expected coordinates from the actual Device:R extents if needed.

- [ ] **Step 4: Run tests**

Run: `cargo test -p sch-engine --test textplace_emit`
Expected: both pass.

Run: `cargo test -p sch-engine`
Expected: pre-existing tests pass EXCEPT any that assert exact legacy field
bytes for symbols whose fields the solver legitimately moved, or that assert
`(justify left)` on centered fields. Inspect each failure: if the new output
is correct per this design, update the test's expectation; if the new output
is wrong, fix the code. Do not blanket-update.

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src/emit.rs crates/sch-engine/tests/textplace_emit.rs
git commit -m "feat(sch-engine): solver-assigned refdes/value field positions"
```

---

### Task 4: Retraction keeps the outward direction

**Files:**
- Modify: `crates/sch-engine/src/emit.rs:754-758` (`retract_colliding_stubs`) and the stale comment at `crates/sch-engine/src/reconcile.rs:998-1005`

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `emit.rs`:

```rust
    #[test]
    fn retracted_label_keeps_outward_direction() {
        // Device:R pin 1 at angle 0 points North; a foreign wire across the
        // stub end forces retraction. The label must land on the pin endpoint
        // KEEPING dir North (angle 90 in the rendered label) so the text still
        // reads away from the body — not reset to East across the pin line.
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0).unwrap();
        w.add_signal_label(&env, "R1", "1", "SIG").unwrap();
        // Foreign wire through the stub end (127.0, 55.88).
        w.add_wire_on_net([121.92, 55.88], [132.08, 55.88], "OTHER");
        let sch = w.finish();
        assert!(
            sch.contains("(label \"SIG\"\n\t\t(at 127 59.69 90)"),
            "retracted label keeps its North orientation:\n{sch}"
        );
    }
```

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo test -p sch-engine --lib retracted_label_keeps_outward_direction`
Expected: FAIL — current code resets dir to East, rendering `(at 127 59.69 0)`.

- [ ] **Step 3: Fix**

In `retract_colliding_stubs`, delete the line `self.labels[i].dir = Dir::East;`
and update the method doc (the “orientation reset to `East`” sentence) to say
the label keeps its outward direction so text reads away from the body.
Update the reconcile.rs:998 comment’s “dir East” mention likewise.

- [ ] **Step 4: Run tests**

Run: `cargo test -p sch-engine`
Expected: the new test passes. `same_net_wire_touch_survives_foreign_retracts`
asserts positions only (not angles) and must still pass. Update any test
asserting the old `0` angle on retracted labels after verifying the new
orientation is the correct one.

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src/emit.rs crates/sch-engine/src/reconcile.rs
git commit -m "fix(sch-engine): retracted stub labels keep outward orientation"
```

---

### Task 5: Full obstacle model + movable stub labels

**Files:**
- Modify: `crates/sch-engine/src/emit.rs` (`solve_text_positions`)
- Test: `crates/sch-engine/tests/textplace_emit.rs`

- [ ] **Step 1: Write the failing test**

Append to `textplace_emit.rs`:

```rust
/// A 555's west-side pins carry signal labels. With pin-name text as an
/// obstacle, a label that would overlap the pin names must move to a free
/// candidate, and NO label box may intersect a pin-name box in the result.
/// We assert via the lint (extended in a later task) being collision-free
/// for label-vs-pin overlaps is not yet lintable here, so assert the
/// concrete fix instead: the stub-end label survives at the stub end (its
/// first candidate), proving the solver does not force-retract everything
/// (regression guard for over-aggressive obstacle modeling).
#[test]
fn timer_pin_labels_stay_on_stub_ends() {
    let Some(env) = detect_env() else { return };
    let mut w = SchematicWriter::new();
    w.add_symbol(&env, "Timer:NE555P", "U1", "NE555P", [150.0, 100.0], 0.0).unwrap();
    w.add_signal_label(&env, "U1", "TR", "N_TR").unwrap();
    let sch = w.finish();
    // The TRIG pin is on the west side; its stub extends west 3.81mm and the
    // label must stay there (dir West renders angle 180).
    assert!(
        sch.contains("(label \"N_TR\"") && sch.contains(" 180)"),
        "west-side stub label survives, reading west:\n{sch}"
    );
}

/// Two power symbols close together: their Value texts (rail names) must not
/// land in overlapping boxes (the VCCDVCC3V3 artifact). Values are solver-
/// moved fields, so assert the rendered positions differ in more than x-epsilon.
#[test]
fn adjacent_power_rail_values_do_not_merge() {
    let Some(env) = detect_env() else { return };
    let mut w = SchematicWriter::new();
    w.add_power_symbol(&env, "power:VCC", "#PWR01", "VCCD", [100.0, 100.0], 0.0).unwrap();
    w.add_power_symbol(&env, "power:VCC", "#PWR02", "VCC3V3", [105.08, 100.0], 0.0).unwrap();
    let sch = w.finish();
    let pos = |val: &str| -> (f64, f64) {
        let seg = sch.split(&format!("(property \"Value\" \"{val}\"")).nth(1).unwrap();
        let at = seg.split("(at ").nth(1).unwrap();
        let mut it = at.split_whitespace();
        (
            it.next().unwrap().parse().unwrap(),
            it.next().unwrap().parse().unwrap(),
        )
    };
    let (x1, y1) = pos("VCCD");
    let (x2, y2) = pos("VCC3V3");
    // Boxes: width 1.1/char, height 1.6. Disjoint if x-ranges or y-ranges are.
    let w1 = 4.0 * 1.1;
    let w2 = 6.0 * 1.1;
    // Conservative center-justified ranges (Center is what the solver picks
    // for power values; adjust if a side candidate was chosen — the assert
    // below only needs SOME separation).
    let overlap_x = (x1 - w1 / 2.0) < (x2 + w2 / 2.0) && (x2 - w2 / 2.0) < (x1 + w1 / 2.0);
    let overlap_y = (y1 - 1.6) < y2 && (y2 - 1.6) < y1;
    assert!(
        !(overlap_x && overlap_y),
        "power rail values must not overlap: {val1:?} at {p1:?}, {val2:?} at {p2:?}",
        val1 = "VCCD", p1 = (x1, y1), val2 = "VCC3V3", p2 = (x2, y2),
    );
}
```

- [ ] **Step 2: Run, verify the power test fails**

Run: `cargo test -p sch-engine --test textplace_emit`
Expected: `adjacent_power_rail_values_do_not_merge` FAILS (both values render
at the fixed right-of-body offset of two symbols 5.08 apart → overlapping).
`timer_pin_labels_stay_on_stub_ends` may already pass — keep it as a
regression guard.

- [ ] **Step 3: Extend `solve_text_positions`**

3a. **Pin-text obstacles** — in the instance loop, after pushing the body box
(skip `#`-prefixed symbols: power graphics have no meaningful pin text):

```rust
            if !inst.refdes.starts_with('#') {
                if let Some(pins) = self.sym_pins.get(&inst.lib_id) {
                    for pg in pins {
                        for b in crate::textplace::pin_text_boxes(pg, inst.at, inst.angle, inst.mirror) {
                            obstacles.push(Obstacle { bbox: b, kind: ObKind::Hard });
                        }
                    }
                }
            }
```

3b. **No-connect obstacles**:

```rust
        for nc in &self.no_connects {
            obstacles.push(Obstacle {
                bbox: [nc.at[0] - 0.64, nc.at[1] - 0.64, nc.at[0] + 0.64, nc.at[1] + 0.64],
                kind: ObKind::Hard,
            });
        }
```

3c. **Split labels into movable (stub) and fixed (no stub)** — fixed ones stay
obstacles as in Task 3; stub labels become movables placed BEFORE fields
(most constrained first). Candidates: keep at stub end, or retract to the pin
endpoint (keeping outward dir). When the retract candidate wins, drop the
stub wire that `retract_colliding_stubs` already materialized:

```rust
        // Fixed labels are obstacles; stub labels are movables.
        for l in &self.labels {
            if l.stub.is_none() {
                obstacles.push(Obstacle {
                    bbox: label_box(l.at, l.dir, text_width(&l.net)),
                    kind: ObKind::Hard,
                });
            }
        }
        let mut stub_idx: Vec<usize> = (0..self.labels.len())
            .filter(|&i| self.labels[i].stub.is_some())
            .collect();
        stub_idx.sort_by(|&a, &b| self.labels[a].uuid_key.cmp(&self.labels[b].uuid_key));
        for &i in &stub_idx {
            let l = &self.labels[i];
            let wdt = text_width(&l.net);
            let owner = l.uuid_key.split(':').next().unwrap_or("").to_string();
            movables.push(Movable {
                owner: Some(owner),
                candidates: vec![
                    label_box(l.at, l.dir, wdt),
                    label_box(l.stub.unwrap().pin_at, l.dir, wdt),
                ],
            });
        }
```

The label's own stub wire is one of the `wires` obstacles; with the 0.4 mm
standoff in `label_box` the stub-end candidate does NOT collide with its own
wire (the box floats above the line). Verify with a temporary dbg if the
stub-end candidate unexpectedly loses.

3d. **Apply label picks, then field picks.** Movables order is now
`[stub labels..., fields...]`; split `picks` accordingly:

```rust
        let picks = choose(&obstacles, &movables);
        let (label_picks, field_picks) = picks.split_at(stub_idx.len());
        for (&i, &pick) in stub_idx.iter().zip(label_picks) {
            if pick == 1 {
                let pin_at = self.labels[i].stub.unwrap().pin_at;
                let end = self.labels[i].at;
                // Drop the stub wire retract_colliding_stubs materialized.
                let a = crate::grid::snap_point(pin_at);
                let b = crate::grid::snap_point(end);
                let key = format!("{}:{}:{}:{}", a[0], a[1], b[0], b[1]);
                self.wires.retain(|w| w.uuid_key != key);
                self.labels[i].at = pin_at;
                self.labels[i].stub = None;
            }
        }
        for ((i, cands), &pick) in apply.into_iter().zip(field_picks) {
            let (r, v) = cands[pick];
            self.instances[i].ref_pos = Some(r);
            self.instances[i].val_pos = Some(v);
        }
```

(Field movables must therefore be pushed AFTER the stub-label movables; move
the field-movable loop below the label loop, keeping `apply` parallel to the
field portion only.)

3e. **Power-symbol Value movables.** Power symbols (refdes `#…`, value
visible — i.e. not `power:PWR_FLAG`) get a Value-only movable, candidates
ordered by the symbol's angle (rail direction): angle 0 (pointing up) →
above, right, left; angle 180 (GND, pointing down) → below, right, left.
Insert in the same field loop (replacing the current skip of `#` refdes —
now only PWR_FLAG and hidden-value instances are skipped entirely):

```rust
            if inst.refdes.starts_with('#') {
                if inst.lib_id == "power:PWR_FLAG" {
                    continue;
                }
                let vw = text_width(&inst.value);
                let above = (
                    TextPos { at: [cx, miny - 0.64], justify: Justify::Center },
                    [cx - vw / 2.0, miny - 2.24, cx + vw / 2.0, miny - 0.64] as BBox,
                );
                let below = (
                    TextPos { at: [cx, maxy + 2.24], justify: Justify::Center },
                    [cx - vw / 2.0, maxy + 0.64, cx + vw / 2.0, maxy + 2.24],
                );
                let right = (
                    TextPos { at: [maxx + 0.64, cy + 0.8], justify: Justify::Left },
                    [maxx + 0.64, cy - 0.8, maxx + 0.64 + vw, cy + 0.8],
                );
                let left = (
                    TextPos { at: [minx - 0.64, cy + 0.8], justify: Justify::Right },
                    [minx - 0.64 - vw, cy - 0.8, minx - 0.64, cy + 0.8],
                );
                let cands = if inst.angle == 180.0 {
                    vec![below, right, left]
                } else {
                    vec![above, right, left]
                };
                movables.push(Movable {
                    owner: Some(inst.refdes.clone()),
                    candidates: cands.iter().map(|c| c.1).collect(),
                });
                power_apply.push((i, cands.into_iter().map(|c| c.0).collect::<Vec<_>>()));
                continue;
            }
```

with `power_apply: Vec<(usize, Vec<TextPos>)>` applied like fields but setting
ONLY `val_pos` (leave `ref_pos` None — Reference is hidden for `#` refdes).
Keep the movable/apply bookkeeping straight: order is
`[stub labels..., field+power movables interleaved in refdes order...]`; the
simplest correct structure is one `enum Apply { Fields(usize, Vec<(TextPos, TextPos)>), PowerVal(usize, Vec<TextPos>) }`
vec parallel to the non-label movables.

- [ ] **Step 4: Run tests**

Run: `cargo test -p sch-engine --test textplace_emit`
Expected: all pass.
Run: `cargo test -p sch-engine`
Expected: green, modulo deliberate expectation updates (same triage rule as
Task 3 Step 4).

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src/emit.rs crates/sch-engine/tests/textplace_emit.rs
git commit -m "feat(sch-engine): full text solver - pin-text obstacles, movable stub labels, power values"
```

---

### Task 6: Run the solver in the reconcile pipeline

**Files:**
- Modify: `crates/sch-engine/src/reconcile.rs:1006` (after `w.retract_colliding_stubs();`)

- [ ] **Step 1: Add the call**

```rust
    w.retract_colliding_stubs();
    // Solve text positions BEFORE linting so the lint sees the exact geometry
    // `finish` will emit (which runs both passes again — both idempotent).
    w.solve_text_positions();
```

Extend the comment block above (reconcile.rs:998) to mention the solver.

- [ ] **Step 2: Verify idempotence is real, not assumed**

Add to `emit.rs` tests:

```rust
    #[test]
    fn solver_is_idempotent_across_finish() {
        let Some(env) = detect_env() else { return };
        let build = |presolve: bool| {
            let mut w = SchematicWriter::new();
            w.add_symbol(&env, "Device:R", "R1", "1k", [100.0, 100.0], 0.0).unwrap();
            w.add_symbol(&env, "Device:R", "R2", "2k", [110.0, 100.0], 0.0).unwrap();
            w.add_signal_label(&env, "R1", "1", "SIG").unwrap();
            if presolve {
                w.retract_colliding_stubs();
                w.solve_text_positions();
            }
            w.finish()
        };
        assert_eq!(build(false), build(true), "pre-solving must not change output");
    }
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p sch-engine`
Expected: green.

- [ ] **Step 4: Commit**

```bash
git add crates/sch-engine/src/emit.rs crates/sch-engine/src/reconcile.rs
git commit -m "feat(sch-engine): solve text positions in reconcile pipeline before lint"
```

---

### Task 7: Lint sees pin text and fields (oracle tightening)

**Files:**
- Modify: `crates/sch-engine/src/emit.rs` (`layout_warnings_excluding`)

- [ ] **Step 1: Write the failing test**

Append to `emit.rs` tests:

```rust
    #[test]
    fn lint_flags_text_on_pin_names() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Timer:NE555P", "U1", "NE555P", [150.0, 100.0], 0.0).unwrap();
        // A legacy (fixed, dir East) label parked straight on the west-side
        // pin names: must be flagged against U1's pin text.
        w.add_cluster_label("X", [145.0, 97.0], Dir::East);
        let warnings = w.layout_warnings();
        assert!(
            warnings.iter().any(|s| s.contains("pin text") && s.contains("U1")),
            "expected a pin-text overlap warning, got {warnings:?}"
        );
    }

    #[test]
    fn lint_uses_rotated_body_extents() {
        let Some(env) = detect_env() else { return };
        // Two 90-degree resistors stacked vertically 5.08 apart: with angle-
        // blind extents (tall box) they appear overlapping; with rotated
        // extents (flat box, half-height 5.08 -> 2.54-ish) they are clear.
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [100.0, 100.0], 90.0).unwrap();
        w.add_symbol(&env, "Device:R", "R2", "2k", [100.0, 110.16], 90.0).unwrap();
        let warnings = w.layout_warnings();
        assert!(
            !warnings.iter().any(|s| s.contains("symbol R1 overlaps symbol R2")),
            "rotated bodies must use rotated extents, got {warnings:?}"
        );
    }
```

(Verify the chosen coordinates with the real Device:R extents at step 3;
adjust the stacking distance so the unrotated boxes overlap but rotated ones
don't.)

- [ ] **Step 2: Run, verify both fail**

Run: `cargo test -p sch-engine --lib lint_`
Expected: FAIL (no pin-text items; angle-blind extents).

- [ ] **Step 3: Implement in `layout_warnings_excluding`**

- Body items: replace `let h = inst.half_extents;` with
  `let h = crate::textplace::rotated_half_extents(inst.half_extents, inst.angle);`
- Label items: replace the inline `match label.dir` box with
  `crate::textplace::label_box(label.at, label.dir, label.net.chars().count() as f64 * 1.1)`
  (single box model shared by solver and lint — they must agree or the solver
  fixes things the lint can't see and vice versa).
- New items per instance (non-`#`): pin text boxes, owned by the refdes so
  own-body pairs stay exempt but FOREIGN text on pin names flags:

```rust
        for inst in &self.instances {
            if inst.refdes.starts_with('#') {
                continue;
            }
            if let Some(pins) = self.sym_pins.get(&inst.lib_id) {
                for pg in pins {
                    for b in crate::textplace::pin_text_boxes(pg, inst.at, inst.angle, inst.mirror) {
                        items.push((format!("pin text of {}", inst.refdes), b, inst.refdes.clone()));
                    }
                }
            }
        }
```

  CAVEAT — own-label-vs-own-pin-text: both carry the same owner refdes, so
  the existing `items[i].2 == items[j].2` exemption hides exactly the 555
  bug class. Refine the exemption: a pair is exempt only when one side is the
  SYMBOL BODY item (`starts_with("symbol ")`) — label-vs-own-body stays
  exempt, label-vs-own-pin-text flags. Implement by tagging items:

```rust
        // items: (description, bbox, owner, is_body)
```

  and the skip becomes `if items[i].2 == items[j].2 && (items[i].3 || items[j].3) { continue; }`.

- Field items (visible Reference/Value at their solved-or-legacy positions),
  owned by the refdes, `is_body = false`. Compute the rendered box from the
  same `(ref_at, ref_j)` resolution as `render_instance` — extract that
  resolution into a small `pub(crate) fn field_anchors(inst: &Instance) -> ((​[f64; 2], Justify), ([f64; 2], Justify))`
  used by both, then box via justify:

```rust
fn field_box(at: [f64; 2], j: Justify, width: f64) -> BBox {
    match j {
        Justify::Left => [at[0], at[1] - 1.6, at[0] + width, at[1]],
        Justify::Right => [at[0] - width, at[1] - 1.6, at[0], at[1]],
        Justify::Center => [at[0] - width / 2.0, at[1] - 1.6, at[0] + width / 2.0, at[1]],
    }
}
```

  Skip hidden fields (`#` refdes Reference; PWR_FLAG Value).

- [ ] **Step 4: Run the full crate suite — this is the oracle-tightening moment**

Run: `cargo test -p sch-engine`
Expected: the two new tests pass. Fixture tests (`grammar_fixtures`,
`bluepill_emit`, …) may now surface REAL pre-existing collisions the lint
could not see before. Triage each:
- solver should have fixed it but didn't → bug in obstacle/candidate code, fix it;
- collision is real but unfixable with current candidates (e.g. cluster
  label vs cluster member) → add the missing candidate (e.g. flip a cluster
  label's dir) only if cheap; otherwise record it in the task notes for
  slice 5 (polish) and relax ONLY that fixture's expectation with a comment
  referencing the spec's visual-gate section. Do NOT silently weaken the
  strict-oracle default.

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src/emit.rs crates/sch-engine/tests/
git commit -m "feat(sch-engine): lint models pin text, fields, rotated bodies"
```

---

### Task 8: Visual regression — regenerate validation renders

**Files:**
- Modify: `docs/validation/*.png` (regenerated)

- [ ] **Step 1: Regenerate**

Run: `cargo run -p agent --example render_validation`
Expected: each fixture renders without warnings-as-errors; PNGs rewritten.

- [ ] **Step 2: Eyeball every render against its reference**

For each of `555-blinker`, `divider-filter`, `mcp1703-power-entry`,
`uart-level-translator`, `bedrock-*-bluepill`: open the PNG and check
specifically for the three artifact classes this slice kills:
1. net label over pin names (was: `N_RST`/`RST` on the 555)
2. merged adjacent power values (was: `VCCDVCC3V3`)
3. value-over-refdes on rotated passives (was: `62R15`)

Any survivor is a bug in this slice — go back to the failing task, write a
minimal reproducing test, fix, re-render. (Floating islands and column
stacking REMAIN — those are slices 2 and 3.)

- [ ] **Step 3: Commit the renders**

```bash
git add docs/validation/*.png
git commit -m "test: regenerate validation renders with text placement solver"
```

---

## Plan self-review notes

- Spec coverage: spec §4 fully (solver, pin-text obstacles, field candidates,
  label candidates, lint fallback); spec §1/§3 explicitly out of scope here
  (slices 2–3); oracle strategy per spec Testing items 1 and 6 partial
  (collision classes only — full visual gate closes in slice 5).
- Coordinates in tests marked “verify against real extents” are derivation
  aids, not gospel — the engineer must pin them to actual KiCAD library
  geometry on first run, per the inline notes.
- Type consistency: `Justify`/`TextPos` defined Task 3, used Tasks 5/7;
  `label_box`/`pin_text_boxes`/`rotated_half_extents` defined Task 2, used
  Tasks 5/7; `choose`/`Movable`/`Obstacle` defined Task 1, used Tasks 3/5.
