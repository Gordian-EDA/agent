//! `place::wire` — the orthogonal elbow router: per-net signal routing
//! (`route_signal`/`route_trunk`), port-exit geometry, and power-rail riser
//! planning + emission (`assign_rail_levels`, `plan_riser_offsets`, `emit_rail`).

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use kicad::KicadInstallation;

use crate::write::SchematicWriter;
use geom::{Dir, EPS, ParentForest, Rect};

use circuit_graph::netclass::{is_connector_like, is_ground};
use sch_model::item::{Incidence, Item};
use sch_model::route::SchRouter;

use sch_model::ir::{Band, LayoutIr, Side};

// ---------------------------------------------------------------------------
// Wiring: rails, signal routing, ports.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(crate) fn wire(
    router: &dyn SchRouter,
    env: &KicadInstallation,
    w: &mut SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
    flag_points: &mut BTreeMap<String, ([f64; 2], f64)>,
) -> io::Result<()> {
    w.set_weld_guard(true);
    let refdes_of = |i: usize| items[i].refdes.clone();
    // Endpoints of every net first, so all rails can share common bands.
    let mut net_eps: BTreeMap<String, Vec<([f64; 2], Dir)>> = BTreeMap::new();
    for (net, pins) in inc {
        let mut eps: Vec<([f64; 2], Dir)> = Vec::new();
        for (i, num) in pins {
            for (ep, dir) in w.pin_dirs(env, &refdes_of(*i), num)? {
                eps.push((ep, dir));
            }
        }
        if !eps.is_empty() {
            net_eps.insert(net.clone(), eps);
        }
    }

    // Rail y per net. Rails in a band share a base y so they align, but two
    // rails whose x-ranges OVERLAP (e.g. VCC3V3 and VCCD flanking one IC) must
    // sit on different rows or their wires would merge into one net. Assign
    // y-levels by greedy interval colouring.
    let rail_y_map = assign_rail_levels(&net_eps, ir);

    // Fan colliding rail risers off shared columns so two rails never merge into
    // one net (the stacked-BGA-balls GND/1V2 short).
    let riser_offsets = plan_riser_offsets(&net_eps, ir, &rail_y_map);

    // Solid symbol bodies for local power-glyph orientation. Unlike the padded
    // placement rectangles, these put pin tips on the boundary, so an outward power
    // marker merely touches its served body while an inward marker overlaps it.
    let power_keepouts: Vec<Rect> = items.iter().map(item_solid_rect).collect();

    // 2-pin body segments, so a rail riser can JOG around a part body it would
    // otherwise be drawn straight through — the stacked same-rail cap column a
    // greedy assignment can't always pull apart.
    let bodies: Vec<([f64; 2], [f64; 2])> = items
        .iter()
        .filter(|it| it.geom.pins.len() == 2)
        .filter_map(|it| {
            let (n0, n1) = (&it.geom.pins[0].number, &it.geom.pins[1].number);
            match (
                w.pin_dirs(env, &it.refdes, n0),
                w.pin_dirs(env, &it.refdes, n1),
            ) {
                (Ok(d0), Ok(d1)) => match (d0.first(), d1.first()) {
                    (Some((a, _)), Some((b, _))) => Some((*a, *b)),
                    _ => None,
                },
                _ => None,
            }
        })
        .collect();

    // Every pin on the sheet, tagged with its net, so a rail's lead-out/riser can never
    // be drawn onto a FOREIGN pin — KiCAD welds a wire that ends on or passes over one,
    // silently shorting the two nets. Phase B gives signal nets this guard through the
    // routing scene; rails are drawn before that scene exists, so they carry their own.
    // The sheet this block is being added beside is foreign in exactly the same way, and
    // its terminals are the ones the block cannot see at all.
    let foreign_pins: Vec<([f64; 2], String)> = net_eps
        .iter()
        .flat_map(|(net, eps)| eps.iter().map(move |(p, _)| (*p, net.clone())))
        .chain(
            w.beside_terminals()
                .into_iter()
                .map(|(p, net)| ([p.x, p.y], net)),
        )
        .collect();

    // Phase A — rails (shared wires + stubs + power symbols), so their wires are
    // in the writer before we build the routing scene. `used_lanes` records every
    // drawn riser (x, y_lo, y_hi, net) so no later rail's riser can land exactly
    // collinear with a DIFFERENT net's — the post-jog re-collision the fan alone
    // cannot see.
    let mut used_lanes: Vec<(f64, f64, f64, String)> = Vec::new();
    for (net, eps) in &net_eps {
        if let Some(band) = ir.rails.get(net) {
            let flag = needs_flag.contains(net).then_some(&mut *flag_points);
            // Draw DISTRIBUTED local power symbols (`rail_y = None` ⇒ one power symbol
            // per pin) where the author marked the net (≥2 placed power symbols).
            // `emit_rail` distributes on its own account too, once it knows how long
            // the trunk and risers it would actually draw are.
            let distribute = ir.rail_locals.contains(net);
            let rail_y = rail_y_map.get(net).copied().filter(|_| !distribute);
            emit_rail(
                env,
                w,
                net,
                eps,
                *band,
                rail_y,
                flag,
                &riser_offsets,
                &bodies,
                &foreign_pins,
                &power_keepouts,
                &mut used_lanes,
            )?;
        }
    }

    // Phase B — the routing scene: component bodies become obstacles, rail wires
    // become foreign segments, and EVERY signal net's pins become foreign points
    // so one net's wire can never run onto another's pin (which would merge them
    // — the old TXD1/RXD1 short).
    let mut scene = w.route_scene();
    for (net, eps) in &net_eps {
        if ir.rails.contains_key(net) {
            continue;
        }
        for (p, _) in eps {
            scene.points.push(((*p).into(), net.clone()));
        }
    }
    let port_exits = plan_port_exits(env, w, items, inc, ir, &net_eps, &mut scene);

    // Phase C — route every signal/port net with the direction-aware,
    // obstacle-avoiding elbow router so wires leave pins along their facing
    // direction and detour around bodies (never through them).
    //
    // Long/crossing hops are delegated to net-label pairs (the human idiom) instead of
    // dragging a literal wire across the sheet. The policy is corpus-anchored (humans
    // keep ~0% of wires >50mm and ~0 crossings), and applies to EVERY board, not just
    // dense ones: the wire-dense small references (555/uart/grid) are exactly where
    // literal long crossing wires read worst. Mirrors the spread-rail →
    // local-power-symbol distribution above.
    // Fewest terminals first: a two-pin local hop claims its channel before a sprawling
    // bus runs through it, so the short connections that carry a sheet's readability are
    // drawn and the wide ones degrade to labels. Alphabetical order decided this before,
    // which is to say nothing decided it.
    let label_policy = LabelPolicy::default();
    let mut order: Vec<&String> = net_eps.keys().collect();
    order.sort_by_key(|net| (net_eps[*net].len(), (*net).clone()));
    for net in order {
        let eps = &net_eps[net];
        if ir.rails.contains_key(net) {
            continue;
        }
        route_signal(
            router,
            env,
            w,
            items,
            inc,
            net,
            eps,
            port_exits.get(net).copied(),
            label_policy,
            &mut scene,
        )?;
    }

    // A power-input net that is not a rail (a barrel-jack VIN feeding a
    // regulator, say) still needs its PWR_FLAG or ERC reports it undriven; the
    // rail phase never draws it, so anchor the flag at the net's first pin.
    for net in needs_flag {
        if flag_points.contains_key(net) {
            continue;
        }
        if let Some((ep, _)) = net_eps.get(net).and_then(|eps| eps.first()) {
            flag_points.insert(net.clone(), (*ep, 0.0));
        }
    }
    Ok(())
}

/// When the orthogonal router should promote a signal hop to a net-LABEL pair instead of
/// drawing the literal wire.
///
/// The judgement is about SHAPE, not length: a long straight run reads better than a short
/// snake, so what a hop may spend is a [`geom::RouteShape`] budget, and how much it may
/// spend depends only on how far apart its ends are.
///
///   * up to [`LABEL_LEN_MM`] — a LOCAL hop, [`geom::SHAPE_GENERAL`]: a Z, or one crossing
///     on an otherwise direct run.
///   * up to [`LONG_SIMPLE_LEN_MM`] — a LONG hop, [`geom::SHAPE_SIMPLE`]: straight, or one
///     corner with no detour. Humans do draw these; what they never draw is a long snake.
///   * beyond that, always a label.
///
/// Two riders. A hop shorter than [`CROSS_LABEL_LEN_MM`] keeps whatever legal route it has
/// — a tight cluster must not fragment into label spam over one crossing. And an endpoint
/// that could not seat a body-clear label forgives one crossing ([`CROWDED_FORGIVES`]),
/// because naming such a pin only moves the defect onto a label over a body.
#[derive(Clone, Copy)]
pub(crate) struct LabelPolicy {
    pub len_mm: f64,
    pub long_simple_len_mm: f64,
    pub cross_len_mm: f64,
}

impl LabelPolicy {
    pub(crate) fn default() -> Self {
        LabelPolicy {
            len_mm: LABEL_LEN_MM,
            long_simple_len_mm: LONG_SIMPLE_LEN_MM,
            cross_len_mm: CROSS_LABEL_LEN_MM,
        }
    }

    /// The shape budget for a hop whose ends are `direct` mm apart, or `None` when no
    /// wire is acceptable at that distance.
    fn budget(&self, direct: f64, label_clear: bool) -> Option<f64> {
        let base = match direct {
            d if d <= self.len_mm => geom::SHAPE_GENERAL,
            d if d <= self.long_simple_len_mm => geom::SHAPE_SIMPLE,
            _ => return None,
        };
        Some(base + if label_clear { 0.0 } else { CROWDED_FORGIVES })
    }

    /// Whether a routed path is worth drawing rather than naming.
    fn keeps(&self, path: &[::geom::Point2], crossings: usize, label_clear: bool) -> bool {
        let shape = geom::RouteShape::of(path, crossings);
        let direct = match (path.first(), path.last()) {
            (Some(a), Some(b)) => a.manhattan(*b),
            _ => return false,
        };
        let drawn = direct + shape.detour_mm;
        if drawn > self.long_simple_len_mm {
            return false;
        }
        if direct <= self.cross_len_mm {
            return true;
        }
        self.budget(direct, label_clear)
            .is_some_and(|budget| shape.cost() <= budget)
    }
}

