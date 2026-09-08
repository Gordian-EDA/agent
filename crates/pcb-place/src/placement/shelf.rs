//! Guaranteed-legal row packing used when the search portfolio cannot legalize.

use geom::{Point2, Rect};
use pcb_model::{PlaceReport, PlaceResult, Placement, PlacementView};

use crate::{
    compute_hpwl_with_rotations, courtyard_margin, derive_nets, is_legal,
    part_placement_bounds_envelope, rotated_copper_bbox, rotated_courtyard_half,
};

/// Pack free parts in rows around locked parts and keepouts, tallest first.
/// Returns `None` when even this cannot fit, so the caller can keep its own answer.
pub fn shelf_pack(problem: &PlacementView) -> Option<PlaceResult> {
    let margin = courtyard_margin(problem.clearance);
    let rotations: Vec<f64> = problem
        .parts
        .iter()
        .map(|p| p.locked.as_ref().map_or(0.0, |l| geom::snap_quadrant(l.rotation)))
        .collect();
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let copper_bbox: Vec<Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();

    let bounds = problem.bounds;
    // The slot a part needs, relative to its origin: courtyard plus margin,
    // widened by whatever copper envelope must also stay inside the board.
    let slots: Vec<Rect> = (0..problem.parts.len())
        .map(|i| {
            let courtyard = Rect::from_center_half(Point2::new(0.0, 0.0), half[i]).inflate(margin);
            let envelope = part_placement_bounds_envelope(&problem.parts[i], half[i], copper_bbox[i]);
            Rect::new(
                courtyard.min_x.min(envelope.min_x),
                courtyard.min_y.min(envelope.min_y),
                courtyard.max_x.max(envelope.max_x),
                courtyard.max_y.max(envelope.max_y),
            )
        })
        .collect();
    let mut pos = vec![bounds.center(); problem.parts.len()];
    let mut obstacles: Vec<Rect> = problem.keepouts.iter().map(|k| k.inflate(margin)).collect();
    let mut free: Vec<usize> = Vec::new();
    for (i, part) in problem.parts.iter().enumerate() {
        match &part.locked {
            Some(locked) => {
                pos[i] = locked.at;
                obstacles.push(Rect::from_center_half(locked.at, half[i]).inflate(margin));
            }
            None => free.push(i),
        }
    }
    free.sort_by(|&a, &b| slots[b].height().total_cmp(&slots[a].height()).then(a.cmp(&b)));

    let step = 0.5;
    let mut row_y = bounds.min_y;
    let mut row_h: f64 = 0.0;
    let mut cursor_x = bounds.min_x;
    for i in free {
        let slot = slots[i];
        let (w, h) = (slot.width(), slot.height());
        loop {
            if cursor_x + w > bounds.max_x + 1e-9 {
                row_y += row_h + step;
                row_h = 0.0;
                cursor_x = bounds.min_x;
            }
            if row_y + h > bounds.max_y + 1e-9 {
                tracing::warn!(
                    reference = %problem.parts[i].reference,
                    "shelf packing ran out of board height"
                );
                return None;
            }
            let candidate = Rect::new(cursor_x, row_y, cursor_x + w, row_y + h);
            match obstacles.iter().find(|o| {
                let (ox, oy) = candidate.axis_penetration(o);
                ox > 1e-9 && oy > 1e-9
            }) {
                Some(hit) => cursor_x = hit.max_x + step,
                None => break,
            }
        }
        pos[i] = Point2::new(cursor_x - slot.min_x, row_y - slot.min_y);
        obstacles.push(Rect::from_center_half(pos[i], half[i]).inflate(margin));
        cursor_x += w + step;
        row_h = row_h.max(h);
    }

    if !is_legal(problem, &half, &copper_bbox, margin, &pos) {
        tracing::warn!("shelf packing produced an illegal layout");
        return None;
    }
    let nets = derive_nets(problem);
    let hpwl = compute_hpwl_with_rotations(problem, &nets, &pos, &rotations);
    Some(PlaceResult {
        placements: problem
            .parts
            .iter()
            .enumerate()
            .map(|(i, part)| Placement {
                reference: part.reference.clone(),
                at: pos[i],
                rotation: rotations[i],
            })
            .collect(),
        legal: true,
        report: PlaceReport {
            overlaps_resolved: 0,
            out_of_bounds_clamps: 0,
            hpwl,
            layout_cost: hpwl,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_model::Part;

    fn part(reference: &str, w: f64, h: f64) -> Part {
        Part { reference: reference.into(), courtyard_w: w, courtyard_h: h, pads: vec![], edge_datum: None, locked: None }
    }

    #[test]
    fn packs_bluepill_sized_parts_legally() {
        let mut parts = vec![part("J1", 5.0, 51.0), part("J2", 5.0, 51.0), part("U1", 9.0, 9.0), part("Y1", 12.0, 5.0)];
        for i in 0..12 {
            parts.push(part(&format!("C{i}"), 2.0, 3.0));
        }
        let problem = PlacementView {
            bounds: Rect { min_x: 0.0, min_y: 0.0, max_x: 60.0, max_y: 60.0 },
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts,
            keepouts: vec![],
            outline: None,
        };
        let result = shelf_pack(&problem).expect("fits");
        assert!(result.legal);
    }
}
