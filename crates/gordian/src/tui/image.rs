//! Inline render cells: a PNG the run wrote, drawn straight into the transcript.
//!
//! A terminal cell is about twice as tall as it is wide, so each cell carries
//! two pixels — the upper half block `▀` painted in the top pixel's colour over
//! a background of the bottom pixel's. Decoding and [`reduce`] are the expensive
//! part and the transcript redraws on every keystroke, so a cell is cached by
//! (path, width) and rebuilt only when the pane resizes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// The half block whose foreground is the upper pixel.
const UPPER: &str = "▀";

/// Tallest an inline render may be, in rows. A schematic is wider than it is
/// tall, so this only bites on a narrow pane.
const MAX_ROWS: u16 = 22;

/// A decoded image, sized for one pane width.
struct Cell {
    width: u16,
    lines: Vec<Line<'static>>,
}

/// Decoded renders, keyed by path.
#[derive(Default)]
pub struct Images {
    cells: HashMap<PathBuf, Cell>,
}

impl Images {
    /// The rows drawing `path` at `width` columns, or an empty vector when the
    /// file cannot be read as a PNG.
    pub fn rows(&mut self, path: &Path, width: u16) -> &[Line<'static>] {
        let stale = self.cells.get(path).is_none_or(|c| c.width != width);
        if stale {
            let lines = decode(path, width).unwrap_or_default();
            self.cells.insert(path.to_path_buf(), Cell { width, lines });
        }
        &self.cells[path].lines
    }

    /// Forget a file whose bytes changed on disk, so the next draw re-decodes it.
    pub fn invalidate(&mut self, path: &Path) {
        self.cells.remove(path);
    }
}

/// One RGB pixel grid.
struct Pixels {
    w: usize,
    h: usize,
    rgb: Vec<[u8; 3]>,
}

impl Pixels {
    fn at(&self, x: usize, y: usize) -> [u8; 3] {
        self.rgb[y.min(self.h - 1) * self.w + x.min(self.w - 1)]
    }
}

/// Read `path` and lay it out as half-block rows `cols` wide.
fn decode(path: &Path, cols: u16) -> Option<Vec<Line<'static>>> {
    let cols = cols.max(1) as usize;
    let source = read_png(path)?;
    // Two pixel rows per terminal row, and a cell is ~half as wide as it is tall.
    let aspect = source.h as f64 / source.w as f64;
    let mut rows = ((cols as f64 * aspect) / 2.0).round().max(1.0) as usize;
    // A tall picture is narrowed rather than squashed: clamping the rows alone
    // would stretch a schematic sideways.
    let cols = match rows > MAX_ROWS as usize {
        true => {
            rows = MAX_ROWS as usize;
            ((rows * 2) as f64 / aspect).round().max(1.0) as usize
        }
        false => cols,
    };
    let scaled = reduce(&source, cols, rows * 2);

    Some(
        (0..rows)
            .map(|row| {
                let spans = (0..cols)
                    .map(|col| {
                        let top = scaled.at(col, row * 2);
                        let bottom = scaled.at(col, row * 2 + 1);
                        Span::styled(
                            UPPER,
                            Style::new().fg(colour(top)).bg(colour(bottom)),
                        )
                    })
                    .collect::<Vec<_>>();
                Line::from(spans)
            })
            .collect(),
    )
}

fn colour([r, g, b]: [u8; 3]) -> ratatui::style::Color {
    ratatui::style::Color::Rgb(r, g, b)
}

/// Decode a PNG to 8-bit RGB, compositing any alpha onto white — a schematic
/// export is transparent where the paper is, and black paper hides the drawing.
fn read_png(path: &Path) -> Option<Pixels> {
    let file = std::fs::File::open(path).ok()?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let channels = info.color_type.samples();
    let has_alpha = matches!(
        info.color_type,
        png::ColorType::Rgba | png::ColorType::GrayscaleAlpha
    );

    let mut rgb = Vec::with_capacity(w * h);
    for pixel in buf[..w * h * channels].chunks_exact(channels) {
        let (r, g, b) = match channels {
            1 | 2 => (pixel[0], pixel[0], pixel[0]),
            _ => (pixel[0], pixel[1], pixel[2]),
        };
        let over_white = |c: u8, a: u8| {
            let a = a as u32;
            ((c as u32 * a + 255 * (255 - a)) / 255) as u8
        };
        rgb.push(match has_alpha {
            true => {
                let a = pixel[channels - 1];
                [over_white(r, a), over_white(g, a), over_white(b, a)]
            }
            false => [r, g, b],
        });
    }
    (w > 0 && h > 0).then_some(Pixels { w, h, rgb })
}

