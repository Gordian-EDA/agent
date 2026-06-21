//! Footprint-library discovery, `.kicad_mod` parsing, and cross-library
//! footprint search — the placement-side analog of [`crate::symlib`] +
//! [`crate::search`].
//!
//! ## Discovery
//!
//! KiCAD ships footprints as `*.pretty` **directories** of individual
//! `.kicad_mod` files under the install's `footprints` share dir (the sibling
//! of the `symbols` dir [`crate::env`] already finds). We mirror `symlib`'s
//! filesystem-scan approach — enumerate the `.pretty` dirs and the
//! `.kicad_mod` files inside them — rather than parsing `fp-lib-table`, exactly
//! as `symlib` scans `*.kicad_sym` files instead of `sym-lib-table`. (The
//! shipped `fp-lib-table` is only a user-config *template*; the on-disk layout
//! is authoritative.)
//!
//! A library's *nickname* is its `.pretty` directory stem (e.g. `Resistor_SMD`)
//! and a footprint's fully-qualified id is `Nickname:Name` — the same
//! `Lib:Name` convention [`crate::search::SymbolIndex`] uses for symbols and the
//! same string KiCAD stores in a board footprint's `lib_id`.
//!
//! ## Parsing
//!
//! `.kicad_mod` parsing is **native** via [`kiutils_kicad::FootprintFile`]
//! (no hand-rolled s-expr extraction): its [`kiutils_kicad::FootprintAst`]
//! exposes `pads` and `graphics` typed exactly like the board-side
//! [`kiutils_kicad::PcbFootprint`]/[`kiutils_kicad::PcbPad`] in [`crate::pcb`],
//! so the parsed data is shaped to serve the same placement/write-back
//! consumers.
//!
//! ## Search
//!
//! [`FootprintIndex`] mirrors [`crate::search::SymbolIndex`] one-for-one:
//! names-only index built up front, fuzzy [`FootprintIndex::search`] ranked by
//! `fuzzy-matcher`'s `SkimMatcherV2` with edit-distance backfill, and identical
//! normalization / deterministic tie-breaks. Per the project convention this
//! reuses that ranking rather than introducing a second fuzzy matcher.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use serde::{Deserialize, Serialize};

use crate::env::KicadEnv;

/// Maximum levenshtein distance for a name to qualify as a "did-you-mean"
/// suggestion. Mirrors [`crate::provider`]'s symbol-side constant.
const SUGGEST_MAX_DISTANCE: usize = 6;
/// Maximum number of "did-you-mean" suggestions returned.
const SUGGEST_LIMIT: usize = 3;

// ── parsed footprint detail ──────────────────────────────────────────────────

/// Through-hole vs. surface-mount, derived from a pad's KiCAD `pad_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PadTechnology {
    /// `smd` — surface-mount; lives on one copper face.
    Smd,
    /// `thru_hole` — drilled; spans the whole copper stack.
    ThruHole,
    /// `np_thru_hole` — non-plated mechanical hole (no copper).
    NpThruHole,
    /// `connect` or anything else KiCAD may add — treated conservatively as
    /// surface copper by consumers that must pick.
    Other,
}

/// A 2-D axis-aligned bounding box in the footprint's own (unplaced) frame,
/// millimetres, KiCAD y-down. `None`-yielding parses collapse to a zero box at
/// the origin so consumers never branch on emptiness.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BBox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl BBox {
    /// A degenerate box at the origin.
    pub fn zero() -> Self {
        BBox { min_x: 0.0, min_y: 0.0, max_x: 0.0, max_y: 0.0 }
    }

    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    fn from_points(pts: &[[f64; 2]]) -> Option<Self> {
        let mut it = pts.iter();
        let &[x0, y0] = it.next()?;
        let mut b = BBox { min_x: x0, min_y: y0, max_x: x0, max_y: y0 };
        for &[x, y] in it {
            b.min_x = b.min_x.min(x);
            b.min_y = b.min_y.min(y);
            b.max_x = b.max_x.max(x);
            b.max_y = b.max_y.max(y);
        }
        Some(b)
    }
}

/// How the courtyard bbox was obtained — recorded so a consumer that needs a
/// trustworthy keep-out can tell a real `*.CrtYd` outline from the documented
/// pad+silk fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CourtyardSource {
    /// Bounding box of the `F.CrtYd` / `B.CrtYd` graphics actually present.
    Crtyd,
    /// No courtyard layer present: bbox of pads plus silkscreen graphics
    /// (documented fallback — may underestimate the true keep-out).
    PadSilkFallback,
}

