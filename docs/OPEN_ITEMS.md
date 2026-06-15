# Open items

Living backlog for the floorplan engine + circuit-lang. Pre-release; breaking
changes are welcome when they make things cleaner. Last swept 2026-06-15.

Validate any engine change with the harness (render `render_targets` →
`/tmp/renders/ours-*.png`, judge with fresh sub-agents vs
`docs/validation/references/*.png`, gate on the netlist oracle). See CLAUDE.md
*Visual review* and the `floorplan-validation-harness` memory.

---

## 1. Features (designed, not built)

### 1.1 circuit-lang v2 YAML — **biggest item**
Design: `docs/superpowers/specs/2026-06-15-circuit-lang-v2-design.md` (committed
`a26e943`). Implement:
- **model + parser** for `power:` (power nets as first-class symbol glyphs,
  replaces `rails:` + `nets:{power:true}`), `ports:` (`<net>: <edge>`),
  `layout: {left, right, near}`.
- **polarity terminals** `positive:` / `negative:` for 2-pin polarized parts,
  mapped to the symbol's anode/cathode (or `+`/`−`) pins by the compiler.
- **enforcement errors**: `between:` on a polarized part →
  `"D2 is polarized — use positive/negative"`; `positive`/`negative` on a
  symmetric part → `"R1 is not polarized — use between"`.
- wire `power`/`ports`/`layout` into `infer_ir`.
- **migrate all 8 fixtures** to v2; the four reference renders must stay
  **byte-identical** (`rails:`→`power:` and diode/LED polarity rewrites must not
  move geometry).

### 1.2 Layout hints still unwired in `infer_ir`
`infer_ir` is a left→right column flow; these v2/earlier hints are parsed-or-
designed but not honored yet:
- **`layout.near`** — block/part adjacency (cap/crystal pinned into a chip's
  column). The single highest-value unwired hint.
- **true top/bottom band** — place a block above/below the main row (today
  top/bottom approximate to middle columns; v2 drops them for parts entirely).
- **`flow: tb`** — a vertical layout mode.
- **semantic block roles** — "treat this block as a filter" → idiom templates.
- **`near` between two satellites** — anchor-relative only for now.

---

## 2. INFER quality — retire the hand sidecar

`infer_ir` is the production default (no LLM frame). To make INFER match the
references and drop the `place`/`ports`/`mirror` sidecar, the ranked plan from
the gap-analysis workflow (`tasks/wvtpl2fpe.output`, run `wf_6c827ceb-234`):

**Batch A — INFER-only, zero sidecar risk** (the sidecar path never runs this
code):
- **A1** promote named multi-pin signal nets to ports — fixes INFER divider
  **missing OUT label** and uart TXD1/RXD1 labels. Prefer the structural signal
  (net reaches a downstream IC pin / incident only to passives) over the name
  heuristic; skip `N$`/anonymous nets.
- **A2** geometry-driven port side (not the `net_is_input` substring).
- **A3** loosen `anchor_tap` to resolve on a single distinct **anchor**, not a
  single pin — kills 555 sprawl.
- **A4** decoupling caps flank their IC supply pin, not `spare_col`
  (mcp1703/uart).
- **A5** decide mirror **before** satellite placement (uart A/B side).
- Known INFER defect to clear alongside: mcp1703 warns `F1 overlaps field U1`.

**Batch B — shared risk** (re-validate all 4 sidecar renders):
- re-add IC field-text reservation in `item_rect` (reverted in round 4 because it
  perturbed the anneal — anneal is OFF by default now, so it may be safe).

---

## 3. Tracked correctness bugs (`KNOWN_TRUTHFULNESS_BUGS`)

The oracle's challenge tier carries these as documented-failing; it auto-fails if
one silently starts passing (remove it from the list when fixed).

- **#21 / `bga-fpga-ice40`** — many/multi-unit power pins don't all reach their
  single rail. The FPGA's GND balls span 5 units; rail routing leaves GND
  fragmented into ~12 nets, one of which shorts onto `1V2`. **Fix:** rail routing
  must connect *every* power-net pin across *all* units to its one rail. (Not a
  positional merge — units render in a non-overlapping row.)
- **`bedrock-selfrepair-bluepill`** — `baseline_ir` adjacent-rail short.

---

## 4. Cleanup / hygiene

- **`lift` `ap_*` vestige** — `crates/sch-layout/src/lift.rs` still reads the
  hidden `ap_block`/`ap_role`/`ap_parent`/`ap_index` reconciliation tags that the
  modern `emit` no longer writes. Dead on modern input, but `lift` is live
  (`apply_design` diff), so it needs a focused follow-up (drop the tag reader or
  re-emit the tags it depends on).
- **Challenge oracle is slow (~9 min)** — polish is bounded (3 iters / 2
  free_nudge rounds) to keep it tractable; references still render in ~1.3 s.
  Revisit if the challenge set grows.
- **uart clutter (review T1/T4)** — J1 `Conn_01x05` text overlaps the pin-3/4
  no-connect X markers (cheap win in `solve_text_positions`); the R13/R15
  termination cluster is busier than the human reference (pull tighter to the IC).
- **Pre-existing, NOT an engine bug** — `cli_netlist` test fails on this KiCAD
  9.0.2 env (old-format `rc_pair.kicad_sch` won't load). Leave as-is or
  regenerate the fixture.
