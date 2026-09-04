//! The **as-drawn text model**: one box per piece of text KiCAD actually
//! renders, and the [`TextSolver`] contract a layout engine plugs its own
//! solver into.
//!
//! Every consumer — the readability lint, the visual-facts measurement, the
//! field/label solver's obstacles and candidates — boxes text through this
//! module, so a lint hit means the render really shows the overlap.
//!
//! # Calibration
//!
//! The constants are measured, not guessed: `kicad-cli sch export svg` writes
//! each string's pen step and strokes its ink as a `<g class="stroked-text">`
//! path group. `tools/sch_text_ink.py glyphs` reads both back — it reproduces
//! every advance and reach below exactly — and `… overlaps` regenerates the
//! ink fixtures the model is held to. Everything is in units of the font size
//! except where stated.
//!
//! `sch-doc`'s `drawn_text` fixture test holds the model to both halves of
//! that: every box CONTAINS the ink KiCAD strokes, and the boxes reproduce
//! every text overlap three rendered sheets actually draw.
//!
//! Two shapes are extrapolated rather than measured, because the writer emits
//! neither: [`LabelShape`]'s `input`/`output` and `passive` pentagon pads.

use geom::{Dir, Point2, Rect};
use kicad_symbol::geometry::{PinGeom, PinTextStyle};

/// KiCAD's default schematic font size (mm).
pub const FONT_SIZE: f64 = 1.27;

/// Per-glyph advance, in units of the font size, for the KiCAD stroke font.
/// Sorted by character so [`glyph_advance`] can binary-search it.
const GLYPH_ADVANCE: [(char, f64); 95] = [
    (' ', 0.7220),
    ('!', 0.5162),
    ('"', 0.8019),
    ('#', 1.0400),
    ('$', 0.9924),
    ('%', 1.1828),
    ('&', 1.2781),
    ('\'', 0.5162),
    ('(', 0.7067),
    (')', 0.7067),
    ('*', 0.8019),
    ('+', 1.2781),
    (',', 0.5162),
    ('-', 1.2781),
    ('.', 0.5162),
    ('/', 1.0876),
    ('0', 0.9924),
    ('1', 0.9924),
    ('2', 0.9924),
    ('3', 0.9924),
    ('4', 0.9924),
    ('5', 0.9924),
    ('6', 0.9924),
    ('7', 0.9924),
    ('8', 0.9924),
    ('9', 0.9924),
    (':', 0.5162),
    (';', 0.5162),
    ('<', 1.2781),
    ('=', 1.2781),
    ('>', 1.2781),
    ('?', 0.8972),
    ('@', 1.3257),
    ('A', 0.8972),
    ('B', 1.0400),
    ('C', 1.0400),
    ('D', 1.0400),
    ('E', 0.9447),
    ('F', 0.8972),
    ('G', 1.0400),
    ('H', 1.0876),
    ('I', 0.5162),
    ('J', 0.8019),
    ('K', 1.0400),
    ('L', 0.8495),
    ('M', 1.1828),
    ('N', 1.0876),
    ('O', 1.0876),
    ('P', 1.0400),
    ('Q', 1.0876),
    ('R', 1.0400),
    ('S', 0.9924),
    ('T', 0.8019),
    ('U', 1.0876),
    ('V', 0.8972),
    ('W', 1.1828),
    ('X', 0.9924),
    ('Y', 0.8972),
    ('Z', 0.9924),
    ('[', 0.7067),
    ('\\', 0.7067),
    (']', 0.7067),
    ('^', 0.6114),
    ('_', 0.8019),
    ('`', 0.4209),
    ('a', 0.9447),
    ('b', 0.9447),
    ('c', 0.8972),
    ('d', 0.9447),
    ('e', 0.8972),
    ('f', 0.6114),
    ('g', 0.9447),
    ('h', 0.9447),
    ('i', 0.5162),
    ('j', 0.5162),
    ('k', 0.8495),
    ('l', 0.5638),
    ('m', 1.3733),
    ('n', 0.9447),
    ('o', 0.9447),
    ('p', 0.9447),
    ('q', 0.9447),
    ('r', 0.6591),
    ('s', 0.8495),
    ('t', 0.6114),
    ('u', 0.9447),
    ('v', 0.8019),
    ('w', 1.0876),
    ('x', 0.8495),
    ('y', 0.8019),
    ('z', 0.8495),
    ('{', 0.7067),
    ('|', 0.9924),
    ('}', 0.7067),
    ('~', 0.7543),
];