/// Route one signal/port net's terminals as a tree (MST) with the direction-
/// aware elbow router. A port adds a virtual terminal just past the net's extent
/// on the named side, then a label there; failure falls back to per-pin labels.
#[allow(clippy::too_many_arguments)]
pub(crate) fn route_signal(
    router: &dyn SchRouter,
    env: &KicadInstallation,
    w: &mut SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    net: &str,
    eps: &[([f64; 2], Dir)],
    port_exit: Option<(Side, [f64; 2])>,
    label_policy: LabelPolicy,
    scene: &mut sch_model::route::RouteScene,
) -> io::Result<()> {
    let port = port_exit.map(|(side, _)| side);

    // Terminals: real pins (with outward dir) + the virtual port exit `plan_port_exits`
    // already settled and reserved.
    let mut terms: Vec<([f64; 2], Option<Dir>)> = eps.iter().map(|(p, d)| (*p, Some(*d))).collect();
    let port_idx = port_exit.map(|(_, at)| {
        terms.push((at, None));
        terms.len() - 1
    });

    if terms.len() < 2 {
        // A lone pin with no port is an intentionally-unconnected signal (e.g. an
        // unused connector RTS/CTS): mark it no-connect — the professional way to
        // show "deliberately dangling" — rather than leaving a floating named
        // label that ERC flags as an isolated pin.
        if let Some((i, num)) = inc.get(net).and_then(|p| p.first()) {
            w.add_no_connect(env, &items[*i].refdes, num)?;
        }
        return Ok(());
    }

    // A LOCAL node — terminals clustered with no component body between them —
    // is drawn as one clean trunk + stubs (a tee), not an MST of independent
    // elbows whose overlapping collinear runs over-junction the node.
    // A nudged singleton port is no longer a local two-terminal tee. Treating a
    // 25 mm collision-avoidance displacement as "local" draws a straight trunk
    // through every intervening pin before the obstacle-aware router gets a say.
    let local_tee_safe = port_idx.is_none_or(|pi| {
        eps.len() != 1
            || (terms[0].0[0] - terms[pi].0[0]).abs() + (terms[0].0[1] - terms[pi].0[1]).abs()
                <= 2.54 + EPS
    });
    if local_tee_safe && route_trunk(w, net, &terms, scene) {
        if let (Some(side), Some(pi)) = (port, port_idx) {
            w.add_cluster_label(net, terms[pi].0, side_dir(side), true);
        }
        return Ok(());
    }

    // Map each real-pin terminal (the first `eps.len()`) back to its (item, pin),
    // rebuilt in the same order `wire()` flattened `inc[net]` into `eps`, so a
    // disconnected component can be bridged with a net label on one of its pins.
    let mut term_pin: Vec<Option<(usize, String)>> = vec![None; terms.len()];
    {
        let mut k = 0;
        for (i, num) in inc.get(net).into_iter().flatten() {
            if let Ok(ds) = w.pin_dirs(env, &items[*i].refdes, num) {
                for _ in ds {
                    if k < eps.len() {
                        term_pin[k] = Some((*i, num.clone()));
                        k += 1;
                    }
                }
            }
        }
    }

    // Whether a NET LABEL on each terminal's pin would read CLEAR of nearby symbol
    // geometry — using the same boxes `layout_warnings` lints against (see
    // `obstacle_boxes`), so a "clear" terminal is clear to the lint. The label sits at
    // the stub end (pin + STUB_MM along the pin's outward dir); a virtual port exit (no
    // pin) is trivially clear. Drives BOTH the crossing-promotion (never strand a pin
    // whose only label collides geometry — the R2-over-U1 overlap) AND the bridge's
    // per-component pin pick.
    const STUB_MM: f64 = 3.81; // matches add_signal_label
    // Obstacle boxes for the label-clearance predictor — the SAME geometry
    // `layout_warnings` lints a net label against, so "clear" here ⇒ no overlap warning
    // there: every symbol's pin-name/number text boxes (the OWN symbol's pin text IS an
    // obstacle — a label over its own pin names is the artifact the lint catches), plus
    // every FOREIGN symbol's body bbox (full `approx_size`, the lint's extent). Only the
    // OWN body is exempt (a label on its own pin legitimately sits inside its generous
    // body bbox — the lint exempts that pairing).
    let obstacle_boxes = |it: usize| -> Vec<::geom::Rect> {
        let mut boxes = Vec::new();
        for (i, item) in items.iter().enumerate() {
            if i != it {
                let s = item.geom.approx_size();
                let quarter = ((item.angle / 90.0).round() as i64).rem_euclid(2) == 1;
                let (bw, bh) = if quarter { (s[1], s[0]) } else { (s[0], s[1]) };
                boxes.push(
                    [
                        item.at[0] - bw / 2.0,
                        item.at[1] - bh / 2.0,
                        item.at[0] + bw / 2.0,
                        item.at[1] + bh / 2.0,
                    ]
                    .into(),
                );
            }
            for pg in &item.geom.pins {
                boxes.extend(sch_model::text::pin_text_boxes(
                    pg,
                    item.at,
                    item.angle,
                    item.mirror,
                ));
            }
        }
        boxes
    };
    let label_clear = |it: usize, num: &str| -> bool {
        let Ok(ds) = w.pin_dirs(env, &items[it].refdes, num) else {
            return true;
        };
        let obstacles = obstacle_boxes(it);
        ds.iter().all(|(ep, dir)| {
            let v = dir.vec();
            let end = [ep[0] + v[0] * STUB_MM, ep[1] + v[1] * STUB_MM];
            let bx = sch_model::text::label_box(end, *dir, sch_model::text::text_width(net));
            !obstacles.iter().any(|r| bx.intersection(r).is_some())
        })
    };
    // Whether each terminal could carry a body-clear net label (a virtual port exit,
    // having no pin, trivially can). Gates the crossing-promotion (only name a hop whose
    // endpoints could both be cleanly labelled) and breaks ties in the bridge's pin pick.
    let term_label_clear: Vec<bool> = term_pin
        .iter()
        .map(|tp| tp.as_ref().is_none_or(|(it, num)| label_clear(*it, num)))
        .collect();
    // Whether the label EMITTER could actually seat a label on each terminal's pin —
    // the same ladder walk `label_stub` does below. `term_label_clear` only asks about
    // the default landing; a pin it rejects may still be seatable one notch further out,
    // and a pin it accepts may not be. The bridge must pick on what the emitter can do,
    // or it hands the writer a pin whose label lands on a neighbour's body.
    let term_label_seatable: Vec<bool> = term_pin
        .iter()
        .map(|tp| {
            tp.as_ref()
                .is_none_or(|(it, num)| label_stub(w, env, scene, &items[*it].refdes, num, net).1)
        })
        .collect();

    let pts: Vec<::geom::Point2> = terms.iter().map(|t| t.0.into()).collect();
    // Union-find over terminals: a successful edge merges its endpoints; a failed
    // one leaves them split. Route each edge as it succeeds and commit it to the
    // scene immediately so later edges detour around it (partial progress, never
    // the old all-or-nothing that label-bombed the whole net on one bad edge).
    let mut parent: Vec<usize> = (0..terms.len()).collect();
    let mut uf = ParentForest::new(&mut parent);
    let mut paths: Vec<Vec<::geom::Point2>> = Vec::new();
    for (i, j) in router.tree_edges(&pts) {
        let (a, da, b) = match (terms[i].1, terms[j].1) {
            (Some(d), _) => (pts[i], d, pts[j]),
            (None, Some(d)) => (pts[j], d, pts[i]),
            (None, None) => (pts[i], dir_toward(pts[i], pts[j]), pts[j]),
        };
        // A hop the policy will not draw at any shape is left unrouted so the union-find
        // leaves its endpoints split — the label-bridge below then names each side,
        // turning a long literal wire into a net-label pair (the human idiom).
        let direct = (a[0] - b[0]).abs() + (a[1] - b[1]).abs();
        if direct > label_policy.long_simple_len_mm {
            continue;
        }
        if let Some(p) = router.route_edge(a, da, b, net, scene) {
            let crossings = sch_model::route::path_crossings(&p, net, scene);
            if !label_policy.keeps(&p, crossings, term_label_clear[i] && term_label_clear[j]) {
                continue;
            }
            for seg in p.windows(2) {
                emit_routed_segment(w, scene, net, seg[0], seg[1]);
            }
            paths.push(p);
            uf.union_to(i, j);
        }
    }

    // SINGLE-PIN PORT whose short pin→exit hop the router couldn't place (dense FPGA GPIO banks at
    // 2.54mm pitch block each other's stubs): force the direct stub so the pin and its exit unify
    // into ONE component. Otherwise the bridge below emits a signal label on the pin (over the IC
    // body) AND the port label at the exit — the net renders twice (the BGA GPIO-bank defect). The
    // The hop is normally short + axis-aligned (single-pin ports follow the pin's own dir), but
    // collision avoidance may have nudged the exit through a neighbouring body. Only force the
    // direct wire while the original short-stub invariant still holds and the live routing scene
    // proves it clear. Otherwise the label bridge below keeps the two components electrically
    // joined by name without drawing a placement-induced short through adjacent pins.
    if eps.len() == 1
        && let Some(pi) = port_idx
        && uf.find(0) != uf.find(pi)
        && safe_forced_single_port_stub(pts[0], pts[pi], net, scene)
    {
        emit_routed_segment(w, scene, net, pts[0], pts[pi]);
        uf.union_to(0, pi);
    }

    // MULTI-PIN PORT whose local pins the MST couldn't join (an op-amp follower's OUT↔IN-
    // feedback the router can't wrap around the body — BLDC current_sense U6/ISENSE_W): force
    // a clean OVERHEAD wire so the local pins unify into ONE component named by the single port
    // label, instead of a duplicate local label on each split pin. The detour leaves each pin
    // along its facing dir then runs OUTSIDE the terminal bbox (above, else below), so it never
    // crosses a body — `path_ok` validates it against the live scene before commit. Only the
    // first `eps.len()` terminals are real pins; the port exit is excluded. Marked ports only,
    // and only when the route genuinely failed, so cleanly-routed nets stay byte-identical.
    if port.is_some() && eps.len() >= 2 {
        // The bodies the detour must clear: any scene solid the feedback pins straddle (the op-amp
        // body between OUT and IN-), unioned, so the band runs fully ABOVE or BELOW it like the
        // clean U5A/U5B follower loops — not just a hair off the pin row (which still grazes the
        // triangle). Fall back to the pin row when no straddled body is found.
        let (px_lo, px_hi) = eps.iter().fold((f64::MAX, f64::MIN), |(lo, hi), (p, _)| {
            (lo.min(p[0]), hi.max(p[0]))
        });
        let (py_lo, py_hi) = eps.iter().fold((f64::MAX, f64::MIN), |(lo, hi), (p, _)| {
            (lo.min(p[1]), hi.max(p[1]))
        });
        // Only the body the feedback pins actually straddle (overlaps their x-span AND their
        // y-span): an op-amp's units stack in one column, so an x-only test grabs the whole
        // column (band lands at the sheet edge, always blocked). Default to the pin row.
        let (mut by_lo, mut by_hi) = (py_lo, py_hi);
        for r in &scene.solids {
            if r[0] < px_hi - EPS && px_lo < r[2] - EPS && r[1] < py_hi + EPS && py_lo < r[3] + EPS
            {
                by_lo = by_lo.min(r[1]);
                by_hi = by_hi.max(r[3]);
            }
        }
        let lead = 2.54;
        let stub = |p: ::geom::Point2, d: Option<Dir>| -> ::geom::Point2 {
            match d {
                Some(Dir::East) => [geom::GRID_50_MIL.snap(p[0] + lead), p[1]].into(),
                Some(Dir::West) => [geom::GRID_50_MIL.snap(p[0] - lead), p[1]].into(),
                Some(Dir::North) => [p[0], geom::GRID_50_MIL.snap(p[1] - lead)].into(),
                Some(Dir::South) => [p[0], geom::GRID_50_MIL.snap(p[1] + lead)].into(),
                None => p,
            }
        };
        for k in 1..eps.len() {
            if uf.find(0) == uf.find(k) {
                continue; // already joined to pin 0's component by the MST
            }
            // Don't force an OVERHEAD detour across a long-haul gap: that recreates the very sheet-wide
            // wraparound the MST already declined (the i2c_sensors U4 SCL pin, ~110 mm from the rest of
            // the bus). Leave such a pin SPLIT so the label-bridge below names it instead — exactly as
            // the too-long MST hop already does, and as the sibling SDA pin already gets. The local
            // op-amp feedback case (pins a few mm apart) is well under the length, so it still forces
            // its clean loop.
            if (pts[0][0] - pts[k][0]).abs() + (pts[0][1] - pts[k][1]).abs() > label_policy.len_mm
            {
                continue;
            }
            let (pa, da) = (pts[0], terms[0].1);
            let (pb, db) = (pts[k], terms[k].1);
            let (sa, sb) = (stub(pa, da), stub(pb, db));
            // Try clear bands at growing distance, BELOW the body first (the conventional
            // follower loop drops under), then ABOVE.
            let mut bands: Vec<f64> = Vec::new();
            for step in 1..=6 {
                let pitch = geom::GRID_50_MIL.pitch();
                bands.push(geom::GRID_50_MIL.snap(by_hi + pitch * step as f64));
                bands.push(geom::GRID_50_MIL.snap(by_lo - pitch * step as f64));
            }
            for band_y in bands {
                let path = vec![
                    pa,
                    sa,
                    [sa[0], band_y].into(),
                    [sb[0], band_y].into(),
                    sb,
                    pb,
                ];
                if sch_model::route::path_ok(&path, net, scene) {
                    for seg in path.windows(2) {
                        if (seg[0][0] - seg[1][0]).abs() > EPS
                            || (seg[0][1] - seg[1][1]).abs() > EPS
                        {
                            emit_routed_segment(w, scene, net, seg[0], seg[1]);
                        }
                    }
                    uf.union_to(0, k);
                    break;
                }
            }
        }
    }

    // Bridge connected components by net name: every component must carry the net
    // somewhere. A component holding the port exit is named by the port label; any
    // other component gets one net label on a real pin. With one component the net
    // is fully wired and no label is emitted.
    //
    // Within a component, pick the pin whose net label reads CLEAR of every foreign
    // body (`term_label_clear`), breaking ties toward the part with the FEWEST pins (a
    // 2-pin satellite's stub reads into open space; an IC pin's label risks landing over
    // the chip body). Falls back to fewest-pins when no candidate is body-clear. Ties
    // keep the earlier terminal (deterministic).
    let pin_count = |i: usize| items[i].geom.pins.len();
    // Per-component: the chosen labelling pin and its score
    // `(emitter_can_seat_it, body_clear, -pin_count)`.
    let mut roots: BTreeMap<usize, Option<(usize, String)>> = BTreeMap::new();
    let mut score: BTreeMap<usize, (bool, bool, std::cmp::Reverse<usize>)> = BTreeMap::new();
    for k in 0..terms.len() {
        let r = uf.find(k);
        let slot = roots.entry(r).or_insert(None);
        let Some(pin) = &term_pin[k] else { continue };
        let cand = (
            term_label_seatable[k],
            term_label_clear[k],
            std::cmp::Reverse(pin_count(pin.0)),
        );
        if slot.is_none() || cand > score[&r] {
            *slot = Some(pin.clone());
            score.insert(r, cand);
        }
    }
    let port_root = port_idx.map(|pi| uf.find(pi));
    // Collision avoidance may nudge the virtual port exit beyond any route the
    // obstacle-aware router can reach. Such an isolated virtual root has no pin or
    // wire beneath it, so emitting its pennant would be a real ERC `label_dangling`.
    // Promote one real root's pin-attached fallback label to global instead; the
    // other real roots keep their same-name local labels and remain connected.
    let port_attached = port_root
        .and_then(|root| roots.get(&root))
        .is_some_and(Option::is_some);
    let global_fallback_root = if port_root.is_some() && !port_attached {
        roots
            .iter()
            .filter(|(_, pin)| pin.is_some())
            .max_by_key(|(root, _)| score.get(root).copied())
            .map(|(root, _)| *root)
    } else {
        None
    };
    if roots.len() > 1 {
        for (root, pin) in &roots {
            if Some(*root) == port_root && port_attached {
                continue; // named by the port label below
            }
            if let Some((i, num)) = pin {
                let (stub, _) = label_stub(w, env, scene, &items[*i].refdes, num, net);
                if Some(*root) == global_fallback_root {
                    w.add_global_signal_label_stub(env, &items[*i].refdes, num, net, stub)?;
                } else {
                    w.add_signal_label_stub(env, &items[*i].refdes, num, net, stub)?;
                }
                if let Ok(ds) = w.pin_dirs(env, &items[*i].refdes, num) {
                    for (p, _) in ds {
                        scene.points.push((p.into(), net.to_string()));
                    }
                }
            }
        }
    }

    // Junction dots: 3-way meets among the routed paths.
    let mut all = paths.clone();
    for segment in w.wire_segments_on_net(net) {
        all.push(vec![segment.a, segment.b]);
    }
    for j in crate::wire::junction_points(&all) {
        w.add_junction_on_net(j, net);
    }
    // A terminal landing inside another same-net segment is a T-join.
    for (p, _) in &terms {
        let interior = w.wire_segments_on_net(net).iter().any(|segment| {
            let point = ::geom::Point2::from(*p);
            let ends = point.near_eq(segment.a, EPS) || point.near_eq(segment.b, EPS);
            !ends && segment.contains_point(point)
        });
        if interior {
            w.add_junction_on_net(*p, net);
        }
    }
    // The port label sits at the virtual exit terminal, facing the edge.
    if port_attached && let (Some(side), Some(pi)) = (port, port_idx) {
        w.add_cluster_label(net, terms[pi].0, side_dir(side), true);
    }
    Ok(())
}

