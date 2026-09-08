//! Index of KiCad stock symbol libraries (`.kicad_sym`).
//!
//! Provides [`Library::search`], [`Library::get`] (pins, bboxes) and
//! [`Library::raw_symbol`] — a flattened s-expression node ready to embed in a schematic's
//! `lib_symbols` section. The parsed index is cached on disk so a warm start is instant.

use crate::sexp::{self, Sexp};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pin {
    pub number: String,
    pub name: String,
    /// input/output/bidirectional/power_in/power_out/passive/...
    pub etype: String,
    /// Library coordinates (Y up), mm.
    pub x: f64,
    pub y: f64,
    /// 0 = pin extends to the right of (x,y) toward the body => pin is on the LEFT side of the body.
    pub angle: i32,
    pub length: f64,
    /// 0 = common to all units.
    pub unit: i32,
    pub hidden: bool,
}

impl Pin {
    pub fn side(&self) -> &'static str {
        match self.angle.rem_euclid(360) {
            0 => "left",
            180 => "right",
            90 => "bottom",
            _ => "top",
        }
    }

    /// Far end of the pin (where a wire attaches), in library coordinates.
    pub fn tip(&self) -> (f64, f64) {
        let r = (self.angle as f64).to_radians();
        (self.x + self.length * r.cos(), self.y + self.length * r.sin())
    }
}

pub type BBox = (f64, f64, f64, f64);

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SymbolInfo {
    pub lib_id: String,
    pub description: String,
    pub keywords: String,
    pub ref_prefix: String,
    pub value: String,
    pub footprint: String,
    pub datasheet: String,
    pub fp_filters: String,
    pub pins: Vec<Pin>,
    pub units: i32,
    /// Library coordinates: xmin, ymin (Y up), xmax, ymax — pins included.
    pub bbox: BBox,
    pub power: bool,
    pub file: String,
    pub offset: usize,
    pub end: usize,
    pub extends: String,
    /// Graphics-only bbox (library coordinates, Y up).
    pub body: BBox,
    /// unit -> bbox incl. pins (common unit 0 merged in).
    pub unit_bbox: HashMap<i32, BBox>,
    /// unit -> graphics-only bbox.
    pub unit_body: HashMap<i32, BBox>,
}

impl SymbolInfo {
    pub fn bbox_for(&self, unit: i32) -> BBox {
        *self.unit_bbox.get(&unit).unwrap_or(&self.bbox)
    }

    pub fn body_for(&self, unit: i32) -> BBox {
        *self.unit_body.get(&unit).unwrap_or(&self.body)
    }

    pub fn pins_for_unit(&self, unit: i32) -> Vec<&Pin> {
        self.pins.iter().filter(|p| p.unit == 0 || p.unit == unit).collect()
    }

    /// Pin by number, falling back to a lookup by name.
    pub fn pin(&self, number: &str, unit: Option<i32>) -> Option<&Pin> {
        let ok = |p: &&Pin| unit.is_none_or(|u| p.unit == 0 || p.unit == u);
        self.pins
            .iter()
            .find(|p| p.number == number && ok(p))
            .or_else(|| self.pins.iter().find(|p| p.name == number && ok(p)))
    }
}

fn symbol_starts(text: &str) -> Vec<(usize, String)> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r#"(?m)^\t\(symbol "((?:[^"\\]|\\.)*)""#).unwrap());
    re.captures_iter(text)
        .map(|c| (c.get(0).unwrap().start(), c.get(1).unwrap().as_str().to_string()))
        .collect()
}

/// (name, start, end) for each top-level symbol in a `.kicad_sym` file.
fn split_library(text: &str) -> Vec<(String, usize, usize)> {
    let starts = symbol_starts(text);
    let close = text.rfind(')').unwrap_or(text.len());
    starts
        .iter()
        .enumerate()
        .map(|(i, (s, name))| {
            let e = starts.get(i + 1).map(|(s2, _)| *s2).unwrap_or(close);
            (name.clone(), *s, e)
        })
        .collect()
}