/// Advance of a glyph the table does not list (`µ`, `Ω`, accented letters):
/// the mean of the measured table, which errs wide.
const UNKNOWN_ADVANCE: f64 = 0.893;

/// Ink top of one text line, in units of the font size, measured from the
/// anchor of a symbol PROPERTY. The line is exactly one size tall and the
/// three justifications are one 0.585-size step apart, which is why only the
/// tops are listed.
const BAND_TOP: [f64; 3] = [0.041, -0.544, -1.129];

/// Slack added around every band, for the stroke KiCAD paints the glyphs with.
const BAND_MARGIN: f64 = 0.06;

/// How far a glyph's ink climbs above / drops below the standard line, in
/// units of the font size. Measured one glyph per cell off a rendered sheet;
/// everything not listed stays inside the line.
const ASCENT_EXTRA: [(f64, &str); 3] = [(0.142, "$(){}"), (0.095, "#[\\]|"), (0.047, "/4^`")];
const DESCENT_EXTRA: [(f64, &str); 6] = [
    (0.381, "(){}"),
    (0.334, "[]gjpqy|"),
    (0.239, "/"),
    (0.191, "#\\"),
    (0.143, "$,;@"),
    (0.096, "Q_"),
];

/// The reach of `text` past the standard line, in units of the font size.
fn reach(text: &str) -> (f64, f64) {
    let worst = |table: &[(f64, &str)]| {
        table
            .iter()
            .filter(|(_, set)| text.chars().any(|c| set.contains(c)))
            .map(|(extra, _)| *extra)
            .fold(0.0, f64::max)
    };
    (worst(&ASCENT_EXTRA), worst(&DESCENT_EXTRA))
}

/// How far each kind of text lifts its ink off the anchor a property uses.
/// A `(text …)` note and a `(label …)` sit higher than a field on the same
/// point; a global label's pentagon puts its text a touch lower.
const LIFT_NOTE: f64 = -0.241;
const LIFT_LABEL: f64 = -0.25;
const LIFT_PORT: f64 = 0.11;

/// The ink band of pin text, which KiCAD centres on its pin line.
fn pin_band(text: &str, size: f64) -> (f64, f64) {
    let (ascent, descent) = reach(text);
    (
        (-0.5 - ascent - BAND_MARGIN) * size,
        (0.5 + descent + BAND_MARGIN) * size,
    )
}

/// A `(global_label …)`'s text starts this far along its reading direction:
/// the pentagon's lead-in.
const GLOBAL_LEAD: f64 = 1.28;

/// Baseline-to-baseline step of a multi-line note, in units of the font size.
const LINE_PITCH: f64 = 1.6;

/// Distance from a pin's line to the centre of its number (and, for a symbol
/// that draws names outside, of its name).
const PIN_TEXT_GAP: f64 = 0.829;

/// Advance of one glyph, in units of the font size.
pub fn glyph_advance(c: char) -> f64 {
    GLYPH_ADVANCE
        .binary_search_by(|(g, _)| g.cmp(&c))
        .map_or(UNKNOWN_ADVANCE, |i| GLYPH_ADVANCE[i].1)
}

/// Width of `text` rendered at `size` — the sum of its glyph advances.
pub fn advance(text: &str, size: f64) -> f64 {
    text.chars().map(glyph_advance).sum::<f64>() * size
}

/// Width of `text` at the default [`FONT_SIZE`].
pub fn text_width(text: &str) -> f64 {
    advance(text, FONT_SIZE)
}

/// Horizontal justification: which side of the anchor the text runs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HJust {
    Left,
    Center,
    Right,
}

/// Vertical justification. KiCAD's default — no token in `(justify …)` — is
/// [`VJust::Center`], which is what the writer emits for symbol fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum VJust {
    Top = 0,
    Center = 1,
    Bottom = 2,
}

/// The `(shape …)` of a global or hierarchical label, which sets how far its
/// pentagon runs past the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelShape {
    Input,
    Output,
    Bidirectional,
    TriState,
    Passive,
}

impl LabelShape {
    /// Pentagon length past the text advance, in units of the font size.
    /// Only `bidirectional` is measured — it is the shape the writer emits —
    /// and the others scale from KiCAD's own ratios.
    fn pad(self) -> f64 {
        match self {
            LabelShape::Input | LabelShape::Output => 1.765,
            LabelShape::Bidirectional | LabelShape::TriState => 2.640,
            LabelShape::Passive => 0.891,
        }
    }
}