/// Emit only the portions of one routed segment not already covered by same-net geometry.
/// Fully covered spans are always reused; covered request endpoints in an existing
/// segment's interior receive junctions so the reuse is electrically attached.
fn emit_routed_segment(
    w: &mut SchematicWriter,
    scene: &mut sch_model::route::RouteScene,
    net: &str,
    a: ::geom::Point2,
    b: ::geom::Point2,
) {
    if let Some(covering) = scene.segments.iter().find(|existing| {
        existing.net == net
            && existing.segment.contains_point(a)
            && existing.segment.contains_point(b)
    }) {
        for at in [a, b] {
            let is_endpoint =
                at.near_eq(covering.segment.a, EPS) || at.near_eq(covering.segment.b, EPS);
            if !is_endpoint {
                w.add_junction_on_net(at, net);
            }
        }
        return;
    }
    let existing: Vec<_> = scene
        .segments
        .iter()
        .filter(|segment| segment.net == net)
        .map(|segment| segment.segment)
        .collect();
    for at in [a, b] {
        if existing.iter().any(|segment| {
            segment.contains_point(at) && !at.near_eq(segment.a, EPS) && !at.near_eq(segment.b, EPS)
        }) {
            w.add_junction_on_net(at, net);
        }
    }
    let mut uncovered = vec![::geom::Segment::new(a, b)];
    for covering in existing {
        uncovered = uncovered
            .into_iter()
            .flat_map(|segment| subtract_collinear_overlap(segment, covering))
            .collect();
    }
    for segment in uncovered {
        w.add_wire_on_net(segment.a, segment.b, net);
        scene
            .segments
            .push(sch_model::route::NetSegment::new(segment.a, segment.b, net));
    }
}

fn subtract_collinear_overlap(
    segment: ::geom::Segment,
    covering: ::geom::Segment,
) -> Vec<::geom::Segment> {
    if !segment.axis_aligned_collinear_overlap(covering) {
        return vec![segment];
    }
    let (dx, dy) = (segment.b.x - segment.a.x, segment.b.y - segment.a.y);
    let length_squared = dx * dx + dy * dy;
    let project = |point: ::geom::Point2| {
        ((point.x - segment.a.x) * dx + (point.y - segment.a.y) * dy) / length_squared
    };
    let lo = project(covering.a).min(project(covering.b)).clamp(0.0, 1.0);
    let hi = project(covering.a).max(project(covering.b)).clamp(0.0, 1.0);
    let point = |t: f64| ::geom::Point2::new(segment.a.x + dx * t, segment.a.y + dy * t);
    let mut remainder = Vec::with_capacity(2);
    if lo * segment.length() > EPS {
        remainder.push(::geom::Segment::new(segment.a, point(lo)));
    }
    if (1.0 - hi) * segment.length() > EPS {
        remainder.push(::geom::Segment::new(point(hi), segment.b));
    }
    remainder
}

/// Where the label bridge seats `net`'s label on `refdes`.`num` — the stub length outward
/// from the pin — and whether that landing actually reads clear.
///
/// One ladder, walked under the WRITER's own geometry (the same the readability lint uses),
/// serving both the choice of which pin in a component carries the net's label and the
/// emission of it. A predictor that disagreed with the emitter picked pins whose label the
/// writer then had to drop on a neighbouring body (the 555 `N_TR`-over-R1 warning). The
/// default 3.81 is tried first, so a pin whose usual landing is clear keeps it. When the
/// outward path is walled in — a neighbouring body sits in every landing further out — the
/// label tucks onto the pin endpoint itself, which is where the writer's stub retraction
/// would put it anyway and which the lint exempts against the pin's own body.
///
/// A rung whose anchor would MERGE the net with another (`anchor_merges`) is not a rung
/// at all: readability may be given up, truthfulness may not. When every rung merges —
/// which needs a foreign wire or pin over this pin's own tip, so only on a block drawn
/// beside existing content — the default landing comes back reported as not clear, and
/// the bridge picks another pin.
///
/// The pin TIP is not a rung. KiCAD draws the pin's number along the pin, so a label
/// sitting on the tip overprints it — the clearance predictor is blind to the label's own
/// symbol and so calls that landing clear, while the readability lint, which is not,
/// reports it. A walled-in pin keeps the nearest outward landing instead, and the writer's
/// own stub retraction still pulls it in where it must.
fn label_stub(
    w: &SchematicWriter,
    env: &KicadInstallation,
    scene: &sch_model::route::RouteScene,
    refdes: &str,
    num: &str,
    net: &str,
) -> (f64, bool) {
    const LADDER: [f64; 5] = [3.81, 6.35, 8.89, 11.43, 13.97];
    let Some((ep, dir)) = w
        .pin_dirs(env, refdes, num)
        .ok()
        .and_then(|ds| ds.first().copied())
    else {
        return (LADDER[0], true);
    };
    let v = dir.vec();
    let landing = |s: f64| {
        geom::GRID_50_MIL.snap_point(::geom::Point2::new(ep[0] + v.x * s, ep[1] + v.y * s))
    };
    let truthful: Vec<f64> = LADDER
        .into_iter()
        .filter(|&s| !anchor_merges(scene, landing(s), net))
        .collect();
    match truthful
        .iter()
        .find(|&&s| w.label_landing_clear(landing(s), dir, net, refdes))
    {
        Some(&s) => (s, true),
        None => (truthful.first().copied().unwrap_or(LADDER[0]), false),
    }
}

pub(crate) fn safe_forced_single_port_stub(
    pin: ::geom::Point2,
    exit: ::geom::Point2,
    net: &str,
    scene: &sch_model::route::RouteScene,
) -> bool {
    let dx = (pin[0] - exit[0]).abs();
    let dy = (pin[1] - exit[1]).abs();
    let short = dx + dy <= 2.54 + EPS;
    let axis_aligned = dx <= EPS || dy <= EPS;
    short && axis_aligned && sch_model::route::path_ok(&[pin, exit], net, scene)
}

/// Draw a multi-terminal node as one straight TRUNK with a drop from every terminal —
/// the way a person draws a node, rather than an MST of independent elbows whose
/// overlapping collinear runs over-junction it. Returns whether it applied; a node too
/// spread out, or one no legal trunk serves, falls through to the router.
///
/// The trunk lines worth trying are every terminal's own coordinate, the coordinates two
/// to eight grid OUTWARD along a terminal's pin, and four or six grid either side of a
/// terminal whose pin points ALONG the trunk — a bus running past a resistor. A terminal
/// facing the trunk drops straight onto it; one facing along it leaves its pin two grid
/// first and then turns. The cheapest whole tee wins: its length, plus a crossing's worth
/// of grid for every foreign wire it passes and a little for each extra wire it needs.
///
/// Nothing is drawn until the WHOLE tee — every drop and the trunk that joins them —
/// passes the full [`sch_model::route::path_ok`] test. Not just a body check: a trunk
/// drawn down an IC's pin column passes over the neighbouring pins, and KiCAD welds a
/// wire to every pin it crosses — the `mixed-signal-adc-frontend` `SDA`/`SCL` short,
/// where SCL's trunk ran from pin 10 straight down through pin 9 to its pull-up. The tee
/// is a shortcut PAST the obstacle-aware router, so it owes everything the router's own
/// edges owe.
pub(crate) fn route_trunk(
    w: &mut SchematicWriter,
    net: &str,
    terms: &[([f64; 2], Option<Dir>)],
    scene: &mut sch_model::route::RouteScene,
) -> bool {
    let span = |axis: usize| {
        let (lo, hi) = terms.iter().fold((f64::MAX, f64::MIN), |(lo, hi), (p, _)| {
            (lo.min(p[axis]), hi.max(p[axis]))
        });
        hi - lo
    };
    if terms.len() < 2 || span(0) + span(1) > TRUNK_SPAN_MM {
        return false;
    }
    let plan = |horizontal: bool| {
        let scene: &sch_model::route::RouteScene = scene;
        trunk_lines(terms, horizontal)
            .into_iter()
            .filter_map(move |line| plan_trunk(terms, horizontal, line, net, scene))
    };
    let best = plan(true).chain(plan(false)).min_by(|a, b| a.0.total_cmp(&b.0));
    let Some((_, paths, feet, horizontal, line)) = best else {
        return false;
    };
    for path in &paths {
        for seg in path.windows(2) {
            emit_routed_segment(w, scene, net, seg[0], seg[1]);
        }
    }
    let (lo, hi) = feet
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), f| (lo.min(*f), hi.max(*f)));
    for foot in feet {
        if foot > lo + EPS && foot < hi - EPS {
            let at = match horizontal {
                true => [foot, line],
                false => [line, foot],
            };
            w.add_junction_on_net(at, net);
        }
    }
    true
}

