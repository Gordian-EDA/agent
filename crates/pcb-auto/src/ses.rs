//! Specctra SES reader: puts a router's wires and vias back on a [`Board`].
//!
//! Ported from `pcbagent.route.ses`, cut down to what this pipeline routes: a whole board
//! with no kept nets, no net filter and no blind vias.

use std::collections::{HashMap, HashSet};

use crate::dsn::pad_bbox;
use crate::geom::{point_in_polygon, seg_point_dist, BBox, Point};
use crate::model::Board;
use crate::rules::signal_track_width;

// ---- parser -------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Sx {
    Atom(String),
    List(Vec<Sx>),
}

impl Sx {
    fn text(&self) -> &str {
        match self {
            Sx::Atom(s) => s,
            Sx::List(_) => "",
        }
    }
    fn as_list(&self) -> Option<&[Sx]> {
        match self {
            Sx::List(v) => Some(v),
            Sx::Atom(_) => None,
        }
    }
    fn num(&self) -> Option<f64> {
        match self {
            Sx::Atom(s) => s.parse().ok(),
            Sx::List(_) => None,
        }
    }
}

fn find<'a>(items: &'a [Sx], head: &str) -> Option<&'a [Sx]> {
    items
        .iter()
        .filter_map(Sx::as_list)
        .find(|l| l.first().map(Sx::text) == Some(head))
}

fn find_all<'a>(items: &'a [Sx], head: &str) -> Vec<&'a [Sx]> {
    items
        .iter()
        .filter_map(Sx::as_list)
        .filter(|l| l.first().map(Sx::text) == Some(head))
        .collect()
}

/// Nested lists of tokens. `(string_quote ")` is tolerated.
fn parse(text: &str) -> anyhow::Result<Vec<Sx>> {
    let text = text.replace("(string_quote \")", "");
    let bytes: Vec<char> = text.chars().collect();
    let mut stack: Vec<Vec<Sx>> = vec![Vec::new()];
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '(' {
            stack.push(Vec::new());
            i += 1;
        } else if c == ')' {
            let done = stack.pop().ok_or_else(|| anyhow::anyhow!("unbalanced parentheses"))?;
            stack
                .last_mut()
                .ok_or_else(|| anyhow::anyhow!("unbalanced parentheses"))?
                .push(Sx::List(done));
            i += 1;
        } else if c == '"' {
            let j = (i + 1..bytes.len())
                .find(|&j| bytes[j] == '"')
                .ok_or_else(|| anyhow::anyhow!("unclosed string"))?;
            stack
                .last_mut()
                .ok_or_else(|| anyhow::anyhow!("unbalanced parentheses"))?
                .push(Sx::Atom(bytes[i + 1..j].iter().collect()));
            i = j + 1;
        } else {
            let mut j = i;
            while j < bytes.len() && !bytes[j].is_whitespace() && !"()\"".contains(bytes[j]) {
                j += 1;
            }
            stack
                .last_mut()
                .ok_or_else(|| anyhow::anyhow!("unbalanced parentheses"))?
                .push(Sx::Atom(bytes[i..j].iter().collect()));
            i = j;
        }
    }
    anyhow::ensure!(stack.len() == 1, "unbalanced parentheses");
    Ok(stack.pop().unwrap_or_default())
}

// ---- session ------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SesWire {
    pub net: String,
    pub layer: String,
    /// mm
    pub width: f64,
    /// mm, KiCad frame
    pub points: Vec<Point>,
}

#[derive(Debug, Clone)]
pub struct SesVia {
    pub net: String,
    pub padstack: String,
    pub pos: Point,
}

#[derive(Debug, Clone, Default)]
pub struct Session {
    pub wires: Vec<SesWire>,
    pub vias: Vec<SesVia>,
    pub nets: Vec<String>,
}

