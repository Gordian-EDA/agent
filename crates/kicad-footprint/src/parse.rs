use std::io;
use std::path::Path;

use geom::{Point2, Rect};

use crate::{BBox, CourtyardSource, Footprint, FootprintPad, PadTechnology};

impl Footprint {
    /// Parse a single `.kicad_mod` file into a [`Footprint`].
    ///
    /// The bare footprint name is taken from the file stem, which is canonical
    /// for `.pretty` libraries: the filename is the footprint name.
    pub fn load(path: &Path) -> io::Result<Footprint> {
        let doc = kiutils_kicad::FootprintFile::read(path).map_err(map_kiutils_err)?;
        let ast = doc.ast();

        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();

        let mut pads: Vec<FootprintPad> = ast.pads.iter().map(pad_detail).collect();
        if pads.iter().any(|p| p.shape == "custom")
            && let Ok(raw) = std::fs::read_to_string(path)
        {
            let bboxes = custom_pad_bboxes(&raw);
            for (bi, pad) in pads.iter_mut().filter(|p| p.shape == "custom").enumerate() {
                if let Some(&(hx, hy)) = bboxes.get(bi) {
                    pad.size = [pad.size[0].max(2.0 * hx), pad.size[1].max(2.0 * hy)];
                }
            }
        }
        let (courtyard, courtyard_source) = courtyard_bbox(ast, &pads);
        let bbox = overall_bbox(ast, &pads).unwrap_or_else(BBox::zero);

        Ok(Footprint {
            name,
            descr: ast.descr.clone(),
            pads,
            courtyard,
            courtyard_source,
            bbox,
        })
    }
}

/// Per custom pad (in file order), the primitive half-extents `(hx, hy)`.
pub(crate) fn custom_pad_bboxes(raw: &str) -> Vec<(f64, f64)> {
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
            out.push((
                bounds.min_x.abs().max(bounds.max_x.abs()),
                bounds.min_y.abs().max(bounds.max_y.abs()),
            ));
        } else {
            out.push((0.0, 0.0));
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
        at: pad.at.unwrap_or([0.0, 0.0]),
        rotation: pad.rotation.unwrap_or(0.0),
        size: pad.size.unwrap_or([0.0, 0.0]),
        shape: pad.shape.clone().unwrap_or_default(),
        layers: pad.layers.clone(),
        technology,
        drill: pad.drill.as_ref().and_then(|d| d.diameter),
    }
}

fn pad_corners(pad: &FootprintPad) -> [Point2; 2] {
    let [cx, cy] = pad.at;
    let [w, h] = pad.size;
    let (hw, hh) = geom::rotated_aabb_half(w, h, pad.rotation);
    [Point2::new(cx - hw, cy - hh), Point2::new(cx + hw, cy + hh)]
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
) -> (BBox, CourtyardSource) {
    let mut crtyd: Vec<Point2> = Vec::new();
    for g in &ast.graphics {
        if matches!(g.layer.as_deref(), Some("F.CrtYd") | Some("B.CrtYd")) {
            crtyd.extend(graphic_points(g));
        }
    }
    if let Some(b) = BBox::from_points(&crtyd) {
        return (b, CourtyardSource::Crtyd);
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
        BBox::from_points(&pts).unwrap_or_else(BBox::zero),
        CourtyardSource::PadSilkFallback,
    )
}

fn overall_bbox(ast: &kiutils_kicad::FootprintAst, pads: &[FootprintPad]) -> Option<BBox> {
    let mut pts: Vec<Point2> = Vec::new();
    for pad in pads {
        pts.extend(pad_corners(pad));
    }
    for g in &ast.graphics {
        pts.extend(graphic_points(g));
    }
    BBox::from_points(&pts)
}

fn map_kiutils_err(e: kiutils_kicad::Error) -> io::Error {
    match e {
        kiutils_kicad::Error::Io(io) => io,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}
