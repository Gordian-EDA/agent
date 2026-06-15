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
- Layout hints used `layout: {edge: left|right|top|bottom}`, but the engine is a
  left→right column flow, so `top`/`bottom` silently did nothing.
- `between: [a, b]` is fine for a resistor but **ambiguous for a polarized part**
  (which slot is the anode?), forcing an escape hatch (`pins: {A: …, K: …}`).

The guiding split: **you author intent (objects + hints); the engine generates
geometry (placements, wires, *and* the power-symbol / port-label glyphs).** A
power symbol is a glyph the engine sprays from "this net is power" — exactly like
wires are glyphs it draws from "these pins share a name."

## The model

A design is:

- **Components** — physical parts (footprint, BOM line, one instance each).
- **Power nets** — declared once; drawn as power-symbol glyphs at *every* tap.
- **Ports** — board I/O; drawn as labels at a sheet edge.
- **Layout hints** — where parts go.
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

### `layout:` — part placement

```yaml
layout:
  left:  [U2]                 # pin toward the LEFT of the left→right flow
  right: [J2, J3]             # ... toward the RIGHT
  near:  {U1: [C1, C2, Y1]}   # keep these ADJACENT to U1
```

- `left` / `right`: a list of refdes or block names pinned toward that side. These
  are the only two values the horizontal column flow can honor. (Internally:
  biases the anchor column order.)
- `near: {<anchor>: [<parts>…]}`: place the listed parts next to `<anchor>` — a
  cap/crystal pinned into that chip's column area (overrides the default
  "tap nearest pin", so it groups even across nets), another IC into the adjacent
  column. `near` inherits the anchor's side. A part should appear in **one** of
  `left`/`right`/`near`; `near` wins on conflict.

Replaces the per-block `layout: {edge: …}`. **No `top`/`bottom` for parts** — they
were never honored; deleted. (A true top/bottom band, `flow: tb`, and semantic
block roles like "filter" are deferred — they are real `infer_ir` layout features,
not syntax. See *Deferred*.)

### `blocks:` / `components:` — the parts

```yaml
blocks:
  power:
    components:
      J1: {part: Connector_Generic:Conn_01x02, pins: {1: VIN, 2: GND}}
```

Unchanged: blocks group parts; refdes are unique design-wide. A component is
`{part, value?, footprint?, dnp?, props?, pins?|between?|positive/negative?,
units?, decouple?}`.

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

# ports: {}   # this board has no off-sheet I/O (9V enters via J1, LED is on-board)

layout:
  left: [power]          # the input module hugs the left edge
  near: {U1: [C3]}       # keep the 555's decoupling cap beside it

blocks:

  power:                 # 9 V input + reverse protection + bulk
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
      C3: {part: C, value: 100nF, between: [+9V,   GND]}              # decoupling
      C4: {part: C, value: 10nF,  between: [N_CV,  GND]}              # CV bypass
      R3: {part: R, value: 1k,    between: [N_Q,   N_LED]}
      D2: {part: Device:LED, value: red, positive: N_LED, negative: GND}
```

## Migration (from v1)

- `rails: [...]` → `power: [...]`. Delete every `nets: {N: {power: true}}`.
- Port nets → a `ports:` entry (`OUT: {port: right}` in v1 plans → `ports: {OUT: right}`).
- Per-block / per-part `layout: {edge: …}` → the top-level `layout: {left,right,near}`;
  drop any `edge: top|bottom` (auto-places).
- Polarized parts using `between:` (or `pins: {A:…,K:…}`) → `positive:`/`negative:`.
- The `nets:` section survives only for the rare `class:` attribute; drop it if unused.

Migrate fixtures: divider-filter, mcp1703-power-entry, 555-blinker,
uart-level-translator, bedrock-oneshot/selfrepair-bluepill, rf-lna-frontend,
mixed-signal-adc-frontend, bga-fpga-ice40. Reference renders must stay
byte-identical (the four tuned targets have no hints; their `rails:`→`power:` and
diode/LED polarity rewrites must not change geometry).

## Deferred (explicitly not in v2)

- A true **top/bottom band** for parts (place a block above/below the main row).
- **`flow: tb`** — a vertical layout mode.
- Semantic **block roles** ("treat this block as a filter") → idiom templates in
  `infer_ir`.
- `near` between two *satellites* (only anchor-relative for now).

These are `infer_ir` layout features, each its own pass — not syntax.
