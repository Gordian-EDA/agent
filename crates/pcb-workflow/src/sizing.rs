//! How big a board its own parts require.
//!
//! One law, used twice: `sync_board` sizes the outline BEFORE anything is
//! placed, and `place_board` quotes the same numbers when a pack fails. A board
//! that is too small is only discovered at placement otherwise, which costs the
//! caller a sync/place round-trip per guess.
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
//!
//! ## Calibration
//!
//! Measured over 79 of the 82 boards in `examples/pcb_circuits` (1–90 parts,
//! SMD, BGA and through-hole; the >100-part scale boards were left out for
//! runtime), each auto-sized at aspect 1.0 and handed to
//! `pcb_engine::place_tuned`:
//!
//! - at `recommended`, **79/79** place legally on the first call;
//! - at 0.85 × that area only 68 do, so the half-density factor is the smallest
//!   that keeps the "one step, legal" promise and it stays 2.0;
//! - 44 of the 79 would still pack at 0.70 × — the promise is generous to the
//!   median board rather than tight everywhere.
//!
//! What used to break the promise was the INPUT, not the factor: `sync_board`
//! measured each part by its library courtyard box while the placer reserves
//! [`placement_extent`]'s origin-symmetric box. On pin-1-origin parts (headers,
//! terminal blocks, most through-hole) that is up to 2× per axis, so such a
//! board was sized from roughly half its real extent — the 9-part 555 of
//! `ne555-tht.json` (741 mm² of extent) was quoted 30 × 30 mm and could not pack
//! there, against the 41 × 41 mm it packs at. Measuring both the same way took
//! the corpus from 44/78 boards legal at their own recommendation to 78/78, for
//! a median recommendation 1.31× larger (1.14× over the boards that already
//! packed).

/// One part's placement extent: the box the placer reserves for it, mm.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PartExtent {
    pub w: f64,
    pub h: f64,
    /// Connectors want an edge each, so they widen the RECOMMENDED board; the
    /// hard floor stays pure geometry.
    pub edge_seeking: bool,
}

/// The extent the placer reserves for a footprint, mm.
///
/// The placer keeps a part's courtyard CENTRED on its origin, so a footprint
/// whose origin sits at pin 1 rather than its middle occupies the box that
/// mirrors it — measuring the raw courtyard instead under-reserves such a part
/// by up to half. Pads are folded in because a footprint may carry no courtyard
/// graphics at all.
pub(crate) fn placement_extent(footprint: &kicad_footprint::Footprint) -> (f64, f64) {
    let (mut hw, mut hh) = abs_half(&footprint.courtyard);
    if let Some(pads) = pad_bbox(&footprint.pads) {
        let (pw, ph) = abs_half(&pads);
        hw = hw.max(pw);
        hh = hh.max(ph);
    }
    (hw * 2.0, hh * 2.0)
}

/// Half-extents of a box mirrored about the footprint origin.
fn abs_half(b: &geom::Rect) -> (f64, f64) {
    (
        b.min_x.abs().max(b.max_x.abs()),
        b.min_y.abs().max(b.max_y.abs()),
    )
}

pub(crate) fn pad_bbox(pads: &[kicad_footprint::FootprintPad]) -> Option<geom::Rect> {
    pads.iter().map(pad_aabb).reduce(|acc, pad| geom::Rect {
        min_x: acc.min_x.min(pad.min_x),
        min_y: acc.min_y.min(pad.min_y),
        max_x: acc.max_x.max(pad.max_x),
        max_y: acc.max_y.max(pad.max_y),
    })
}

fn pad_aabb(pad: &kicad_footprint::FootprintPad) -> geom::Rect {
    let half =
        geom::Point2::new(pad.size.x / 2.0, pad.size.y / 2.0).rotated_half_extents(pad.rotation);
    geom::Rect::new(
        pad.at.x - half.x,
        pad.at.y - half.y,
        pad.at.x + half.x,
        pad.at.y + half.y,
    )
}

/// The routing demand a rule set puts on the outline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RoutingDemand {
    pub clearance: f64,
    pub track_width: f64,
    pub layer_count: u32,
    pub net_count: usize,
}

/// Extents pack at roughly half density once orientation, courtyard margin and
/// escape room are paid for. Calibrated, not assumed: at this factor every
/// sampled board places on the first call, and at 0.85 × the area eleven of them
/// stop.
const PACKING_FACTOR: f64 = 2.0;

/// Copper must keep this far from the board edge, so every part is inset by it.
const EDGE_CLEAR_MM: f64 = 0.5;

/// Routing rules and per-edge channel demand for a placed-board outline fit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RoutingChannels {
    pub clearance: f64,
    pub track_width: f64,
    pub via_diameter: f64,
    pub layer_count: u32,
    /// West, east, north, south net counts.
    pub edge_net_counts: [usize; 4],
}

/// Routing space outside the compact placed-part core, in millimetres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct EdgeHeadroom {
    pub west: f64,
    pub east: f64,
    pub north: f64,
    pub south: f64,
}