/// What a drawn text is, for exemption decisions in a lint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    /// A visible symbol property (Reference, Value, or any other shown field).
    Field,
    /// A local `(label …)`.
    Label,
    /// A `(global_label …)` or `(hierarchical_label …)`: the text inside it.
    PortLabel,
    /// A free `(text …)` note.
    FreeText,
    PinName,
    PinNumber,
}

impl TextKind {
    /// Whether KiCAD draws this text as part of a symbol's pin, which is the
    /// library's layout rather than the sheet's: two of a symbol's own pin
    /// texts touching is not something a placement can fix.
    pub fn is_pin_text(self) -> bool {
        matches!(self, TextKind::PinName | TextKind::PinNumber)
    }
}

/// One piece of text the sheet renders, with the box it renders into.
#[derive(Debug, Clone, PartialEq)]
pub struct DrawnText {
    /// Owning refdes, where the text belongs to a symbol.
    pub owner: Option<String>,
    pub kind: TextKind,
    pub text: String,
    pub bbox: Rect,
}

/// Place a box given in the text's own frame (`u` along the reading direction,
/// `v` across it) onto the sheet, rotated by KiCAD's `angle` about `anchor`.
///
/// KiCAD angles run counter-clockwise while sheet y grows downward, so the
/// screen rotation is by `-angle`.
fn place(anchor: Point2, angle: f64, (u0, v0, u1, v1): (f64, f64, f64, f64)) -> Rect {
    let (sin, cos) = (-angle).to_radians().sin_cos();
    let corner =
        |u: f64, v: f64| Point2::new(anchor.x + u * cos - v * sin, anchor.y + u * sin + v * cos);
    let pts = [
        corner(u0, v0),
        corner(u0, v1),
        corner(u1, v0),
        corner(u1, v1),
    ];
    Rect::bounding(&pts).expect("four corners bound a rect")
}

/// The span the text occupies along its reading direction, per justification.
fn along(width: f64, hjust: HJust) -> (f64, f64) {
    match hjust {
        HJust::Left => (0.0, width),
        HJust::Center => (-width / 2.0, width / 2.0),
        HJust::Right => (-width, 0.0),
    }
}

/// The ink band `text` occupies across its reading direction.
fn across(text: &str, size: f64, vjust: VJust, lift: f64) -> (f64, f64) {
    let top = BAND_TOP[vjust as usize] + lift;
    let (ascent, descent) = reach(text);
    (
        (top - ascent - BAND_MARGIN) * size,
        (top + 1.0 + descent + BAND_MARGIN) * size,
    )
}

/// The box a plain text — a symbol field, a free note — draws into.
///
/// `angle` is the angle KiCAD *draws* at. For a symbol property that is
/// `(symbol_angle + field_angle) mod 180`: a property's angle is relative to
/// its symbol, and KiCAD auto-flips text past 180° to keep it readable.
pub fn drawn_box(
    text: &str,
    size: f64,
    hjust: HJust,
    vjust: VJust,
    angle: f64,
    anchor: Point2,
) -> Rect {
    lifted_box(text, size, hjust, vjust, 0.0, angle, anchor)
}

/// The box a free `(text …)` note draws into. A note sits higher on its
/// anchor than a symbol property does, by [`LIFT_NOTE`].
///
/// A note may carry newlines. The box is as wide as its widest line and as
/// tall as the stack, which grows away from the justified edge: a
/// bottom-justified note keeps its LAST line on the anchor and piles the rest
/// above it, a top-justified one the other way, a centred one splits.
pub fn note_box(
    text: &str,
    size: f64,
    hjust: HJust,
    vjust: VJust,
    angle: f64,
    anchor: Point2,
) -> Rect {
    let lines: Vec<&str> = text.split('\n').collect();
    let width = lines
        .iter()
        .map(|line| advance(line, size))
        .fold(0.0, f64::max);
    let (u0, u1) = along(width, hjust);
    let (v0, v1) = across(text, size, vjust, LIFT_NOTE);
    let stack = (lines.len() - 1) as f64 * LINE_PITCH * size;
    let (up, down) = match vjust {
        VJust::Bottom => (stack, 0.0),
        VJust::Top => (0.0, stack),
        VJust::Center => (stack / 2.0, stack / 2.0),
    };
    place(anchor, angle, (u0, v0 - up, u1, v1 + down))
}

