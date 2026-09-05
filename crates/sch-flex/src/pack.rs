//! The one block packer: where a box goes among the boxes already down.
//!
//! Both callers pack the same thing — a block's finished FRAME, the drawing plus the air
//! the realiser's dashed rectangle and the field text need — onto the same corner
//! lattice. The typesetter packs a page from nothing ([`corner_pack`]); a graft seats one
//! more block among the frames a sheet already carries ([`landings`]). They share the
//! candidate corners so that a sheet built one block at a time lands where the same
//! blocks would have landed had they been packed together.

use geom::{PAGE_MARGIN as MARGIN, Point2, Rect};
use sch_model::tree::UNIT_MM;

/// Air between two block frames the typesetter packs onto a page of its own.
pub const BLOCK_GAP: f64 = 6.0 * UNIT_MM;
/// Room each block keeps outside its parts for the dashed frame the realiser draws around
/// it and the field text the solver seats along its edge.
pub const FRAME_PAD: f64 = 4.0 * UNIT_MM;

/// A placed block's rect grown by half `gap` on every side, so two blocks that merely
/// respect the gap do not read as overlapping.
fn gapped(r: &Rect, gap: f64) -> Rect {
    let m = gap / 2.0 - geom::EPS;
    Rect::new(r.min_x - m, r.min_y - m, r.max_x + m, r.max_y + m)
}

/// Every free landing for a box of `size` among `placed`, lowest then left-most first.
///
/// The candidates are exactly the corners a bottom-left pack can use: beside and under
/// each box already down, and the same corners projected back onto the page margins, so a
/// block can slide up into the band a short neighbour leaves. `limit` is how far right of
/// the margin the box may reach; `gap` is the air kept between two frames.
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

/// How far apart two blocks are drawn: the gap between their boxes, along both axes.
/// Touching blocks are zero apart, and the wire or label pair that joins them is about
/// this long.
pub fn apart(a: &Rect, b: &Rect) -> f64 {
    let dx = (b.min_x - a.max_x).max(a.min_x - b.max_x).max(0.0);
    let dy = (b.min_y - a.max_y).max(a.min_y - b.max_y).max(0.0);
    dx + dy
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
