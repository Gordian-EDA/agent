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
        let form = form_modules(&problem.items, &problem.inc, &classes, &g, &anchors);
        if std::env::var_os("SPINE_DEBUG").is_some() {
            for (ci, c) in g.chains.iter().enumerate() {
                if !c.parts.is_empty() && !form.consumed.contains_key(&ci) {
                    let refs: Vec<&str> = c
                        .parts
                        .iter()
                        .map(|&p| problem.items[p].refdes.as_str())
                        .collect();
                    eprintln!(
                        "[spine] unconsumed chain {ci}: {:?} {} .. {} ({:?})",
                        refs,
                        c.a.net,
                        c.b.net,
                        c.role(&classes)
                    );
                }
            }
        }
        let scene = build_scene(&problem.items, &g, form, &classes);
        let dirs = pin_dirs(env, problem);
        let origins = arrange(&problem.items, &g, &scene, &dirs);

        let mut placed = vec![false; problem.items.len()];
        for (sn, node) in scene.nodes.iter().enumerate() {
            for p in &node.places {
                problem.items[p.item].at =
                    [origins[sn].x + p.offset.x, origins[sn].y + p.offset.y].into();
                problem.items[p.item].angle = p.angle;
                placed[p.item] = true;
            }
        }

        // ── Orphan sweep: EVERY item gets a position. Multi-unit stragglers sit
        // under a placed sibling; the rest stack in a column right of the sheet.
        let mut max_x: f64 = 0.0;
        let mut max_y: f64 = 0.0;
        for (i, it) in problem.items.iter().enumerate() {
            if placed[i] {
                max_x = max_x.max(it.at[0]);
                max_y = max_y.max(it.at[1]);
            }
        }
        let mut orphan_y = 0.0;
        let sibling: BTreeMap<String, usize> = problem
            .items
            .iter()
            .enumerate()
            .filter(|(i, _)| placed[*i])
            .map(|(i, it)| (it.refdes.clone(), i))
            .collect();
        for i in 0..problem.items.len() {
            if placed[i] {
                continue;
            }
            let refdes = problem.items[i].refdes.clone();
            if let Some(&s) = sibling.get(refdes.as_str()) {
                let base = problem.items[s].at;
                let h = problem.items[s].geom.approx_size();
                problem.items[i].at = [base[0], base[1] + h[1] + ROW_GAP].into();
            } else {
                problem.items[i].at = [max_x + COL_GAP * 2.0, orphan_y].into();
                orphan_y += problem.items[i].geom.approx_size()[1] + ROW_GAP;
            }
            placed[i] = true;
        }
        debug_assert!(placed.iter().all(|&p| p), "orphan sweep must place all");

        // ── Safety passes shared with the incumbent engines.
        if std::env::var_os("SPINE_DEBUG").is_some() {
            eprintln!("[spine] overlaps pre-decongest: {}", body_overlap_count(&problem.items));
        }
        decongest(&mut problem.items);
        normalize(&mut problem.items);

        // ── Routed self-check; fall back to anneal on correctness failure.
        let realizer = RoutedSheetRealizer::new(env, &problem.inc, &ir);
        let eval = RoutedEvaluator::new(&realizer);
        let breaks = eval.truthfulness_breaks(&problem.items);
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
        if (breaks > 0 || overlaps > 0) && std::env::var_os("SPINE_NO_FALLBACK").is_none() {
            if problem.options.debug_timing {
                eprintln!("[spine] falling back to anneal (breaks={breaks}, overlaps={overlaps})");
            }
            return anneal_place::Anneal.place(env, design, problem, Some(ir));
        }

        let warnings = eval.warnings(&problem.items);
        let crossings = eval.crossings(&problem.items);
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
