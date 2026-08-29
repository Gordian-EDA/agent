//! Hint-driven and structured locking: grid tiling, ring-surround, edge-lock, and
//! the unified radial fan-out fast-path. These all LOCK parts at computed cells so
//! later stages lay out the rest around a tidy, overlap-free-by-construction core.

use super::geometry::{
    clamp_center_for_envelope, datum_rotation_for_edge, part_edge_target,
    part_placement_bounds_envelope, rotated_copper_bbox, rotated_courtyard_half,
};
use pcb_model::Point2;
use pcb_place_api::{Edge, LockedAt, Part, PlaceProblem, PlacementHints};
use pcb_place_api::{decoupling_pairs, series_pairs};

/// Lock each member of a `grid` group at a computed cell of a regular grid (row-major,
/// member order), centred in the group's region. The column count is sized from the
/// region aspect and member count; the cell PITCH is the largest member footprint extent
/// plus a clearance gap — NOT the region divided by the grid, which scatters the array
/// across the whole region when the parts are small (an LED matrix tiled at ~19 % fill).
/// A TIGHT pitch keeps the array compact and legal; centring it leaves an even border.
/// The centred block is then slid back on-board (and each cell clamped into bounds) so a
/// region smaller than the array footprint near a board edge cannot push LOCKED cells
/// off-board — which the legalizer could never recover, forcing `legal: false`.
/// Locked members are then fixed for the rest of placement, so the annealer lays out the
/// remaining parts around the tidy array instead of scattering it. A no-op for groups
/// without `grid`/`region`, or whose members aren't found.
///
/// NOTE: tightening the pitch makes the array compact and correct, but it does not yet
/// GROW the array to fill a large board — that needs a board bounds-fit pass (size the
/// outline to the placed extent) which does not exist in `pcb-place`. The pitch math here
/// is the correct primitive that pass would build on.
pub fn apply_grid_hints(problem: &mut PlaceProblem, hints: &PlacementHints) {
    for g in &hints.groups {
        // `surround`: ring the members tightly around a locked target part's edges
        // (the decoupling pattern). Handled first; falls through to `grid` otherwise.
        if let Some(target) = &g.surround {
            apply_surround(problem, &g.members, target, 0.6);
            continue;
        }
        if !g.grid {
            continue;
        }
        let Some(region) = &g.region else { continue };
        let idxs: Vec<usize> = g
            .members
            .iter()
            .filter_map(|r| problem.parts.iter().position(|p| &p.reference == r))
            .collect();
        if idxs.is_empty() {
            continue;
        }
        let n = idxs.len();
        let (rw, rh) = (region.max_x - region.min_x, region.max_y - region.min_y);
        let cols = (((n as f64) * rw / rh).sqrt().round() as usize).clamp(1, n);
        let rows = n.div_ceil(cols);
        // Spread the members evenly over the region (cell centres). A tighter
        // footprint-pitch packing was tried for denser fill, but on a board with a
        // grid-hinted array (e.g. dual-bga's resistor bank) the tight block crowds the
        // routing channels and regresses completion; the even spread keeps the array
        // routable. A final clamp keeps every locked courtyard on-board even if the
        // region is tucked at a board edge (the cells are locked, so the legalizer
        // cannot pull an off-board one back).
        let (px, py) = (rw / cols as f64, rh / rows as f64);
        let b = &problem.bounds;
        let rotation = g.rotation.unwrap_or(0.0);
        for (k, &i) in idxs.iter().enumerate() {
            let (c, r) = (k % cols, k / cols);
            let mut at = Point2 {
                x: region.min_x + (c as f64 + 0.5) * px,
                y: region.min_y + (r as f64 + 0.5) * py,
            };
            at = b.clamp_center_for_half(at, rotated_courtyard_half(&problem.parts[i], rotation));
            problem.parts[i].locked = Some(LockedAt { at, rotation });
        }
    }
}