/// One pad of a footprint, reference-designator-agnostic — i.e. as the library
/// defines it, before placement assigns nets/positions on a board. Offsets are
/// in the footprint's own unrotated frame, matching [`kiutils_kicad::PcbPad`]
/// so placement can translate/rotate them onto a board exactly like
/// [`crate::pcb`] already does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FootprintPad {
    /// Pad number/name as a string (`"1"`, `"A1"`, `"GND"`); may repeat.
    pub number: String,
    /// Centre offset `[x, y]` in the footprint frame, millimetres.
    pub at: [f64; 2],
    /// Local pad rotation in degrees, if the file specifies one.
    pub rotation: f64,
    /// Pad copper size `[w, h]`, millimetres.
    pub size: [f64; 2],
    /// KiCAD pad shape token (`rect`, `roundrect`, `circle`, `oval`, …).
    pub shape: String,
    /// Copper/technical layers the pad occupies (`F.Cu`, `*.Cu`, …).
    pub layers: Vec<String>,
    /// Mounting technology (SMD / through-hole / …).
    pub technology: PadTechnology,
    /// Drill diameter in millimetres for a through-hole pad, else `None`.
    pub drill: Option<f64>,
}

/// A fully parsed footprint: everything placement needs to instantiate it onto a
/// board, independent of any board context. Serializable so it can be cached or
/// shipped across a tool boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Footprint {
    /// Bare footprint name (the `.kicad_mod` stem / the `(footprint "NAME")`).
    pub name: String,
    /// Free-text description from `(descr …)`, if any.
    pub descr: Option<String>,
    /// Reference-designator-agnostic pad list.
    pub pads: Vec<FootprintPad>,
    /// Courtyard bounding box (placement keep-out).
    pub courtyard: BBox,
    /// How [`Self::courtyard`] was derived.
    pub courtyard_source: CourtyardSource,
    /// Overall bounding box over every pad and graphic element.
    pub bbox: BBox,
}

