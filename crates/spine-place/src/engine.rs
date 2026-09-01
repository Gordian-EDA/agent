//! The `spine` placement engine: deterministic grammar typesetting.
//!
//! parse (net classes → chains) → modules → layered ordering → coordinates →
//! orphan sweep → decongest/normalize → routed self-check.

use std::collections::{BTreeMap, BTreeSet};

use circuit_lang::model::Design;
use geom::Point2;
use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use kicad_symbol::{PinDir as SymPinDir, SymbolMeta, find_pin};

use sch_floorplan::contract::{
    PlacementEngine, PlacementOutput, RoutedEvaluator, RoutedSheetRealizer, SchematicPlaceProblem,
};
use sch_floorplan::engine_support::{
    apply_cells, assign_cells, body_overlap_count, decongest, normalize, relation_viol,
    repair_relations,
};
use sch_place::ir::LayoutIr;
use sch_place::place::PlaceResult;

use crate::chain::contract;
use crate::module::form_modules;
use crate::net::classify_nets;
use crate::order::{PinDir, arrange};
use crate::scene::build_scene;

const COL_GAP: f64 = 10.16;
const ROW_GAP: f64 = 7.62;

/// Deterministic grammar-typesetting engine.
pub struct SpinePlace;

impl PlacementEngine for SpinePlace {
    fn name(&self) -> &'static str {
        "spine"
    }

    fn place(
        &self,
        env: &KicadInstallation,
        design: &Design,
        problem: &mut SchematicPlaceProblem,
        ir: Option<LayoutIr>,
    ) -> PlacementOutput {
        // Authored cells and grids are input to this engine, not a reason to
        // silently substitute a sibling engine. Engine selection remains a
        // caller-owned decision.
        let ir = ir.unwrap_or_else(|| {
            sch_floorplan::floorplan::infer_ir_with_options(env, design, problem.options)
        });
        for item in &mut problem.items {
            item.frozen = ir.frozen.contains(&item.refdes);
        }

        // Two-pass label fixpoint: pass 1 reserves optimistically (wire-first).
        // If warnings remain, pass 2 re-forms with the nets that ACTUALLY
        // realized as labels reserving their full names — breaking the
        // reserve→spread→label fixpoint (designed into form_modules, wired
        // here). Keep whichever pass measures better.
        let out1 = self.place_pass(env, design, problem, ir.clone(), None);
        if out1.result.warnings == 0 || out1.result.engine != "spine" {
            return out1;
        }
        let classes = classify_nets(&problem.inc, &ir);
        let labeled = crate::compact::labeled_nets(&problem.items, &problem.inc, &classes);
        if labeled.is_empty() {
            return out1;
        }
        let items1: Vec<_> = problem.items.iter().map(|it| (it.at, it.angle)).collect();
        let out2 = self.place_pass(env, design, problem, ir, Some(labeled));
        let key = |o: &PlacementOutput| {
            (
                o.result.truthfulness_breaks,
                o.result.warnings,
                o.result.crossings.total(),
            )
        };
        if out2.result.engine == "spine" && key(&out2) < key(&out1) {
            if problem.options.debug_timing {
                eprintln!(
                    "[spine] label pass-2 kept: warn {} -> {}",
                    out1.result.warnings, out2.result.warnings
                );
            }
            out2
        } else {
            for (it, (at, angle)) in problem.items.iter_mut().zip(items1) {
                it.at = at;
                it.angle = angle;
            }
            if problem.options.debug_timing {
                eprintln!(
                    "[spine] label pass-2 rejected: {:?} vs {:?}",
                    key(&out1),
                    key(&out2)
                );
            }
            out1
        }
    }
}

