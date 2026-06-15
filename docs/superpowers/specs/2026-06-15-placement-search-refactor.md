# Placement-search refactor — one seeded search, greedy + SA as strategies

Status: **design / proposal** (not implemented). Informed by a 3-lens design panel
+ adversarial synthesis (run `wf_a680da5b-0ca`). Owner decisions pending (see end).

## The problem

`emit`'s placement is two optimizers over two representations plus a hard repair
outside the cost:

```
assign_cells → SEARCH over Vec<Cell>{col,row,orient}  (refine_cells / anneal_cells, score_cells)
            → apply_cells (Cell → mm) → normalize
            → polish    (CONTINUOUS mm: align_to_pins + compact + free_nudge, score_items)
            → decongest (UNCONDITIONAL overlap relaxation, NOT cost-gated)
```

The search minimizes `cost(apply_cells(cells))` — the pre-polish **column/row
table** — but what ships is `polish(decongest(apply_cells(cells)))`. The optimizer
optimizes a **proxy**; polish and decongest then mutate a result it never saw. The
SA only searches the coarse cell space, so "improve the SA" has a low ceiling: it
can't influence the fine geometry that ships.

## The goal

Merge the projection + polish + decongest **into** the placement search so the
optimizer's objective **is** the shipped geometry — one search over grid-snapped
mm positions with a multi-scale move set — while keeping **greedy and SA as
swappable counterparts** behind one interface (SA later a paid feature).

## Design

### Unified state
The search operates on `Vec<Item>` directly — three mutable fields per item:
`at:[f64;2]` (grid-snapped mm, `crate::grid::snap`, 1.27), `angle` (0/90/180/270),
and a **new `mirror:bool`** lifted off `ir.mirror` (so a move can flip it and the
cost sees it). `Cell{col,row,orient}` survives only as authoring IR and as the
**seed**: `assign_cells → apply_cells → normalize` runs **once** to project the IR
grid into the initial, fully-aligned, overlap-free mm start; the search then runs
in mm. `score_cells` is deleted (it was the apply+score wrapper); the search calls
`score_items` directly.

### The strategy interface (greedy + SA share everything but the accept rule)
```rust
struct SearchCtx<'a> { env, inc, ir, needs_flag, frozen: &'a [bool] }
type Cost<'a> = dyn Fn(&[Item]) -> f64 + 'a;          // == build_writer + layout_cost
trait MoveGen { fn propose(&self, items:&mut [Item], cx:&SearchCtx, rng:&mut Rng) -> Option<Undo>; }
trait PlacementStrategy {
    fn search(&self, items:&mut Vec<Item>, cost:&Cost, moves:&dyn MoveGen, cx:&SearchCtx, seed:u64);
}
```
`Greedy` (was `refine_cells`) and `Anneal` (was `anneal_cells`) are two impls over
the **same** `Cost`, the **same** `MoveGen`, the **same** `Undo` rollback. The
ONLY divergence is the accept rule (strict `c+0.5<best` vs Metropolis
`rng.unit() < exp(-Δ/t)`) and the loop driver (fixpoint sweep vs cooled iteration
keeping best-seen). Zero duplicated cost/move logic. Because SA seeds from the
same projected start and keeps best-seen, **SA can only match-or-beat greedy** —
shipping it as the paid tier is strictly an upgrade, never a regression.

Selection is one factory at the single point in `emit` (replacing floorplan.rs
~645–661): env `LAYOUT_SEARCH=greedy|anneal` (with `ANNEAL=1`/`GREEDY=1` aliases
during migration), then a `Tier` enum (Free→Greedy, Pro→Anneal) plumbed from agent
config so SA becomes a paid feature **without touching the engine**.