/// Convert channel density into a physical routing strip on every edge.
///
/// A strip carries `ceil(edge_nets / copper_layers)` parallel lanes. Empty
/// strips retain only KiCad's copper-to-edge clearance. Otherwise the strip is
/// `edge_clear + clearance + lanes × max(track_width, via_diameter) +
/// (lanes - 1) × clearance`.
pub(crate) fn routing_headroom(channels: RoutingChannels) -> EdgeHeadroom {
    let element = channels.track_width.max(channels.via_diameter).max(0.0);
    let clearance = channels.clearance.max(0.0);
    let layers = channels.layer_count.max(1) as usize;
    let strip = |nets: usize| {
        let lanes = nets.div_ceil(layers);
        if lanes == 0 {
            EDGE_CLEAR_MM
        } else {
            EDGE_CLEAR_MM
                + clearance
                + lanes as f64 * element
                + lanes.saturating_sub(1) as f64 * clearance
        }
    };
    EdgeHeadroom {
        west: strip(channels.edge_net_counts[0]),
        east: strip(channels.edge_net_counts[1]),
        north: strip(channels.edge_net_counts[2]),
        south: strip(channels.edge_net_counts[3]),
    }
}

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
    let longest_side = parts.iter().map(|p| p.w.max(p.h)).fold(0.0, f64::max);
    let deepest_short_side = parts.iter().map(|p| p.w.min(p.h)).fold(0.0, f64::max);
    let hard_w = longest_side + 2.0 * EDGE_CLEAR_MM;
    let hard_h = deepest_short_side + 2.0 * EDGE_CLEAR_MM;
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
    fn routing_headroom_scales_with_cut_density_and_layer_capacity() {
        let two_layer = routing_headroom(RoutingChannels {
            clearance: 0.2,
            track_width: 0.25,
            via_diameter: 0.6,
            layer_count: 2,
            edge_net_counts: [0, 1, 5, 8],
        });
        assert_eq!(two_layer.west, EDGE_CLEAR_MM);
        assert!((two_layer.east - 1.3).abs() < 1e-9);
        assert!((two_layer.north - 2.9).abs() < 1e-9);
        assert!((two_layer.south - 3.7).abs() < 1e-9);

        let four_layer = routing_headroom(RoutingChannels {
            clearance: 0.2,
            track_width: 0.25,
            via_diameter: 0.6,
            layer_count: 4,
            edge_net_counts: [0, 1, 5, 8],
        });
        assert!(four_layer.north < two_layer.north);
        assert!(four_layer.south < two_layer.south);
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

    /// The through-hole 555 of `examples/pcb_circuits/ne555-tht.json`, measured
    /// by [`placement_extent`]: a DIP-8, three axial resistors, three
    /// electrolytics and two headers — all pin-1-origin parts, so all much
    /// larger to the placer than their library courtyard box.
    fn tht_555_board() -> Vec<PartExtent> {
        let part = |w, h, edge_seeking| PartExtent { w, h, edge_seeking };
        vec![
            part(17.34, 18.28, false),
            part(22.42, 3.0, false),
            part(22.42, 3.0, false),
            part(22.42, 3.0, false),
            part(8.0, 5.5, false),
            part(12.1, 3.0, false),
            part(9.3, 6.8, false),
            part(3.54, 8.64, true),
            part(3.54, 13.7, true),
        ]
    }

    /// The calibration, pinned: the board this rule recommends for the 555 is
    /// one `place_tuned` can pack on the first call, and the library-courtyard
    /// measure it replaces recommends one that cannot be packed at all.
    #[test]
    fn the_through_hole_555_is_recommended_a_board_it_can_actually_pack() {
        let sizing = size_board(&tht_555_board(), 1.0, demand());
        assert_eq!((sizing.recommended_w, sizing.recommended_h), (41.0, 41.0));
        assert!(
            (sizing.courtyard_area_mm2 - 741.4).abs() < 0.1,
            "{sizing:?}"
        );
        assert!(places_legally(&tht_555_board(), 41.0, 41.0));
        // Measured with the raw library courtyards this board used to be sized
        // from: 363.9 mm², a 30 x 30 mm recommendation, and no legal packing.
        assert!(!places_legally(&tht_555_board(), 30.0, 30.0));
    }

    /// The placer keeps a courtyard centred on the part origin, so sizing must
    /// measure a pin-1-origin footprint by the box that mirrors it.
    #[test]
    fn a_pin_one_origin_footprint_is_measured_by_the_box_the_placer_reserves() {
        let source = r#"(footprint "Header"
          (version 20240108)
          (generator "test")
          (layer "F.Cu")
          (fp_line (start -1.5 -1.5) (end 4 -1.5)
            (stroke (width 0.05) (type solid)) (layer "F.CrtYd"))
          (fp_line (start -1.5 1.5) (end 4 1.5)
            (stroke (width 0.05) (type solid)) (layer "F.CrtYd"))
          (pad "1" thru_hole rect (at 0 0) (size 1.7 1.7) (layers "*.Cu" "*.Mask"))
          (pad "2" thru_hole oval (at 2.54 0) (size 1.7 1.7) (layers "*.Cu" "*.Mask")))"#;
        let footprint =
            kicad_footprint::Footprint::parse_str("Header", source).expect("parse fixture");
        assert_eq!(
            footprint.courtyard.max_x - footprint.courtyard.min_x,
            5.5,
            "the library courtyard is 5.5 mm wide"
        );
        let (w, h) = placement_extent(&footprint);
        assert_eq!((w, h), (8.0, 3.0), "mirrored about the origin at pin 1");
    }

    /// Every part fits inside the board it is offered, with the courtyard margin
    /// the placer enforces — the promise `recommended` is calibrated against.
    fn places_legally(parts: &[PartExtent], w: f64, h: f64) -> bool {
        let problem = pcb_place::PlacementView {
            bounds: geom::Rect::new(0.0, 0.0, w, h),
            clearance: 0.15,
            layer_count: 2,
            min_trace_width: 0.15,
            parts: parts
                .iter()
                .enumerate()
                .map(|(index, extent)| pcb_place::Part {
                    reference: format!("{}{index}", if extent.edge_seeking { "J" } else { "U" }),
                    courtyard_w: extent.w,
                    courtyard_h: extent.h,
                    pads: vec![],
                    edge_datum: None,
                    locked: None,
                })
                .collect(),
            keepouts: vec![],
            outline: None,
        };
        pcb_engine::place_tuned(&problem, &pcb_place::PlacementHints::default()).legal
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