fn lifted_box(
    text: &str,
    size: f64,
    hjust: HJust,
    vjust: VJust,
    lift: f64,
    angle: f64,
    anchor: Point2,
) -> Rect {
    let (u0, u1) = along(advance(text, size), hjust);
    let (v0, v1) = across(text, size, vjust, lift);
    place(anchor, angle, (u0, v0, u1, v1))
}

/// The box a local `(label …)` draws into: [`drawn_box`] shifted by the
/// standoff that floats label text clear of the wire it names.
pub fn local_label_box(
    text: &str,
    size: f64,
    hjust: HJust,
    vjust: VJust,
    angle: f64,
    anchor: Point2,
) -> Rect {
    lifted_box(text, size, hjust, vjust, LIFT_LABEL, angle, anchor)
}

/// The box the TEXT of a `(global_label …)` / `(hierarchical_label …)` draws
/// into — the glyphs only, not the pentagon around them (see
/// [`port_label_outline`]).
pub fn port_label_text_box(
    text: &str,
    size: f64,
    hjust: HJust,
    angle: f64,
    anchor: Point2,
) -> Rect {
    let (u0, u1) = along(advance(text, size), hjust);
    let lead = if hjust == HJust::Right {
        -GLOBAL_LEAD * size
    } else {
        GLOBAL_LEAD * size
    };
    let (v0, v1) = across(text, size, VJust::Center, LIFT_PORT);
    place(anchor, angle, (u0 + lead, v0, u1 + lead, v1))
}

/// The pentagon a `(global_label …)` / `(hierarchical_label …)` draws around
/// its text: half-height one font size, running `advance + shape padding` from
/// the anchor along the reading direction.
pub fn port_label_outline(
    text: &str,
    size: f64,
    shape: LabelShape,
    hjust: HJust,
    angle: f64,
    anchor: Point2,
) -> Rect {
    let length = advance(text, size) + shape.pad() * size;
    let (u0, u1) = if hjust == HJust::Right {
        (-length, 0.0)
    } else {
        (0.0, length)
    };
    place(anchor, angle, (u0, -size, u1, size))
}

/// How the writer orients a label reading away from the body along `dir`:
/// the DRAWN angle and horizontal justification, matching `write::emit`.
///
/// The writer emits 0/180/90/270; KiCAD folds a text angle into `[0, 180)` so
/// it never reads upside down, which is why West and South come back as their
/// folded angle with the justification carrying the direction.
pub fn label_pose(dir: Dir) -> (f64, HJust) {
    match dir {
        Dir::East => (0.0, HJust::Left),
        Dir::West => (0.0, HJust::Right),
        Dir::North => (90.0, HJust::Left),
        Dir::South => (90.0, HJust::Right),
    }
}

/// Box of a local net label the writer emits at `at` reading along `dir`.
pub fn label_box(at: impl Into<Point2>, dir: Dir, text: &str) -> Rect {
    let (angle, hjust) = label_pose(dir);
    local_label_box(text, FONT_SIZE, hjust, VJust::Bottom, angle, at.into())
}

/// Box of a global label (a port) the writer emits at `at` reading along
/// `dir`: the pentagon, which is what the eye and the page see.
pub fn global_label_box(at: impl Into<Point2>, dir: Dir, text: &str) -> Rect {
    let (angle, hjust) = label_pose(dir);
    port_label_outline(
        text,
        FONT_SIZE,
        LabelShape::Bidirectional,
        hjust,
        angle,
        at.into(),
    )
}

/// A pin as the sheet places it: where the model measures its text from.
pub struct DrawnPin<'a> {
    /// Connection point (the tip a wire attaches to), sheet space.
    pub tip: Point2,
    /// Unit vector pointing away from the symbol body.
    pub out: Point2,
    pub length: f64,
    pub name: &'a str,
    pub number: &'a str,
    pub style: PinTextStyle,
}