impl SpinePlace {
    fn place_pass(
        &self,
        env: &KicadInstallation,
        _design: &Design,
        problem: &mut SchematicPlaceProblem,
        ir: LayoutIr,
        labeled: Option<std::collections::BTreeSet<String>>,
    ) -> PlacementOutput {
        let t0 = std::time::Instant::now();
        // Spine owns the free-form grammar, but inferred frozen idioms are a
        // physical layout contract just like they are in the anneal engine.
        // Cache their canonical cell poses and re-seat them after each grammar
        // variant; decongestion then moves only loose bystanders out of the way.
        let canonical_frozen = {
            let mut canonical = problem
                .items
                .iter()
                .filter(|item| item.frozen)
                .cloned()
                .collect::<Vec<_>>();
            let cells = assign_cells(&canonical, &ir);
            apply_cells(&mut canonical, &cells);
            normalize(&mut canonical);
            let poses = canonical
                .into_iter()
                .map(|item| (item.refdes, (item.at, item.angle)))
                .collect::<BTreeMap<_, _>>();
            problem
                .items
                .iter()
                .map(|item| poses.get(&item.refdes).copied())
                .collect::<Vec<_>>()
        };
        let seat_frozen = |items: &mut [sch_place::item::Item]| {
            for (item, pose) in items.iter_mut().zip(&canonical_frozen) {
                if let Some((at, angle)) = pose {
                    item.at = *at;
                    item.angle = *angle;
                }
            }
        };

        // ── Parse: net classes, chain contraction, anchor set.
        let classes = classify_nets(&problem.inc, &ir);
        let g = contract(&problem.items, &problem.inc, &classes);
        let mut in_chain = vec![false; problem.items.len()];
        for c in &g.chains {
            for &p in &c.parts {
                in_chain[p] = true;
            }
        }
        let mut anchors: Vec<usize> = (0..problem.items.len()).filter(|&i| !in_chain[i]).collect();
        let conn = |i: usize| {
            problem.items[i]
                .pins
                .iter()
                .filter(|(_, _, n)| n.is_some())
                .count()
        };
        anchors.sort_by(|&a, &b| {
            conn(b)
                .cmp(&conn(a))
                .then(problem.items[a].refdes.cmp(&problem.items[b].refdes))
        });

        // ── Modules + scene + ordering + coordinates.
        let dirs = pin_dirs(env, problem);
        let problem_inc = problem.inc.clone();
        let port_nets: BTreeSet<String> = ir.ports.keys().cloned().collect();
        let form = form_modules(
            &problem.items,
            &problem_inc,
            &classes,
            &g,
            &anchors,
            labeled.as_ref(),
            &port_nets,
        );
        let debug = problem.options.debug_timing;
        if debug {
            for (ci, c) in g.chains.iter().enumerate() {
                if !c.parts.is_empty() && !form.consumed.contains_key(&ci) {
                    let refs: Vec<&str> = c
                        .parts
                        .iter()
                        .map(|&p| problem.items[p].refdes.as_str())
                        .collect();
                    eprintln!(
                        "[spine] unconsumed chain {ci}: {refs:?} {} .. {} ({:?})",
                        c.a.net,
                        c.b.net,
                        c.role(&classes)
                    );
                }
            }
        }
        if debug {
            for &a in &anchors {
                let it = &problem.items[a];
                let conn = it.pins.iter().filter(|(_, _, n)| n.is_some()).count();
                eprintln!(
                    "[anchor] {} part={} geom_pins={} connected={} pins={:?}",
                    it.refdes,
                    it.part,
                    it.geom.pins.len(),
                    conn,
                    it.pins
                );
            }
            for m in &form.modules {
                let sats: Vec<String> = m
                    .sats
                    .iter()
                    .map(|s| {
                        format!(
                            "{}@({:.0},{:.0})",
                            problem.items[s.item].refdes, s.offset.x, s.offset.y
                        )
                    })
                    .collect();
                eprintln!(
                    "[module] {} env=({:.0},{:.0})..({:.0},{:.0}) sats={sats:?}",
                    problem.items[m.anchor].refdes,
                    m.env_min.x,
                    m.env_min.y,
                    m.env_max.x,
                    m.env_max.y
                );
            }
        }
        let scene = build_scene(&problem.items, &g, form, &classes, labeled.as_ref());

        // One full placement variant: arrange (folded or not), commit, orphan
        // sweep, safety passes, truthfulness self-check with collinearity
        // stagger. Returns the metrics the fold A/B decides on.
        let realizer = RoutedSheetRealizer::new(env, &problem.inc, &ir, problem.options);
        let eval = RoutedEvaluator::new(&realizer);
        // Bundle-freed nodes: every wired chain of the node rides a BUNDLE (>=4
        // parallel nets between one item pair — always realized as labels), so
        // the node is effectively free for shelf packing, whatever its spans.
        let bundle_free: std::collections::BTreeSet<usize> = {
            let mut pair_count: BTreeMap<(usize, usize), usize> = BTreeMap::new();
            for (net, pins) in problem_inc.iter() {
                if classes.get(net).is_some_and(|c| c.is_rail()) || pins.len() != 2 {
                    continue;
                }
                let (a, b) = (pins[0].0, pins[1].0);
                if a != b {
                    *pair_count.entry((a.min(b), a.max(b))).or_default() += 1;
                }
            }
            // A pair is label-bound when it is a BUNDLE (>=4 parallel nets) or
            // when both sides are connectors — a connector panel interlinks by
            // labels by convention, never by drawn harness wires.
            let bundled_pair = |a: usize, b: usize| {
                pair_count.get(&(a.min(b), a.max(b))).copied().unwrap_or(0) >= 4
                    || (circuit_graph::netclass::is_connector_like(&problem.items[a].part)
                        && circuit_graph::netclass::is_connector_like(&problem.items[b].part))
            };
            let node_anchor_item = |v: usize| scene.nodes[v].anchor;
            (0..scene.nodes.len())
                .filter(|&v| {
                    let mut any = false;
                    for (ci, (ea, eb)) in scene.ends.iter() {
                        let (Some(x), Some(y)) = (ea, eb) else {
                            continue;
                        };
                        if *x != v && *y != v {
                            continue;
                        }
                        any = true;
                        let c = &g.chains[*ci];
                        if !c.parts.is_empty() {
                            return false;
                        }
                        let (Some(ia), Some(ib)) = (node_anchor_item(*x), node_anchor_item(*y))
                        else {
                            return false;
                        };
                        if !bundled_pair(ia, ib) {
                            return false;
                        }
                    }
                    any
                })
                .collect()
        };

        let run_variant = |items: &mut Vec<sch_place::item::Item>,
                           v: crate::order::Variants|
         -> usize {
            let origins = arrange(items, &g, &scene, &dirs, v, &bundle_free);
            let mut placed = vec![false; items.len()];
            let commit =
                |origins: &[Point2], items: &mut [sch_place::item::Item], placed: &mut [bool]| {
                    for (sn, node) in scene.nodes.iter().enumerate() {
                        for p in &node.places {
                            items[p.item].at =
                                [origins[sn].x + p.offset.x, origins[sn].y + p.offset.y].into();
                            items[p.item].angle = p.angle;
                            placed[p.item] = true;
                        }
                    }
                };
            commit(&origins, items, &mut placed);

            // Orphan sweep: EVERY item gets a position. Multi-unit stragglers
            // sit under a placed sibling; the rest stack right of the sheet.
            let mut max_x: f64 = 0.0;
            for (i, it) in items.iter().enumerate() {
                if placed[i] {
                    max_x = max_x.max(it.at[0]);
                }
            }
            let mut orphan_y = 0.0;
            let sibling: BTreeMap<String, usize> = items
                .iter()
                .enumerate()
                .filter(|(i, _)| placed[*i])
                .map(|(i, it)| (it.refdes.clone(), i))
                .collect();
            for i in 0..items.len() {
                if placed[i] {
                    continue;
                }
                let refdes = items[i].refdes.clone();
                if let Some(&s) = sibling.get(refdes.as_str()) {
                    let base = items[s].at;
                    let h = items[s].geom.approx_size();
                    items[i].at = [base[0], base[1] + h[1] + ROW_GAP].into();
                } else {
                    items[i].at = [max_x + COL_GAP * 2.0, orphan_y].into();
                    orphan_y += items[i].geom.approx_size()[1] + ROW_GAP;
                }
                placed[i] = true;
            }
            debug_assert!(placed.iter().all(|&p| p), "orphan sweep must place all");
            seat_frozen(items);

            // Crystal clusters re-seat canonically (opt-in; kept only if it
            // doesn't ADD body overlaps — dual-crystal boards collide).
            decongest(items);
            normalize(items);

            let mut breaks = eval.truthfulness_breaks(items);
            // Exact-collinearity repair: stagger co-columnar scene nodes one
            // grid step and re-check — removes the coincidence class wholesale.
            if breaks > 0 {
                let mut staggered = origins.clone();
                for (sn, o) in staggered.iter_mut().enumerate() {
                    o.x += (sn % 5) as f64 * 1.27;
                }
                let mut placed2 = vec![false; items.len()];
                commit(&staggered, items, &mut placed2);
                seat_frozen(items);
                decongest(items);
                normalize(items);
                breaks = eval.truthfulness_breaks(items);
            }
            breaks
        };

        // Shape-weighted sheet area (cm², landscape-1.4 target): the tie-break
        // that lets a page-shaped layout beat an equally-clean BANNER — raw
        // area always prefers the banner and shape never wins.
        let sheet_area = |items: &[sch_place::item::Item]| -> i64 {
            use sch_floorplan::engine_support::item_rect;
            let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
            for it in items {
                let r = item_rect(it, it.at);
                lo[0] = lo[0].min(r.min_x);
                lo[1] = lo[1].min(r.min_y);
                hi[0] = hi[0].max(r.max_x);
                hi[1] = hi[1].max(r.max_y);
            }
            let (w, h) = ((hi[0] - lo[0]).max(1.0), (hi[1] - lo[1]).max(1.0));
            let aspect = w / h;
            let shape = 1.0 + (aspect / 1.4 - 1.0).abs().min(3.0);
            ((w * h * shape) / 100.0) as i64
        };
        // One A/B gate serves every self-proving pass: snapshot, measure the
        // 7-tuple (breaks, overlaps, relation violations, through-body, warnings,
        // labels, area — area zeroed when the pass shouldn't trade shape), apply, re-measure,
        // keep on `b <= a` else restore. Returns whether the variant stuck.
        let dbg = problem.options.debug_timing;
        let mut breaks = run_variant(&mut problem.items, crate::order::Variants::default());
        let measure = |items: &Vec<sch_place::item::Item>, brk: usize, with_area: bool| {
            let x = eval.crossings(items);
            (
                brk,
                body_overlap_count(items),
                // The author's relational intent ranks above aesthetics: every
                // self-proving pass below self-rejects if it would break a relation.
                relation_viol(items, &ir),
                x.body + x.ic,
                eval.warnings(items),
                crate::compact::labeled_nets(items, &problem_inc, &classes).len(),
                if with_area { sheet_area(items) } else { 0 },
            )
        };
        let ab_gate = |items: &mut Vec<sch_place::item::Item>,
                       breaks: &mut usize,
                       name: &str,
                       with_area: bool,
                       apply: &mut dyn FnMut(&mut Vec<sch_place::item::Item>) -> usize|
         -> bool {
            let before: Vec<_> = items.iter().map(|it| (it.at, it.angle)).collect();
            let a = measure(items, *breaks, with_area);
            let b_breaks = apply(items);
            let b = measure(items, b_breaks, with_area);
            if b <= a {
                *breaks = b_breaks;
                if dbg {
                    eprintln!("[spine] {name} kept: {a:?} -> {b:?}");
                }
                true
            } else {
                for (it, (at, angle)) in items.iter_mut().zip(before) {
                    it.at = at;
                    it.angle = angle;
                }
                if dbg {
                    eprintln!("[spine] {name} rejected: {a:?} vs {b:?}");
                }
                false
            }
        };
        use crate::order::Variants;

        // Strap column: stub islets in one dedicated refdes-sorted column (the
        // human "straps region").
        let strap_on = ab_gate(
            &mut problem.items,
            &mut breaks,
            "strap column",
            false,
            &mut |it| {
                run_variant(
                    it,
                    Variants {
                        strap_col: true,
                        ..Default::default()
                    },
                )
            },
        );
        // Shelf: free label-island modules in a ~square block instead of the
        // wide banner their weak junction edges produce; area breaks ties.
        let shelf_on = ab_gate(&mut problem.items, &mut breaks, "shelf", true, &mut |it| {
            run_variant(
                it,
                Variants {
                    strap_col: strap_on,
                    shelf: true,
                    ..Default::default()
                },
            )
        });
        // Hop-align: junction-hop port alignment; vertical snaps can merge
        // nets, and breaks lead the tuple, so a merging alignment self-rejects.
        let hop_on = ab_gate(
            &mut problem.items,
            &mut breaks,
            "hop-align",
            true,
            &mut |it| {
                run_variant(
                    it,
                    Variants {
                        strap_col: strap_on,
                        shelf: shelf_on,
                        hop_align: true,
                        ..Default::default()
                    },
                )
            },
        );
        // Fold: a wide sheet re-runs with the layer sequence folded into rows
        // (the human page-wrap); only attempted past the banner threshold.
        {
            let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
            for it in problem.items.iter() {
                lo[0] = lo[0].min(it.at[0]);
                lo[1] = lo[1].min(it.at[1]);
                hi[0] = hi[0].max(it.at[0]);
                hi[1] = hi[1].max(it.at[1]);
            }
            let (w, h) = (hi[0] - lo[0], hi[1] - lo[1]);
            if w > 1.8 * h.max(30.0) {
                ab_gate(&mut problem.items, &mut breaks, "fold", false, &mut |it| {
                    run_variant(
                        it,
                        Variants {
                            fold: true,
                            strap_col: strap_on,
                            shelf: shelf_on,
                            hop_align: hop_on,
                        },
                    )
                });
            }
        }
        // Node-pack: slide whole scene nodes left then up against the packed
        // field (the human sprawl gap).
        ab_gate(
            &mut problem.items,
            &mut breaks,
            "squash",
            false,
            &mut |it| {
                let mut groups: Vec<Vec<usize>> = scene
                    .nodes
                    .iter()
                    .map(|n| n.places.iter().map(|p| p.item).collect())
                    .filter(|g: &Vec<usize>| !g.is_empty())
                    .collect();
                let mut grouped = vec![false; it.len()];
                for g in &groups {
                    for &i in g {
                        grouped[i] = true;
                    }
                }
                for (i, is_grouped) in grouped.iter().enumerate() {
                    if !is_grouped {
                        groups.push(vec![i]);
                    }
                }
                crate::compact::pack_nodes(it, &groups, &problem_inc, &classes);
                seat_frozen(it);
                decongest(it);
                normalize(it);
                eval.truthfulness_breaks(it)
            },
        );
        // Bands: same-type modules align into refdes-sorted columns/grids —
        // each band gates individually so one colliding band can't veto the rest.
        for band in crate::bands::plan(&problem.items, &scene) {
            if debug {
                let refs: Vec<&str> = band
                    .members
                    .iter()
                    .filter_map(|&sn| {
                        scene.nodes[sn]
                            .places
                            .first()
                            .map(|p| problem.items[p.item].refdes.as_str())
                    })
                    .collect();
                eprintln!("[band] {} members: {refs:?}", band.key);
            }
            ab_gate(
                &mut problem.items,
                &mut breaks,
                &format!("band {}", band.key),
                false,
                &mut |it| {
                    crate::bands::apply(it, &scene, &band);
                    normalize(it);
                    eval.truthfulness_breaks(it)
                },
            );
        }

        // The grammar typesetter has no relational move of its own, so its intent is
        // honoured by PROJECTION: run the shared repair, then keep it only if the same
        // gate every other pass answers to says it did not regress anything.
        ab_gate(
            &mut problem.items,
            &mut breaks,
            "relations",
            false,
            &mut |it| {
                repair_relations(it, &ir);
                seat_frozen(it);
                decongest(it);
                normalize(it);
                eval.truthfulness_breaks(it)
            },
        );

        let overlaps = body_overlap_count(&problem.items);
        if overlaps > 0 && problem.options.debug_timing {
            use sch_floorplan::engine_support::item_rect;
            for i in 0..problem.items.len() {
                for j in (i + 1)..problem.items.len() {
                    let (a, b) = (&problem.items[i], &problem.items[j]);
                    if item_rect(a, a.at).overlaps(&item_rect(b, b.at)) {
                        eprintln!(
                            "[spine] overlap {}({}) at {:?} vs {}({}) at {:?}",
                            a.refdes, a.unit, a.at, b.refdes, b.unit, b.at
                        );
                    }
                }
            }
        }
        if problem.options.debug_timing {
            eprintln!(
                "[spine] {} items, {} nodes, {} chains -> breaks={breaks} overlaps={overlaps} in {:.1?}",
                problem.items.len(),
                scene.nodes.len(),
                g.chains.len(),
                t0.elapsed()
            );
        }
        let crossings = eval.crossings(&problem.items);
        let warnings = eval.warnings(&problem.items);
        if warnings > 0
            && problem.options.debug_timing
            && let Ok(mut w) = realizer.realize_writer(
                None,
                &problem.items,
                sch_floorplan::contract::RouteRealization::ShippedSheet,
            )
        {
            w.set_frame(true);
            w.prepare();
            for msg in w.layout_warnings() {
                eprintln!("[spine] warn: {msg}");
            }
        }
        PlacementOutput {
            result: PlaceResult {
                engine: "spine".into(),
                truthfulness_breaks: breaks,
                warnings,
                crossings,
                cost: (warnings * 10 + crossings.total()) as f64,
            },
            ir,
        }
    }
}

/// (item, pin) → flow direction, from the symbol library's electrical types.
fn pin_dirs(
    env: &KicadInstallation,
    problem: &SchematicPlaceProblem,
) -> BTreeMap<(usize, String), PinDir> {
    let table = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let mut meta_cache: BTreeMap<String, Option<SymbolMeta>> = BTreeMap::new();
    let mut dirs = BTreeMap::new();
    for (i, it) in problem.items.iter().enumerate() {
        let meta = meta_cache
            .entry(it.part.clone())
            .or_insert_with(|| table.symbol(&it.part));
        let Some(meta) = meta else { continue };
        for (num, _name, net) in &it.pins {
            if net.is_none() {
                continue;
            }
            let Some(pm) = find_pin(&meta.pins, num) else {
                continue;
            };
            let dir = match pm.dir {
                SymPinDir::Out => Some(PinDir::Source),
                SymPinDir::In => Some(PinDir::Sink),
                _ => None,
            };
            if let Some(dir) = dir {
                dirs.insert((i, num.clone()), dir);
            }
        }
    }
    dirs
}
