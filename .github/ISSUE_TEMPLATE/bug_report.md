---
name: Bug report
about: A defect in schematic generation, PCB place/route, or the agent
labels: bug
---

**What happened**
A clear description of the bug.

**Repro**
- The prompt or input circuit (or a minimal `.kicad_sch` / `.kicad_pcb` file)
- The command you ran

**Expected vs actual**

**Environment**
- KiCAD version (`kicad-cli version`)
- OS / Rust version (`rustc --version`)
- DRC/ERC output, if relevant

A failing case added to the relevant schematic-derived PCB flow or floorplan netlist test is the
most useful report of all.