/// Parse a Specctra session file: the router's wires and vias, in KiCad's mm/y-down frame.
pub fn read_ses(text: &str) -> anyhow::Result<Session> {
    let doc = parse(text)?;
    let top: &[Sx] = match doc.first() {
        Some(Sx::List(v)) => v,
        _ => &doc,
    };
    let routes = find(top, "routes").ok_or_else(|| anyhow::anyhow!("SES has no routes section"))?;
    let (unit, per) = match find(routes, "resolution") {
        Some(r) => (
            r.get(1).map(Sx::text).unwrap_or("um").to_string(),
            r.get(2).and_then(Sx::num).unwrap_or(10.0),
        ),
        None => ("um".to_string(), 10.0),
    };
    let scale = match unit.as_str() {
        "um" => 0.001,
        "mm" => 1.0,
        "mil" => 0.0254,
        "inch" | "in" => 25.4,
        other => anyhow::bail!("SES resolution in unknown unit {other:?}"),
    };
    anyhow::ensure!(per != 0.0, "SES resolution divides by zero");
    let to_mm = scale / per;

    let mut s = Session::default();
    let empty: [Sx; 0] = [];
    let net_out = find(routes, "network_out").unwrap_or(&empty);
    for net in find_all(net_out, "net") {
        let name = net.get(1).map(Sx::text).unwrap_or("").to_string();
        s.nets.push(name.clone());
        for wire in find_all(net, "wire") {
            let Some(path) = find(wire, "path") else { continue };
            let layer = path.get(1).map(Sx::text).unwrap_or("").to_string();
            let width = path.get(2).and_then(Sx::num).unwrap_or(0.0) * to_mm;
            let nums: Vec<f64> = path
                .iter()
                .skip(3)
                .filter(|t| t.as_list().is_none())
                .filter_map(Sx::num)
                .collect();
            let points = nums
                .chunks_exact(2)
                .map(|c| (c[0] * to_mm, -c[1] * to_mm))
                .collect();
            s.wires.push(SesWire { net: name.clone(), layer, width, points });
        }
        for via in find_all(net, "via") {
            let (Some(x), Some(y)) = (via.get(2).and_then(Sx::num), via.get(3).and_then(Sx::num))
            else {
                continue;
            };
            s.vias.push(SesVia {
                net: name.clone(),
                padstack: via.get(1).map(Sx::text).unwrap_or("").to_string(),
                pos: (x * to_mm, -y * to_mm),
            });
        }
    }
    Ok(s)
}

/// Size, drill and spanned layers of a via padstack named `Via[i-j]_size:drill_um`.
/// Anything else falls back to `default` on the full stack.
pub fn via_geometry(
    padstack: &str,
    copper: &[String],
    default: (f64, f64),
) -> (f64, f64, (String, String)) {
    let full = || {
        (
            copper.first().cloned().unwrap_or_default(),
            copper.last().cloned().unwrap_or_default(),
        )
    };
    let parsed = (|| {
        let start = padstack.find("Via[")?;
        let rest = &padstack[start + 4..];
        let (a, rest) = rest.split_once('-')?;
        let (b, rest) = rest.split_once(']')?;
        let rest = rest.strip_prefix('_')?;
        let (size, rest) = rest.split_once(':')?;
        let drill = rest.strip_suffix("_um").unwrap_or(rest);
        let last = copper.len().saturating_sub(1);
        Some((
            a.parse::<usize>().ok()?.min(last),
            b.parse::<usize>().ok()?.min(last),
            size.parse::<f64>().ok()?,
            drill.parse::<f64>().ok()?,
        ))
    })();
    match parsed {
        Some((a, b, size, drill)) => (
            size / 1000.0,
            drill / 1000.0,
            (copper[a].clone(), copper[b].clone()),
        ),
        None => (default.0, default.1, full()),
    }
}

/// A pad's copper box and the layers it reaches.
type PadBox = (BBox, HashSet<String>);
/// A pour outline and the layers it covers.
type Plane = (Vec<Point>, HashSet<String>);