/// Ring `members` tightly around the perimeter of the LOCKED `target` part (the
/// decoupling-cap pattern): space them evenly by ARC LENGTH around the target's
/// courtyard, just outside each edge, and lock each there. Arc-length spacing makes
/// the per-edge count proportional to edge length, so a long edge gets more caps than
/// a short one — a tall IC no longer overflows (and overlaps) its short edges. The
/// target must already be locked (the agent fixes the IC first) so its centre is known.
/// A no-op otherwise.
pub fn apply_surround(problem: &mut PlaceProblem, members: &[String], target: &str, gap: f64) {
    let Some(ti) = problem.parts.iter().position(|p| p.reference == target) else {
        return;
    };
    let Some(loc) = problem.parts[ti].locked.clone() else {
        return;
    };
    let (cx, cy) = (loc.at.x, loc.at.y);
    let (hw, hh) = (
        problem.parts[ti].courtyard_w / 2.0,
        problem.parts[ti].courtyard_h / 2.0,
    );
    let idxs: Vec<usize> = members
        .iter()
        .filter_map(|r| problem.parts.iter().position(|p| &p.reference == r))
        .collect();
    let n = idxs.len();
    if n == 0 {
        return;
    }
    // `gap` = mm clear of the IC courtyard edge (an outer ring uses a larger gap).
    // Walk the courtyard perimeter clockwise: top (len 2hw) → right (2hh) → bottom
    // (2hw) → left (2hh). Place member k at arc position (k+0.5)/n of the perimeter.
    let perim = 4.0 * (hw + hh);
    for (k, &i) in idxs.iter().enumerate() {
        let (chw, chh) = (
            problem.parts[i].courtyard_w / 2.0,
            problem.parts[i].courtyard_h / 2.0,
        );
        let pos = (k as f64 + 0.5) / n as f64 * perim;
        let at = if pos < 2.0 * hw {
            Point2 {
                x: cx - hw + pos,
                y: cy - hh - gap - chh,
            } // top, L→R
        } else if pos < 2.0 * hw + 2.0 * hh {
            Point2 {
                x: cx + hw + gap + chw,
                y: cy - hh + (pos - 2.0 * hw),
            } // right, T→B
        } else if pos < 4.0 * hw + 2.0 * hh {
            Point2 {
                x: cx + hw - (pos - 2.0 * hw - 2.0 * hh),
                y: cy + hh + gap + chh,
            } // bottom, R→L
        } else {
            Point2 {
                x: cx - hw - gap - chw,
                y: cy + hh - (pos - 4.0 * hw - 2.0 * hh),
            } // left, B→T
        };
        problem.parts[i].locked = Some(LockedAt { at, rotation: 0.0 });
    }
}

/// Lock each named connector at a BOARD EDGE, distributed by arc position around
/// the perimeter and seated just inside the bound. Deterministic — the soft
/// edge-seek SA force is unreliable when the interior is crowded (it strands a
/// connector or two inboard, which the critic flags). Already-locked parts are
/// left alone. Pairs with [`apply_surround`]: IC + caps centred, connectors framed
/// at the edges, the rest placed between, then the outline tightens to it.
pub fn apply_edge_lock(problem: &mut PlaceProblem, refs: &[String]) {
    let b = problem.bounds;
    let idxs: Vec<usize> = refs
        .iter()
        .filter_map(|r| {
            problem
                .parts
                .iter()
                .position(|p| &p.reference == r && p.locked.is_none())
        })
        .collect();
    let n = idxs.len();
    if n == 0 {
        return;
    }
    let (w, h) = (b.max_x - b.min_x, b.max_y - b.min_y);
    let perim = 2.0 * (w + h);
    for (k, &i) in idxs.iter().enumerate() {
        let t = (k as f64 + 0.5) / n as f64 * perim;
        let (edge, tangent) = if t < w {
            (Edge::N, b.min_x + t)
        } else if t < w + h {
            (Edge::E, b.min_y + (t - w))
        } else if t < 2.0 * w + h {
            (Edge::S, b.max_x - (t - w - h))
        } else {
            (Edge::W, b.max_y - (t - 2.0 * w - h))
        };
        let rotation = datum_rotation_for_edge(&problem.parts[i], edge);
        let half = rotated_courtyard_half(&problem.parts[i], rotation);
        let normal = part_edge_target(&problem.parts[i], rotation, edge, &b, half);
        let at = match edge {
            Edge::N | Edge::S => Point2 {
                x: tangent,
                y: normal,
            },
            Edge::E | Edge::W => Point2 {
                x: normal,
                y: tangent,
            },
        };
        let at = if problem.parts[i].edge_datum.is_some() {
            let copper = rotated_copper_bbox(&problem.parts[i], rotation);
            let envelope = part_placement_bounds_envelope(&problem.parts[i], half, copper);
            let mut clamped = clamp_center_for_envelope(&b, at, envelope);
            // Preserve the physical datum exactly. If copper cannot clear this
            // edge at the mechanical position, legality must reject it instead
            // of silently shifting the datum into the board.
            match edge {
                Edge::N | Edge::S => clamped.y = at.y,
                Edge::E | Edge::W => clamped.x = at.x,
            }
            clamped
        } else {
            b.clamp_center_for_half(at, half)
        };
        problem.parts[i].locked = Some(LockedAt { at, rotation });
    }
}

