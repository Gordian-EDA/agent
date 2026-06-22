//! Parse board-DSL YAML text into the kernel [`BoardDesign`], collecting
//! [`Diagnostics`] (with spans + "did you mean" suggestions) as it goes.
//!
//! Walks the literal-preserving [`crate::yaml::Node`] tree; every scalar is its
//! exact source string, so `0.8`/`GND`/`90` keep their form.

use crate::diag::{Diagnostic, Diagnostics, Span};
use crate::model::*;
use crate::yaml::{self, Node};
use indexmap::IndexMap;

/// Parse + structurally validate. `None` design iff there were errors.
pub fn parse_str(src: &str) -> (Option<BoardDesign>, Diagnostics) {
    let (root, mut ds) = match yaml::load(src) {
        Ok(x) => x,
        Err(e) => return (None, e),
    };
    let top = match &root {
        Node::Map(entries, _) => entries,
        _ => {
            ds.push(Diagnostic::error(
                "top-level",
                "the board document must be a mapping",
            ));
            return (None, ds);
        }
    };

    known_keys(top, &["version", "name", "board", "parts", "place"], &mut ds);

    // version: required, must be 1.
    match find(top, "version") {
        Some(n) => match scalar(n) {
            Some("1") => {}
            Some(other) => ds.push(
                Diagnostic::error("version", format!("unsupported version `{other}` (expected 1)"))
                    .with_span(n.span()),
            ),
            None => ds.push(Diagnostic::error("version", "version must be the scalar 1").with_span(n.span())),
        },
        None => ds.push(Diagnostic::error("version", "missing required `version: 1`")),
    }

    let name = find(top, "name").and_then(scalar).map(str::to_string);

    let board = match find(top, "board") {
        Some(n) => parse_board(n, &mut ds),
        None => {
            ds.push(Diagnostic::error("board", "missing required `board:` section"));
            BoardSpec::default()
        }
    };

    let parts = match find(top, "parts") {
        Some(n) => parse_parts(n, &mut ds),
        None => {
            ds.push(Diagnostic::error("parts", "missing required `parts:` section"));
            IndexMap::new()
        }
    };

    let groups = match find(top, "place") {
        Some(n) => parse_place(n, &mut ds),
        None => IndexMap::new(),
    };

    let design = BoardDesign {
        name,
        board,
        parts,
        groups,
    };
    let out = if ds.has_errors() { None } else { Some(design) };
    (out, ds)
}

// ── board ────────────────────────────────────────────────────────────────

fn parse_board(n: &Node, ds: &mut Diagnostics) -> BoardSpec {
    let mut spec = BoardSpec::default();
    let Node::Map(m, _) = n else {
        ds.push(Diagnostic::error("board", "`board` must be a mapping").with_span(n.span()));
        return spec;
    };
    known_keys(m, &["layers", "outline", "rules"], ds);
    if let Some(l) = find(m, "layers") {
        if let Some(v) = parse_u32(l, "board.layers", ds) {
            if matches!(v, 2 | 4 | 6 | 8) {
                spec.layers = v;
            } else {
                ds.push(
                    Diagnostic::error("layers", format!("layer count `{v}` must be 2, 4, 6, or 8"))
                        .with_span(l.span()),
                );
            }
        }
    }
    if let Some(o) = find(m, "outline") {
        if let Some(outline) = parse_outline(o, ds) {
            spec.outline = outline;
        }
    }
    if let Some(r) = find(m, "rules") {
        spec.rules = parse_rules(r, ds);
    }
    spec
}

fn parse_outline(n: &Node, ds: &mut Diagnostics) -> Option<Outline> {
    let Node::Map(m, _) = n else {
        ds.push(
            Diagnostic::error("outline", "`outline` must be a mapping like {rect: [w, h]}")
                .with_span(n.span()),
        );
        return None;
    };
    known_keys(m, &["rect", "circle", "polygon"], ds);
    if let Some(r) = find(m, "rect") {
        let nums = number_seq(r, "outline.rect", ds);
        if nums.len() == 2 {
            return Some(Outline::Rect {
                w: nums[0],
                h: nums[1],
            });
        }
        ds.push(Diagnostic::error("outline", "`rect` needs [width, height]").with_span(r.span()));
        return None;
    }
    if let Some(c) = find(m, "circle") {
        if let Some(rad) = parse_f64(c, "outline.circle", ds) {
            return Some(Outline::Circle { r: rad });
        }
        return None;
    }
    if let Some(p) = find(m, "polygon") {
        let Node::Seq(pts, _) = p else {
            ds.push(
                Diagnostic::error("outline", "`polygon` must be a list of [x, y] points")
                    .with_span(p.span()),
            );
            return None;
        };
        let mut out = Vec::new();
        for pt in pts {
            let nums = number_seq(pt, "outline.polygon point", ds);
            if nums.len() == 2 {
                out.push((nums[0], nums[1]));
            }
        }
        if out.len() >= 3 {
            return Some(Outline::Polygon(out));
        }
        ds.push(Diagnostic::error("outline", "`polygon` needs at least 3 points").with_span(p.span()));
        return None;
    }
    ds.push(
        Diagnostic::error("outline", "`outline` needs one of rect / circle / polygon")
            .with_span(n.span()),
    );
    None
}

