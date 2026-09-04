//! Page fitting: where the drawing sits on the sheet and how big the sheet is.
//!
//! A schematic is only readable if all of it is inside the frame KiCAD draws.
//! Nothing upstream guarantees that: a block placed BESIDE existing content
//! carries whatever coordinates the region search chose, including negative
//! ones, and every renderer clips those away silently. So the document — the one
//! place that sees the whole sheet — owns the answer: [`SchDoc::refit_page`]
//! measures what is drawn, shifts it rigidly to the margin, and picks the
//! smallest standard page that holds it.
//!
//! What may move is the caller's to say. A graft promises the sheet's own parts stay
//! where they are, so the fit is told which items are frozen and slides only the rest —
//! and a slide that is not whole-sheet is not rigid, so it is offered only when the new
//! drawing does not touch the frozen one.

use std::collections::BTreeSet;

use geom::{GRID_50_MIL, PAGE_MARGIN, Point2, Rect};

use crate::body::body_rect;
use crate::doc::SchDoc;
use crate::model::{Item, Pose};
use crate::sexpr::{num, quoted, tagged};

/// Bottom band a KiCAD title block occupies inside the frame, in mm. Content
/// that reaches into it is overprinted by the sheet metadata.
pub const TITLE_BLOCK_BAND: f64 = 33.0;

/// Rendered width of a text run, in mm — the same 1.1 mm/character estimate the
/// realiser's text solver uses, scaled by the font size KiCAD defaults to.
fn text_width(s: &str, size: f64) -> f64 {
    s.chars().count() as f64 * 1.1 * (size / 1.27)
}

/// The standard landscape pages a generated sheet may use, smallest first.
///
/// The ladder starts at A5 because the page is what the reader sees: a four-part circuit
/// on A4 is a drawing stranded in one corner of an empty sheet, which the visual critic
/// scored a point WORSE than the snug custom page it replaced. Standard sizes are what
/// humans draw on (a `User` page is 3% of the reference corpus); starting small is what
/// keeps the ink on the paper.
pub const STANDARD_PAGES: [(&str, [f64; 2]); 4] = [
    ("A5", [210.0, 148.0]),
    ("A4", [297.0, 210.0]),
    ("A3", [420.0, 297.0]),
    ("A2", [594.0, 420.0]),
];

/// The smallest standard page holding content of `size` mm (margins already included),
/// or `None` when nothing standard does.
pub fn standard_page(size: [f64; 2]) -> Option<(&'static str, [f64; 2])> {
    STANDARD_PAGES
        .into_iter()
        .find(|(_, page)| page[0] >= size[0] && page[1] >= size[1])
}

/// What [`SchDoc::refit_page`] did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageFit {
    /// The grid-snapped shift applied to the items that were free to move. `[0, 0]`
    /// means the drawing already started inside the frame, or that the only shift that
    /// would have rescued it would have torn it away from the frozen content.
    pub shift: [f64; 2],
    /// The page the sheet now declares, in mm.
    pub page: [f64; 2],
    /// Whether the page is one of [`STANDARD_PAGES`]. False means the content is
    /// larger than A2 and the sheet keeps a `User` page sized to fit it — visible
    /// content beats a standard name.
    pub standard: bool,
}

/// Every `(at x y …)` and `(xy x y)` under `node` — the sheet-space geometry of an item
/// the typed model does not decode (a hierarchical sheet, a bus, a rule area). Never
/// called on `(lib_symbols)`, whose coordinates are symbol-local, not sheet-space.
fn raw_points(node: &kiutils_sexpr::Node, out: &mut Vec<Point2>) {
    let v = crate::sexpr::items(node);
    if matches!(crate::sexpr::head(node), Some("at" | "xy")) {
        if let (Some(x), Some(y)) = (
            v.get(1).and_then(crate::sexpr::number),
            v.get(2).and_then(crate::sexpr::number),
        ) {
            out.push(Point2::new(x, y));
        }
        return;
    }
    for child in v {
        raw_points(child, out);
    }
}