/// Place `ordered` parts in CONCENTRIC rings around the (locked) IC, density-aware
/// so each ring holds only as many as fit without overlap (the rest spill to the
/// next, larger ring). Members keep their given order around the perimeter — pass
/// [`series_fanout_order`] output for a radial fan-out (short, parallel,
/// non-crossing escapes). Unlike [`apply_surround`] (one rectangular ring → corner
/// overlap + illegal beyond ~a dozen parts), this scales to a full pin field and
/// stays legal. Each member is locked.
pub fn fan_out_rings(
    problem: &mut PlaceProblem,
    ic: usize,
    ordered: &[String],
    start_gap: f64,
    spacing: f64,
) {
    let Some(loc) = problem.parts[ic].locked.clone() else {
        return;
    };
    let (cx, cy) = (loc.at.x, loc.at.y);
    let (hw, hh) = (
        problem.parts[ic].courtyard_w / 2.0,
        problem.parts[ic].courtyard_h / 2.0,
    );
    let idxs: Vec<usize> = ordered
        .iter()
        .filter_map(|r| problem.parts.iter().position(|p| &p.reference == r))
        .collect();
    let row_step = spacing; // radial gap between successive rings
    let mut k = 0usize;
    let mut ring = 0usize;
    while k < idxs.len() {
        let g = start_gap + ring as f64 * row_step;
        let (ihw, ihh) = (hw + g, hh + g);
        let perim = 4.0 * (ihw + ihh);
        let cap = ((perim / spacing).floor() as usize).max(1);
        let n = cap.min(idxs.len() - k);
        for j in 0..n {
            let i = idxs[k + j];
            let pos = (j as f64 + 0.5) / n as f64 * perim;
            let at = if pos < 2.0 * ihw {
                Point2 {
                    x: cx - ihw + pos,
                    y: cy - ihh,
                }
            } else if pos < 2.0 * ihw + 2.0 * ihh {
                Point2 {
                    x: cx + ihw,
                    y: cy - ihh + (pos - 2.0 * ihw),
                }
            } else if pos < 4.0 * ihw + 2.0 * ihh {
                Point2 {
                    x: cx + ihw - (pos - 2.0 * ihw - 2.0 * ihh),
                    y: cy + ihh,
                }
            } else {
                Point2 {
                    x: cx - ihw,
                    y: cy + ihh - (pos - 4.0 * ihw - 2.0 * ihh),
                }
            };
            problem.parts[i].locked = Some(LockedAt { at, rotation: 0.0 });
        }
        k += n;
        ring += 1;
    }
}

/// A point on the perimeter of the axis-aligned rectangle (cx±rw, cy±rh) at arc
/// position `pos` ∈ [0, 4(rw+rh)), walking top→right→bottom→left.
fn ring_pos(cx: f64, cy: f64, rw: f64, rh: f64, pos: f64) -> Point2 {
    if pos < 2.0 * rw {
        Point2 {
            x: cx - rw + pos,
            y: cy - rh,
        }
    } else if pos < 2.0 * rw + 2.0 * rh {
        Point2 {
            x: cx + rw,
            y: cy - rh + (pos - 2.0 * rw),
        }
    } else if pos < 4.0 * rw + 2.0 * rh {
        Point2 {
            x: cx + rw - (pos - 2.0 * rw - 2.0 * rh),
            y: cy + rh,
        }
    } else {
        Point2 {
            x: cx - rw,
            y: cy + rh - (pos - 4.0 * rw - 2.0 * rh),
        }
    }
}

fn fanout_order_by_ic_pad_angle(problem: &PlaceProblem, ic: usize, parts: &[usize]) -> Vec<usize> {
    let mut keyed: Vec<(f64, String, usize)> = parts
        .iter()
        .map(|&part| {
            (
                part_angle_around_ic(problem, ic, part),
                problem.parts[part].reference.clone(),
                part,
            )
        })
        .collect();
    keyed.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    keyed.into_iter().map(|(_, _, part)| part).collect()
}

