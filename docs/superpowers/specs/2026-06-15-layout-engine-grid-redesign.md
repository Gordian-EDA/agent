# Layout engine redesign — the grid is the contract

Status: **design** (not yet implemented). Pre-release; breaking changes are fine.
Companion to `2026-06-15-circuit-lang-v2-design.md` (the author-facing `layout:`
2D grid). This doc is the *engine* side: how the grid reshapes
`crates/sch-layout/src/floorplan.rs`.

## Thesis

The floorplan engine is **already a grid placer** and nobody noticed. The Layout
IR carries `place: BTreeMap<refdes, Cell { col, row, orient }>`; `assign_cells`
drops each part into a `(col, row)` cell; `apply_cells` renders the grid to mm
(each column as wide as its widest member, each row as tall as its tallest, a
skipped index reserving no space). That ordinal grid *is* the 2D array the author
now writes.

So the redesign is not a rewrite — it is three moves:

1. **Expose the grid.** Author-facing `layout:` populates `place` directly
   (col = index in row, row = row index). One producer instead of the indirect
   "edge hint → `band_rank` → anchor column order" machinery.
2. **Make the grid rigid.** Today `refine_cells` may reorder anything; tomorrow it
   treats gridded topology as fixed and only optimizes *local* geometry.
3. **Unify authored and inferred.** With no `layout:`, `infer_ir` (→ `infer_grid`)
   still produces `place`. Authored grid and inferred grid feed the *same*
   downstream. The grid is the universal IR.

The payoff is the long-running cost lesson from
[[floorplan-engine-state]] stated structurally: **a single global cost over
diverse circuits is whack-a-mole because the optimizer owns too many degrees of
freedom (the global arrangement).** The grid takes that DOF away — the author (or
one inference pass) fixes arrangement; the optimizer is left with local geometry,
which a local cost *can* judge.

## The grid IR

Keep `LayoutIr` as the contract; sharpen the meaning of `place`:

```rust
pub struct LayoutIr {
    pub flow:   Flow,                       // Lr (default) | Tb
    pub rails:  BTreeMap<String, Band>,     // power net → top/bottom bus  (inferred)
    pub place:  BTreeMap<String, Cell>,     // refdes → grid cell          (AUTHORED or inferred)
    pub ports:  BTreeMap<String, Side>,     // net → exit edge             (authored `ports:` or inferred)
    pub mirror: BTreeSet<String>,           // anchors to flip L↔R         (inferred)
    pub float:  BTreeSet<String>,           // NEW: spanning anchors that float in their cell-span
}
```

`Cell { col, row, orient }` is unchanged. The only structural addition is `float`
— the set of anchors named in more than one grid cell (the "repeated = floating"
rule). Everything else the author used to influence (`side`/`edge`/`near`) is
gone from the model (see *Deletions*).

### Authored grid → `place`

`grid_from_layout(design) -> partial LayoutIr.place`:

- Walk `layout:` rows. For row `r`, cell `c`, entry `name`:
  - resolve `name` **block-first** then as a refdes;
  - a **block** expands to all its component refdes, each pinned to `(col=c,
    row=r)` (they share the cell's column region; intra-block stacking is local,
    below);
  - a **component** pins that one refdes to `(c, r)`.
- An entry appearing in ≥2 cells → add every owned refdes to `float`; its anchor
  cell is the **centroid** of its occurrences (rounded), and refine may slide it
  within their bounding box.
- `~` / empty → no entry (a reserved hole; the ordinal grid already skips it).

### Inferred grid → `place` (was `infer_ir`)

Rename `infer_ir` → `infer_grid`; same job, but its *output is understood as a
grid*, and its default ordering is explicit:

- **Default order = block declaration order**, laid out as a single row
  left→right (col = block index). This is the "order decides position" default —
  cheap, predictable, no heuristic.
- Then refine that row from connectivity *only where it helps*: split a too-tall
  column into rows, pull a satellite block beside its anchor, choose `mirror`,
  choose `ports` sides, set `orient`. These are the existing inference rules
  (`anchor_tap`, `wants_mirror`, `series_orient`, …) — they keep running; they
  just annotate a grid instead of fighting to *invent* the column order.
- **Delete the order heuristic.** `order_anchors` + `band_rank` exist only to turn
  edge hints into a column order. With authored grids and declaration-order
  defaults, both are dead. (This is the single biggest simplification.)

### Compose authored ∪ inferred

`layout:` may be **partial** (pin the three structural anchors, ignore the rest):

1. start from `grid_from_layout` (authored cells, rigid);
2. run `infer_grid` for everything **not** already placed — ungridded parts get a
   spare column (today's `assign_cells` `spare = max_col + 1` path already does
   exactly this) or, better, attach to the gridded anchor they're most incident
   to (the adjacency rule, now load-bearing);
3. the union is the final `place`.

## Pipeline

Unchanged in shape; the change is *where arrangement is decided* and *what refine
may move*:

```
gather → incidence → compute_needs_flag
       → GRID  = grid_from_layout ∪ infer_grid          // was: infer_ir
       → assign_cells                                    // unchanged: place → Vec<Cell>
       → refine_cells  (GRID-RIGID)                      // narrowed move set
       → polish (align_to_pins + compact + free_nudge)   // unchanged
       → decongest → build_writer
       → prepare (split_wires_at_nodes, text, reframe) → finish
```