/// Shrink to `w`×`h` by keeping, from each source box, the pixel whose
/// brightness is furthest from the picture's own average.
///
/// Averaging the box instead — the obvious choice — is what makes a downsampled
/// schematic vanish: one dark wire against a box of white paper averages to
/// pale grey. Keeping the outlier holds the ink on light artwork and the traces
/// on a dark board alike, without either being special-cased.
fn reduce(source: &Pixels, w: usize, h: usize) -> Pixels {
    let background = source.rgb.iter().map(|p| luma(*p) as u64).sum::<u64>()
        / source.rgb.len().max(1) as u64;
    let mut rgb = Vec::with_capacity(w * h);
    for y in 0..h {
        let y0 = y * source.h / h;
        let y1 = ((y + 1) * source.h).div_ceil(h).max(y0 + 1).min(source.h);
        for x in 0..w {
            let x0 = x * source.w / w;
            let x1 = ((x + 1) * source.w).div_ceil(w).max(x0 + 1).min(source.w);
            let mut best = source.at(x0, y0);
            let mut furthest = 0;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = source.at(sx, sy);
                    let distance = (luma(p) as i64 - background as i64).unsigned_abs();
                    if distance >= furthest {
                        furthest = distance;
                        best = p;
                    }
                }
            }
            rgb.push(best);
        }
    }
    Pixels { w, h, rgb }
}

/// Perceived brightness, for picking the pixel that carries the detail.
fn luma([r, g, b]: [u8; 3]) -> u32 {
    (2 * r as u32 + 5 * g as u32 + b as u32) / 8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a `w`×`h` RGB PNG whose left half is red and right half is blue.
    fn two_tone_png(dir: &Path, w: u32, h: u32) -> PathBuf {
        let path = dir.join("two-tone.png");
        let mut data = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                data.extend_from_slice(if x < w / 2 {
                    &[255u8, 0, 0]
                } else {
                    &[0u8, 0, 255]
                });
            }
        }
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&data)
            .unwrap();
        path
    }

    #[test]
    fn a_png_becomes_half_block_rows_that_keep_its_colours() {
        let dir = tempfile::tempdir().unwrap();
        let png = two_tone_png(dir.path(), 40, 20);
        let mut images = Images::default();

        let rows = images.rows(&png, 20).to_vec();

        assert!(!rows.is_empty());
        assert!(rows.iter().all(|l| l.spans.len() == 20));
        let first = &rows[0].spans;
        assert_eq!(first[0].style.fg, Some(ratatui::style::Color::Rgb(255, 0, 0)));
        assert_eq!(
            first[19].style.fg,
            Some(ratatui::style::Color::Rgb(0, 0, 255))
        );
    }

    /// The aspect ratio survives the half-block packing: a 2:1 image at 20
    /// columns is 5 rows (20 columns × 0.5 aspect ÷ 2 pixel rows per cell).
    #[test]
    fn the_row_count_follows_the_aspect_ratio() {
        let dir = tempfile::tempdir().unwrap();
        let png = two_tone_png(dir.path(), 40, 20);
        let mut images = Images::default();
        assert_eq!(images.rows(&png, 20).len(), 5);
    }

    /// A picture too tall for the pane is narrowed, never squashed: the rows
    /// stop at the cap and the columns follow the aspect down.
    #[test]
    fn a_tall_picture_is_narrowed_rather_than_stretched() {
        let dir = tempfile::tempdir().unwrap();
        let png = two_tone_png(dir.path(), 20, 200);
        let mut images = Images::default();

        let rows = images.rows(&png, 60).to_vec();

        assert_eq!(rows.len(), MAX_ROWS as usize);
        let cols = rows[0].spans.len();
        assert_eq!(cols, 4, "20:200 at 44 pixel rows is 4 columns wide");
    }

    #[test]
    fn a_file_that_is_not_a_png_draws_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.png");
        std::fs::write(&path, b"not a png").unwrap();
        assert!(Images::default().rows(&path, 20).is_empty());
    }
}
