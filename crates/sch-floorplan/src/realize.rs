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

use kicad::KicadInstallation;
use sch_check::Design;
use sch_doc::SchDoc;
use sch_model::ir::LayoutIr;
use sch_model::item::{Incidence, Item};

use crate::floorplan::place::add_orphan_label_columns;
use crate::floorplan::place::{RouteRealization, RoutedSheetRealizer};
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
    let mut writer = realizer.realize_writer(draw.title, items, RouteRealization::ShippedSheet)?;
    add_orphan_label_columns(&mut writer, design, inc);
    writer.set_frame(draw.frame);
    writer.prepare();
    Ok(writer)
}

/// Render a finished writer as a standalone document.
pub fn to_doc(writer: SchematicWriter) -> sch_doc::Result<SchDoc> {
    SchDoc::parse(&writer.finish())
}

/// Graft a finished writer's content into `doc`, returning the new symbols' UUIDs.
pub fn graft(doc: &mut SchDoc, writer: SchematicWriter) -> sch_doc::Result<Vec<String>> {
    let sheet = to_doc(writer)?;
    fit_page(doc, &sheet);
    let adopted = doc.adopt(&sheet)?;
    debug_assert_unique_wire_segments(doc);
    Ok(adopted)
}

/// Grow `doc`'s page to hold what `sheet` draws. The realiser sizes its own page to its
/// content; a graft carries the content across, so the page has to follow or the drawing
/// lands off the sheet and renders blank.
fn fit_page(doc: &mut SchDoc, sheet: &SchDoc) {
    if let Some(size) = sheet.page() {
        doc.grow_page(size);
    }
}

/// Graft only a writer's wiring — wires, junctions, labels, markers, text — for a
/// re-wire of symbols the document already holds.
pub fn graft_drawing(doc: &mut SchDoc, writer: SchematicWriter) -> sch_doc::Result<()> {
    let sheet = to_doc(writer)?;
    fit_page(doc, &sheet);
    doc.adopt_drawing(&sheet)?;
    debug_assert_unique_wire_segments(doc);
    Ok(())
}

/// Assert in debug builds that the adopted sheet has unique unordered wire segments.
fn debug_assert_unique_wire_segments(doc: &SchDoc) {
    #[cfg(debug_assertions)]
    {
        let mut seen = std::collections::BTreeSet::new();
        for wire in doc.wires() {
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
}
