# Gordian

[![CI](https://github.com/Gordian-EDA/agent/actions/workflows/ci.yml/badge.svg)](https://github.com/Gordian-EDA/agent/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

An LLM agent that designs **KiCAD schematics and PCBs** from a natural-language prompt.

You describe a circuit — *"a USB-C powered temperature logger with an ESP32-S3, a 3.3 V LDO,
and an I²C sensor"* — and the agent produces a real `.kicad_sch` schematic and a routed
`.kicad_pcb` board, validated against KiCAD's own ERC/DRC.

## The idea

The LLM never emits coordinates or copper. It works *around* a set of **deterministic engines**,
calling them through a small tool surface:

- it chooses parts, footprints, nets, and design rules;
- the engines do the spatial work — schematic floorplanning, component placement, and copper
  routing — reproducibly;
- every result is gated by an **oracle** (an in-house DRC lint plus `kicad-cli`'s ERC/DRC), so the
  agent self-repairs off structured diagnostics and never ships a board that lies about
  connectivity or clearance.

This split keeps the layout reproducible and correct while letting the model handle the open-ended
part of design — intent, part selection, and triage.

## Quick start

Requires a recent **Rust** toolchain (edition 2024, rustc ≥ 1.85) and an installed **KiCAD 9 or 10**
(for its symbol/footprint libraries and `kicad-cli` ERC/DRC). The engines auto-detect KiCAD's
libraries (e.g. `/usr/share/kicad/symbols`).

Auto-detection uses the single KiCAD installation selected by the current
environment. On machines with multiple majors installed, configure one coherent
installation explicitly:

```toml
[kicad]
symbolDir = "/opt/kicad10/share/kicad/symbols"
footprintDir = "/opt/kicad10/share/kicad/footprints"
cliPath = "/opt/kicad10/bin/kicad-cli"
pcbnewPath = "/opt/kicad10/bin/pcbnew"
attachRunning = false
enableApiConfig = true # explicit opt-in if managed launch should enable IPC
```

The live IPC connection verifies that the selected/running PCB editor has the
same major version as `kicad-cli`.

```sh
# Build
cargo build --release

# First run creates a platform config file, e.g. ~/.config/gordian/config.toml.
# Set llm.model and llm.apiKey there before running the agent.

# Run one headless design turn:
cargo run --release -p gordian -- agent --project ./my_board "a 3.3V buck converter from 12V, 2A"

# Or the interactive copilot (chat + per-mutation approval):
cargo run --release -p gordian -- tui --project ./my_board
```

PCB physical design uses one tuned place-then-route engine. Schematic placement
remains independently selectable:

```toml
[engines]
schematicPlacer = "cluster" # cluster | anneal | spine
```

## Architecture

A Rust workspace; the LLM orchestrates the deterministic crates:

| Crate | Role |
|-------|------|
| `gordian` | CLI + ratatui copilot TUI — the entry point |
| `gordian-core` / `gordian-llm` / `gordian-runtime` | Agent loop and prompts, provider abstraction, configuration, project context, and the tool-result contract |
| `gordian-tools-sch` | Live `.kicad_sch` queries, guarded mutators, bulk placement, rewiring, and authoritative checks |
| `pcb-workflow` | Application workflows that coordinate PCB creation, placement, routing, validation, rendering, and fabrication export |
| `kicad-board` | KiCad PCB persistence boundary: live IPC snapshots, domain conversion, and atomic offline board edits |
| `sch-check` | The kernel circuit model (`Design`), its semantic lints and deterministic ERC, and the `place_parts` tool input |
| `circuit-graph` | Attributed circuit graph + a declarative idiom matcher |
| `sch-doc` | Lossless editable `.kicad_sch` document and pure-Rust connectivity extractor |
| `sch-floorplan` / `sch-place` | Deterministic live schematic placement, arrangement, rewiring, and shared placement model |
| `anneal-place` / `cluster-place` / `spine-place` | Interchangeable schematic placement engines |
| `kicad-symbol` / `kicad-footprint` | KiCAD library discovery, metadata, and geometry |
| `pcb-model` | Unified `PcbProblem -> PcbSolution` framework contract |
| `pcb-engine` | Gordian's single tuned place-then-route PCB engine |
| `pcb-place` | Placement views and the engine's tuned placement phase |
| `pcb-route-grid` | Grid/A\* primitives for the tuned routing phase |
| `pcb-route-mesh` | Tuned routing pipeline and lower-level mesh diagnostics |
| `pcb-drc` | Extensible PCB geometry and connectivity DRC |
| `kicad` / `kicad-ipc` | KiCAD discovery and CLI driver, plus the live pcbnew IPC session |
| `geom` | Shared geometry primitives |

## Testing

```sh
cargo test --workspace --quiet  # unit + integration tests
tools/live_kicad_test.sh 9      # live pcbnew IPC suite; also accepts 10
cargo clippy --workspace --all-targets -- -D warnings
                               # lints for libs, bins, examples, tests, and doctests
cargo run -p pcb-workflow --example validate_pcb_corpus --quiet
                               # PCB smoke: real KiCAD footprints, place + route + DRC lint
cargo run -p pcb-workflow --example validate_pcb_corpus --quiet -- --required
                               # Required PCB gate: smoke boards plus power, LED, and dense BGA
cargo run -p pcb-workflow --example validate_pcb_corpus --quiet -- power-buck led-array
                               # Named ad-hoc real-board checks
cargo run -p pcb-workflow --example validate_pcb_corpus --quiet -- bga25-route
                               # Dense BGA auto-router qualification check
cargo run -p pcb-workflow --example validate_pcb_corpus --quiet -- --router sequential -v bga25-route
                               # Explicit non-A* sequential-grid diagnostic route
cargo run -p pcb-workflow --example validate_pcb_corpus --quiet -- --router astar -v bga25-route
                               # Explicit grid A* baseline diagnostic route
cargo run -p pcb-workflow --example validate_pcb_corpus --quiet -- --router mesh-global -v bga25-route
                               # Capacity-mesh global-routing isolation for heavy failures
cargo run -p pcb-workflow --example validate_pcb_corpus --quiet -- --router mesh-assign -v bga25-route
                               # Capacity-mesh crossing/via assignment isolation
cargo run -p pcb-workflow --example validate_pcb_corpus --quiet -- --router mesh-detail -v bga25-route
                               # Raw detailed cell-routing isolation; production rescue is intentionally disabled
cargo run -p pcb-workflow --example validate_pcb_corpus --quiet -- --router mesh --inspect-net S1,S2 -v bga25-route
                               # Bounded endpoint/copper inspection for failed or recently repaired nets
cargo run -p pcb-workflow --example validate_pcb_corpus --quiet -- --router mesh-detail --inspect-failed-nets -v bga25-route
                               # Automatically inspect every failed raw-detail net
cargo run -p pcb-workflow --example validate_pcb_corpus --quiet -- --router mesh-assign --inspect-detail-jobs --inspect-net S4,VCC bga25-route
                               # Detailed crossing/cell-job inspection for dense-placement routing pressure
```

Live product quality is evaluated separately from correctness tests. The small
VLM-judged suite under `quality/` runs natural-language create/edit/replace cases:

```sh
python3 quality/run.py --list
python3 quality/run.py create-hard-pcb
```

The runner uses the same `llm.endpoint`, `llm.apiKey`, and `llm.model` from the
platform Gordian config as normal agent runs; it does not maintain separate
quality credentials.

Each run records KiCAD ERC/DRC facts, before/after renders, the agent transcript,
and a judge verdict containing only `score` and `issues` under `quality/runs/`.

PCB changes should be exercised through the same schematic-derived and current-board tools the
agent uses; avoid privileged JSON-only board construction paths in tests.
The schematic validation corpus under `docs/validation` is optional in this checkout; tests that
need it skip cleanly when the corpus is absent.

## Status

Active development (`0.1.0`). The PCB and schematic engines route/layout real KiCAD boards through
the corpus smoke checks, with additional slower real-board checks for power, LED, and dense BGA
examples. The required PCB gate is `validate_pcb_corpus --required`; it is intentionally smaller
than `--all`, which includes scale/stress fixtures. Dense multilayer/BGA production routing is
qualified through the `mesh` portfolio (detailed routing plus adaptive rescue); `mesh-detail` is a
raw diagnostic isolation mode for tightening the per-cell detailed stage.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