/// Boxes of a placed pin's rendered NAME and NUMBER text.
///
/// KiCAD draws the number centred on the pin line's midpoint, one
/// [`PIN_TEXT_GAP`] off the line on the reading frame's "up" side (screen −y
/// for a horizontal pin, −x for a vertical one). Where the symbol sets a
/// positive `pin_names` offset the name is drawn *inside* the body, that far
/// past the pin's body end and centred on the pin axis; where the offset is
/// zero the name takes the up side and the number moves to the other. Hidden
/// pins, hidden names and hidden numbers draw nothing.
pub fn placed_pin_texts(pin: &DrawnPin) -> Vec<(TextKind, Rect)> {
    let mut out = Vec::new();
    if pin.style.pin_hidden {
        return out;
    }
    let horizontal = pin.out.x.abs() > pin.out.y.abs();
    let step = |d: f64| Point2::new(pin.tip.x - d * pin.out.x, pin.tip.y - d * pin.out.y);
    let mid = step(0.5 * pin.length);
    // A band of text centred on the pin line, `side` picking which flank.
    let banded = |centre: Point2, text: &str, size: f64, side: f64| {
        let half = advance(text, size) / 2.0;
        let off = side * PIN_TEXT_GAP * size;
        let (up, down) = pin_band(text, size);
        if horizontal {
            Rect::new(
                centre.x - half,
                centre.y + off + up,
                centre.x + half,
                centre.y + off + down,
            )
        } else {
            Rect::new(
                centre.x + off + up,
                centre.y - half,
                centre.x + off + down,
                centre.y + half,
            )
        }
    };
    let names_inside = pin.style.name_offset > 0.0;
    if !pin.style.numbers_hidden && !pin.number.is_empty() {
        let side = if names_inside { -1.0 } else { 1.0 };
        out.push((
            TextKind::PinNumber,
            banded(mid, pin.number, pin.style.number_size, side),
        ));
    }
    let named = !pin.name.is_empty() && pin.name != "~";
    if named && !pin.style.names_hidden {
        let size = pin.style.name_size;
        if names_inside {
            let anchor = step(pin.length + pin.style.name_offset);
            let width = advance(pin.name, size);
            let (up, down) = pin_band(pin.name, size);
            // The name reads on INTO the body, away from the pin tip.
            let rect = if horizontal {
                let (x0, x1) = if pin.out.x > 0.0 {
                    (anchor.x - width, anchor.x)
                } else {
                    (anchor.x, anchor.x + width)
                };
                Rect::new(x0, anchor.y + up, x1, anchor.y + down)
            } else {
                let (y0, y1) = if pin.out.y > 0.0 {
                    (anchor.y - width, anchor.y)
                } else {
                    (anchor.y, anchor.y + width)
                };
                Rect::new(anchor.x + up, y0, anchor.x + down, y1)
            };
            out.push((TextKind::PinName, rect));
        } else {
            out.push((TextKind::PinName, banded(mid, pin.name, size, -1.0)));
        }
    }
    out
}

/// [`placed_pin_texts`] for a library pin placed by an instance transform.
pub fn pin_texts(
    pin: &PinGeom,
    inst_at: Point2,
    inst_angle: f64,
    inst_mirror: bool,
) -> Vec<(TextKind, Rect)> {
    let to_sheet = |local: Point2| {
        let off = local.transform_offset(inst_angle, inst_mirror);
        Point2::new(inst_at.x + off.x, inst_at.y + off.y)
    };
    // A library pin's angle points INTO the body, so a wire leaves along its
    // opposite.
    let (sin, cos) = (pin.angle + 180.0).to_radians().sin_cos();
    let out_dir = Point2::new(cos, sin).transform_offset(inst_angle, inst_mirror);
    placed_pin_texts(&DrawnPin {
        tip: to_sheet(pin.at),
        out: out_dir,
        length: pin.length,
        name: &pin.name,
        number: &pin.number,
        style: pin.text,
    })
}

/// [`pin_texts`] without the kinds — what an obstacle set needs.
pub fn pin_text_boxes(
    pin: &PinGeom,
    inst_at: Point2,
    inst_angle: f64,
    inst_mirror: bool,
) -> Vec<Rect> {
    pin_texts(pin, inst_at, inst_angle, inst_mirror)
        .into_iter()
        .map(|(_, rect)| rect)
        .collect()
}

/// Thin obstacle box around a wire segment (inflated 0.13 mm).
pub fn wire_box(a: Point2, b: Point2) -> Rect {
    Rect::from_points(a, b).inflate(0.13)
}

/// What a piece of geometry belongs to. An obstacle and a movable that name the
/// same owner belong together, so the movable is exempt from that obstacle.
#[derive(Clone, PartialEq, Eq)]
pub enum Owner {
    /// A symbol, by refdes: its body, and the fields and pin labels that ride it
    /// (a label on its own pin endpoint sits inside the body's generous bbox).
    Symbol(String),
    /// A net, by name: its wires, and the labels anchored on them.
    Net(String),
}

/// Fixed geometry a movable must not collide with.
pub struct Obstacle {
    pub bbox: Rect,
    /// `None` for geometry nothing is exempt from: pin text, no-connects, fixed labels.
    pub owner: Option<Owner>,
}

