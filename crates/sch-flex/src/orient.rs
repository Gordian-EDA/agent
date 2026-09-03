//! Which way a part faces — the conventions a human draws by, applied to a leaf from the
//! role its container gives it.
//!
//! Nothing here is a search: each rule reads the part's own pins and the nets its
//! neighbours share, and returns one pose.

use geom::Dir;
use sch_model::tree::Axis;

use crate::part::{Part, Pose, rail_penalty};

/// The pose a leaf takes when its author did not name one.
///
/// - A connector, or anything with more than four pins, stands as the library drew it.
/// - A transistor picks the rot/mirror that points its ground pin down and its supply up.
/// - A 2-pin part touching a rail, or stacked in a column, stands vertical (ground pin at
///   the bottom); anything else in a row lies along the row, which is what "a row is one
///   signal path" means geometrically.
pub fn default_pose(part: &Part, axis: Axis) -> Pose {
    if part.is_connector() || part.pins.len() > 4 {
        return Pose::default();
    }
    if !part.two_pin() {
        return upright(part);
    }
    let rails = part.rails();
    let on_rail = rails.iter().any(|net| circuit_graph::netclass::is_ground(net)) || rails.len() == 2;
    if on_rail || axis == Axis::Col {
        return standing(part);
    }
    Pose {
        angle: part.rotations_along(true).first().copied().unwrap_or(0.0),
        mirror: false,
    }
}

/// A multi-pin part (transistor, small regulator) turned so its rails point the way a
/// reader expects: ground down, supply up. Ties break toward the unrotated, unmirrored
/// drawing.
fn upright(part: &Part) -> Pose {
    [
        (0.0, false),
        (0.0, true),
        (180.0, false),
        (180.0, true),
    ]
    .into_iter()
    .map(|(angle, mirror)| Pose { angle, mirror })
    .min_by(|a, b| cost(part, *a).total_cmp(&cost(part, *b)))
    .unwrap_or_default()
}

fn cost(part: &Part, pose: Pose) -> f64 {
    let rails: f64 = part
        .pins
        .iter()
        .filter_map(|pin| {
            let net = part.net(pin)?;
            circuit_graph::netclass::is_power_net(net)
                .then(|| rail_penalty(net, part.pin_dir(pin, pose)))
        })
        .sum();
    rails + if pose.angle == 0.0 { 0.0 } else { 1.0 } + if pose.mirror { 0.5 } else { 0.0 }
}

/// A 2-pin part stood on end, ground pin at the bottom and supply at the top.
fn standing(part: &Part) -> Pose {
    let vertical = part.rotations_along(false);
    for angle in &vertical {
        let pose = Pose {
            angle: *angle,
            mirror: false,
        };
        let seated = part.pins.iter().any(|pin| {
            part.net(pin).is_some_and(|net| {
                circuit_graph::netclass::is_power_net(net)
                    && part.pin_dir(pin, pose)
                        == if circuit_graph::netclass::is_ground(net) {
                            Dir::South
                        } else {
                            Dir::North
                        }
            })
        });
        if seated {
            return pose;
        }
    }
    Pose {
        angle: vertical.first().copied().unwrap_or(0.0),
        mirror: false,
    }
}

/// The model's `rot` for a 2-pin part follows the R/C convention — 0 stands the part up,
/// 90 lays it along the row. Parts the library draws horizontally (crystals, diodes, LEDs)
/// need that mapped onto their own native axis so 90 still means "lying".
pub fn authored_pose(part: &Part, rot: i32, mirror: bool) -> Pose {
    let angle = rot as f64;
    if !part.two_pin() || part.is_connector() {
        return Pose { angle, mirror };
    }
    let horizontal = matches!(rot, 90 | 270);
    let along = part.rotations_along(horizontal);
    let angle = if along.contains(&angle) || along.is_empty() {
        angle
    } else {
        let base = along[0];
        if matches!(rot, 180 | 270) {
            (base + 180.0) % 360.0
        } else {
            base
        }
    };
    let pose = Pose { angle, mirror };
    // An explicit orientation still never points a ground pin up or a supply pin down.
    let upside_down = part.pins.iter().any(|pin| {
        part.net(pin).is_some_and(|net| {
            circuit_graph::netclass::is_power_net(net)
                && part.pin_dir(pin, pose)
                    == if circuit_graph::netclass::is_ground(net) {
                        Dir::North
                    } else {
                        Dir::South
                    }
        })
    });
    if upside_down {
        Pose {
            angle: (angle + 180.0) % 360.0,
            mirror,
        }
    } else {
        pose
    }
}