### Move set (multi-scale, all grid-snapped, all cost-scored)
Mobile = satellites (anchors held, as today; anchor mobility deferred — open Q).
- **Coarse** (today's cell moves, now in mm, snapping to COL_GAP/ROW_GAP so they
  still leap a "cell"): `relocate`, `swap`, `rotate` (cycle the 4 angles),
  `mirror` (folds `ir.mirror` exploration into the search — applies to anchors),
  `side_flip` (reflect across the served anchor's column x).
- **Fine** (today's polish, now per-move so the search *trades* them, not a
  myopic post-pass): `snap_to_axis` (one 1.27 step toward the pin axis /
  `group_axis`), `compact` (one step toward the centroid), `free_nudge` (±1.27 in
  x/y — the off-axis freedom that reaches uncross-a-wire positions).
- **Key behavior change**: drop `compact`/`free_nudge`'s clearance-padded overlap
  hard-gate — a move *into* a transient overlap is allowed if it strictly lowers
  total cost, because overlap is now priced (below). This is the freedom polish
  cannot currently use.

### Re-earning alignment (the hard part — a naive merge goes organic-blob)
The table gave free column alignment (one x per column via `track_centres`). A
continuous search loses it the instant a move offsets a part 1.27. Re-earn it by
**carrying the column identity forward as data, not as a layout mechanism**:
- at seed time, record `align_group[i]` (the seed `cells[i].col` → a group id) and
  `group_axis[i]` (the seed mm x from `apply_cells`' `col_x`),
- add an **`align_viol` cost term**: per group, sum `|item.at[0] − group_axis|`
  (x-spread off the shared track), weight ~4 — so a column-mate **pays to leave**
  its shared x but a real wire win can still buy the move. Soft + escapable: the
  whole point of folding `align_to_pins` in.
- augment with the `snap_to_axis` move so a satellite can also snap to a live pin
  axis the seed column didn't capture.

### decongest → overlap-as-cost + a thin hard net
Keep `body_overlap_count` + label overlaps at **1500** (the wall = the guarantee's
price) AND add a small continuous **penetration-depth** term (Σ `min(pen_x,pen_y)`
over overlapping pairs, weight ~50) so the gradient points **out** of an overlap —
a count alone can't break a tie between two equally-overlapping candidates, which
is exactly why `decongest` had to exist procedurally. Keep `decongest()` unchanged
as a thin final `hard_separate()` **safety net**, run once post-search; it now
fires only on a rare sub-grid touch instead of being the primary mechanism.

### Cost summary (`layout_cost`)
Add: `align_viol` (~4), `pin_axis_miss` (~0.5), `penetration` (~50). Keep at 1500:
overlap wall. Keep unchanged (already judge shipped geometry):
merges/shorts/foreign_taps (2000), fallbacks (1000), body+ic_body_cross (30),
orient_viol (12), spine_viol (10), corners (7), congestion (7), crossings (5),
junctions (1), stray (0.5), length (0.15). Do **not** add an aspect/height term
(history: it regressed mcp1703). Every weight retune re-validated against all four
references + the oracle — review the **PICs, not the cost numbers**.

### Determinism + regression
`search` takes an explicit `seed:u64` (default the existing `0xD1B54A32D192ED03`).
All randomness flows through one `Rng(seed)`; state is `Vec<Item>`, incidence is a
`BTreeMap`, item order is gather order → deterministic. Regression shifts from
"byte-identical to the historical hand-tuned references" to **deterministic
snapshot + intentional re-baseline**:
- snapshot = a compact `{refdes → (at, angle, mirror)}` hash per fixture × strategy
  × seed (immune to emit() formatting churn),
- an **"emit twice, assert identical"** test guards against accidental
  nondeterminism (land it *before* any randomized move),
- the **netlist oracle stays the always-on hard correctness gate** (it asserts
  connectivity, invariant to geometry); the **unbiased-subagent visual review**
  fills the aesthetic gap the dropped byte gate leaves; a deliberate change that
  moves a snapshot re-baselines it as an explicit reviewed commit.

## Staged migration (each step independently shippable)

0. **Lift `mirror` onto Item** (no behavior change). build_writer reads `it.mirror`.
   Most-reviewed step — a scored/shipped mirror desync is the exact bug class this
   kills. Gate: references + oracle byte-identical.
1. **Interface, no logic change**: introduce `SearchCtx`/`Cost`/`PlacementStrategy`;
   wrap today's `refine_cells`/`anneal_cells` bodies verbatim as `Greedy`/`Anneal`,
   still over `Vec<Cell>` via `score_cells`. Byte-identical.
2. **One MoveGen, still cells**: `MultiScaleMoves` + `Undo` over the existing cell
   moves; both strategies call it. Byte-identical.
3. **Snapshot harness FIRST**: deterministic snapshot test + bless path
   (`UPDATE_SNAPSHOTS=1`) + emit-twice-identical, capturing CURRENT output. Land
   the replacement gate *before* any geometry change.
4. **Flip state to mm**: search over `Vec<Item>`; seed via the one-time
   apply_cells (expose `col_x` for `group_axis`); derive `align_group`; mm moves.
   Delete `score_cells`. Output changes → new snapshot + visual review + re-baseline.
5. **Fold polish in + re-earn alignment**: add fine moves (drop the clearance
   gate); delete `polish()`; add `align_viol`/`pin_axis_miss`/`penetration`; tune
   against all four + oracle until columns hold crisp. Re-baseline.
6. **decongest → thin net**: reduce to a post-search `hard_separate()`; assert it
   moves <1 part on references, never reintroduces a short; oracle green.
7. **Tier wiring + make SA win**: `pick_strategy` (env + Tier); verify SA beats
   greedy on the **INFER frames** (#28) now that objective == geometry.
8. **Cleanup**: delete dead code (`score_cells`, the standalone polish wrappers,
   `nudges`, anti-thrash/same-cell guards). Keep `Rng`, `apply_cells`/`normalize`
   (seed), `decongest` (renamed net).

## Risks
- **Whack-a-mole**: the four references are saturated; any new term can regress the
  two that already match (divider, mcp1703). Mitigate: re-validate all four +
  oracle per change; PICs not numbers; the visual gate is mandatory.
- **Organic-blob**: too-weak `align_viol` → columns drift; too-strong → pins parts
  that should leave. Seed starts aligned (alignment preserved-by-default), but the
  weight needs the four-target loop.
- **Dropping the clearance gate**: a dense BGA could keep an overlap only
  `hard_separate` fixes, reintroducing "the optimizer never saw it" at the margin.
  Mitigate: high wall, assert `hard_separate` ≈ 0 moves, confirm no new short.
- **Eval blowup**: `score_items` (build_writer + reroute) per move over a bigger
  space; the 121-ball BGA may strain the ~505s oracle. Keep explicit iteration
  budgets (speed deferred, but don't blow the test budget).
- **Re-baseline policy**: a subtle cost bug could pass (deterministic + oracle-clean)
  yet look worse — every re-baseline MUST pair with the visual review.

## Owner decisions — RESOLVED (2026-06-15)
1. **Re-baseline policy** → **YES**, drop byte-identical-to-references; gate on
   deterministic snapshot + the netlist oracle + unbiased-subagent visual review.
2. **Scope** → **the whole arc (steps 0–8)**.
3. **Tier scope** → **env-only now** (`LAYOUT_SEARCH` + `ANNEAL`/`GREEDY` aliases);
   the real `Tier` enum from agent config is a follow-up.
4. **Anchor mobility** → **anchors are mobile** — add a grouped anchor-drag move
   (anchor + its satellites move atomically; careful Undo). Authored-grid anchors
   stay in the `frozen` set.
5. **Alignment scope** → columns-only first (rows carried by spine/orient); revisit.
6. **SA multi-start** → keep the seeded-vs-broad best-of-two for the paid tier.