#[derive(Debug, Clone, Default)]
pub struct ApplyResult {
    /// Nets the session routed that the board knows.
    pub nets: usize,
    pub tracks: usize,
    pub vias: usize,
    /// Tracks and vias removed before the session's copper was laid.
    pub removed: usize,
    pub unknown_nets: Vec<String>,
    pub applied_nets: Vec<String>,
}

/// Wire layers ending at `pos`, tolerant to the router's 0.1 um rounding across a cell edge.
fn layers_at(ends: &HashMap<(i64, i64, i64), HashSet<String>>, nid: i64, pos: Point) -> HashSet<String> {
    let (cx, cy) = ((pos.0 * 100.0).round() as i64, (pos.1 * 100.0).round() as i64);
    let mut out = HashSet::new();
    for dx in -1..=1 {
        for dy in -1..=1 {
            if let Some(s) = ends.get(&(nid, cx + dx, cy + dy)) {
                out.extend(s.iter().cloned());
            }
        }
    }
    out
}

/// Layers of wires whose body (not only an end) runs under a via of the given radius.
fn layers_through(segs: &[(Point, Point, String)], pos: Point, radius: f64) -> HashSet<String> {
    segs.iter()
        .filter(|(a, b, _)| seg_point_dist(*a, *b, pos) <= radius - 0.005)
        .map(|(_, _, l)| l.clone())
        .collect()
}

/// Layers of a pad or pour a via of this span can land on. A wildcard covers whatever it spans.
fn reach(layers: &HashSet<String>, span: &HashSet<String>) -> HashSet<String> {
    if layers.iter().any(|l| l.starts_with("*.") || l.starts_with("F&B.")) {
        return layers.clone();
    }
    layers.intersection(span).cloned().collect()
}

/// True when a pad of the net covers `pos` on a copper layer the wires do not use.
fn via_on_pad(
    pos: Point,
    wire_layers: &HashSet<String>,
    pads: &[PadBox],
    span: &HashSet<String>,
) -> bool {
    pads.iter().any(|(box_, layers)| {
        box_.inflate(0.05).contains(pos) && reach(layers, span).difference(wire_layers).next().is_some()
    })
}

/// True when a pour of the net covers `pos` on a copper layer the wires do not use.
fn via_in_plane(
    pos: Point,
    wire_layers: &HashSet<String>,
    planes: &[Plane],
    span: &HashSet<String>,
) -> bool {
    planes.iter().any(|(poly, layers)| {
        reach(layers, span).difference(wire_layers).next().is_some() && point_in_polygon(pos, poly)
    })
}

