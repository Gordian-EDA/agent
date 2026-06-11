# Elbow Router Implementation Plan (Aesthetics Slice 3 of 5)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every local (intra-block, non-power) net is drawn as real wires with junction dots — no more floating islands connected by matching label text. Labels remain only for power/global nets, inter-block nets, and router-failure degradations.

**Architecture:** New pure module `crates/sch-engine/src/route.rs` (no I/O): orientation-aware Manhattan elbow between two terminals + a best-first repair loop that shifts one interior segment to midpoint-derived candidates until collision-free (tscircuit's schematic-trace-solver approach, validated in the research phase — see spec). Per net: minimum spanning tree over terminals picks which pairs get wires. Integration restructures `emit_design_reconciled`: signal-pin labels and cluster net labels are DEFERRED — collected per net during the component/decoration loops, then either routed (wires + junctions emitted, labels dropped) or labeled exactly as today (fallback, with a recorded degradation note).

**Spec:** `docs/superpowers/specs/2026-06-11-schematic-aesthetics-design.md` section 3 (this is the spec's slice 2, landed after placement — see the ordering note in the placement plan).

**Verified integration facts (from slices 1-2):**
- Terminals available in `emit_design_reconciled`: anchor pin endpoints via `w.pin_dirs(env, refdes, pin)` (endpoint + outward `Dir`); cluster tap points via `gi.geoms[block][ci].tap_points[net] + origins[cluster_key]`; join wires already connect slotted clusters (those pins are in `join_points` and need no routing).
- Cluster decoration (`emit_cluster_decoration`, reconcile.rs:902) emits cluster-internal wires (net-attributed) BEFORE the lint section; per-pin signal labels happen in the component loop via `emit_pin` (reconcile.rs:881).
- Obstacles: `emitted_at` (IndexMap refdes→position, reconcile.rs:826) + `gi.sizes` give body rects; `textplace::rotated_half_extents` for angles (angles via the layout/members maps); `SchematicWriter.wires` (net-attributed since slice 1's idempotence fix) gives wire segments.
- Wire-touch semantics (from `retract_colliding_stubs`): ending on a foreign segment or passing through a foreign net's anchor point = electrical merge. Crossing a foreign segment mid-to-mid (no shared endpoint, perpendicular) is safe. Same-net touches are deliberate joins.
- `boxes_overlap`, `wire_box`, `point_on_segment` (emit.rs:1019) exist; junction emission via `w.add_junction` (deduped).
- The injectivity oracle (`grammar_fixtures`) catches any routing short; the bluepill test asserts zero lint warnings.

---

### Task 1: `route.rs` — elbow primitive

**Files:** Create `crates/sch-engine/src/route.rs`; register `mod route;` in lib.rs.

- [ ] Types + failing tests first:

```rust
//! Elbow router: Manhattan wires between net terminals, repair-by-shifting.
use crate::emit::Dir;

pub(crate) type Pt = [f64; 2];

/// A polyline path of axis-aligned segments (consecutive points).
pub(crate) type Path = Vec<Pt>;

/// 2-4 point Manhattan elbow from `a` (leaving along `dir_a` for >= lead mm)
/// to `b`. Returns the simplest path: straight when collinear-compatible,
/// else one L (two segments) or one Z (three segments).
pub(crate) fn elbow(a: Pt, dir_a: Dir, b: Pt) -> Path
```

Tests: straight east (a→b same y, b east of a) = 2 points; L-shape (b northeast,
dir East) = [a, [bx, ay], b]; Z-shape when b is BEHIND dir_a (lead segment 2.54
out, then back) = 4 points; all segments axis-aligned (assert helper).

- [ ] Implement: lead point `a + 2.54*dir_a`; if b on the lead axis → straight/L
  from lead; else route lead → [b.x, lead.y] → b (E/W dirs) or
  lead → [lead.x, b.y] → b (N/S dirs); drop zero-length segments; dedupe
  consecutive duplicate points.
- [ ] `cargo test -p sch-engine --lib route` green. Commit:
  `feat(sch-engine): manhattan elbow primitive`

### Task 2: collision model + path validity

```rust
/// Routing obstacles. All coordinates sheet mm.
pub(crate) struct RouteScene {
    /// Solid rects (symbol bodies): a path segment may not intersect.
    pub solids: Vec<[f64; 4]>,
    /// Foreign anchor points with their net: a segment may not pass through
    /// a point whose net differs from the routed net.
    pub points: Vec<(Pt, String)>,
    /// Existing segments with their net (cluster wires, stubs, prior routes).
    /// Touching (shared point / collinear overlap / endpoint-on-segment) a
    /// DIFFERENT net's segment is forbidden; perpendicular mid-crossing is ok.
    pub segments: Vec<(Pt, Pt, String)>,
}

pub(crate) fn path_ok(path: &Path, net: &str, scene: &RouteScene) -> bool
```

Tests: segment through a solid → false; through foreign point → false;
collinear overlap with foreign segment → false; perpendicular crossing of a
foreign segment (no shared endpoint) → true; touching same-net segment → true.
Reuse `point_on_segment` semantics (make it `pub(crate)` in emit.rs).
Commit: `feat(sch-engine): route collision model`

### Task 3: repair loop + per-edge router

```rust
/// Route one edge: elbow, then best-first repair shifting one interior
/// segment per step to candidate offsets (midpoints between the blocking
/// obstacle and neighbors), visited-set dedupe, expansion cap 200.
/// Returns None when no valid path is found within the cap.
pub(crate) fn route_edge(a: Pt, dir_a: Dir, b: Pt, net: &str, scene: &RouteScene) -> Option<Path>
```

Tests on synthetic scenes: clear field → elbow returned; single blocking rect →
detour found and `path_ok`; fully walled-in → None; determinism (same input,
same output). Commit: `feat(sch-engine): elbow repair router`

### Task 4: per-net MST + junctions

```rust
/// Order terminal pairs: MST over Manhattan distance, deterministic
/// tie-break by terminal index.
pub(crate) fn mst_edges(terminals: &[Pt]) -> Vec<(usize, usize)>

/// Junction points: where >=3 segment ENDS of one net's emitted paths meet.
pub(crate) fn junction_points(paths: &[Path]) -> Vec<Pt>
```

Tests: 3 collinear terminals → 2 edges, no redundant pair; T-shaped meeting
point of 3 ends → one junction. Commit: `feat(sch-engine): net MST + junctions`

### Task 5: pipeline integration (the big one)

**Modify `crates/sch-engine/src/reconcile.rs`:**

1. The component loop's `emit_pin` signal branch DEFERS: instead of
   `add_signal_label`, push `(net, refdes, pin, ep, dir)` into a
   `pending_signals: BTreeMap<net, Vec<…>>` (power/no-connect/joined behavior
   unchanged). Record which BLOCK each net's pins touch (for locality).
2. `emit_cluster_decoration` defers its net labels the same way: collect
   `(net, label_pos, dir, tap_point)` into `pending_cluster_labels` instead of
   `w.add_cluster_label` (wires/buses/junctions still emit immediately).
   Tap points join the net's terminal list.
3. After decoration + power flags (all fixed geometry known), build the
   `RouteScene` (bodies from `emitted_at`+sizes+angles; points/segments from
   the writer state — add a `pub(crate)` accessor on SchematicWriter), then
   per net in BTreeMap order:
   - **Routable** = non-power, single-block, >= 2 terminals.
   - Route MST edges; ALL edges ok → emit wires (`add_wire_on_net`) +
     junctions; the net's labels are DROPPED (terminals connect by wire);
     append routed segments to the scene so later nets avoid them.
   - Any edge fails → emit the net's deferred labels exactly as today
     (`add_signal_label` per pin + `add_cluster_label` per cluster label) and
     push `"route: block {b}: net {n} fell back to labels"` into
     layout_warnings (visible degradation, spec policy).
   - Non-routable nets → emit deferred labels as today.
4. Wiring oracle: extend `EmitOutput` docs — every local net is either fully
   routed or has a degradation warning; add a fixture assertion in
   `grammar_fixtures.rs` that the 555 has ZERO `route:` warnings (it must
   fully route).

Suite + fixture triage (injectivity is the safety net for routing shorts —
expect to iterate on `path_ok` edge cases). Commit:
`feat(sch-engine): route local nets as wires with label fallback`

### Task 6: visual gate

Render all fixtures; the 555/divider/mcp1703/uart must show fully-wired locals
(C2→CONT, THRES/TRIG ladder taps, uart A1/A2 series chains), junction dots at
T-joins, no label-pairs on local nets. Iterate router costs/candidates. Then
update `docs/superpowers/specs/...` non-goal checkboxes if scope shifted, and
the layout-grammar-status memory.

## Self-review notes
- Spec section 3 fully covered (MST ✓ elbow ✓ repair ✓ junctions ✓ degradation ✓
  policy table ✓ via routable-net classification).
- Deferred-label restructure preserves the exact legacy path for fallback nets,
  so behavior degrades to slice-2 output, never worse.
- Types defined in Tasks 1-4 are exactly what Task 5 consumes.