fn minmax(v: &[f64]) -> Option<(f64, f64)> {
    if v.is_empty() {
        return None;
    }
    Some((v.iter().cloned().fold(f64::INFINITY, f64::min), v.iter().cloned().fold(f64::NEG_INFINITY, f64::max)))
}

#[derive(Default)]
struct UnitAcc {
    xs: Vec<f64>,
    ys: Vec<f64>,
    gxs: Vec<f64>,
    gys: Vec<f64>,
}

pub fn parse_symbol_node(node: &Sexp, lib_id: &str) -> SymbolInfo {
    static UNIT_RE: OnceLock<regex::Regex> = OnceLock::new();
    let unit_re = UNIT_RE.get_or_init(|| regex::Regex::new(r"_(\d+)_(\d+)$").unwrap());

    let mut pins: Vec<Pin> = Vec::new();
    let (mut xs, mut ys, mut gxs, mut gys) = (vec![], vec![], vec![], vec![]);
    let mut per_unit: HashMap<i32, UnitAcc> = HashMap::new();
    let mut units = 1;
    let extends = node.child("extends").and_then(|e| e.as_list()).map(|l| l[1].text()).unwrap_or_default();

    for sub in node.children("symbol") {
        let subname = sub.as_list().and_then(|l| l.get(1)).map(|v| v.text()).unwrap_or_default();
        let caps = unit_re.captures(&subname);
        let unit: i32 = caps.as_ref().map(|c| c[1].parse().unwrap_or(0)).unwrap_or(0);
        let style: i32 = caps.as_ref().map(|c| c[2].parse().unwrap_or(1)).unwrap_or(1);
        if style > 1 {
            continue; // skip De Morgan alternates
        }
        units = units.max(unit);
        let acc = per_unit.entry(unit).or_default();
        for item in sub.as_list().unwrap().iter().skip(1).filter(|i| i.is_list()) {
            match item.tag() {
                "pin" => {
                    let l = item.as_list().unwrap();
                    let at = item.child("at").and_then(|a| a.as_list()).map(|a| a.to_vec()).unwrap_or_default();
                    let hidden = item
                        .child("hide")
                        .and_then(|h| h.as_list())
                        .map(|h| h.get(1).map(|v| v.text()) == Some("yes".into()))
                        .unwrap_or(false)
                        || l[1..4.min(l.len())].iter().any(|a| a.as_str() == Some("hide"));
                    let p = Pin {
                        number: item.child("number").and_then(|n| n.as_list()).map(|n| n[1].text()).unwrap_or_default(),
                        name: item.child("name").and_then(|n| n.as_list()).map(|n| n[1].text()).unwrap_or_default(),
                        etype: l.get(1).map(|v| v.text()).unwrap_or_default(),
                        x: at.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
                        y: at.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0),
                        angle: at.get(3).and_then(|v| v.as_f64()).unwrap_or(0.0) as i32,
                        length: item.child("length").and_then(|n| n.as_list()).and_then(|n| n[1].as_f64()).unwrap_or(2.54),
                        unit,
                        hidden,
                    };
                    let (ex, ey) = p.tip();
                    xs.extend([p.x, ex]);
                    acc.xs.extend([p.x, ex]);
                    ys.extend([p.y, ey]);
                    acc.ys.extend([p.y, ey]);
                    pins.push(p);
                }
                "rectangle" => {
                    for k in ["start", "end"] {
                        if let Some(c) = item.child(k).and_then(|c| c.as_list()) {
                            gxs.push(c[1].as_f64().unwrap_or(0.0));
                            acc.gxs.push(c[1].as_f64().unwrap_or(0.0));
                            gys.push(c[2].as_f64().unwrap_or(0.0));
                            acc.gys.push(c[2].as_f64().unwrap_or(0.0));
                        }
                    }
                }
                "polyline" => {
                    if let Some(pts) = item.child("pts") {
                        for xy in pts.children("xy") {
                            let l = xy.as_list().unwrap();
                            gxs.push(l[1].as_f64().unwrap_or(0.0));
                            acc.gxs.push(l[1].as_f64().unwrap_or(0.0));
                            gys.push(l[2].as_f64().unwrap_or(0.0));
                            acc.gys.push(l[2].as_f64().unwrap_or(0.0));
                        }
                    }
                }
                "circle" => {
                    let c = item.child("center").and_then(|c| c.as_list()).map(|c| c.to_vec()).unwrap_or_default();
                    let r = item.child("radius").and_then(|c| c.as_list()).and_then(|c| c[1].as_f64()).unwrap_or(0.0);
                    let (cx, cy) =
                        (c.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0), c.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0));
                    gxs.extend([cx - r, cx + r]);
                    acc.gxs.extend([cx - r, cx + r]);
                    gys.extend([cy - r, cy + r]);
                    acc.gys.extend([cy - r, cy + r]);
                }
                "arc" => {
                    for k in ["start", "mid", "end"] {
                        if let Some(c) = item.child(k).and_then(|c| c.as_list()) {
                            gxs.push(c[1].as_f64().unwrap_or(0.0));
                            acc.gxs.push(c[1].as_f64().unwrap_or(0.0));
                            gys.push(c[2].as_f64().unwrap_or(0.0));
                            acc.gys.push(c[2].as_f64().unwrap_or(0.0));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    xs.extend(gxs.iter().cloned());
    ys.extend(gys.iter().cloned());

    let mut unit_bbox = HashMap::new();
    let mut unit_body = HashMap::new();
    if units > 1 {
        let empty = UnitAcc::default();
        let common = per_unit.get(&0).unwrap_or(&empty);
        let (cxs, cys, cgxs, cgys) =
            (common.xs.clone(), common.ys.clone(), common.gxs.clone(), common.gys.clone());
        for (u, acc) in per_unit.iter() {
            if *u == 0 {
                continue;
            }
            let axs: Vec<f64> = [acc.xs.clone(), acc.gxs.clone(), cxs.clone(), cgxs.clone()].concat();
            let ays: Vec<f64> = [acc.ys.clone(), acc.gys.clone(), cys.clone(), cgys.clone()].concat();
            let bgx: Vec<f64> = [acc.gxs.clone(), cgxs.clone()].concat();
            let bgy: Vec<f64> = [acc.gys.clone(), cgys.clone()].concat();
            if let (Some((x0, x1)), Some((y0, y1))) = (minmax(&axs), minmax(&ays)) {
                unit_bbox.insert(*u, (x0, y0, x1, y1));
            }
            if let (Some((x0, x1)), Some((y0, y1))) = (minmax(&bgx), minmax(&bgy)) {
                unit_body.insert(*u, (x0, y0, x1, y1));
            } else if let (Some((x0, x1)), Some((y0, y1))) = (minmax(&acc.xs), minmax(&acc.ys)) {
                unit_body.insert(*u, (x0, y0, x1, y1));
            }
        }
    }

    let bbox = match (minmax(&xs), minmax(&ys)) {
        (Some((x0, x1)), Some((y0, y1))) => (x0, y0, x1, y1),
        _ => (0.0, 0.0, 0.0, 0.0),
    };
    let body = if !gxs.is_empty() {
        let (x0, x1) = minmax(&gxs).unwrap();
        let (y0, y1) = minmax(&gys).unwrap();
        (x0, y0, x1, y1)
    } else if !pins.is_empty() {
        let tips: Vec<(f64, f64)> = pins.iter().map(|p| p.tip()).collect();
        let tx: Vec<f64> = tips.iter().map(|t| t.0).collect();
        let ty: Vec<f64> = tips.iter().map(|t| t.1).collect();
        let (x0, x1) = minmax(&tx).unwrap();
        let (y0, y1) = minmax(&ty).unwrap();
        (x0, y0, x1, y1)
    } else {
        (0.0, 0.0, 0.0, 0.0)
    };

    SymbolInfo {
        lib_id: lib_id.to_string(),
        description: node.prop_value("Description").unwrap_or_default(),
        keywords: node.prop_value("ki_keywords").unwrap_or_default(),
        ref_prefix: {
            let r = node.prop_value("Reference").unwrap_or_default();
            if r.is_empty() { "U".into() } else { r }
        },
        value: node.prop_value("Value").unwrap_or_default(),
        footprint: node.prop_value("Footprint").unwrap_or_default(),
        datasheet: node.prop_value("Datasheet").unwrap_or_default(),
        fp_filters: node.prop_value("ki_fp_filters").unwrap_or_default(),
        pins,
        units,
        bbox,
        power: node.child("power").is_some(),
        file: String::new(),
        offset: 0,
        end: 0,
        extends,
        body,
        unit_bbox,
        unit_body,
    }
}

/// One `search_symbols` result row.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SymbolHit {
    pub lib_id: String,
    pub description: String,
    pub footprint: String,
    pub pins: usize,
}

/// The parsed library index. Cloning is cheap (symbols are shared).
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Library {
    pub symbols: std::collections::BTreeMap<String, Arc<SymbolInfo>>,
    #[serde(skip)]
    text_cache: std::sync::Arc<std::sync::Mutex<HashMap<String, Arc<String>>>>,
}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Library({} symbols)", self.symbols.len())
    }
}

