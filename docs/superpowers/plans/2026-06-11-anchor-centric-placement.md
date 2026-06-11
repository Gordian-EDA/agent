# Anchor-Centric Placement Implementation Plan (Aesthetics Slice 2 of 5)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Blocks arrange like the hand-drawn references: the anchor IC sits at the center, each cluster slots beside the IC pin it feeds (East pin → right, West → left), power-feed clusters go above, ground-heavy clusters below — replacing today's single-column stacking.

**Architecture:** Restructure `place_with_anchor_pins` (place.rs) around a per-block anchor-centric layout: anchors placed on a horizontal axis first, then clusters assigned a side via a pure classification function (primary anchor tap's pin direction; rail polarity when tap-less), slotted with outward-push overlap resolution. The existing single-tap E/W slot-with-join logic generalizes to multi-tap clusters using a deterministic primary tap. Row-packing (`pack_units`) remains the fallback for anchor-less blocks. `layout_rev` gains a placer-version token so priors from the old placer are discarded once.

**Spec:** `docs/superpowers/specs/2026-06-11-schematic-aesthetics-design.md` section 2. **Ordering note:** this is the spec's slice 3, deliberately landed before the router (spec slice 2 / build-order item 2): routing quality depends on placement compactness, and placement improves renders even with label connectivity. The spec's "each step independently landable" covers this swap.

**Key existing facts (verified):**
- `Layout { positions, angles, cluster_origins, joins }` (place.rs:25); joins = (pin_endpoint, tap_point, net), consumed by reconcile to draw straight join wires.
- `Cluster.anchor_taps: Vec<(NetName, RefDes, String)>` (grammar.rs:295) — may be MULTIPLE per cluster; current slot pass skips clusters with ≠1 tap (place.rs:339).
- `AnchorPinEnds: (RefDes, pin) → (offset@angle0, Dir)` (place.rs:51).
- `ChainClass::{RailRail, ToRail, Series}` (grammar.rs:245); `is_ground(net)` (grammar.rs:16); `design.nets[net].power`.
- `pack_units` (place.rs:142) row-packing; `overlaps_any` (place.rs:192).
- `layout_rev` (reconcile.rs:142) hashes `edge|near|role[|cluster=…]`.
- Cluster tap alignment invariant: keep tap-aligned coordinates EXACT (un-snapped y for E/W) so joins stay straight (place.rs:366-369 comment).
- Tests use `Mock:BIG`/`Mock:SMALL` providers + `build_test_geoms` (place.rs:434-482).

---

### Task 1: Side classification (`place.rs`, pure function)

**Files:** Modify `crates/sch-engine/src/place.rs`

- [ ] **Step 1: Write failing tests**

```rust
    #[test]
    fn cluster_side_follows_primary_tap_direction() {
        // Tap on an East pin -> Side::East; West pin -> Side::West.
        let mut pin_ends = AnchorPinEnds::new();
        pin_ends.insert(("U1".into(), "A".into()), ([10.16, 0.0], Dir::East));
        pin_ends.insert(("U1".into(), "B".into()), ([-10.16, 0.0], Dir::West));
        let taps_e = vec![("N1".to_string(), "U1".to_string(), "A".to_string())];
        let taps_w = vec![("N2".to_string(), "U1".to_string(), "B".to_string())];
        assert_eq!(cluster_side(&taps_e, &pin_ends, &|_| false, &|_| false), Side::East);
        assert_eq!(cluster_side(&taps_w, &pin_ends, &|_| false, &|_| false), Side::West);
    }

    #[test]
    fn tapless_cluster_side_follows_rail_polarity() {
        // No taps: a cluster touching a V+ rail goes North (above), a ground-
        // only one goes South (below). V+ wins when both are present (the
        // cluster hangs from the supply; its ground end points down anyway).
        let pe = AnchorPinEnds::new();
        let no_taps: Vec<(String, String, String)> = vec![];
        let is_vplus = |n: &str| n == "9V";
        let is_gnd = |n: &str| n == "GND";
        // Helper takes the cluster's net set via closures over a provided list;
        // see Step 3 signature: nets are passed as a slice.
        assert_eq!(side_of_tapless(&["9V".into(), "N1".into()], &is_vplus, &is_gnd), Side::North);
        assert_eq!(side_of_tapless(&["N1".into(), "GND".into()], &is_vplus, &is_gnd), Side::South);
        let _ = (pe, no_taps);
    }
```

- [ ] **Step 2: Run, verify failure** — `cargo test -p sch-engine --lib cluster_side` (compile error: types missing).

- [ ] **Step 3: Implement**

```rust
/// Which side of its anchor a cluster attaches to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    East,
    West,
    North,
    South,
}

/// Side from the PRIMARY anchor tap (first tap whose pin direction is known —
/// anchor_taps order is deterministic from `analyze`). Falls back to rail
/// polarity (`side_of_tapless`) when no tap resolves.
pub(crate) fn cluster_side(
    taps: &[(String, String, String)],
    pin_ends: &AnchorPinEnds,
    is_vplus: &dyn Fn(&str) -> bool,
    is_gnd: &dyn Fn(&str) -> bool,
) -> Side {
    for (_net, aref, apin) in taps {
        if let Some(&(_, dir)) = pin_ends.get(&(aref.clone(), apin.clone())) {
            return match dir {
                Dir::East => Side::East,
                Dir::West => Side::West,
                Dir::North => Side::North,
                Dir::South => Side::South,
            };
        }
    }
    let nets: Vec<String> = taps.iter().map(|(n, _, _)| n.clone()).collect();
    side_of_tapless(&nets, is_vplus, is_gnd)
}

/// Rail-polarity side for a cluster with no resolvable tap: V+ -> North,
/// ground -> South, neither -> South (decoupling/banks read best at the
/// bottom, matching the references).
pub(crate) fn side_of_tapless(
    nets: &[String],
    is_vplus: &dyn Fn(&str) -> bool,
    is_gnd: &dyn Fn(&str) -> bool,
) -> Side {
    if nets.iter().any(|n| is_vplus(n)) {
        Side::North
    } else if nets.iter().any(|n| is_gnd(n)) {
        Side::South
    } else {
        Side::South
    }
}
```

- [ ] **Step 4: Run** — `cargo test -p sch-engine --lib cluster_side` + `tapless` → PASS.
- [ ] **Step 5: Commit** — `feat(sch-engine): cluster side classification for anchor-centric placement`

---

### Task 2: Per-block anchor-centric layout

**Files:** Modify `crates/sch-engine/src/place.rs`

The core restructure. New function, called per block from `place_with_anchor_pins`, replacing the pack-then-slot flow for blocks that HAVE at least one anchor; anchor-less blocks keep `pack_units`.

- [ ] **Step 1: Write failing tests**

```rust
    /// East-tap cluster lands right of the IC with a straight join; the
    /// remaining sides hold their assigned clusters without overlaps.
    #[test]
    fn anchor_centric_block_slots_clusters_by_side() {
        // U1 with pins on both sides; R-divider tapped from an East pin,
        // pull-up cluster to 9V (no resolvable tap -> North).
        let design = compile(
            "
version: 1
name: t
rails: [9V, GND]
blocks:
  a:
    components:
      U1: {part: Mock:BIG, pins: {A: CC1, B: N2, C: N3, D: N4}}
      R1: {part: Device:R, value: 5k1, between: [CC1, GND]}
      C5: {part: Device:C, value: 100n, between: [9V, GND]}
      C6: {part: Device:C, value: 100n, between: [9V, GND]}
",
        );
        let provider = place_test_provider();
        let graphs: IndexMap<String, BlockGraph> = design
            .blocks
            .keys()
            .map(|n| (n.clone(), crate::grammar::analyze(&design, n, &provider)))
            .collect();
        let mock_pins = |_: &str, pin: &str| match pin {
            "1" => Some([0.0, -3.81]),
            "2" => Some([0.0, 3.81]),
            _ => None,
        };
        let geoms = build_test_geoms(&design, &graphs, &mock_pins);
        let mut pin_ends = AnchorPinEnds::new();
        pin_ends.insert(("U1".into(), "A".into()), ([10.16, 0.0], Dir::East));

        let layout = place_with_anchor_pins(&design, &SizeMap::new(), &graphs, &geoms, &pin_ends);
        let u1 = layout.positions["U1"];
        // R-divider cluster East of U1 with a straight join on CC1.
        let join = layout.joins.iter().find(|(_, _, n)| n == "CC1").expect("join");
        assert_eq!(join.0[1], join.1[1], "straight horizontal join");
        assert!(layout.positions["R1"][0] > u1[0], "R1 east of U1");
        // 9V decouple bank (rail-rail, tap-less) sits BELOW the anchor row
        // if ground-classified, or ABOVE if V+ classified (9V present -> North).
        assert!(layout.positions["C5"][1] < u1[1], "bank above the anchor");
        // No member shares a position with U1.
        assert!(no_overlaps(&layout), "{layout:?}");
    }
```

(Adjust the bank-side expectation after Step 3 if the geometry test reveals the
9V/GND bank classifies South via `is_gnd` first — the assertion must encode the
IMPLEMENTED priority: V+ before ground, per `side_of_tapless`.)

- [ ] **Step 2: Run, verify failure** — today the bank packs in a row right of U1, R1's cluster slots East only if single-tap; assertions on `C5` fail.

- [ ] **Step 3: Implement `layout_block_anchor_centric`**

Signature and algorithm (replaces the per-block placement body when
`!graph.anchors.is_empty()`):

```rust
/// Anchor-centric per-block layout. Returns the block envelope; writes
/// positions/angles/origins/joins through the same maps as the packed path.
#[allow(clippy::too_many_arguments)]
fn layout_block_anchor_centric(
    block_name: &str,
    block: &Block,
    graph: &BlockGraph,
    geoms: &[ClusterGeom],
    sizes: &SizeMap,
    pin_ends: &AnchorPinEnds,
    design: &Design,
    block_origin: [f64; 2],
    positions: &mut IndexMap<RefDes, [f64; 2]>,
    angles: &mut IndexMap<RefDes, f64>,
    cluster_origins: &mut IndexMap<String, [f64; 2]>,
    joins: &mut Vec<([f64; 2], [f64; 2], String)>,
) -> [f64; 2] // envelope
```

Algorithm:
1. **Anchors on a row.** Anchors in `graph.anchors` order, left→right, each
   centered vertically on a common axis `axis_y`; horizontal pitch =
   `cell_of(anchor)` widths + `BAND_GAP_MM`. (Single-anchor blocks: the IC
   centers the block.) Record each anchor's rect.
2. **Classify clusters.** For each cluster (index order): `side =
   cluster_side(&cluster.anchor_taps, pin_ends, …)` with `is_vplus(n) =
   design.nets.get(n).map_or(false, |a| a.power) && !is_ground(n)` and
   `crate::grammar::is_ground`.
3. **Slot E/W clusters tap-aligned.** Primary tap = first tap with a known pin
   end. Pin endpoint = anchor position + offset (angle 0). Cluster origin
   exactly as today's slot pass (x snapped once, y exact for straight join),
   `JOIN_MM` gap. Overlap with already-placed rects → push outward (E: +x,
   W: −x) in 2.54 mm steps until free (cap 40 steps, then fall through to the
   leftover list). Record the join for the PRIMARY tap only (other taps keep
   label connectivity until the router slice).
4. **Stack N/S clusters.** North clusters in a row above the anchor row
   (right-to-left fill, `BLOCK_GAP_MM` above the tallest anchor rect); South
   in a row below. No joins (label connectivity); x-centered near their
   primary tap pin when known, else packed in arrival order. Same outward-push
   on overlap (N: −y, S: +y).
5. **Leftovers** (push-cap hit): packed in a row below everything via
   `pack_units` on the remainder.
6. Envelope = bbox of all placed rects, returned for band stacking.

Wire it in `place_with_anchor_pins`: for each block, if its graph has anchors
→ the new path; else → existing `pack_units` path. Keep the existing
anchor-slot post-pass ONLY for the packed path (the centric path subsumes it).

- [ ] **Step 4: Run** — new test + `cargo test -p sch-engine --lib place` → all place tests pass. `single_tap_cluster_slots_beside_its_anchor_pin` must STILL pass (the centric path reproduces E/W tap-aligned slotting; update only if join geometry is identical-but-relocated, verifying the new positions are correct first).

- [ ] **Step 5: Commit** — `feat(sch-engine): anchor-centric per-block placement`

---

### Task 3: Placer version in layout_rev + pipeline integration

**Files:** Modify `crates/sch-engine/src/reconcile.rs:147`

- [ ] **Step 1:** Add the placer token to `layout_rev`'s desc:

```rust
    let mut desc = format!(
        "placer=v2|edge={:?}|near={:?}|role={:?}",
        block.layout.edge, block.layout.near, comp.layout_role,
    );
```

- [ ] **Step 2:** Run `cargo test -p sch-engine --test layout_rev --test reconcile` — tests asserting rev stability across UNRELATED edits must still pass; any golden rev strings update deliberately.

- [ ] **Step 3: Commit** — `feat(sch-engine): bump layout_rev for anchor-centric placer (one-time re-place)`

---

### Task 4: Full suite + fixture triage

- [ ] **Step 1:** `cargo test -p sch-engine --no-fail-fast`. The new geometry WILL surface new lint collisions (clusters now adjacent to ICs). Triage per the slice-1 rule: solver/obstacle bug → fix; missing candidate → add; genuinely-tight intentional adjacency → extend the adjacency allowlist (anchor↔slotted-cluster-member pairs are the expected new class — add `(anchor, member)` pairs for clusters whose join was recorded, in the reconcile allowlist builder).
- [ ] **Step 2:** Commit fixes — `fix(sch-engine): lint/solver adjustments for anchor-centric geometry`

---

### Task 5: Visual gate

- [ ] **Step 1:** `cargo run -p agent --example render_validation`
- [ ] **Step 2:** Eyeball all renders vs `docs/validation/references/`: the 555's RC network must sit beside the IC (left, where DISCH/THRES/TRIG exit), the LED branch near OUT (right), C2 below CONT; uart pull-up stacks near U2's B-side; mcp1703 banks below the LDO. Column-of-islands must be GONE. Iterate constants (gaps, push step, side priorities) until the arrangement reads like the reference. Sparseness warnings should drop.
- [ ] **Step 3:** Commit — `feat(sch-engine): anchor-centric placement tuning from visual review`

## Plan self-review notes

- Spec §2 coverage: side assignment ✓ (Task 1), generalized slotting ✓ (Task 2 step 3), greedy outward push ✓, multi-anchor row ✓ (left→right by `graph.anchors` order — signal-flow ordering deferred to the polish slice, recorded here as a conscious cut), grid ✓ (snap as today), rev bump ✓ (Task 3).
- The N/S cluster join wires are deliberately NOT drawn this slice (labels remain); the router slice replaces them. Only E/W primary-tap joins are wires, same as today's invariant.
- Type consistency: `Side`, `cluster_side`, `side_of_tapless` (Task 1) consumed in Task 2; `layout_block_anchor_centric` private to place.rs.
