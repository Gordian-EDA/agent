//! The one block packer: where a box goes among the boxes already down.
//!
//! Both callers pack the same thing — a block's finished FRAME, the drawing plus the air
//! the realiser's dashed rectangle needs around it and the band its title is seated in —
//! onto the same corner lattice. The typesetter packs a page from nothing ([`corner_pack`]); a graft seats one
//! more block among the frames a sheet already carries ([`landings`]). They share the
//! candidate corners so that a sheet built one block at a time lands where the same
//! blocks would have landed had they been packed together.

use geom::{PAGE_MARGIN as MARGIN, Point2, Rect};
use sch_model::tree::UNIT_MM;

/// Air between two block frames the typesetter packs onto a page of its own.
pub const BLOCK_GAP: f64 = 6.0 * UNIT_MM;
/// Air a block keeps outside its drawing for the dashed frame the realiser draws around
/// it.
///
/// This is the ONE frame pad: the realiser inflates a block's ink by exactly this to draw
/// the border, so reserving anything else on a side is air nothing occupies — paid on
/// every block on every sheet.
pub const FRAME_PAD: f64 = 3.0 * UNIT_MM;

/// Extra room a block keeps ABOVE its frame, where the realiser seats the block's title:
/// a clear line, then the title itself.
///
/// It is reserved on that side alone. A title is the one thing a block draws outside its
/// border, so charging every side for it — which is what a single fatter pad does — buys
/// nothing but whitespace on the other three.
pub const CAPTION_BAND: f64 = 3.0 * UNIT_MM;

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
        // What is packed is the block's frame WITH its caption band on top; what is
        // returned is where the frame itself goes, the band's height below it.
        let (w, h) = sizes[i];
        let h = h + CAPTION_BAND;
        let at = landings(&placed, (w, h), limit, BLOCK_GAP)
            .first()
            .copied()
            .unwrap_or(Point2::new(MARGIN, used_h + MARGIN + BLOCK_GAP));
        placed.push(Rect::new(at.x, at.y, at.x + w, at.y + h));
        origins[i] = Point2::new(at.x, at.y + CAPTION_BAND);
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
        assert!(
            height <= 90.0 + 2.0 * CAPTION_BAND,
            "all three inside the tall block's own band, captions included: {height}"
        );
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
