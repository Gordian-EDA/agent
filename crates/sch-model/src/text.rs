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
//! each string's exact advance (`textLength`) and its exact ink (a
//! `stroked-text` path group). Fitting those over 1868 texts on seven placed
//! sheets gives the numbers below, all in units of the font size except where
//! stated. The resulting boxes contain the drawn ink with a median slack of
//! +0.13 mm and a worst case of −0.41 mm, and they reproduce **103 of 103**
//! ink-level text overlaps on those sheets.

use geom::{Dir, Point2, Rect};
use kicad_symbol::geometry::{PinGeom, PinTextStyle};

/// KiCAD's default schematic font size (mm).
pub const FONT_SIZE: f64 = 1.27;

/// Per-glyph advance, in units of the font size, for the KiCAD stroke font.
/// Sorted by character so [`glyph_advance`] can binary-search it.
const GLYPH_ADVANCE: [(char, f64); 91] = [
    ('!', 0.5162),
    ('"', 0.8019),
    ('#', 1.0400),
    ('$', 0.9924),
    ('%', 1.1828),
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
    ('=', 1.2781),
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

/// Advance of a glyph the table does not list (accented letters, `µ`, `Ω`,
/// the space): the mean of the measured table, which errs wide.
const UNKNOWN_ADVANCE: f64 = 0.893;

/// Half the ink height, plus a hair of margin: KiCAD's ink is exactly one font
/// size tall, and 0.056 covers the stroke's outer half (stroke = 0.12·size).
const HALF_HEIGHT: f64 = 0.556;

/// Ink band of `justify bottom` text, relative to its anchor.
const BOTTOM_BAND: (f64, f64) = (-1.13, 0.06);

/// A local `(label …)` floats this far off its anchor, away from the wire it
/// names — the standoff that keeps a label off its own wire.
const LABEL_STANDOFF: f64 = 0.319;

/// A `(global_label …)`'s text starts this far along its reading direction:
/// the pentagon's lead-in.
const GLOBAL_LEAD: f64 = 1.394;

/// A `(global_label …)`'s text centre sits this far past the anchor, across
/// the reading direction.
const GLOBAL_CROSS_SHIFT: f64 = 0.076;

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
pub enum VJust {
    Top,
    Center,
    Bottom,
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
    /// Pentagon length past the text advance, in mm.
    fn pad(self) -> f64 {
        match self {
            LabelShape::Input | LabelShape::Output => 2.242,
            LabelShape::Bidirectional | LabelShape::TriState => 3.353,
            LabelShape::Passive => 1.131,
        }
    }

    /// Decode a KiCAD `(shape …)` token, defaulting to `input`.
    pub fn parse(token: &str) -> Self {
        match token {
            "output" => LabelShape::Output,
            "bidirectional" => LabelShape::Bidirectional,
            "tri_state" => LabelShape::TriState,
            "passive" => LabelShape::Passive,
            _ => LabelShape::Input,
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
    let corner = |u: f64, v: f64| {
        Point2::new(
            anchor.x + u * cos - v * sin,
            anchor.y + u * sin + v * cos,
        )
    };
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

/// The ink band across the reading direction, per justification.
fn across(size: f64, vjust: VJust) -> (f64, f64) {
    match vjust {
        VJust::Center => (-HALF_HEIGHT * size, HALF_HEIGHT * size),
        VJust::Bottom => (BOTTOM_BAND.0 * size, BOTTOM_BAND.1 * size),
        VJust::Top => (-BOTTOM_BAND.1 * size, -BOTTOM_BAND.0 * size),
    }
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
    let (u0, u1) = along(advance(text, size), hjust);
    let (v0, v1) = across(size, vjust);
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
    let (u0, u1) = along(advance(text, size), hjust);
    let (v0, v1) = across(size, vjust);
    let shift = -LABEL_STANDOFF * size;
    place(anchor, angle, (u0, v0 + shift, u1, v1 + shift))
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
    let shift = GLOBAL_CROSS_SHIFT * size;
    place(
        anchor,
        angle,
        (
            u0 + lead,
            -HALF_HEIGHT * size + shift,
            u1 + lead,
            HALF_HEIGHT * size + shift,
        ),
    )
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
    let length = advance(text, size) + shape.pad();
    let (u0, u1) = if hjust == HJust::Right {
        (-length, 0.0)
    } else {
        (0.0, length)
    };
    place(anchor, angle, (u0, -size, u1, size))
}

/// How the writer orients a label reading away from the body along `dir`:
/// the drawn angle and horizontal justification, matching `write::emit`.
pub fn label_pose(dir: Dir) -> (f64, HJust) {
    match dir {
        Dir::East => (0.0, HJust::Left),
        Dir::West => (180.0, HJust::Right),
        Dir::North => (90.0, HJust::Left),
        Dir::South => (270.0, HJust::Right),
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
    let step = |d: f64| {
        Point2::new(pin.tip.x - d * pin.out.x, pin.tip.y - d * pin.out.y)
    };
    let mid = step(0.5 * pin.length);
    // A band of text centred on the pin line, `side` picking which flank.
    let banded = |centre: Point2, text: &str, size: f64, side: f64| {
        let half = advance(text, size) / 2.0;
        let off = side * PIN_TEXT_GAP * size;
        let cross = HALF_HEIGHT * size;
        if horizontal {
            Rect::new(centre.x - half, centre.y + off - cross, centre.x + half, centre.y + off + cross)
        } else {
            Rect::new(centre.x + off - cross, centre.y - half, centre.x + off + cross, centre.y + half)
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
            let cross = HALF_HEIGHT * size;
            // The name reads on INTO the body, away from the pin tip.
            let rect = if horizontal {
                let (x0, x1) = if pin.out.x > 0.0 {
                    (anchor.x - width, anchor.x)
                } else {
                    (anchor.x, anchor.x + width)
                };
                Rect::new(x0, anchor.y - cross, x1, anchor.y + cross)
            } else {
                let (y0, y1) = if pin.out.y > 0.0 {
                    (anchor.y - width, anchor.y)
                } else {
                    (anchor.y, anchor.y + width)
                };
                Rect::new(anchor.x - cross, y0, anchor.x + cross, y1)
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

/// Fixed geometry a movable must not collide with.
pub enum ObKind {
    /// A symbol body, exempted for text OWNED by that refdes (a label on its
    /// own pin endpoint legitimately sits inside its symbol's generous bbox).
    OwnExempt(String),
    /// Never exempted: pin text, wires, fixed labels, no-connects.
    Hard,
}

pub struct Obstacle {
    pub bbox: Rect,
    pub kind: ObKind,
}

/// One piece of movable text with its candidate boxes in preference order.
pub struct Movable {
    /// Owning refdes, matched against [`ObKind::OwnExempt`].
    pub owner: Option<String>,
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