/// Shift every point [`raw_points`] would report, in place.
fn shift_raw_points(node: &mut kiutils_sexpr::Node, dx: f64, dy: f64) {
    let point = matches!(crate::sexpr::head(node), Some("at" | "xy"));
    let Some(children) = crate::sexpr::items_mut(node) else {
        return;
    };
    if point {
        for (i, d) in [(1, dx), (2, dy)] {
            if let Some(v) = children.get(i).and_then(crate::sexpr::number) {
                children[i] = num(v + d);
            }
        }
        return;
    }
    for child in children {
        shift_raw_points(child, dx, dy);
    }
}

/// The corner points of one item as drawn, appended to `out`.
///
/// Symbol bodies come from the embedded definition's graphics ([`crate::body::body_rect`]),
/// so this is the shape KiCAD renders, not a pin-span approximation; visible field text,
/// label text and annotation text are boxed by their estimated width in both directions,
/// which covers every justification without decoding one.
fn item_points(doc: &SchDoc, item: &Item, points: &mut Vec<Point2>) {
    // A label drawn at 90 or 270 degrees reaches its WIDTH up and down the sheet and only
    // its height across it. Measuring every text as if it ran horizontally understates a
    // vertical net label by the length of its name, which is how a sheet that fits gets
    // shifted until that label prints over the frame rule.
    let text_at = |pose: Pose, s: &str, size: f64, points: &mut Vec<Point2>| {
        let w = text_width(s, size);
        let (across, along) = if pose.rot.rem_euclid(180.0) == 90.0 {
            (size, w)
        } else {
            (w, size)
        };
        points.push(Point2::new(pose.x - across, pose.y - along));
        points.push(Point2::new(pose.x + across, pose.y + along));
    };
    match item {
        Item::Symbol(inst) => {
            if let Some(r) = body_rect(doc, inst) {
                points.push(Point2::new(r.min_x, r.min_y));
                points.push(Point2::new(r.max_x, r.max_y));
            } else {
                points.push(inst.at.point());
            }
            for field in inst.fields.values() {
                if field.hidden || field.value.is_empty() {
                    continue;
                }
                if let Some(at) = field.at {
                    text_at(at, &field.value, field.font_size[0].max(1.27), points);
                }
            }
        }
        Item::Wire(w) => points.extend(w.points.iter().copied()),
        Item::Junction(j) => points.push(j.at),
        Item::NoConnect(n) => points.push(n.at),
        Item::Label(l) => text_at(l.at, &l.text, 1.27, points),
        Item::Text(t) => text_at(t.at, &t.text, 1.27, points),
        Item::Rectangle(r) => points.extend([r.start, r.end]),
        Item::Sheet(s) => {
            points.push(s.at);
            points.push(Point2::new(s.at.x + s.size.x, s.at.y + s.size.y));
        }
        // Buses, images, rule areas — whatever the typed model does not decode
        // still occupies the sheet and still carries connectivity.
        Item::Other(raw) => raw_points(&raw.node, points),
        Item::LibSymbols(_) => {}
    }
}

/// Where a set of items connects: the points that join nets, and the wire segments other
/// points can attach along.
#[derive(Default)]
struct Wiring {
    points: Vec<Point2>,
    segments: Vec<[Point2; 2]>,
}

impl Wiring {
    fn is_empty(&self) -> bool {
        self.points.is_empty() && self.segments.is_empty()
    }

    fn shifted(&self, [dx, dy]: [f64; 2]) -> Wiring {
        let move_point = |p: &Point2| Point2::new(p.x + dx, p.y + dy);
        Wiring {
            points: self.points.iter().map(move_point).collect(),
            segments: self
                .segments
                .iter()
                .map(|[a, b]| [move_point(a), move_point(b)])
                .collect(),
        }
    }

    /// Whether any connection point of one drawing lies on a point or a wire of the other.
    fn touches(&self, other: &Wiring) -> bool {
        let on_wire =
            |p: &Point2, w: &Wiring| w.segments.iter().any(|[a, b]| on_segment(*p, *a, *b));
        self.points
            .iter()
            .any(|p| other.points.iter().any(|q| p.near_eq(*q, TOUCH_EPS)) || on_wire(p, other))
            || other.points.iter().any(|p| on_wire(p, self))
    }
}

