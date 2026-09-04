//! Seating a block's dashed frame, its title and its note.
//!
//! Captions are decoration the realiser fully controls, so a caption that runs
//! through the drawing is a bug it can always avoid rather than report. A note
//! wraps to its frame's width and every caption is seated at the first candidate
//! that touches no ink: the sheet's symbols, pin text, fields, labels and wires,
//! every block's frame, and the captions already seated.

use geom::{GRID_50_MIL, Point2, Rect};

use super::{SchematicWriter, SheetText, field_anchors, field_box, label_rect};

/// One block's decoration, as [`SchematicWriter::add_block_frames`] draws it.
pub struct BlockFrame<'a> {
    /// Frame caption, drawn bold at a free corner of the frame.
    pub title: &'a str,
    /// The block's one-line explanation, wrapped to the frame's width.
    pub note: Option<&'a str>,
    /// Refdes of the parts the frame encloses.
    pub members: &'a [String],
}

/// Air between the block's outermost ink and its frame.
const FRAME_PAD: f64 = 3.81;
/// Air between a frame and the caption seated against it.
const GAP: f64 = 1.27;
const TITLE_SIZE: f64 = 1.778;
const NOTE_SIZE: f64 = 1.27;
/// A note wraps to its frame's width, held between these so a narrow block does
/// not stack one word per line and a wide one does not run the width of the page.
const WRAP_MIN: f64 = 45.0;
const WRAP_MAX: f64 = 90.0;
/// How far a caption may be pushed away from its frame looking for clear air.
const PUSH_STEPS: usize = 4;

impl SchematicWriter {
    /// Draw one dashed frame per block, captioned with its title and note.
    ///
    /// Every frame is drawn first, so a caption can be seated knowing where all of
    /// them are; the captions then go down in the given order, each avoiding the
    /// ones already seated. A block with no parts on this sheet draws nothing.
    ///
    /// Call AFTER [`SchematicWriter::prepare`], so the fields are where the solver
    /// put them; `prepare` is idempotent and re-running it reframes the sheet with
    /// the decoration included.
    pub fn add_block_frames(&mut self, blocks: &[BlockFrame<'_>]) {
        let framed: Vec<(usize, Rect)> = blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| Some((i, self.member_bbox(b.members)?.inflate(FRAME_PAD))))
            .collect();
        for (i, frame) in &framed {
            self.add_rect(
                [frame.min_x, frame.min_y],
                [frame.max_x, frame.max_y],
                blocks[*i].title,
            );
        }

        let ink = self.ink_boxes();
        let mut taken: Vec<Rect> = framed.iter().map(|(_, f)| *f).collect();
        for (i, frame) in &framed {
            let block = &blocks[*i];
            let title = self.seat(
                block.title,
                TITLE_SIZE,
                true,
                &format!("{}:title", block.title),
                &title_corners(*frame, &boxed(block.title, TITLE_SIZE)),
                &ink,
                &taken,
            );
            taken.push(title);
            let Some(note) = block.note.filter(|n| !n.is_empty()) else {
                continue;
            };
            for text in wrappings(note, *frame) {
                let corners = note_corners(*frame, title, &boxed(&text, NOTE_SIZE));
                let seated = self.seat(
                    &text,
                    NOTE_SIZE,
                    false,
                    &format!("{}:note", block.title),
                    &corners,
                    &ink,
                    &taken,
                );
                taken.push(seated);
                break;
            }
        }
    }

    /// Seat one caption at the first corner that touches nothing, falling back to
    /// the corner it fouls least. Returns the box it was seated in.
    #[allow(clippy::too_many_arguments)]
    fn seat(
        &mut self,
        text: &str,
        size: f64,
        bold: bool,
        key: &str,
        corners: &[Point2],
        ink: &[Rect],
        taken: &[Rect],
    ) -> Rect {
        let shape = boxed(text, size);
        let mut best: Option<(f64, Point2, Rect)> = None;
        for corner in corners {
            let anchor = GRID_50_MIL.snap_point(Point2::new(
                corner.x - shape.min_x,
                corner.y - shape.min_y,
            ));
            let at = Rect::new(
                anchor.x + shape.min_x,
                anchor.y + shape.min_y,
                anchor.x + shape.max_x,
                anchor.y + shape.max_y,
            );
            let fouled: f64 = ink
                .iter()
                .chain(taken)
                .filter_map(|o| at.intersection(o).map(|i| i.area()))
                .sum();
            if fouled <= 0.0 {
                best = Some((0.0, anchor, at));
                break;
            }
            if best.as_ref().is_none_or(|(f, ..)| fouled < *f) {
                best = Some((fouled, anchor, at));
            }
        }
        let (_, anchor, at) = best.expect("every caption has at least one candidate corner");
        self.add_text(text, anchor, size, bold, key);
        at
    }

    /// The box around a block's parts: their bodies and their solved field text.
    fn member_bbox(&self, members: &[String]) -> Option<Rect> {
        let mut bbox: Option<Rect> = None;
        let mut grow = |r: Rect| {
            bbox = Some(bbox.map_or(r, |b: Rect| {
                Rect::new(
                    b.min_x.min(r.min_x),
                    b.min_y.min(r.min_y),
                    b.max_x.max(r.max_x),
                    b.max_y.max(r.max_y),
                )
            }));
        };
        for inst in self
            .instances
            .iter()
            .filter(|i| members.iter().any(|m| *m == i.refdes))
        {
            let h = inst.half_extents.rotated_half_extents(inst.angle);
            grow(Rect::new(
                inst.at.x - h[0],
                inst.at.y - h[1],
                inst.at.x + h[0],
                inst.at.y + h[1],
            ));
            let (r, v) = field_anchors(inst);
            for (pos, text) in [(r, &inst.refdes), (v, &inst.value)] {
                if !text.is_empty() {
                    grow(field_box(pos.at, pos.justify, text));
                }
            }
        }
        bbox
    }

