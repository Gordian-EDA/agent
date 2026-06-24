# Whole-project refactor — function-separated, algorithm-named, tiered crates

Status: planned (Jun 2026). Branch: TBD (off merged `main`, which now carries both the
schematic engine and the interactive-KiCAD-IPC PCB work).

## Goal
Restructure the agent + PCB + schematic code so the crate graph is legible:
one role per crate, names that say what they *are*, a clean free/premium boundary,
and the reusable interop published. No behaviour change — pure moves, gated.

## Principles (every move obeys these)
1. **A crate needs a reason beyond "it's a function"** — a *tier boundary*, a
   *publish target*, or a *real reuse seam*. Pure-size splits get inlined.
   (So: no `svg-raster` crate — the resvg call stays in `agent`; `legalize` folds
   into `force-place`; `pcb-engine`/`kicad-bridge`/`forceplace` and `-pro` suffixes
   all disappear.)
2. **Name an algorithm crate by its algorithm** (`grid-astar`, `greedy-place`).
   A crate with *no* algorithm — a contract, an I/O/transform, a rule-checker, a
   renderer, an interop shim — takes a domain/function name (it has no algorithm to
   reflect). When two domains share an algorithm *class*, qualify by the
   *distinctive variant*, never by a `pcb-`/`sch-` prefix
   (`anneal-place` vs `floorplan-anneal`).
3. **Traits are the only swap seam.** Free baseline + private premium impl behind
   one trait, chosen at runtime by entitlement. The same trait is the plugin
   contract and it kills cross-crate cycles.
4. **Correctness is never paywalled; the free tier must stand alone** (it produces a
   legal, routed/placed, DRC/ERC-clean, exportable artifact by itself).
5. **Open-core: publish the plumbing, keep the brains** (interop → crates.io;
   quality algorithms → private overlay).
6. **One role per crate; no PCB/schematic mixing; shared interop is genuinely shared.**
7. **Acyclic:** every algorithm crate depends only on the model + traits; the tool
   layer wires impls together.

## Target structure
```
crates/
  kicad/   SHARED interop — all publishable, no moat
    kicad-ipc        live board over the KiCAD 9 IPC API (NNG/prost + Session)
    kicad-sexpr      .kicad_pcb + .kicad_sym s-expr read/write (footprint + symbol)
    kicad-cli     typed wrapper over the kicad-cli binary (drc/erc/plot/export)
    specctra         Specctra .dsn/.ses codec
    kicad-lib-index  fuzzy footprint/symbol index (SkimMatcherV2)

  pcb/
    pcb-model        types + traits (Placer/Router/DrcOracle/FreerouteBackend)   [contract]
    force-place      force-directed (Fruchterman-Reingold) + legalizer  : Placer  [free]
    grid-astar       sequential grid A* router                          : Router  [free]
    drc-lint         clearance/width/via/connectivity DRC               : DrcOracle[free]
    freerouting      Specctra bridge + LocalJar backend                 : Router  [free]
    pcb-svg          diagnostic board render (engine view + failures)            [free]
    pcb-synth        BoardDraft -> .kicad_pcb (planes/zones/outline/silk)        [free]
    anneal-place     simulated-annealing board placement                : Placer  [premium]
    fanout-place     radial fan-out placement (the 8-9/10 placer)       : Placer  [premium]
    negotiated-mesh  negotiated rip-up/reroute over a quadtree capacity mesh : Router [premium]
    freerouting-hosted  hosted multi-seed backend (DRC+critic pick-best) : FreerouteBackend [premium]

  sch/
    circuit-lang     Circuit-YAML: parse/desugar/lint/emit             [contract/language]
    circuit-graph    attributed circuit graph + idiom detection (graph-similarity)
    sch-ir           LayoutIR + infer_ir + gather_* idiom seating (shared scaffold)
    elbow-route      elbow / Manhattan schematic router                : (sch Router) [free]
    sch-emit         placed symbols -> .kicad_sch                                 [free]
    sch-lift         .kicad_sch -> canonical YAML                                 [free]
    sch-textplace    collision-free label-placement solver                       [free]
    sch-erc          electrical rule check                             : (sch check)[free]
    greedy-place     greedy chain placement (the env/test default)     : Placer    [free]
    floorplan-anneal locality-aware floorplan simulated annealing       : Placer   [premium]

  agent/   agent-core (LLM loop + ToolCtx/registry + svg_to_png) · pcb-tools · sch-tools
           AI design + critic loop  [premium feature]
  gordian/ CLI + TUI shell
```

## PCB <-> schematic symmetry (why one structure fits both)
| concern | PCB | schematic | shared |
|---|---|---|---|
| model/lang | pcb-model | circuit-graph + circuit-lang + sch-ir | — |
| placement (free) | force-place | greedy-place | — |
| placement (premium) | anneal-place · fanout-place | floorplan-anneal | — |
| routing (free) | grid-astar · freerouting | elbow-route | — |
| routing (premium) | negotiated-mesh · freerouting-hosted | — | — |
| emit -> KiCAD | pcb-synth | sch-emit | kicad-sexpr |
| read <- KiCAD | (kicad-sexpr) | sch-lift | kicad-sexpr |
| library | (footprint) | (symbol) | kicad-sexpr + kicad-lib-index |
| correctness | drc-lint | sch-erc | — |
| live edit / CLI | — | — | kicad-ipc / kicad-cli |
| tools | pcb-tools | sch-tools | agent-core |