fn parse_rules(n: &Node, ds: &mut Diagnostics) -> Rules {
    let mut rules = Rules::default();
    let Node::Map(m, _) = n else {
        ds.push(Diagnostic::error("rules", "`rules` must be a mapping").with_span(n.span()));
        return rules;
    };
    known_keys(
        m,
        &["clearance", "trace_width", "via", "net_widths", "pours"],
        ds,
    );
    if let Some(c) = find(m, "clearance") {
        if let Some(v) = parse_f64(c, "rules.clearance", ds) {
            rules.clearance = v;
        }
    }
    if let Some(t) = find(m, "trace_width") {
        if let Some(v) = parse_f64(t, "rules.trace_width", ds) {
            rules.trace_width = v;
        }
    }
    if let Some(v) = find(m, "via") {
        let nums = number_seq(v, "rules.via", ds);
        if nums.len() == 2 {
            rules.via_diameter = nums[0];
            rules.via_drill = nums[1];
        } else {
            ds.push(
                Diagnostic::error("rules", "`via` needs [diameter, drill] in mm").with_span(v.span()),
            );
        }
    }
    if let Some(nw) = find(m, "net_widths") {
        if let Node::Map(map, _) = nw {
            for ((k, kspan), val) in map {
                if let Some(w) = parse_f64(val, "rules.net_widths value", ds) {
                    rules.net_widths.insert(k.clone(), w);
                } else {
                    ds.push(
                        Diagnostic::error("net_widths", format!("width for net `{k}` must be a number"))
                            .with_span(*kspan),
                    );
                }
            }
        } else {
            ds.push(
                Diagnostic::error("rules", "`net_widths` must be a {net: mm} mapping")
                    .with_span(nw.span()),
            );
        }
    }
    if let Some(p) = find(m, "pours") {
        if let Node::Seq(items, _) = p {
            for item in items {
                if let Some(pour) = parse_pour(item, ds) {
                    rules.pours.push(pour);
                }
            }
        } else {
            ds.push(Diagnostic::error("rules", "`pours` must be a list").with_span(p.span()));
        }
    }
    rules
}

fn parse_pour(n: &Node, ds: &mut Diagnostics) -> Option<Pour> {
    let Node::Map(m, _) = n else {
        ds.push(
            Diagnostic::error("pours", "each pour must be {net: .., layer: ..}").with_span(n.span()),
        );
        return None;
    };
    known_keys(m, &["net", "layer"], ds);
    let net = find(m, "net").and_then(scalar).map(str::to_string);
    let layer = find(m, "layer").and_then(scalar).map(str::to_string);
    match (net, layer) {
        (Some(net), Some(layer)) => Some(Pour { net, layer }),
        _ => {
            ds.push(Diagnostic::error("pours", "pour needs both `net` and `layer`").with_span(n.span()));
            None
        }
    }
}

// ── parts ──────────────────────────────────────────────────────────────────

