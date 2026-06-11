//! Text placement solver: choose collision-free positions for movable text.
//!
//! Pure geometry — no I/O, no KiCAD environment — so the core is unit-testable
//! and deterministic. `emit.rs` builds [`Obstacle`]s and [`Movable`]s from
//! writer state, calls [`choose`], and applies the returned candidate indices.
//!
//! The solver is greedy: movables are processed in the order given (callers
//! pass a deterministic order — most-constrained first), each takes its first
//! candidate that collides with nothing, and the chosen box becomes an
//! obstacle for everything after it. Greedy is enough here because candidate
//! lists are short and ordered by convention (the first candidate is the
//! KiCAD-conventional spot); a global optimizer would buy little and cost
//! determinism scrutiny.

/// An axis-aligned bbox: `[min_x, min_y, max_x, max_y]` (sheet mm, y down).
pub(crate) type BBox = [f64; 4];

/// Whether two boxes overlap (open intervals: edge-touching is NOT overlap,
/// matching the lint's `boxes_overlap` so solver and oracle agree).
pub(crate) fn boxes_overlap(a: &BBox, b: &BBox) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

/// Fixed geometry a movable must not collide with.
pub(crate) enum ObKind {
    /// A symbol body, exempted for text OWNED by that refdes (a label on its
    /// own pin endpoint legitimately sits inside its symbol's generous bbox).
    OwnExempt(String),
    /// Never exempted: pin text, wires, fixed labels, no-connects.
    Hard,
}

pub(crate) struct Obstacle {
    pub bbox: BBox,
    pub kind: ObKind,
}

/// One piece of movable text with its candidate boxes in preference order.
pub(crate) struct Movable {
    /// Owning refdes, matched against [`ObKind::OwnExempt`].
    pub owner: Option<String>,
    /// Candidate bboxes, best-first. Never empty.
    pub candidates: Vec<BBox>,
}

/// For each movable (in order), the index of the first candidate that collides
/// with no obstacle (minus own-body exemptions) and no previously chosen box.
/// Falls back to candidate 0 when none is free (caller's lint then flags it —
/// visible degradation, per spec).
pub(crate) fn choose(obstacles: &[Obstacle], movables: &[Movable]) -> Vec<usize> {
    let mut placed: Vec<BBox> = Vec::new();
    let mut out = Vec::with_capacity(movables.len());
    for m in movables {
        let free = |b: &BBox| {
            obstacles.iter().all(|o| match &o.kind {
                ObKind::OwnExempt(r) if Some(r) == m.owner.as_ref() => true,
                _ => !boxes_overlap(b, &o.bbox),
            }) && placed.iter().all(|p| !boxes_overlap(b, p))
        };
        let idx = m.candidates.iter().position(|c| free(c)).unwrap_or(0);
        placed.push(m.candidates[idx]);
        out.push(idx);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hard(b: BBox) -> Obstacle {
        Obstacle { bbox: b, kind: ObKind::Hard }
    }

    #[test]
    fn picks_first_free_candidate() {
        let obstacles = vec![hard([0.0, 0.0, 10.0, 10.0])];
        let m = Movable {
            owner: None,
            candidates: vec![[5.0, 5.0, 8.0, 8.0], [12.0, 0.0, 15.0, 3.0]],
        };
        assert_eq!(choose(&obstacles, &[m]), vec![1]);
    }

    #[test]
    fn falls_back_to_candidate_zero_when_all_collide() {
        let obstacles = vec![hard([0.0, 0.0, 20.0, 20.0])];
        let m = Movable {
            owner: None,
            candidates: vec![[1.0, 1.0, 2.0, 2.0], [3.0, 3.0, 4.0, 4.0]],
        };
        assert_eq!(choose(&obstacles, &[m]), vec![0]);
    }

    #[test]
    fn own_body_is_exempt_but_hard_is_not() {
        let obstacles = vec![
            Obstacle { bbox: [0.0, 0.0, 10.0, 10.0], kind: ObKind::OwnExempt("R1".into()) },
            hard([0.0, 0.0, 4.0, 4.0]),
        ];
        // Candidate 0 overlaps both; only the body is exempt for R1, so the
        // hard obstacle still rejects it. Candidate 1 overlaps the body only
        // -> exempt -> chosen.
        let m = Movable {
            owner: Some("R1".into()),
            candidates: vec![[1.0, 1.0, 3.0, 3.0], [5.0, 5.0, 9.0, 9.0]],
        };
        assert_eq!(choose(&obstacles, &[m]), vec![1]);
        // A different owner gets no exemption anywhere -> all collide -> 0.
        let m2 = Movable {
            owner: Some("R2".into()),
            candidates: vec![[1.0, 1.0, 3.0, 3.0], [5.0, 5.0, 9.0, 9.0]],
        };
        assert_eq!(choose(&obstacles, &[m2]), vec![0]);
    }

    #[test]
    fn chosen_boxes_block_later_movables() {
        let a = Movable { owner: None, candidates: vec![[0.0, 0.0, 5.0, 5.0]] };
        let b = Movable {
            owner: None,
            candidates: vec![[1.0, 1.0, 4.0, 4.0], [10.0, 10.0, 12.0, 12.0]],
        };
        assert_eq!(choose(&[], &[a, b]), vec![0, 1]);
    }

    #[test]
    fn edge_touching_is_not_collision() {
        let obstacles = vec![hard([0.0, 0.0, 10.0, 10.0])];
        let m = Movable { owner: None, candidates: vec![[10.0, 0.0, 14.0, 4.0]] };
        assert_eq!(choose(&obstacles, &[m]), vec![0]);
    }
}
