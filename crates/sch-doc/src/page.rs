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
//! Rigid translation cannot change connectivity (every coincidence is
//! preserved), so this runs unconditionally after every write path.

use geom::{GRID_50_MIL, Point2, Rect};

use crate::body::body_rect;
use crate::doc::SchDoc;
use crate::model::{Item, Pose};
use crate::sexpr::{num, quoted, tagged};

/// Clearance kept between the drawn content and the page edge, in mm. Half an
/// inch: KiCAD's own drawing frame border is 10 mm, so this keeps content off
/// the border rule as well as off the paper edge.
pub const PAGE_MARGIN: f64 = 12.7;

/// Bottom band a KiCAD title block occupies inside the frame, in mm. Content
/// that reaches into it is overprinted by the sheet metadata.
const TITLE_BLOCK_BAND: f64 = 33.0;

/// Rendered width of a text run, in mm — the same 1.1 mm/character estimate the
/// realiser's text solver uses, scaled by the font size KiCAD defaults to.
fn text_width(s: &str, size: f64) -> f64 {
    s.chars().count() as f64 * 1.1 * (size / 1.27)
}

/// The standard landscape pages a generated sheet may use, smallest first.
/// Humans use A4 and A3 for boards of this size and almost never a custom page.
const STANDARD_PAGES: [(&str, [f64; 2]); 3] =
    [("A4", [297.0, 210.0]), ("A3", [420.0, 297.0]), ("A2", [594.0, 420.0])];

/// What [`SchDoc::refit_page`] did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageFit {
    /// The grid-snapped shift applied to every drawn item.
    pub shift: [f64; 2],
    /// The page the sheet now declares, in mm.
    pub page: [f64; 2],
    /// Whether the page is one of [`STANDARD_PAGES`]. False means the content is
    /// larger than A2 and the sheet keeps a `User` page sized to fit it — visible
    /// content beats a standard name.
    pub standard: bool,
}

impl SchDoc {
    /// The bounding box of everything drawn on the sheet, in mm.
    ///
    /// Symbol bodies come from the embedded definition's graphics
    /// ([`crate::body::body_rect`]), so this is the shape KiCAD renders, not a
    /// pin-span approximation; visible field text, label text and annotation text
    /// are boxed by their estimated width in both directions, which covers every
    /// justification without decoding one. `None` for an empty sheet.
    pub fn content_bbox(&self) -> Option<Rect> {
        let mut points: Vec<Point2> = Vec::new();
        let text_at = |pose: Pose, s: &str, size: f64, points: &mut Vec<Point2>| {
            let w = text_width(s, size);
            points.push(Point2::new(pose.x - w, pose.y - size));
            points.push(Point2::new(pose.x + w, pose.y + size));
        };
        for item in self.items() {
            match item {
                Item::Symbol(inst) => {
                    if let Some(r) = body_rect(self, inst) {
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
                            text_at(at, &field.value, field.font_size[0].max(1.27), &mut points);
                        }
                    }
                }
                Item::Wire(w) => points.extend(w.points.iter().copied()),
                Item::Junction(j) => points.push(j.at),
                Item::NoConnect(n) => points.push(n.at),
                Item::Label(l) => text_at(l.at, &l.text, 1.27, &mut points),
                Item::Text(t) => text_at(t.at, &t.text, 1.27, &mut points),
                Item::Rectangle(r) => points.extend([r.start, r.end]),
                Item::Sheet(s) => {
                    points.push(s.at);
                    points.push(Point2::new(s.at.x + s.size.x, s.at.y + s.size.y));
                }
                Item::LibSymbols(_) | Item::Other(_) => {}
            }
        }
        Rect::bounding(&points)
    }

    /// Shift every drawn item by `(dx, dy)` mm.
    ///
    /// Rigid, so no two points that coincided stop coinciding: the netlist is
    /// invariant under it. Symbol field positions are absolute in KiCAD and move
    /// with their symbol.
    pub fn translate(&mut self, dx: f64, dy: f64) {
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
        for item in self.items_mut() {
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
                    s.raw.touch();
                }
                Item::LibSymbols(_) | Item::Other(_) => {}
            }
        }
        self.mark_edited();
    }

    /// Bring the whole drawing inside the frame and size the page to it.
    ///
    /// The content's minimum corner lands at [`PAGE_MARGIN`] and the page becomes
    /// the smallest of A4/A3/A2 landscape that holds it — the sizes humans draw
    /// on. Only content larger than A2 keeps a `User` page, because an invisible
    /// drawing is worse than an unconventional page. When a title block is
    /// present the page also reserves the band it prints in, so metadata never
    /// overlaps the lowest parts.
    ///
    /// The shift is snapped to the 50 mil grid, so grid-aligned geometry stays
    /// grid-aligned (KiCAD's ERC rejects off-grid endpoints). `None` for an empty
    /// sheet, which keeps whatever page it declares.
    pub fn refit_page(&mut self) -> Option<PageFit> {
        let bbox = self.content_bbox()?;
        let dx = GRID_50_MIL.snap(PAGE_MARGIN - bbox.min_x);
        let dy = GRID_50_MIL.snap(PAGE_MARGIN - bbox.min_y);
        self.translate(dx, dy);

        let band = self.has_title_block().then_some(TITLE_BLOCK_BAND).unwrap_or(0.0);
        let need = [
            bbox.width() + 2.0 * PAGE_MARGIN,
            bbox.height() + 2.0 * PAGE_MARGIN + band,
        ];
        let fit = STANDARD_PAGES
            .iter()
            .find(|(_, size)| size[0] >= need[0] && size[1] >= need[1]);
        let (page, standard) = match fit {
            Some((name, size)) => {
                self.set_paper(tagged("paper", vec![quoted(*name)]));
                (*size, true)
            }
            None => {
                self.set_paper(tagged(
                    "paper",
                    vec![quoted("User"), num(need[0]), num(need[1])],
                ));
                (need, false)
            }
        };
        Some(PageFit {
            shift: [dx, dy],
            page,
            standard,
        })
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
