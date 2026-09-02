//! Design rules the footprints on a board can actually satisfy.
//!
//! A board seeded with a clearance wider than the gap between two pads of a part
//! it carries is born failing DRC: no placement and no router can widen a
//! footprint's own pitch, so every route attempt refuses and the caller loops.
//! [`pad_limited_rules`] reads the pads that will be on the board and lowers the
//! seed rules to what those pads permit, never below the fabrication floor.

use kicad_footprint::{Footprint, FootprintPad};
use pcb_model::Point2;

/// Fabrication floor: no derivation may take a rule under this, whatever the
/// footprint's pitch. A part tighter than this needs a different process, not a
/// looser board.
pub(crate) const FAB_MIN_CLEARANCE_MM: f64 = 0.1;
/// Fabrication floor for track width, for the same reason.
pub(crate) const FAB_MIN_TRACE_WIDTH_MM: f64 = 0.1;

/// Keep a hair of margin under the measured pad gap so a rule sitting exactly on
/// the geometry does not fail on floating-point noise in KiCAD's own DRC.
const GAP_MARGIN: f64 = 0.9;

/// The tightest geometry any one footprint contributes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PadLimits {
    /// Smallest edge-to-edge gap between two differently-numbered pads sharing a
    /// copper layer, mm.
    pub min_pad_gap: Option<f64>,
    /// Smallest pad short side, mm.
    pub min_pad_width: Option<f64>,
    /// The footprint and pad pair that set `min_pad_gap`, e.g.
    /// `("Package_TO_SOT_SMD:SOT-23-5", "3", "4")`.
    pub tightest: Option<(String, String, String)>,
}

impl PadLimits {
    fn none() -> Self {
        PadLimits {
            min_pad_gap: None,
            min_pad_width: None,
            tightest: None,
        }
    }

    fn merge(mut self, other: PadLimits) -> Self {
        if let Some(gap) = other.min_pad_gap
            && self.min_pad_gap.is_none_or(|current| gap < current)
        {
            self.min_pad_gap = Some(gap);
            self.tightest = other.tightest;
        }
        self.min_pad_width = match (self.min_pad_width, other.min_pad_width) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self
    }
}

/// A pad's axis-aligned copper extent, honouring 90°-family rotations.
fn pad_half_extent(pad: &FootprintPad) -> Point2 {
    let quarter_turn = (pad.rotation.rem_euclid(180.0) - 90.0).abs() < 1.0;
    let (w, h) = if quarter_turn {
        (pad.size.y, pad.size.x)
    } else {
        (pad.size.x, pad.size.y)
    };
    Point2 {
        x: w / 2.0,
        y: h / 2.0,
    }
}

fn shares_copper_layer(a: &FootprintPad, b: &FootprintPad) -> bool {
    let matches = |x: &str, y: &str| x == y || x == "*.Cu" || y == "*.Cu";
    a.copper_layers()
        .iter()
        .any(|la| b.copper_layers().iter().any(|lb| matches(la, lb)))
}

/// Edge-to-edge gap between two axis-aligned pads (0 when they overlap).
fn pad_gap(a: &FootprintPad, b: &FootprintPad) -> f64 {
    let ha = pad_half_extent(a);
    let hb = pad_half_extent(b);
    let dx = (a.at.x - b.at.x).abs() - (ha.x + hb.x);
    let dy = (a.at.y - b.at.y).abs() - (ha.y + hb.y);
    dx.max(dy).max(0.0)
}

/// The tightest pad geometry in one footprint's source text.
///
/// Pads sharing a number are the same electrical node (a split thermal pad, a
/// multi-pad connector shield), so they never constrain clearance.
pub(crate) fn footprint_limits(lib_id: &str, source: &str) -> PadLimits {
    let Ok(footprint) = Footprint::parse_str(lib_id, source) else {
        return PadLimits::none();
    };
    let pads: Vec<&FootprintPad> = footprint
        .pads
        .iter()
        .filter(|pad| !pad.copper_layers().is_empty())
        .collect();
    let mut limits = PadLimits::none();
    for pad in &pads {
        let half = pad_half_extent(pad);
        let width = (half.x * 2.0).min(half.y * 2.0);
        if width > 0.0 {
            limits.min_pad_width = Some(limits.min_pad_width.map_or(width, |m| m.min(width)));
        }
    }
    for (i, a) in pads.iter().enumerate() {
        for b in &pads[i + 1..] {
            if a.number == b.number || !shares_copper_layer(a, b) {
                continue;
            }
            let gap = pad_gap(a, b);
            if limits.min_pad_gap.is_none_or(|current| gap < current) {
                limits.min_pad_gap = Some(gap);
                limits.tightest = Some((lib_id.to_owned(), a.number.clone(), b.number.clone()));
            }
        }
    }
    limits
}

/// The tightest geometry across every footprint the board will carry.
pub(crate) fn board_limits<'a>(
    footprints: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> PadLimits {
    footprints
        .into_iter()
        .map(|(lib_id, source)| footprint_limits(lib_id, source))
        .fold(PadLimits::none(), PadLimits::merge)
}

/// Round a derived rule down to a 0.01 mm grid so it reads as a design rule
/// rather than a measurement.
fn to_rule_grid(v: f64) -> f64 {
    (v * 100.0).floor() / 100.0
}

