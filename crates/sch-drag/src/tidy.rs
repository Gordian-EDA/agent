//! "Drag it around until it looks cleanest", as a search.
//!
//! Every step is a drag a person could have made — nudge, rotate, mirror, line
//! a pin up with the pin it connects to, swap two parts, carry a chip and its
//! decoupling caps somewhere else in one piece, snap a scattered bank onto one
//! column. A step is kept when the sheet measures better and, at the annealing
//! temperature, sometimes when it does not, so the search can climb out of a
//! tidy-looking dead end. A step that would break the drawing cannot be kept at
//! all: [`crate::drag`] rolls itself back before the evaluator sees it.
//!
//! The temperature is calibrated from the moves themselves. A schematic's score
//! is mostly made of terms no single drag can touch, so scaling the temperature
//! by the score — the obvious thing — accepts everything and searches nothing.

use std::collections::HashMap;
use std::time::Instant;

use geom::{Point2, Rect};
use sch_doc::{Mirror, SchDoc};

use crate::drag::{DragError, Placement, drag_many};
use crate::eval::{Metrics, Weights, measure};
use crate::route::snap;
use crate::sheet::Sheet;

/// How long to search, how to score, and where to start the random stream.
#[derive(Debug, Clone)]
pub struct TidyOptions {
    pub seconds: f64,
    pub weights: Weights,
    pub seed: u64,
}

impl Default for TidyOptions {
    fn default() -> Self {
        TidyOptions {
            seconds: 20.0,
            weights: Weights::default(),
            seed: 0x5EED,
        }
    }
}

/// What the search did and what it cost.
#[derive(Debug, Clone, Default)]
pub struct TidyReport {
    pub before: Metrics,
    pub after: Metrics,
    pub proposed: usize,
    pub accepted: usize,
    pub improved: usize,
    /// Steps the drag gate refused — the honest count of how often a
    /// good-looking move would have broken the drawing.
    pub refused: usize,
    /// Steps ruled out before the drag, because the part would have landed on
    /// another one or off the page.
    pub blocked: usize,
    pub seconds: f64,
}

impl TidyReport {
    pub fn gain(&self, w: &Weights) -> f64 {
        self.before.score(w) - self.after.score(w)
    }
}

/// One step of the search: everything it moves, and where to.
type Step = Vec<(String, Placement)>;

const NUDGES: [(f64, f64); 8] = [
    (2.54, 0.0),
    (-2.54, 0.0),
    (0.0, 2.54),
    (0.0, -2.54),
    (7.62, 0.0),
    (-7.62, 0.0),
    (0.0, 7.62),
    (0.0, -7.62),
];

const ROTATIONS: [f64; 4] = [0.0, 90.0, 180.0, 270.0];
const MIRRORS: [Mirror; 3] = [Mirror::None, Mirror::X, Mirror::Y];

/// The sheet as one step sees it: who may move, where they are, what they take
/// up, and how far the frame reaches.
struct Board<'a> {
    sheet: &'a Sheet,
    movable: &'a [String],
    poses: HashMap<String, Placement>,
    bodies: HashMap<String, Rect>,
    bounds: Rect,
}

impl Board<'_> {
    fn choose<'b, T>(&self, rng: &mut fastrand::Rng, from: &'b [T]) -> Option<&'b T> {
        (!from.is_empty()).then(|| &from[rng.usize(..from.len())])
    }

    /// Where a symbol's body would land, without doing the drag: the same box,
    /// carried and turned. Enough to rule out a pose that lands on another part
    /// before paying for a route.
    fn body_after(&self, id: &str, to: Placement) -> Option<Rect> {
        let (here, rect) = (self.poses.get(id)?, self.bodies.get(id)?);
        let quarter_turn = ((to.rot - here.rot) / 90.0).round() as i64 % 2 != 0;
        let half = if quarter_turn {
            (rect.height() / 2.0, rect.width() / 2.0)
        } else {
            (rect.width() / 2.0, rect.height() / 2.0)
        };
        let centre = Point2::new(
            rect.center().x + to.at.x - here.at.x,
            rect.center().y + to.at.y - here.at.y,
        );
        Some(Rect::from_center_half(centre, half))
    }

    /// Whether a step can be ruled out on geometry alone.
    fn obstructed(&self, step: &Step) -> bool {
        let moving: Vec<&String> = step.iter().map(|(id, _)| id).collect();
        step.iter().any(|(id, to)| match self.body_after(id, *to) {
            None => true,
            Some(rect) => {
                !self.bounds.contains_rect_eps(&rect, 0.0)
                    || self
                        .bodies
                        .iter()
                        .any(|(other, box_)| !moving.contains(&other) && box_.overlaps(&rect))
            }
        })
    }

    /// The small parts hanging off one symbol: net neighbours with at most two
    /// pins, close enough to read as belonging to it. A chip and its decoupling
    /// caps travel as a block, the way a person moves them.
    fn block(&self, id: &str) -> Vec<String> {
        let Some(hub) = self.bodies.get(id) else {
            return vec![id.to_string()];
        };
        let nets: Vec<&str> = self
            .sheet
            .pins_of(id)
            .filter_map(|p| self.sheet.net_at(p.at))
            .collect();
        let mut pins: HashMap<&str, usize> = HashMap::new();
        for pin in &self.sheet.pins {
            *pins.entry(pin.owner.as_str()).or_default() += 1;
        }
        let mut block = vec![id.to_string()];
        for other in self.movable {
            let Some(rect) = self.bodies.get(other) else {
                continue;
            };
            if other == id || pins.get(other.as_str()).is_none_or(|n| *n > 2) {
                continue;
            }
            let shares_a_net = self
                .sheet
                .pins_of(other)
                .filter_map(|p| self.sheet.net_at(p.at))
                .any(|net| nets.contains(&net));
            if shares_a_net && hub.dist_to_rect(rect) < 25.4 {
                block.push(other.clone());
            }
        }
        block
    }
}

