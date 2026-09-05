//! `realize` — a finished placement becomes document items.
//!
//! [`crate::write::SchematicWriter`] is the realiser: it holds the placed symbols, the
//! routed wires, the solved labels and the no-connect markers of one finished sheet. This
//! module is the last step — turning that into [`sch_doc::SchDoc`] items, either as a
//! whole new sheet ([`to_doc`]) or grafted into a document that already has content
//! ([`graft`] / [`graft_drawing`]).
//!
//! The writer renders one self-contained sheet, so a graft goes through that rendering and
//! is adopted by the target: [`sch_doc::SchDoc::adopt`] re-derives every UUID and
//! retargets the `(instances)` paths, which is what a symbol drawn on one sheet needs to
//! be annotated on another. Nothing already in the target is touched, so its untouched
//! items still write back from their original bytes.

use std::collections::BTreeSet;

use kicad::KicadInstallation;
use sch_check::Design;
use sch_doc::SchDoc;
use sch_model::ir::LayoutIr;
use sch_model::item::{Incidence, Item};

use crate::floorplan::place::add_orphan_label_columns;
use crate::floorplan::place::RoutedSheetRealizer;
use crate::write::SchematicWriter;

/// How a block is drawn, beyond the items themselves.
#[derive(Debug, Clone, Copy, Default)]
pub struct Draw<'a> {
    /// Sheet title, rendered in the drawing frame's title block.
    pub title: Option<&'a str>,
    /// Shift the finished drawing to the page margin. Right when these items are the
    /// whole sheet, wrong when the caller placed them beside content that is already
    /// there — so live editing leaves it off.
    pub frame: bool,
    /// Nets a power-output pin already drives in the document being drawn into. The
    /// realiser adds no `PWR_FLAG` for these: a second one is an ERC error.
    pub driven: &'a [String],
    /// The drawing these items are being added BESIDE, when the sheet already has
    /// content: its pins, wire ends and label anchors with the nets they carry. The
    /// router, the stub retraction and the net audit all treat it as foreign, so this
    /// block can neither draw across it nor weld onto it. `None` for a whole sheet.
    pub beside: Option<&'a sch_model::route::RouteScene>,
}

/// Draw `items` at the poses they carry: route, label, no-connect, text-solve.
pub fn realize_block(
    env: &KicadInstallation,
    design: &Design,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    draw: Draw<'_>,
) -> std::io::Result<SchematicWriter> {
    let mut realizer = RoutedSheetRealizer::new(env, inc, ir).already_driven(draw.driven);
    if let Some(scene) = draw.beside {
        realizer = realizer.beside(scene);
    }
    let mut writer = realizer.realize_writer(draw.title, items)?;
    add_orphan_label_columns(&mut writer, design, inc);
    writer.set_frame(draw.frame);
    writer.prepare();
    draw_block_frames(&mut writer, design, items);
    // Re-run the (idempotent) finalize so the reframe sees the frames it must keep on
    // the page; the text solve and wire splitting are unchanged by decoration.
    writer.prepare();
    Ok(writer)
}

/// Draw one dashed frame per design region that has parts on this sheet, captioned with
/// the region's title and carrying its note.
///
/// A region the tools synthesized ([`sch_model::result::synthesized_block`]) is not a
/// functional block — it is everything the author did not divide up — so it gets no
/// frame; the drawing frame and title block already delimit the sheet.
fn draw_block_frames(writer: &mut SchematicWriter, design: &Design, items: &[Item]) {
    let members: Vec<(&String, Vec<String>)> = design
        .blocks
        .keys()
        .filter(|name| !sch_model::result::synthesized_block(name))
        .map(|name| {
            let refs = items
                .iter()
                .filter(|it| &it.block == name)
                .map(|it| it.refdes.clone())
                .collect();
            (name, refs)
        })
        .collect();
    let frames: Vec<crate::write::BlockFrame<'_>> = members
        .iter()
        .filter(|(_, refs)| !refs.is_empty())
        .map(|(name, refs)| {
            let block = &design.blocks[*name];
            crate::write::BlockFrame {
                title: block.title.as_deref().unwrap_or(name),
                note: block.note.as_deref(),
                members: refs,
            }
        })
        .collect();
    writer.add_block_frames(&frames);
}

/// Render a finished writer as a standalone document.
pub fn to_doc(writer: SchematicWriter) -> sch_doc::Result<SchDoc> {
    SchDoc::parse(&writer.finish())
}

