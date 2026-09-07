//! The one block packer: where a box goes among the boxes already down.
//!
//! Both callers pack the same thing — a block's finished FRAME, the drawing plus the air
//! the realiser's dashed rectangle and the field text need — onto the same corner
//! lattice. The typesetter packs a page from nothing ([`corner_pack`]); a graft seats one
//! more block among the frames a sheet already carries ([`landings`]). They share the
//! candidate corners so that a sheet built one block at a time lands where the same
//! blocks would have landed had they been packed together.

use geom::{PAGE_MARGIN as MARGIN, Point2, Rect};
use sch_model::item::Item;
use sch_model::tree::UNIT_MM;

use crate::part::{Part, Pose};

/// Air between two block frames the typesetter packs onto a page of its own.
pub const BLOCK_GAP: f64 = 6.0 * UNIT_MM;
/// Air between a block's outermost ink and the dashed frame the realiser draws around it.
/// A hairline over the pad the realiser itself uses, so a frame drawn around the ink a
/// block claimed stays inside the claim.
pub const FRAME_PAD: f64 = 4.0 * UNIT_MM;

/// The sheet a block CLAIMS: everything its parts can draw at the poses they now hold —
/// bodies, the reference/value pair, and the net labels and rail glyphs hanging off their
/// pins — grown by [`FRAME_PAD`], which is the rect the realiser then outlines.
///
/// This is the ONE rect a block claims: a graft seats it among the frames a sheet already
/// carries, and the realiser draws its outline around the same ink. Measuring the seat on
/// bare bodies is what put a grafted frame straight through its neighbour's.
///
/// Room is kept for a label on EVERY connected pin, not just the one pin per net that
/// leaves the block ([`crate::label_pins`], which is what the block's INTERNAL spacing is
/// measured with). What the writer actually labels is decided by the router, downstream of
/// every placement: a net it cannot wire is drawn as a label on each of its pins, and a
/// claim that assumed otherwise came up 15 mm short of the frame the realiser then drew.
/// A claim is the one measure that has to be an upper bound.
///
/// The reference/value pair is measured the same way and for the same reason: which of a
/// body's four sides the text solver seats it against is settled after the sheet is drawn,
/// so the claim keeps room on all four. [`sch_model::geometry::field_pad`] reserves only
/// the band an IC stacks its pair in, which is the right answer for the spacing INSIDE a
/// block and 6 mm short of a passive's frame at the edge of one.
pub fn block_frame(items: &[Item], members: &[usize]) -> Option<Rect> {
    /// A clear line between a body and the reference/value pair seated beside it, and the
    /// band the same pair takes above or below one — the four sides `write::textsolve`
    /// chooses between.
    const FIELD_GAP: f64 = 1.27;
    const FIELD_BAND: f64 = 5.59;
    let fields = |item: &Item| {
        let text = sch_model::text::text_width(&item.value)
            .max(sch_model::text::text_width(&item.refdes));
        let r = sch_model::geometry::body_rect(item, item.at);
        Rect::new(
            r.min_x - FIELD_GAP - text,
            r.min_y - FIELD_BAND,
            r.max_x + FIELD_GAP + text,
            r.max_y + FIELD_BAND,
        )
    };
    let labelled = members
        .iter()
        .flat_map(|i| {
            items[*i]
                .pins
                .iter()
                .filter(|(_, _, net)| net.is_some())
                .map(move |(number, ..)| (*i, number.clone()))
        })
        .collect();
    members
        .iter()
        .map(|i| {
            let item = &items[*i];
            let pose = Pose {
                angle: item.angle,
                mirror: item.mirror,
            };
            let r = Part::new(*i, item, &labelled).extent(pose);
            let drawn = Rect::new(
                item.at.x + r.min_x,
                item.at.y + r.min_y,
                item.at.x + r.max_x,
                item.at.y + r.max_y,
            );
            let text = fields(item);
            Rect::new(
                drawn.min_x.min(text.min_x),
                drawn.min_y.min(text.min_y),
                drawn.max_x.max(text.max_x),
                drawn.max_y.max(text.max_y),
            )
        })
        .reduce(|a, b| {
            Rect::new(
                a.min_x.min(b.min_x),
                a.min_y.min(b.min_y),
                a.max_x.max(b.max_x),
                a.max_y.max(b.max_y),
            )
        })
        .map(|hull| hull.inflate(FRAME_PAD))
}

