use std::path::{Path, PathBuf};

use geom::{Point2, Rect};

use crate::error::{Error, Result};
use crate::{CourtyardSource, Footprint, FootprintPad, PadTechnology, PcbEdgeDatum};

impl Footprint {
    /// Parse a single `.kicad_mod` file into a [`Footprint`].
    ///
    /// The bare footprint name is taken from the file stem, which is canonical
    /// for `.pretty` libraries: the filename is the footprint name. The library
    /// is unknown for a standalone file, so [`Footprint::id`] is `None`.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Footprint> {
        let path = path.as_ref();
        let doc = kiutils_kicad::FootprintFile::read(path).map_err(|e| map_kiutils_err(path, e))?;
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        Ok(build_footprint(name, doc.ast(), || {
            std::fs::read_to_string(path).ok()
        }))
    }

    /// Parse `.kicad_mod` `source` text directly, attributing it to `name`.
    ///
    /// `kiutils` 0.3 only parses footprints from a path, so the text is staged
    /// in a tempfile; the workaround is contained here and never surfaces in the
    /// public contract. [`Footprint::id`] is `None`.
    pub fn parse_str(name: impl Into<String>, source: &str) -> Result<Footprint> {
        let tmp = tempfile::Builder::new()
            .suffix(".kicad_mod")
            .tempfile()
            .map_err(|e| Error::Io {
                path: PathBuf::from("<parse_str>"),
                source: e,
            })?;
        std::fs::write(tmp.path(), source).map_err(|e| Error::Io {
            path: tmp.path().to_path_buf(),
            source: e,
        })?;
        let doc = kiutils_kicad::FootprintFile::read(tmp.path())
            .map_err(|e| map_kiutils_err(tmp.path(), e))?;
        Ok(build_footprint(name.into(), doc.ast(), || {
            Some(source.to_string())
        }))
    }
}

/// Assemble a [`Footprint`] from a parsed AST. `raw_source` is consulted only
/// when a custom-shaped pad needs its primitive extents recovered from text.
fn build_footprint(
    name: String,
    ast: &kiutils_kicad::FootprintAst,
    raw_source: impl FnOnce() -> Option<String>,
) -> Footprint {
    let mut pads: Vec<FootprintPad> = ast.pads.iter().map(pad_detail).collect();
    let raw = raw_source();
    if pads.iter().any(|p| p.shape == "custom")
        && let Some(raw) = raw.as_deref()
    {
        let half_extents = custom_pad_half_extents(raw);
        for (bi, pad) in pads.iter_mut().filter(|p| p.shape == "custom").enumerate() {
            if let Some(half) = half_extents.get(bi) {
                pad.size = Point2::new(pad.size.x.max(2.0 * half.x), pad.size.y.max(2.0 * half.y));
            }
        }
    }
    let (courtyard, courtyard_source) = courtyard_bbox(ast, &pads);
    let bounds = overall_bbox(ast, &pads).unwrap_or_else(Rect::zero);
    let pcb_edge_datum = pcb_edge_datum(ast, raw.as_deref());

    Footprint {
        id: None,
        name,
        descr: ast.descr.clone(),
        pads,
        courtyard,
        courtyard_source,
        bounds,
        pcb_edge_datum,
    }
}