## Free / premium
Same logic both sides: **quality + AI = premium; language + correctness + I/O +
a real baseline = free.** The free tier stands alone (it already does — schematic
`greedy-place` is the current default and gets the full idiom-aware `sch-ir`).
- **Premium:** `anneal-place`, `fanout-place`, `negotiated-mesh`, `freerouting-hosted`,
  `floorplan-anneal`, and the AI design/critic loop.
- **Free:** everything else, incl. baselines `force-place`/`grid-astar`/`greedy-place`/
  `elbow-route` and the open `freerouting` (local).
- **Publish:** `kicad-ipc`, `kicad-sexpr`, `specctra`, `kicad-cli`, `drc-lint`,
  `circuit-lang`.

## Traits (in pcb-model; the entitlement swap seam)
```rust
trait Placer  { fn place(&self, p: &PlaceProblem) -> PlaceResult; }
trait Router  { fn route(&self, p: &RouteProblem) -> RouteResult; }   // grid-astar, negotiated-mesh, freerouting all impl this
trait DrcOracle { fn check(&self, b: &Board) -> DrcReport; }
trait FreerouteBackend { fn run(&self, dsn: &str, o: &Opts) -> Result<Ses>; } // LocalJar (free) / Hosted (premium)
```
- `freerouting` is a `Router` whose `.route` does the dsn/ses codec and delegates the
  actual route to an injected `FreerouteBackend` (local vs hosted) — Freerouting is
  not a special concept, just another `Router`.
- `anneal-place`/`fanout-place` take an injected `&dyn Router` for routability ranking
  → they depend on the trait, not on `negotiated-mesh`. No cross-crate cycle.
- `route_auto`/`place_best` (run-several-pick-best) become a `BestOf` combinator in the
  tool layer — itself a `Router`/`Placer` — uniform across all engines incl. hosted
  multi-seed.

## Migration sequence (phase 0-1 done & merged; pure moves behind re-exports, one gate/phase)
1. **DONE** — delete `forceplace`; carve out `pcb-model`. *(merged to main)*
2. **Shared interop** — `kicad-bridge` -> `kicad-sexpr` / `kicad-cli` / `specctra` /
   `kicad-lib-index`; delete `kicad-bridge`. (Unblocks both sides — do first.)
3. **PCB engine** — `pcb-engine` -> `force-place`/`grid-astar`/`drc-lint`/`pcb-svg`
   (free) + `anneal-place`/`fanout-place`/`negotiated-mesh` (premium); selectors ->
   `pcb-tools` `BestOf`; delete `pcb-engine`.
4. **Schematic engine** — `sch-layout` -> `sch-emit`/`sch-lift`/`elbow-route`/
   `sch-textplace`/`sch-ir`/`greedy-place` (free) + `floorplan-anneal` (premium).
   `floorplan.rs` (6786 loc) is the hard one; the `PlacementStrategy` trait already
   splits Greedy/Anneal, so the seam exists.
5. **Agent** — extract `agent-core` (resolves the ToolCtx cycle) + `pcb-tools` + `sch-tools`.
6. **Tiering** — premium crates -> private overlay behind `feature="pro"`; entitlement
   registry in `agent-core`; `freerouting-hosted` backend.
7. **Group + publish** — `git mv` into `crates/{kicad,pcb,sch}/` + workspace glob;
   publish the interop crates + `circuit-lang`.

Phases 2/3/4 are independent -> parallelizable. Run on a fresh branch off `main`.

## Gates (run BOTH every phase)
- **Schematic:** `cargo test --release --workspace` (47 suites incl.
  `-p sch-layout --test floorplan_netlist` netlist oracle).
- **PCB:** `cargo run --release -p agent --example board_harness` (DRC PASS across
  77 boards) + the 9/10 critic spot-check (`led-array-60`, `bga-decoupled`).

## Naming rule recap (the one I kept slipping on)
Algorithm crate -> algorithm name. Non-algorithm crate (contract / I/O / checker /
renderer / interop) -> domain or function name. Same algorithm in two domains ->
qualify by the distinctive variant (`anneal-place` vs `floorplan-anneal`), never by a
`pcb-`/`sch-` prefix.

## Open items
- `sch-textplace`: keep the function name unless its solver is a nameable method.
- `negotiated-mesh` vs `capacity-mesh`: chose `negotiated-mesh` (names the search).
- `sch-ir`: the idiom-seating `gather_*` passes are heuristic, not one named
  algorithm; kept as the shared scaffold. If they grow, the graph-similarity idiom
  *detection* could split into its own algorithm-named crate.
