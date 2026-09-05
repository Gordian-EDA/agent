//! Seating a block's dashed frame, its title and its note.
//!
//! Captions are decoration the realiser fully controls, so a caption that runs
//! through the drawing is a bug it can always avoid rather than report. A note
//! wraps to its frame's width and every caption is seated at the first candidate
//! that touches no ink: the sheet's symbols, pin text, fields, labels and wires,
//! every block's frame, and the captions already seated.

use geom::{GRID_50_MIL, Point2, Rect};

use sch_flex::pack::FRAME_PAD;

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

/// Air between a frame and the caption seated against it. A caption whose
/// ascenders graze the dashed border reads as a mistake, so this is a clear
/// line's worth rather than a hairline.
const GAP: f64 = 1.905;
const TITLE_SIZE: f64 = 2.54;
const NOTE_SIZE: f64 = 1.27;
/// A note wraps to its frame's width, held between these so a narrow block does
/// not stack one word per line and a wide one does not run the width of the page.
const WRAP_MIN: f64 = 45.0;
const WRAP_MAX: f64 = 90.0;
/// How far a caption may be pushed away from its frame looking for clear air.
/// Two rungs: a caption further off than that has stopped looking like it
/// belongs to the block, which is worse than the overlap it bought.
const PUSH_STEPS: usize = 2;

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
            .filter_map(|(i, b)| Some((i, self.block_frame(b.members)?)))
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
            let corners = title_corners(*frame, &boxed(block.title, TITLE_SIZE));
            let (_, anchor, title) = best_seat(block.title, TITLE_SIZE, &corners, &ink, &taken);
            self.add_text(
                block.title,
                anchor,
                TITLE_SIZE,
                true,
                &format!("{}:title", block.title),
            );
            taken.push(title);
            let Some(note) = block.note.filter(|n| !n.is_empty()) else {
                continue;
            };
            // Widest wrap first; a narrower column is only worth it if the wide
            // one has nowhere clear to sit.
            let seated = wrappings(note, *frame)
                .into_iter()
                .map(|text| {
                    let corners = note_corners(*frame, title, &boxed(&text, NOTE_SIZE));
                    let (fouled, anchor, at) = best_seat(&text, NOTE_SIZE, &corners, &ink, &taken);
                    (fouled, text, anchor, at)
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .expect("a note has at least one wrapping");
            self.add_text(
                &seated.1,
                seated.2,
                NOTE_SIZE,
                false,
                &format!("{}:note", block.title),
            );
            taken.push(seated.3);
        }
    }

    /// The rectangle drawn around one block: its parts' bodies and fields, reaching out
    /// over the net labels they carry wherever that reach stays off ANOTHER block's ink.
    ///
    /// Reaching without that check is how a frame ends up drawn straight through the
    /// neighbouring column's capacitor, which says that part is in a block it is not in —
    /// worse than clipping a label of its own. Only the strip a side actually gains is
    /// tested, and only against real PARTS: the rails and flags a `#` reference marks
    /// belong to no block, and testing against them would be circular, since the ones a
    /// member's own pins carry sit in exactly the strip the growth exists to cover.
    ///
    /// One block at a time is all an incremental call holds, so on that path there are no
    /// foreign parts to test and the reach is taken on trust.
    fn block_frame(&self, members: &[String]) -> Option<Rect> {
        let tight = self.member_bbox(members)?.inflate(FRAME_PAD);
        let Some(labels) = self.member_label_bbox(members) else {
            return Some(tight);
        };
        let want = labels.inflate(FRAME_PAD);
        let foreign = self.foreign_part_ink(members);
        let mut frame = tight;
        for side in 0..4 {
            let mut wider = frame;
            let strip = match side {
                0 => {
                    wider.min_x = frame.min_x.min(want.min_x);
                    Rect::new(wider.min_x, wider.min_y, frame.min_x, wider.max_y)
                }
                1 => {
                    wider.min_y = frame.min_y.min(want.min_y);
                    Rect::new(wider.min_x, wider.min_y, wider.max_x, frame.min_y)
                }
                2 => {
                    wider.max_x = frame.max_x.max(want.max_x);
                    Rect::new(frame.max_x, wider.min_y, wider.max_x, wider.max_y)
                }
                _ => {
                    wider.max_y = frame.max_y.max(want.max_y);
                    Rect::new(wider.min_x, frame.max_y, wider.max_x, wider.max_y)
                }
            };
            if foreign.iter().all(|o| strip.intersection(o).is_none()) {
                frame = wider;
            }
        }
        Some(frame)
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
            .filter(|i| members.contains(&i.refdes))
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

    /// The box around the net labels a block's parts carry — the ink a frame drawn to
    /// the bodies alone cuts through, which is what a mirrored connector's label column
    /// runs into. A pin label's uuid key names the part it hangs off.
    fn member_label_bbox(&self, members: &[String]) -> Option<Rect> {
        self.labels
            .iter()
            .filter(|label| {
                members
                    .iter()
                    .any(|refdes| label.uuid_key.starts_with(&format!("{refdes}:")))
            })
            .map(|label| super::label_rect(label, label.at, label.dir))
            .reduce(|a, b| {
                Rect::new(
                    a.min_x.min(b.min_x),
                    a.min_y.min(b.min_y),
                    a.max_x.max(b.max_x),
                    a.max_y.max(b.max_y),
                )
            })
    }

    /// Ink drawn by a real part this block does not hold: its body and field text.
    fn foreign_part_ink(&self, members: &[String]) -> Vec<Rect> {
        let mut out = Vec::new();
        for inst in self
            .instances
            .iter()
            .filter(|i| !i.refdes.starts_with('#') && !members.contains(&i.refdes))
        {
            let h = inst.half_extents.rotated_half_extents(inst.angle);
            out.push(Rect::new(
                inst.at.x - h[0],
                inst.at.y - h[1],
                inst.at.x + h[0],
                inst.at.y + h[1],
            ));
            let (r, v) = field_anchors(inst);
            for (pos, text) in [(r, &inst.refdes), (v, &inst.value)] {
                if !text.is_empty() {
                    out.push(field_box(pos.at, pos.justify, text));
                }
            }
        }
        out
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

/// The least-fouled seat for `text` among `corners`, as (fouled area, anchor,
/// box). Corners are tried in order and the first clear one wins, so the
/// preference the caller encoded in that order decides every uncontested case.
fn best_seat(
    text: &str,
    size: f64,
    corners: &[Point2],
    ink: &[Rect],
    taken: &[Rect],
) -> (f64, Point2, Rect) {
    let shape = boxed(text, size);
    let mut best: Option<(f64, Point2, Rect)> = None;
    for corner in corners {
        let anchor =
            GRID_50_MIL.snap_point(Point2::new(corner.x - shape.min_x, corner.y - shape.min_y));
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
            return (0.0, anchor, at);
        }
        if best.as_ref().is_none_or(|(f, ..)| fouled < *f) {
            best = Some((fouled, anchor, at));
        }
    }
    best.expect("every caption has at least one candidate corner")
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

/// The two x positions a caption may take against `frame`: aligned with its left
/// edge, or with its right. A caption wider than its frame only gets the left one
/// — sliding it left to right-align would carry it out of the block it names.
fn spans(frame: Rect, w: f64) -> Vec<f64> {
    match frame.width() >= w {
        true => vec![frame.min_x, frame.max_x - w],
        false => vec![frame.min_x],
    }
}

/// Min corners for a title, best first. A title belongs over its frame's top-left
/// corner, so that corner is tried at increasing heights before the other three
/// are considered: a sheet whose titles all sit in the same corner reads as one
/// drawing, and moving a title is a bigger change than lifting it.
fn title_corners(frame: Rect, shape: &Rect) -> Vec<Point2> {
    let (w, h) = (shape.width(), shape.height());
    let mut out = Vec::new();
    for x in spans(frame, w) {
        for push in 0..PUSH_STEPS {
            out.push((x, frame.min_y - GAP - h - push as f64 * (h + GAP)));
        }
    }
    out.extend(spans(frame, w).into_iter().map(|x| (x, frame.max_y + GAP)));
    out.into_iter().map(|(x, y)| Point2::new(x, y)).collect()
}

/// Min corners for a note, best first: under its frame, then over it, and only
/// then out beside it.
///
/// Every corner but the last two keeps the note within its frame's own x-span, so
/// a reader never has to guess which block it explains — the failure that costs
/// more than the overlap it was avoiding. Over the frame means over the title
/// too, which leaves the explanation reading before the heading, so those come
/// second; beside the frame is the last resort.
fn note_corners(frame: Rect, title: Rect, shape: &Rect) -> Vec<Point2> {
    let (w, h) = (shape.width(), shape.height());
    let mut out = Vec::new();
    for push in 0..PUSH_STEPS {
        let below = frame.max_y + GAP + push as f64 * (h + GAP);
        out.extend(spans(frame, w).into_iter().map(|x| (x, below)));
    }
    for push in 0..PUSH_STEPS {
        let above = title.min_y - GAP - h - push as f64 * (h + GAP);
        out.extend(spans(frame, w).into_iter().map(|x| (x, above)));
    }
    out.extend([
        (frame.max_x + GAP, frame.min_y),
        (frame.min_x - GAP - w, frame.min_y),
    ]);
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
