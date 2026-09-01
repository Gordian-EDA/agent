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
use sch_place::ir::LayoutIr;
use sch_place::item::{Incidence, Item};
use sch_place::place::PlaceOptions;

use crate::contract::{RouteRealization, RoutedSheetRealizer};
use crate::floorplan::place::add_orphan_label_columns;
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
    pub options: PlaceOptions,
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
    let realizer = RoutedSheetRealizer::new(env, inc, ir, draw.options);
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
    doc.adopt(&sheet)
}

/// Graft only a writer's wiring — wires, junctions, labels, markers, text — for a
/// re-wire of symbols the document already holds.
pub fn graft_drawing(doc: &mut SchDoc, writer: SchematicWriter) -> sch_doc::Result<()> {
    let sheet = to_doc(writer)?;
    doc.adopt_drawing(&sheet)
}