/// Line one of a symbol's pins up with a pin it shares a net with — the move
/// that turns a dog-leg into a straight wire.
fn alignment(board: &Board, rng: &mut fastrand::Rng, id: &str, here: Placement) -> Option<Step> {
    let mine: Vec<Point2> = board.sheet.pins_of(id).map(|p| p.at).collect();
    let pin = *board.choose(rng, &mine)?;
    let net = board.sheet.net_at(pin)?;
    let partners: Vec<Point2> = board
        .sheet
        .pins
        .iter()
        .filter(|p| p.owner != id && board.sheet.net_at(p.at) == Some(net))
        .map(|p| p.at)
        .collect();
    let target = *board.choose(rng, &partners)?;
    let at = if rng.bool() {
        Point2::new(snap(here.at.x + target.x - pin.x), here.at.y)
    } else {
        Point2::new(here.at.x, snap(here.at.y + target.y - pin.y))
    };
    Some(vec![(id.to_string(), Placement { at, ..here })])
}

/// Put a part beside one of its net partners, which is where it belongs and
/// where a nudge would take a hundred steps to reach.
fn teleport(board: &Board, rng: &mut fastrand::Rng, id: &str, here: Placement) -> Option<Step> {
    let nets: Vec<&str> = board
        .sheet
        .pins_of(id)
        .filter_map(|p| board.sheet.net_at(p.at))
        .collect();
    let partners: Vec<Point2> = board
        .sheet
        .pins
        .iter()
        .filter(|p| p.owner != id)
        .filter(|p| board.sheet.net_at(p.at).is_some_and(|n| nets.contains(&n)))
        .map(|p| p.at)
        .collect();
    let anchor = *board.choose(rng, &partners)?;
    let reach = 7.62 + rng.f64() * 12.7;
    let (dx, dy) = NUDGES[rng.usize(..4)];
    let at = Point2::new(
        snap(anchor.x + dx.signum() * reach),
        snap(anchor.y + dy.signum() * reach),
    );
    Some(vec![(id.to_string(), Placement { at, ..here })])
}

/// Bring a part onto the column or row its identical twins already share.
/// Random nudges never find a bank; this is the move that draws one.
fn bank_snap(board: &Board, rng: &mut fastrand::Rng, id: &str, here: Placement) -> Option<Step> {
    let lib = &board.sheet.bodies.iter().find(|b| b.uuid == id)?.lib_id;
    let twins: Vec<Point2> = board
        .sheet
        .bodies
        .iter()
        .filter(|b| b.lib_id == *lib && b.uuid != id)
        .filter_map(|b| board.poses.get(&b.uuid).map(|p| p.at))
        .collect();
    let twin = *board.choose(rng, &twins)?;
    let at = if rng.bool() {
        Point2::new(twin.x, here.at.y)
    } else {
        Point2::new(here.at.x, twin.y)
    };
    Some(vec![(id.to_string(), Placement { at, ..here })])
}

fn carry_block(board: &Board, rng: &mut fastrand::Rng, id: &str) -> Option<Step> {
    let (dx, dy) = NUDGES[rng.usize(..NUDGES.len())];
    Some(
        board
            .block(id)
            .into_iter()
            .filter_map(|member| {
                let pose = board.poses.get(&member)?;
                Some((
                    member,
                    Placement {
                        at: Point2::new(snap(pose.at.x + dx), snap(pose.at.y + dy)),
                        ..*pose
                    },
                ))
            })
            .collect(),
    )
}

