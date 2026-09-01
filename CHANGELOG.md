# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
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
- **Schematic generation** (`sch-layout` + `circuit-lang`): natural-language prompt →
  deterministic `.kicad_sch`, with multi-sheet floorplanning, shelf-pack + locality-aware annealed
  placement, idiom recognition (decoupling, crystal, LED indicators, …), and an authoritative netlist
  oracle.
- **PCB place & route** (`pcb-engine` + `forceplace` + `kicad-bridge`): force-directed placement and
  a deterministic copper autorouter (slice-1 grid ∨ capacity-mesh detailed), GND/VCC plane synthesis,
  HDI micro-via in-pad escape, and per-net trace widths — every board gated to **0 copper-error DRC
  faults** by an in-house lint plus `kicad-cli` DRC.
- **Agent** (`agent` + `gordian`): provider-agnostic LLM client (OpenAI / AWS Bedrock), a tool
  surface over the engines, pattern-aware failure triage, and a `resize_board` lever for the
  placement-convergence path. The model never emits coordinates.