/// One piece of movable text with its candidate boxes in preference order.
pub struct Movable {
    /// What this text belongs to; obstacles with the same owner do not block it.
    pub owner: Option<Owner>,
    /// Candidate bboxes, best-first. Never empty.
    pub candidates: Vec<Rect>,
}

/// One movable's outcome: which candidate box it took, and whether that box was actually
/// free (a `false` fit means every candidate collided and the caller must degrade the
/// text — lint it, or hide it when it is optional).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pick {
    pub candidate: usize,
    pub fits: bool,
}

/// A schematic TEXT solver: the leaf that seats refdes/value fields and net labels.
///
/// ## Contract
/// - **Deterministic and order-respecting.** Movables are solved in the order given
///   (callers pass most-constrained first); the same input always yields the same picks.
/// - **Total.** Exactly one [`Pick`] per movable, always with a valid candidate index —
///   a movable with no free spot falls back to candidate 0 with `fits: false`.
/// - **Pure.** No I/O, no KiCAD environment.
pub trait TextSolver {
    /// Open provenance: the solver's stable name (e.g. `"greedy"`).
    fn name(&self) -> &'static str;

    /// Seat every movable clear of the obstacles and of each other.
    fn solve(&self, obstacles: &[Obstacle], movables: &[Movable]) -> Vec<Pick>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(got: Rect, want: Rect) {
        for (g, w) in [
            (got.min_x, want.min_x),
            (got.min_y, want.min_y),
            (got.max_x, want.max_x),
            (got.max_y, want.max_y),
        ] {
            assert!((g - w).abs() < 1e-6, "got {got:?}, want {want:?}");
        }
    }

    /// The table is sorted, which is what makes the binary search valid.
    #[test]
    fn glyph_table_is_sorted_and_covers_the_printable_ascii() {
        assert!(GLYPH_ADVANCE.windows(2).all(|w| w[0].0 < w[1].0));
        for c in ('a'..='z')
            .chain('A'..='Z')
            .chain('0'..='9')
            .chain("_-+./#()".chars())
        {
            assert!(
                GLYPH_ADVANCE.iter().any(|(g, _)| *g == c),
                "no measured advance for {c:?}"
            );
        }
        // The stroke font is proportional, and the model must not flatten it:
        // `M` is more than twice `i`, and the space is narrower than any letter.
        assert!(glyph_advance('M') > 2.0 * glyph_advance('i'));
        assert!(glyph_advance(' ') < glyph_advance('a'));
        // Anything unmeasured — `µ`, `Ω` — errs wide.
        assert_eq!(glyph_advance('µ'), UNKNOWN_ADVANCE);
    }

    /// Ink reach is per glyph: a string of parentheses is visibly taller than
    /// one of capitals, and a box that ignores that clips the render.
    #[test]
    fn descenders_and_brackets_deepen_the_box() {
        let at = Point2::new(0.0, 0.0);
        let plain = drawn_box("ABC", FONT_SIZE, HJust::Left, VJust::Center, 0.0, at);
        let deep = drawn_box("A(g)", FONT_SIZE, HJust::Left, VJust::Center, 0.0, at);
        assert!(deep.max_y > plain.max_y + 0.4, "{deep:?} vs {plain:?}");
        assert!(deep.min_y < plain.min_y - 0.1, "{deep:?} vs {plain:?}");
        // A string with neither keeps the plain band.
        let same = drawn_box("XYZ", FONT_SIZE, HJust::Left, VJust::Center, 0.0, at);
        assert!((same.max_y - plain.max_y).abs() < 1e-9);
    }

    /// A note stacks its lines downward and is as wide as its widest line.
    #[test]
    fn a_multi_line_note_stacks_downward() {
        let at = Point2::new(0.0, 0.0);
        let one = note_box("SHORT", FONT_SIZE, HJust::Left, VJust::Bottom, 0.0, at);
        let two = note_box(
            "SHORT\nA MUCH LONGER LINE",
            FONT_SIZE,
            HJust::Left,
            VJust::Bottom,
            0.0,
            at,
        );
        assert!(
            (two.max_y - one.max_y).abs() < 1e-9,
            "the last line stays on the anchor"
        );
        assert!(two.min_y < one.min_y - 1.9, "the first line piles above it");
        assert!(two.width() > one.width(), "the widest line sets the width");
    }

