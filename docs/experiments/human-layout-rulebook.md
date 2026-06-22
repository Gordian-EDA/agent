# Human dense-schematic layout rulebook (mined from 18 dense human boards)

Source: mining workflow over `~/kicad-dataset` dense slice (2026-06-21, 6 vision-survey
groups → synthesis). Full run: task w59yukjlp. Each rule has a **measurable proxy** an
engine can compute. Ranked by impact on making dense auto-layout look human.

**Core finding:** professional dense schematics win on **GLOBAL organization**, not local
neatness. Naive auto-layout gets the LOCAL discipline right (grid, orthogonality,
orientation) but fails the GLOBAL structure (labels-not-wires, disjoint blocks, signal
flow). Engine status noted per rule.

## HIGH-priority global levers

1. **Label-vs-wire (support 6/6).** Long-haul / cross-block nets → name-matched net-label
   pairs; draw a wire ONLY when both endpoints are in the same cluster AND span is short.
   *Proxy:* median wire < 8mm; **<2% of wires > 50mm (humans ~0%); flag any wire >50mm as a
   label candidate.** Every label-net name appears ≥2× (orphan-label==0 hard gate).
   *Engine:* ❌ labels are only a route-failure fallback, penalized 1000. **← LEVER A (first).**
2. **Disjoint functional blocks + gutters (6/6).** Cluster by connectivity; #clusters≈0.22–0.34×#parts;
   cluster-bbox overlap ≈0; inter-cluster gap ≥10–15mm; intra-block NN spacing 5–6.35mm,
   gap/intra > 2. *Engine:* ❌ no block notion; flat placement. **← LEVER B.**
3. **Distribute power as local symbols (6/6).** Local rail+GND tap at each power pin, no
   page-spanning rails. *Proxy:* (#power-port-syms/#power-pins)>0.6; stubs ≤10mm; bus_count==0.
   *Engine:* ✅ partial — `rail_should_distribute` does this on boards >FAST_PINS.
4. **1.27mm grid + 100% orthogonal (6/6).** *Proxy:* grid_frac≥0.98 both axes; diagonal_fraction==0.
   *Engine:* ✅ snap-to-grid + elbow router. (Verify our renders actually hit these.)
5. **Left-to-right signal flow (4/6).** Inputs far-left, outputs far-right, ICs mid by
   topo depth. *Proxy:* directional_score=(mean_x(out)-mean_x(in))/width > 0.5; corr(IC.x,
   topo-depth)>0.5. *Engine:* ❌ no flow-direction term. **← LEVER C.**

## MED-priority

6. **Decoupling cap pin-adjacent (5/6).** dist(cap, served IC power pin) ≤15–25mm; cap+rail+GND
   x-collinear ±1grid; same cluster. *Engine:* ✅ partial (`stray` term, idiom detect).
7. **Orientation grammar (6/6).** Series→flow-axis (horizontal); shunt/tap→perpendicular
   (vertical, GND lowest); rotations ∈{0,90,180,270}. *Engine:* ✅ orient_viol/spine.
8. **Power-symbol polarity (4/6).** +rail glyphs up/above pin, GND down/below. *Engine:* ✅ leg dir.
9. **Repeated motif → matrix tiling (3/6).** Exact lattice, uniform pitch (CoV<0.05 row). *Engine:* ❓.
10. **High-pin IC = label fan-out hub (3/6).** ICs ≥16–20 pins: short stubs → side-banked
    labels, ~nothing routes out of body. *Proxy:* ≥0.8 labels in two tight X-bands; ≥0.6
    labels/pin. *Engine:* ❌ — direct consequence of LEVER A on big ICs. **← LEVER A extension.**

## LOW
11. Connectors pinned to nearest edge + parallel comb of short stubs→labels (2/6). (PCB has edge_seek.)
12. Block-title captions / bounding rectangles (4/6) — annotation polish, not geometry.

## Implication for the plan
Engine = strong LOCAL, weak GLOBAL. Order: **A (label long nets) → B (disjoint blocks) →
C (signal flow)**. A is highest-support, most decisive (per synthesis), cheapest
(precedented), connectivity-safe. B/C are the placement-side global structure.
