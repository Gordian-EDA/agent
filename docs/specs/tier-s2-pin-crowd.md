# Tier S2 — pin-adjacency-vs-congestion as a holistic cost (`pin_crowd`)

Source: tier-s2-foundation workflow (wf_becb27fb-6fb, 4 research agents + xhigh synthesis), 2026-06-22.

> IMPLEMENTATION NUANCE (verified while reading the code): small multi-sheet SUB-SHEETS use the ROUTED
> cost (`layout_cost`/`stray`, floorplan.rs:4321/:4779), NOT the proxy fast lane (`proxy_cost`:3140, gated >FAST_PINS).
> In the routed path `signal_anchor_centroid` ALREADY targets a lone power cap (a power net not in ir.rails is
> treated as Signal → pulls to the IC pin), and that pull is deliberately WEAK (0.5) to avoid cramming (:4790).
> So the orphaned-cap is the WEAK-PULL-vs-CRAM trade-off: `pin_crowd` (the missing repulsion) is what lets the
> pull be strengthened without cramming. Add pin_crowd to the ROUTED `base` (OUTSIDE it, like `multiunit`:4606,
> structurally 0 for ≤34-pin single-cap references) AND gate on MULTISHEET_REFINE. The proxy cohesion-target
> fix (is_rail||is_power_net @:3055) is for large-board lone caps; secondary for the multi-sheet targets.

---

I now have full grounding. The proxy `cohere` term (floorplan.rs:3185) sums per-satellite Manhattan distance to its cohesion target's centroid with no counter-pressure for crowding — exactly the missing "pin-adjacency vs congestion" balance the DEFECT→COST map calls out as defect (a)'s root. I have everything to write the plan.

# Tier S2 Plan — Pin-Adjacency-vs-Congestion as a Holistic Cost Term

## Diagnosis (grounded in the code, not the brief)

The dominant unresolved trade-off is **defect (a): the orphaned decoupling cap**. Its root is precise and confirmed by reading the cost: the proxy `cohere` term (`floorplan.rs:3185`) and its routed mirror `stray` (`:4782`) pull each satellite toward the centroid of the anchor pin(s) it taps — a *pure attraction with no repulsion*. The only congestion term in either cost (`count_congestion`, `:4931`, weighted `neat*7` at `:4588`) measures **junction-dot packing and close-wire runs** — a property of *routed wires*, not of *part-body occupancy near a pin*. So when several caps tap the same IC supply pin (or a cap wants the one open slot beside a busy IC), nothing in the cost says "that pin-x is already crowded." The SA either stacks them (and the separate `decongest` post-pass evicts one 40 mm out — the iter-23 failure) or never pulls the lone cap in at all because the proxy `cohere` edge never fired (the idiom-graph closure at `:753` only marks a net Power for `ir.rails` nets, so a lone cap on a small sub-sheet is classed Signal and gets no cohesion target).

