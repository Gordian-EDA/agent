# Gordian

[![CI](https://github.com/Gordian-EDA/agent/actions/workflows/ci.yml/badge.svg)](https://github.com/Gordian-EDA/agent/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

An LLM agent that designs **KiCad schematics and PCBs** from a natural-language prompt.

You describe a circuit and the agent produces a real `.kicad_sch` schematic and, when asked, a
routed `.kicad_pcb` board — validated against KiCad's own ERC/DRC.

## The idea

The model never emits coordinates or copper. It describes the circuit as a **netlist** (parts,
values, footprints, and the net every pin connects to) plus **layout trees** — a CSS-flexbox-like
`row`/`col` tree of parts per functional block. A deterministic engine turns that into an exact
drawing: it places parts, routes wires, adds labels and power symbols, compiles a real
`.kicad_sch`, and reports connectivity/geometry issues back to the model. The same split carries
through to the board: schematic accepted -> deterministic placement + Freerouting + KiCad DRC
produce the `.kicad_pcb`, with no further model involvement.

This keeps the drawing reproducible and correct while the model handles the open-ended part of
design: part selection, pin mapping, and composing a block that looks like a human drew it.

### The seven tools

The design loop (`crates/gordian-core/src/tools.rs`) drives exactly seven tools:

| Tool | Purpose |
|------|---------|
| `search_symbols` | Search KiCad's stock symbol libraries by part number, function, or lib prefix |
| `symbol_info` | Pin table (number, name, electrical type, side) of a symbol, per unit |
| `build` | Lay out + compile the netlist and layout trees into `.kicad_sch`; fast, no KiCad, no image |
| `erc` | Run KiCad ERC on the last build |
| `render` | Render the last build with a coordinate grid, for the model to inspect |
| `review` | Independent visual critic: score 1-10 against a reference sheet, plus a defect list |
| `finish` | Deliver the last build, gated on a clean build, ERC with no errors, and a review >= 8 |

Editing an existing sheet swaps `build`'s argument for a **patch** (`remove`/`update`/`add`)
against the live design instead of a whole new one; the model gets the current design as JSON with
stable ids on every element so it can scope a change precisely.

Once a build is accepted, the composition polish pass (`compose`) may revise only the layout trees
against the critic's defects — the netlist is frozen, so a round can only improve the drawing.

## Skills

`skills/<name>/SKILL.md` files hold a verified parts list, pin map, and ready-to-build design JSON
for one well-known circuit family (e.g. a Blue Pill). `gordian-skills` selects up to two skills
against the prompt — a deterministic trigger-keyword pass, then `fuzzy-matcher`'s `SkimMatcherV2`
ranking of the prompt against each skill's triggers and name — and rides the match with the
opening message, framed as a starting point the model should adapt rather than a fixed answer.

## The PCB stage

Once the schematic is accepted, `pcb-auto` builds the board deterministically from the same
netlist: it sizes an outline at human courtyard density, seats connectors on the edges, places the
rest by connectivity, pours a ground zone, routes with a headless **Freerouting** (bundled at
`vendor/freerouting.jar`, driven as a subprocess), and gates the result on KiCad's own DRC. Nothing
here calls the model; it runs concurrently with the schematic's composition polish pass and starts
as soon as the first clean build exists, since a change of *drawing* is not necessarily a change of
*netlist*.

## Quick start

Requires:

- A recent **Rust** toolchain (edition 2024, rustc >= 1.85).
- **KiCad 10** (only KiCad 10 or newer is supported) — for its symbol/footprint libraries and
  `kicad-cli` (ERC, DRC, render, fabrication export).
- **Java 25** on `PATH` — Freerouting is a JVM router, invoked headless by `pcb-auto`.

```sh
cargo build --release

# First run creates the platform config file, e.g. ~/.config/gordian/config.toml,
# from config.example.toml. Set llm.model and llm.apiKey there.

# Design a schematic and route its board:
cargo run --release -p gordian -- agent --project ./my_board "a 3.3V buck converter from 12V, 2A"

# Schematic only:
cargo run --release -p gordian -- agent --project ./my_board --no-pcb "..."
```

`gordian agent --help` lists the rest: `--budget <seconds>` (wall clock for the whole run),
`--max-builds <n>` (ceiling on `build` calls), `--no-review` (skip the composition polish pass).
Editing an existing project re-runs the same command against a directory that already holds a
schematic; the model receives it as a patch target instead of starting from a blank sheet.

## Configuration

Gordian reads/writes `~/.config/gordian/config.toml` (`gordian-runtime::platform::config_path`).
`config.example.toml` documents every key the config model
(`crates/gordian-runtime/src/config.rs`) actually reads: `[llm]` (adapter, model, apiKey,
endpoint, maxTokens, ephemeralCache, reasoningEffort, captureReasoning, visionCapable), `[kicad]`
(optional symbolDir/footprintDir/cliPath overrides — auto-detected otherwise), `[project]`
(schematicFilename), and `[agent]` (budgetSeconds, maxBuilds; overridden by the matching CLI
flags).

## Architecture

| Crate | Role |
|-------|------|
| `gordian` | CLI entry point (`gordian agent ...`) |
| `gordian-core` | The agent: system prompt, the seven tools, the design loop, the visual critic, the composition pass, and the PCB stage glue |
| `gordian-llm` | The `Provider` seam and the `genai`-backed production LLM client |
| `gordian-runtime` | `GordianConfig`, its platform path, and process tracing |
| `gordian-skills` | `skills/<name>/SKILL.md` loading and fuzzy selection against a prompt |
| `sch-engine` | The deterministic schematic engine: netlist + layout trees -> laid out, compiled `.kicad_sch`, checked |
| `pcb-auto` | Deterministic PCB auto-layout: outline, placement, Freerouting, ground pour, KiCad DRC |
| `kicad` | KiCad installation discovery and typed `kicad-cli` operations (ERC/DRC, render, netlist, fab export) |
| `kicad-symbol` | `.kicad_sym` library reader, pin metadata, symbol drawing geometry, search |
| `kicad-footprint` | `.pretty` footprint library reader, pad/courtyard geometry, search |
| `geom` | Leaf math shared across the schematic and PCB stacks (2-D shapes, grid snapping, deterministic ids) |
| `quality-facts` | `sch_facts`/`pcb_facts` binaries: deterministic facts about a `.kicad_sch`/`.kicad_pcb` for the quality harness |

## Testing

```sh
cargo test --workspace --quiet
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

### Quality harness

`quality/run.py` runs end-to-end create/edit prompts through the real agent and scores the result:
deterministic facts first (`quality-facts`, KiCad ERC/DRC), then a VLM judge second. A failed
deterministic check caps the score regardless of what the judge thought.

```sh
python3 quality/run.py --list
python3 quality/run.py --suite schematic --jobs 2 --output quality/runs/schematic
python3 quality/run.py --question "is the board production-ready?" create-hard-pcb
```

Suites: `schematic` (`dataset-*` + `prompt-*`), `campaign` (end-to-end schematic+PCB), `pcb`, `all`.
`--jobs N` runs N cases at a time; `--repeat N` runs each case N times and reports the median (a
single VLM read has real run-to-run variance).

The runner uses `llm.endpoint`/`llm.apiKey`/`llm.model` from the platform config, same as a normal
agent run, and KiCad checks/reference renders use `KICAD_CLI` or `kicad.cliPath` from it — only
KiCad 10 is accepted. `tools/schematic_critic.py` and `tools/pcb_critic.py` are the two VLM
critics `quality/run.py` shells out to for the judge pass; `gordian-core::critic` embeds the same
rubric text (`tools/schematic_critic_*.txt`) so the agent's own in-loop `review` tool grades a
sheet by the identical standard.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