/// How close two connection points must be to count as one, in mm — the 1 µm the
/// connectivity extractor quantises to.
const TOUCH_EPS: f64 = 0.001;

/// Whether `p` lies on the segment `a`-`b`, endpoints included.
fn on_segment(p: Point2, a: Point2, b: Point2) -> bool {
    let (ab, ap) = ((b.x - a.x, b.y - a.y), (p.x - a.x, p.y - a.y));
    let len = (ab.0 * ab.0 + ab.1 * ab.1).sqrt();
    if len < TOUCH_EPS {
        return p.near_eq(a, TOUCH_EPS);
    }
    let along = (ap.0 * ab.0 + ap.1 * ab.1) / len;
    let across = (ap.0 * ab.1 - ap.1 * ab.0) / len;
    across.abs() <= TOUCH_EPS && along >= -TOUCH_EPS && along <= len + TOUCH_EPS
}

impl SchDoc {
    /// The bounding box of everything drawn on the sheet, in mm. `None` for an
    /// empty sheet.
    pub fn content_bbox(&self) -> Option<Rect> {
        self.bbox_where(|_| true)
    }

    /// The bounding box of the items `keep` accepts, in mm. `None` when it accepts
    /// nothing drawn.
    fn bbox_where(&self, mut keep: impl FnMut(&Item) -> bool) -> Option<Rect> {
        let mut points: Vec<Point2> = Vec::new();
        for item in self.items().iter().filter(|item| keep(item)) {
            item_points(self, item, &mut points);
        }
        Rect::bounding(&points)
    }