    /// A field carries no vertical justify token, so KiCAD straddles the
    /// anchor with it; the horizontal token decides which way it runs.
    #[test]
    fn field_text_straddles_its_anchor() {
        let at = Point2::new(10.0, 20.0);
        let left = drawn_box("R1", FONT_SIZE, HJust::Left, VJust::Center, 0.0, at);
        assert!((left.min_x - 10.0).abs() < 1e-9);
        assert!((left.width() - text_width("R1")).abs() < 1e-9);
        assert!(left.min_y < 20.0 && left.max_y > 20.0, "{left:?}");
        let right = drawn_box("R1", FONT_SIZE, HJust::Right, VJust::Center, 0.0, at);
        assert!((right.max_x - 10.0).abs() < 1e-9);
        let centre = drawn_box("R1", FONT_SIZE, HJust::Center, VJust::Center, 0.0, at);
        assert!((centre.center().x - 10.0).abs() < 1e-9);
        // The three justifications sit one line-step apart vertically.
        let bottom = drawn_box("R1", FONT_SIZE, HJust::Left, VJust::Bottom, 0.0, at);
        let top = drawn_box("R1", FONT_SIZE, HJust::Left, VJust::Top, 0.0, at);
        assert!(bottom.max_y < left.max_y && left.max_y < top.max_y);
    }

    /// A 90° text reads bottom-to-top: the advance lies along -y.
    #[test]
    fn rotation_turns_the_advance_onto_the_other_axis() {
        let flat = drawn_box(
            "ABC",
            FONT_SIZE,
            HJust::Left,
            VJust::Center,
            0.0,
            Point2::new(0.0, 0.0),
        );
        let turned = drawn_box(
            "ABC",
            FONT_SIZE,
            HJust::Left,
            VJust::Center,
            90.0,
            Point2::new(0.0, 0.0),
        );
        assert!((turned.height() - flat.width()).abs() < 1e-9);
        assert!((turned.width() - flat.height()).abs() < 1e-9);
        assert!(
            turned.max_y <= 1e-9 && turned.min_y < 0.0,
            "reads upward: {turned:?}"
        );
    }

    /// A local label floats off its anchor, away from the wire it names — the
    /// 0.4 mm standoff that keeps a label's ink clear of its own wire, and the
    /// millimetre the old centred box was wrong by.
    #[test]
    fn local_label_floats_off_its_wire() {
        let at = Point2::new(0.0, 0.0);
        let field = drawn_box("NET", FONT_SIZE, HJust::Left, VJust::Bottom, 0.0, at);
        let label = local_label_box("NET", FONT_SIZE, HJust::Left, VJust::Bottom, 0.0, at);
        assert!(
            label.max_y < field.max_y,
            "a label rides higher than a field"
        );
        assert!(
            label.max_y < 0.0,
            "its ink never reaches the wire at the anchor"
        );
        assert!(
            (label.height() - field.height()).abs() < 1e-9,
            "same line, same height"
        );
    }

    /// The writer emits every port as a `bidirectional` global label, whose
    /// pentagon runs 3.353 mm past the text.
    #[test]
    fn port_pentagon_extends_past_its_text() {
        let text = "USB_DP";
        let outline = port_label_outline(
            text,
            FONT_SIZE,
            LabelShape::Bidirectional,
            HJust::Left,
            0.0,
            Point2::new(0.0, 0.0),
        );
        close(
            outline,
            Rect::new(
                0.0,
                -FONT_SIZE,
                text_width(text) + 2.640 * FONT_SIZE,
                FONT_SIZE,
            ),
        );
        // The glyphs sit inside it, starting past the pentagon's lead-in.
        let inner = port_label_text_box(text, FONT_SIZE, HJust::Left, 0.0, Point2::new(0.0, 0.0));
        assert!(inner.min_x > outline.min_x && inner.max_x < outline.max_x);
        assert!(inner.min_y > outline.min_y && inner.max_y < outline.max_y);
    }