**The structural insight:** every prior fix was a *post-pass* (snap-beside, decongest, align_to_pins) — two local passes that fight (iter 14's lesson: only ONE pass holding both constraints sticks). The cost has the attraction half of the trade-off but is **missing the repulsion half as a cost term**. Add the missing half *to the objective* and the SA resolves the trade-off holistically during search instead of two post-passes undoing each other.

---

## 1. THE FIRST CHANGE — a per-pin local-occupancy congestion term, balanced against `cohere`/`stray`

### What it rewards/penalizes
Add **`pin_crowd`**: for each anchor pin that is a cohesion/stray *target*, penalize the number of satellite bodies whose centres fall within a local-crowding radius of that pin **beyond the first**. This is the explicit counter-pressure to `cohere` (proxy) and `stray` (routed): the first cap snaps to the supply pin (good — humans want exactly one decoupling cap within 15–25 mm of each supply pin, per HUMAN TARGETS), but the second, third… each pay a rising cost, so the SA spreads a decoupling *bank* across the IC's several supply pins (or to adjacent open columns) instead of stacking them on one pin-x. It directly prices the trade-off the post-passes couldn't: cohesion still pulls in, crowding pushes back, and the *same* objective picks the balance.

It is deliberately **not** a generic whitespace/`spread` term (that family is the proven minimize-trap, DEFECT→COST (b), and HUMAN TARGETS flags sprawl as match-not-minimize). It is a *local, per-target-pin* occupancy count — the one congestion signal the cost is missing.

### The math
Add to BOTH `proxy_cost` (`:3203`) and the routed `base` (`:4587`), sharing one weight. For the proxy, the cohesion targets are already in hand (`cohesion: &[(usize, Vec<(usize,usize)>)]`, `:3144`). For each *target pin* `p` (the centroid endpoints already computed at `:3173`), bin the satellites by which target pin they are pulled to. Let `n_p` = number of satellites whose nearest cohesion-target endpoint is `p` and whose body centre lies within radius `R` of `p`. Then:

```
pin_crowd = Σ_over_target_pins_p  max(0, n_p − 1)²        // quadratic: first cap free, each extra hurts more
```

- `R` = local-crowding radius (the decoupling band: candidate ≈ 15 mm — see derivation below).
- Quadratic in the excess so the gradient grows with crowding (the SA feels stacking immediately) while a single co-placed cap is exactly free (preserves the cap-hugs-pin win).
- In the **routed** cost reuse the `stray` machinery: `count_pin_crowd` iterates the same `<3`-pin satellites `count_stray` does (`:4779`), maps each to its `signal_anchor_centroid`/`supply_pin_target` pin, bins by that pin, applies the same `max(0,n−1)²`.

Insertion points (one focused change, gated):
- Proxy: new addend `+ PIN_CROWD_W * pin_crowd` at `:3207`, guarded so it is **zero unless `MULTISHEET_REFINE` is set** (mirror the existing `std::env::var("MULTISHEET_REFINE").is_ok()` gates at `:999`, `:2362`, `:5178`) — this is what keeps every reference/snapshot byte-identical (the proxy fast lane is already `>FAST_PINS`-only, but the env gate is the belt-and-suspenders the prior wins all used).
- Routed: new addend in `base` (`:4587`), same env gate. Because `max(0,n−1)²` is **0 for every single-unit / single-cap configuration**, it is structurally bit-identical on all ≤34-pin references *even before* the env gate (same trick `multiunit`/`sib_spread` uses at `:4606`) — the env gate is then pure insurance.

Also fix the *missing cohesion edge* that strands the lone cap, **without** the broad idiom-graph change that was reverted (iter 20): do **not** touch the closure at `:753`. Instead, in `cohesion_targets` (`:3071`) the pure-decoupling fallback already targets the nearest supply/gnd pin via `is_rail = ir.rails.contains_key(net)` — extend that *one predicate* to `is_rail = ir.rails.contains_key(net) || is_power_net(net)` (`:3055`), so a lone power cap whose net is `is_power_net` but not in `ir.rails` (the small-sub-sheet case) gets a supply-pin target. This is a one-line, *local-to-the-target-builder* change (it cannot create connectivity faults — it only adds an attraction target), and it is the same `is_power_net` test already used elsewhere (`:2356`, `:320`). It is gated by the same env flag.

### How to set `PIN_CROWD_W` and `R` from data — NOT hand-tuning

This is the part the brief insists on: derive, don't guess. Two corpus-anchored quantities, one VLM confirmation.

**Step A — fix `R` from the human corpus (no search).** HUMAN TARGETS gives decoupling-cap-to-supply-pin distance band **15–25 mm** and intra-block NN spacing **5–6.35 mm**. `R` is the radius inside which a *second* body is "crowding" the first: set `R` = the human intra-block NN pitch's upper edge plus one grid ≈ **7.6 mm** (so two caps at ≤1 grid apart count as crowding, two caps at the human 15 mm band-edge do not). Confirm by running `layout_metrics.py` over the 4 parsed samples (005b53684a1d, 0e47274cfeaf, 00024b278ea9, 039ca98ac254): measure, per supply pin, how many cap centres fall within candidate radii {5, 7.6, 12.7, 15} mm; pick the largest `R` at which the *human* `max(0,n−1)` is still ≈0 (humans don't stack). That radius is `R`. This is a corpus *measurement*, not a sweep.

