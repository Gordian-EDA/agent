---
name: Bug report
about: A defect in schematic generation, PCB place/route, or the agent
labels: bug
---

**What happened**
A clear description of the bug.

**Repro**
- The prompt or input circuit (or a minimal `.kicad_sch` / `.kicad_pcb` / circuit-lang snippet)
- The command you ran

**Expected vs actual**

**Environment**
- KiCAD version (`kicad-cli version`)
- OS / Rust version (`rustc --version`)
- DRC/ERC output, if relevant

A failing case added to the relevant harness (`board_harness`, `floorplan_netlist`) is the most
useful report of all.
