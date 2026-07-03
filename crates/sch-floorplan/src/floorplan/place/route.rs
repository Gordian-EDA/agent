//! `place::wire` — the orthogonal elbow router: per-net signal routing
//! (`route_signal`/`route_local_tee`), port-exit geometry, and power-rail riser
//! planning + emission (`assign_rail_levels`, `plan_riser_offsets`, `emit_rail`).

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use kicad_env::KicadEnv;

use crate::write::SchematicWriter;
use geom::{Dir, EPS, ParentForest, Rect};

use super::*;
use sch_place::item::{Incidence, Item};
use sch_place::netclass::{is_connector_like, is_ground};

use sch_place::ir::{Band, LayoutIr, Side};

// ---------------------------------------------------------------------------
// Wiring: rails, signal routing, ports.
// ---------------------------------------------------------------------------

pub(crate) fn wire(
    env: &KicadEnv,
    w: &mut SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
    flag_points: &mut BTreeMap<String, ([f64; 2], f64)>,
    fan_risers: bool,
) -> io::Result<()> {
    let refdes_of = |i: usize| items[i].refdes.clone();
    // Auto-distributing a spread rail into local power symbols only applies to LARGER
    // boards (`pins > FAST_PINS`). Every reference/snapshot fixture (≤34 pins) keeps
    // its tuned short trunk even when its GND rail happens to span the sheet width, so
    // those greedy renders stay byte-identical. The author opt-in (`rail_locals`)
    // still works on any board.
    let pin_total: usize = items.iter().map(|it| it.geom.pins.len()).sum();

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
    // one net (the stacked-BGA-balls GND/1V2 short). Finalize-only: the per-move
    // scorer passes `fan_risers = false` so transient mid-search collisions never
    // perturb the placement.
    let riser_offsets = if fan_risers {
        plan_riser_offsets(&net_eps, ir, &rail_y_map)
    } else {
        BTreeMap::new()
    };

    // Solid symbol bodies for local power-glyph orientation. Unlike the padded
    // placement rectangles, these put pin tips on the boundary, so an outward power
    // marker merely touches its served body while an inward marker overlaps it.
    let power_keepouts: Vec<Rect> = items.iter().map(item_solid_rect).collect();

    // 2-pin body segments (finalize-only, so the per-move scorer is untouched) so a
    // rail riser can JOG around a part body it would otherwise be drawn straight
    // through — the stacked same-rail cap column the SA can't always pull apart.
    let bodies: Vec<([f64; 2], [f64; 2])> = if fan_risers {
        items
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
            .collect()
    } else {
        Vec::new()
    };

    // Driver-pin position per driven non-ground rail, so a rail's power symbol can be
    // anchored at its regulator/IC OUTPUT (the LDO `VO`) instead of the trunk's left end
    // or a bypass cap — making the regulated rail's exit unambiguous. Multi-sheet only
    // (and finalize-only via `fan_risers`): the per-move scorer and every single-sheet
    // reference snapshot pass an empty map ⇒ their power-symbol placement is byte-identical.
    let rail_drivers = if fan_risers && std::env::var_os("MULTISHEET_REFINE").is_some() {
        driven_rail_drivers(env, w, items, inc, ir)
    } else {
        BTreeMap::new()
    };

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
            // per pin) when either the author marked the net (≥2 placed power symbols)
            // OR the net's pins are spread far enough that a single spanning trunk
            // would be a long cross-sheet detour with a knot of converging risers —
            // the professional idiom on a multi-module board, and the fix for the
            // recurring "scattered caps / congested rail knot / long detour rails"
            // critic complaints. Tight/small rails (every reference fixture) stay
            // under the span gate and keep their clean short trunk → byte-identical.
            // On a multi-sheet sub-sheet (small, so below the FAST_PINS gate) a power net
            // whose pins still SPREAD across the sheet draws a page-spanning trunk that reads
            // as a "bare stub" at a far pin (rule 3 violation, the CAN-node VDD defect). Give
            // such a net distributed LOCAL symbols at each pin. Span-gated so tight 2-pin taps
            // keep their clean short trunk. Gated on MULTISHEET_REFINE ⇒ refs byte-identical.
            let multisheet_spread =
                std::env::var("MULTISHEET_REFINE").is_ok() && eps.len() >= 2 && {
                    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
                    for (p, _) in eps {
                        lo[0] = lo[0].min(p[0]);
                        lo[1] = lo[1].min(p[1]);
                        hi[0] = hi[0].max(p[0]);
                        hi[1] = hi[1].max(p[1]);
                    }
                    (hi[0] - lo[0]) + (hi[1] - lo[1]) > 38.0
                };
            let distribute = !ir.rail_force.contains(net)
                && (ir.rail_locals.contains(net)
                    || (pin_total > FAST_PINS && rail_should_distribute(eps))
                    || multisheet_spread);
            let rail_y = rail_y_map.get(net).copied().filter(|_| !distribute);
            let driver = rail_drivers.get(net).copied();
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
                &power_keepouts,
                driver,
                fan_risers,
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
        // Reserve each port's pennant box up front so a LATER net's wire routes
        // around it instead of straight through someone else's edge tag. The label
        // is added during Phase C — too late to obstruct nets routed before it.
        if let Some(side) = effective_port_side(ir.ports.get(net).copied(), eps) {
            // Mirror route_signal's IC-pin override so the reserved box matches where
            // the label actually lands (clear of the IC's long pin-name text).
            let (side, at) = ic_port_exit_override(env, w, items, inc, net, eps, side)
                .unwrap_or((side, port_exit_point(eps, side)));
            let at = nudge_port_exit(&scene, at, side, net);
            scene
                .label_solids
                .push((port_label_obstacle(at, side, net), net.clone()));
        }
    }

    // Phase C — route every signal/port net with the direction-aware,
    // obstacle-avoiding elbow router so wires leave pins along their facing
    // direction and detour around bodies (never through them).
    //
    // Long/crossing hops are delegated to net-label pairs (the human idiom) instead of
    // dragging a literal wire across the sheet. FINALIZE-ONLY (`fan_risers`), so the
    // per-move scorer's cost landscape — and thus the placement — is never perturbed.
    // The policy is corpus-anchored (humans keep ~0% of wires >50mm and ~0 crossings),
    // and applies to EVERY board, not just dense ones: the wire-dense small references
    // (555/uart/grid) are exactly where literal long crossing wires read worst. Mirrors
    // the spread-rail → local-power-symbol distribution above.
    let label_policy = fan_risers.then(LabelPolicy::from_env);
    for (net, eps) in &net_eps {
        if ir.rails.contains_key(net) {
            continue;
        }
        route_signal(
            env,
            w,
            items,
            inc,
            net,
            eps,
            ir.ports.get(net).copied(),
            label_policy,
            &mut scene,
        )?;
    }
    Ok(())
}