/// Candidate coordinates for a trunk on the given axis: every terminal's own line, the
/// lines two to eight grid outward along a terminal that faces across the trunk, and four
/// or six grid to either side of one that faces along it.
fn trunk_lines(terms: &[([f64; 2], Option<Dir>)], horizontal: bool) -> Vec<f64> {
    let axis = usize::from(horizontal);
    let grid = geom::GRID_50_MIL;
    let mut out = Vec::new();
    for (p, dir) in terms {
        out.push(grid.snap(p[axis]));
        let along = dir.map(|d| match horizontal {
            true => d.vec().y == 0.0,
            false => d.vec().x == 0.0,
        });
        match (dir, along) {
            (Some(d), Some(false)) => {
                let step = match horizontal {
                    true => d.vec().y,
                    false => d.vec().x,
                };
                out.extend([2.0, 4.0, 6.0, 8.0].map(|k| grid.snap(p[axis] + step * k * 1.27)));
            }
            _ => out.extend(
                [-6.0, -4.0, 4.0, 6.0].map(|k| grid.snap(p[axis] + k * 1.27)),
            ),
        }
    }
    out.sort_by(f64::total_cmp);
    out.dedup_by(|a, b| (*a - *b).abs() < EPS);
    out
}

/// The tee one candidate trunk line would draw: its cost, the paths, the feet along the
/// trunk, and the line itself. `None` when a terminal cannot reach the line, or any wire
/// the tee would draw is illegal.
type Tee = (f64, Vec<Vec<::geom::Point2>>, Vec<f64>, bool, f64);
fn plan_trunk(
    terms: &[([f64; 2], Option<Dir>)],
    horizontal: bool,
    line: f64,
    net: &str,
    scene: &sch_model::route::RouteScene,
) -> Option<Tee> {
    /// Grid charged for a foreign wire the tee passes over, and for each extra wire drawn.
    const CROSSING: f64 = 20.0 * 1.27;
    const EXTRA_WIRE: f64 = 3.0 * 1.27;
    /// A drop leaves its pin at least this far before it may turn.
    const LEAD: f64 = 2.0 * 1.27;

    let axis = usize::from(horizontal);
    let other = 1 - axis;
    let point = |along: f64, across: f64| match horizontal {
        true => ::geom::Point2::new(along, across),
        false => ::geom::Point2::new(across, along),
    };
    let mut paths: Vec<Vec<::geom::Point2>> = Vec::new();
    let mut feet: Vec<f64> = Vec::new();
    for (p, dir) in terms {
        let at = point(p[other], p[axis]);
        if (p[axis] - line).abs() <= EPS {
            feet.push(p[other]);
            continue;
        }
        let across = dir.map(|d| match horizontal {
            true => d.vec().y,
            false => d.vec().x,
        });
        match across {
            // Facing across the trunk: straight out onto it, and only outward.
            Some(step) if step != 0.0 => {
                if (line - p[axis]) * step < LEAD - EPS {
                    return None;
                }
                paths.push(vec![at, point(p[other], line)]);
                feet.push(p[other]);
            }
            // Facing along it (or a virtual port with no pin): out along the pin first,
            // then turn onto the trunk.
            _ => {
                let step = dir.map_or(1.0, |d| match horizontal {
                    true => d.vec().x,
                    false => d.vec().y,
                });
                let turn = p[other] + step * LEAD;
                paths.push(vec![at, point(turn, p[axis]), point(turn, line)]);
                feet.push(turn);
            }
        }
    }
    if feet.len() < 2 {
        return None;
    }
    let (lo, hi) = feet
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), f| (lo.min(*f), hi.max(*f)));
    if hi - lo > EPS {
        paths.push(vec![point(lo, line), point(hi, line)]);
    }
    if !paths
        .iter()
        .all(|path| sch_model::route::path_ok(path, net, scene))
    {
        return None;
    }
    let length: f64 = paths
        .iter()
        .flat_map(|path| path.windows(2).map(|s| s[0].manhattan(s[1])))
        .sum();
    let crossings: usize = paths
        .iter()
        .map(|path| sch_model::route::path_crossings(path, net, scene))
        .sum();
    let cost =
        length + CROSSING * crossings as f64 + EXTRA_WIRE * paths.len() as f64;
    Some((cost, paths, feet, horizontal, line))
}

/// A virtual port-exit point just past the net's pin extent on `side`.
pub(crate) fn port_exit_point(eps: &[([f64; 2], Dir)], side: Side) -> [f64; 2] {
    // A single-pin port (a gate / divider tap) needs only a short stub to seat its
    // pennant clear of its own body; a long one would push the pennant into the
    // NEXT symbol in a packed row (an h-bridge's four FETs at minimum pitch). A
    // multi-pin port exits past the whole net's extent, so it keeps the longer
    // reach to clear the last pin.
    let reach: f64 = if eps.len() == 1 { 2.54 } else { 7.62 };
    let xs: Vec<f64> = eps.iter().map(|(p, _)| p[0]).collect();
    let ys: Vec<f64> = eps.iter().map(|(p, _)| p[1]).collect();
    let (min_x, max_x) = (
        xs.iter().cloned().fold(f64::MAX, f64::min),
        xs.iter().cloned().fold(f64::MIN, f64::max),
    );
    let (min_y, max_y) = (
        ys.iter().cloned().fold(f64::MAX, f64::min),
        ys.iter().cloned().fold(f64::MIN, f64::max),
    );
    // Align the exit with the pin nearest that edge so the wire runs straight.
    match side {
        Side::Right => {
            let y = eps
                .iter()
                .max_by(|a, b| a.0[0].total_cmp(&b.0[0]))
                .map(|t| t.0[1])
                .unwrap_or(min_y);
            [geom::GRID_50_MIL.snap(max_x + reach), y]
        }
        Side::Left => {
            let y = eps
                .iter()
                .min_by(|a, b| a.0[0].total_cmp(&b.0[0]))
                .map(|t| t.0[1])
                .unwrap_or(min_y);
            [geom::GRID_50_MIL.snap(min_x - reach), y]
        }
        Side::Top => {
            let x = eps
                .iter()
                .min_by(|a, b| a.0[1].total_cmp(&b.0[1]))
                .map(|t| t.0[0])
                .unwrap_or(min_x);
            [x, geom::GRID_50_MIL.snap(min_y - reach)]
        }
        Side::Bottom => {
            let x = eps
                .iter()
                .max_by(|a, b| a.0[1].total_cmp(&b.0[1]))
                .map(|t| t.0[0])
                .unwrap_or(max_x);
            [x, geom::GRID_50_MIL.snap(max_y + reach)]
        }
    }
}

pub(crate) fn side_dir(side: Side) -> Dir {
    match side {
        Side::Right => Dir::East,
        Side::Left => Dir::West,
        Side::Top => Dir::North,
        Side::Bottom => Dir::South,
    }
}

/// The sheet edge a pin facing `dir` exits toward — the inverse of [`side_dir`].
/// Used so a single-pin port's exit follows the pin's real orientation.
pub(crate) fn dir_to_side(dir: Dir) -> Side {
    match dir {
        Dir::East => Side::Right,
        Dir::West => Side::Left,
        Dir::North => Side::Top,
        Dir::South => Side::Bottom,
    }
}

/// The sheet edge a port net actually exits toward: when EVERY pin faces the
/// same HORIZONTAL way, geometry beats the name heuristic that picks
/// Left/Right from the net name — a pennant on the name side of two
/// west-facing tail stubs lands in the wire to the next symbol. Vertical or
/// mixed facings keep the `ir.ports` side (pennants read horizontally; a
/// divider tap's north/south pins still exit left/right by name).
pub(crate) fn effective_port_side(port: Option<Side>, eps: &[([f64; 2], Dir)]) -> Option<Side> {
    match port {
        Some(_) if eps.len() == 1 => Some(dir_to_side(eps[0].1)),
        Some(_)
            if !eps.is_empty()
                && matches!(eps[0].1, Dir::East | Dir::West)
                && eps.iter().all(|(_, d)| *d == eps[0].1) =>
        {
            Some(dir_to_side(eps[0].1))
        }
        other => other,
    }
}

/// When a port net taps an IC pin, the name-inferred side can point straight INTO
/// the IC body — dragging the global label across the IC's long internal pin-name
/// text (the ADXL343 `SDA/SDI/SDIO` / `SCL/SCLK` garble). Return a corrected
/// (side, exit) that hugs the IC pin and reads OUTWARD (the way the pin faces, away
/// from the body) whenever the name side would land the label on the pin-name text.
///
/// General + self-adjusting: the trigger and the clearance are driven by the IC's
/// ACTUAL pin-name text extent (`length + offset + text_width(name)`), so a short
/// pin name never triggers (output stays byte-identical) and a long one is cleared.
/// Anchoring the exit on the IC pin (not the whole-net bbox) also lets the virtual
/// exit JOIN the IC pin's routed component — so the net reads as ONE clean port
/// label, never the redundant junction-label-plus-body-label pair.
pub(crate) fn ic_port_exit_override(
    env: &KicadInstallation,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    net: &str,
    eps: &[([f64; 2], Dir)],
    name_side: Side,
) -> Option<(Side, [f64; 2])> {
    const NAME_OFFSET: f64 = 0.508; // KiCAD default pin-name offset (matches the `label` solver)
    let snap = |v| geom::GRID_50_MIL.snap(v);
    // Map the net's pins (same flatten order `wire()` used to build `eps`) back to
    // their (item, pin) so we can read each IC pin's geometry + name.
    let mut pin_of: Vec<Option<(usize, String)>> = vec![None; eps.len()];
    {
        let mut k = 0;
        for (i, num) in inc.get(net).into_iter().flatten() {
            if let Ok(ds) = w.pin_dirs(env, &items[*i].refdes, num) {
                for _ in ds {
                    if k < eps.len() {
                        pin_of[k] = Some((*i, num.clone()));
                        k += 1;
                    }
                }
            }
        }
    }
    // Find an IC anchor pin on this net (≥3-pin non-connector) whose outward facing
    // is HORIZONTALLY OPPOSITE the name side — the case where a Left/Right name exit
    // would cross its body. Among candidates pick the one whose pin name reaches
    // deepest (the worst overlap).
    let name_dir = side_dir(name_side);
    let mut best: Option<([f64; 2], Dir, f64)> = None; // (pin tip, outward dir, name extent)
    for (k, slot) in pin_of.iter().enumerate() {
        let Some((i, num)) = slot else { continue };
        let it = &items[*i];
        if it.geom.pins.len() < 3 || is_connector_like(&it.part) {
            continue;
        }
        let (tip, dir) = eps[k];
        // Only the horizontal-vs-horizontal clash: the pin faces the OPPOSITE way to
        // the name exit, so the exit would be pushed toward the body / pin-name text.
        let opposed = matches!(
            (dir, name_dir),
            (Dir::West, Dir::East) | (Dir::East, Dir::West)
        );
        if !opposed {
            continue;
        }
        // This pin's NAME text extent, measured from the tip INTO the body. We only
        // care about the case where the name actually reaches past a plain exit reach.
        let Some(pg) = it.geom.pins.iter().find(|p| p.number == *num) else {
            continue;
        };
        if pg.name == "~" {
            continue;
        }
        let extent = pg.length + NAME_OFFSET + sch_model::text::text_width(&pg.name);
        if best.map(|(_, _, e)| extent > e).unwrap_or(true) {
            best = Some((tip, dir, extent));
        }
    }
    let (tip, dir, extent) = best?;
    // The plain name-side exit would sit at `tip ± reach` TOWARD the body — and the
    // pin name occupies `[tip, tip + extent]` on that side. So a name-side exit
    // collides exactly when `extent` exceeds the plain reach. Only then do we flip;
    // otherwise leave the caller's behavior untouched (short-name parts byte-identical).
    let plain_reach = if eps.len() == 1 { 2.54 } else { 7.62 };
    if extent <= plain_reach + EPS {
        return None;
    }
    // Flip to the side the pin FACES (away from the body) and hug the pin: the exit
    // sits one plain reach OUTWARD of the tip, reading away from the IC. This places
    // the single port label clear of the body and lets it join the pin's component.
    let side = dir_to_side(dir);
    let exit = match dir {
        Dir::West => [snap(tip[0] - 2.54), tip[1]],
        Dir::East => [snap(tip[0] + 2.54), tip[1]],
        _ => return None,
    };
    Some((side, exit))
}

