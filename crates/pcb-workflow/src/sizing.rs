//! How big a board its own parts require.
//!
//! One law, used twice: `regenerate_board` sizes the outline BEFORE anything is
//! placed, and `place_board` quotes the same numbers when a pack fails. A board
//! that is too small is only discovered at placement otherwise, which costs the
//! caller a regenerate/place round-trip per guess.
//!
//! Two answers come out of it:
//!
//! - **required** — a hard floor: the courtyards cannot overlap, so no outline
//!   smaller than their total area can hold them, and every part must fit inside
//!   the outline at SOME rotation. Below this, placement is impossible, which is
//!   what makes it safe to refuse a board up front. Nothing softer than geometry
//!   belongs here — a connector's wish for an edge shapes `recommended`, not
//!   this.
//! - **recommended** — what will actually place and route: courtyards pack at
//!   roughly half density, plus the room the rules imply (a channel is one track
//!   plus two clearances, and extra copper layers carry part of the demand) and
//!   a copper-free edge ring. This is what an auto-sized board is born with.

/// One part's placement extent: its courtyard, mm.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PartExtent {
    pub w: f64,
    pub h: f64,
    /// Connectors want an edge each, so they widen the RECOMMENDED board; the
    /// hard floor stays pure geometry.
    pub edge_seeking: bool,
}

/// The routing demand a rule set puts on the outline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RoutingDemand {
    pub clearance: f64,
    pub track_width: f64,
    pub layer_count: u32,
    pub net_count: usize,
}

/// Courtyards pack at roughly half density once orientation, courtyard margin
/// and escape room are paid for — the factor `place_board`'s retry estimate has
/// always used.
const PACKING_FACTOR: f64 = 2.0;

/// Copper must keep this far from the board edge, so every part is inset by it.
const EDGE_CLEAR_MM: f64 = 0.5;

/// The board sizes a set of parts implies, mm.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoardSizing {
    pub required_w: f64,
    pub required_h: f64,
    pub recommended_w: f64,
    pub recommended_h: f64,
    pub courtyard_area_mm2: f64,
    /// The board's LONG side must be at least this: the biggest part's long
    /// side, whichever way the placer turns it.
    required_long_side: f64,
    /// The board's SHORT side must be at least this, for the same reason.
    required_short_side: f64,
}

impl BoardSizing {
    /// Whether an outline is at or above the hard floor. Only a `false` here
    /// justifies refusing a board before it is written.
    ///
    /// Orientation-free: the courtyards need their area, and every part needs to
    /// fit at SOME rotation, so it is the board's long and short sides that are
    /// tested — not its width and height.
    pub fn fits(&self, w: f64, h: f64) -> bool {
        const SLACK: f64 = 1e-6;
        w * h + SLACK >= self.courtyard_area_mm2
            && w.max(h) + SLACK >= self.required_long_side
            && w.min(h) + SLACK >= self.required_short_side
    }

    /// Grow `recommended` past an outline that has already failed to place.
    ///
    /// A retry that suggests the size which just failed — or a smaller one — is
    /// a loop. Placement is not a pure function of area (locked parts, edge
    /// datums, aspect), so a failure is evidence the estimate was optimistic.
    pub fn grown_past(mut self, failed_w: f64, failed_h: f64) -> Self {
        const RETRY_GROWTH: f64 = 1.3;
        self.recommended_w = self.recommended_w.max((failed_w * RETRY_GROWTH).ceil());
        self.recommended_h = self.recommended_h.max((failed_h * RETRY_GROWTH).ceil());
        self
    }
}

/// Lay `area` out at `aspect` (width / height), then widen to the floors.
fn outline_for(area: f64, aspect: f64, min_w: f64, min_h: f64) -> (f64, f64) {
    let aspect = if aspect.is_finite() && aspect > 0.0 {
        aspect
    } else {
        1.0
    };
    let height = (area / aspect).sqrt();
    ((aspect * height).max(min_w), height.max(min_h))
}

