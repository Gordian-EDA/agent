# Contributing to Gordian

Thanks for your interest! This document covers how to build, test, and submit changes.

## Development setup

- **Rust** edition 2024 (rustc ≥ 1.85) — `rustup` is recommended.
- **KiCad 9 or 10** installed, for its symbol/footprint libraries and `kicad-cli` ERC/DRC. The engines
  auto-detect the libraries (e.g. `/usr/share/kicad/symbols`).
- A model provider for the agent itself (only needed to run the LLM loop, not for the engines or
  most tests). Run `gordian tui` once to create the platform `config.toml`, then set `llm.model`,
  `llm.apiKey`, and optionally `llm.endpoint`.

```sh
cargo build --release
cargo test --workspace --quiet
cargo clippy --workspace --all-targets -- -D warnings
```

## The core invariant: never ship copper that lies

The single most important rule. The engines are **oracle-gated**: every routed board is checked by
the in-house DRC lint *and* `kicad-cli pcb drc`, and any copper that fails clearance/width/via/bounds
or connectivity is dropped and reported as honestly unrouted — never shipped as a passing board.

A change is a **regression** if it raises copper-error DRC faults or breaks connectivity, even if the
render looks nicer. Gate every engine change on:

```sh
cargo test --release -p gordian-core
```

For PCB router changes, also run the required corpus harness for the production portfolio and any
explicit router strategy you touched:

```sh
cargo run -p gordian-core --example validate_pcb_corpus --quiet -- --required
```

Use `--router mesh`, `--router sequential`, or `--router mesh-detail` when isolating a specific
strategy. `--all` is a stress sweep, not the required per-change gate.

For the schematic side, the netlist oracle is authoritative:

```sh
cargo test --release -p sch-floorplan
```

## Working style

- **Verify first.** Before tuning constants or adding speculative fixes, reproduce the problem and
  confirm the mechanism. Many subsystems have a probe/e2e harness — use it to establish a baseline,
  then to prove the fix.
- **The LLM never emits coordinates.** Spatial decisions (placement, routing, floorplanning) belong
  in the deterministic engines. The agent chooses parts, nets, and rules and triages failures.
- **Match the surrounding code.** Comment density, naming, and idiom should read like the file you're
  editing. Keep design rationale close to the code when it is needed to maintain the behavior.
- **Determinism.** Engine output must be reproducible for the same input (seeded SA, no wall-clock /
  RNG in layout). Tests rely on this.

## Submitting changes

1. Branch from `main`.
2. Keep commits focused; explain *why* in the message, not just *what*.
3. Ensure `cargo test --workspace --quiet`,
   `cargo clippy --workspace --all-targets -- -D warnings`, and the required PCB corpus harness
   stay clean.
4. Open a pull request describing the change and how you verified it.

## Reporting issues

Include the prompt or input circuit, the produced `.kicad_sch` / `.kicad_pcb` (or a minimal
repro), the KiCAD version, and the DRC/ERC output. A failing case added to the relevant harness is
the most useful bug report of all.