/// Replace the copper of every net the session routed with the session's wires and vias.
///
/// Track widths are floored at the design-rule minimum so the router's rounding cannot fall
/// under it. A via whose wires all stay on one layer is dropped unless it is what joins that
/// wire to a pad or a copper pour of the same net on another layer. `only_nets`, when it is
/// non-empty, ignores every other net the session carried copper for.
pub fn apply_ses(
    board: &mut Board,
    text: &str,
    copper: &[String],
    only_nets: &HashSet<String>,
) -> anyhow::Result<ApplyResult> {
    let mut s = read_ses(text)?;
    if !only_nets.is_empty() {
        s.nets.retain(|n| only_nets.contains(n));
        s.wires.retain(|w| only_nets.contains(&w.net));
        s.vias.retain(|v| only_nets.contains(&v.net));
    }
    let rules = board.design_rules();
    let ids: HashMap<String, i64> = board.nets().into_iter().map(|n| (n.name, n.id)).collect();
    let unknown_nets: Vec<String> = s.nets.iter().filter(|n| !ids.contains_key(*n)).cloned().collect();
    let applied_nets: Vec<String> = s.nets.iter().filter(|n| ids.contains_key(*n)).cloned().collect();
    let routed_ids: HashSet<i64> = applied_nets.iter().filter_map(|n| ids.get(n)).copied().collect();
    let removed = if routed_ids.is_empty() {
        0
    } else {
        board.remove_copper(Some(&routed_ids))
    };

    // floor against router rounding below the design-rule minimum only -- flooring at a net's
    // policy width instead would silently widen a net routed deliberately thin
    let min_width = signal_track_width(&rules, None);
    let mut tracks = 0usize;
    let mut vias = 0usize;
    // (net, x, y) in 10 um cells -> layers of tracks ending there
    let mut ends: HashMap<(i64, i64, i64), HashSet<String>> = HashMap::new();
    // net -> (a, b, layer) for mid-segment via landings
    let mut segs: HashMap<i64, Vec<(Point, Point, String)>> = HashMap::new();
    for wire in &s.wires {
        let Some(&nid) = ids.get(&wire.net) else { continue };
        for ab in wire.points.windows(2) {
            let (a, b) = (ab[0], ab[1]);
            if (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6 {
                continue;
            }
            board.add_track(a, b, wire.width.max(min_width), &wire.layer, nid);
            tracks += 1;
            segs.entry(nid).or_default().push((a, b, wire.layer.clone()));
            for p in [a, b] {
                ends.entry((nid, (p.0 * 100.0).round() as i64, (p.1 * 100.0).round() as i64))
                    .or_default()
                    .insert(wire.layer.clone());
            }
        }
    }

    let mut pad_boxes: HashMap<i64, Vec<PadBox>> = HashMap::new();
    for f in board.footprints() {
        for p in &f.pads {
            if p.net_id != 0 {
                pad_boxes
                    .entry(p.net_id)
                    .or_default()
                    .push((pad_bbox(&f, p), p.copper_layers().into_iter().collect()));
            }
        }
    }
    let mut planes: HashMap<i64, Vec<Plane>> = HashMap::new();
    for z in board.zones() {
        if z.keepout.is_none() && z.net_id != 0 && !z.polygon.is_empty() {
            planes
                .entry(z.net_id)
                .or_default()
                .push((z.polygon.clone(), z.layers.iter().cloned().collect()));
        }
    }

    let no_pads: Vec<PadBox> = Vec::new();
    let no_planes: Vec<Plane> = Vec::new();
    let no_segs: Vec<(Point, Point, String)> = Vec::new();
    for via in &s.vias {
        let Some(&nid) = ids.get(&via.net) else { continue };
        let (size, drill, layers) = via_geometry(&via.padstack, copper, (rules.via_size, rules.via_drill));
        let span: HashSet<String> = match (
            copper.iter().position(|l| *l == layers.0),
            copper.iter().position(|l| *l == layers.1),
        ) {
            (Some(a), Some(b)) => copper[a.min(b)..=a.max(b)].iter().cloned().collect(),
            _ => copper.iter().cloned().collect(),
        };
        let mut layers_here = layers_at(&ends, nid, via.pos);
        layers_here.extend(layers_through(
            segs.get(&nid).unwrap_or(&no_segs),
            via.pos,
            size / 2.0,
        ));
        let here: HashSet<String> = layers_here.intersection(&span).cloned().collect();
        // A via whose wires all stay on one layer is redundant, unless it is what joins that
        // wire to a pad or a copper pour of the same net on another layer.
        if here.len() < 2
            && layers_here.is_subset(&span)
            && !via_on_pad(via.pos, &here, pad_boxes.get(&nid).unwrap_or(&no_pads), &span)
            && !via_in_plane(via.pos, &here, planes.get(&nid).unwrap_or(&no_planes), &span)
        {
            continue;
        }
        board.add_via(via.pos, size, drill, nid, (&layers.0, &layers.1));
        vias += 1;
    }

    Ok(ApplyResult {
        nets: routed_ids.len(),
        tracks,
        vias,
        removed,
        unknown_nets,
        applied_nets,
    })
}