/// Size a board from the parts it will carry.
///
/// `aspect` is the width/height the caller wants preserved (1.0 for a fresh
/// auto-sized board, the current outline's ratio when growing one).
pub(crate) fn size_board(parts: &[PartExtent], aspect: f64, demand: RoutingDemand) -> BoardSizing {
    let courtyard_area_mm2: f64 = parts.iter().map(|p| p.w * p.h).sum();
    let max_w = parts.iter().map(|p| p.w).fold(0.0, f64::max);
    let max_h = parts.iter().map(|p| p.h).fold(0.0, f64::max);
    // A connector row wants one uninterrupted edge, so the board is at least as
    // wide as the connectors laid end to end.
    let connector_span: f64 = parts
        .iter()
        .filter(|p| p.edge_seeking)
        .map(|p| p.w + 2.0 * EDGE_CLEAR_MM)
        .sum();

    // The placer rotates parts, so the floor a part imposes is its SHORT side on
    // the board's short side and its long side on the long side.
    let widest = parts.iter().map(|p| p.w.max(p.h)).fold(0.0, f64::max);
    let narrowest = parts.iter().map(|p| p.w.min(p.h)).fold(0.0, f64::max);
    let hard_w = widest + 2.0 * EDGE_CLEAR_MM;
    let hard_h = narrowest + 2.0 * EDGE_CLEAR_MM;
    // Courtyards may not overlap, so their bare total area is a floor nothing
    // can beat — which is what makes refusing a smaller board up front honest.
    let (required_w, required_h) = outline_for(courtyard_area_mm2, aspect, hard_w, hard_h);

    // Room to place, not merely to fit: connectors want an edge each, and the
    // widest part wants clearance on both sides at either orientation.
    let min_w = (max_w.max(max_h) + 2.0 * EDGE_CLEAR_MM).max(connector_span);
    let min_h = max_h.max(max_w) + 2.0 * EDGE_CLEAR_MM;
    let packed_area = courtyard_area_mm2 * PACKING_FACTOR;
    // Routing room: each net needs one channel — a track plus a clearance on
    // each side — running a characteristic board-crossing distance, and the
    // signal layers share that demand.
    let channel_pitch = demand.track_width + 2.0 * demand.clearance;
    let signal_layers = f64::from(demand.layer_count.max(2));
    let span = packed_area.sqrt();
    let routing_area = demand.net_count as f64 * channel_pitch * span / signal_layers;
    let (mut recommended_w, mut recommended_h) =
        outline_for(packed_area + routing_area, aspect, min_w, min_h);
    // An edge ring no copper may enter, on all four sides.
    let ring = 2.0 * (EDGE_CLEAR_MM + demand.clearance);
    recommended_w += ring;
    recommended_h += ring;

    BoardSizing {
        required_w: required_w.ceil().max(1.0),
        required_h: required_h.ceil().max(1.0),
        recommended_w: recommended_w.ceil().max(required_w.ceil()).max(1.0),
        recommended_h: recommended_h.ceil().max(required_h.ceil()).max(1.0),
        courtyard_area_mm2: (courtyard_area_mm2 * 10.0).round() / 10.0,
        required_long_side: hard_w,
        required_short_side: hard_h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A USB-C receptacle, a SOT-23-5 LDO and seven 0603 passives — the nine-part
    /// board whose placement failure started this.
    fn nine_part_board() -> Vec<PartExtent> {
        let mut parts = vec![
            PartExtent {
                w: 9.2,
                h: 7.6,
                edge_seeking: true,
            },
            PartExtent {
                w: 3.0,
                h: 3.0,
                edge_seeking: false,
            },
        ];
        parts.extend(std::iter::repeat_n(
            PartExtent {
                w: 2.0,
                h: 1.5,
                edge_seeking: false,
            },
            7,
        ));
        parts
    }

    fn demand() -> RoutingDemand {
        RoutingDemand {
            clearance: 0.15,
            track_width: 0.15,
            layer_count: 2,
            net_count: 8,
        }
    }

    #[test]
    fn a_recommended_board_is_larger_than_the_smallest_one_that_fits() {
        let sizing = size_board(&nine_part_board(), 1.0, demand());
        assert!(
            sizing.recommended_w > sizing.required_w && sizing.recommended_h > sizing.required_h,
            "{sizing:?}"
        );
        assert!(sizing.fits(sizing.required_w, sizing.required_h));
        assert!(sizing.fits(sizing.recommended_w, sizing.recommended_h));
    }

    /// `required` must be a bound placement cannot beat, not the packing
    /// estimate — refusing a board up front is only honest if nothing smaller
    /// could ever have worked.
    #[test]
    fn required_is_a_hard_area_floor_the_courtyards_alone_impose() {
        let sizing = size_board(&nine_part_board(), 1.0, demand());
        let courtyards: f64 = nine_part_board().iter().map(|p| p.w * p.h).sum();
        assert!(sizing.required_w * sizing.required_h >= courtyards);
        assert!(
            sizing.required_w * sizing.required_h
                < courtyards * PACKING_FACTOR + sizing.required_w * 2.0,
            "{sizing:?}"
        );
    }

    #[test]
    fn a_board_below_the_required_size_is_rejected() {
        let sizing = size_board(&nine_part_board(), 1.0, demand());
        assert!(!sizing.fits(sizing.required_w - 1.0, sizing.required_h));
        assert!(!sizing.fits(sizing.required_w, sizing.required_h - 1.0));
    }

    /// A connector row shapes what is comfortable, not what is possible: two
    /// 30 mm connectors fit a board that puts one on each of two edges, so the
    /// hard floor may not sum them — only `recommended` may.
    #[test]
    fn a_connector_row_widens_the_recommendation_but_not_the_hard_floor() {
        let wide = vec![
            PartExtent {
                w: 30.0,
                h: 2.0,
                edge_seeking: true,
            },
            PartExtent {
                w: 30.0,
                h: 2.0,
                edge_seeking: true,
            },
        ];
        let sizing = size_board(&wide, 1.0, demand());
        assert!(sizing.required_w < 62.0, "{sizing:?}");
        assert!(sizing.required_w >= 31.0, "{sizing:?}");
        assert!(sizing.recommended_w >= 62.0, "{sizing:?}");
    }

    /// A long, thin part fits a board narrower than the part only because the
    /// placer can turn it; the hard floor must know that.
    #[test]
    fn the_hard_floor_lets_a_long_part_be_rotated_into_place() {
        let bar = vec![PartExtent {
            w: 30.0,
            h: 2.0,
            edge_seeking: false,
        }];
        let sizing = size_board(&bar, 0.25, demand());
        assert!(sizing.fits(4.0, 31.0), "{sizing:?}");
        assert!(!sizing.fits(4.0, 20.0), "{sizing:?}");
    }

    /// A retry must never propose the size that just failed.
    #[test]
    fn a_failed_outline_is_always_grown_past() {
        let sizing = size_board(&nine_part_board(), 1.0, demand()).grown_past(80.0, 60.0);
        assert!(sizing.recommended_w > 80.0, "{sizing:?}");
        assert!(sizing.recommended_h > 60.0, "{sizing:?}");
    }

    #[test]
    fn more_copper_layers_need_less_routing_headroom() {
        let parts = nine_part_board();
        let two = size_board(&parts, 1.0, demand());
        let four = size_board(
            &parts,
            1.0,
            RoutingDemand {
                layer_count: 4,
                ..demand()
            },
        );
        assert_eq!(two.required_w, four.required_w);
        assert!(four.recommended_w < two.recommended_w, "{two:?} {four:?}");
    }
}