    /// Every piece of ink a caption must not sit on: symbol bodies and their pin
    /// text, field text, labels, wires, and the captions already on the sheet.
    fn ink_boxes(&self) -> Vec<Rect> {
        let mut ink = Vec::new();
        for inst in &self.instances {
            let h = inst.half_extents.rotated_half_extents(inst.angle);
            ink.push(Rect::new(
                inst.at.x - h[0],
                inst.at.y - h[1],
                inst.at.x + h[0],
                inst.at.y + h[1],
            ));
            if let Some(pins) = self.sym_pins.get(&inst.lib_id) {
                for pg in pins {
                    ink.extend(sch_model::text::pin_text_boxes(
                        pg,
                        inst.at,
                        inst.angle,
                        inst.mirror,
                    ));
                }
            }
            let (r, v) = field_anchors(inst);
            for (pos, text) in [(r, &inst.refdes), (v, &inst.value)] {
                if !text.is_empty() {
                    ink.push(field_box(pos.at, pos.justify, text));
                }
            }
        }
        for label in &self.labels {
            ink.push(label_rect(label, label.at, label.dir));
        }
        for wire in &self.wires {
            ink.push(sch_model::text::wire_box(wire.a, wire.b));
        }
        for text in &self.texts {
            ink.push(super::sheet_text_box(text));
        }
        ink
    }
}

/// The box a caption of `text` covers when anchored at the origin.
fn boxed(text: &str, size: f64) -> Rect {
    super::sheet_text_box(&SheetText {
        text: text.to_string(),
        at: Point2::new(0.0, 0.0),
        size,
        bold: false,
        uuid_key: String::new(),
    })
}

/// Min corners for a title, best first: the frame's four outside corners, the
/// top-left one — where a human writes it — first.
fn title_corners(frame: Rect, shape: &Rect) -> Vec<Point2> {
    let (w, h) = (shape.width(), shape.height());
    let (above, below) = (frame.min_y - GAP - h, frame.max_y + GAP);
    let (left, right) = (frame.min_x, frame.max_x - w);
    [
        (left, above),
        (right, above),
        (left, below),
        (right, below),
    ]
    .into_iter()
    .map(|(x, y)| Point2::new(x, y))
    .collect()
}

/// Min corners for a note, best first: under the frame, then over it clear of the
/// title, then beside it — each pushed further out while it stays fouled.
fn note_corners(frame: Rect, title: Rect, shape: &Rect) -> Vec<Point2> {
    let (w, h) = (shape.width(), shape.height());
    let step = h + GAP;
    let mut out = Vec::new();
    for push in 0..PUSH_STEPS {
        let d = push as f64 * step;
        let below = frame.max_y + GAP + d;
        let above = frame.min_y.min(title.min_y) - GAP - h - d;
        out.extend([
            (frame.min_x, below),
            (frame.max_x - w, below),
            (frame.min_x, above),
            (frame.max_x - w, above),
        ]);
    }
    for push in 0..PUSH_STEPS {
        let d = push as f64 * (w + GAP);
        out.extend([
            (frame.max_x + GAP + d, frame.min_y),
            (frame.min_x - GAP - w - d, frame.min_y),
        ]);
    }
    out.into_iter().map(|(x, y)| Point2::new(x, y)).collect()
}

/// The note's text at each wrap width worth trying, widest first: a note wrapped
/// to its frame reads as belonging to it, and a narrower column is the fallback
/// when the wide one has nowhere clear to sit.
fn wrappings(note: &str, frame: Rect) -> Vec<String> {
    let wide = frame.width().clamp(WRAP_MIN, WRAP_MAX);
    let mut widths = vec![wide];
    if wide > WRAP_MIN + 1.0 {
        widths.push(WRAP_MIN);
    }
    widths.into_iter().map(|w| wrap(note, w)).collect()
}

/// `note` broken into lines no wider than `width` mm at [`NOTE_SIZE`], greedily.
/// A word longer than the width gets its own line rather than being cut.
fn wrap(note: &str, width: f64) -> String {
    let mut lines: Vec<String> = Vec::new();
    for word in note.split_whitespace() {
        match lines.last_mut() {
            Some(line)
                if sch_model::text::advance(&format!("{line} {word}"), NOTE_SIZE) <= width =>
            {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_note_wraps_to_the_frame_width() {
        let note = "R3/R4=15.8k and C2/C3=10nF set approximately 1 kHz; U1A buffers the \
                    unity-gain Sallen-Key stage and U1B provides the gain of ten.";
        let wrapped = wrap(note, 60.0);
        assert!(wrapped.lines().count() > 1);
        for line in wrapped.lines() {
            assert!(sch_model::text::advance(line, NOTE_SIZE) <= 60.0, "{line}");
        }
        assert_eq!(
            wrapped.split_whitespace().collect::<Vec<_>>(),
            note.split_whitespace().collect::<Vec<_>>(),
        );
    }

    #[test]
    fn the_title_prefers_the_frames_top_left() {
        let frame = Rect::new(10.0, 20.0, 60.0, 50.0);
        let shape = boxed("Power Entry", TITLE_SIZE);
        let corners = title_corners(frame, &shape);
        assert_eq!(corners[0].x, frame.min_x);
        assert!(corners[0].y < frame.min_y);
    }
}