### `refine_cells` becomes grid-rigid

Today refine proposes nudge / swap / rotate / side-flip over *all* items, gated by
routed cost. Under the rigid grid, the move set is **scoped by whether a part is
gridded**:

- **Gridded anchor (in authored `place`, not in `float`)** — its `(col,row)` is
  **frozen**. Allowed: `orient` flip, `mirror` flip. *Not* allowed: changing
  col/row (no cross-cell reordering). The author said where it goes.
- **Float anchor (in `float`)** — may move *within the bounding box of its cells*
  only; same orient/mirror freedom.
- **Ungridded part (satellite / inferred)** — full freedom: nudge, swap with
  another ungridded part, re-attach to a different anchor column, orient. This is
  where the optimizer still earns its keep (clean cap rows, spine collinearity).

Net effect: the search space shrinks to *local* geometry, so refine is faster and
can't wander a reference layout into a cheaper-but-uglier basin (the mcp1703
regression in [[floorplan-engine-state]]).

### Cost simplification

The grid removes the terms that *priced global arrangement* (and that caused the
verify-against-all-four whack-a-mole):

- **Drop / demote** `spread` (compactness across the whole frame — the grid sets
  spacing) and the anchor-ordering bias `order_anchors`/`band_rank` consumed.
- **Keep** every term that judges *local geometry*, because that is now all refine
  controls: `count_corners`, `count_body_crossings` + `count_ic_body_crossings`,
  `count_foreign_taps`, `body_overlap_count`, `orient_viol`, `spine_viol`,
  `cluster_label_boxes`, `supply_pin_target`.

The litmus stays: when a layout is ugly-but-cheap, find the *local* term that
distinguishes good from bad and price it — but now there is no global DOF for a
mispriced term to exploit board-wide.

### Anneal: retire it

`anneal_cells` (multi-start SA) is already OFF by default and, per
[[floorplan-engine-state]], never wins once refine + a good cost place the
targets. A rigid grid makes the global escape **moot** — global structure is the
grid, authored or inferred. Recommendation: **delete `anneal_cells` and the
`ANNEAL` flag** with this change (one fewer code path, one fewer "verify all four"
axis). If a future free-form mode wants global search, it returns as its own thing.

## Block-cell expansion & float (the two new behaviours)

1. **Block cell.** A block name in a cell claims a column region for all its
   parts. The block's anchor (its IC, or its highest-degree part) takes the cell;
   its satellites stack/flank locally by the existing inference (decoupling caps
   to the supply pin, series parts along the flow). Intra-block arrangement is
   *never* authored — it is inferred, exactly as the v2 language doc promises.
2. **Float.** A repeated anchor (`MCU` in two rows) is placed once at the centroid
   of its cells and added to `float`; refine slides it within the cells' bounding
   box to minimize wire length to its references. This is the central-hub idiom
   (an MCU between two connector banks) with no row pinned.

## Deletions (kill legacy with the change)

Per the standing "kill all legacy" direction:

- `circuit_lang::model`: drop `Edge`, `LayoutHint`, `Block.layout`,
  `Component.layout` (the per-block/part edge hint added in `6b6e81d`/`86d5bd4`).
  Add the `layout:` grid (a `Vec<Vec<Option<String>>>` on `Design`).
- `floorplan.rs`: delete `order_anchors`, `band_rank`. Add `grid_from_layout`;
  rename `infer_ir` → `infer_grid`; add `float` handling to `assign_cells` /
  `apply_cells` / `refine_cells`. Delete `anneal_cells`, `nudges`, the `ANNEAL`
  path, `Rng` if unused after.
- `parse.rs` / `surface.rs` / `desugar.rs`: parse top-level `layout:` (the 2D
  array of block/refdes names); remove the per-component/-block `layout` key.

## Validation (the gate never changes)

- **References byte-identical.** The four tuned targets have **no** `layout:`
  grid, so they exercise `infer_grid`'s declaration-order default + inference.
  Their renders must stay byte-for-byte identical — that is the hard regression
  gate for "did the unification change inferred placement?" Render with
  `render_targets`, judge with fresh sub-agents (CLAUDE.md *Visual review*).
- **Grid honored.** Add a fixture *with* a `layout:` grid (the J1/MCU/USB shape)
  and assert: gridded anchors land in grid order; a repeated anchor floats between
  its rows; ungridded caps land beside their anchor.
- **Truthful netlist.** `cargo test --release -p sch-layout --test
  floorplan_netlist` stays green across both tiers throughout.

## Why this is the right shape

- It matches how the engine *already* works (`place`/`Cell`/`assign_cells`), so it
  is mostly deletion + exposure, not new machinery.
- It puts the one non-inferable thing (global arrangement) in the author's hands
  at exactly the granularity they think in (a 2D floorplan of the big blocks).
- It shrinks the optimizer to local geometry, ending the multi-circuit cost
  whack-a-mole at the root (fewer DOF) instead of by adding yet another term.