/// Bumped whenever [`SymbolInfo`] changes shape, so stale caches are rejected instead of misread.
const CACHE_MAGIC: &[u8] = b"gordian-symindex-1\n";

fn cache_path(symbol_dir: &Path) -> PathBuf {
    let mut key = symbol_dir.to_string_lossy().to_string();
    let mut newest = 0u64;
    if let Ok(rd) = std::fs::read_dir(symbol_dir) {
        let mut n = 0;
        for e in rd.flatten() {
            n += 1;
            if let Ok(m) = e.metadata()
                && let Ok(t) = m.modified()
                && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
            {
                newest = newest.max(d.as_secs());
            }
        }
        key.push_str(&format!("#{n}#{newest}"));
    }
    let mut h: u64 = 1469598103934665603;
    for b in key.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    let base = directories::BaseDirs::new()
        .map(|b| b.cache_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("gordian").join(format!("symindex-{h:016x}.bin"))
}

impl Library {
    /// Load the index for `symbol_dir`, using the on-disk cache when it is current.
    pub fn load(symbol_dir: &Path) -> Result<Library> {
        let cache = cache_path(symbol_dir);
        if let Ok(bytes) = std::fs::read(&cache)
            && bytes.starts_with(CACHE_MAGIC)
            && let Ok((symbols, _)) = bincode::serde::decode_from_slice::<
                std::collections::BTreeMap<String, Arc<SymbolInfo>>,
                _,
            >(&bytes[CACHE_MAGIC.len()..], bincode::config::standard())
            && !symbols.is_empty()
        {
            let lib = Library { symbols, text_cache: Default::default() };
            set_index(lib.clone());
            return Ok(lib);
        }
        let idx = Library::build(symbol_dir)?;
        if let Some(dir) = cache.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(bytes) = bincode::serde::encode_to_vec(&idx.symbols, bincode::config::standard()) {
            let _ = std::fs::write(&cache, [CACHE_MAGIC, &bytes].concat());
        }
        set_index(idx.clone());
        Ok(idx)
    }

    /// Parse every library under `symbol_dir` (single `.kicad_sym` files and KiCad 10
    /// `.kicad_symdir` directories).
    pub fn build(symbol_dir: &Path) -> Result<Library> {
        let mut units: Vec<(String, Vec<PathBuf>)> = Vec::new();
        let mut entries: Vec<PathBuf> = std::fs::read_dir(symbol_dir)
            .with_context(|| format!("symbol dir {}", symbol_dir.display()))?
            .flatten()
            .map(|e| e.path())
            .collect();
        entries.sort();
        for p in entries {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if name.ends_with(".kicad_sym") && p.is_file() {
                units.push((name.trim_end_matches(".kicad_sym").to_string(), vec![p]));
            } else if name.ends_with(".kicad_symdir") && p.is_dir() {
                let mut files: Vec<PathBuf> = std::fs::read_dir(&p)
                    .map(|rd| {
                        rd.flatten()
                            .map(|e| e.path())
                            .filter(|f| f.extension().is_some_and(|e| e == "kicad_sym"))
                            .collect()
                    })
                    .unwrap_or_default();
                files.sort();
                units.push((name.trim_end_matches(".kicad_symdir").to_string(), files));
            }
        }

        use rayon::prelude::*;
        let parsed: Vec<Vec<(String, SymbolInfo)>> = units
            .par_iter()
            .map(|(libname, files)| {
                let mut out = Vec::new();
                for fp in files {
                    let Ok(text) = std::fs::read_to_string(fp) else { continue };
                    for (name, s, e) in split_library(&text) {
                        let Some(node) = sexp::loads(&text[s..e]) else { continue };
                        let lib_id = format!("{libname}:{name}");
                        let mut info = parse_symbol_node(&node, &lib_id);
                        info.file = fp.to_string_lossy().to_string();
                        info.offset = s;
                        info.end = e;
                        out.push((lib_id, info));
                    }
                }
                out
            })
            .collect();

        let mut symbols: std::collections::BTreeMap<String, SymbolInfo> = Default::default();
        for group in parsed {
            for (lib_id, info) in group {
                symbols.insert(lib_id, info);
            }
        }
        // resolve `extends`: inherit geometry from the parent
        let inherited: Vec<(String, SymbolInfo)> = symbols
            .iter()
            .filter(|(_, i)| !i.extends.is_empty())
            .filter_map(|(id, i)| {
                let libname = id.split(':').next().unwrap_or("");
                let parent = symbols.get(&format!("{}:{}", libname, i.extends))?;
                let mut c = i.clone();
                c.pins = parent.pins.clone();
                c.bbox = parent.bbox;
                c.units = parent.units;
                c.power = parent.power;
                c.body = parent.body;
                c.unit_bbox = parent.unit_bbox.clone();
                c.unit_body = parent.unit_body.clone();
                Some((id.clone(), c))
            })
            .collect();
        for (id, info) in inherited {
            symbols.insert(id, info);
        }
        Ok(Library {
            symbols: symbols.into_iter().map(|(k, v)| (k, Arc::new(v))).collect(),
            text_cache: Default::default(),
        })
    }

    pub fn get(&self, lib_id: &str) -> Option<Arc<SymbolInfo>> {
        self.symbols.get(lib_id).cloned()
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    fn lib_text(&self, fname: &str) -> Arc<String> {
        let mut cache = self.text_cache.lock().unwrap();
        cache
            .entry(fname.to_string())
            .or_insert_with(|| Arc::new(std::fs::read_to_string(fname).unwrap_or_default()))
            .clone()
    }

    /// Flattened symbol node (with `extends` resolved) named by full lib_id, for `lib_symbols`.
    pub fn raw_symbol(&self, lib_id: &str) -> Option<Sexp> {
        let info = self.symbols.get(lib_id)?;
        let text = self.lib_text(&info.file);
        let mut node = sexp::loads(text.get(info.offset..info.end)?)?;
        let libname = lib_id.split(':').next().unwrap_or("");
        if !info.extends.is_empty() {
            let pnode = self.raw_symbol(&format!("{}:{}", libname, info.extends))?;
            let pitems = pnode.as_list()?.to_vec();
            let mut props: Vec<(String, Sexp)> = Vec::new();
            let push_prop = |props: &mut Vec<(String, Sexp)>, c: &Sexp| {
                let name = c.as_list().and_then(|l| l.get(1)).map(|v| v.text()).unwrap_or_default();
                match props.iter_mut().find(|(n, _)| *n == name) {
                    Some(slot) => slot.1 = c.clone(),
                    None => props.push((name, c.clone())),
                }
            };
            for c in pitems.iter().skip(1) {
                if c.tag() == "property" {
                    push_prop(&mut props, c);
                }
            }
            for c in node.as_list()?.iter().skip(1) {
                if c.tag() == "property" {
                    push_prop(&mut props, c);
                }
            }
            let merged: Vec<Sexp> = pitems.into_iter().filter(|c| c.tag() != "property").collect();
            let (mut head, mut tail): (Vec<Sexp>, Vec<Sexp>) = (Vec::new(), Vec::new());
            for c in merged {
                if matches!(c.tag(), "symbol" | "embedded_fonts") {
                    tail.push(c);
                } else {
                    head.push(c);
                }
            }
            let mut items: Vec<Sexp> = head;
            items.extend(props.into_iter().map(|(_, c)| c));
            items.extend(tail);
            items.retain(|c| c.tag() != "extends");
            let cname = lib_id.split_once(':').map(|(_, n)| n).unwrap_or(lib_id);
            for c in items.iter_mut() {
                if c.tag() == "symbol"
                    && let Some(l) = c.as_list_mut()
                    && let Some(n) = l.get_mut(1)
                {
                    *n = Sexp::Str(n.text().replacen(&info.extends, cname, 1));
                }
            }
            node = Sexp::List(items);
        }
        if let Some(l) = node.as_list_mut() {
            if l.len() > 1 {
                l[1] = Sexp::Str(lib_id.to_string());
            }
        }
        Some(node)
    }

    /// Ranked search rows for the `search_symbols` tool.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SymbolHit> {
        self.search_infos(query, limit)
            .into_iter()
            .map(|i| SymbolHit {
                lib_id: i.lib_id.clone(),
                description: i.description.clone(),
                footprint: i.footprint.clone(),
                pins: i.pins.len(),
            })
            .collect()
    }

    /// The pin-table text `symbol_info` returns to the model.
    pub fn info_text(&self, lib_id: &str, unit: Option<u32>) -> Result<String> {
        let info = self.get(lib_id).ok_or_else(|| {
            anyhow::anyhow!("unknown symbol '{lib_id}' - use search_symbols to find the right lib id")
        })?;
        Ok(describe(&info, unit.map(|u| u as i32)))
    }

    /// Ranked text search across lib id, keywords and description.
    pub fn search_infos(&self, query: &str, limit: usize) -> Vec<Arc<SymbolInfo>> {
        let q = query.trim();
        if let Some(hit) = self.symbols.get(q) {
            return vec![hit.clone()];
        }
        let ql = q.to_lowercase();
        let raw: Vec<String> = split_toks(&ql);
        let mut toks: std::collections::BTreeSet<String> = raw.iter().cloned().collect();
        if raw.len() > 1 {
            toks.insert(raw.concat());
        }
        toks.retain(|t| !t.is_empty());
        let lib_filter = if ql.contains(':') { ql.split(':').next().unwrap().to_string() } else { String::new() };

        let mut scored: Vec<(f64, String)> = Vec::new();
        for (lib_id, info) in &self.symbols {
            let (libname, name) = lib_id.split_once(':').unwrap_or((lib_id.as_str(), ""));
            if !lib_filter.is_empty() && libname.to_lowercase() != lib_filter {
                continue;
            }
            let nl = name.to_lowercase();
            let squashed = nl.replace('_', "").replace('-', "");
            let name_toks: std::collections::BTreeSet<String> = split_toks_keep_empty(&nl);
            let kwl = info.keywords.to_lowercase();
            let kw_toks: std::collections::BTreeSet<String> = split_toks_keep_empty(&kwl);
            let desc = info.description.to_lowercase();
            let desc_toks: std::collections::BTreeSet<String> = split_toks_keep_empty(&desc);
            let (mut score, mut matched) = (0.0f64, 0usize);
            for t in &toks {
                let mut best = 0.0f64;
                if *t == nl || *t == squashed {
                    best = 12.0;
                } else if name_toks.contains(t) {
                    best = 6.0;
                } else if t.len() >= 3 && squashed.contains(t.as_str()) {
                    best = if nl.starts_with(t.as_str()) { 3.5 } else { 2.5 };
                }
                if kw_toks.contains(t) {
                    best = best.max(3.0);
                } else if t.len() >= 3 && kwl.contains(t.as_str()) {
                    best = best.max(1.5);
                }
                if desc_toks.contains(t) {
                    best = best.max(2.0);
                } else if t.len() >= 4 && desc.contains(t.as_str()) {
                    best = best.max(1.0);
                }
                if libname.to_lowercase() == *t {
                    best = best.max(2.0);
                }
                if best != 0.0 {
                    matched += 1;
                }
                score += best;
            }
            if matched == 0 {
                continue;
            }
            let frac = matched as f64 / raw.len().max(1) as f64;
            if frac < 0.5 && raw.len() > 1 {
                continue;
            }
            score += 6.0 * frac;
            if !info.extends.is_empty() {
                score -= 0.1;
            }
            score -= 0.01 * nl.len() as f64;
            scored.push((score, lib_id.clone()));
        }
        sort_desc(&mut scored);
        if scored.is_empty() {
            // fallback: longest-common-prefix on part numbers (STM32F103C8T6 -> STM32F103C8Tx)
            let mut by_len: Vec<&String> = toks.iter().collect();
            by_len.sort_by_key(|t| std::cmp::Reverse(t.len()));
            for t in by_len {
                if t.len() < 5 {
                    continue;
                }
                for lib_id in self.symbols.keys() {
                    let nl = lib_id.split_once(':').map(|(_, n)| n).unwrap_or("").to_lowercase();
                    let n = t.chars().zip(nl.chars()).take_while(|(a, b)| a == b).count();
                    if n >= 5.max((t.len() as f64 * 0.6) as usize) {
                        scored.push((n as f64 - 0.01 * nl.len() as f64, lib_id.clone()));
                    }
                }
                if !scored.is_empty() {
                    break;
                }
            }
            sort_desc(&mut scored);
        }
        scored.iter().take(limit).filter_map(|(_, l)| self.symbols.get(l).cloned()).collect()
    }
}

/// Python's `sorted(scored, reverse=True)` on (score, lib_id) tuples.
fn sort_desc(scored: &mut [(f64, String)]) {
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| b.1.cmp(&a.1))
    });
}