/// Settle every marked port net's pennant ONCE, before any of them is drawn, reserving
/// the box it occupies. The box contains its own anchor, so a later port cannot be
/// nudged onto an earlier one — two global labels sharing a coordinate ARE one net, the
/// `I2C_SCL`/`I2C_SDA` short on a pull-up network, where the first pennant slid off its
/// body straight onto its neighbour's anchor.
///
/// The anchor is reserved as a point too, which is what stops the next port
/// landing on it; the box alone would be far too coarse a keepout (see `anchor_merges`).
///
/// [`route_signal`] reads the answer instead of recomputing it, so the reservation and
/// the label it stands for can never disagree.
fn plan_port_exits(
    env: &KicadInstallation,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    net_eps: &BTreeMap<String, Vec<([f64; 2], Dir)>>,
    scene: &mut sch_model::route::RouteScene,
) -> BTreeMap<String, (Side, [f64; 2])> {
    let mut exits = BTreeMap::new();
    for (net, eps) in net_eps {
        if ir.rails.contains_key(net) {
            continue;
        }
        let Some(side) = effective_port_side(ir.ports.get(net).copied(), eps) else {
            continue;
        };
        // A port tapping an IC pin whose long internal pin-name text the name-inferred
        // side would cross (the ADXL343 SDA/SCL garble) hugs the pin's outward face.
        let (side, at) = ic_port_exit_override(env, w, items, inc, net, eps, side)
            .unwrap_or((side, port_exit_point(eps, side)));
        let at = nudge_port_exit(scene, at, side, net);
        scene
            .label_solids
            .push((port_label_obstacle(at, side, net), net.clone()));
        scene.points.push((at.into(), net.clone()));
        exits.insert(net.clone(), (side, at));
    }
    exits
}

/// The box a port pennant occupies, for the router to keep FOREIGN wires out of it
/// (a wire drawn across someone else's edge tag). Directional: the pennant + text
/// extend OUTWARD from the exit anchor along `side`; `BACK` covers the connecting
/// vertex that reaches slightly back toward the wire. `HALF` is the text half-height.
/// Slide a pennant's exit outward along its side until it is both TRUTHFUL and clear:
/// the exit-extent heuristic measures the NET's pins, so it can land the pennant
/// inside an unrelated neighbour's body — or, once nudged off it, on the anchor a
/// neighbouring port already claimed. No collision, no move — clean sheets stay
/// byte-identical.
///
/// The two hazards are not equal. Sitting on a body is ugly; sitting on another net's
/// anchor, wire or pennant is a SHORT, and the netlist cannot tell the pennant from what
/// it landed on. So the ladder is walked for truthfulness first: a clear-and-truthful
/// rung wins, else the first merely-ugly truthful one. Only when EVERY rung merges is
/// the caller's own starting point handed back, for the audit to report.
pub(crate) fn nudge_port_exit(
    scene: &sch_model::route::RouteScene,
    at: [f64; 2],
    side: Side,
    net: &str,
) -> [f64; 2] {
    let merges = |at: [f64; 2]| anchor_merges(scene, at.into(), net);
    let blocked = |at: [f64; 2]| solids_hit(&scene.solids, &port_label_obstacle(at, side, net));
    let step = |at: [f64; 2], n: f64| match side {
        Side::Right => [at[0] + 2.54 * n, at[1]],
        Side::Left => [at[0] - 2.54 * n, at[1]],
        Side::Top => [at[0], at[1] - 2.54 * n],
        Side::Bottom => [at[0], at[1] + 2.54 * n],
    };
    let ladder = (0..=10).map(|n| step(at, f64::from(n)));
    let mut truthful = None;
    for candidate in ladder {
        if merges(candidate) {
            continue;
        }
        if !blocked(candidate) {
            return candidate;
        }
        truthful.get_or_insert(candidate);
    }
    truthful.unwrap_or(at)
}

/// Whether seating `net`'s label at `at` would MERGE it with another net.
///
/// A label is an electrical terminal: whatever its anchor lands ON, it joins. So the
/// anchor may not sit on a foreign net's wire or on a foreign terminal — a pin tip, or
/// another label's anchor. This is the truthfulness half of every label-seating
/// decision; readability is the other half, and it is the one that gives way.
///
/// A foreign pennant's TEXT BOX is not a merge hazard, only an ugly one: it is long, and
/// treating it as untouchable pushed a GPIO bank's pennants past anywhere their nets
/// could route to, leaving three of them dangling.
pub(crate) fn anchor_merges(
    scene: &sch_model::route::RouteScene,
    at: ::geom::Point2,
    net: &str,
) -> bool {
    scene
        .segments
        .iter()
        .any(|s| s.net != net && s.segment.dist2_to_point(at) < 0.01)
        || scene
            .points
            .iter()
            .any(|(p, other)| other != net && p.dist2(at) < 0.01)
}

fn solids_hit(solids: &[::geom::Rect], r: &::geom::Rect) -> bool {
    solids.iter().any(|s| s.overlaps(r))
}

pub(crate) fn port_label_obstacle(at: [f64; 2], side: Side, net: &str) -> ::geom::Rect {
    let w = sch_model::text::text_width(net) + 2.54;
    const BACK: f64 = geom::GRID_50_MIL.pitch();
    const HALF: f64 = 2.0;
    match side {
        Side::Left => ::geom::Rect::new(at[0] - w, at[1] - HALF, at[0] + BACK, at[1] + HALF),
        Side::Right => ::geom::Rect::new(at[0] - BACK, at[1] - HALF, at[0] + w, at[1] + HALF),
        // Top/Bottom pennants render rotated: text runs along y, so the long extent
        // is vertical and the cross-extent is the text height.
        Side::Top => ::geom::Rect::new(at[0] - HALF, at[1] - w, at[0] + HALF, at[1] + BACK),
        Side::Bottom => ::geom::Rect::new(at[0] - HALF, at[1] - BACK, at[0] + HALF, at[1] + w),
    }
}

/// A coarse Manhattan direction from `a` toward `b`.
pub(crate) fn dir_toward(a: impl Into<::geom::Point2>, b: impl Into<::geom::Point2>) -> Dir {
    let a = a.into();
    let b = b.into();
    if (b[0] - a[0]).abs() >= (b[1] - a[1]).abs() {
        if b[0] >= a[0] { Dir::East } else { Dir::West }
    } else if b[1] >= a[1] {
        Dir::South
    } else {
        Dir::North
    }
}

/// A signal-net MST hop whose (direct OR routed) length exceeds this (mm) is delegated
/// to a name-matched net-label pair instead of a drawn wire — the professional idiom for
/// long-haul / cross-block connectivity (see [`LabelPolicy`]).
///
/// 50mm, anchored directly to the human corpus (`tools/layout_metrics.py`): humans keep
/// ~0% of wires above 50mm (`wire_frac_gt50` median 0). A literal wire longer than this is
/// the auto-layout "spaghetti" tell.
pub(crate) const LABEL_LEN_MM: f64 = 50.0;

/// Longest rail segment (mm) that still reads as a wire: above this the trunk, one of its
/// risers or one of its lead-outs is a cross-sheet detour, and [`emit_rail`] gives the
/// trunk up for distributed local power symbols instead.
///
/// A rail wire is a wire, so it is held to the same corpus rule as a signal wire —
/// humans keep ~0% of wires above 50mm, whatever net they are on. Measured the same way
/// too: on the geometry actually drawn, not on a pin-span proxy. Its own literal, so that
/// tuning the signal policy never silently redistributes a board's power.
pub(crate) const RAIL_SEGMENT_MAX: f64 = 50.0;

/// The CROSSING-driven label threshold (mm): a hop longer than this whose literal route
/// would cross a foreign wire is named rather than drawn (see [`LabelPolicy`]). Lower than
/// [`LABEL_LEN_MM`] because a crossing — not raw length — is the trigger; a crossing reads
/// as clutter regardless of length, but very short hops stay drawn so the sheet keeps its
/// local wires (humans still draw short stubs; `label_per_part` ≈ 0.76, not everything).
/// 19mm ≈ 7.5 grid: above the human wire-length median (~5mm) and p75, so only the longer,
/// genuinely-crossing hops promote.
pub(crate) const CROSS_LABEL_LEN_MM: f64 = 19.0;

/// The longest hop still drawn as a wire (mm) — and only when its shape is SIMPLE
/// (straight, or one corner with no detour). 60 grid: humans draw the occasional long
/// clean run between two blocks; what they never draw is a long snake.
pub(crate) const LONG_SIMPLE_LEN_MM: f64 = 76.2;

/// How spread a node may be (mm, the sum of its terminal bbox's two sides) and still be
/// drawn as one trunk with drops. 44 grid: past that the node is not a node any more, and
/// the obstacle-aware router should draw it edge by edge.
const TRUNK_SPAN_MM: f64 = 55.88;

/// Shape budget added when an endpoint could not seat a body-clear net label: exactly one
/// crossing. Naming a walled-in pin does not remove the defect, it moves it onto a label
/// over a body — so such a hop is forgiven a crossing, and nothing more.
const CROWDED_FORGIVES: f64 = 20.0;

/// Assign each drawn rail (≥3 pins) a y. Rails in a band share a base y, but overlapping
/// x-ranges are pushed to successive rows (away from the content) via greedy interval
/// colouring, so distinct rails never merge into one wire.
pub(crate) fn assign_rail_levels(
    net_eps: &BTreeMap<String, Vec<([f64; 2], Dir)>>,
    ir: &LayoutIr,
) -> BTreeMap<String, f64> {
    const RAIL_GAP: f64 = 6.35;
    let mut out = BTreeMap::new();
    for band in [Band::Top, Band::Bottom] {
        // (net, min_x, max_x), only rails actually drawn as a wire.
        let mut rails: Vec<(String, f64, f64)> = ir
            .rails
            .iter()
            .filter(|(_, b)| **b == band)
            .filter_map(|(n, _)| {
                let e = net_eps.get(n)?;
                if e.len() < 3 {
                    return None;
                }
                let min_x = e.iter().map(|(p, _)| p[0]).fold(f64::MAX, f64::min);
                let max_x = e.iter().map(|(p, _)| p[0]).fold(f64::MIN, f64::max);
                Some((n.clone(), min_x, max_x))
            })
            .collect();
        if rails.is_empty() {
            continue;
        }
        rails.sort_by(|a, b| a.1.total_cmp(&b.1));
        // Base y: the band edge across all these rails' pins.
        let ys = rails
            .iter()
            .filter_map(|(n, _, _)| net_eps.get(n))
            .flatten()
            .map(|(p, _)| p[1]);
        let base = match band {
            Band::Top => ys.fold(f64::MAX, f64::min) - 5.08,
            Band::Bottom => ys.fold(f64::MIN, f64::max) + 5.08,
        };
        // Greedy interval colouring: level = first row with no x-overlap.
        let mut levels: Vec<Vec<(f64, f64)>> = Vec::new();
        for (net, lo, hi) in rails {
            let mut placed = false;
            for (lvl, occ) in levels.iter_mut().enumerate() {
                if occ.iter().all(|&(a, b)| hi < a - EPS || lo > b + EPS) {
                    occ.push((lo, hi));
                    let y =
                        base + lvl as f64 * RAIL_GAP * if band == Band::Top { -1.0 } else { 1.0 };
                    out.insert(net.clone(), y);
                    placed = true;
                    break;
                }
            }
            if !placed {
                let lvl = levels.len();
                levels.push(vec![(lo, hi)]);
                let y = base + lvl as f64 * RAIL_GAP * if band == Band::Top { -1.0 } else { 1.0 };
                out.insert(net, y);
            }
        }
    }
    out
}

