# circuit-lang v2 — YAML redesign

Status: **design** (not yet implemented). Pre-release; breaking changes are fine.

## Why

The current YAML overloads one primitive — the **net** — with jobs that humans
think of as different *objects*, and hides the objects that matter:

- Power (`GND`, `+5V`) is the abstract net name `between: [VCC, OUT]`, with its
  power-ness declared *elsewhere* (`rails:` **and** `nets: {power: true}` — two
  ways to say the same thing). But a human draws power as a **symbol** they drop
  on a pin.
- Board I/O ("ports") are inferred from single-pin nets (fragile — misses the
  divider's multi-pin `OUT`) or buried as a net attribute.
- Layout hints **over-specified**. `layout: {edge: top|bottom}` silently did
  nothing (the engine is a left→right flow), and worse, `near: {U1: [C1]}` asked
  the human to hand-place things the optimizer **already knows from the netlist**:
  a decoupling cap *is* the 2-pin part bridging U1's VCC↔GND; a crystal *is* the
  part on the OSC pins. The graph says they're adjacent — making the author
  restate it is noise. A hint should carry **only what the graph can't encode**.
- `between: [a, b]` is fine for a resistor but **ambiguous for a polarized part**
  (which slot is the anode?), forcing an escape hatch (`pins: {A: …, K: …}`).

The guiding split: **you author intent (objects + hints); the engine generates
geometry (placements, wires, *and* the power-symbol / port-label glyphs).** A
power symbol is a glyph the engine sprays from "this net is power" — exactly like
wires are glyphs it draws from "these pins share a name."

And the sharp line on hints: **the optimizer holds the full netlist + symbol
library, so everything *local and geometric* is derivable** (adjacency,
clustering, series spines, rail-tap orientation, mirroring, port sides). The one
thing a graph fundamentally **cannot** encode is **global arrangement** — a
netlist has no intrinsic up/down/left/right, so "connectors around a central MCU,
power on the left" is genuine human intent. That is the *only* thing a hint
carries, and its natural shape is a **coarse 2D grid of the skeleton** (the
`layout:` array). See *What the optimizer infers* below.

## The model

A design is:

- **Components** — physical parts (footprint, BOM line, one instance each).
- **Power nets** — declared once; drawn as power-symbol glyphs at *every* tap.
- **Ports** — board I/O; drawn as labels at a sheet edge.
- **Layout grid** — an optional 2D array placing the *skeleton* (which anchors go
  where). Everything ungridded the optimizer arranges itself.
- **Signal wires** — *implicit*: pins that share a name are connected. No syntax.

Power symbols and ports are **not** components — they have no footprint/BOM and
you author zero instances; the engine emits as many glyphs as the taps require.

## Syntax

### `power:` — power nets (the rails)

```yaml
power: [+9V, +3V3, GND]
```

A list of net names that are power. Naming one in a connection (`between`,
`positive`/`negative`, `pins`) taps that rail and the engine draws a power-symbol
glyph there. Replaces `rails:` and removes `nets: {power: true}` (there is now
exactly one way to declare power). Multiple rails (a complex board's
`+5V`/`+3V3`/`VCCA`/`VCCD`/…) is just a longer list; each is its own glyph,
sprayed at every pin that taps it.

### `ports:` — board I/O

```yaml
ports:
  OUT:  right
  TXD1: right
  CLK:  left
```

`<net>: <edge>` where `<edge>` is `left | right | top | bottom`. Declares a net is
a board port whose hierarchical-label glyph exits that sheet edge. All four edges
are honest here (a *label* genuinely sits on any edge, unlike a part). Declaring
a port also *forces* port-ness, fixing the multi-pin inference gap (the divider's
`OUT`). A single-pin signal net with no entry still auto-infers (input-ish name →
left, else right).

### `layout:` — the placement grid

Position is a **2D grid**. A top-level `layout:` is an array of rows; each row is
a left→right list of cells. Row index is the vertical position (top→bottom),
column index the horizontal (left→right):

```yaml
layout:
  - [J1, MCU, USB]
  - [J2, MCU]
```

This reads: `J1` top-left, `MCU` top-centre, `USB` top-right; `J2` below `J1`,
and `MCU` again below-centre. It is a **relatively rigid** constraint — the engine
honors the grid *topology* (what is left-of / above what) but computes the exact
spacing, orientation, mirroring, and wiring itself.

- **A cell names a block or a component** (resolved block-first). A *block* cell
  expands to all that block's parts, kept together in that grid region.
- **Ordinal, not metric.** Column = x order, row = y order; a skipped index
  reserves no space. Rows may be **ragged** and align by column index — above,
  `J2` sits under `J1`, the second `MCU` under the first. Use `~` (YAML null) for
  a deliberate hole: `[A, ~, B]` leaves the middle column empty.
- **A name repeated across cells floats.** `MCU` occupies column 1 of *both* rows,
  so the engine places it **once** and floats it — here vertically, to sit
  adjacent to everything that wires to it (the central-hub idiom, without pinning
  its row). General rule: an entry appearing in N cells spans their bounding
  region and the optimizer floats it within that box.
- **Only the skeleton goes in the grid.** Pin the structural anchors (ICs,
  connectors, modules); leave the rest out. Every ungridded part (decoupling caps,
  passives, crystals) is placed by inference next to the gridded anchor it wires
  to. Keep the grid **sparse**.

**No `layout:`?** Blocks flow left→right in **declaration order** (a single row)
and the engine infers the rest from connectivity. The grid is the override for
when you want explicit 2D control — you reach for it on a board with a real
floorplan (connectors around a central MCU), not a 6-part filter.

This **replaces** `side` / per-part `edge` / `near` outright: the grid states
position directly, so there is nothing left to bias. Blocks no longer carry any
layout field; placement is the grid's job (or declaration order's).