/// Graft a finished writer's content into `doc`, returning the new symbols' UUIDs.
///
/// The grafted block carries whatever coordinates the region search chose — it is placed
/// BESIDE what is already there, so it may reach outside the frame — and adoption changes
/// what the sheet as a whole spans. [`sch_doc::SchDoc::refit_page`] settles both, moving
/// only the block just adopted: the sheet's own parts are the caller's, and a graft that
/// slid them would be an edit nobody asked for.
pub fn graft(doc: &mut SchDoc, writer: SchematicWriter) -> sch_doc::Result<Vec<String>> {
    let sheet = to_doc(writer)?;
    replace_frames(doc, &sheet);
    let seated = seated_uuids(doc);
    let adopted = doc.adopt(&sheet)?;
    debug_assert_unique_wire_segments(doc);
    doc.refit_page(&seated);
    Ok(adopted)
}

/// The UUIDs of everything on the sheet right now — what a graft must leave untouched.
fn seated_uuids(doc: &SchDoc) -> BTreeSet<String> {
    doc.items()
        .iter()
        .filter_map(|item| item.uuid().map(String::from))
        .collect()
}

/// A caption's text with its line breaks undone, so a note recognises its own older self
/// even though the frame it wrapped to has since changed width.
fn unwrapped(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// How far a caption may sit from the frame it names. A title is seated a clear line above
/// its outline and a note a clear line below, so anything further off is another block's.
const CAPTION_REACH: f64 = 12.7;

/// The caption `frame` wears: the nearest one within [`CAPTION_REACH`].
fn nearest_caption<'a>(
    frame: &geom::Rect,
    captions: &'a [(String, geom::Point2)],
) -> Option<&'a str> {
    let gap = |at: &geom::Point2| {
        let dx = (frame.min_x - at.x).max(at.x - frame.max_x).max(0.0);
        let dy = (frame.min_y - at.y).max(at.y - frame.max_y).max(0.0);
        dx.hypot(dy)
    };
    captions
        .iter()
        .map(|(text, at)| (gap(at), text))
        .filter(|(d, _)| *d <= CAPTION_REACH)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, text)| text.as_str())
}

/// Every block frame drawn on `doc`.
fn frame_rects(doc: &SchDoc) -> Vec<geom::Rect> {
    doc.items()
        .iter()
        .filter_map(|item| match item {
            sch_doc::Item::Rectangle(r) => Some(geom::Rect::from_points(r.start, r.end)),
            _ => None,
        })
        .collect()
}

/// Drop the block frames `sheet` is about to redraw — and only those.
///
/// A frame belongs to the block whose caption it wears, so a caption this sheet redraws
/// takes its old rectangle with it wherever that rectangle sits: a block extended by a
/// later call gets a new frame around the parts it now has, and a block redrawn somewhere
/// else leaves nothing behind. Frames are decoration the realiser owns.
///
/// Deleting on OVERLAP instead is what stripped a neighbour's outline the moment two
/// blocks were seated next to each other — and it was needed only because the seat
/// reserved less sheet than the frame drew, so adjacent frames intersected by
/// construction. They no longer do ([`sch_flex::pack::block_frame`]), so overlap says
/// nothing about whose frame a rectangle is.
///
/// Ownership runs from the FRAME outward, not from the caption: a rectangle belongs to
/// the caption nearest it, and is stale when that caption is one this sheet redraws. A
/// block too small to be worth an outline draws its caption bare, and a bare caption
/// asking for the nearest rectangle would carry off its neighbour's — where a frame
/// asking for its nearest caption always finds the title seated against its own corner.
fn replace_frames(doc: &mut SchDoc, sheet: &SchDoc) {
    let captions: BTreeSet<String> = sheet
        .items()
        .iter()
        .filter_map(|item| match item {
            sch_doc::Item::Text(t) => Some(unwrapped(&t.text)),
            _ => None,
        })
        .collect();
    if captions.is_empty() {
        return;
    }
    let seated: Vec<(String, geom::Point2)> = doc
        .items()
        .iter()
        .filter_map(|item| match item {
            sch_doc::Item::Text(t) => Some((unwrapped(&t.text), t.at.point())),
            _ => None,
        })
        .collect();
    let stale: Vec<geom::Rect> = frame_rects(doc)
        .into_iter()
        .filter(|frame| {
            nearest_caption(frame, &seated).is_some_and(|text| captions.contains(text))
        })
        .collect();
    doc.retain_drawing(|item| match item {
        sch_doc::Item::Rectangle(r) => !stale.contains(&geom::Rect::from_points(r.start, r.end)),
        sch_doc::Item::Text(t) => !captions.contains(&unwrapped(&t.text)),
        _ => true,
    });
}

/// Graft only a writer's wiring — wires, junctions, labels, markers, text — for a
/// re-wire of symbols the document already holds.
pub fn graft_drawing(doc: &mut SchDoc, writer: SchematicWriter) -> sch_doc::Result<()> {
    let sheet = to_doc(writer)?;
    replace_frames(doc, &sheet);
    let seated = seated_uuids(doc);
    doc.adopt_drawing(&sheet)?;
    debug_assert_unique_wire_segments(doc);
    doc.refit_page(&seated);
    Ok(())
}

