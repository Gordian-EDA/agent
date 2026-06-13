# Floorplan engine — human-style schematic layout

## Goal

Make the schematic engine produce professional, human-style layouts that match
the references in `docs/validation/references/` for four cases — `divider-filter`,
`mcp1703-power-entry`, `555-blinker`, `uart-level-translator` — judged by topology
and quality when viewed zoomed to content. The `logic-board-spaghetti` reference
is an explicit non-goal.

The LLM agent that writes circuit-lang YAML must never specify coordinates. It may
emit *rough semantic guidelines* (which net is a rail, where the ICs go, which nets
exit). All exact geometry is the engine's job.

## Why the current output looks auto-generated

Four systematic gaps (see `.superpowers/brainstorm/.../diagnosis.html`):

1. **Power**: one power symbol stamped per power pin → arrow/triangle confetti.
   Humans use shared **rails** (V+ wire across the top, GND across the bottom).
2. **Flow**: local idiom packing in a band; no global left→right signal / top→bottom
   power direction; the IC is not the centered anchor.
3. **Wires vs labels**: router gives up across blocks → net-label bombing.
4. **Framing**: content-fit bbox rendered on a full A4 sheet → tiny in the corner.

The current `grammar.rs` carries ~20 implicit heuristics (RailRail/ToRail/Series
classification, anchor taps, ground-pin flips, banks). That complexity is the smell
to remove.

## Architecture: two layers

```
circuit-lang YAML (connectivity + rails:, between:; zero coordinates)
   │
   ▼  Layer 1 — FLOORPLAN (LLM subagent via Bedrock, or a deterministic baseline)
   │   emits the Layout IR (four keys), geometry-free
   ▼
   │  Layer 2 — GEOMETRY COMPILER (deterministic Rust)
   │   IR + netlist + symbol pin geometry → exact grid coords, rails as real
   │   wires, orthogonal routing, minimal power symbols, labels/ports, framing
   ▼  .kicad_sch
```

- **Layer 1** lives in the `agent` crate (it calls Bedrock). `sch-engine` stays
  pure/async-free. `sch-engine` also ships a **deterministic baseline IR**
  generator so it renders without an LLM and so the compiler is testable from
  pinned fixtures.
- **Layer 2** is a new `floorplan` module in `sch-engine`, built on the existing
  `SchematicWriter` (`add_symbol_full`, `pin_dirs`, `add_wire`, `add_junction`,
  `add_power_symbol`, `add_power_flag`, `add_signal_label`, `add_rect`, `finish`)
  and `route.rs`. It **replaces** `grammar.rs` + `cluster_geom.rs` + `place.rs`
  as the layout path. `emit.rs`, `grid.rs`, `ids.rs`, `route.rs` are reused.

## The Layout IR — the four-key language

Minimal and elegant. The LLM emits only the *frame*; a small fixed rule set fills
in the rest from connectivity.

```yaml
flow:  lr                            # lr | tb. Global signal direction. Default lr.
rails: { "+5V": top, "+3V3": top, GND: bottom }   # net -> band (top | bottom)
place: { U1: [1, 0], MCU: [2, -1] }  # refdes -> coarse unitless cell [col, row].
                                     # Usually only ICs; ANY refdes may be pinned
                                     # to override inference. Never millimetres.
ports: { OUT: right, "5V_BUS": left }            # net -> edge side (left|right|top|bottom)
```

Rust model (in `sch-engine::floorplan`):

```rust
pub enum Flow { Lr, Tb }
pub enum Band { Top, Bottom }
pub enum Side { Left, Right, Top, Bottom }
pub struct Cell { pub col: i32, pub row: i32 }
pub struct LayoutIr {
    pub flow: Flow,                       // default Lr
    pub rails: IndexMap<String, Band>,    // net -> band
    pub place: IndexMap<String, Cell>,    // refdes -> coarse cell
    pub ports: IndexMap<String, Side>,    // net -> edge side
}
```

The IR is small enough to serialize as JSON for the subagent's structured output
and to hand-author as a checked-in fixture per case.

## The five inference rules (the only "magic")

Applied by the geometry compiler over the netlist, given the IR's rails/place/ports:

1. **Rail stub**: a pin on a rail-net gets a vertical stub up/down to that rail's y.
2. **Rail-to-rail 2-pin part** (one pin on top rail, one on bottom rail) →
   **vertical**, top-rail pin up, hung below its rail; multiple such parts on the
   same rail span spread along the rail (decoupling rows, divider legs).
3. **Anchor tap**: a 2-pin part tapping a single placed IC pin sits **on that pin's
   side**, oriented along the pin.
4. **Series run**: a chain of 2-pin parts on shared private nets is laid along the
   route between its endpoints (along `flow`).
5. **Anchor ordering**: ICs without a `place` cell are ordered along `flow` by net
   adjacency. Cells resolve to columns/rows; column x and row y are sized to the
   widest/tallest occupant plus spacing.

If a circuit needs something the rules don't give, the LLM pins the part in
`place` — same primitive, no new grammar.

## Geometry compiler stages (Layer 2)

1. **Resolve placement**: assign every component an `(at, angle)`.
   - Anchors → cell → coordinate (grid of columns × rows, each cell sized to fit).
   - Passives → inference rules 2–4, relative to rails / anchor pins.
   - Snap all to the 1.27 mm grid.
2. **Place symbols** via `add_symbol_full`; record positions.
3. **Build rails**: for each rail net, draw a horizontal wire at the band y spanning
   the x-range of its pins; stub every rail pin to it (rule 1); one `PWR_FLAG`
   per rail (not per pin). Drop power symbols only where a rail would be visually
   worse than a local power symbol (e.g. a lone GND on a divider leg) — decide by
   pin count on the net.
4. **Route signal nets**: build per-net MST, route edges with `route::route_edge`
   (orthogonal, obstacle-aware), add junctions at ≥3-way joins. Wires preferred;
   label fallback only for nets the IR marks as ports or that fail routing.
5. **Ports & labels**: nets in `ports` → a signal label/flag at the named edge with
   a wire to it. Otherwise minimal labels.
6. **Frame**: sheet auto-sized to content (pick the smallest standard size, or a
   custom page slightly larger than the content bbox) so the drawing fills the
   view. Drop the dashed block-boundary box unless the block has a note/title worth
   showing.

## Orientation

`add_symbol_full` currently hardcodes `mirror:false`; orientation is via `angle`
(0/90/180/270). Device:R/C at angle 0 are vertical with pin 1 on top. Horizontal =
angle 90. `between: [topNet, bottomNet]` already encodes which pin is the "top"
pin, so vertical orientation needs no new hint. Mirror support (for the symmetric
uart B-side) is added to `add_symbol_full` only if the uart case needs it.

## Layer 1 subagent (after the compiler matches via fixtures)

- New layout subagent in `agent` crate: given the netlist summary (components,
  nets, rails, each symbol's pin sides) it returns the four-key IR as a Bedrock
  tool-call (structured JSON), model = `AGENT_MODEL` (default Opus). One self-repair
  retry if the IR references unknown nets/refdes or yields a failed compile.
- A deterministic baseline IR generator in `sch-engine` (rails from `design.rails`
  with a top/bottom heuristic, ICs auto-ordered, no ports) is the fallback and the
  test default.

## Testing & acceptance

- **Unit**: IR (de)serialization; each inference rule's outcome on a small netlist;
  rail builder (wire spans, single flag per rail); port placement.
- **Integration per case**: hand-authored IR fixture → compile → assert ERC-clean
  (existing `kicad-cli sch erc` path) + structural facts (e.g. divider: R7 above R8,
  both vertical, OUT label at right edge; mcp1703: GND rail spans all cap bottoms,
  one GND flag).
- **Visual**: `cargo run -p agent --example render_validation` renders all four to
  PNG; compared by eye against `docs/validation/references/`. Done when all four
  match in topology and quality zoomed to content.
- Determinism is no longer a hard requirement (aesthetics-first); the geometry
  compiler stays deterministic given an IR, and the subagent's IR per case is pinned
  for regression.

## Out of scope

- `logic-board-spaghetti` (dense multi-IC board).
- Reproducing the exact KiCAD symbol variant a human picked (e.g. the specific 555
  symbol with logical pin grouping); matching topology/aesthetics is the bar.