fn part_angle_around_ic(problem: &PlaceProblem, ic: usize, part: usize) -> f64 {
    let part_nets: std::collections::BTreeSet<&str> = problem.parts[part]
        .pads
        .iter()
        .filter_map(|pad| pad.net.as_deref())
        .collect();
    let mut signal_angles = Vec::new();
    let mut all_angles = Vec::new();
    for pad in &problem.parts[ic].pads {
        let Some(net) = pad.net.as_deref() else {
            continue;
        };
        if !part_nets.contains(net) {
            continue;
        }
        let angle = pad.offset.y.atan2(pad.offset.x);
        all_angles.push(angle);
        if !is_powerish_net(net) {
            signal_angles.push(angle);
        }
    }
    mean_angle(if signal_angles.is_empty() {
        &all_angles
    } else {
        &signal_angles
    })
    .unwrap_or(0.0)
}

fn mean_angle(angles: &[f64]) -> Option<f64> {
    if angles.is_empty() {
        return None;
    }
    let (sin, cos) = angles.iter().fold((0.0, 0.0), |(sin, cos), angle| {
        (sin + angle.sin(), cos + angle.cos())
    });
    Some(sin.atan2(cos))
}

fn resonator_parts(problem: &PlaceProblem, ic: usize) -> Vec<usize> {
    let ic_nets: std::collections::BTreeSet<&str> = problem.parts[ic]
        .pads
        .iter()
        .filter_map(|pad| pad.net.as_deref())
        .collect();
    problem
        .parts
        .iter()
        .enumerate()
        .filter_map(|(idx, part)| {
            if idx == ic || part.pads.len() != 2 {
                return None;
            }
            let nets: Vec<&str> = part
                .pads
                .iter()
                .filter_map(|pad| pad.net.as_deref())
                .collect();
            if nets.len() != 2
                || nets[0] == nets[1]
                || nets.iter().any(|net| is_powerish_net(net))
                || !nets.iter().all(|net| ic_nets.contains(net))
            {
                return None;
            }
            Some(idx)
        })
        .collect()
}

fn bus_peripherals(problem: &PlaceProblem, ic: usize) -> Vec<usize> {
    let ic_nets: std::collections::BTreeSet<&str> = problem.parts[ic]
        .pads
        .iter()
        .filter_map(|pad| pad.net.as_deref())
        .filter(|net| !is_powerish_net(net))
        .collect();
    (0..problem.parts.len())
        .filter(|&idx| idx != ic && problem.parts[idx].pads.len() >= 4)
        .filter(|&idx| {
            problem.parts[idx]
                .pads
                .iter()
                .filter_map(|pad| pad.net.as_deref())
                .filter(|net| !is_powerish_net(net) && ic_nets.contains(net))
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                >= 3
        })
        .collect()
}

fn resonator_load_caps(problem: &PlaceProblem, resonator: usize) -> Vec<usize> {
    let resonator_nets: std::collections::BTreeSet<&str> = problem.parts[resonator]
        .pads
        .iter()
        .filter_map(|pad| pad.net.as_deref())
        .collect();
    problem
        .parts
        .iter()
        .enumerate()
        .filter_map(|(idx, part)| {
            if idx == resonator || part.pads.len() != 2 {
                return None;
            }
            let nets: Vec<&str> = part
                .pads
                .iter()
                .filter_map(|pad| pad.net.as_deref())
                .collect();
            if nets.len() != 2 {
                return None;
            }
            let touches_resonator = nets.iter().any(|net| resonator_nets.contains(net));
            let touches_power = nets.iter().any(|net| is_powerish_net(net));
            (touches_resonator && touches_power).then_some(idx)
        })
        .collect()
}

fn place_resonator_clusters(
    problem: &mut PlaceProblem,
    ic: usize,
    resonators: &[usize],
    ic_hw: f64,
    ic_hh: f64,
) {
    for &resonator in resonators {
        let mut members = resonator_load_caps(problem, resonator);
        members.sort_by(|&a, &b| {
            part_angle_around_ic(problem, ic, a)
                .partial_cmp(&part_angle_around_ic(problem, ic, b))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| problem.parts[a].reference.cmp(&problem.parts[b].reference))
        });
        let insert = members.len() / 2;
        members.insert(insert, resonator);
        place_tangent_cluster(
            problem,
            ic,
            &members,
            part_angle_around_ic(problem, ic, resonator),
            ic_hw,
            ic_hh,
        );
    }
}