/// When the orthogonal router should promote a signal hop to a net-LABEL pair
/// instead of drawing the literal wire — the wire-vs-label decision, anchored to
/// the human corpus (`tools/layout_metrics.py`: humans keep wires short, ~0% >50mm,
/// and ~0 crossings; long literal crossing wires are the auto-layout "spaghetti"
/// tell). A hop is labelled when EITHER:
///   * its (direct or routed) length exceeds [`LABEL_LEN_MM`] — too long to draw; or
///   * its DIRECT pin gap exceeds [`CROSS_LABEL_LEN_MM`], the literal route would CROSS
///     a foreign wire, AND both endpoints could carry a body-clear label — a crossing
///     reads as clutter, so name it instead (but never if naming would just move the
///     defect to a label-over-body).
/// Local short hops (the bulk of a human sheet) stay drawn, so `label_per_part`
/// climbs toward the human ~0.76 without labelling everything.
#[derive(Clone, Copy)]
pub(crate) struct LabelPolicy {
    pub len_mm: f64,
    pub cross_len_mm: f64,
}

impl LabelPolicy {
    /// Thresholds, overridable for sweeps via `SIGNAL_LABEL_SPAN_MM` (length) and
    /// `SIGNAL_CROSS_SPAN_MM` (crossing) — set length very high to disable entirely.
    pub(crate) fn from_env() -> Self {
        let env = |k: &str, d: f64| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        LabelPolicy {
            len_mm: env("SIGNAL_LABEL_SPAN_MM", LABEL_LEN_MM),
            cross_len_mm: env("SIGNAL_CROSS_SPAN_MM", CROSS_LABEL_LEN_MM),
        }
    }
}

