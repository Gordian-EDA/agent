# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Breaking
- **The live `.kicad_sch` file is now the sole schematic source of truth.** The
  YAML authoring language, draft workspace, whole-sheet apply workflow,
  multisheet composer, and netlist-lift crate were removed. New designs and
  multi-part additions use one connectivity-only `place_parts` call; focused
  edits use guarded live mutators, with every write individually approved.
- **Schematic completion is deterministic.** A clean `check_schematic` after a
  successful mutation satisfies the turn contract immediately, and reviewed
  turns use those facts instead of a visual or semantic LLM critic.

### Fixed
- **Queued prompts could interleave out of order.** `AgentEvent`s (a turn's
  final reply, `TurnDone`) and its completion signal (which drains the queue
  and spawns the next turn) travel on two separate channels; a bare
  `tokio::select!` doesn't preserve ordering across channels, so an unlucky
  poll could process the completion signal — and start the next queued turn —
  before the previous turn's own reply had been added to the transcript.
  Queuing several prompts back to back made this easy to hit. The event loop
  now polls `biased`, which (given the task already guarantees it sends all
  its events before signaling completion) deterministically drains a turn's
  events before ever touching its completion signal.
- **A completed turn could show nothing for a real reply.** Paragraph-gated
  streaming (see below) buffers assistant text until a blank line closes a
  paragraph or the turn finalizes it. `TurnDone` used to just drop that buffer
  outright, so a short reply with no blank line in it — the common case — went
  missing whenever the turn ended without a clean finalizing `AssistantText`
  (a provider quirk, not something the UI can assume never happens). `TurnDone`
  now flushes whatever is still buffered instead of discarding it.

### Changed
- **The status-bar model label is no longer clipped to three words**: it used
  to cut every model id down to its first three dash-separated segments after
  stripping the vendor namespace, which mangled short ids (`gpt-6-luna` showed
  as `6-luna`). Only the namespace is stripped now; the footer's existing
  ellipsis discipline already handles a bar too narrow to fit it.
- **Welcome splash names the project**: the tagline now reads "the schematic &
  PCB design copilot · ~/path/to/project" — the schematic's directory, tildified
  under `$HOME` — so it's clear at a glance which project a session is open on.
- **The help overlay is borderless and full width**, joining the completion
  and unwind menus in dropping the rounded box and title bar. It used to be a
  narrower card centred over the transcript; without its border to mark the
  edge, that left the busy transcript peeking down both margins with nothing
  separating the two, reading as corruption rather than a deliberate gap.
- **The floating menus are borderless**: the `/command` completion list and the
  unwind picker drop their rounded box, title, and selection caret. They are now
  full-width lists seated on the composer, with the selected row as a solid
  accent bar that is legible from anywhere along it.
- **TUI transcript view**: the `↑n` scrollback badge is replaced by a proportional
  scrollbar in the right-hand gutter, whose thumb takes the accent while scrolled
  back and recedes at the tail. `End` (on an empty prompt) and `Ctrl-End` (always)
  jump to the latest output, and an off-tail hint makes that discoverable.
- **Assistant prose renders a paragraph at a time** rather than token by token.
  A partial paragraph stays buffered until it is finished, so a sentence never
  reflows on screen as it is written; the running indicator carries the in-flight
  signal instead. The trailing live cursor is gone with it.
- **TUI colour scheme**: every styled span now names a semantic role from a single
  `tui::theme` module instead of a hardcoded ANSI colour. The palette is a warm dark
  ramp with two accent tiers — copper (`#d98b4a`) for the rare, high-salience surfaces
  (brand, user caret, composer focus, selection) and teal (`#5fb3b8`) for the frequent
  structural ones (tool names, headings, inline code) — replacing the single overloaded
  cyan. The screenshot harness renders from the same constants, so previews can no
  longer drift from what ships.

### Added
- Initial open-source release scaffolding: Apache-2.0 `LICENSE`, `README`, `CONTRIBUTING`,
  `.env.example`, and per-crate package metadata.

## [0.1.0]

Initial public version of **Gordian** — an LLM agent that designs KiCAD schematics and PCBs from
natural-language prompts, orchestrating deterministic, oracle-gated layout and routing engines.

### Highlights
- **Live schematic authoring** (`sch-doc` + `gordian-tools-sch` + `sch-floorplan`):
  lossless `.kicad_sch` edits, connectivity-delta guards, solver-owned placement
  and wiring, idiom recognition, and authoritative lint/ERC checks.
- **PCB place and route** (`pcb-workflow` + `pcb-engine`): deterministic placement,
  routing, DRC, rendering, and fabrication export over the live schematic netlist.
- **Agent application** (`gordian-core` + `gordian-llm` + `gordian-runtime` +
  `gordian`): provider-agnostic orchestration, per-mutation approval, a headless
  CLI, and an interactive ratatui copilot. The model never emits coordinates.