fn parse_parts(n: &Node, ds: &mut Diagnostics) -> IndexMap<String, Part> {
    let mut parts = IndexMap::new();
    let Node::Map(m, _) = n else {
        ds.push(
            Diagnostic::error("parts", "`parts` must be a {refdes: {...}} mapping").with_span(n.span()),
        );
        return parts;
    };
    for ((refdes, rspan), body) in m {
        let Node::Map(pm, _) = body else {
            ds.push(
                Diagnostic::error("part", format!("part `{refdes}` must be a mapping"))
                    .with_span(*rspan),
            );
            continue;
        };
        known_keys(pm, &["footprint", "pads", "edge", "corner", "lock"], ds);
        let mut part = Part::default();
        match find(pm, "footprint").and_then(scalar) {
            Some(fp) => part.footprint = fp.to_string(),
            None => ds.push(
                Diagnostic::error("part", format!("part `{refdes}` is missing a `footprint`"))
                    .with_span(*rspan),
            ),
        }
        if let Some(pads) = find(pm, "pads") {
            if let Node::Map(pads_map, _) = pads {
                for ((pad, _), net) in pads_map {
                    match scalar(net) {
                        Some(nm) => {
                            part.pads.insert(pad.clone(), nm.to_string());
                        }
                        None => ds.push(
                            Diagnostic::error("pads", format!("pad `{pad}` of `{refdes}` needs a net name"))
                                .with_span(net.span()),
                        ),
                    }
                }
            } else {
                ds.push(
                    Diagnostic::error("pads", format!("`pads` of `{refdes}` must be a {{pad: net}} map"))
                        .with_span(pads.span()),
                );
            }
        }
        part.edge = find(pm, "edge").map(|n| parse_bool(n, "edge", ds)).unwrap_or(false);
        part.corner = find(pm, "corner").map(|n| parse_bool(n, "corner", ds)).unwrap_or(false);
        if let Some(l) = find(pm, "lock") {
            part.lock = parse_lock(l, ds);
        }
        if parts.insert(refdes.clone(), part).is_some() {
            ds.push(
                Diagnostic::error("part", format!("duplicate part `{refdes}`")).with_span(*rspan),
            );
        }
    }
    parts
}

fn parse_lock(n: &Node, ds: &mut Diagnostics) -> Option<Lock> {
    let Node::Map(m, _) = n else {
        ds.push(Diagnostic::error("lock", "`lock` must be {at: [x, y], rot: deg}").with_span(n.span()));
        return None;
    };
    known_keys(m, &["at", "rot"], ds);
    let at = find(m, "at").map(|a| number_seq(a, "lock.at", ds)).unwrap_or_default();
    if at.len() != 2 {
        ds.push(Diagnostic::error("lock", "`lock.at` needs [x, y]").with_span(n.span()));
        return None;
    }
    let rot = find(m, "rot").and_then(|r| parse_i32(r, "lock.rot", ds)).unwrap_or(0);
    if !matches!(rot, 0 | 90 | 180 | 270) {
        ds.push(
            Diagnostic::error("lock", format!("rotation `{rot}` must be 0/90/180/270"))
                .with_span(n.span()),
        );
    }
    Some(Lock {
        x: at[0],
        y: at[1],
        rot,
    })
}

// ── place ────────────────────────────────────────────────────────────────

fn parse_place(n: &Node, ds: &mut Diagnostics) -> IndexMap<String, Group> {
    let mut groups = IndexMap::new();
    let Node::Map(m, _) = n else {
        ds.push(Diagnostic::error("place", "`place` must be a mapping").with_span(n.span()));
        return groups;
    };
    known_keys(m, &["groups"], ds);
    let Some(g) = find(m, "groups") else {
        return groups;
    };
    let Node::Map(gm, _) = g else {
        ds.push(Diagnostic::error("place", "`place.groups` must be a {name: {...}} map").with_span(g.span()));
        return groups;
    };
    for ((name, nspan), body) in gm {
        let Node::Map(bm, _) = body else {
            ds.push(Diagnostic::error("group", format!("group `{name}` must be a mapping")).with_span(*nspan));
            continue;
        };
        known_keys(bm, &["members", "region", "edge", "surround", "grid"], ds);
        let mut group = Group::default();
        if let Some(mem) = find(bm, "members") {
            group.members = string_seq(mem, "group.members", ds);
        } else {
            ds.push(Diagnostic::error("group", format!("group `{name}` needs `members`")).with_span(*nspan));
        }
        if let Some(r) = find(bm, "region") {
            let nums = number_seq(r, "group.region", ds);
            if nums.len() == 4 {
                group.region = Some([nums[0], nums[1], nums[2], nums[3]]);
            } else {
                ds.push(
                    Diagnostic::error("group", "`region` needs [min_x, min_y, max_x, max_y]")
                        .with_span(r.span()),
                );
            }
        }
        group.edge = find(bm, "edge").and_then(scalar).map(str::to_string);
        if let Some(e) = &group.edge {
            if !matches!(e.as_str(), "n" | "s" | "e" | "w") {
                ds.push(
                    Diagnostic::error("group", format!("edge `{e}` must be n/s/e/w"))
                        .with_span(find(bm, "edge").unwrap().span()),
                );
            }
        }
        group.surround = find(bm, "surround").and_then(scalar).map(str::to_string);
        group.grid = find(bm, "grid").map(|n| parse_bool(n, "grid", ds)).unwrap_or(false);
        groups.insert(name.clone(), group);
    }
    groups
}