/// Route one signal/port net's terminals as a tree (MST) with the direction-
/// aware elbow router. A port adds a virtual terminal just past the net's extent
/// on the named side, then a label there; failure falls back to per-pin labels.
pub(crate) fn route_signal(
    env: &KicadEnv,
    w: &mut SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    net: &str,
    eps: &[([f64; 2], Dir)],
    port: Option<Side>,
    label_policy: Option<LabelPolicy>,
    scene: &mut crate::wire::RouteScene,
) -> io::Result<()> {
    // A single-pin port follows its pin's real direction (see effective_port_side):
    // a MOSFET gate faces left but the name heuristic would exit it right, onto the
    // body. Multi-pin marked ports keep the name-inferred side.
    let port = effective_port_side(port, eps);

    // A port tapping an IC pin whose long internal pin-name text the name-inferred
    // side would cross (the ADXL343 SDA/SCL garble): flip the exit to the pin's
    // outward face and hug the pin, clear of the body. Self-adjusting on the pin's
    // actual name extent, so short-name parts (all references) stay byte-identical.
    let ic_exit = port.and_then(|side| ic_port_exit_override(env, w, items, inc, net, eps, side));
    let port = ic_exit.map(|(s, _)| s).or(port);

    // Terminals: real pins (with outward dir) + an optional virtual port exit.
    let mut terms: Vec<([f64; 2], Option<Dir>)> = eps.iter().map(|(p, d)| (*p, Some(*d))).collect();
    let port_idx = port.map(|side| {
        let at = ic_exit
            .map(|(_, at)| at)
            .unwrap_or_else(|| port_exit_point(eps, side));
        let at = nudge_port_exit(scene, at, side, net);
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
    if route_local_tee(w, net, &terms, scene) {
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
                boxes.extend(crate::label::pin_text_boxes(
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
            let bx = crate::label::label_box(end, *dir, crate::label::text_width(net));
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

    let pts: Vec<::geom::Point2> = terms.iter().map(|t| t.0.into()).collect();
    // Union-find over terminals: a successful edge merges its endpoints; a failed
    // one leaves them split. Route each edge as it succeeds and commit it to the
    // scene immediately so later edges detour around it (partial progress, never
    // the old all-or-nothing that label-bombed the whole net on one bad edge).
    let mut parent: Vec<usize> = (0..terms.len()).collect();
    let mut uf = ParentForest::new(&mut parent);
    let mut paths: Vec<Vec<::geom::Point2>> = Vec::new();
    for (i, j) in crate::wire::mst_edges(&pts) {
        let (a, da, b) = match (terms[i].1, terms[j].1) {
            (Some(d), _) => (pts[i], d, pts[j]),
            (None, Some(d)) => (pts[j], d, pts[i]),
            (None, None) => (pts[i], dir_toward(pts[i], pts[j]), pts[j]),
        };
        // A hop longer than the label policy's length is left unrouted so the union-find
        // leaves its endpoints split — the label-bridge below then names each side,
        // turning a long literal wire into a net-label pair (the human idiom). The policy
        // is Some only at finalize (see `wire`), so every per-move route is unaffected.
        let direct = (a[0] - b[0]).abs() + (a[1] - b[1]).abs();
        if let Some(pol) = label_policy
            && direct > pol.len_mm
        {
            continue;
        }
        if let Some(p) = crate::wire::route_edge(a, da, b, net, scene) {
            if let Some(pol) = label_policy {
                // The DIRECT gap may be short while the only obstacle-free ROUTE is a sheet-wide
                // DETOUR (two ICs whose shared bus pins face opposite ways, so the wire wraps the
                // perimeter). A drawn wraparound reads far worse than naming each end, so discard a
                // path whose routed length exceeds the policy length and leave the endpoints split.
                let routed: f64 = p
                    .windows(2)
                    .map(|s| (s[0][0] - s[1][0]).abs() + (s[0][1] - s[1][1]).abs())
                    .sum();
                if routed > pol.len_mm {
                    continue;
                }
                // CROSSING-DRIVEN promotion: a cross-block hop whose literal route would CROSS a
                // foreign wire reads as spaghetti (humans keep ~0 crossings). Name it instead —
                // leave the endpoints split for the label-bridge. Gated on the DIRECT pin-to-pin
                // gap (not the routed length): a LOCAL node (terminals a few mm apart) keeps its
                // wires even when the only obstacle-free route detours far around a body, so a
                // tight cluster isn't fragmented into label spam. Only promote when BOTH endpoints
                // could carry a body-CLEAR label (predictor matches the lint's geometry), so a pin
                // whose label would land over a chip body or pin-name text — including after the
                // finalize stub-retraction — keeps its wire instead of becoming a lint-flagged
                // label. Conservative on purpose: the TIER-1 references must stay 0-warning.
                if direct > pol.cross_len_mm
                    && term_label_clear[i]
                    && term_label_clear[j]
                    && crate::wire::path_crossings(&p, net, scene) > 0
                {
                    continue;
                }
            }
            for seg in p.windows(2) {
                w.add_wire_on_net(seg[0], seg[1], net);
                scene
                    .segments
                    .push(crate::wire::NetSegment::new(seg[0], seg[1], net));
            }
            paths.push(p);
            uf.union_to(i, j);
        }
    }

    // SINGLE-PIN PORT whose short pin→exit hop the router couldn't place (dense FPGA GPIO banks at
    // 2.54mm pitch block each other's stubs): force the direct stub so the pin and its exit unify
    // into ONE component. Otherwise the bridge below emits a signal label on the pin (over the IC
    // body) AND the port label at the exit — the net renders twice (the BGA GPIO-bank defect). The
    // hop is short + axis-aligned (single-pin ports follow the pin's own dir) so it's safe, and it
    // only fires when the route genuinely failed, so cleanly-routed references stay byte-identical.
    if eps.len() == 1
        && let Some(pi) = port_idx
        && uf.find(0) != uf.find(pi)
    {
        w.add_wire_on_net(pts[0], pts[pi], net);
        scene
            .segments
            .push(crate::wire::NetSegment::new(pts[0], pts[pi], net));
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
            // the bus). When the label policy is active, leave such a pin SPLIT so the label-bridge
            // below names it instead — exactly as the too-long MST hop already does, and as the sibling
            // SDA pin already gets. The local op-amp feedback case (pins a few mm apart) is well under
            // the length, so it still forces its clean loop.
            if let Some(pol) = label_policy
                && (pts[0][0] - pts[k][0]).abs() + (pts[0][1] - pts[k][1]).abs() > pol.len_mm
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
                if crate::wire::path_ok(&path, net, scene) {
                    for seg in path.windows(2) {
                        if (seg[0][0] - seg[1][0]).abs() > EPS
                            || (seg[0][1] - seg[1][1]).abs() > EPS
                        {
                            w.add_wire_on_net(seg[0], seg[1], net);
                            scene
                                .segments
                                .push(crate::wire::NetSegment::new(seg[0], seg[1], net));
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
    // Per-component: the chosen labelling pin and its score `(body_clear, -pin_count)`.
    let mut roots: BTreeMap<usize, Option<(usize, String)>> = BTreeMap::new();
    let mut score: BTreeMap<usize, (bool, std::cmp::Reverse<usize>)> = BTreeMap::new();
    for k in 0..terms.len() {
        let r = uf.find(k);
        let slot = roots.entry(r).or_insert(None);
        let Some(pin) = &term_pin[k] else { continue };
        let cand = (term_label_clear[k], std::cmp::Reverse(pin_count(pin.0)));
        if slot.is_none() || cand > score[&r] {
            *slot = Some(pin.clone());
            score.insert(r, cand);
        }
    }
    let port_root = port_idx.map(|pi| uf.find(pi));
    if std::env::var_os("ROUTE_DEBUG").is_some() {
        let unlabeled = roots.values().filter(|p| p.is_none()).count();
        eprintln!(
            "[route] net {net}: uf-components={} terms={} unlabeled(no-pin)={}",
            roots.len(),
            terms.len(),
            unlabeled
        );
    }
    if roots.len() > 1 {
        for (root, pin) in &roots {
            if Some(*root) == port_root {
                continue; // named by the port label below
            }
            if let Some((i, num)) = pin {
                w.add_signal_label(env, &items[*i].refdes, num, net)?;
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
        w.add_junction(j);
    }
    // A terminal landing inside another same-net segment is a T-join.
    for (p, _) in &terms {
        let interior = w.wire_segments_on_net(net).iter().any(|segment| {
            let point = ::geom::Point2::from(*p);
            let ends = point.near_eq(segment.a, EPS) || point.near_eq(segment.b, EPS);
            !ends && segment.contains_point(point)
        });
        if interior {
            w.add_junction(*p);
        }
    }
    // The port label sits at the virtual exit terminal, facing the edge.
    if let (Some(side), Some(pi)) = (port, port_idx) {
        w.add_cluster_label(net, terms[pi].0, side_dir(side), true);
    }
    Ok(())
}

/// Draw a clustered net as a single-trunk tee (one straight trunk + a short
/// stub from each terminal), returning true if it applied. Used when the
/// terminals are close together AND no component body sits between them, so a
/// trunk is safe — far cleaner than an MST of overlapping elbows. Spread or
/// obstacle-crossing nets return false and fall through to the router.
pub(crate) fn route_local_tee(
    w: &mut SchematicWriter,
    net: &str,
    terms: &[([f64; 2], Option<Dir>)],
    scene: &mut crate::wire::RouteScene,
) -> bool {
    const LOCAL: f64 = 30.48;
    let xs: Vec<f64> = terms.iter().map(|t| t.0[0]).collect();
    let ys: Vec<f64> = terms.iter().map(|t| t.0[1]).collect();
    let (min_x, max_x) = (
        xs.iter().cloned().fold(f64::MAX, f64::min),
        xs.iter().cloned().fold(f64::MIN, f64::max),
    );
    let (min_y, max_y) = (
        ys.iter().cloned().fold(f64::MAX, f64::min),
        ys.iter().cloned().fold(f64::MIN, f64::max),
    );
    if max_x - min_x > LOCAL || max_y - min_y > LOCAL {
        return false;
    }
    // A body strictly inside the terminal bbox would be cut by the trunk.
    let bbox = [min_x, min_y, max_x, max_y];
    let hits_body = scene.solids.iter().any(|r| {
        r[0] < bbox[2] - EPS && bbox[0] < r[2] - EPS && r[1] < bbox[3] - EPS && bbox[1] < r[3] - EPS
    });
    if hits_body {
        return false;
    }
    // Trunk along the longer axis, on the (lower-)median terminal line so the
    // most terminals sit on it without a stub.
    let median = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[(v.len() - 1) / 2]
    };
    let horizontal = (max_x - min_x) >= (max_y - min_y);
    if horizontal {
        let ty = geom::GRID_50_MIL.snap(median(ys));
        w.add_wire_on_net([min_x, ty], [max_x, ty], net);
        scene.segments.push(crate::wire::NetSegment::new(
            [min_x, ty].into(),
            [max_x, ty].into(),
            net,
        ));
        for (p, _) in terms {
            if (p[1] - ty).abs() > EPS {
                w.add_wire_on_net(*p, [p[0], ty], net);
            }
            if p[0] > min_x + EPS && p[0] < max_x - EPS {
                w.add_junction([p[0], ty]);
            }
        }
    } else {
        let tx = geom::GRID_50_MIL.snap(median(xs));
        w.add_wire_on_net([tx, min_y], [tx, max_y], net);
        scene.segments.push(crate::wire::NetSegment::new(
            [tx, min_y].into(),
            [tx, max_y].into(),
            net,
        ));
        for (p, _) in terms {
            if (p[0] - tx).abs() > EPS {
                w.add_wire_on_net(*p, [tx, p[1]], net);
            }
            if p[1] > min_y + EPS && p[1] < max_y - EPS {
                w.add_junction([tx, p[1]]);
            }
        }
    }
    true
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
    env: &KicadEnv,
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
        let extent = pg.length + NAME_OFFSET + crate::label::text_width(&pg.name);
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

/// The box a port pennant occupies, for the router to keep FOREIGN wires out of it
/// (a wire drawn across someone else's edge tag). Directional: the pennant + text
/// extend OUTWARD from the exit anchor along `side`; `BACK` covers the connecting
/// vertex that reaches slightly back toward the wire. `HALF` is the text half-height.
/// Slide a pennant's exit outward along its side until its box clears every
/// body solid: the exit-extent heuristic measures the NET's pins, so it can
/// land the pennant inside an unrelated neighbour's body. No overlap, no move
/// — clean sheets stay byte-identical.
pub(crate) fn nudge_port_exit(
    scene: &sch_io::wire::RouteScene,
    mut at: [f64; 2],
    side: Side,
    net: &str,
) -> [f64; 2] {
    // A candidate is bad if the pennant box sits on a BODY, or if its anchor
    // would touch a FOREIGN net's wire — a global label's anchor point on a
    // wire JOINS that net (the preamp breaks=2 regression).
    let bad = |at: [f64; 2]| {
        let r = port_label_obstacle(at, side, net);
        solids_hit(&scene.solids, &r)
            || scene.segments.iter().any(|seg| {
                seg.net != net && seg.segment.dist2_to_point(at.into()) < 0.01
            })
    };
    if !bad(at) {
        return at;
    }
    let start = at;
    for _ in 0..10 {
        match side {
            Side::Right => at[0] += 2.54,
            Side::Left => at[0] -= 2.54,
            Side::Top => at[1] -= 2.54,
            Side::Bottom => at[1] += 2.54,
        }
        if !bad(at) {
            return at;
        }
    }
    start
}

fn solids_hit(solids: &[::geom::Rect], r: &::geom::Rect) -> bool {
    solids.iter().any(|s| s.overlaps(r))
}

pub(crate) fn port_label_obstacle(at: [f64; 2], side: Side, net: &str) -> ::geom::Rect {
    let w = crate::label::text_width(net) + 2.54;
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

/// Assign each drawn rail (≥3 pins) a y. Rails in a band share a base y, but
/// overlapping x-ranges are pushed to successive rows (away from the content)
/// via greedy interval colouring, so distinct rails never merge into one wire.
/// Half-perimeter span (mm) of a rail net above which a single spanning trunk is a
/// long cross-sheet detour and the net is better drawn as distributed local power
/// symbols. ~30 grid cells; every reference/snapshot fixture's rails span far less
/// (≤34-pin compact boards), so they keep their trunk and stay byte-identical.
pub(crate) const RAIL_DISTRIBUTE_SPAN: f64 = 76.0;

/// A signal-net MST hop whose (direct OR routed) length exceeds this (mm) is delegated
/// to a name-matched net-label pair instead of a drawn wire — the professional idiom for
/// long-haul / cross-block connectivity. The signal-net analog of [`RAIL_DISTRIBUTE_SPAN`].
/// Applied FINALIZE-ONLY (see [`LabelPolicy`]) so the per-move scorer / placement is never
/// perturbed.
///
/// 50mm, anchored directly to the human corpus (`tools/layout_metrics.py`): humans keep
/// ~0% of wires above 50mm (`wire_frac_gt50` median 0). A literal wire longer than this is
/// the auto-layout "spaghetti" tell. Override via `SIGNAL_LABEL_SPAN_MM`.
pub(crate) const LABEL_LEN_MM: f64 = 50.0;

/// The CROSSING-driven label threshold (mm): a hop longer than this whose literal route
/// would cross a foreign wire is named rather than drawn (see [`LabelPolicy`]). Lower than
/// [`LABEL_LEN_MM`] because a crossing — not raw length — is the trigger; a crossing reads
/// as clutter regardless of length, but very short hops stay drawn so the sheet keeps its
/// local wires (humans still draw short stubs; `label_per_part` ≈ 0.76, not everything).
/// 19mm ≈ 7.5 grid: above the human wire-length median (~5mm) and p75, so only the longer,
/// genuinely-crossing hops promote. Override via `SIGNAL_CROSS_SPAN_MM`.
pub(crate) const CROSS_LABEL_LEN_MM: f64 = 19.0;

/// Whether a rail net's pins are spread far enough to prefer DISTRIBUTED local power
/// symbols over one spanning trunk (see [`RAIL_DISTRIBUTE_SPAN`]). A net with <3 pins
/// already draws per-pin symbols, so it's irrelevant there.
pub(crate) fn rail_should_distribute(eps: &[([f64; 2], Dir)]) -> bool {
    if eps.len() < 3 {
        return false;
    }
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    for (p, _) in eps {
        lo[0] = lo[0].min(p[0]);
        lo[1] = lo[1].min(p[1]);
        hi[0] = hi[0].max(p[0]);
        hi[1] = hi[1].max(p[1]);
    }
    (hi[0] - lo[0]) + (hi[1] - lo[1]) > RAIL_DISTRIBUTE_SPAN
}

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
    if std::env::var_os("RISER_DEBUG").is_some() {
        eprintln!("[riser] {} risers, contested: {:?}", risers.len(), contested);
        for r in &risers {
            eprintln!("[riser]   {:?}", r);
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
        g if g.starts_with("GND") || g.starts_with("VSS") || g == "AGND" || g == "DGND" => "GND",
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

/// A rail: with ≥3 pins, draw a horizontal wire at `rail_y` spanning them, stub
/// each pin to it, and put one power symbol at the left end. With fewer pins (or
/// no common band), emit a per-pin power symbol instead (the clustered case,
/// e.g. a divider's two GNDs).
///
/// `driver` (multi-sheet only) is the world position of the regulator/IC OUTPUT pin
/// that drives this rail: when present, the rail's single power symbol is anchored
/// there (the LDO `VO`) so the regulated rail's exit is unambiguous — instead of the
/// trunk's left end or a bypass cap. In the per-pin path it also collapses the
/// scattered per-pin symbols into ONE symbol at the driver with a wire to each other
/// pin, tying the output cap to the regulator output instead of leaving it a detached
/// implicit-net island.
pub(crate) fn emit_rail(
    env: &KicadEnv,
    w: &mut SchematicWriter,
    net: &str,
    eps: &[([f64; 2], Dir)],
    band: Band,
    rail_y: Option<f64>,
    flag: Option<&mut BTreeMap<String, ([f64; 2], f64)>>,
    riser_offsets: &BTreeMap<(String, i64), f64>,
    bodies: &[([f64; 2], [f64; 2])],
    power_keepouts: &[Rect],
    driver: Option<[f64; 2]>,
    fan_risers: bool,
    used_lanes: &mut Vec<(f64, f64, f64, String)>,
) -> io::Result<()> {
    let lib = power_lib_id(net);
    let Some(rail_y) = rail_y.filter(|_| eps.len() >= 3) else {
        // DRIVEN small rail: anchor the supply symbol at the regulator OUTPUT pin and
        // wire every other pin of the net to it, so the regulated rail's exit reads
        // straight off the driver (the LDO `VO`) and the output cap is a drawn member
        // of the net, not a detached implicit-net island. Only when the driver pin is
        // actually one of this net's endpoints. Multi-sheet only (driver is `None`
        // elsewhere), so reference snapshots keep the per-pin behaviour below.
        //
        // CRITICAL: the star is a hub-and-spoke whose spokes are blind Manhattan hops
        // (no body/foreign-pin avoidance). On a SPREAD rail (a distributed power net
        // with many pins scattered across the sheet — the MCU's stacked decoupling-cap
        // columns) those spokes become long risers that run STRAIGHT DOWN a cap column,
        // crossing every cap's GND pin and body in between → the rail swallows GND and
        // the GPIO pins it grazes (the bedrock/ice40 "PA2/GPIO ↔ 3V3" short). The star
        // is only safe for a TIGHT driven cluster (LDO VO + its 1-2 output caps), so
        // gate it on a small pin-bounding-box extent. A spread driven rail falls through
        // to per-pin LOCAL power symbols below — one symbol AT each pin, no riser to
        // cross anything.
        const DRIVEN_STAR_MAX_SPREAD: f64 = 38.0; // matches the multisheet_spread gate
        let driver = driver.filter(|_| {
            let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
            for (p, _) in eps {
                lo[0] = lo[0].min(p[0]);
                lo[1] = lo[1].min(p[1]);
                hi[0] = hi[0].max(p[0]);
                hi[1] = hi[1].max(p[1]);
            }
            (hi[0] - lo[0]) + (hi[1] - lo[1]) <= DRIVEN_STAR_MAX_SPREAD
        });
        if let Some(dp) = driver
            && let Some((_, ddir)) = eps
                .iter()
                .copied()
                .find(|(p, _)| (p[0] - dp[0]).abs() < EPS && (p[1] - dp[1]).abs() < EPS)
        {
            let angle = choose_power_angle(net, ddir, dp, power_keepouts);
            w.add_power_symbol(env, &lib, &format!("#PWR_{net}"), net, dp, angle)?;
            for (ep, _) in eps.iter() {
                if (ep[0] - dp[0]).abs() >= EPS || (ep[1] - dp[1]).abs() >= EPS {
                    // Manhattan two-segment hop from the driver to this pin (a single
                    // straight wire when they already share a row/column).
                    if (ep[0] - dp[0]).abs() >= EPS && (ep[1] - dp[1]).abs() >= EPS {
                        w.add_wire_on_net(dp, [ep[0], dp[1]], net);
                        w.add_wire_on_net([ep[0], dp[1]], *ep, net);
                    } else {
                        w.add_wire_on_net(dp, *ep, net);
                    }
                }
            }
            if let (Some(flag_points), Some(_)) = (
                flag,
                eps.iter()
                    .find(|(p, _)| (p[0] - dp[0]).abs() < EPS && (p[1] - dp[1]).abs() < EPS),
            ) {
                flag_points
                    .entry(net.to_string())
                    .or_insert((dp, flag_angle(power_glyph_dir(net, angle))));
            }
            return Ok(());
        }
        // One power symbol per pin — but MERGE a pin into a nearby, COLLINEAR
        // already-placed symbol (≤2 grid, same x or y) via a short connecting wire
        // instead of stamping a second symbol. Two adjacent same-net pins (e.g. the
        // 3V3 tops of two I2C pull-ups) otherwise render duplicate side-by-side "3V3"
        // labels (the recurring text-overlap defect). The ≤2-grid limit only fuses an
        // immediate neighbour, never the whole spread (which would recreate the long
        // trunk distribution exists to avoid).
        const MERGE: f64 = 5.08;
        // A GND tie on a SIDE (E/W) pin — a lone address-select / strap pin like the BME280 SDO=GND
        // (I2C address 0x76) — gets a power_angle of 90°/270°, so its triangle points SIDEWAYS into
        // open space and reads as a dangling port labelled "GND" (the i2c_sensors floating-GND defect).
        // Convention is the GND triangle points DOWN, so we re-orient such a symbol to angle 0 below.
        // Multi-sheet only and gated on `fan_risers`, the FINALIZE-only flag ⇒ the per-move SA scorer
        // passes `fan_risers = false` so its cost landscape is byte-identical and the placement is never
        // perturbed, and every single-sheet reference snapshot keeps its exact per-pin placement. Only
        // ground (the recurring eyesore); V+ side ties keep their outward arrow.
        let drop_side_gnd =
            is_ground(net) && fan_risers && std::env::var_os("MULTISHEET_REFINE").is_some();
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
            if let Some(&near) = rail_taps.iter().find(|&&p| {
                let d = (p[0] - ep[0]).abs() + (p[1] - ep[1]).abs();
                d > EPS && d <= MERGE && ((p[0] - ep[0]).abs() < EPS || (p[1] - ep[1]).abs() < EPS)
            }) {
                w.add_wire_on_net(*ep, near, net);
                w.add_junction(*ep);
                w.add_junction(near);
                continue;
            }
            // A GND symbol on an E/W pin points SIDEWAYS (angle 90/270), reading as a dangling port.
            // Re-orient it to point DOWN (angle 0 — the conventional GND triangle) IN PLACE: a pure
            // angle change adds no wire, so the measured crossing geometry the SA scores on is
            // unchanged and the placement is not perturbed. The triangle's connection point stays at
            // the pin tip, so connectivity is identical.
            let angle = if drop_side_gnd && matches!(dir, Dir::East | Dir::West) {
                choose_power_angle_preferred(net, *dir, *ep, power_keepouts, 0.0)
            } else {
                choose_power_angle(net, *dir, *ep, power_keepouts)
            };
            if let Some((flag_idx, symbol_idx)) = split_flag
                && k == symbol_idx
            {
                let flag_ep = eps[flag_idx].0;
                w.add_wire_on_net(flag_ep, *ep, net);
                w.add_junction(flag_ep);
                w.add_junction(*ep);
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
        return Ok(());
    };
    // Each pin's attach point on the rail. A side (E/W) pin leads OUTWARD first
    // and attaches there, so its riser never runs up the IC edge past the other
    // pins on that side (which would block their signals). On top of that, a riser
    // sharing a column with a different rail's overlapping riser gets fanned into
    // a separate lane (`riser_offsets`) so the two rails never merge into a short.
    // Final riser x per pin: the base column + any anti-short fan offset, THEN a
    // finalize JOG one lane at a time off any part body the straight riser would be
    // drawn through (a stacked same-rail cap column, or a mis-oriented cap whose own
    // body sits between its pin and the rail). The riser then leads sideways out of
    // the pin and descends in a clear lane — clearing both its own body and a
    // neighbour's. `bodies` is empty on the per-move scorer (finalize-only), so the
    // placement is never churned by this.
    let mut attaches: Vec<f64> = Vec::with_capacity(eps.len());
    for (ep, dir) in eps {
        let base = riser_base_x(ep, *dir);
        let mut ax = base
            + riser_offsets
                .get(&(net.to_string(), col_key(base)))
                .copied()
                .unwrap_or(0.0);
        let (rlo, rhi) = (ep[1].min(rail_y), ep[1].max(rail_y));
        if !bodies.is_empty()
            && riser_hits_body(ax, rlo, rhi, bodies)
            && let Some(clear) = (1..=8)
                .flat_map(|k| [k as f64, -(k as f64)])
                .map(|m| ax + m * RAIL_LANE)
                .find(|&c| !riser_hits_body(c, rlo, rhi, bodies))
        {
            ax = clear;
        }
        // The fan plans against BASE columns and the body-jog moves risers
        // independently, so two different nets can still land one lane. The
        // shared registry is the last word: shift until the lane is clean.
        if fan_risers {
            let conflict = |x: f64, lanes: &[(f64, f64, f64, String)]| {
                lanes.iter().any(|(lx, lo, hi, lnet)| {
                    lnet != net && (lx - x).abs() < EPS && rlo < hi - EPS && *lo < rhi - EPS
                })
            };
            if conflict(ax, used_lanes) && std::env::var_os("RISER_DEBUG").is_some() {
                eprintln!("[lane] conflict for {net} at x={ax}");
            }
            if conflict(ax, used_lanes)
                && let Some(clear) = (1..=8)
                    .flat_map(|k| [k as f64, -(k as f64)])
                    .map(|m| ax + m * RAIL_LANE)
                    .find(|&c| {
                        !conflict(c, used_lanes)
                            && (bodies.is_empty() || !riser_hits_body(c, rlo, rhi, bodies))
                    })
            {
                ax = clear;
            }
            used_lanes.push((ax, rlo, rhi, net.to_string()));
        }
        attaches.push(ax);
    }
    let span_lo = attaches.iter().copied().fold(f64::MAX, f64::min);
    let span_hi = attaches.iter().copied().fold(f64::MIN, f64::max);
    w.add_wire_on_net([span_lo, rail_y], [span_hi, rail_y], net);
    for ((ep, _dir), &ax) in eps.iter().zip(&attaches) {
        if (ax - ep[0]).abs() > EPS {
            w.add_wire_on_net(*ep, [ax, ep[1]], net); // lead out
        }
        w.add_wire_on_net([ax, ep[1]], [ax, rail_y], net); // riser
        w.add_junction([ax, rail_y]);
    }
    // One power symbol at the left end (pin coincident with the rail). A top
    // rail's symbol sits above, a bottom rail's below — both at angle 0. For a DRIVEN
    // rail, anchor it at the DRIVER pin's attach point instead, so the supply label
    // reads at the regulator OUTPUT (the rail's source) rather than at whatever cap
    // sits leftmost on the trunk.
    let sym_x = driver
        .and_then(|dp| {
            eps.iter()
                .zip(&attaches)
                .find(|((ep, _), _)| (ep[0] - dp[0]).abs() < EPS && (ep[1] - dp[1]).abs() < EPS)
                .map(|(_, &ax)| ax)
        })
        .unwrap_or(span_lo);
    let flag_at = [sym_x, rail_y];
    w.add_power_symbol(env, &lib, &format!("#PWR_{net}"), net, flag_at, 0.0)?;
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

fn power_angle_candidates(net: &str, dir: Dir, preferred: Option<f64>) -> Vec<f64> {
    let mut out = Vec::new();
    for a in [
        preferred.unwrap_or_else(|| conventional_power_angle(net, dir)),
        conventional_power_angle(net, dir),
        0.0,
        90.0,
        180.0,
        270.0,
    ] {
        if !out.iter().any(|b: &f64| (*b - a).abs() < EPS) {
            out.push(a);
        }
    }
    out
}

pub(crate) fn choose_power_angle(net: &str, dir: Dir, at: [f64; 2], keepouts: &[Rect]) -> f64 {
    choose_power_angle_inner(net, dir, at, keepouts, None)
}

pub(crate) fn choose_power_angle_preferred(
    net: &str,
    dir: Dir,
    at: [f64; 2],
    keepouts: &[Rect],
    preferred: f64,
) -> f64 {
    choose_power_angle_inner(net, dir, at, keepouts, Some(preferred))
}

fn choose_power_angle_inner(
    net: &str,
    dir: Dir,
    at: [f64; 2],
    keepouts: &[Rect],
    preferred: Option<f64>,
) -> f64 {
    let candidates = power_angle_candidates(net, dir, preferred);
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