**Step B — set `PIN_CROWD_W` by the marginal-trade rule against `stray`/`cohere`.** The weight must make *one* cap's crowding cost equal to the `stray`/`cohere` pull it would have to overcome to leave a crowded pin for the next-nearest supply pin. The `cohere` weight is `0.5` (`:3207`), `stray` is `0.5` (`:4590`). Moving a stacked cap from a crowded pin to the IC's *next* supply pin costs ≈ the inter-pin pitch `Δ` in extra `cohere`/`stray` (typical IC supply-pin spacing on these parts ≈ one IC width, measure `Δ` from the same 4 samples). Set:
```
PIN_CROWD_W  s.t.  PIN_CROWD_W · (2·1 − 1) ≈ 0.5 · Δ     // the 2nd cap's marginal crowd = the cohere cost of relocating it
⇒ PIN_CROWD_W ≈ 0.5 · Δ
```
This *derives* the weight from the existing `cohere`/`stray` scale and a measured pitch, so the new term and the term it balances are commensurate by construction — the 1st cap stays, the 2nd is indifferent between stacking and relocating, the 3rd strictly prefers to relocate (quadratic). No magic constant.

**Step C — VLM confirmation only as a *validator*, never the driver** (FIELD SURVEY: critic validates, doesn't drive). Run the critic on the two live boards at the derived `(R, PIN_CROWD_W)` and at the two flanking values `0.5·PIN_CROWD_W`, `2·PIN_CROWD_W`. Keep the derived value unless a flank is a *strict* critic win; this is a 3-point confirmation, not a CMA-ES loop.

---

## 2. VALIDATION PROTOCOL (the same bar every prior win cleared)

Run in order; **any** failure ⇒ revert the whole change.

1. **Gated on `MULTISHEET_REFINE`.** Both new addends and the `is_power_net` cohesion-target extension are inside `std::env::var("MULTISHEET_REFINE").is_ok()` guards. Confirm by grepping the diff: every new term sits behind the gate.

2. **`placement_snapshot` byte-identity.** `cargo test --release -p sch-layout --test placement_snapshot`. Must pass unchanged. Two independent guarantees: (i) the env gate (snapshot harness runs *without* `MULTISHEET_REFINE`); (ii) structural zero — `max(0,n−1)²` is 0 for every ≤34-pin single-cap reference even with the gate open. If snapshot shifts a single byte, the change is wrong — stop.

3. **Anneal netlist oracle (the truthfulness gate, per `premium-oracle-gate` memory).**
   `LAYOUT_SEARCH=anneal cargo test --release -p sch-layout --test floorplan_netlist`
   plus the standard `cargo test --release -p sch-layout --test floorplan_netlist`. Connectivity must be identical — the term touches placement only, never wires, so any net delta is a bug.

4. **Faithful VLM critic on the two LIVE boards** (the real S2 targets):
   ```
   set -a; . ./.env; set +a
   MULTISHEET_REFINE=1 ANNEAL=1 cargo run --release -p agent --example agent_design -- /tmp/live2.png "<IoT gateway from live2.draft.yaml>"
   MULTISHEET_REFINE=1 ANNEAL=1 cargo run --release -p agent --example agent_design -- /tmp/live3.png "<CAN node from live3.draft.yaml>"
   python3 tools/schematic_critic.py /tmp/live2-<sheet>.png --circuit "IoT gateway <block>" --show-reasoning
   python3 tools/schematic_critic.py /tmp/live3-<sheet>.png --circuit "CAN node <block>" --show-reasoning
   ```
   Score **every sheet** of both boards before and after.

5. **The bar: STRICT win.** Multiple sheets must rise (target: the orphaned-decoupling-cap defect resolved on the sheets that had it) and **no sheet may drop**. Specifically check the critic's `defects` list: the "scattered/orphaned decoupling cap" entry must disappear on affected sheets and **not** be replaced by a new "stacked caps" or "cap far from IC" defect. Confirm against the engine ground-truth (`EmitOutput.body_crossings`/`wire_through_body`) that no new body-crossing appeared. If any sheet regresses, or the net effect is mixed, **revert** — exactly as iters 16/20/23 were reverted.

---

## 3. RISKS and how the protocol contains them

- **The cost is tuned; broad changes regress (iter 20).** Mitigation: the change is *additive and local*. The new term is a single addend that is *provably zero* on the configurations that define the references (single cap per pin), so it cannot perturb the tuned basin where the engine already works — it only activates where a crowd of caps exists, which is precisely the defect region.
- **Two-pass fighting (iters 13/14/23).** Mitigation: this is a **cost term, not a post-pass**. There is no second pass to undo it; `decongest`/`align_to_pins` run *after* and now agree with the objective (the SA already de-crowded), so they have nothing to fight. This is the structural fix the iter-14 "one unified loop" lesson points at, lifted into the objective itself.
- **Quadratic crowding could fling caps away (the `spread` minimize-trap, defect b).** Mitigation: the *first* cap is exactly free (`max(0,n−1)`), so a correctly co-placed cap is never pushed off its pin — the term only bites *excess* co-location. And `PIN_CROWD_W` is set marginally equal to the `cohere` relocation cost (Step B), so a cap relocates to the *next supply pin of the same IC*, not into open space.
- **Weight derivation could be off-scale.** Mitigation: the weight is anchored to the *existing* `cohere`/`stray` 0.5 and a *measured* pin pitch, so it is commensurate by construction; the 3-point VLM confirmation (Step C) catches any residual mis-scale, and the strict-win bar reverts it if it doesn't help.
- **`is_power_net` cohesion-target extension over-fires.** Mitigation: it only *adds an attraction target* to a cap that currently has none — it cannot create a short, merge, or fault (those are routed-cost walls untouched here), and the netlist oracle (gate 3) proves connectivity is unchanged. It does NOT touch the idiom-graph closure (`:753`) that iter-20 broke.

The gating + snapshot byte-identity + anneal-oracle + strict-win critic bar is the exact ratchet that let prior wins ship and reverted the three that didn't; this change is built to pass all four or be discarded whole.

---

## 4. SCALE-UP — from one derived term to a learned linear cost

If `pin_crowd` lands as a strict win, it validates the central S2 thesis (FIELD SURVEY rec (a)): the cost is **linear over features the engine already computes**, and the SA only ever *compares* candidates under it, so the absolute scale is irrelevant and the whole aesthetic sub-vector can be **learned by pairwise ranking** — with `pin_crowd` now in the feature basis.

**Path:**
1. **Freeze correctness walls, expose the aesthetic feature vector.** Refactor `layout_cost` (`:4321`) so the aesthetic terms (`crossings, congestion, corners, junctions, stray, length, spread, orient_viol, spine_viol, multiunit, pin_crowd`) are returned as a **feature vector** `φ(layout)` and the cost is `w·φ`. The correctness walls (merges/overlaps/fallbacks/grid/body_cross, `:4571`) stay fixed and out of `w`. This is a mechanical extraction — no behaviour change at the current `w`.
2. **Data plumbing (corpus-only, no LLM in the loop — the tractable path).** For each of the 500 dense human boards: extract `φ(human_render)` via the *engine's own* feature builder (avoids the scale-mismatch risk FIELD SURVEY flags), and `φ(engine_output)` on the same netlist. Add free auto-labels: perturb each human layout (small SA-hot kicks) to get `φ(perturbed)` with the label `human ≺ perturbed`. The oracle plumbing already exists: `board_harness`-style batch render + `layout_metrics.py` for the human side, the existing extractor for the engine side.
3. **Fit (RankSVM / Bradley–Terry, convex, seconds).** Margin loss `w·φ(preferred) + m ≤ w·φ(other)` over {human≺engine, human≺perturbed}, L2-regularized, weights constrained **non-negative** (so no term inverts into a connectivity incentive), held-out split for overfit. `pin_crowd` gets a *learned* weight that supersedes the hand-derived one — confirming or correcting Step B.
4. **VLM only at the end, as oracle not objective.** Validate the learned `w` on the live boards + the four references with `schematic_critic.py`; ship only on a strict win over the current hand-tuned `w`, with the current weights as the always-available fallback. Optionally a *narrow* second step — CMA-ES/BO over just the 3–4 VLM-sensitive weights (`spread`, `congestion`, `pin_crowd`, `stray`), **seeded at the learned `w`**, tightly box-bounded, every proposal gated on the netlist oracle (FIELD SURVEY rec (c), step two).
5. **Match-not-minimize refit for the two trap features.** Once weights are learned, convert `spread` and the long-net/`length` features from pure-minimize to **target-band** penalties centered at the human percentiles (sprawl ≈ 20–25, HUMAN TARGETS) — the single highest-value structural refit, but *after* the linear-rank fit proves the basin, so the band edit is validated against a known-good `w`, not a moving target.

`pin_crowd` is both the immediate defect fix and the proof-of-concept that the cost is a learnable linear ranker — it is the first feature added to a basis the whole corpus can then fit.

---

**Key file references:** `crates/sch-layout/src/floorplan.rs` — `proxy_cost` cohere addend (`:3185`, new term at `:3207`), routed `base` (`:4587`), `count_stray` (`:4770`, mirror for `count_pin_crowd`), `cohesion_targets` decoupling fallback (`:3071`, the `is_rail` one-line extension at `:3055`), `count_congestion` (`:4931`, the existing-but-wrong-granularity congestion), `is_power_net` (`:241`), `MULTISHEET_REFINE` gates (`:999`/`:2362`/`:5178`). Tests: `crates/sch-layout/tests/placement_snapshot.rs`, `crates/sch-layout/tests/floorplan_netlist.rs`.
---

## Implementation finding (iter 24) — the orphaned-cap is a NO-ROOM problem, not weak-pull

Implemented `decap_cohesion` (DECAP_HUG_W=2.5 stronger pull + PIN_CROWD_W=8 quadratic anti-stack),
gated on MULTISHEET_REFINE, added outside `layout_cost`'s `base`/`multiunit` (`+0.0` ⇒ snapshot
byte-identical, verified). Result on the IoT-gateway sensors sheet: **C5 stayed ~47 mm from U4** —
the stronger pull did NOT co-place it. ROOT (deeper than the plan assumed): `signal_anchor_centroid`
ALREADY targets C5 at U4's 3V3 pin and the pull fires, but co-placing C5 there requires the cap body
to OVERLAP the occupied area beside U4 (R10 pull-up + wires), and `overlaps` is a 1500× hard wall the
hug (≤ a few × distance) cannot overcome. And there is no crowd to relieve (C5 is the only cap). So
for a lone cap by a BUSY IC, neither hug nor pin_crowd helps — the IC needs RESERVED SPACE for its
decap during placement (the IC's effective footprint should include a decap slot), so co-placing
doesn't collide. That is the real missing lever: **reserve-a-decap-slot-beside-each-IC-power-pin**
(a footprint-inflation / keepout during the search), of which pin_crowd is only the anti-stack half.