/// A placed block's rect grown by half `gap` on every side, so two blocks that merely
/// respect the gap do not read as overlapping.
fn gapped(r: &Rect, gap: f64) -> Rect {
    let m = gap / 2.0 - geom::EPS;
    Rect::new(r.min_x - m, r.min_y - m, r.max_x + m, r.max_y + m)
}

/// The free landings for a box of `size` among `placed`, lowest then left-most first.
///
/// The candidates are the corners a bottom-left pack can use: beside and under each box
/// already down, and the same corners projected back onto the page margins, so a block can
/// slide up into the band a short neighbour leaves. `limit` is how far right of the margin
/// the box may reach; `gap` is the air kept between two frames.
///
/// This is deliberately not every position a box could legally take: the hole bounded by
/// two DIFFERENT neighbours is named by no single box's corner. Offering those as well
/// (the full cross-product of the candidate x and y edges) was measured over the 24-fixture
/// block replay and left the sheets emptier, not fuller — the extra freedom is spent by
/// [`crate::pack`]'s callers, which score a landing on the sheet it makes now and cannot
/// see the blocks still to come.
pub fn landings(placed: &[Rect], size: (f64, f64), limit: f64, gap: f64) -> Vec<Point2> {
    let (w, h) = size;
    let mut corners = vec![Point2::new(MARGIN, MARGIN)];
    for r in placed {
        corners.push(Point2::new(r.max_x + gap, r.min_y));
        corners.push(Point2::new(r.min_x, r.max_y + gap));
        corners.push(Point2::new(r.max_x + gap, MARGIN));
        corners.push(Point2::new(MARGIN, r.max_y + gap));
    }
    corners.retain(|c| c.x >= MARGIN - geom::EPS && c.y >= MARGIN - geom::EPS);
    corners.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
    corners.dedup_by(|a, b| (a.x - b.x).abs() < geom::EPS && (a.y - b.y).abs() < geom::EPS);
    corners.retain(|c| {
        let r = Rect::new(c.x, c.y, c.x + w, c.y + h);
        // A run that exactly fills the limit must not be pushed off it by dust.
        r.max_x <= MARGIN + limit + geom::EPS
            && !placed.iter().any(|p| gapped(p, gap).overlaps(&r))
    });
    corners
}

