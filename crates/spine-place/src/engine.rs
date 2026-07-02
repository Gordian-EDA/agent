//! The `spine` placement engine: deterministic grammar typesetting.
//!
//! parse (net classes → chains) → modules → layered ordering → coordinates →
//! orphan sweep → decongest/normalize → routed self-check. Falls back to the
//! anneal engine when the shipped self-check finds truthfulness breaks or body
//! overlaps, so it is never worse than the incumbent on correctness.

use std::collections::BTreeMap;

use circuit_lang::model::Design;
use geom::Point2;
use kicad_env::KicadEnv;
use kicad_symbol::{PinDir as SymPinDir, SymbolMeta, find_pin};
use kicad_symbol::SymbolTable;

use sch_floorplan::contract::{
    PlacementEngine, PlacementOutput, RoutedEvaluator, RoutedSheetRealizer,
    SchematicPlaceProblem, body_overlap_count, decongest, infer_ir, normalize,
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
        env: &KicadEnv,
        design: &Design,
        problem: &mut SchematicPlaceProblem,
        ir: Option<LayoutIr>,
    ) -> PlacementOutput {
        // A caller-AUTHORED layout (sidecar/LLM cells or a relative grid) is the
        // author's arrangement, not a hint — half-honoring it reads worse than
        // either engine. Those boards keep the tuned anneal path. Our own
        // inferred frame below doesn't count: its cells are seeds, not intent.
        if let Some(authored) = ir
            .as_ref()
            .filter(|ir| !ir.place.is_empty() || !ir.grid.is_empty())
        {
            let authored = authored.clone();
            return anneal_place::Anneal.place(env, design, problem, Some(authored));
        }
        let ir = ir.unwrap_or_else(|| infer_ir(env, design));
        let t0 = std::time::Instant::now();

        // ── Parse: net classes, chain contraction, anchor set.
        let classes = classify_nets(&problem.inc, &ir);
        let g = contract(&problem.items, &problem.inc, &classes);
        let mut in_chain = vec![false; problem.items.len()];
        for c in &g.chains {
            for &p in &c.parts {
                in_chain[p] = true;
            }
        }
        let mut anchors: Vec<usize> = (0..problem.items.len())
            .filter(|&i| !in_chain[i])
            .collect();
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
        let form = form_modules(&problem.items, &problem_inc, &classes, &g, &anchors, None);
        let debug = std::env::var_os("SPINE_DEBUG").is_some();
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
        if std::env::var_os("SPINE_DEBUG").is_some() {
            for &a in &anchors {
                let it = &problem.items[a];
                let conn = it.pins.iter().filter(|(_, _, n)| n.is_some()).count();
                eprintln!(
                    "[anchor] {} part={} geom_pins={} connected={} pins={:?}",
                    it.refdes, it.part, it.geom.pins.len(), conn, it.pins
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
                    m.env_min.x, m.env_min.y, m.env_max.x, m.env_max.y
                );
            }
        }
        let scene = build_scene(&problem.items, &g, form, &classes);

        // One full placement variant: arrange (folded or not), commit, orphan
        // sweep, safety passes, truthfulness self-check with collinearity
        // stagger. Returns the metrics the fold A/B decides on.
        let realizer = RoutedSheetRealizer::new(env, &problem.inc, &ir);
        let eval = RoutedEvaluator::new(&realizer);
        let mut run_variant = |items: &mut Vec<sch_place::item::Item>, fold: bool| -> usize {
            let origins = arrange(items, &g, &scene, &dirs, fold);
            let mut placed = vec![false; items.len()];
            let commit = |origins: &[Point2],
                          items: &mut [sch_place::item::Item],
                          placed: &mut [bool]| {
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

            // Crystal clusters re-seat canonically (opt-in; kept only if it
            // doesn't ADD body overlaps — dual-crystal boards collide).
            if std::env::var_os("SPINE_IDIOM").is_some() {
                let before: Vec<_> = items.iter().map(|it| (it.at, it.angle)).collect();
                let overlaps_before = body_overlap_count(items);
                if sch_floorplan::contract::align_idiom_clusters(items, &ir) {
                    decongest(items);
                    if body_overlap_count(items) > overlaps_before {
                        for (it, (at, angle)) in items.iter_mut().zip(before) {
                            it.at = at;
                            it.angle = angle;
                        }
                    }
                }
            }
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
                decongest(items);
                normalize(items);
                breaks = eval.truthfulness_breaks(items);
            }
            breaks
        };

        let mut breaks = run_variant(&mut problem.items, false);

        // Fold A/B: a wide sheet re-runs with the layer sequence folded into
        // rows (the human page-wrap); kept only when strictly no worse on
        // breaks/overlaps/warnings — folding helps chain boards and can hurt
        // label-island boards, so it must prove itself per sheet.
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
                let unfolded: Vec<_> =
                    problem.items.iter().map(|it| (it.at, it.angle)).collect();
                let xa = eval.crossings(&problem.items);
                let a = (
                    breaks,
                    body_overlap_count(&problem.items),
                    xa.body + xa.ic,
                    crate::compact::labeled_nets(&problem.items, &problem_inc, &classes).len(),
                    eval.warnings(&problem.items),
                );
                let b_breaks = run_variant(&mut problem.items, true);
                let xb = eval.crossings(&problem.items);
                let b = (
                    b_breaks,
                    body_overlap_count(&problem.items),
                    xb.body + xb.ic,
                    crate::compact::labeled_nets(&problem.items, &problem_inc, &classes).len(),
                    eval.warnings(&problem.items),
                );
                if b <= a {
                    breaks = b_breaks;
                    if problem.options.debug_timing {
                        eprintln!("[spine] fold kept: {a:?} -> {b:?}");
                    }
                } else {
                    for (it, (at, angle)) in problem.items.iter_mut().zip(unfolded) {
                        it.at = at;
                        it.angle = angle;
                    }
                    if problem.options.debug_timing {
                        eprintln!("[spine] fold rejected: {a:?} vs {b:?}");
                    }
                }
            }
        }

        // Node-pack A/B: slide whole scene nodes left then up until they rest
        // against the packed field (the human sprawl gap), keep only if
        // breaks/overlaps/warnings hold.
        {
            let before: Vec<_> = problem.items.iter().map(|it| (it.at, it.angle)).collect();
            let xa = eval.crossings(&problem.items);
            let a = (
                breaks,
                body_overlap_count(&problem.items),
                xa.body + xa.ic,
                crate::compact::labeled_nets(&problem.items, &problem_inc, &classes).len(),
                eval.warnings(&problem.items),
            );
            let mut groups: Vec<Vec<usize>> = scene
                .nodes
                .iter()
                .map(|n| n.places.iter().map(|p| p.item).collect())
                .filter(|g: &Vec<usize>| !g.is_empty())
                .collect();
            let mut grouped = vec![false; problem.items.len()];
            for g in &groups {
                for &i in g {
                    grouped[i] = true;
                }
            }
            for i in 0..problem.items.len() {
                if !grouped[i] {
                    groups.push(vec![i]);
                }
            }
            crate::compact::pack_nodes(&mut problem.items, &groups, &problem_inc, &classes);
            decongest(&mut problem.items);
            normalize(&mut problem.items);
            let b_breaks = eval.truthfulness_breaks(&problem.items);
            let xb = eval.crossings(&problem.items);
            let b = (
                b_breaks,
                body_overlap_count(&problem.items),
                xb.body + xb.ic,
                crate::compact::labeled_nets(&problem.items, &problem_inc, &classes).len(),
                eval.warnings(&problem.items),
            );
            if b <= a {
                breaks = b_breaks;
                if problem.options.debug_timing {
                    eprintln!("[spine] squash kept: {a:?} -> {b:?}");
                }
            } else {
                for (it, (at, angle)) in problem.items.iter_mut().zip(before) {
                    it.at = at;
                    it.angle = angle;
                }
                if problem.options.debug_timing {
                    eprintln!("[spine] squash rejected: {a:?} vs {b:?}");
                }
            }
        }

        // Band A/B: same-type modules align into refdes-sorted columns/grids —
        // the "same things share an axis" aesthetic humans read first. Each
        // band gates INDIVIDUALLY: one colliding band must not veto the rest.
        for band in crate::bands::plan(&problem.items, &scene) {
            let before: Vec<_> = problem.items.iter().map(|it| (it.at, it.angle)).collect();
            let xa = eval.crossings(&problem.items);
            let a = (
                breaks,
                body_overlap_count(&problem.items),
                xa.body + xa.ic,
                crate::compact::labeled_nets(&problem.items, &problem_inc, &classes).len(),
                eval.warnings(&problem.items),
            );
            crate::bands::apply(&mut problem.items, &scene, &band);
            normalize(&mut problem.items);
            let b_breaks = eval.truthfulness_breaks(&problem.items);
            let xb = eval.crossings(&problem.items);
            let b = (
                b_breaks,
                body_overlap_count(&problem.items),
                xb.body + xb.ic,
                crate::compact::labeled_nets(&problem.items, &problem_inc, &classes).len(),
                eval.warnings(&problem.items),
            );
            if b <= a {
                breaks = b_breaks;
                if problem.options.debug_timing {
                    eprintln!("[spine] band {} kept: {a:?} -> {b:?}", band.key);
                }
            } else {
                for (it, (at, angle)) in problem.items.iter_mut().zip(before) {
                    it.at = at;
                    it.angle = angle;
                }
                if problem.options.debug_timing {
                    eprintln!("[spine] band {} rejected: {a:?} vs {b:?}", band.key);
                }
            }
        }


        let overlaps = body_overlap_count(&problem.items);
        if overlaps > 0 && std::env::var_os("SPINE_DEBUG").is_some() {
            use sch_floorplan::contract::item_rect;
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
        let through_body = crossings.body + crossings.ic;
        if (breaks > 0 || overlaps > 0 || through_body > 4)
            && std::env::var_os("SPINE_NO_FALLBACK").is_none()
        {
            if problem.options.debug_timing {
                eprintln!(
                    "[spine] falling back to anneal (breaks={breaks}, overlaps={overlaps}, through_body={through_body})"
                );
            }
            return anneal_place::Anneal.place(env, design, problem, Some(ir));
        }

        let warnings = eval.warnings(&problem.items);
        if warnings > 0 && std::env::var_os("SPINE_DEBUG").is_some() {
            if let Ok(mut w) = realizer.realize_writer(
                None,
                &problem.items,
                sch_floorplan::contract::RouteRealization::ShippedSheet,
            ) {
                w.set_frame(true);
                w.prepare();
                for msg in w.layout_warnings() {
                    eprintln!("[spine] warn: {msg}");
                }
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
fn pin_dirs(env: &KicadEnv, problem: &SchematicPlaceProblem) -> BTreeMap<(usize, String), PinDir> {
    let table = SymbolTable::from_env(env);
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
