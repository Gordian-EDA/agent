# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial open-source release scaffolding: Apache-2.0 `LICENSE`, `README`, `CONTRIBUTING`,
  `.env.example`, and per-crate package metadata.

## [0.1.0]

Initial public version of **auto-pcb** — an LLM agent that designs KiCAD schematics and PCBs from
natural-language prompts, orchestrating deterministic, oracle-gated layout and routing engines.

### Highlights
- **Schematic generation** (`sch-layout` + `circuit-lang` + `crossmin`): natural-language prompt →
  deterministic `.kicad_sch`, with multi-sheet floorplanning, crossing-minimised layout, idiom
  recognition (decoupling, crystal, LED indicators, …), and an authoritative netlist oracle.
- **PCB place & route** (`pcb-engine` + `forceplace` + `kicad-bridge`): force-directed placement and
  a deterministic copper autorouter (slice-1 grid ∨ capacity-mesh detailed), GND/VCC plane synthesis,
  HDI micro-via in-pad escape, and per-net trace widths — every board gated to **0 copper-error DRC
  faults** by an in-house lint plus `kicad-cli` DRC.
- **Agent** (`agent` + `autopcb`): provider-agnostic LLM client (OpenAI / AWS Bedrock), a tool
  surface over the engines, pattern-aware failure triage, and a `resize_board` lever for the
  placement-convergence path. The model never emits coordinates.