fn place_bus_clusters(
    problem: &mut PlaceProblem,
    ic: usize,
    buses: &[usize],
    ic_hw: f64,
    ic_hh: f64,
) {
    for &bus in buses {
        place_tangent_cluster(
            problem,
            ic,
            &[bus],
            part_angle_around_ic(problem, ic, bus),
            ic_hw,
            ic_hh,
        );
    }
}

fn place_tangent_cluster(
    problem: &mut PlaceProblem,
    _ic: usize,
    members: &[usize],
    angle: f64,
    ic_hw: f64,
    ic_hh: f64,
) {
    if members.is_empty() {
        return;
    }
    let dir = Point2 {
        x: angle.cos(),
        y: angle.sin(),
    };
    let tangent = Point2 {
        x: -dir.y,
        y: dir.x,
    };
    let radial_half = |part: &Part| -> f64 {
        dir.x.abs() * part.courtyard_w / 2.0 + dir.y.abs() * part.courtyard_h / 2.0
    };
    let tangent_half = |part: &Part| -> f64 {
        tangent.x.abs() * part.courtyard_w / 2.0 + tangent.y.abs() * part.courtyard_h / 2.0
    };
    let cluster_radial = members
        .iter()
        .map(|&idx| radial_half(&problem.parts[idx]))
        .fold(0.0, f64::max);
    let boundary = (if dir.x.abs() > 1e-9 {
        ic_hw / dir.x.abs()
    } else {
        f64::INFINITY
    })
    .min(if dir.y.abs() > 1e-9 {
        ic_hh / dir.y.abs()
    } else {
        f64::INFINITY
    });
    let base_r = boundary + cluster_radial + 0.8;
    let total_tangent: f64 = members
        .iter()
        .map(|&idx| 2.0 * tangent_half(&problem.parts[idx]) + 0.6)
        .sum::<f64>()
        - 0.6;
    let mut cursor = -total_tangent / 2.0;
    for &idx in members {
        let th = tangent_half(&problem.parts[idx]);
        cursor += th;
        problem.parts[idx].locked = Some(LockedAt {
            at: Point2 {
                x: dir.x * base_r + tangent.x * cursor,
                y: dir.y * base_r + tangent.y * cursor,
            },
            rotation: 0.0,
        });
        cursor += th + 0.6;
    }
}

fn locked_max_extent(problem: &PlaceProblem) -> f64 {
    problem
        .parts
        .iter()
        .filter_map(|part| part.locked.as_ref().map(|loc| (part, loc)))
        .map(|(part, loc)| {
            (loc.at.x.abs() + part.courtyard_w / 2.0).max(loc.at.y.abs() + part.courtyard_h / 2.0)
        })
        .fold(0.0, f64::max)
}

fn resolve_locked_overlaps_radially(problem: &mut PlaceProblem, fixed: usize) {
    for _ in 0..2000 {
        let mut hit = None;
        'pairs: for a in 0..problem.parts.len() {
            for b in (a + 1)..problem.parts.len() {
                if locked_overlap(problem, a, b) {
                    hit = Some((a, b));
                    break 'pairs;
                }
            }
        }
        let Some((a, b)) = hit else {
            return;
        };
        let move_idx = match (a == fixed, b == fixed) {
            (true, true) => return,
            (true, false) => b,
            (false, true) => a,
            (false, false) => {
                if locked_radius(problem, a) >= locked_radius(problem, b) {
                    a
                } else {
                    b
                }
            }
        };
        push_locked_outward(problem, move_idx, 1.0);
    }
}

fn locked_overlap(problem: &PlaceProblem, a: usize, b: usize) -> bool {
    let (Some(la), Some(lb)) = (&problem.parts[a].locked, &problem.parts[b].locked) else {
        return false;
    };
    let (ahw, ahh) = rotated_locked_half(&problem.parts[a], la.rotation);
    let (bhw, bhh) = rotated_locked_half(&problem.parts[b], lb.rotation);
    (la.at.x - lb.at.x).abs() < ahw + bhw && (la.at.y - lb.at.y).abs() < ahh + bhh
}

fn locked_radius(problem: &PlaceProblem, idx: usize) -> f64 {
    problem.parts[idx]
        .locked
        .as_ref()
        .map(|loc| loc.at.x.hypot(loc.at.y))
        .unwrap_or(0.0)
}