### What the optimizer infers — *never* hint these

The optimizer holds the full netlist + symbol library. Anything it can derive,
you must **not** restate — restating it is noise that can only go stale or
conflict. It already infers, from incidence and pin geometry alone:

- **Adjacency** (the old `near`). A 2-pin part that bridges an IC's pins is placed
  next to it: a **decoupling cap** (between a supply pin and GND), a **crystal**
  (on the OSC pins), a **series resistor** on a signal pin. The graph already says
  "these share a pin" — adjacency falls out of it.
- **Clustering.** Caps tapping one supply pin flank that pin; a block's parts stay
  grouped.
- **Series spines / collinearity.** Degree-2 chains (a divider's R7→R8) are laid
  collinear.
- **Orientation.** Series parts run horizontal, rail taps vertical — classified by
  rail incidence.
- **Mirror / flip.** Which way a symbol faces, from which side its nets exit.
- **Port side** for an obvious single-pin signal (an input-ish net → left). An
  explicit `ports:` entry overrides this.
- **Intra-module arrangement** — the relative positions of parts *within* a block.

Litmus test for any proposed hint: *could the optimizer compute this from the
netlist + symbols?* If yes, it is **not** a hint — fix the inference instead.

### `blocks:` / `components:` — the parts

```yaml
blocks:
  power:
    components:
      J1: {part: Connector_Generic:Conn_01x02, pins: {1: VIN, 2: GND}}
```

A block purely **groups** parts (and names a unit the `layout:` grid can place by
block name); refdes are unique design-wide. Blocks carry **no** layout field. A
component is `{part, value?, footprint?, dnp?, props?,
pins?|between?|positive/negative?, units?, decouple?}` — also no layout field
(placement is the grid's job, or declaration order's).

### 2-pin connections: symmetric vs polarized

A 2-pin part connects either way, but the syntax depends on whether the pins are
interchangeable:

```yaml
# SYMMETRIC (R, L, ceramic C, switch, fuse) — order is meaningless
R1: {part: R, value: 4k7,  between: [+9V, N_DIS]}
C2: {part: C, value: 10uF, between: [N_RC, GND]}

# POLARIZED (diode, LED, polarized/tantalum cap) — name the terminals
D1: {part: Device:D,   value: 1N4007, positive: VIN,   negative: +9V}   # anode→VIN
C1: {part: Device:CP,  value: 100uF,  positive: +9V,   negative: GND}
D2: {part: Device:LED, value: red,    positive: N_LED, negative: GND}
```

- `between: [a, b]` — symmetric sugar; the two nets map to the part's two pins
  arbitrarily (fine, they're interchangeable).
- `positive:` / `negative:` — the compiler maps `positive` to the symbol's
  +/anode pin and `negative` to its −/cathode pin. You never need to know whether
  the part names them `A`/`K` or `1`/`2`. Full words for consistent style.
- **Enforcement.** The compiler knows polarity from the symbol (a diode has
  anode/cathode, a polarized cap a `+` pin). It is an error to use:
  - `between:` on a polarized part → `"D2 is polarized — use positive/negative"`,
  - `positive`/`negative` on a symmetric part → `"R1 is not polarized — use between"`.

  No silent wrong-way-round diodes. (Multi-pin parts still use named `pins:`,
  which is already unambiguous — this is 2-pin sugar only.)

## Complete example — power + 555 blinker

```yaml
version: 1
name: 555-blinker

power: [+9V, GND]

# ports:  {}   # no off-sheet I/O (9V enters via J1, LED is on-board)
# layout: ...  # omitted — only two blocks, so declaration order (power, then
#              # blinker) lays them left→right; no grid needed.

blocks:

  power:                 # 9 V input + reverse protection + bulk (declared first → left)
    components:
      J1: {part: Connector_Generic:Conn_01x02, pins: {1: VIN, 2: GND}}
      D1: {part: Device:D,  value: 1N4007, positive: VIN, negative: +9V}  # reverse-polarity
      C1: {part: Device:CP, value: 100uF,  positive: +9V, negative: GND}  # bulk

  blinker:               # 555 astable + LED
    components:
      U1:
        part: Timer:NE555P
        pins:
          VCC: +9V
          GND: GND
          R:   +9V       # reset held high
          DIS: N_DIS
          THR: N_RC
          TR:  N_RC      # trigger tied to threshold → astable
          CV:  N_CV
          Q:   N_Q
      R1: {part: R, value: 4k7,   between: [+9V,   N_DIS]}
      R2: {part: R, value: 10k,   between: [N_DIS, N_RC]}
      C2: {part: C, value: 10uF,  between: [N_RC,  GND]}              # timing
      C3: {part: C, value: 100nF, between: [+9V,   GND]}  # decoupling → auto-placed by U1
      C4: {part: C, value: 10nF,  between: [N_CV,  GND]}              # CV bypass
      R3: {part: R, value: 1k,    between: [N_Q,   N_LED]}
      D2: {part: Device:LED, value: red, positive: N_LED, negative: GND}
```

## Migration (from v1)

- `rails: [...]` → `power: [...]`. Delete every `nets: {N: {power: true}}`.
- Port nets → a `ports:` entry (`OUT: {port: right}` in v1 plans → `ports: {OUT: right}`).
- Per-block / per-part `layout: {edge: …}` and every `near:` hint → **deleted**.
  If a board needs explicit arrangement, write a top-level `layout:` grid naming
  the blocks/anchors; otherwise rely on declaration order. Adjacency/clustering/
  intra-block order are inferred — if a part lands wrong, fix the inference, don't
  add a hint. (The four references have no grid and must stay byte-identical, so
  their inferred placement is the regression gate.)
- Polarized parts using `between:` (or `pins: {A:…,K:…}`) → `positive:`/`negative:`.
- The `nets:` section survives only for the rare `class:` attribute; drop it if unused.

Migrate fixtures: divider-filter, mcp1703-power-entry, 555-blinker,
uart-level-translator, bedrock-oneshot/selfrepair-bluepill, rf-lna-frontend,
mixed-signal-adc-frontend, bga-fpga-ice40. Reference renders must stay
byte-identical (the four tuned targets have no hints; their `rails:`→`power:` and
diode/LED polarity rewrites must not change geometry).

## Deferred (explicitly not in v2)

- **`flow: tb`** — a top→bottom variant of the whole grid (transpose the axes).
  The grid already gives rows *and* columns, so multi-row floorplans work today;
  `flow` only flips which axis is the dominant signal direction.
- Semantic **block roles** ("treat this block as a filter") → idiom templates in
  the grid inference.
- A **span-region float** richer than a single repeated anchor (e.g. an anchor
  declared to straddle three columns) — for v2, repetition across cells is the
  only float.

These are inference / engine features, each its own pass — not new syntax. Note
`near` and `side` are **not** here: they are deliberately gone. `near` is
subsumed by adjacency inference; `side` by the grid (and declaration order). If
placement comes out wrong with no grid, the fix is a better inference rule, never
a per-part hint.