/// A side (E/W) pin leads OUTWARD this far before its riser climbs to the rail,
/// so the riser never runs up the IC edge past the other pins on that side.
pub(crate) const RAIL_LEAD: f64 = 2.54;

/// Lane width used to fan colliding rail risers off a shared column. Half the
/// 2.54 BGA pitch, so an offset riser sits in the gutter between two ball columns
/// rather than landing on a neighbouring pin.
pub(crate) const RAIL_LANE: f64 = geom::GRID_50_MIL.pitch();

pub(crate) fn split_flag_power_pair(eps: &[([f64; 2], Dir)], merge: f64) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize, f64)> = None;
    for i in 0..eps.len() {
        for j in (i + 1)..eps.len() {
            let (a, b) = (eps[i].0, eps[j].0);
            let d = (a[0] - b[0]).abs() + (a[1] - b[1]).abs();
            if d <= EPS || d > merge {
                continue;
            }
            if (a[0] - b[0]).abs() >= EPS && (a[1] - b[1]).abs() >= EPS {
                continue;
            }
            if best.is_none_or(|(_, _, bd)| d < bd - EPS) {
                let (flag, power) = if geom::Point2::from(a).cmp_xy(b.into()).is_le() {
                    (i, j)
                } else {
                    (j, i)
                };
                best = Some((flag, power, d));
            }
        }
    }
    best.map(|(flag, power, _)| (flag, power))
}

fn item_solid_rect(item: &Item) -> Rect {
    let s = item.geom.approx_size();
    let h = geom::Point2::new(s.x / 2.0, s.y / 2.0).rotated_half_extents(item.angle);
    let hx = (h[0] - 2.54).max(1.27);
    let hy = (h[1] - 2.54).max(1.27);
    Rect::new(
        item.at[0] - hx,
        item.at[1] - hy,
        item.at[0] + hx,
        item.at[1] + hy,
    )
}

/// The x a pin's vertical riser sits at, before any anti-collision offset: side
/// pins lead out, top/bottom pins climb straight up. Must match `emit_rail`.
pub(crate) fn riser_base_x(ep: &[f64; 2], dir: Dir) -> f64 {
    match dir {
        Dir::East => ep[0] + RAIL_LEAD,
        Dir::West => ep[0] - RAIL_LEAD,
        _ => ep[0],
    }
}

pub(crate) fn col_key(x: f64) -> i64 {
    (x * 100.0).round() as i64
}

/// Plan per-net horizontal offsets so two rails whose vertical risers would share
/// a column and overlap in y — a SHORT, e.g. stacked BGA balls GND below / 1V2
/// above whose risers cross in the gap — get fanned into separate columns.
/// Returns `(net, riser_base_column) -> dx`. Risers in an uncontested column get
/// no entry (dx = 0), so simple boards (the references) are untouched.
pub(crate) fn plan_riser_offsets(
    net_eps: &BTreeMap<String, Vec<([f64; 2], Dir)>>,
    ir: &LayoutIr,
    rail_y_map: &BTreeMap<String, f64>,
) -> BTreeMap<(String, i64), f64> {
    // Every drawn riser as (net, riser_x, y_lo, y_hi).
    let mut risers: Vec<(String, f64, f64, f64)> = Vec::new();
    for (net, eps) in net_eps {
        if !ir.rails.contains_key(net) || eps.len() < 3 {
            continue;
        }
        let Some(&ry) = rail_y_map.get(net) else {
            continue;
        };
        for (p, dir) in eps {
            let x = riser_base_x(p, *dir);
            risers.push((net.clone(), x, p[1].min(ry), p[1].max(ry)));
        }
    }
    // A column is contested only when two DIFFERENT rails' risers are EXACTLY
    // collinear (same x to float tolerance — that's the one geometry KiCAD merges)
    // and overlap in y by more than a point. The exactness matters: mid-search the
    // geometry is continuous mm (pins not yet grid-snapped), so two risers can pass
    // within microns without ever shorting — a coarse bucket would fan those and
    // churn the placement (the references / grid-demo). column_key -> nets to fan.
    let mut contested: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    for i in 0..risers.len() {
        for j in (i + 1)..risers.len() {
            let (ref na, xa, loa, hia) = risers[i];
            let (ref nb, xb, lob, hib) = risers[j];
            if na == nb || (xa - xb).abs() > EPS {
                continue;
            }
            if hia < lob + EPS || hib < loa + EPS {
                continue; // no real y-overlap (separated or just touching)
            }
            let nets = contested.entry(col_key(xa)).or_default();
            nets.insert(na.clone());
            nets.insert(nb.clone());
        }
    }
    let mut offsets = BTreeMap::new();
    for (col, nets) in &contested {
        // Fan the contested nets into distinct lanes, deterministic by name:
        // 0 -> +lane, 1 -> -lane, 2 -> +2·lane, 3 -> -2·lane, …
        for (rank, net) in nets.iter().enumerate() {
            let step = (rank / 2 + 1) as f64 * RAIL_LANE;
            let dx = if rank % 2 == 0 { step } else { -step };
            offsets.insert((net.clone(), *col), dx);
        }
    }
    offsets
}

/// Map a net name to its `power:` symbol lib_id (best-effort, KiCAD aliases).
pub(crate) fn power_lib_id(net: &str) -> String {
    let alias = match net.to_ascii_uppercase().as_str() {
        "3V3" | "+3V3" => "+3V3",
        "5V" | "+5V" => "+5V",
        "9V" | "+9V" => "+9V",
        "3.3V" => "+3.3V",
        "12V" | "+12V" => "+12V",
        "VCC" => "VCC",
        "VDD" => "VDD",
        g if is_ground(g) => "GND",
        // Custom rail (e.g. VCC3V3, VCCD): a generic donor symbol whose Value
        // names the net — KiCAD derives the global net from the Value field.
        _ => "VCC",
    };
    format!("power:{alias}")
}

/// True if a vertical riser drawn at `x` spanning y∈[ylo,yhi] would pass through
/// the CENTRAL body (past the pin stubs) of some 2-pin part — the case where a
/// rail trunk's riser is drawn straight through a cap/resistor it does not connect
/// to (a stacked same-rail cap column threads the upper cap's riser through the
/// lower body; a mis-oriented cap threads its own). Mirrors the parallel/collinear/
/// perpendicular body-crossing detectors so a jog that clears this also clears the
/// counted crossing. Endpoint-only contact (the riser's own pin) is excluded by the
/// PIN_STUB inset on the body span.
pub(crate) fn riser_hits_body(x: f64, ylo: f64, yhi: f64, bodies: &[([f64; 2], [f64; 2])]) -> bool {
    const PLATE_HALF: f64 = 1.4;
    const PIN_STUB: f64 = 2.54;
    for (a, b) in bodies {
        if (a[0] - b[0]).abs() < EPS {
            // Vertical part: a riser collinear/parallel within the plate width that
            // spans the central body.
            if (x - a[0]).abs() >= PLATE_HALF {
                continue;
            }
            let (blo, bhi) = (a[1].min(b[1]) + PIN_STUB, a[1].max(b[1]) - PIN_STUB);
            if bhi > blo + EPS && ylo < bhi - EPS && yhi > blo + EPS {
                return true;
            }
        } else if (a[1] - b[1]).abs() < EPS {
            // Horizontal part: a riser crossing it perpendicular, strictly inside
            // the central span (its own connecting riser lands at a pin END → outside).
            let (xlo, xhi) = (a[0].min(b[0]) + PIN_STUB, a[0].max(b[0]) - PIN_STUB);
            if xhi > xlo + EPS
                && x > xlo + EPS
                && x < xhi - EPS
                && ylo < a[1] - EPS
                && yhi > a[1] + EPS
            {
                return true;
            }
        }
    }
    false
}

/// True if the L-shaped run a rail draws for one pin — the horizontal lead-out from
/// `ep` to column `x`, then the vertical riser from there to `rail_y` — touches a pin
/// belonging to a DIFFERENT net.
///
/// KiCAD's netlister welds a wire to any pin it ends on *or passes over*, so such a run
/// silently merges the two nets: the sheet renders fine and the netlist is wrong (the
/// `rf-lna-frontend` `RF_IN`/`GND` short, where a cap's ground riser ran down the column
/// it shared with J1 and landed on J1's `In` pin). The riser-fan
/// ([`plan_riser_offsets`]) only separates riser from riser and [`riser_hits_body`] only
/// sees 2-pin bodies, so neither catches it; this is the pin-level guard, the rail-phase
/// counterpart of the foreign points Phase B puts in the signal router's scene.
///
/// `pins` carries every pin on the sheet with its net; entries on `net` are the run's own
/// terminals and are skipped.
pub(crate) fn riser_hits_foreign_pin(
    net: &str,
    ep: [f64; 2],
    x: f64,
    rail_y: f64,
    pins: &[([f64; 2], String)],
) -> bool {
    let within = |v: f64, a: f64, b: f64| v > a.min(b) - EPS && v < a.max(b) + EPS;
    pins.iter().any(|(p, pin_net)| {
        pin_net != net
            && (((p[1] - ep[1]).abs() < EPS && within(p[0], ep[0], x))
                || ((p[0] - x).abs() < EPS && within(p[1], ep[1], rail_y)))
    })
}

/// A rail: with ≥3 pins, draw a horizontal wire spanning them, stub each pin up to it,
/// and put one power symbol at the left end. With fewer pins, no common band, no row the
/// trunk can occupy without touching another net, or a trunk/riser longer than
/// [`RAIL_SEGMENT_MAX`], emit a per-pin power symbol instead ([`emit_local_power`] — the
/// clustered case, e.g. a divider's two GNDs).
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_rail(
    env: &KicadInstallation,
    w: &mut SchematicWriter,
    net: &str,
    eps: &[([f64; 2], Dir)],
    band: Band,
    rail_y: Option<f64>,
    flag: Option<&mut BTreeMap<String, ([f64; 2], f64)>>,
    riser_offsets: &BTreeMap<(String, i64), f64>,
    bodies: &[([f64; 2], [f64; 2])],
    foreign_pins: &[([f64; 2], String)],
    power_keepouts: &[Rect],
    used_lanes: &mut Vec<(f64, f64, f64, String)>,
) -> io::Result<()> {
    let Some(rail_y) = rail_y.filter(|_| eps.len() >= 3) else {
        return emit_local_power(env, w, net, eps, flag, power_keepouts);
    };
    // A rail's whole geometry — every riser column, the trunk and its span — follows from
    // the row it sits on, so the row is what is searched: the assigned row first, then
    // rows stepping OUTWARD from the parts. A trunk is drawn with no obstacle router of
    // its own, so a row it cannot own alone is rejected whole rather than patched.
    let foreign_wires: Vec<(f64, f64, f64)> = foreign_rows(w, net);
    let plan = |y: f64| {
        let attaches =
            plan_rail_attaches(net, eps, y, riser_offsets, bodies, foreign_pins, used_lanes);
        let span = (
            attaches.iter().copied().fold(f64::MAX, f64::min),
            attaches.iter().copied().fold(f64::MIN, f64::max),
        );
        (attaches, span)
    };
    // The NEAREST row the trunk can own: the assigned one, then rows stepping OUTWARD from
    // the parts a lane at a time. Everything is on the 50-mil grid, so "shares no point
    // with another net" already means a full grid step of air. A rail with nowhere to go
    // gives up the trunk for distributed local power symbols.
    let outward = if band == Band::Top { -1.0 } else { 1.0 };
    let Some((rail_y, attaches, (span_lo, span_hi))) = std::iter::once(rail_y)
        .chain((1..=8).map(|k| rail_y + outward * k as f64 * RAIL_LANE))
        .enumerate()
        .find_map(|(step, y)| {
            let (attaches, span) = plan(y);
            let clear = trunk_clear(net, y, span, foreign_pins, &foreign_wires)
                // The assigned row is where the level assignment put the rail, bodies and
                // all; only a row we moved to has to earn its way past them.
                && (step == 0 || !trunk_hits_body(y, span, power_keepouts));
            clear.then_some((y, attaches, span))
        })
    else {
        return emit_local_power(env, w, net, eps, flag, power_keepouts);
    };
    // The trunk and its risers are drawn literally, with no router and no length
    // policy of their own, so a rail whose pins are spread across the sheet becomes
    // exactly the "one wire runs the whole width" tell — the 723 mm GND riser and the
    // 260 mm BMS_GND trunk that made our live-edit sheets read as machine output.
    // Measure what would be drawn and, when a segment is too long to read as a wire,
    // give the trunk up for distributed local power symbols. Length is the honest
    // test: it is what the reader sees, unlike the pin half-perimeter it replaces.
    let longest = eps
        .iter()
        .zip(&attaches)
        .flat_map(|((p, _), &ax)| [(p[1] - rail_y).abs(), (ax - p[0]).abs()])
        .chain(std::iter::once(span_hi - span_lo))
        .fold(0.0, f64::max);
    if longest > RAIL_SEGMENT_MAX {
        return emit_local_power(env, w, net, eps, flag, power_keepouts);
    }
    for (ep, &ax) in eps.iter().map(|(p, _)| p).zip(&attaches) {
        used_lanes.push((ax, ep[1].min(rail_y), ep[1].max(rail_y), net.to_string()));
    }
    w.add_wire_on_net([span_lo, rail_y], [span_hi, rail_y], net);
    for ((ep, _dir), &ax) in eps.iter().zip(&attaches) {
        if (ax - ep[0]).abs() > EPS {
            w.add_wire_on_net(*ep, [ax, ep[1]], net); // lead out
        }
        w.add_wire_on_net([ax, ep[1]], [ax, rail_y], net); // riser
        w.add_junction_on_net([ax, rail_y], net);
    }
    // One power symbol at the left end (pin coincident with the rail). A top
    // rail's symbol sits above, a bottom rail's below — both at angle 0.
    let sym_x = span_lo;
    let flag_at = [sym_x, rail_y];
    w.add_power_symbol(
        env,
        &power_lib_id(net),
        &format!("#PWR_{net}"),
        net,
        flag_at,
        0.0,
    )?;
    // The ERC flag (only when this net needs one) sits COINCIDENT with the rail's
    // power symbol, rotated to extend the same way the symbol does (up for a top
    // V+ rail, down for a bottom GND rail) — into open space, no dangling stub.
    if let Some(flag_points) = flag {
        let angle = if band == Band::Top { 0.0 } else { 180.0 };
        flag_points
            .entry(net.to_string())
            .or_insert(([sym_x, rail_y], angle));
    }
    Ok(())
}

