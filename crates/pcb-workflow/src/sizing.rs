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
//!   smaller than their total area (or narrower than the widest part, or than a
//!   connector row) can hold them. Below this, placement is impossible, which is
//!   what makes it safe to refuse a board up front.
//! - **recommended** — what will actually place and route: courtyards pack at
//!   roughly half density, plus the room the rules imply (a channel is one track
//!   plus two clearances, and extra copper layers carry part of the demand) and
//!   a copper-free edge ring. This is what an auto-sized board is born with.

/// One part's placement extent: its courtyard, mm.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PartExtent {
    pub w: f64,
    pub h: f64,
    /// Connectors want board edge, so they set a floor on the board's width.
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
}

impl BoardSizing {
    pub fn fits(&self, w: f64, h: f64) -> bool {
        w + 1e-6 >= self.required_w && h + 1e-6 >= self.required_h
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

    let min_w = (max_w + 2.0 * EDGE_CLEAR_MM).max(connector_span);
    let min_h = max_h + 2.0 * EDGE_CLEAR_MM;
    // Courtyards may not overlap, so their bare total area is a floor nothing
    // can beat — which is what makes refusing a smaller board up front honest.
    let (required_w, required_h) = outline_for(courtyard_area_mm2, aspect, min_w, min_h);

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
        required_w: required_w.ceil(),
        required_h: required_h.ceil(),
        recommended_w: recommended_w.ceil().max(required_w.ceil()),
        recommended_h: recommended_h.ceil().max(required_h.ceil()),
        courtyard_area_mm2: (courtyard_area_mm2 * 10.0).round() / 10.0,
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

    #[test]
    fn the_widest_part_and_the_connector_row_set_the_width_floor() {
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
        assert!(sizing.required_w >= 62.0, "{sizing:?}");
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
