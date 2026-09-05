//! The greedy [`TextSolver`] leaf: collision-free positions for movable text.
//!
//! Pure geometry — no I/O, no KiCAD environment — so the core is unit-testable
//! and deterministic. `textsolve.rs` builds [`Obstacle`]s and [`Movable`]s from
//! writer state, calls [`GreedyText`], and applies the returned picks.
//!
//! The solver is greedy: movables are processed in the order given (callers
//! pass a deterministic order — most-constrained first), each takes its first
//! candidate that collides with nothing, and the chosen box becomes an
//! obstacle for everything after it. Greedy is enough here because candidate
//! lists are short and ordered by convention (the first candidate is the
//! KiCAD-conventional spot); a global optimizer would buy little and cost
//! determinism scrutiny.

use geom::Rect;
use sch_model::text::{Movable, Obstacle, Pick, TextSolver};

/// First-fit text placement in the caller's order.
pub struct GreedyText;

impl TextSolver for GreedyText {
    fn name(&self) -> &'static str {
        "greedy"
    }

    /// For each movable (in order), the first candidate that collides with no obstacle
    /// (minus same-owner exemptions) and no previously chosen box.
    ///
    /// When nothing is free the pick is the candidate burying the LEAST foreign ink,
    /// reported `fits: false` so the caller can still degrade it (lint-flagged for
    /// fields/labels, hidden for optional text like repeated power-rail names).
    /// Falling back to candidate 0 instead put the text at its conventional spot
    /// *because* that spot was conventional, which on a crowded symbol is squarely
    /// on the pin names — area, not order, is what decides whether the run still
    /// reads.
    fn solve(&self, obstacles: &[Obstacle], movables: &[Movable]) -> Vec<Pick> {
        // Two text boxes that merely ABUT (share an edge, 0 gap) pass the strict-inequality
        // overlap test yet render as one run ("10kGND", "3V3GND"). Keep a small gap between
        // movable text boxes by testing a slightly GROWN candidate against already-placed text.
        const TEXT_GAP: f64 = 0.6;
        let grow = |b: &Rect| -> Rect { b.inflate(TEXT_GAP) };
        let mut placed: Vec<Rect> = Vec::new();
        let mut out = Vec::with_capacity(movables.len());
        for m in movables {
            let free = |b: &Rect| {
                obstacles.iter().all(|o| {
                    (o.owner.is_some() && o.owner == m.owner)
                        || b.intersection(&o.bbox).is_none()
                }) && placed.iter().all(|p| grow(b).intersection(p).is_none())
            };
            let buried = |b: &Rect| -> f64 {
                let own = |o: &&Obstacle| !(o.owner.is_some() && o.owner == m.owner);
                obstacles
                    .iter()
                    .filter(own)
                    .map(|o| o.bbox)
                    .chain(placed.iter().copied())
                    .filter_map(|o| b.intersection(&o))
                    .map(|hit| hit.width() * hit.height())
                    .sum()
            };
            let pick = m.candidates.iter().position(free);
            let idx = pick.unwrap_or_else(|| {
                (0..m.candidates.len())
                    .min_by(|&a, &b| buried(&m.candidates[a]).total_cmp(&buried(&m.candidates[b])))
                    .unwrap_or(0)
            });
            placed.push(m.candidates[idx]);
            out.push(Pick {
                candidate: idx,
                fits: pick.is_some(),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::Rect;
    use sch_model::text::Owner;

    fn hard(b: Rect) -> Obstacle {
        Obstacle { bbox: b, owner: None }
    }

    #[test]
    fn picks_first_free_candidate() {
        let obstacles = vec![hard(Rect::new(0.0, 0.0, 10.0, 10.0))];
        let m = Movable {
            owner: None,
            candidates: vec![
                Rect::new(5.0, 5.0, 8.0, 8.0),
                Rect::new(12.0, 0.0, 15.0, 3.0),
            ],
        };
        assert_eq!(
            GreedyText.solve(&obstacles, &[m]),
            vec![Pick {
                candidate: 1,
                fits: true
            }]
        );
    }

    #[test]
    fn falls_back_to_candidate_zero_when_all_collide() {
        let obstacles = vec![hard(Rect::new(0.0, 0.0, 20.0, 20.0))];
        let m = Movable {
            owner: None,
            candidates: vec![Rect::new(1.0, 1.0, 2.0, 2.0), Rect::new(3.0, 3.0, 4.0, 4.0)],
        };
        assert_eq!(
            GreedyText.solve(&obstacles, &[m]),
            vec![Pick {
                candidate: 0,
                fits: false
            }]
        );
    }

    #[test]
    fn own_body_is_exempt_but_hard_is_not() {
        let obstacles = vec![
            Obstacle {
                bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
                owner: Some(Owner::Symbol("R1".into())),
            },
            hard(Rect::new(0.0, 0.0, 4.0, 4.0)),
        ];
        // Candidate 0 overlaps both; only the body is exempt for R1, so the
        // hard obstacle still rejects it. Candidate 1 overlaps the body only
        // -> exempt -> chosen.
        let m = Movable {
            owner: Some(Owner::Symbol("R1".into())),
            candidates: vec![Rect::new(1.0, 1.0, 3.0, 3.0), Rect::new(5.0, 5.0, 9.0, 9.0)],
        };
        assert_eq!(
            GreedyText.solve(&obstacles, &[m]),
            vec![Pick {
                candidate: 1,
                fits: true
            }]
        );
        // A different owner gets no exemption anywhere -> all collide -> fallback.
        let m2 = Movable {
            owner: Some(Owner::Symbol("R2".into())),
            candidates: vec![Rect::new(1.0, 1.0, 3.0, 3.0), Rect::new(5.0, 5.0, 9.0, 9.0)],
        };
        assert_eq!(
            GreedyText.solve(&obstacles, &[m2]),
            vec![Pick {
                candidate: 0,
                fits: false
            }]
        );
    }

    #[test]
    fn chosen_boxes_block_later_movables() {
        let a = Movable {
            owner: None,
            candidates: vec![Rect::new(0.0, 0.0, 5.0, 5.0)],
        };
        let b = Movable {
            owner: None,
            candidates: vec![
                Rect::new(1.0, 1.0, 4.0, 4.0),
                Rect::new(10.0, 10.0, 12.0, 12.0),
            ],
        };
        assert_eq!(
            GreedyText.solve(&[], &[a, b]),
            vec![
                Pick {
                    candidate: 0,
                    fits: true
                },
                Pick {
                    candidate: 1,
                    fits: true
                }
            ]
        );
    }

    #[test]
    fn edge_touching_is_not_collision() {
        let obstacles = vec![hard(Rect::new(0.0, 0.0, 10.0, 10.0))];
        let m = Movable {
            owner: None,
            candidates: vec![Rect::new(10.0, 0.0, 14.0, 4.0)],
        };
        assert_eq!(
            GreedyText.solve(&obstacles, &[m]),
            vec![Pick {
                candidate: 0,
                fits: true
            }]
        );
    }
}