fn push_locked_outward(problem: &mut PlaceProblem, idx: usize, step: f64) {
    let Some(loc) = problem.parts[idx].locked.as_mut() else {
        return;
    };
    let len = loc.at.x.hypot(loc.at.y);
    if len <= 1e-9 {
        loc.at.x += step;
    } else {
        loc.at.x += loc.at.x / len * step;
        loc.at.y += loc.at.y / len * step;
    }
}

fn rotated_locked_half(part: &Part, rotation: f64) -> (f64, f64) {
    if matches!(geom::snap_quadrant(rotation) as i32, 90 | 270) {
        (part.courtyard_h / 2.0, part.courtyard_w / 2.0)
    } else {
        (part.courtyard_w / 2.0, part.courtyard_h / 2.0)
    }
}

fn is_powerish_net(net: &str) -> bool {
    // Reference/regulated rails count as "powerish" for hint weighting even
    // though they are not supply rails in the shared vocabulary.
    let n = net.trim_start_matches('/').to_ascii_uppercase();
    circuit_graph::netclass::is_power_net(&n)
        || n.starts_with("VREG")
        || n.starts_with("VREF")
        || n.starts_with("VSS")
        || n.starts_with('+')
}

/// UNIFIED radial fan-out placement: the dominant IC centred, its decoupling caps
/// then series resistors (in IC-pad order) then other passives on density-aware
/// CONCENTRIC rings, and connectors on the outer frame — EVERYTHING placed
/// overlap-free by construction and locked, with `bounds` sized to fit. This is the
/// placer the lock-then-legalize path couldn't be: escapes route radially (short,
/// parallel, non-crossing) and the board is compact. Returns false (no-op) when
/// there's no clear dominant fine-pitch IC or the agent already pinned parts.
pub fn unified_fanout_place(problem: &mut PlaceProblem, hints: &PlacementHints) -> bool {
    let n = problem.parts.len();
    if problem.parts.iter().any(|p| p.locked.is_some()) {
        return false; // respect any agent-pinned layout
    }
    let original = problem.clone();
    let original_bounds = problem.bounds;
    // Edge-seeking parts are connectors/mechanical interfaces even when their
    // reference uses an IC-like prefix (some libraries assign USB receptacles U1).
    // Never let a high-pad-count edge connector steal the central fan-out role.
    let is_edge_part = |i: usize| {
        let reference = &problem.parts[i].reference;
        hints.edge_seek.iter().any(|r| r == reference)
            || hints.corner_seek.iter().any(|r| r == reference)
            || hints
                .groups
                .iter()
                .any(|group| group.edge.is_some() && group.members.iter().any(|r| r == reference))
    };
    let Some(ic) = (0..n)
        .filter(|&i| problem.parts[i].pads.len() >= 16 && !is_edge_part(i))
        .max_by_key(|&i| problem.parts[i].pads.len())
    else {
        return false;
    };
    // A connector with 2 power pads (e.g. a 1x02 VCC/GND header) looks like a
    // decoupling cap to decoupling_pairs — exclude J/P/H refs so connectors go to the
    // edge frame, not the inner cap ring.
    let is_connector = |i: usize| {
        is_edge_part(i)
            || matches!(
                problem.parts[i].reference.chars().next(),
                Some('J') | Some('P') | Some('H')
            )
    };
    let caps: Vec<usize> = decoupling_pairs(problem)
        .iter()
        .filter(|(c, a)| *a == ic && !is_connector(*c))
        .map(|(c, _)| *c)
        .collect();
    let res: Vec<usize> = series_pairs(problem)
        .iter()
        .filter(|(r, a)| *a == ic && !is_connector(*r))
        .map(|(r, _)| *r)
        .collect();
    let resonators: Vec<usize> = resonator_parts(problem, ic)
        .into_iter()
        .filter(|r| !is_connector(*r))
        .collect();
    let bus_parts: Vec<usize> = bus_peripherals(problem, ic)
        .into_iter()
        .filter(|p| !is_connector(*p))
        .collect();
    let resonator_loads: std::collections::BTreeSet<usize> = resonators
        .iter()
        .flat_map(|&r| resonator_load_caps(problem, r))
        .collect();
    // Engage when the dominant IC has enough fan-out members to ring (decoupling caps
    // and/or series elements). Caps alone <3 isn't enough, but caps+series ≥3 is — so
    // boards with few caps but a series/connector fan-out still get the neat ring.
    if caps.len() + res.len() + resonators.len() + bus_parts.len() < 3 {
        return false;
    }
    let caps_ordered = fanout_order_by_ic_pad_angle(problem, ic, &caps);
    let cap_ring_ordered: Vec<usize> = caps_ordered
        .iter()
        .copied()
        .filter(|idx| !resonator_loads.contains(idx))
        .collect();
    let res_ordered = fanout_order_by_ic_pad_angle(problem, ic, &res);
    let resonators_ordered = fanout_order_by_ic_pad_angle(problem, ic, &resonators);
    let bus_ordered = fanout_order_by_ic_pad_angle(problem, ic, &bus_parts);

    let used: std::collections::BTreeSet<usize> = std::iter::once(ic)
        .chain(caps_ordered.iter().copied())
        .chain(res_ordered.iter().copied())
        .chain(resonators_ordered.iter().copied())
        .chain(bus_ordered.iter().copied())
        .collect();
    let (mut connectors, mut others) = (Vec::new(), Vec::new());
    for i in 0..n {
        if used.contains(&i) {
            continue;
        }
        if is_connector(i) {
            connectors.push(i);
        } else {
            others.push(i);
        }
    }

    let (ihw, ihh) = (
        problem.parts[ic].courtyard_w / 2.0,
        problem.parts[ic].courtyard_h / 2.0,
    );
    problem.parts[ic].locked = Some(LockedAt {
        at: Point2 { x: 0.0, y: 0.0 },
        rotation: 0.0,
    });

    // Concentric rings: caps (innermost), then pad-ordered resistors, then others.
    let mut ring_order = cap_ring_ordered;
    ring_order.extend(res_ordered.iter().copied());
    ring_order.extend(others.iter().copied());
    ring_order.retain(|&idx| {
        problem.parts[idx]
            .courtyard_w
            .max(problem.parts[idx].courtyard_h)
            + 0.8
            <= 8.0
    });
    let spacing = ring_order
        .iter()
        .map(|&idx| {
            problem.parts[idx]
                .courtyard_w
                .max(problem.parts[idx].courtyard_h)
                + 0.8
        })
        .fold(3.0_f64, f64::max);
    if spacing > 8.0 || ring_order.is_empty() {
        *problem = original;
        return false;
    }
    let mut max_extent = ihw.max(ihh);
    let (mut k, mut ring) = (0usize, 0usize);
    while k < ring_order.len() {
        let g = 3.0 + ring as f64 * spacing;
        let (rw, rh) = (ihw + g, ihh + g);
        let perim = 4.0 * (rw + rh);
        let cap = ((perim / spacing).floor() as usize).max(1);
        let m = cap.min(ring_order.len() - k);
        for j in 0..m {
            let i = ring_order[k + j];
            let pos = (j as f64 + 0.5) / m as f64 * perim;
            problem.parts[i].locked = Some(LockedAt {
                at: ring_pos(0.0, 0.0, rw, rh, pos),
                rotation: 0.0,
            });
        }
        max_extent = max_extent.max(rw.max(rh));
        k += m;
        ring += 1;
    }
    place_resonator_clusters(problem, ic, &resonators_ordered, ihw, ihh);
    place_bus_clusters(problem, ic, &bus_ordered, ihw, ihh);
    resolve_locked_overlaps_radially(problem, ic);
    max_extent = max_extent.max(locked_max_extent(problem));

    // Connectors on the outer frame, each ROTATED to lie flat along its edge (long
    // dim along the edge, SHORT dim pointing outward) and packed per-side by its long
    // dim. The frame sits just the short half-extent beyond the rings → compact.
    let conn_out = connectors
        .iter()
        .map(|&i| {
            problem.parts[i]
                .courtyard_w
                .min(problem.parts[i].courtyard_h)
                / 2.0
        })
        .fold(0.0_f64, f64::max);
    let frame = max_extent + conn_out + 3.0;
    // Group connectors per side, then CENTRE each side's run on its edge so none sit
    // at a corner (where adjacent-edge connectors would collide).
    // Use only the opposite top/bottom edges: adjacent edges can collide at the shared
    // corner when headers are large relative to the fanout cluster.
    let mut by_side: [Vec<usize>; 4] = Default::default();
    for (j, &i) in connectors.iter().enumerate() {
        by_side[(j % 2) * 2].push(i);
    }
    for (side, group) in by_side.iter().enumerate() {
        let total: f64 = group
            .iter()
            .map(|&i| {
                problem.parts[i]
                    .courtyard_w
                    .max(problem.parts[i].courtyard_h)
                    + 2.0
            })
            .sum::<f64>()
            - 2.0;
        let mut cur = -total / 2.0;
        for &i in group {
            let (cw, ch) = (problem.parts[i].courtyard_w, problem.parts[i].courtyard_h);
            let l = cw.max(ch);
            let horizontal_edge = side == 0 || side == 2;
            let rot = if (cw >= ch) == horizontal_edge {
                0.0
            } else {
                90.0
            };
            cur += l / 2.0;
            let along = cur;
            cur += l / 2.0 + 2.0;
            problem.parts[i].locked = Some(LockedAt {
                at: match side {
                    0 => Point2 {
                        x: along,
                        y: -frame,
                    },
                    1 => Point2 { x: frame, y: along },
                    2 => Point2 { x: along, y: frame },
                    _ => Point2 {
                        x: -frame,
                        y: along,
                    },
                },
                rotation: rot,
            });
        }
    }

    // Nudge each connector OUTWARD (away from centre) until it clears every ring part
    // — the frame estimate can under-clear a connector whose courtyard exceeds the
    // short-dim guess. Locked ring parts don't move; the connector slides out.
    let chalf = |p: &Part, rot: f64| -> (f64, f64) {
        if matches!(geom::snap_quadrant(rot) as i32, 90 | 270) {
            (p.courtyard_h / 2.0, p.courtyard_w / 2.0)
        } else {
            (p.courtyard_w / 2.0, p.courtyard_h / 2.0)
        }
    };
    for &i in &connectors {
        for _ in 0..60 {
            let (ix, iy, irot) = {
                let l = problem.parts[i].locked.as_ref().unwrap();
                (l.at.x, l.at.y, l.rotation)
            };
            let (ihw, ihh) = chalf(&problem.parts[i], irot);
            let hit = (0..problem.parts.len()).any(|j| {
                if j == i {
                    return false;
                }
                let Some(lj) = &problem.parts[j].locked else {
                    return false;
                };
                let (jhw, jhh) = chalf(&problem.parts[j], lj.rotation);
                (ix - lj.at.x).abs() < ihw + jhw && (iy - lj.at.y).abs() < ihh + jhh
            });
            if !hit {
                break;
            }
            // Push along whichever axis is the connector's outward (edge-normal) one,
            // away from centre, so it slides off the ring rather than along it.
            let l = problem.parts[i].locked.as_mut().unwrap();
            if ihh <= ihw {
                l.at.y += if iy >= 0.0 { 2.0 } else { -2.0 };
            } else {
                l.at.x += if ix >= 0.0 { 2.0 } else { -2.0 };
            }
        }
    }

    // Shift everything into the first quadrant with a margin and size `bounds` to fit.
    let margin = 2.0;
    let (mut mnx, mut mny, mut mxx, mut mxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for p in &problem.parts {
        if let Some(l) = &p.locked {
            // Rotation-aware extent: a 90/270° part swaps w/h.
            let (phw, phh) = if matches!(geom::snap_quadrant(l.rotation) as i32, 90 | 270) {
                (p.courtyard_h / 2.0, p.courtyard_w / 2.0)
            } else {
                (p.courtyard_w / 2.0, p.courtyard_h / 2.0)
            };
            mnx = mnx.min(l.at.x - phw);
            mny = mny.min(l.at.y - phh);
            mxx = mxx.max(l.at.x + phw);
            mxy = mxy.max(l.at.y + phh);
        }
    }
    let placed_w = (mxx - mnx) + 2.0 * margin;
    let placed_h = (mxy - mny) + 2.0 * margin;
    let board_w = original_bounds.max_x - original_bounds.min_x;
    let board_h = original_bounds.max_y - original_bounds.min_y;
    if placed_w > board_w + 1e-9 || placed_h > board_h + 1e-9 {
        *problem = original;
        return false;
    }
    let (dx, dy) = (
        original_bounds.min_x + margin - mnx + (board_w - placed_w) / 2.0,
        original_bounds.min_y + margin - mny + (board_h - placed_h) / 2.0,
    );
    for p in &mut problem.parts {
        if let Some(l) = &mut p.locked {
            l.at.x += dx;
            l.at.y += dy;
        }
    }
    problem.bounds = original_bounds;
    true
}
