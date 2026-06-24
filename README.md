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

Requires a recent **Rust** toolchain (edition 2024, rustc ≥ 1.85) and an installed **KiCAD ≥ 8**
(for its symbol/footprint libraries and `kicad-cli` ERC/DRC). The engines auto-detect KiCAD's
libraries (e.g. `/usr/share/kicad/symbols`).

```sh
# Build
cargo build --release

# Configure a model provider in .env (OpenAI-compatible gateway shown):
#   OPENAI_API_KEY=...        OPENAI_BASE_URL=...        OPENAI_MODEL=...
# or AWS Bedrock:
#   AWS_BEARER_TOKEN_BEDROCK=...   AWS_REGION=...   AGENT_PROVIDER=bedrock

# Run one headless design turn:
cargo run --release -p gordian -- agent --project ./my_board "a 3.3V buck converter from 12V, 2A"

# Or the interactive copilot (chat + apply-gate cockpit):
cargo run --release -p gordian -- tui --project ./my_board
```

## Architecture

A Rust workspace; the LLM orchestrates the deterministic crates:

| Crate | Role |
|-------|------|
| `gordian` | CLI + ratatui copilot TUI — the entry point |
| `llm-client` | Provider-agnostic LLM client: neutral message/tool types + the `Provider` trait (OpenAI / AWS Bedrock backends) |
| `gordian-core` | The KiCAD agent: the turn loop + apply-gate, the schematic/PCB tools, prompts, review, and render — over any `llm-client` `Provider` |
| `circuit-lang` | Parser, linter, and canonical emitter for the circuit markup language |
| `circuit-graph` | Attributed circuit graph + a declarative idiom matcher |
| `sch-place-core` / `sch-io` / `sch-model` | Deterministic schematic floorplan core (`Design` → `.kicad_sch` and back), over the `greedy-place`/`anneal-place` engines, with the shared model + I/O layers |
| `grid-astar` / `pcb-place` / `negotiated-mesh` | Deterministic placement + grid-A\* escape + capacity-mesh copper routing |
| `pcb-synth` / `drc-lint` | `.kicad_pcb` synthesis and DRC lint |
| `kicad-sexpr` / `kicad-cli-rs` / `kicad-ipc` / `specctra` | KiCAD file I/O, `kicad-cli` driver, live IPC session, Specctra DSN/SES |

## Testing

```sh
cargo test --workspace          # unit + integration tests
cargo clippy --workspace        # lints
```

Deterministic end-to-end harnesses route/export every fixture circuit against the real installed
KiCAD libraries and assert **0 copper-error DRC faults** (unrouted nets are reported honestly, never
hidden):

```sh
cargo run --release -p gordian-core --example board_harness   # PCB place/route/DRC across all fixtures
```

## Status

Active development (`0.1.0`). The PCB and schematic engines route/lay-out dense, real-world boards
(including BGAs with 50+ components) DRC-clean; HDI micro-via escape and multi-sheet schematic layout
are implemented. See `docs/specs/` for the design notes behind each subsystem.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