fn propose(board: &Board, rng: &mut fastrand::Rng) -> Option<Step> {
    let id = board.choose(rng, board.movable)?.clone();
    let here = *board.poses.get(&id)?;
    match rng.u32(0..100) {
        0..25 => {
            let (dx, dy) = NUDGES[rng.usize(..NUDGES.len())];
            let at = Point2::new(snap(here.at.x + dx), snap(here.at.y + dy));
            Some(vec![(id, Placement { at, ..here })])
        }
        25..38 => {
            let rot = ROTATIONS[rng.usize(..ROTATIONS.len())];
            (rot != here.rot).then(|| vec![(id, Placement { rot, ..here })])
        }
        38..46 => {
            let mirror = MIRRORS[rng.usize(..MIRRORS.len())];
            (mirror != here.mirror).then(|| vec![(id, Placement { mirror, ..here })])
        }
        46..56 => {
            let other = board.choose(rng, board.movable)?.clone();
            let there = *board.poses.get(&other)?;
            (other != id).then(|| {
                vec![
                    (
                        id,
                        Placement {
                            at: there.at,
                            ..here
                        },
                    ),
                    (
                        other,
                        Placement {
                            at: here.at,
                            ..there
                        },
                    ),
                ]
            })
        }
        56..70 => alignment(board, rng, &id, here),
        70..82 => teleport(board, rng, &id, here),
        82..92 => carry_block(board, rng, &id),
        _ => bank_snap(board, rng, &id, here),
    }
}

fn page_bounds(doc: &SchDoc) -> Rect {
    let [w, h] = doc.page().unwrap_or([297.0, 210.0]);
    Rect::new(10.16, 10.16, w - 10.16, h - 10.16)
}

fn board_of<'a>(doc: &SchDoc, sheet: &'a Sheet, movable: &'a [String], bounds: Rect) -> Board<'a> {
    Board {
        sheet,
        movable,
        poses: movable
            .iter()
            .filter_map(|id| Some((id.clone(), Placement::of(doc, id)?)))
            .collect(),
        bodies: sheet
            .bodies
            .iter()
            .map(|b| (b.uuid.clone(), b.rect))
            .collect(),
        bounds,
    }
}

/// Search over drags for the cleanest version of this sheet.
///
/// `movable` names symbols by UUID or by reference designator; a reference a
/// multi-unit part shares names only its first unit, so a caller that wants
/// every unit to move should pass UUIDs. Symbols outside `movable` never move.
/// The best sheet found is written back into `doc`; if nothing beat the start,
/// `doc` is left exactly as it was.
pub fn tidy(doc: &mut SchDoc, movable: &[String], options: &TidyOptions) -> TidyReport {
    let weights = options.weights;
    let bounds = page_bounds(doc);
    let mut rng = fastrand::Rng::with_seed(options.seed);
    let start = Instant::now();

    let mut sheet = Sheet::of(doc);
    let before = measure(&sheet);
    let mut current = doc.clone();
    let mut current_score = before.score(&weights);
    let mut best = current.clone();
    let mut best_score = current_score;
    let mut report = TidyReport {
        before,
        after: before,
        ..Default::default()
    };
    if movable.is_empty() {
        return report;
    }

    // The temperature has to be in the currency of the *moves*, not of the
    // score: most of a schematic's score is terms no single drag can touch, so
    // a temperature scaled by the score accepts everything and searches nothing.
    let mut sampled: Vec<f64> = Vec::new();
    let mut hot = 1.0;
    let mut since_best = 0usize;

    while start.elapsed().as_secs_f64() < options.seconds {
        let progress = start.elapsed().as_secs_f64() / options.seconds;
        let temperature = hot * (0.01_f64).powf(progress);

        let board = board_of(&current, &sheet, movable, bounds);
        let Some(step) = propose(&board, &mut rng) else {
            continue;
        };
        if step.is_empty() || board.obstructed(&step) {
            report.blocked += 1;
            continue;
        }
        report.proposed += 1;

        let mut trial = current.clone();
        let trial_sheet = match drag_many(&mut trial, &step, &sheet) {
            Ok((_, after)) => after,
            Err(DragError::Truthfulness(_) | DragError::Disconnection(_)) => {
                report.refused += 1;
                continue;
            }
            Err(_) => continue,
        };

        let score = measure(&trial_sheet).score(&weights);
        let delta = score - current_score;
        if sampled.len() < 200 {
            sampled.push(delta.abs());
            if sampled.len() == 200 {
                sampled.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
                hot = sampled[sampled.len() / 2].max(1.0);
            }
        }
        if !(delta < 0.0 || rng.f64() < (-delta / temperature).exp()) {
            continue;
        }
        report.accepted += 1;
        if delta < 0.0 {
            report.improved += 1;
        }
        current = trial;
        current_score = score;
        sheet = trial_sheet;
        if current_score < best_score - 1e-9 {
            best_score = current_score;
            best = current.clone();
            since_best = 0;
        } else {
            since_best += 1;
            // A walk that has wandered this long is worth less than another
            // attempt from the best sheet seen.
            if since_best > 400 {
                current = best.clone();
                current_score = best_score;
                sheet = Sheet::of(&current);
                since_best = 0;
            }
        }
    }

    *doc = best;
    report.after = measure(&Sheet::of(doc));
    report.seconds = start.elapsed().as_secs_f64();
    report
}