/// What another net already occupies that a trunk could land on, as rows `(y, x_lo,
/// x_hi)`: a horizontal run is the whole stretch it covers, and a vertical run
/// contributes its two ENDPOINTS (a trunk crossing a foreign vertical mid-span merely
/// crosses over, but ending on one welds them).
fn foreign_rows(w: &SchematicWriter, net: &str) -> Vec<(f64, f64, f64)> {
    let mut out = Vec::new();
    let drawn = w
        .wires_with_nets()
        .into_iter()
        .filter_map(|seg| Some((seg.segment, seg.net?)));
    for (segment, seg_net) in drawn.chain(w.beside_wires()) {
        if seg_net == net {
            continue;
        }
        let (a, b) = (segment.a, segment.b);
        if (a.y - b.y).abs() < EPS {
            out.push((a.y, a.x.min(b.x), a.x.max(b.x)));
        } else {
            out.push((a.y, a.x, a.x));
            out.push((b.y, b.x, b.x));
        }
    }
    out
}

/// Does a trunk on row `rail_y` spanning `span` share no point with another net — no
/// foreign pin tip on it, no foreign run along it, no foreign wire end on it?
fn trunk_clear(
    net: &str,
    rail_y: f64,
    span: (f64, f64),
    foreign_pins: &[([f64; 2], String)],
    foreign_runs: &[(f64, f64, f64)],
) -> bool {
    let (lo, hi) = span;
    let overlaps_x = |a: f64, b: f64| a <= hi + EPS && b >= lo - EPS;
    !foreign_pins
        .iter()
        .any(|(p, pin_net)| pin_net != net && (p[1] - rail_y).abs() < EPS && overlaps_x(p[0], p[0]))
        && !foreign_runs
            .iter()
            .any(|&(y, x_lo, x_hi)| (y - rail_y).abs() < EPS && overlaps_x(x_lo, x_hi))
}

/// Would a trunk on row `rail_y` spanning `span` be drawn through a symbol body? The row
/// search can step a rail well past its band, and a trunk sliced through a module reads
/// as a defect even where it shorts nothing.
fn trunk_hits_body(rail_y: f64, span: (f64, f64), bodies: &[Rect]) -> bool {
    let (lo, hi) = span;
    bodies.iter().any(|b| {
        rail_y > b.min_y + EPS && rail_y < b.max_y - EPS && lo < b.max_x - EPS && hi > b.min_x + EPS
    })
}

/// Where each pin attaches to a trunk on row `rail_y`.
///
/// A side (E/W) pin leads OUTWARD first and attaches there, so its riser never runs up
/// the IC edge past the other pins on that side. On top of that, a riser sharing a column
/// with a different rail's overlapping riser gets fanned into a separate lane
/// (`riser_offsets`), then JOGGED one lane at a time until the L-shaped run (lead-out +
/// riser) is clear of ALL THREE hazards at once — a part body drawn through (a stacked
/// same-rail cap column, or a mis-oriented cap whose own body sits between its pin and the
/// rail), a FOREIGN pin the run would weld onto, and a lane another net's riser already
/// occupies. One predicate, one search: jogging off one hazard can no longer land on
/// another.
fn plan_rail_attaches(
    net: &str,
    eps: &[([f64; 2], Dir)],
    rail_y: f64,
    riser_offsets: &BTreeMap<(String, i64), f64>,
    bodies: &[([f64; 2], [f64; 2])],
    foreign_pins: &[([f64; 2], String)],
    used_lanes: &[(f64, f64, f64, String)],
) -> Vec<f64> {
    let mut attaches: Vec<f64> = Vec::with_capacity(eps.len());
    for (ep, dir) in eps {
        let base = riser_base_x(ep, *dir);
        let mut ax = base
            + riser_offsets
                .get(&(net.to_string(), col_key(base)))
                .copied()
                .unwrap_or(0.0);
        let (rlo, rhi) = (ep[1].min(rail_y), ep[1].max(rail_y));
        // The fan plans against BASE columns and the jog moves risers independently, so
        // two different nets can still land one lane; `used_lanes` is the last word.
        let clear = |x: f64| {
            !riser_hits_body(x, rlo, rhi, bodies)
                && !riser_hits_foreign_pin(net, *ep, x, rail_y, foreign_pins)
                && !used_lanes.iter().any(|(lx, lo, hi, lnet)| {
                    lnet != net && (lx - x).abs() < EPS && rlo < hi - EPS && *lo < rhi - EPS
                })
        };
        if !clear(ax)
            && let Some(jogged) = (1..=8)
                .flat_map(|k| [k as f64, -(k as f64)])
                .map(|m| ax + m * RAIL_LANE)
                .find(|&c| clear(c))
        {
            ax = jogged;
        }
        attaches.push(ax);
    }
    attaches
}