// ── helpers ──────────────────────────────────────────────────────────────

fn find<'a>(map: &'a [((String, Span), Node)], key: &str) -> Option<&'a Node> {
    map.iter().find(|((k, _), _)| k == key).map(|(_, v)| v)
}

fn scalar(n: &Node) -> Option<&str> {
    match n {
        Node::Scalar(s, _) => Some(s.as_str()),
        _ => None,
    }
}

fn parse_f64(n: &Node, field: &str, ds: &mut Diagnostics) -> Option<f64> {
    match scalar(n).and_then(|s| s.parse::<f64>().ok()) {
        Some(v) => Some(v),
        None => {
            ds.push(Diagnostic::error("number", format!("`{field}` must be a number")).with_span(n.span()));
            None
        }
    }
}

fn parse_u32(n: &Node, field: &str, ds: &mut Diagnostics) -> Option<u32> {
    match scalar(n).and_then(|s| s.parse::<u32>().ok()) {
        Some(v) => Some(v),
        None => {
            ds.push(Diagnostic::error("number", format!("`{field}` must be a whole number")).with_span(n.span()));
            None
        }
    }
}

fn parse_i32(n: &Node, field: &str, ds: &mut Diagnostics) -> Option<i32> {
    match scalar(n).and_then(|s| s.parse::<i32>().ok()) {
        Some(v) => Some(v),
        None => {
            ds.push(Diagnostic::error("number", format!("`{field}` must be a whole number")).with_span(n.span()));
            None
        }
    }
}

fn parse_bool(n: &Node, field: &str, ds: &mut Diagnostics) -> bool {
    match scalar(n) {
        Some("true") => true,
        Some("false") => false,
        _ => {
            ds.push(Diagnostic::error("bool", format!("`{field}` must be true or false")).with_span(n.span()));
            false
        }
    }
}

/// A flat sequence of numbers, or a single scalar number (so `circle: 16` and
/// `rect: [44, 32]` both work). Bad elements are diagnosed and skipped.
fn number_seq(n: &Node, field: &str, ds: &mut Diagnostics) -> Vec<f64> {
    match n {
        Node::Seq(items, _) => items
            .iter()
            .filter_map(|it| parse_f64(it, field, ds))
            .collect(),
        Node::Scalar(_, _) => parse_f64(n, field, ds).into_iter().collect(),
        _ => {
            ds.push(Diagnostic::error("number", format!("`{field}` must be a number or list")).with_span(n.span()));
            Vec::new()
        }
    }
}

fn string_seq(n: &Node, field: &str, ds: &mut Diagnostics) -> Vec<String> {
    match n {
        Node::Seq(items, _) => items
            .iter()
            .filter_map(|it| match scalar(it) {
                Some(s) => Some(s.to_string()),
                None => {
                    ds.push(Diagnostic::error("list", format!("`{field}` entries must be names")).with_span(it.span()));
                    None
                }
            })
            .collect(),
        _ => {
            ds.push(Diagnostic::error("list", format!("`{field}` must be a list")).with_span(n.span()));
            Vec::new()
        }
    }
}

/// Flag any map key not in `allowed`, with a strsim "did you mean" suggestion.
fn known_keys(map: &[((String, Span), Node)], allowed: &[&str], ds: &mut Diagnostics) {
    for ((k, span), _) in map {
        if !allowed.contains(&k.as_str()) {
            let mut d = Diagnostic::error("unknown-key", format!("unknown key `{k}`")).with_span(*span);
            if let Some(best) = nearest(k, allowed) {
                d = d.with_suggestion(best);
            }
            ds.push(d);
        }
    }
}

fn nearest(k: &str, allowed: &[&str]) -> Option<String> {
    allowed
        .iter()
        .map(|a| (*a, strsim::levenshtein(k, a)))
        .filter(|(_, d)| *d <= 2)
        .min_by_key(|(_, d)| *d)
        .map(|(a, _)| a.to_string())
}