/// Resolve the `Dwgs.User` line associated with a `PCB Edge` user-text marker.
/// A footprint may contain other construction lines, so proximity to the
/// marker is the deterministic association rule. Without the marker, ordinary
/// drawing geometry is never promoted to placement metadata.
fn pcb_edge_datum(
    ast: &kiutils_kicad::FootprintAst,
    raw_source: Option<&str>,
) -> Option<PcbEdgeDatum> {
    let mut markers: Vec<Point2> = ast
        .graphics
        .iter()
        .filter(|g| {
            g.token == "fp_text"
                && g.layer.as_deref() == Some("Dwgs.User")
                && g.text
                    .as_deref()
                    .is_some_and(|text| text.trim().eq_ignore_ascii_case("PCB Edge"))
        })
        .filter_map(|g| g.at.map(Point2::from))
        .collect();
    if let Some(raw) = raw_source {
        markers.extend(raw_pcb_edge_markers(raw));
    }
    if markers.is_empty() {
        return None;
    }

    ast.graphics
        .iter()
        .filter(|g| g.token == "fp_line" && g.layer.as_deref() == Some("Dwgs.User"))
        .filter_map(|g| {
            let start = Point2::from(g.start?);
            let end = Point2::from(g.end?);
            let line = geom::Segment::new(start, end);
            (line.length() > geom::EPS).then(|| {
                let distance = markers
                    .iter()
                    .map(|&marker| line.dist2_to_point(marker))
                    .fold(f64::INFINITY, f64::min);
                (distance, PcbEdgeDatum { start, end })
            })
        })
        .min_by(|(ad, a), (bd, b)| {
            ad.total_cmp(bd)
                .then_with(|| a.start.x.total_cmp(&b.start.x))
                .then_with(|| a.start.y.total_cmp(&b.start.y))
                .then_with(|| a.end.x.total_cmp(&b.end.x))
                .then_with(|| a.end.y.total_cmp(&b.end.y))
        })
        .map(|(_, datum)| datum)
}

/// `kiutils` 0.3 records the `fp_text` subtype (`user`) as its text field, so
/// recover labelled marker positions from the raw block until the upstream AST
/// exposes the following text atom. This parser is deliberately narrow: only
/// `fp_text` blocks containing both the exact label and `Dwgs.User` qualify.
fn raw_pcb_edge_markers(raw: &str) -> Vec<Point2> {
    let mut markers = Vec::new();
    let mut search = 0;
    while let Some(rel) = raw[search..].find("(fp_text") {
        let start = search + rel;
        let Some(end) = matching_paren(raw, start) else {
            break;
        };
        let block = &raw[start..end];
        search = end;
        if !block.contains("\"PCB Edge\"") || !block.contains("\"Dwgs.User\"") {
            continue;
        }
        let Some(at_rel) = block.find("(at ") else {
            continue;
        };
        let mut values = block[at_rel + "(at ".len()..].split_whitespace();
        let (Some(x), Some(y)) = (values.next(), values.next()) else {
            continue;
        };
        if let (Ok(x), Ok(y)) = (x.parse::<f64>(), y.trim_end_matches(')').parse::<f64>()) {
            markers.push(Point2::new(x, y));
        }
    }
    markers
}

/// Per custom pad, the primitive half-extents in file order.
pub(crate) fn custom_pad_half_extents(raw: &str) -> Vec<Point2> {
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(rel) = raw[search..].find("(pad ") {
        let start = search + rel;
        let Some(end) = matching_paren(raw, start) else {
            break;
        };
        let block = &raw[start..end];
        search = end;
        if !block.contains(" custom") {
            continue;
        }
        let mut points = Vec::new();
        let mut p = 0;
        while let Some(r) = block[p..].find("(xy ") {
            let s = p + r + "(xy ".len();
            let mut it = block[s..].split_whitespace();
            if let (Some(xs), Some(ys)) = (it.next(), it.next())
                && let (Ok(x), Ok(y)) = (xs.parse::<f64>(), ys.trim_end_matches(')').parse::<f64>())
            {
                points.push(Point2::new(x, y));
            }
            p = s;
        }
        if let Some(bounds) = Rect::bounding(&points) {
            out.push(Point2::new(
                bounds.min_x.abs().max(bounds.max_x.abs()),
                bounds.min_y.abs().max(bounds.max_y.abs()),
            ));
        } else {
            out.push(Point2::new(0.0, 0.0));
        }
    }
    out
}

