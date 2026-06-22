# Schematic-driven PCB (`derive_board`)

## Problem

Today the schematic and PCB flows are independent. `create_board` makes the LLM
**re-type** every part, footprint, and `pad_nets` map by hand — even though it just
authored (or could lift) a schematic that already contains the parts and the full
netlist. This is error-prone, and nothing checks that the board's connectivity matches
the schematic's. Human EDA works the other way: capture the schematic once, then reuse
its parts + nets for the board, associating a footprint per part.

Two user scenarios must both be served:

1. **Full e2e** — the agent designs the schematic from a prompt, then lays out the board.
2. **Bring-your-own schematic** — the user supplies a `.kicad_sch` (drawn in KiCAD) and
   asks the agent only to assign footprints and lay out the board.

## Guiding principle: the schematic engine never cares about footprints

`sch-layout` is footprint-agnostic and **stays that way**. The emit path already hardcodes
an empty property — `emit.rs`: `(property "Footprint" "")` — and never reads
`Component.footprint`. Two consequences drive the whole design:

- **Footprints must live board-side**, not in the schematic. A footprint written into an
  agent-authored `.kicad_sch` would be wiped on the next `apply_design` re-emit. So the
  source of truth for "which footprint for refdes X" is a board-side assignment, not the
  sheet.
- **No `sch-layout` change is needed or wanted.** `derive_board` *calls* the existing
  `lift()`; `assign_footprints`'s optional sheet write-back is a `kicad-bridge` in-place
  property patch, never a re-emit. `create_design`'s prompt continues to **not** mention
  footprints — authoring stays about symbols + nets only.

## Architecture: one shared spine

Both scenarios collapse onto:

```
Design  ──►  assign_footprints  ──►  derive_board  ──►  place_board ─► route_board ─► export_board
(source)     (fill refdes→lib_id)    (BoardDraft)
```

The only thing that varies is where the `Design` (parts + pin→net) comes from:

| | Scenario 1 (agent-authored) | Scenario 2 (user-supplied) |
|---|---|---|
| Connectivity source | compiled draft `Design` (`Component.pins`) | `kicad-cli` netlist of the user's `.kicad_sch` |
| Footprint origin | `assign_footprints` (all parts) | user's pre-assigned footprints + `assign_footprints` for gaps |
| Schematic write path | `apply_design` re-emits (agent owns layout) | **never re-emit**; optional in-place property patch only |
| Gate | `apply_design` + `derive_board` | `derive_board` (the agent doesn't write the user's sheet) |
| `derive_board` | identical | identical |

`derive_board` accepts connectivity from **either** source, normalized to
`(refdes, pin#→net)` tuples. Connectivity is read footprint-free; footprints are joined in
from the board-side assignment map.

## Where footprints are stored

A board-side assignment map: `refdes → footprint lib_id`, persisted in
`.autopcb/footprints.json` (sibling of `board.json`). Rationale:

- Survives schematic re-emits (the schematic can't hold it — see the principle).
- Lets `derive_board` be a pure read: connectivity (Design/netlist) ⨝ footprints (map).
- For Scenario 2, the map is **seeded** from any footprints the user already assigned in
  KiCAD (read from the netlist's `<comp><footprint>` field), so the agent only fills gaps.

## Tool contracts

### `assign_footprints`

> Set the footprint for parts: a `refdes → lib_id` map. A plain board-side map writer over
> `.autopcb/footprints.json` — **not** gated, and it does **not** read the `Design` or report
> gaps (that's `derive_board`'s job). It is the canonical home for footprint selection: the
> agent finds a `lib_id` with `search_footprints`, then sets it here. Kept deliberately simple
> — footprint assignment is low-stakes (a trivially-overwritten map entry).

- Input: `{ assignments: {refdes: lib_id} }` (required).
- Behavior: merges the assignments onto the existing map and writes it. The **one** check is
  that each `lib_id` is a real footprint (via the footprint index) — an unknown one would
  silently break the board downstream. Unknown lib_ids are returned in `unknown` with
  suggestions and **skipped**; the valid ones are saved (partial success).
- Output: `{ ok, footprints: {refdes: lib_id}, unknown?: [{reference, lib_id, suggestions}] }`.
- **Gaps live in `derive_board`**, not here — it joins the map against the schematic's parts
  and reports any part still missing a footprint. The agent typically assigns all parts in
  one call (it knows them), and `derive_board` catches anything missed.
- For a **user-supplied** sheet, an optional `kicad-bridge` in-place patch can write the
  `Footprint` property back onto each `(symbol …)` (a targeted s-expr edit, **not** a re-emit)
  so KiCAD shows the assignment — a later slice, never touching wires/placement/connectivity.

> Deferred to `derive_board`: the symbol-pin-count ⟷ footprint-pad-count validation (it needs
> the `Design`, which `derive_board` already has).

### `derive_board`

> Build (or update) the board draft from the schematic's parts + netlist, joining in the
> footprint assignment map. The LLM supplies only the outline and rules. Replaces the
> hand-typed `create_board` parts array for the schematic-driven path.

- Input: `{ bounds, rules?, source?: "draft"|"sch", overwrite?: bool, commit?: bool }`.
  - `source` — `draft` (compiled YAML draft) or `sch` (lift the project `.kicad_sch`);
    default: `sch` if the file exists, else `draft`.
- Behavior:
  1. Obtain connectivity: compile the draft `Design`, **or** `lift()`→`compile()` /
     parse the `kicad-cli` netlist for a user sheet.
  2. For each component → one `DraftPart`: `footprint` from the assignment map (else report
     as an unresolved gap → caller runs `assign_footprints`), `pad_nets` from the pins via
     the pin→pad rules below.
  3. Diff against any existing `BoardDraft` (see Sync) and write.
- Output: `{ parts: N, nets: M, footprints: "N/N", unresolved:[…], diff:{added,removed,
  rewired}, warnings:[…] }`. **Gated** like `apply_design`.

### Sync / drift (re-run semantics)

`derive_board` is idempotent and reconciles on re-run (refdes is the identity, matching
`apply_design`'s reconciliation):

- **added** parts → appended, placed on the next `place_board`.
- **removed** parts → dropped (and from any placement).
- **rewired** nets → `pad_nets` updated.
- **unchanged** parts → **placement preserved** (don't disturb a laid-out board for an
  unrelated schematic edit); only new parts need placing.

## Pin → pad mapping

KiCAD convention: **symbol pin number = footprint pad number**, and `lift`/the netlist key
pins by number. So `Component.pins` (and netlist `<node pin=..>`) map directly to
`pad_nets` (keyed by pad number). Edge cases:

1. **Multi-unit parts** — flatten `Component.units` onto the single physical package before
   mapping; all units' pins share the one footprint's pads.
2. **Pin# ≠ pad#** (rare connectors) — validate symbol pin count vs footprint pad count;
   surface a mismatch as a recoverable error rather than guessing.
3. **No-connect / unmentioned pins** — left out of `pad_nets` (a pad absent from the map is
   unconnected, matching `create_board` semantics). `unconnected-*` nets are dropped.

## Consistency lint (the missing oracle)

A check that compares the board's net partition against the schematic's `Design` and
reports drift (a part/net on the board that the schematic doesn't have, or vice-versa).
Run inside `derive_board` (so a fresh derive is consistent by construction) and offered as
a gate before `export_board`, closing the "nothing verifies board ⟷ schematic" gap.

## Worked examples

**Scenario 1 — full e2e**
```
search_symbols → create_design(YAML)        part: only, no footprints (unchanged)
apply_design(commit)            ── GATE ──   write .kicad_sch + ERC (Footprint stays "")
search_footprints → assign_footprints        refdes → lib_id → .autopcb/footprints.json (not gated)
derive_board({bounds, rules})   ── GATE ──   parts+nets from Design, footprints from the map;
                                             reports any part still missing a footprint
place_board → route_board → export_board
```

**Scenario 2 — bring-your-own `.kicad_sch`**
```
read_schematic / lift(user.kicad_sch)        Design (parts, pin#→net, any pre-assigned footprints)
search_footprints → assign_footprints        fill missing → map (optional in-place patch to user sheet)
derive_board({bounds, rules})   ── GATE ──   identical to S1
place_board → route_board → export_board     user's connectivity untouched
```

## What does NOT change

- **`sch-layout`** — untouched. `derive_board` calls `lift()`; `assign_footprints` patches
  via `kicad-bridge`. Emit keeps writing an empty `Footprint` property.
- **`create_design`** — keeps authoring symbols + nets only; no footprint guidance added.
- **`create_board`** — stays as the explicit-parts escape hatch for board-only / no-
  schematic use. `derive_board` is additive, not a replacement.
- **The "LLM never emits coordinates" contract** — preserved; `derive_board` adds parts +
  nets, never positions.

## Suggested implementation slices

1. **Footprint map + `assign_footprints`** (board-side store, suggestions, gate). No
   derive yet — proves footprint selection end-to-end.
2. **`derive_board` from the draft `Design`** (Scenario 1), reusing `create_board`'s
   `BoardDraft` builder with `pad_nets` from `Component.pins`.
3. **`source: "sch"`** — derive from a user `.kicad_sch` via `lift`/netlist (Scenario 2) +
   the in-place footprint patcher in `kicad-bridge`.
4. **Sync/diff re-run semantics** (placement preservation) + the **consistency lint**.

Each slice is gated on `cargo test -p pcb-engine -p kicad-bridge -p agent` and the
`board_harness` staying 0-copper-fault.

## Open questions

- Footprint suggestion ranking: symbol value + pin count is the obvious key; do we also use
  the symbol's own `Footprint Filters` (KiCAD symbols carry fpFilters) when present?
- For Scenario 2, do we *always* back-patch footprints into the user's sheet, or only on
  request? (Default proposed: only on request, to keep their file pristine.)