fn is_tok_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '+'
}

/// Python `re.split(r"[^a-z0-9.+]+", s)` with empty pieces dropped.
fn split_toks(s: &str) -> Vec<String> {
    s.split(|c: char| !is_tok_char(c)).filter(|t| !t.is_empty()).map(|t| t.to_string()).collect()
}

/// Same split but keeping the empty pieces Python's `set(re.split(...))` retains.
fn split_toks_keep_empty(s: &str) -> std::collections::BTreeSet<String> {
    s.split(|c: char| !is_tok_char(c)).map(|t| t.to_string()).collect()
}

static INDEX: OnceLock<Library> = OnceLock::new();

/// Install the process-wide library index used by [`index`].
pub fn set_index(idx: Library) {
    let _ = INDEX.set(idx);
}

/// The process-wide library index (empty until [`Library::load`] or [`set_index`] runs).
/// Like the Python module global it is installed once per process.
pub fn index() -> &'static Library {
    INDEX.get_or_init(Library::default)
}

/// Python `%g` formatting, used by [`describe`].
fn g(v: f64) -> String {
    if v == 0.0 {
        return if v.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    if !v.is_finite() {
        return format!("{v}");
    }
    let exp = v.abs().log10().floor() as i32;
    if !(-4..6).contains(&exp) {
        let s = format!("{v:.5e}");
        let (m, e) = s.split_once('e').unwrap();
        let m = if m.contains('.') { m.trim_end_matches('0').trim_end_matches('.') } else { m };
        let ev: i32 = e.parse().unwrap_or(0);
        return format!("{}e{}{:02}", m, if ev < 0 { "-" } else { "+" }, ev.abs());
    }
    let prec = (5 - exp).max(0) as usize;
    let mut s = format!("{v:.prec$}");
    if s.contains('.') {
        s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    s
}

fn pad_left(s: &str, w: usize) -> String {
    format!("{}{}", " ".repeat(w.saturating_sub(s.chars().count())), s)
}

fn pad_right(s: &str, w: usize) -> String {
    format!("{}{}", s, " ".repeat(w.saturating_sub(s.chars().count())))
}

/// Compact text description of a symbol for the LLM (the `symbol_info` tool output).
pub fn describe(info: &SymbolInfo, unit: Option<i32>) -> String {
    const G: f64 = 1.27;
    let desc: String = info.description.chars().take(120).collect();
    let mut lines = vec![format!(
        "{}  ref={}  units={}  desc={}",
        info.lib_id, info.ref_prefix, info.units, desc
    )];
    let (x0, y0, x1, y1) = info.bbox;
    lines.push(format!(
        "  extent incl. pins at rot 0 (grid units, relative to anchor; +x right, +y DOWN on sheet): \
         x {}..{}, y {}..{}   footprint={}",
        g(x0 / G),
        g(x1 / G),
        g(-y1 / G),
        g(-y0 / G),
        if info.footprint.is_empty() { "-" } else { &info.footprint }
    ));
    let us: Vec<i32> = match unit {
        Some(u) if u != 0 => vec![u],
        _ => (1..=info.units).collect(),
    };
    for u in us {
        let mut pins: Vec<&Pin> = info.pins.iter().filter(|p| p.unit == 0 || p.unit == u).collect();
        if info.units > 1 {
            lines.push(format!("  unit {u}:"));
        }
        pins.sort_by(|a, b| {
            let key = |p: &Pin| {
                let s = p.side();
                let second = if s == "left" || s == "right" { -p.y } else { p.x };
                (s.to_string(), second)
            };
            let (ka, kb) = (key(a), key(b));
            ka.0.cmp(&kb.0).then(ka.1.partial_cmp(&kb.1).unwrap_or(std::cmp::Ordering::Equal))
        });
        for p in pins {
            lines.push(format!(
                "    pin {} {} {} {} offset ({},{}){}",
                pad_left(&p.number, 4),
                pad_right(&p.name, 16),
                pad_right(&p.etype, 14),
                pad_right(p.side(), 6),
                g(p.x / G),
                g(-p.y / G),
                if p.hidden { " (hidden)" } else { "" }
            ));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_resistor() {
        let text = "(kicad_symbol_lib\n\t(symbol \"R\"\n\t\t(property \"Reference\" \"R\")\n\
                    \t\t(symbol \"R_0_1\" (rectangle (start -1.016 -2.54) (end 1.016 2.54)))\n\
                    \t\t(symbol \"R_1_1\" (pin passive line (at 0 3.81 270) (length 1.27) \
                    (name \"~\") (number \"1\")))\n\t)\n)";
        let (_, s, e) = split_library(text)[0].clone();
        let node = sexp::loads(&text[s..e]).unwrap();
        let info = parse_symbol_node(&node, "Device:R");
        assert_eq!(info.pins.len(), 1);
        assert_eq!(info.pins[0].number, "1");
        assert_eq!(info.pins[0].angle, 270);
        assert_eq!(info.pins[0].side(), "top");
        assert_eq!(info.body, (-1.016, -2.54, 1.016, 2.54));
        assert_eq!(info.ref_prefix, "R");
    }

    #[test]
    fn python_g_formatting() {
        assert_eq!(g(2.0), "2");
        assert_eq!(g(-1.5), "-1.5");
        assert_eq!(g(0.0), "0");
        assert_eq!(g(1.0 / 3.0), "0.333333");
    }
}