/// One power symbol per pin: the clustered/distributed idiom, and the fallback whenever a
/// spanning trunk is not available.
fn emit_local_power(
    env: &KicadInstallation,
    w: &mut SchematicWriter,
    net: &str,
    eps: &[([f64; 2], Dir)],
    flag: Option<&mut BTreeMap<String, ([f64; 2], f64)>>,
    power_keepouts: &[Rect],
) -> io::Result<()> {
    let lib = power_lib_id(net);
    // One power symbol per pin — but MERGE a pin into a nearby, COLLINEAR
    // already-placed symbol (≤2 grid, same x or y) via a short connecting wire
    // instead of stamping a second symbol. Two adjacent same-net pins (e.g. the
    // 3V3 tops of two I2C pull-ups) otherwise render duplicate side-by-side "3V3"
    // labels (the recurring text-overlap defect). The ≤2-grid limit only fuses an
    // immediate neighbour, never the whole spread (which would recreate the long
    // trunk distribution exists to avoid).
    const MERGE: f64 = 5.08;
    let split_flag = flag
        .as_ref()
        .and_then(|_| split_flag_power_pair(eps, MERGE));
    let mut rail_taps: Vec<[f64; 2]> = Vec::new();
    let mut idx = 0usize;
    let mut first_flag: Option<([f64; 2], f64)> = None;
    for (k, (ep, dir)) in eps.iter().enumerate() {
        if split_flag.is_some_and(|(flag_idx, _)| k == flag_idx) {
            continue;
        }
        if let Some(near) = rail_taps
            .iter()
            .map(|&p| {
                let d = (p[0] - ep[0]).abs() + (p[1] - ep[1]).abs();
                (p, d)
            })
            .filter(|(p, d)| {
                *d > EPS
                    && *d <= MERGE
                    && ((p[0] - ep[0]).abs() < EPS || (p[1] - ep[1]).abs() < EPS)
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(p, _)| p)
        {
            w.add_wire_on_net(*ep, near, net);
            w.add_junction_on_net(*ep, net);
            w.add_junction_on_net(near, net);
            continue;
        }
        // A GND symbol on an E/W pin points SIDEWAYS (angle 90/270), reading as a dangling port.
        // Re-orient it to point DOWN (angle 0 — the conventional GND triangle) IN PLACE: a pure
        // angle change adds no wire, so the measured crossing geometry the SA scores on is
        // unchanged and the placement is not perturbed. The triangle's connection point stays at
        // the pin tip, so connectivity is identical.
        let angle = choose_power_angle(net, *dir, *ep, power_keepouts);
        if let Some((flag_idx, symbol_idx)) = split_flag
            && k == symbol_idx
        {
            let flag_ep = eps[flag_idx].0;
            w.add_wire_on_net(flag_ep, *ep, net);
            w.add_junction_on_net(flag_ep, net);
            w.add_junction_on_net(*ep, net);
            rail_taps.push(flag_ep);
            first_flag.get_or_insert((flag_ep, flag_angle(power_glyph_dir(net, angle))));
        }
        w.add_power_symbol(env, &lib, &format!("#PWR_{net}_{idx}"), net, *ep, angle)?;
        first_flag.get_or_insert((*ep, flag_angle(power_glyph_dir(net, angle))));
        rail_taps.push(*ep);
        idx += 1;
    }
    // One ERC flag per net (KiCAD treats an undriven power-input pin as an
    // error here). Place it COINCIDENT with the first power symbol, rotated
    // so its diamond extends the SAME outward direction as that symbol's
    // arrow/triangle — into the open space the power symbol already claims,
    // so the flag reads as part of the supply marker, never a floating leash.
    if let (Some(flag_points), Some((ep, angle))) = (flag, first_flag) {
        flag_points.entry(net.to_string()).or_insert((ep, angle));
    }
    Ok(())
}

/// Angle for a non-ground per-pin power symbol given the pin's outward direction.
///
/// KiCAD's VCC/VDD-family glyph extends north at angle 0, so side pins need the
/// opposite horizontal mapping from GND-family symbols.
pub(crate) fn power_angle(dir: Dir) -> f64 {
    match dir {
        Dir::North => 0.0,
        Dir::South => 180.0,
        Dir::East => 270.0,
        Dir::West => 90.0,
    }
}

fn ground_power_angle(dir: Dir) -> f64 {
    match dir {
        Dir::North => 180.0,
        Dir::South => 0.0,
        Dir::East => 90.0,
        Dir::West => 270.0,
    }
}

fn conventional_power_angle(net: &str, dir: Dir) -> f64 {
    if is_ground(net) {
        ground_power_angle(dir)
    } else {
        power_angle(dir)
    }
}

fn power_angle_candidates(net: &str, dir: Dir) -> Vec<f64> {
    let mut out = Vec::new();
    for a in [conventional_power_angle(net, dir), 0.0, 90.0, 180.0, 270.0] {
        if !out.iter().any(|b: &f64| (*b - a).abs() < EPS) {
            out.push(a);
        }
    }
    out
}

pub(crate) fn choose_power_angle(net: &str, dir: Dir, at: [f64; 2], keepouts: &[Rect]) -> f64 {
    let candidates = power_angle_candidates(net, dir);
    let mut best = candidates[0];
    let mut best_score = power_glyph_overlap_score(net, at, best, keepouts);
    for &angle in &candidates[1..] {
        let score = power_glyph_overlap_score(net, at, angle, keepouts);
        if score + EPS < best_score {
            best = angle;
            best_score = score;
        }
    }
    best
}

fn power_glyph_overlap_score(net: &str, at: [f64; 2], angle: f64, keepouts: &[Rect]) -> f64 {
    let glyph = power_glyph_box(net, at, angle);
    keepouts
        .iter()
        .filter_map(|body| glyph.intersection(body))
        .map(|r| 1000.0 + r.width() * r.height())
        .sum()
}

pub(crate) fn power_glyph_box(net: &str, at: [f64; 2], angle: f64) -> Rect {
    const REACH: f64 = 3.0;
    const HALF: f64 = 1.5;
    let x = at[0];
    let y = at[1];
    match power_glyph_dir(net, angle) {
        Dir::East => Rect::new(x, y - HALF, x + REACH, y + HALF),
        Dir::West => Rect::new(x - REACH, y - HALF, x, y + HALF),
        Dir::North => Rect::new(x - HALF, y - REACH, x + HALF, y),
        Dir::South => Rect::new(x - HALF, y, x + HALF, y + REACH),
    }
}

pub(crate) fn power_glyph_dir(net: &str, angle: f64) -> Dir {
    let q = ((angle / 90.0).round() as i64).rem_euclid(4);
    if is_ground(net) {
        match q {
            0 => Dir::South,
            1 => Dir::East,
            2 => Dir::North,
            _ => Dir::West,
        }
    } else {
        match q {
            0 => Dir::North,
            1 => Dir::West,
            2 => Dir::South,
            _ => Dir::East,
        }
    }
}

/// Angle (CCW) for a `PWR_FLAG` so its diamond — which points up (North) at 0° —
/// extends in the pin's outward `dir`, matching the power symbol it sits on.
pub(crate) fn flag_angle(dir: Dir) -> f64 {
    match dir {
        Dir::North => 0.0,
        Dir::West => 90.0,
        Dir::South => 180.0,
        Dir::East => 270.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routed_segment_already_covered_in_reverse_is_not_emitted() {
        let mut writer = SchematicWriter::new();
        let mut scene = sch_model::route::RouteScene::default();
        emit_routed_segment(
            &mut writer,
            &mut scene,
            "+3V3",
            [105.41, 2.54].into(),
            [105.41, 24.13].into(),
            
        );
        emit_routed_segment(
            &mut writer,
            &mut scene,
            "+3V3",
            [105.41, 24.13].into(),
            [105.41, 21.59].into(),
            
        );

        assert_eq!(writer.wires_with_nets().len(), 1);
        assert_eq!(scene.segments.len(), 1);
        assert_eq!(writer.junction_positions(), vec![[105.41, 21.59]]);

        writer.prepare();
        assert_eq!(writer.wires_with_nets().len(), 2);
    }

    #[test]
    fn routed_segment_emits_only_the_remainder_after_partial_overlap() {
        let mut writer = SchematicWriter::new();
        let mut scene = sch_model::route::RouteScene::default();
        emit_routed_segment(
            &mut writer,
            &mut scene,
            "5V_FUSED",
            [105.41, 21.59].into(),
            [105.41, 24.13].into(),
            
        );
        emit_routed_segment(
            &mut writer,
            &mut scene,
            "5V_FUSED",
            [105.41, 24.13].into(),
            [105.41, 2.54].into(),
            
        );

        assert_eq!(writer.wires_with_nets().len(), 2);
        assert_eq!(scene.segments.len(), 2);
        assert!(writer.wires_with_nets().iter().any(|wire| {
            wire.segment.a.near_eq([105.41, 21.59].into(), EPS)
                && wire.segment.b.near_eq([105.41, 2.54].into(), EPS)
        }));

        writer.prepare();
        let mut pairs = BTreeSet::new();
        for wire in writer.wires_with_nets() {
            let a = crate::write::point_key(wire.segment.a);
            let b = crate::write::point_key(wire.segment.b);
            assert!(pairs.insert(if a <= b { (a, b) } else { (b, a) }));
        }
    }

    #[test]
    fn routed_segment_covered_by_beside_wire_gets_attachment_junctions() {
        let mut writer = SchematicWriter::new();
        let mut scene = sch_model::route::RouteScene::default();
        scene.segments.push(sch_model::route::NetSegment::new(
            [10.16, 10.16].into(),
            [20.32, 10.16].into(),
            "SIG",
        ));

        emit_routed_segment(
            &mut writer,
            &mut scene,
            "SIG",
            [12.7, 10.16].into(),
            [17.78, 10.16].into(),
            
        );

        assert!(writer.wires_with_nets().is_empty());
        assert_eq!(
            writer.junction_positions(),
            vec![[12.7, 10.16], [17.78, 10.16]]
        );
    }

    /// The `rf-lna-frontend` short, constructed directly: a ground riser drawn straight
    /// up the column it shares with J1 passes over J1's `In` pin, welding `RF_IN` onto
    /// `GND`. One lane of jog clears it.
    #[test]
    fn a_riser_over_a_foreign_pin_is_a_short() {
        let pins = vec![
            ([30.48, 45.72], "RF_IN".to_string()),
            ([30.48, 33.02], "GND".to_string()),
        ];
        let (ep, rail_y) = ([30.48, 33.02], 55.88);
        assert!(riser_hits_foreign_pin("GND", ep, 30.48, rail_y, &pins));
        assert!(!riser_hits_foreign_pin(
            "GND",
            ep,
            30.48 + RAIL_LANE,
            rail_y,
            &pins
        ));
    }

    /// The horizontal lead-out is drawn too, so a jog must not sweep the riser column
    /// across a neighbouring pin sharing the row.
    #[test]
    fn a_lead_out_across_a_foreign_pin_is_a_short() {
        let pins = vec![([35.56, 33.02], "SDA".to_string())];
        let (ep, rail_y) = ([30.48, 33.02], 55.88);
        assert!(riser_hits_foreign_pin("GND", ep, 38.1, rail_y, &pins));
        assert!(!riser_hits_foreign_pin("GND", ep, 33.02, rail_y, &pins));
    }

    /// The trunk itself was planned from the rail's own pins alone, so a foreign pin on
    /// the chosen row was welded to the rail by the finalize wire-split. The row search
    /// must step the trunk off it — the riser guard cannot see this: the run to the pin's
    /// own column is clear, it is the SPAN that crosses the foreign pin.
    #[test]
    fn a_trunk_row_carrying_a_foreign_pin_is_rejected() {
        let eps = [
            ([20.32, 40.64], Dir::South),
            ([40.64, 40.64], Dir::South),
            ([60.96, 40.64], Dir::South),
        ];
        let foreign = vec![([40.64, 45.72], "SIG".to_string())];
        let attaches = plan_rail_attaches("GND", &eps, 45.72, &BTreeMap::new(), &[], &foreign, &[]);
        let span = (
            attaches.iter().copied().fold(f64::MAX, f64::min),
            attaches.iter().copied().fold(f64::MIN, f64::max),
        );
        assert!(!trunk_clear("GND", 45.72, span, &foreign, &[]));
        assert!(trunk_clear("GND", 45.72 + RAIL_LANE, span, &foreign, &[]));
        // A foreign pin OUTSIDE the span never blocks the row.
        let aside = vec![([90.0, 45.72], "SIG".to_string())];
        assert!(trunk_clear("GND", 45.72, span, &aside, &[]));
    }

    /// A trunk laid along another rail's trunk merges the two nets outright; ending on a
    /// foreign riser does too, which is why a vertical contributes its endpoints.
    #[test]
    fn a_trunk_row_occupied_by_a_foreign_run_is_rejected() {
        let span = (20.32, 60.96);
        let runs = vec![(45.72, 30.0, 50.0)];
        assert!(!trunk_clear("GND", 45.72, span, &[], &runs));
        assert!(trunk_clear("GND", 45.72 + RAIL_LANE, span, &[], &runs));
    }

    /// End to end: the rail is emitted and NOTHING it draws may touch the foreign pin.
    /// Before the row search this failed — the trunk was drawn at the assigned row
    /// whatever sat on it, and the finalize wire-split then welded the two nets.
    #[test]
    fn an_emitted_rail_never_draws_over_a_foreign_pin() {
        let Some(env) = KicadInstallation::detect() else {
            eprintln!("SKIP: no KiCAD environment detected");
            return;
        };
        let eps = [
            ([20.32, 40.64], Dir::South),
            ([40.64, 40.64], Dir::South),
            ([60.96, 40.64], Dir::South),
        ];
        let foreign = [([40.64, 45.72], "SIG".to_string())];
        let mut w = SchematicWriter::new();
        w.set_weld_guard(true);
        emit_rail(
            &env,
            &mut w,
            "GND",
            &eps,
            Band::Bottom,
            Some(45.72),
            None,
            &BTreeMap::new(),
            &[],
            &foreign,
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        let pin = ::geom::Point2::from(foreign[0].0);
        for seg in w.wires_with_nets() {
            assert!(
                !seg.segment.contains_point(pin),
                "the rail was drawn onto the foreign SIG pin: {:?} -> {:?}",
                seg.segment.a,
                seg.segment.b
            );
        }
    }

    /// A split flag stub adds both endpoints as taps. The next collinear pin must merge
    /// into the nearer one, or its wire covers the stub in reverse after splitting.
    #[test]
    fn local_power_merge_uses_the_nearest_split_flag_tap() {
        let Some(env) = KicadInstallation::detect() else {
            eprintln!("SKIP: no KiCAD environment detected");
            return;
        };
        let eps = [
            ([20.32, 40.64], Dir::North),
            ([22.86, 40.64], Dir::North),
            ([25.4, 40.64], Dir::North),
        ];
        let mut w = SchematicWriter::new();
        let mut flags = BTreeMap::new();
        emit_local_power(&env, &mut w, "3V3", &eps, Some(&mut flags), &[]).unwrap();

        let pairs: BTreeSet<_> = w
            .wires_with_nets()
            .into_iter()
            .map(|wire| {
                let a = crate::write::point_key(wire.segment.a);
                let b = crate::write::point_key(wire.segment.b);
                if a <= b { (a, b) } else { (b, a) }
            })
            .collect();
        assert_eq!(
            pairs,
            BTreeSet::from([
                ((20320, 40640), (22860, 40640)),
                ((22860, 40640), (25400, 40640)),
            ])
        );

        w.prepare();
        let mut pairs = BTreeSet::new();
        for wire in w.wires_with_nets() {
            let a = crate::write::point_key(wire.segment.a);
            let b = crate::write::point_key(wire.segment.b);
            assert!(pairs.insert(if a <= b { (a, b) } else { (b, a) }));
        }
    }

    /// A row the search stepped out to must not slice a symbol body in half.
    #[test]
    fn a_searched_row_through_a_body_is_rejected() {
        let body = Rect::new(30.0, 40.0, 50.0, 60.0);
        assert!(trunk_hits_body(50.0, (20.32, 60.96), &[body]));
        assert!(!trunk_hits_body(70.0, (20.32, 60.96), &[body]));
        // A trunk that stops short of the body never reaches it.
        assert!(!trunk_hits_body(50.0, (0.0, 20.0), &[body]));
    }

    /// A rail's own terminals sit on its lead-out and riser by construction; only
    /// FOREIGN pins are shorts.
    #[test]
    fn a_riser_may_touch_its_own_nets_pins() {
        let pins = vec![
            ([30.48, 45.72], "GND".to_string()),
            ([30.48, 33.02], "GND".to_string()),
        ];
        assert!(!riser_hits_foreign_pin(
            "GND",
            [30.48, 33.02],
            30.48,
            55.88,
            &pins
        ));
    }
}