    /// A pin's number rides above its line at the midpoint; its name sits
    /// inside the body, past the pin's body end.
    #[test]
    fn pin_number_rides_the_line_and_the_name_sits_inside() {
        // A west-side pin: tip at (0,0), body to the east, 2.54 long.
        let pin = DrawnPin {
            tip: Point2::new(0.0, 0.0),
            out: Point2::new(-1.0, 0.0),
            length: 2.54,
            name: "RST",
            number: "4",
            style: PinTextStyle::default(),
        };
        let boxes = placed_pin_texts(&pin);
        assert_eq!(boxes.len(), 2);
        let (kind, number) = boxes[0];
        assert_eq!(kind, TextKind::PinNumber);
        // Centred on the midpoint (1.27, 0), one gap ABOVE the line.
        assert!((number.center().x - 1.27).abs() < 1e-9);
        assert!((number.width() - text_width("4")).abs() < 1e-9);
        assert!(
            number.contains(Point2::new(1.27, -PIN_TEXT_GAP * FONT_SIZE)),
            "the number's line sits one gap above the pin: {number:?}"
        );
        let (kind, name) = boxes[1];
        assert_eq!(kind, TextKind::PinName);
        // Body end at x = 2.54, plus the 0.508 name offset, running on east.
        assert!((name.min_x - 3.048).abs() < 1e-9, "{name:?}");
        assert!((name.width() - text_width("RST")).abs() < 1e-9);
        assert!(
            name.min_y < 0.0 && name.max_y > 0.0,
            "centred on the pin axis"
        );
    }

    /// With `pin_names` offset 0 the name goes OUTSIDE — it takes the line's
    /// upper side and pushes the number to the other.
    #[test]
    fn zero_name_offset_puts_the_name_outside() {
        let pin = DrawnPin {
            tip: Point2::new(0.0, 0.0),
            out: Point2::new(-1.0, 0.0),
            length: 2.54,
            name: "G",
            number: "7",
            style: PinTextStyle {
                name_offset: 0.0,
                ..PinTextStyle::default()
            },
        };
        let boxes = placed_pin_texts(&pin);
        assert_eq!(boxes.len(), 2);
        assert!(boxes[0].1.min_y > 0.0, "number below the line");
        assert!(boxes[1].1.max_y < 0.0, "name above the line");
    }

    /// Hidden pins, hidden names and hidden numbers draw nothing.
    #[test]
    fn hidden_pin_text_is_not_drawn() {
        let base = DrawnPin {
            tip: Point2::new(0.0, 0.0),
            out: Point2::new(0.0, 1.0),
            length: 2.54,
            name: "GND",
            number: "1",
            style: PinTextStyle::default(),
        };
        assert!(
            placed_pin_texts(&DrawnPin {
                style: PinTextStyle {
                    pin_hidden: true,
                    ..PinTextStyle::default()
                },
                ..base
            })
            .is_empty()
        );
        let quiet = placed_pin_texts(&DrawnPin {
            style: PinTextStyle {
                names_hidden: true,
                numbers_hidden: true,
                ..PinTextStyle::default()
            },
            ..base
        });
        assert!(quiet.is_empty());
        // An unnamed pin still shows its number.
        let unnamed = placed_pin_texts(&DrawnPin { name: "~", ..base });
        assert_eq!(unnamed.len(), 1);
        assert_eq!(unnamed[0].0, TextKind::PinNumber);
    }

    /// A vertical pin's text turns with it: the number takes the line's left.
    #[test]
    fn vertical_pin_text_turns_with_the_pin() {
        let pin = DrawnPin {
            tip: Point2::new(0.0, 0.0),
            out: Point2::new(0.0, 1.0),
            length: 2.54,
            name: "~",
            number: "12",
            style: PinTextStyle::default(),
        };
        let boxes = placed_pin_texts(&pin);
        assert_eq!(boxes.len(), 1);
        assert!(boxes[0].1.max_x < 0.0, "number left of a vertical pin line");
        assert!(
            boxes[0].1.height() > boxes[0].1.width(),
            "text reads vertically"
        );
    }

    /// The writer's four label orientations read away from the body, so a
    /// label's box always extends in the direction its stub points.
    #[test]
    fn a_label_reads_along_its_stub() {
        let north = label_box(Point2::new(0.0, 0.0), Dir::North, "NET");
        assert!(
            north.max_y <= 1e-9 && north.min_y < 0.0,
            "reads north: {north:?}"
        );
        let south = label_box(Point2::new(0.0, 0.0), Dir::South, "NET");
        assert!(
            south.min_y >= -1e-9 && south.max_y > 0.0,
            "reads south: {south:?}"
        );
        let east = label_box(Point2::new(0.0, 0.0), Dir::East, "NET");
        assert!(
            east.min_x >= -1e-9 && east.max_x > 0.0,
            "reads east: {east:?}"
        );
        let west = label_box(Point2::new(0.0, 0.0), Dir::West, "NET");
        assert!(
            west.max_x <= 1e-9 && west.min_x < 0.0,
            "reads west: {west:?}"
        );
    }
}