/// Byte index just past the `)` that closes the `(` at `open`.
pub(crate) fn matching_paren(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    for (i, &byte) in b.iter().enumerate().skip(open) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

fn pad_detail(pad: &kiutils_kicad::FpPad) -> FootprintPad {
    let technology = match pad.pad_type.as_deref() {
        Some("smd") => PadTechnology::Smd,
        Some("thru_hole") => PadTechnology::ThruHole,
        Some("np_thru_hole") => PadTechnology::NpThruHole,
        _ => PadTechnology::Other,
    };
    FootprintPad {
        number: pad.number.clone().unwrap_or_default(),
        at: pad.at.map(Point2::from).unwrap_or(Point2::new(0.0, 0.0)),
        rotation: pad.rotation.unwrap_or(0.0),
        size: pad.size.map(Point2::from).unwrap_or(Point2::new(0.0, 0.0)),
        shape: pad.shape.clone().unwrap_or_default(),
        layers: pad.layers.clone(),
        technology,
        drill: pad.drill.as_ref().and_then(|d| d.diameter),
    }
}

fn pad_corners(pad: &FootprintPad) -> [Point2; 2] {
    let half = Point2::new(pad.size.x / 2.0, pad.size.y / 2.0).rotated_half_extents(pad.rotation);
    [
        Point2::new(pad.at.x - half.x, pad.at.y - half.y),
        Point2::new(pad.at.x + half.x, pad.at.y + half.y),
    ]
}

fn graphic_points(g: &kiutils_kicad::FpGraphic) -> Vec<Point2> {
    let mut pts: Vec<Point2> = [g.start, g.end, g.center, g.at]
        .into_iter()
        .flatten()
        .map(Point2::from)
        .collect();
    if g.token == "fp_arc"
        && let (Some(s), Some(e)) = (g.start, g.end)
    {
        let chord = geom::Segment::new(s.into(), e.into());
        let mid = chord.midpoint();
        let r = chord.length() / 2.0;
        pts.push(Point2::new(mid.x - r, mid.y - r));
        pts.push(Point2::new(mid.x + r, mid.y + r));
    }
    if g.token == "fp_circle"
        && let (Some(c), Some(e)) = (g.center.or(g.start), g.end)
    {
        let center = Point2::from(c);
        let r = center.dist(e.into());
        pts.push(Point2::new(center.x - r, center.y - r));
        pts.push(Point2::new(center.x + r, center.y + r));
    }
    pts
}

fn courtyard_bbox(
    ast: &kiutils_kicad::FootprintAst,
    pads: &[FootprintPad],
) -> (Rect, CourtyardSource) {
    let mut crtyd: Vec<Point2> = Vec::new();
    for g in &ast.graphics {
        if matches!(g.layer.as_deref(), Some("F.CrtYd") | Some("B.CrtYd")) {
            crtyd.extend(graphic_points(g));
        }
    }
    if let Some(b) = Rect::bounding(&crtyd) {
        return (b, CourtyardSource::ExplicitCourtyard);
    }

    let mut pts: Vec<Point2> = Vec::new();
    for pad in pads {
        pts.extend(pad_corners(pad));
    }
    for g in &ast.graphics {
        if g.layer.as_deref().is_some_and(|l| l.ends_with(".SilkS")) {
            pts.extend(graphic_points(g));
        }
    }
    (
        Rect::bounding(&pts).unwrap_or_else(Rect::zero),
        CourtyardSource::EstimatedFromPadsAndSilkscreen,
    )
}

fn overall_bbox(ast: &kiutils_kicad::FootprintAst, pads: &[FootprintPad]) -> Option<Rect> {
    let mut pts: Vec<Point2> = Vec::new();
    for pad in pads {
        pts.extend(pad_corners(pad));
    }
    for g in &ast.graphics {
        pts.extend(graphic_points(g));
    }
    Rect::bounding(&pts)
}

fn map_kiutils_err(path: &Path, e: kiutils_kicad::Error) -> Error {
    match e {
        kiutils_kicad::Error::Io(io) => Error::Io {
            path: path.to_path_buf(),
            source: io,
        },
        other => Error::Parse {
            path: path.to_path_buf(),
            message: other.to_string(),
        },
    }
}