    /// Shift every drawn item by `(dx, dy)` mm.
    ///
    /// Rigid, so no two points that coincided stop coinciding: the netlist is
    /// invariant under it. Symbol field positions are absolute in KiCAD and move
    /// with their symbol.
    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.translate_where(dx, dy, |_| true);
    }

    /// Shift the items `keep` accepts by `(dx, dy)` mm, leaving the rest where they are.
    ///
    /// Rigid only within the moved set: a point of a moved item that coincided with a
    /// point of a kept one stops coinciding, so a partial shift CAN change the netlist
    /// and the caller has to establish that it does not.
    fn translate_where(&mut self, dx: f64, dy: f64, mut keep: impl FnMut(&Item) -> bool) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let shift = |p: &mut Point2| {
            p.x += dx;
            p.y += dy;
        };
        let shift_pose = |p: &mut Pose| {
            p.x += dx;
            p.y += dy;
        };
        for item in self.items_mut().iter_mut().filter(|item| keep(item)) {
            match item {
                Item::Symbol(inst) => {
                    shift_pose(&mut inst.at);
                    for field in inst.fields.values_mut() {
                        if let Some(at) = field.at.as_mut() {
                            shift_pose(at);
                        }
                    }
                    inst.raw.touch();
                }
                Item::Wire(w) => {
                    w.points.iter_mut().for_each(shift);
                    w.raw.touch();
                }
                Item::Junction(j) => {
                    shift(&mut j.at);
                    j.raw.touch();
                }
                Item::NoConnect(n) => {
                    shift(&mut n.at);
                    n.raw.touch();
                }
                Item::Label(l) => {
                    shift_pose(&mut l.at);
                    l.raw.touch();
                }
                Item::Text(t) => {
                    shift_pose(&mut t.at);
                    t.raw.touch();
                }
                Item::Rectangle(r) => {
                    shift(&mut r.start);
                    shift(&mut r.end);
                    r.raw.touch();
                }
                Item::Sheet(s) => {
                    shift(&mut s.at);
                    for pin in &mut s.pins {
                        shift_pose(&mut pin.at);
                    }
                    // A hierarchical sheet re-emits its parsed node verbatim — the typed
                    // fields are read-only — so the node itself is what has to move. Its
                    // border pins are connection points; leaving them behind while the
                    // rest of the drawing moves would tear the netlist apart.
                    shift_raw_points(&mut s.raw.node, dx, dy);
                    s.raw.touch();
                }
                Item::Other(raw) => {
                    let before = raw.node.clone();
                    shift_raw_points(&mut raw.node, dx, dy);
                    if raw.node != before {
                        raw.touch();
                    }
                }
                Item::LibSymbols(_) => {}
            }
        }
        self.mark_edited();
    }

    /// Bring the drawing inside the frame and size the page to it.
    ///
    /// Content that starts before [`PAGE_MARGIN`] — which is content the drawing frame
    /// clips away, invisibly — is pushed back to it, and the page becomes the smallest of
    /// A5/A4/A3/A2 landscape that holds the result: the sizes humans draw on. Only content
    /// larger than A2 keeps a `User` page, because an invisible drawing is worse than an
    /// unconventional page. A sheet carrying a title block also reserves the band it
    /// prints in, so metadata never overprints the lowest parts.
    ///
    /// `frozen` names, by UUID, the items that were already on the sheet before whatever
    /// write this fit follows — a graft's promise that it placed its block BESIDE the
    /// existing parts and did not touch them. Those never move: only the rest slides, and
    /// only far enough to reach the margin, so an existing symbol still writes back from
    /// its own bytes. An empty `frozen` means the whole sheet may slide, which is what a
    /// sheet drawn from scratch and a whole-sheet re-arrange both want.
    ///
    /// A partial slide is not rigid, so it is offered only when the movable drawing does
    /// not touch the frozen one — before or after the shift. A re-wire of seated symbols
    /// draws straight to their pins and so is never slid at all.
    ///
    /// While anything is frozen the shift is one way and only as far as the margin: a
    /// drawing that already starts inside the frame is not moved at all, so an untouched
    /// item still writes back from its own bytes — the crate's round-trip guarantee — and a
    /// caller that read a coordinate a moment ago still finds the part there. With nothing
    /// frozen the whole sheet is the fit's to place, and it is finally CENTRED on the page
    /// it just chose, so the slack sits as a margin rather than as an empty lower-right.
    ///
    /// The shift is snapped to the 50 mil grid, so grid-aligned geometry stays
    /// grid-aligned (KiCAD's ERC rejects off-grid endpoints). `None` for an empty
    /// sheet, which keeps whatever page it declares.
    pub fn refit_page(&mut self, frozen: &BTreeSet<String>) -> Option<PageFit> {
        // An item with no UUID cannot be told apart from one the caller froze, so it is
        // treated as frozen: the denylist fails closed.
        let movable = |item: &Item| item.uuid().is_some_and(|u| !frozen.contains(u));
        let mut shift = [0.0, 0.0];
        if let Some(bbox) = self.bbox_where(movable) {
            let want = [
                GRID_50_MIL.snap((PAGE_MARGIN - bbox.min_x).max(0.0)),
                GRID_50_MIL.snap((PAGE_MARGIN - bbox.min_y).max(0.0)),
            ];
            if self.slide_is_safe(want, movable) {
                shift = want;
                self.translate_where(shift[0], shift[1], movable);
            }
        }

        let bbox = self.content_bbox()?;
        // Always: KiCAD's drawing sheet paints the title block whether or not the
        // document carries a `(title_block …)` node — the node only fills in its text —
        // so content that reaches into the band is overprinted either way. This is what
        // `usable_pages` has always assumed.
        let band = TITLE_BLOCK_BAND;
        let need = [bbox.max_x + PAGE_MARGIN, bbox.max_y + PAGE_MARGIN + band];
        let (page, standard) = match standard_page(need) {
            Some((name, size)) => {
                self.set_paper(tagged("paper", vec![quoted(name)]));
                (size, true)
            }
            None => {
                self.set_paper(tagged(
                    "paper",
                    vec![quoted("User"), num(need[0]), num(need[1])],
                ));
                (need, false)
            }
        };
        // With the page settled and nothing frozen, share the slack out instead of leaving
        // it all along the bottom and right: a drawing pinned to the margin reads as a
        // circuit stranded in the corner of a sheet too big for it, which is the defect the
        // visual critic names on every under-filled page. The page is already chosen, so
        // centring cannot buy a bigger one.
        if frozen.is_empty() && standard && let Some(bbox) = self.content_bbox() {
            let middle = |lo: f64, hi: f64| GRID_50_MIL.snap((hi - lo) / 2.0).clamp(-lo, hi);
            let centre = [
                middle(bbox.min_x - PAGE_MARGIN, page[0] - PAGE_MARGIN - bbox.max_x),
                middle(bbox.min_y - PAGE_MARGIN, page[1] - PAGE_MARGIN - band - bbox.max_y),
            ];
            if centre != [0.0, 0.0] {
                self.translate_where(centre[0], centre[1], |_| true);
                shift = [shift[0] + centre[0], shift[1] + centre[1]];
            }
        }

        Some(PageFit {
            shift,
            page,
            standard,
        })
    }

    /// Whether moving the items `keep` accepts by `shift` leaves every connection alone.
    ///
    /// It does when the two drawings do not touch — no connection point of one sitting on
    /// a point or a wire of the other — neither where they are now nor where the shift
    /// would put them. Touching is the only way a partial move can join or break a net;
    /// two wires that merely cross are not connected in KiCAD, only a junction connects
    /// them, and a junction is a point.
    fn slide_is_safe(&self, shift: [f64; 2], keep: impl Fn(&Item) -> bool) -> bool {
        if shift == [0.0, 0.0] {
            return true;
        }
        let frozen = self.connection_geometry(|item| !keep(item));
        if frozen.is_empty() {
            return true;
        }
        let moving = self.connection_geometry(keep);
        !moving.touches(&frozen) && !moving.shifted(shift).touches(&frozen)
    }

    /// The connection geometry of the items `keep` accepts.
    fn connection_geometry(&self, mut keep: impl FnMut(&Item) -> bool) -> Wiring {
        let mut w = Wiring::default();
        for item in self.items().iter().filter(|item| keep(item)) {
            match item {
                Item::Symbol(inst) => w
                    .points
                    .extend(crate::pins::pins_of(self, inst).iter().map(|pin| pin.at)),
                Item::Wire(wire) => {
                    w.points.extend(wire.points.iter().copied());
                    w.segments
                        .extend(wire.points.windows(2).map(|p| [p[0], p[1]]));
                }
                Item::Junction(j) => w.points.push(j.at),
                Item::NoConnect(n) => w.points.push(n.at),
                Item::Label(l) => w.points.push(l.at.point()),
                Item::Sheet(s) => w.points.extend(s.pins.iter().map(|p| p.at.point())),
                // Buses and whatever else the typed model does not decode still carry
                // connectivity, and every coordinate they name is a place they carry it.
                Item::Other(raw) => raw_points(&raw.node, &mut w.points),
                Item::Text(_) | Item::Rectangle(_) | Item::LibSymbols(_) => {}
            }
        }
        w
    }

    pub(crate) fn has_title_block(&self) -> bool {
        self.items().iter().any(|item| {
            matches!(item, Item::Other(raw) if crate::sexpr::head(&raw.node) == Some("title_block"))
        })
    }

    /// Replace the `(paper …)` node, adding one when the sheet has none.
    fn set_paper(&mut self, paper: kiutils_sexpr::Node) {
        let replaced = self.items_mut().iter_mut().any(|item| match item {
            Item::Other(raw) if crate::sexpr::head(&raw.node) == Some("paper") => {
                raw.node = paper.clone();
                raw.touch();
                true
            }
            _ => false,
        });
        if !replaced {
            self.insert_item(Item::Other(Box::new(crate::model::Retained::owned(paper))));
        }
        self.mark_edited();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sheet whose content sits at negative coordinates — what a graft leaves behind.
    fn off_page_sheet() -> SchDoc {
        SchDoc::parse(
            "(kicad_sch\n\
             \t(version 20250114)\n\
             \t(paper \"User\" 657.78 411.16)\n\
             \t(wire (pts (xy -76.2 -15.24) (xy -76.2 20.32)) (uuid \"w1\"))\n\
             \t(label \"VBUS\" (at -76.2 -15.24 0) (uuid \"l1\"))\n\
             )\n",
        )
        .expect("parse")
    }

    #[test]
    fn refit_brings_content_inside_the_frame_and_picks_a_standard_page() {
        let mut doc = off_page_sheet();
        let fit = doc.refit_page(&BTreeSet::new()).expect("content to fit");
        assert!(fit.standard, "this content belongs on a standard page");
        assert_eq!(
            fit.page,
            [210.0, 148.0],
            "a 35 mm sketch takes the smallest standard page"
        );
        let bbox = doc.content_bbox().expect("content");
        assert!(
            bbox.min_x >= PAGE_MARGIN - 1.27 && bbox.min_y >= PAGE_MARGIN - 1.27,
            "content must start at the margin, got {bbox:?}"
        );
    }

    #[test]
    fn refit_keeps_the_drawing_rigid() {
        let mut doc = off_page_sheet();
        let before = crate::connect::extract(&doc);
        doc.refit_page(&BTreeSet::new());
        let after = crate::connect::extract(&doc);
        assert!(
            crate::Netlist::diff(&before, &after).is_empty(),
            "a rigid shift cannot change connectivity"
        );
    }

    /// A graft: the sheet's own symbol is frozen, the new block landed off-page.
    fn grafted_sheet() -> (SchDoc, String) {
        let symbol = "\t(symbol\n\
             \t\t(lib_id \"rectifier_schlib:VSIN\")\n\
             \t\t(at 102.87 101.6 0)\n\
             \t\t(uuid \"seated\")\n\
             \t)";
        let text = format!(
            "(kicad_sch\n\
             \t(version 20250114)\n\
             \t(paper \"A4\")\n\
             {symbol}\n\
             \t(wire (pts (xy -25.4 -12.7) (xy -25.4 20.32)) (uuid \"new-w\"))\n\
             \t(label \"VOUT\" (at -25.4 -12.7 0) (uuid \"new-l\"))\n\
             )\n"
        );
        (SchDoc::parse(&text).expect("parse"), symbol.to_string())
    }

    #[test]
    fn a_graft_slides_only_the_new_block() {
        let (mut doc, symbol) = grafted_sheet();
        let frozen = BTreeSet::from(["seated".to_string()]);
        let fit = doc.refit_page(&frozen).expect("content to fit");

        assert!(fit.standard, "the fitted sheet belongs on a standard page");
        assert!(
            doc.to_text().contains(&symbol),
            "the seated symbol was rewritten:\n{}",
            doc.to_text()
        );
        let bbox = doc.content_bbox().expect("content");
        assert!(
            bbox.min_x >= PAGE_MARGIN - 1.27 && bbox.min_y >= PAGE_MARGIN - 1.27,
            "the new block is still off-page: {bbox:?}"
        );
        assert!(
            fit.page[0] >= bbox.max_x && fit.page[1] >= bbox.max_y,
            "the page does not hold the drawing: {fit:?} vs {bbox:?}"
        );
    }

    #[test]
    fn a_slide_that_would_tear_the_drawing_is_abandoned() {
        let mut doc = SchDoc::parse(
            "(kicad_sch\n\
             \t(version 20250114)\n\
             \t(paper \"A4\")\n\
             \t(wire (pts (xy 5.08 40.64) (xy 25.4 40.64)) (uuid \"seated\"))\n\
             \t(wire (pts (xy 5.08 40.64) (xy 5.08 60.96)) (uuid \"new\"))\n\
             )\n",
        )
        .expect("parse");
        let fit = doc
            .refit_page(&BTreeSet::from(["seated".to_string()]))
            .expect("content to fit");

        assert_eq!(
            fit.shift,
            [0.0, 0.0],
            "the new wire was slid off the seated one"
        );
        let new = doc.wires().find(|w| w.uuid == "new").expect("the new wire");
        assert_eq!(
            new.points[0],
            Point2::new(5.08, 40.64),
            "its junction moved"
        );
    }

    #[test]
    fn content_past_a2_keeps_a_fitted_user_page() {
        let mut doc = SchDoc::parse(
            "(kicad_sch\n\
             \t(version 20250114)\n\
             \t(paper \"A4\")\n\
             \t(wire (pts (xy 0 0) (xy 900 500)) (uuid \"w1\"))\n\
             )\n",
        )
        .expect("parse");
        let fit = doc.refit_page(&BTreeSet::new()).expect("content to fit");
        assert!(!fit.standard, "content this large has no standard page");
        assert!(fit.page[0] > 900.0 && fit.page[1] > 500.0);
    }
}