impl Footprint {
    /// Parse a single `.kicad_mod` file into a [`Footprint`].
    ///
    /// Uses [`kiutils_kicad::FootprintFile`] natively. The bare footprint name
    /// is taken from the file stem, which is canonical for `.pretty` libraries:
    /// the filename *is* the footprint name (the modern format stores no
    /// separate bare-name token on the `(footprint …)` node).
    pub fn load(path: &Path) -> io::Result<Footprint> {
        let doc = kiutils_kicad::FootprintFile::read(path).map_err(map_kiutils_err)?;
        let ast = doc.ast();

        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();

        let mut pads: Vec<FootprintPad> = ast.pads.iter().map(pad_detail).collect();
        // kiutils 0.3 exposes only a COUNT of a custom pad's primitives, not their
        // geometry — so a custom pad (e.g. an FFC connector's polygon mounting tab)
        // would be modelled by its tiny base anchor, under-sizing the real copper.
        // The placer/router/outline would then seat parts too close or crop the
        // board outline inside the actual pad (a copper_edge_clearance fault KiCAD
        // catches). Re-parse each custom pad's primitive bounding box from the raw
        // source and grow the pad to it. (Same kiutils-drops-geometry class as the
        // fp_arc/fp_circle courtyard fixes.)
        if pads.iter().any(|p| p.shape == "custom")
            && let Ok(raw) = std::fs::read_to_string(path) {
                let bboxes = custom_pad_bboxes(&raw);
                let mut bi = 0;
                for pad in pads.iter_mut().filter(|p| p.shape == "custom") {
                    if let Some(&(hx, hy)) = bboxes.get(bi) {
                        pad.size = [pad.size[0].max(2.0 * hx), pad.size[1].max(2.0 * hy)];
                    }
                    bi += 1;
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

    /// Number of pads — the count placement validates against a symbol's pins.
    pub fn pad_count(&self) -> usize {
        self.pads.len()
    }
}

/// Per custom pad (in file order), the half-extents `(hx, hy)` of its primitive
/// polygon, parsed from the raw `.kicad_mod` source (kiutils 0.3 drops these).
/// Primitive points are relative to the pad origin, so the centred-rect model's
/// half-extent on each axis is the max absolute coordinate. Covers `(xy …)`
/// points (gr_poly / gr_line) — the shape FFC/FPC and similar custom pads use.
pub(crate) fn custom_pad_bboxes(raw: &str) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(rel) = raw[search..].find("(pad ") {
        let start = search + rel;
        let Some(end) = matching_paren(raw, start) else { break };
        let block = &raw[start..end];
        search = end;
        if !block.contains(" custom") {
            continue;
        }
        let (mut hx, mut hy) = (0.0_f64, 0.0_f64);
        let mut p = 0;
        while let Some(r) = block[p..].find("(xy ") {
            let s = p + r + "(xy ".len();
            let mut it = block[s..].split_whitespace();
            if let (Some(xs), Some(ys)) = (it.next(), it.next())
                && let (Ok(x), Ok(y)) =
                    (xs.parse::<f64>(), ys.trim_end_matches(')').parse::<f64>())
                {
                    hx = hx.max(x.abs());
                    hy = hy.max(y.abs());
                }
            p = s;
        }
        out.push((hx, hy));
    }
    out
}

/// Byte index just past the `)` that closes the `(` at `open`.
pub(crate) fn matching_paren(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    for i in open..b.len() {
        match b[i] {
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

/// Translate one [`kiutils_kicad::FpPad`] into a [`FootprintPad`], defaulting
/// missing geometry to zero the way [`crate::pcb`] does for board pads.
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

/// Corner points of a pad's axis-aligned copper rectangle in the footprint
/// frame (local pad rotation folded in via an enclosing AABB, matching
/// [`crate::pcb`]'s v1 conservatism).
fn pad_corners(pad: &FootprintPad) -> [[f64; 2]; 2] {
    let [cx, cy] = pad.at;
    let [w, h] = pad.size;
    let (hw, hh) = rotated_aabb_half(w, h, pad.rotation);
    [[cx - hw, cy - hh], [cx + hw, cy + hh]]
}

/// Axis-aligned half-extents of a `w × h` rectangle rotated `deg` degrees.
/// Identical formula to [`crate::pcb`]'s `rotated_aabb_half`.
fn rotated_aabb_half(w: f64, h: f64, deg: f64) -> (f64, f64) {
    let theta = deg.to_radians();
    let (s, c) = theta.sin_cos();
    let hw = (w / 2.0 * c).abs() + (h / 2.0 * s).abs();
    let hh = (w / 2.0 * s).abs() + (h / 2.0 * c).abs();
    (hw, hh)
}

/// All defined geometry points of a graphic (`start`/`end`/`center`/`at`).
fn graphic_points(g: &kiutils_kicad::FpGraphic) -> Vec<[f64; 2]> {
    let mut pts: Vec<[f64; 2]> = [g.start, g.end, g.center, g.at].into_iter().flatten().collect();
    // An `fp_arc` bulges BEYOND its endpoints, but kiutils 0.3 drops the `(mid)`
    // apex — so a rounded courtyard (a crystal's curved end, a round connector)
    // would be read only to its chord and the bbox under-sized, seating parts too
    // close (a courtyard-overlap DRC fault KiCAD catches). Bound the arc
    // conservatively by a square of side = the chord length centred on the chord
    // midpoint; this contains any arc up to a semicircle, which every courtyard
    // arc is. (Recovers the HC49 crystal's true 8.47mm extent from start/end.)
    if g.token == "fp_arc"
        && let (Some(s), Some(e)) = (g.start, g.end) {
            let mid = [(s[0] + e[0]) / 2.0, (s[1] + e[1]) / 2.0];
            let r = ((s[0] - e[0]).powi(2) + (s[1] - e[1]).powi(2)).sqrt() / 2.0;
            pts.push([mid[0] - r, mid[1] - r]);
            pts.push([mid[0] + r, mid[1] + r]);
        }
    // An `fp_circle` ((center) + an `(end)` point on the circumference) bounds a
    // disc of that radius — but only the two stored points would be read, missing
    // the ±radius extent on the other quadrants. A radial cap / round footprint's
    // circular courtyard would then be badly under-sized (the D8 electrolytic's
    // 8mm courtyard read as a sliver). Add the disc's bounding box.
    if g.token == "fp_circle"
        && let (Some(c), Some(e)) = (g.center.or(g.start), g.end) {
            let r = ((c[0] - e[0]).powi(2) + (c[1] - e[1]).powi(2)).sqrt();
            pts.push([c[0] - r, c[1] - r]);
            pts.push([c[0] + r, c[1] + r]);
        }
    pts
}

/// Courtyard bounding box. Primary source is the bbox of every `F.CrtYd` /
/// `B.CrtYd` graphic; if the footprint has no courtyard layer at all we fall
/// back to the bbox of pads plus silkscreen (`*.SilkS`) — documented as a
/// possible underestimate of the true keep-out.
fn courtyard_bbox(
    ast: &kiutils_kicad::FootprintAst,
    pads: &[FootprintPad],
) -> (BBox, CourtyardSource) {
    let mut crtyd: Vec<[f64; 2]> = Vec::new();
    for g in &ast.graphics {
        if matches!(g.layer.as_deref(), Some("F.CrtYd") | Some("B.CrtYd")) {
            crtyd.extend(graphic_points(g));
        }
    }
    if let Some(b) = BBox::from_points(&crtyd) {
        return (b, CourtyardSource::Crtyd);
    }

    // Fallback: pad corners + silkscreen graphic points.
    let mut pts: Vec<[f64; 2]> = Vec::new();
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

/// Overall bbox over every pad rectangle and every graphic point.
fn overall_bbox(
    ast: &kiutils_kicad::FootprintAst,
    pads: &[FootprintPad],
) -> Option<BBox> {
    let mut pts: Vec<[f64; 2]> = Vec::new();
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

// ── library discovery / lazy load ────────────────────────────────────────────

/// The `footprints` share directory for a [`KicadEnv`]: the sibling of its
/// `symbol_dir` (`…/share/kicad/symbols` → `…/share/kicad/footprints`).
fn footprint_dir(env: &KicadEnv) -> PathBuf {
    env.symbol_dir
        .parent()
        .map(|p| p.join("footprints"))
        .unwrap_or_else(|| PathBuf::from("footprints"))
}

/// Enumerate the `Nickname → .pretty path` of every installed footprint
/// library, sorted by nickname for determinism. A `.pretty` whose stem is not
/// valid UTF-8 is skipped.
fn discover_libraries(footprint_dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut libs: Vec<(String, PathBuf)> = std::fs::read_dir(footprint_dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.extension().is_some_and(|x| x == "pretty"))
        .filter_map(|p| {
            let nick = p.file_stem()?.to_str()?.to_string();
            Some((nick, p))
        })
        .collect();
    libs.sort();
    Ok(libs)
}

// ── footprint index (search) ─────────────────────────────────────────────────

/// A search hit: a fully qualified `Nickname:Name` id and its pad count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FootprintHit {
    pub lib_id: String,
    pub pad_count: usize,
}

/// One indexed footprint, pre-normalized for ranking (mirrors
/// [`crate::search`]'s private `Entry`).
struct Entry {
    lib_id: String,
    /// `.kicad_mod` path, for lazy detail parsing of returned hits.
    path: PathBuf,
    /// Lowercased `lib_id` with non-alphanumeric runs collapsed to spaces.
    normalized: String,
}

/// Name index over every footprint in every installed `.pretty` library — the
/// placement-side analog of [`crate::search::SymbolIndex`].
pub struct FootprintIndex {
    entries: Vec<Entry>,
    /// nickname → `.pretty` path, for [`Self::libraries`] / [`Self::suggest`].
    libraries: Vec<(String, PathBuf)>,
    /// lib_id → memoized parsed footprint (lazy, for returned hits).
    cache: Mutex<HashMap<String, Option<Footprint>>>,
}

impl FootprintIndex {
    /// Scan every `*.pretty/*.kicad_mod` under the environment's footprint
    /// directory and index footprint names. Names only — no `.kicad_mod`
    /// parsing at build time (detail is resolved lazily for hits).
    pub fn build(env: &KicadEnv) -> io::Result<FootprintIndex> {
        Self::build_from_dir(&footprint_dir(env))
    }

    /// Build directly from a footprints directory (a dir of `.pretty` dirs) —
    /// the test/override entry point, analogous to
    /// [`KicadEnv::with_symbol_dir`].
    pub fn build_from_dir(footprint_dir: &Path) -> io::Result<FootprintIndex> {
        let libraries = discover_libraries(footprint_dir)?;
        let mut entries = Vec::new();

        for (nick, dir) in &libraries {
            // A single unreadable library must not take down the whole index.
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut names: Vec<(String, PathBuf)> = rd
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "kicad_mod"))
                .filter_map(|p| {
                    let name = p.file_stem()?.to_str()?.to_string();
                    Some((name, p))
                })
                .collect();
            names.sort(); // deterministic order, stable tie-breaks
            for (name, path) in names {
                let lib_id = format!("{nick}:{name}");
                entries.push(Entry {
                    normalized: normalize(&lib_id),
                    lib_id,
                    path,
                });
            }
        }

        Ok(FootprintIndex {
            entries,
            libraries,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Number of indexed footprints.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of discovered `.pretty` libraries.
    pub fn library_count(&self) -> usize {
        self.libraries.len()
    }

    /// The discovered library nicknames, sorted.
    pub fn libraries(&self) -> impl Iterator<Item = &str> {
        self.libraries.iter().map(|(nick, _)| nick.as_str())
    }

    /// Fully-qualified `Nickname:Name` ids of every footprint in `nickname`,
    /// in indexed (sorted) order. Empty if the nickname is unknown.
    pub fn footprints_in(&self, nickname: &str) -> Vec<&str> {
        let prefix = format!("{nickname}:");
        self.entries
            .iter()
            .filter(|e| e.lib_id.starts_with(&prefix))
            .map(|e| e.lib_id.as_str())
            .collect()
    }

    /// Return the `n` best matches for `query`, best first. Ranking and
    /// determinism are identical to [`crate::search::SymbolIndex::search`];
    /// pad counts are resolved lazily for the returned hits only.
    pub fn search(&self, query: &str, n: usize) -> Vec<FootprintHit> {
        let needle = normalize(query);
        if needle.is_empty() {
            return Vec::new();
        }
        rank(&self.entries, &needle, n)
            .into_iter()
            .map(|i| {
                let lib_id = self.entries[i].lib_id.clone();
                let pad_count = self.footprint(&lib_id).map_or(0, |fp| fp.pad_count());
                FootprintHit { lib_id, pad_count }
            })
            .collect()
    }

    /// Parse (and memoize) the footprint for a `Nickname:Name` id, returning a
    /// reference into the cache. `None` if the id is unknown or fails to parse.
    ///
    /// The lock is released before returning; the cached `Footprint` is cloned
    /// out rather than borrowed, mirroring the symbol provider's memoization
    /// without needing address stability.
    pub fn footprint(&self, lib_id: &str) -> Option<Footprint> {
        {
            let cache = self.cache.lock().expect("footprint cache poisoned");
            if let Some(slot) = cache.get(lib_id) {
                return slot.clone();
            }
        }
        let parsed = self
            .entries
            .iter()
            .find(|e| e.lib_id == lib_id)
            .and_then(|e| Footprint::load(&e.path).ok());
        let mut cache = self.cache.lock().expect("footprint cache poisoned");
        cache.insert(lib_id.to_string(), parsed.clone());
        parsed
    }

    /// The `.kicad_mod` file path for a `Nickname:Name` id, if the id is known.
    /// Board synthesis ([`crate::synth::synthesize_board`]) reads the raw source
    /// text to transform the footprint body, so it needs the on-disk path that
    /// [`Footprint::load`] already resolves internally.
    pub fn footprint_path(&self, lib_id: &str) -> Option<&Path> {
        self.entries
            .iter()
            .find(|e| e.lib_id == lib_id)
            .map(|e| e.path.as_path())
    }

    /// The raw `.kicad_mod` source text for a `Nickname:Name` id, if known and
    /// readable. The single description board synthesis transforms (the same file
    /// [`Self::footprint`] parses), keeping the PlaceProblem and the board
    /// coherent — see [`crate::synth`].
    pub fn footprint_source(&self, lib_id: &str) -> Option<String> {
        let path = self.footprint_path(lib_id)?;
        std::fs::read_to_string(path).ok()
    }

    /// "Did-you-mean" suggestions for a `Nickname:Name` id whose footprint
    /// could not be found — closest names *within the same library* by edit
    /// distance. Mirrors [`crate::provider`]'s symbol-side `suggest`.
    pub fn suggest(&self, lib_id: &str) -> Vec<String> {
        let Some((nick, name)) = lib_id.split_once(':') else {
            return Vec::new();
        };
        let needle = name.to_lowercase();
        let prefix = format!("{nick}:");
        let mut hits: Vec<(usize, &str)> = self
            .entries
            .iter()
            .filter(|e| e.lib_id.starts_with(&prefix))
            .map(|e| {
                let bare = &e.lib_id[prefix.len()..];
                (strsim::levenshtein(&needle, &bare.to_lowercase()), e.lib_id.as_str())
            })
            .filter(|(d, _)| *d <= SUGGEST_MAX_DISTANCE)
            .collect();
        hits.sort();
        hits.into_iter()
            .take(SUGGEST_LIMIT)
            .map(|(_, id)| id.to_string())
            .collect()
    }
}

/// Rank `entries` against an already-normalized `needle`, returning the indices
/// of the best `n`, best first. Byte-for-byte the same algorithm as
/// [`crate::search`]'s `rank`: fzf-style [`SkimMatcherV2`] subsequence scoring,
/// edit-distance backfill so the caller is never starved, deterministic ties
/// (fuzzy: higher score, then shorter normalized, then `lib_id`; backfill:
/// `lib_id`).
fn rank(entries: &[Entry], needle: &str, n: usize) -> Vec<usize> {
    let matcher = SkimMatcherV2::default();

    let mut fuzzy: Vec<(i64, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| matcher.fuzzy_match(&e.normalized, needle).map(|s| (s, i)))
        .collect();
    fuzzy.sort_by(|&(sa, ia), &(sb, ib)| {
        sb.cmp(&sa)
            .then_with(|| entries[ia].normalized.len().cmp(&entries[ib].normalized.len()))
            .then_with(|| entries[ia].lib_id.cmp(&entries[ib].lib_id))
    });

    let mut chosen: Vec<usize> = fuzzy.into_iter().take(n).map(|(_, i)| i).collect();
    if chosen.len() >= n {
        return chosen;
    }

    let taken: std::collections::HashSet<usize> = chosen.iter().copied().collect();
    let mut rest: Vec<(f64, usize)> = entries
        .iter()
        .enumerate()
        .filter(|(i, _)| !taken.contains(i))
        .map(|(i, e)| (1.0 - strsim::normalized_levenshtein(needle, &e.normalized), i))
        .collect();
    rest.sort_by(|&(da, ia), &(db, ib)| {
        da.partial_cmp(&db)
            .expect("distances are finite")
            .then_with(|| entries[ia].lib_id.cmp(&entries[ib].lib_id))
    });
    chosen.extend(rest.into_iter().take(n - chosen.len()).map(|(_, i)| i));
    chosen
}

/// Lowercase and collapse runs of non-alphanumeric characters into single
/// spaces — identical to [`crate::search`]'s `normalize`, so a footprint query
/// behaves exactly like a symbol query.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with(' ') && !out.is_empty() {
            out.push(' ');
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(lib_id: &str) -> Entry {
        Entry {
            normalized: normalize(lib_id),
            lib_id: lib_id.to_string(),
            path: PathBuf::new(),
        }
    }

    #[test]
    fn fuzzy_ranks_exact_fragment_first() {
        let entries = vec![
            entry("Resistor_SMD:R_0402_1005Metric"),
            entry("Resistor_SMD:R_0603_1608Metric"),
            entry("Package_TO_SOT_SMD:SOT-23"),
        ];
        let ranked = rank(&entries, &normalize("R_0603_1608Metric"), 3);
        assert_eq!(entries[ranked[0]].lib_id, "Resistor_SMD:R_0603_1608Metric");
    }

    #[test]
    fn typo_returns_closest_via_backfill() {
        let entries = vec![entry("Package_TO_SOT_SMD:SOT-23"), entry("Resistor_SMD:R_0603_1608Metric")];
        // "0663" is not a subsequence of "0603"; backfill must still answer.
        let ranked = rank(&entries, &normalize("R_0663"), 1);
        assert_eq!(ranked.len(), 1);
        assert_eq!(entries[ranked[0]].lib_id, "Resistor_SMD:R_0603_1608Metric");
    }

    #[test]
    fn normalize_matches_symbol_search_conventions() {
        assert_eq!(normalize("Resistor_SMD:R_0603_1608Metric"), "resistor smd r 0603 1608metric");
        assert_eq!(normalize("SOT-23"), "sot 23");
    }
}