/// Assert in debug builds that the adopted sheet has unique unordered wire segments.
///
/// Checked on what adoption produced, before the page fit: the fit slides the new block
/// away from the sheet's own drawing, which would pull a duplicate pair apart and hide
/// the defect this catches.
fn debug_assert_unique_wire_segments(_doc: &SchDoc) {
    #[cfg(debug_assertions)]
    {
        let mut seen = std::collections::BTreeSet::new();
        for wire in _doc.wires() {
            for points in wire.points.windows(2) {
                let a = crate::write::point_key(points[0]);
                let b = crate::write::point_key(points[1]);
                let pair = if a <= b { (a, b) } else { (b, a) };
                debug_assert!(
                    seen.insert(pair),
                    "adopted wire segments must have unique unordered endpoint pairs; repeated {pair:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A captioned block frame, seated the way [`crate::write::SchematicWriter`] seats
    /// one: the title a clear line above the outline's top-left corner.
    fn captioned(writer: &mut SchematicWriter, title: &str, frame: geom::Rect) {
        writer.add_rect([frame.min_x, frame.min_y], [frame.max_x, frame.max_y], title);
        writer.add_text(
            title,
            [frame.min_x, frame.min_y - 1.905],
            2.54,
            true,
            &format!("{title}:title"),
        );
    }

    fn frames(doc: &SchDoc) -> Vec<geom::Rect> {
        doc.items()
            .iter()
            .filter_map(|item| match item {
                sch_doc::Item::Rectangle(r) => Some(geom::Rect::from_points(r.start, r.end)),
                _ => None,
            })
            .collect()
    }

    /// Redrawing one block takes its own outline with it, wherever the old one sat, and
    /// leaves every other block's alone — a neighbour's frame is not the redrawn block's
    /// to delete, however close the two land.
    #[test]
    fn a_redraw_replaces_its_own_frame_and_only_its_own() {
        let neighbour = geom::Rect::new(101.6, 50.8, 152.4, 101.6);
        let mut sheet = SchematicWriter::new();
        captioned(&mut sheet, "regulators", geom::Rect::new(25.4, 50.8, 76.2, 101.6));
        captioned(&mut sheet, "audio", neighbour);
        let mut doc = to_doc(sheet).unwrap();
        assert_eq!(frames(&doc).len(), 2);

        let mut redraw = SchematicWriter::new();
        captioned(&mut redraw, "regulators", geom::Rect::new(25.4, 127.0, 88.9, 177.8));
        replace_frames(&mut doc, &to_doc(redraw).unwrap());

        assert_eq!(
            frames(&doc),
            vec![neighbour],
            "the redrawn block's old frame goes, the neighbour's stays"
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "adopted wire segments must have unique unordered endpoint pairs")]
    fn graft_rejects_reversed_wire_duplicate() {
        let mut first = SchematicWriter::new();
        first.add_wire_on_net([10.16, 10.16], [11.43, 10.16], "SIG");
        let mut doc = to_doc(first).unwrap();
        let mut second = SchematicWriter::new();
        second.add_wire_on_net([11.43, 10.16], [10.16, 10.16], "SIG");

        let _ = graft(&mut doc, second);
    }

    /// A block whose natural landing is off the page must not drag the sheet under it.
    #[test]
    fn graft_slides_only_the_new_block_onto_the_page() {
        let mut seated = SchematicWriter::new();
        seated.add_wire_on_net([101.6, 101.6], [127.0, 101.6], "SEATED");
        let mut doc = to_doc(seated).unwrap();
        doc.refit_page(&Default::default());
        let before: Vec<geom::Point2> = doc.wires().flat_map(|w| w.points.clone()).collect();
        let seated_y = before[0].y;

        let mut block = SchematicWriter::new();
        block.add_wire_on_net([-25.4, -12.7], [-25.4, 12.7], "NEW");
        graft(&mut doc, block).unwrap();

        let after: Vec<geom::Point2> = doc
            .wires()
            .filter(|w| w.points.iter().any(|p| p.y == seated_y))
            .flat_map(|w| w.points.clone())
            .collect();
        assert_eq!(before, after, "the seated wire moved");
        let bbox = doc.content_bbox().unwrap();
        assert!(
            bbox.min_x >= geom::PAGE_MARGIN && bbox.min_y >= geom::PAGE_MARGIN,
            "the block is still off the page: {bbox:?}"
        );
        let page = doc.page().expect("a page");
        assert!(
            page[0] >= bbox.max_x && page[1] >= bbox.max_y,
            "the page does not hold the drawing: {page:?} vs {bbox:?}"
        );
    }
}