/// Lower `clearance` and `min_trace_width` to what `limits` permits, floored at
/// the fabrication minimums. Returns the adjusted pair plus one sentence per
/// change, for the caller to publish.
pub(crate) fn pad_limited_rules(
    clearance: f64,
    min_trace_width: f64,
    limits: &PadLimits,
) -> (f64, f64, Vec<String>) {
    let mut notes = Vec::new();
    let mut clearance_out = clearance;
    let mut width_out = min_trace_width;

    if let Some(gap) = limits.min_pad_gap {
        let cap = to_rule_grid(gap * GAP_MARGIN).max(FAB_MIN_CLEARANCE_MM);
        if cap < clearance_out {
            let where_at = limits.tightest.as_ref().map_or_else(
                || "a footprint on this board".to_owned(),
                |(lib_id, a, b)| format!("{lib_id} pads {a}/{b}"),
            );
            notes.push(format!(
                "clearance {clearance:.2} → {cap:.2} mm: {where_at} are only {gap:.3} mm apart, so \
                 the requested clearance could never pass DRC on this board"
            ));
            clearance_out = cap;
        }
    }
    if let Some(pad_width) = limits.min_pad_width {
        let cap = to_rule_grid(pad_width).max(FAB_MIN_TRACE_WIDTH_MM);
        if cap < width_out {
            notes.push(format!(
                "min_trace_width {min_trace_width:.2} → {cap:.2} mm: the narrowest pad on this \
                 board is {pad_width:.3} mm, and a track may not be wider than the pad it lands on"
            ));
            width_out = cap;
        }
    }
    (clearance_out, width_out, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two 0.6 x 1.1 mm pads on a 0.95 mm pitch: a SOT-23 row. Gap 0.35 mm.
    const SOT23_ROW: &str = r#"(footprint "SOT-23"
      (pad "1" smd roundrect (at -0.95 0.95) (size 0.6 1.1) (layers "F.Cu" "F.Paste" "F.Mask"))
      (pad "2" smd roundrect (at 0 0.95) (size 0.6 1.1) (layers "F.Cu" "F.Paste" "F.Mask"))
      (pad "3" smd roundrect (at 0.95 0.95) (size 0.6 1.1) (layers "F.Cu" "F.Paste" "F.Mask"))
    )"#;

    /// A 0.5 mm-pitch USB-C-like row: 0.3 mm pads, 0.2 mm gaps.
    const FINE_PITCH: &str = r#"(footprint "USB_C"
      (pad "A1" smd rect (at 0 0) (size 0.3 1.2) (layers "F.Cu" "F.Paste" "F.Mask"))
      (pad "A2" smd rect (at 0.5 0) (size 0.3 1.2) (layers "F.Cu" "F.Paste" "F.Mask"))
      (pad "A3" smd rect (at 1.0 0) (size 0.3 1.2) (layers "F.Cu" "F.Paste" "F.Mask"))
    )"#;

    /// Two halves of one thermal pad share a number: not a clearance pair.
    const SPLIT_THERMAL: &str = r#"(footprint "DFN"
      (pad "9" smd rect (at 0 0) (size 1.0 1.0) (layers "F.Cu" "F.Paste" "F.Mask"))
      (pad "9" smd rect (at 1.05 0) (size 1.0 1.0) (layers "F.Cu" "F.Paste" "F.Mask"))
    )"#;

    fn approx(a: Option<f64>, expected: f64) {
        let got = a.expect("a measured limit");
        assert!((got - expected).abs() < 1e-6, "{got} != {expected}");
    }

    #[test]
    fn pad_gap_is_measured_edge_to_edge() {
        let limits = footprint_limits("Package_TO_SOT_SMD:SOT-23", SOT23_ROW);
        approx(limits.min_pad_gap, 0.35);
        approx(limits.min_pad_width, 0.6);
        assert_eq!(
            limits.tightest,
            Some((
                "Package_TO_SOT_SMD:SOT-23".to_owned(),
                "1".to_owned(),
                "2".to_owned()
            ))
        );
    }

    #[test]
    fn pads_sharing_a_number_are_one_node_and_never_constrain_clearance() {
        let limits = footprint_limits("Package_DFN_QFN:DFN", SPLIT_THERMAL);
        assert_eq!(limits.min_pad_gap, None);
        approx(limits.min_pad_width, 1.0);
    }

    #[test]
    fn a_fine_pitch_part_lowers_the_seed_clearance_below_the_default() {
        let limits = board_limits([("Connector_USB:USB_C", FINE_PITCH)]);
        approx(limits.min_pad_gap, 0.2);
        let (clearance, width, notes) = pad_limited_rules(0.15, 0.15, &limits);
        assert_eq!(clearance, 0.15, "0.2 mm of pad gap still admits 0.15");
        assert_eq!(width, 0.15);
        assert!(notes.is_empty(), "{notes:?}");

        let (clearance, _, notes) = pad_limited_rules(0.25, 0.15, &limits);
        assert_eq!(clearance, 0.18);
        assert!(
            notes[0].contains("Connector_USB:USB_C pads A1/A2"),
            "{notes:?}"
        );
    }

    #[test]
    fn the_fabrication_floor_is_never_crossed() {
        let overlapping = PadLimits {
            min_pad_gap: Some(0.0),
            min_pad_width: Some(0.01),
            tightest: None,
        };
        let (clearance, width, notes) = pad_limited_rules(0.2, 0.2, &overlapping);
        assert_eq!(clearance, FAB_MIN_CLEARANCE_MM);
        assert_eq!(width, FAB_MIN_TRACE_WIDTH_MM);
        assert_eq!(notes.len(), 2);
    }

    #[test]
    fn the_tightest_footprint_on_the_board_wins() {
        let limits = board_limits([
            ("Package_TO_SOT_SMD:SOT-23", SOT23_ROW),
            ("Connector_USB:USB_C", FINE_PITCH),
        ]);
        approx(limits.min_pad_gap, 0.2);
        approx(limits.min_pad_width, 0.3);
        assert_eq!(limits.tightest.unwrap().0, "Connector_USB:USB_C");
    }
}