/// Bottom-left packing of `sizes`, visited in `order`, into a box `limit` wide and
/// unbounded in height: each block takes the lowest, then left-most corner that clears the
/// blocks already down. Row-major shelves leave a dead band under every short block; a
/// corner pack lets the next block slide up into it.
///
/// Returns the origins in the ORIGINAL index order, plus the finished sheet's extent.
pub fn corner_pack(sizes: &[(f64, f64)], order: &[usize], limit: f64) -> (Vec<Point2>, f64, f64) {
    let mut placed: Vec<Rect> = Vec::with_capacity(order.len());
    let mut origins = vec![Point2::new(MARGIN, MARGIN); sizes.len()];
    let (mut used_w, mut used_h) = (0.0f64, 0.0f64);
    for &i in order {
        let (w, h) = sizes[i];
        let at = landings(&placed, (w, h), limit, BLOCK_GAP)
            .first()
            .copied()
            .unwrap_or(Point2::new(MARGIN, used_h + MARGIN + BLOCK_GAP));
        placed.push(Rect::new(at.x, at.y, at.x + w, at.y + h));
        origins[i] = at;
        used_w = used_w.max(at.x + w - MARGIN);
        used_h = used_h.max(at.y + h - MARGIN);
    }
    (origins, used_w, used_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vertical `Device:R` at the origin, with `net` on its top pin.
    fn resistor(refdes: &str, net: &str) -> Item {
        let pin = |number: &str, y: f64, angle: f64| kicad_symbol::geometry::PinGeom {
            number: number.into(),
            name: "~".into(),
            at: Point2::new(0.0, y),
            angle,
            length: 2.54,
            unit: 1,
            text: Default::default(),
        };
        Item {
            refdes: refdes.into(),
            block: "filter".into(),
            part: "Device:R".into(),
            value: "10k".into(),
            footprint: None,
            geom: kicad_symbol::geometry::SymbolGeometry {
                lib_id: "Device:R".into(),
                pins: vec![pin("1", 3.81, 270.0), pin("2", -3.81, 90.0)],
                raw_definition: String::new(),
            },
            pins: vec![("1".into(), String::new(), Some(net.into()))],
            at: Point2::new(100.0, 100.0),
            angle: 0.0,
            unit: 1,
            mirror: false,
            preseeded: false,
            open: false,
            supports: None,
        }
    }

    /// The claim covers everything the sheet will draw for the block, whatever the router
    /// and the text solver then decide: the label on a pin — on EVERY pin, since which
    /// ones get one is settled downstream — and the reference/value pair on whichever
    /// side of the body it lands.
    #[test]
    fn a_claim_covers_the_text_the_block_has_yet_to_draw() {
        let items = [resistor("R1", "VERY_LONG_NET_NAME")];
        let claim = block_frame(&items, &[0]).expect("a block with a part has a frame");
        let body = sch_model::geometry::body_rect(&items[0], items[0].at);

        let label = sch_model::text::text_width("VERY_LONG_NET_NAME");
        assert!(
            claim.min_y <= body.min_y - label,
            "the label off pin 1 reaches out of the claim: {claim:?}"
        );
        let field = sch_model::text::text_width("10k").max(sch_model::text::text_width("R1"));
        assert!(
            claim.min_x <= body.min_x - field && claim.max_x >= body.max_x + field,
            "the field pair reaches out of the claim: {claim:?}"
        );
    }

    /// A third block tucks into the band a short second block leaves beside a tall first,
    /// instead of starting a row below both — the dead page a shelf pack cannot use.
    #[test]
    fn a_short_block_does_not_strand_the_page_under_it() {
        let blocks = [(140.0, 90.0), (100.0, 30.0), (90.0, 50.0)];
        let (origins, _, height) = corner_pack(&blocks, &[0, 1, 2], 247.62);
        assert_eq!(origins[1].y, origins[0].y, "the short block sits beside the tall one");
        assert_eq!(origins[2].x, origins[1].x, "and the third tucks under it");
        assert!(origins[2].y > origins[1].y);
        assert_eq!(height, 90.0, "all three inside the tall block's own band");
    }

    /// A block seated among frames already down can fill the hole a short one leaves:
    /// that landing is offered, and it is the one that grows the sheet by nothing.
    #[test]
    fn a_landing_fills_the_hole_a_short_block_leaves() {
        let tall = Rect::new(MARGIN, MARGIN, MARGIN + 60.0, MARGIN + 100.0);
        let short = Rect::new(tall.max_x + 12.7, MARGIN, tall.max_x + 72.7, MARGIN + 30.0);
        let taken = [tall, short];
        let sheet = |at: &Point2| {
            let r = Rect::new(at.x, at.y, at.x + 50.0, at.y + 40.0);
            let hull = taken.iter().fold(r, |a, b| {
                Rect::new(a.min_x.min(b.min_x), a.min_y.min(b.min_y), a.max_x.max(b.max_x), a.max_y.max(b.max_y))
            });
            hull.width() * hull.height()
        };
        let best = landings(&taken, (50.0, 40.0), 300.0, 12.7)
            .into_iter()
            .min_by(|a, b| sheet(a).total_cmp(&sheet(b)))
            .expect("a landing");
        assert!(best.x >= short.min_x && best.y > short.max_y, "{best:?} is not the hole");
    }
}
