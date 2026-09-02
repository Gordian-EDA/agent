//! "Drag it around until it looks cleanest", as a search.
//!
//! Every step is a drag a person could have made — nudge, rotate, mirror, swap
//! two parts, line a pin up with the pin it connects to — kept when the sheet
//! measures better and, with a small annealing temperature, sometimes kept when
//! it does not, so the search can climb out of a tidy-looking dead end. A step
//! that would change the netlist cannot be kept at all: [`crate::drag`] rolls
//! itself back before the evaluator ever sees it.

use std::collections::HashMap;
use std::time::Instant;

use geom::Point2;
use sch_doc::{Mirror, SchDoc};

use crate::drag::{DragError, Placement, drag_from};
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
    /// Steps the truthfulness gate refused, which is the honest count of how
    /// often a good-looking move would have rewired the board.
    pub refused: usize,
    pub seconds: f64,
}

impl TidyReport {
    pub fn gain(&self, w: &Weights) -> f64 {
        self.before.score(w) - self.after.score(w)
    }
}

/// One step of the search: a drag, or a pair of them.
#[derive(Debug, Clone)]
enum Step {
    Pose(String, Placement),
    Swap(String, Placement, String, Placement),
}

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

fn page_bounds(doc: &SchDoc) -> geom::Rect {
    let [w, h] = doc.page().unwrap_or([297.0, 210.0]);
    geom::Rect::new(12.7, 12.7, w - 12.7, h - 12.7)
}

/// Line one of a symbol's pins up with a pin it shares a net with — the move
/// that turns a dog-leg into a straight wire.
fn alignment(sheet: &Sheet, rng: &mut fastrand::Rng, id: &str, here: Placement) -> Option<Step> {
    let uuid = sheet
        .pins
        .iter()
        .find(|p| p.owner == id || p.refdes == id)?
        .owner
        .clone();
    let mine: Vec<&sch_doc::PlacedPin> = sheet.pins_of(&uuid).collect();
    if mine.is_empty() {
        return None;
    }
    let pin = mine[rng.usize(..mine.len())];
    let net = sheet.net_at(pin.at)?;
    let partners: Vec<Point2> = sheet
        .pins
        .iter()
        .filter(|p| p.owner != uuid && sheet.net_at(p.at) == Some(net))
        .map(|p| p.at)
        .collect();
    if partners.is_empty() {
        return None;
    }
    let target = partners[rng.usize(..partners.len())];
    let at = if rng.bool() {
        Point2::new(snap(here.at.x + target.x - pin.at.x), here.at.y)
    } else {
        Point2::new(here.at.x, snap(here.at.y + target.y - pin.at.y))
    };
    Some(Step::Pose(id.to_string(), Placement { at, ..here }))
}

fn propose(
    sheet: &Sheet,
    rng: &mut fastrand::Rng,
    movable: &[String],
    poses: &HashMap<String, Placement>,
    bounds: &geom::Rect,
) -> Option<Step> {
    let id = &movable[rng.usize(..movable.len())];
    let here = *poses.get(id)?;
    let inside = |p: Point2| bounds.contains(p);
    match rng.u32(0..100) {
        0..30 => {
            let (dx, dy) = NUDGES[rng.usize(..NUDGES.len())];
            let at = Point2::new(here.at.x + dx, here.at.y + dy);
            inside(at).then(|| Step::Pose(id.clone(), Placement { at, ..here }))
        }
        30..45 => {
            let rot = ROTATIONS[rng.usize(..ROTATIONS.len())];
            (rot != here.rot).then(|| Step::Pose(id.clone(), Placement { rot, ..here }))
        }
        45..55 => {
            let mirror = MIRRORS[rng.usize(..MIRRORS.len())];
            (mirror != here.mirror).then(|| Step::Pose(id.clone(), Placement { mirror, ..here }))
        }
        55..70 => {
            let other = &movable[rng.usize(..movable.len())];
            let there = *poses.get(other)?;
            (other != id).then(|| {
                Step::Swap(
                    id.clone(),
                    Placement {
                        at: there.at,
                        ..here
                    },
                    other.clone(),
                    Placement {
                        at: here.at,
                        ..there
                    },
                )
            })
        }
        _ => alignment(sheet, rng, id, here).filter(|step| match step {
            Step::Pose(_, p) => inside(p.at),
            Step::Swap(..) => true,
        }),
    }
}

/// Apply a step, threading the sheet view through so each drag pays for one
/// scene pass instead of two.
fn apply(doc: &mut SchDoc, step: &Step, before: &Sheet) -> Result<Sheet, DragError> {
    match step {
        Step::Pose(id, to) => drag_from(doc, id, *to, before).map(|(_, after)| after),
        Step::Swap(a, to_a, b, to_b) => {
            // Park the first one clear of the second, or the two drags collide
            // on the square they are exchanging.
            let park = Placement {
                at: Point2::new(to_a.at.x, to_a.at.y - 1000.0),
                ..*to_a
            };
            let (_, parked) = drag_from(doc, a, park, before)?;
            let (_, swapped) = drag_from(doc, b, *to_b, &parked)?;
            drag_from(doc, a, *to_a, &swapped).map(|(_, after)| after)
        }
    }
}

/// Search over drags for the cleanest version of this sheet.
///
/// `movable` names symbols by UUID or by reference designator; a reference a
/// multi-unit part shares names only its first unit, so a caller that wants
/// every unit to move should pass UUIDs. Symbols outside `movable` never move. The best sheet found is written back
/// into `doc`; if nothing beat the start, `doc` is left exactly as it was.
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

    let hot = 0.03 * current_score.max(1.0);
    while start.elapsed().as_secs_f64() < options.seconds {
        let progress = start.elapsed().as_secs_f64() / options.seconds;
        let temperature = hot * (0.02_f64).powf(progress);

        let poses: HashMap<String, Placement> = movable
            .iter()
            .filter_map(|id| Placement::of(&current, id).map(|p| (id.clone(), p)))
            .collect();
        let Some(step) = propose(&sheet, &mut rng, movable, &poses, &bounds) else {
            continue;
        };
        report.proposed += 1;

        let mut trial = current.clone();
        let trial_sheet = match apply(&mut trial, &step, &sheet) {
            Ok(after) => after,
            Err(DragError::Truthfulness(_)) => {
                report.refused += 1;
                continue;
            }
            Err(_) => continue,
        };

        let score = measure(&trial_sheet).score(&weights);
        let delta = score - current_score;
        let keep = delta < 0.0 || rng.f64() < (-delta / temperature).exp();
        if !keep {
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
        }
    }

    *doc = best;
    report.after = measure(&Sheet::of(doc));
    report.seconds = start.elapsed().as_secs_f64();
    report
}
