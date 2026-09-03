//! Pin geometry and `(lib_symbols)` definition extraction for a single symbol.
//!
//! The schematic writer needs two things per used symbol that the
//! geometry-free `crate::PinMeta` deliberately omits:
//!
//! 1. **Pin geometry** — each pin's local position (`at`), `angle`, and
//!    `length`. The connection endpoint is the pin's local `at` itself (the
//!    tip where wires attach); `length` runs from `at` *into* the symbol body
//!    along `angle`, so the engine uses `at` directly (NOT `at + length`) when
//!    transforming to sheet coordinates.
//! 2. **The full symbol definition** — the balanced `(symbol "Lib:Name" …)`
//!    S-expression block to splice into the schematic's `(lib_symbols)` set.
//!
//! ## Name form inside `(lib_symbols)`
//!
//! A `.kicad_sym` file names its top-level symbols *bare* (`(symbol "R" …)`).
//! Inside a schematic's `(lib_symbols)`, KiCAD renames that top-level wrapper
//! to the fully-qualified `Lib:Name` form (`(symbol "Device:R" …)`) while
//! leaving the **nested** sub-unit blocks (`R_0_1`, `R_1_1`) untouched. We
//! reproduce exactly that: take the `.kicad_sym` node verbatim and rewrite only
//! the leading name atom to `"<lib>:<name>"`.
//!
//! ## `extends` (derived symbols)
//!
//! A derived symbol (e.g. `Device:Filter_EMI_C` `(extends "C_Feedthrough")`)
//! has **no body of its own**. Embedding the derived block verbatim — keeping
//! the `(extends …)` clause — makes KiCAD *load* the schematic but resolve
//! **zero pins** (it does not consult the local `.kicad_sym` at netlist time).
//! KiCAD therefore needs the parent's body **inlined** under the derived name.
//! Empirically verified with `kicad-cli`, the working form is the parent's
//! `(symbol …)` block with two rewrites: the top-level name set to
//! `"<lib>:<derived>"`, and every nested sub-block prefix rewritten from
//! `Parent_` to `Derived_` (so `C_Feedthrough_1_1` becomes `Filter_EMI_C_1_1`).
//! That yields the correct pin count in the exported netlist; the verbatim
//! `(extends)` form yields zero nodes.

use std::io;
use std::path::{Path, PathBuf};

use geom::{Point2, Rect};
use kiutils_kicad::{SymPin, Symbol, SymbolLibFile};
use kiutils_sexpr::{Atom, Node, parse_one};

/// Local geometry of a single symbol pin, in symbol coordinates.
///
/// `at` is the pin's connection-point root in millimetres (symbol Y grows
/// upward); the pin line extends `length` mm from there along `angle`
/// (degrees). The schematic-space endpoint is computed by the writer after
/// applying the instance's position/rotation/mirror.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PinGeom {
    /// Pin number (e.g. `"1"`, `"2"`, `"A3"`).
    pub number: String,
    /// Pin name (e.g. `"~"`, `"VCC"`, `"GND"`).
    pub name: String,
    /// Local position of the pin's connection root, in mm (symbol Y grows upward).
    pub at: Point2,
    /// Pin orientation in degrees (0/90/180/270).
    pub angle: f64,
    /// Pin line length in mm.
    pub length: f64,
    /// 1-based unit this pin belongs to. Multi-unit symbols (op-amps, logic
    /// gates) split their pins across units; this carries the identity so a
    /// consumer placing a specific unit can filter. Unit 0 / common pins are
    /// folded to 1 (matching `symlib.rs::PinMeta`); direct (non-sub-block)
    /// pins are unit 1.
    pub unit: u8,
    /// How KiCAD draws this pin's name and number.
    #[serde(default)]
    pub text: PinTextStyle,
}

/// The symbol-level settings that decide whether — and where — KiCAD draws a
/// pin's name and number.
///
/// `name_offset` is the symbol's `(pin_names (offset …))`: a positive offset
/// puts the name *inside* the body, that far past the pin's body end; zero puts
/// it outside, centred on the pin line like the number but on the opposite side.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PinTextStyle {
    pub name_offset: f64,
    pub names_hidden: bool,
    pub numbers_hidden: bool,
    /// This pin's own `hide` flag: a hidden pin draws neither name nor number.
    pub pin_hidden: bool,
    pub name_size: f64,
    pub number_size: f64,
}

impl Default for PinTextStyle {
    fn default() -> Self {
        Self {
            name_offset: 0.508,
            names_hidden: false,
            numbers_hidden: false,
            pin_hidden: false,
            name_size: 1.27,
            number_size: 1.27,
        }
    }
}

/// Geometry plus the embeddable `(lib_symbols)` definition for one symbol.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SymbolGeometry {
    /// Fully-qualified `Lib:Name` identifier.
    pub lib_id: String,
    /// All pins (own pins, or the parent's pins when the symbol `extends`),
    /// flattened across **every** unit. For a multi-unit symbol this Vec holds
    /// the pins of all units interleaved, so `pins.len()` is the total pin count
    /// across the whole symbol, **not** the per-unit count. A consumer placing a
    /// specific unit must filter by [`PinGeom::unit`].
    pub pins: Vec<PinGeom>,
    /// The balanced `(symbol "Lib:Name" …)` block ready to splice into a
    /// schematic's `(lib_symbols)`. For derived symbols this is the parent's
    /// body re-targeted to the derived name.
    pub raw_definition: String,
}

thread_local! {
    /// Per-thread memo of resolved symbol geometry, keyed by (symbol dir, lib_id).
    /// The libraries are read-only at runtime, so caching is safe and turns the
    /// refinement loop's thousands of re-routes (each re-reading the same handful
    /// of `.kicad_sym` files) from disk-bound into in-memory.
    static GEOM_CACHE: std::cell::RefCell<
        std::collections::HashMap<(String, String), SymbolGeometry>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

impl SymbolGeometry {
    /// Load pin geometry and the embeddable definition for `lib_id`
    /// (`"Lib:Name"`) from the detected KiCAD symbol libraries. Memoized per
    /// thread by (symbol dir, lib_id).
    pub fn load(symbol_dir: &Path, lib_id: &str) -> io::Result<SymbolGeometry> {
        let key = (
            symbol_dir.to_string_lossy().into_owned(),
            lib_id.to_string(),
        );
        if let Some(g) = GEOM_CACHE.with(|c| c.borrow().get(&key).cloned()) {
            return Ok(g);
        }
        let g = Self::load_uncached(symbol_dir, lib_id)?;
        GEOM_CACHE.with(|c| c.borrow_mut().insert(key, g.clone()));
        Ok(g)
    }

    /// The actual library read + resolve (uncached). See [`Self::load`].
    fn load_uncached(symbol_dir: &Path, lib_id: &str) -> io::Result<SymbolGeometry> {
        let (lib, name) = lib_id.split_once(':').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("lib_id must be 'Lib:Name', got {lib_id:?}"),
            )
        })?;

        let library = load_symbol_library(symbol_dir, lib)?;
        let symbols = &library.symbols;

        let sym = find_symbol(symbols, name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("symbol {name:?} not in {}", library.source),
            )
        })?;

        // Resolve the symbol that actually carries the geometry/body: follow
        // `extends` while the current symbol has no pins of its own.
        let body = resolve_body(symbols, sym);

        let pins = collect_pins(body, sym);

        // Raw definition: the parent body's block, retargeted to `lib_id` and
        // (when derived) with nested sub-block prefixes rewritten.
        let raw_definition = build_definition(&library.texts, symbols, lib_id, name, sym, body)?;

        Ok(SymbolGeometry {
            lib_id: lib_id.to_string(),
            pins,
            raw_definition,
        })
    }

    /// Read geometry straight out of a `(symbol "Lib:Name" …)` block that a
    /// schematic embeds, rather than from an installed library.
    ///
    /// A saved schematic carries the definition of every symbol it places, which is
    /// the only copy when the part came from a project-local library. Editing such a
    /// sheet has to work off what the file itself says.
    pub fn from_definition(lib_id: &str, definition: &str) -> io::Result<SymbolGeometry> {
        let wrapped = format!(
            "(kicad_symbol_lib (version 20241209) (generator \"embedded\")\n{definition}\n)"
        );
        let file = tempfile::Builder::new().suffix(".kicad_sym").tempfile()?;
        std::fs::write(file.path(), &wrapped)?;
        let doc = SymbolLibFile::read(file.path()).map_err(map_kiutils_err)?;
        let symbols = doc.ast().symbols.clone();
        let name = lib_id.rsplit(':').next().unwrap_or(lib_id);
        let sym = find_symbol(&symbols, lib_id)
            .or_else(|| find_symbol(&symbols, name))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("embedded definition does not declare {lib_id:?}"),
                )
            })?;
        Ok(SymbolGeometry {
            lib_id: lib_id.to_string(),
            pins: collect_pins(resolve_body(&symbols, sym), sym),
            raw_definition: definition.to_string(),
        })
    }

    /// The balanced `(symbol "Lib:Name" …)` block for `(lib_symbols)`.
    pub fn definition_sexpr(&self) -> &str {
        &self.raw_definition
    }

    /// Approximate body extents `(width, height)` in mm, derived from pin
    /// connection points (pins bound the drawn body closely for almost every
    /// KiCAD symbol). Floors at 5.08 mm and pads 2.54 mm per side so even a
    /// bare two-pin passive gets a sane footprint.
    pub fn approx_size(&self) -> Point2 {
        let mut points: Vec<Point2> = self.pins.iter().map(|p| p.at).collect();
        points.push(Point2::new(0.0, 0.0));
        let bounds = Rect::bounding(&points).unwrap_or_else(|| Rect::new(0.0, 0.0, 0.0, 0.0));
        Point2::new(
            bounds.width().max(5.08) + 5.08,
            bounds.height().max(5.08) + 5.08,
        )
    }
}

struct LoadedSymbolLibrary {
    symbols: Vec<Symbol>,
    texts: Vec<String>,
    source: String,
}

fn load_symbol_library(symbol_dir: &Path, lib: &str) -> io::Result<LoadedSymbolLibrary> {
    let flat = symbol_dir.join(format!("{lib}.kicad_sym"));
    if flat.is_file() {
        return load_symbol_files(vec![flat], lib);
    }

    let split = symbol_dir.join(format!("{lib}.kicad_symdir"));
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&split)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "kicad_sym"))
        .collect();
    paths.sort();
    load_symbol_files(paths, lib)
}

fn load_symbol_files(paths: Vec<PathBuf>, lib: &str) -> io::Result<LoadedSymbolLibrary> {
    let mut symbols = Vec::new();
    let mut texts = Vec::new();
    let source = if paths.len() == 1 {
        paths[0].display().to_string()
    } else {
        format!("{lib}.kicad_symdir")
    };

    for path in paths {
        let text = std::fs::read_to_string(&path)?;
        let doc = SymbolLibFile::read(&path).map_err(map_kiutils_err)?;
        symbols.extend(doc.ast().symbols.iter().cloned());
        texts.push(text);
    }

    Ok(LoadedSymbolLibrary {
        symbols,
        texts,
        source,
    })
}

/// Find a top-level symbol by its bare name.
fn find_symbol<'a>(symbols: &'a [Symbol], name: &str) -> Option<&'a Symbol> {
    symbols.iter().find(|s| s.name.as_deref() == Some(name))
}

/// Follow `extends` until reaching the symbol that owns the body (has pins),
/// or the end of the chain. Cycle-safe via a visited set; on a cycle or a
/// missing parent the last reachable symbol is returned.
fn resolve_body<'a>(symbols: &'a [Symbol], start: &'a Symbol) -> &'a Symbol {
    let mut cur = start;
    let mut seen: Vec<&str> = Vec::new();
    loop {
        if !pins_of(cur).is_empty() {
            return cur;
        }
        let Some(name) = cur.name.as_deref() else {
            return cur;
        };
        if seen.contains(&name) {
            return cur; // extends cycle: stop deliberately
        }
        seen.push(name);
        match cur.extends.as_deref().and_then(|p| find_symbol(symbols, p)) {
            Some(parent) => cur = parent,
            None => return cur, // no body and no resolvable parent
        }
    }
}

/// All pins owned by a symbol: direct pins plus the pins of every
/// `<NAME>_<unit>_<bodystyle>` sub-block.
fn pins_of(sym: &Symbol) -> Vec<&SymPin> {
    let mut out: Vec<&SymPin> = sym.pins.iter().collect();
    for unit in &sym.units {
        out.extend(unit.pins.iter());
    }
    out
}

/// Build [`PinGeom`]s from a symbol's owned pins, tagging each with its 1-based
/// unit and dropping any pin missing the geometry we require (number/at/length).
///
/// Direct pins are unit 1; sub-block pins take the unit digit parsed from the
/// `<NAME>_<unit>_<bodystyle>` block name (unit 0 / common folded to 1).
/// `body` owns the pin geometry; `style_from` is the symbol whose
/// `pin_names`/`pin_numbers` settings apply (a derived symbol keeps its own).
fn collect_pins(body: &Symbol, style_from: &Symbol) -> Vec<PinGeom> {
    let style = |p: &SymPin| PinTextStyle {
        name_offset: style_from.pin_names_offset.unwrap_or(0.508),
        names_hidden: style_from.pin_names_hide,
        numbers_hidden: style_from.pin_numbers_hide,
        pin_hidden: p.hide,
        ..PinTextStyle::default()
    };
    let sym = body;
    let mut out: Vec<PinGeom> = sym.pins.iter().filter_map(|p| pin_geom(p, 1, style(p))).collect();
    for unit in &sym.units {
        let unit_no = unit
            .name
            .as_deref()
            .and_then(unit_number)
            // Unit 0 holds graphics / pins common to all units; PinGeom units
            // are 1-based, so fold it into unit 1.
            .map_or(1, |u| u.max(1));
        out.extend(unit.pins.iter().filter_map(|p| pin_geom(p, unit_no, style(p))));
    }
    out
}

/// Build a single [`PinGeom`] for `unit`, dropping pins lacking number/at/length.
fn pin_geom(p: &SymPin, unit: u8, text: PinTextStyle) -> Option<PinGeom> {
    Some(PinGeom {
        number: p.number.clone()?,
        name: p.name.clone().unwrap_or_default(),
        at: Point2::from(p.at?),
        angle: p.angle.unwrap_or(0.0),
        length: p.length?,
        unit,
        text,
    })
}

/// Extract the unit number from a sub-block name like `LM358_1_1`
/// (`<NAME>_<unit>_<bodystyle>`). Mirrors `symlib.rs::unit_number`.
fn unit_number(block_name: &str) -> Option<u8> {
    let mut parts = block_name.rsplitn(3, '_');
    let _bodystyle = parts.next()?;
    parts.next()?.parse().ok()
}

/// Extract and retarget the `(symbol …)` block for `lib_id`.
///
/// `body` is the symbol that owns the geometry (the derived symbol itself when
/// it has its own body, else the resolved parent). We slice the parent body's
/// balanced block out of the original symbol-library text via the CST span, then
/// rewrite names so the block is valid inside `(lib_symbols)`.
fn build_definition(
    texts: &[String],
    symbols: &[Symbol],
    lib_id: &str,
    requested_name: &str,
    requested: &Symbol,
    body: &Symbol,
) -> io::Result<String> {
    let body_name = body
        .name
        .as_deref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "symbol body has no name"))?;

    let block = texts
        .iter()
        .find_map(|text| symbol_block(text, body_name))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("could not locate (symbol {body_name:?} …) block in source"),
            )
        })?;

    // A derived symbol's body lives in its parent, but its properties do not:
    // KiCad permits every descendant in an `extends` chain to override fields
    // such as Value, Footprint, Datasheet, and Description.  Flatten those
    // overrides from the body outward before retargeting the definition.  A
    // plain copy of the parent body would silently advertise the parent's part
    // number and ratings in the generated schematic.
    let mut out = block;
    for descendant in inheritance_chain(symbols, requested, body) {
        let descendant_name = descendant.name.as_deref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "derived symbol in inheritance chain has no name",
            )
        })?;
        let descendant_block = texts
            .iter()
            .find_map(|text| symbol_block(text, descendant_name))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "could not locate derived (symbol {descendant_name:?} …) block in source"
                    ),
                )
            })?;
        out = overlay_properties(out, &descendant_block)?;
    }

    // 1) Retarget the top-level name atom to the fully-qualified lib_id.
    out = replace_top_name(&out, body_name, lib_id);

    // 2) Derived symbol: the body came from a different (parent) symbol, so
    //    rewrite the nested sub-block prefixes `Parent_*` -> `Requested_*` so
    //    KiCAD pairs the unit blocks with the derived symbol's bare name.
    if body_name != requested_name {
        out = out.replace(
            &format!("(symbol \"{body_name}_"),
            &format!("(symbol \"{requested_name}_"),
        );
    }

    Ok(out)
}

/// Return descendants from the body toward `requested`, excluding the body.
/// Applying their properties in this order implements nearest-child-wins
/// inheritance for chains of arbitrary depth.
fn inheritance_chain<'a>(
    symbols: &'a [Symbol],
    requested: &'a Symbol,
    body: &'a Symbol,
) -> Vec<&'a Symbol> {
    let body_name = body.name.as_deref();
    let mut cur = requested;
    let mut descendants = Vec::new();
    let mut seen = std::collections::HashSet::new();

    while cur.name.as_deref() != body_name {
        let Some(name) = cur.name.as_deref() else {
            break;
        };
        if !seen.insert(name) {
            break;
        }
        descendants.push(cur);
        let Some(parent) = cur.extends.as_deref().and_then(|p| find_symbol(symbols, p)) else {
            break;
        };
        cur = parent;
    }

    descendants.reverse();
    descendants
}

/// Replace the base definition's direct `(property …)` nodes with the direct
/// property nodes declared by one derived symbol.  CST spans keep arbitrary
/// quoted strings and formatting intact.
fn overlay_properties(mut base: String, derived: &str) -> io::Result<String> {
    for (key, replacement) in direct_properties(derived)? {
        if let Some(span) = direct_property_span(&base, &key)? {
            base.replace_range(span, &replacement);
        } else {
            // Custom properties need not exist on the body.  Keep them at the
            // top level, immediately before the first graphical unit block.
            let insert_at = first_direct_symbol_span(&base)?
                .map_or_else(|| base.rfind(')').unwrap_or(base.len()), |span| span.start);
            base.insert_str(insert_at, &format!("{replacement}\n\t\t"));
        }
    }
    Ok(base)
}

fn direct_properties(block: &str) -> io::Result<Vec<(String, String)>> {
    let mut properties = Vec::new();
    for (head, key, span) in direct_child_spans(block)? {
        if head == "property"
            && let Some(key) = key
        {
            properties.push((key, block[span].to_string()));
        }
    }
    Ok(properties)
}

fn direct_property_span(block: &str, wanted: &str) -> io::Result<Option<std::ops::Range<usize>>> {
    Ok(direct_child_spans(block)?
        .into_iter()
        .find_map(|(head, key, span)| {
            (head == "property" && key.as_deref() == Some(wanted)).then_some(span)
        }))
}

fn first_direct_symbol_span(block: &str) -> io::Result<Option<std::ops::Range<usize>>> {
    Ok(direct_child_spans(block)?
        .into_iter()
        .find_map(|(head, _, span)| (head == "symbol").then_some(span)))
}

type ChildSpan = (String, Option<String>, std::ops::Range<usize>);

fn direct_child_spans(block: &str) -> io::Result<Vec<ChildSpan>> {
    let doc =
        parse_one(block).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let Some(Node::List { items, .. }) = doc.nodes.first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "symbol definition is not an S-expression list",
        ));
    };
    Ok(items
        .iter()
        .filter_map(|node| {
            let Node::List { items, span } = node else {
                return None;
            };
            let Some(Node::Atom {
                atom: Atom::Symbol(head),
                ..
            }) = items.first()
            else {
                return None;
            };
            let second = match items.get(1) {
                Some(Node::Atom {
                    atom: Atom::Quoted(value),
                    ..
                }) => Some(value.clone()),
                _ => None,
            };
            Some((head.clone(), second, span.start..span.end))
        })
        .collect())
}

/// Slice the balanced `(symbol "<name>" …)` block out of `text` using the
/// CST node span (robust against nested parens, quoted strings, and comments).
fn symbol_block(text: &str, name: &str) -> Option<String> {
    let doc = parse_one(text).ok()?;
    // `parse_one` yields a single root: `(kicad_symbol_lib …)`.
    let root = doc.nodes.first()?;
    let Node::List { items, .. } = root else {
        return None;
    };
    for node in items {
        let Node::List { items: inner, span } = node else {
            continue;
        };
        // head atom must be `symbol`, second atom the quoted name.
        let head = inner.first();
        let is_symbol = matches!(
            head,
            Some(Node::Atom { atom: Atom::Symbol(s), .. }) if s == "symbol"
        );
        if !is_symbol {
            continue;
        }
        let matches_name = matches!(
            inner.get(1),
            Some(Node::Atom { atom: Atom::Quoted(s), .. }) if s == name
        );
        if matches_name {
            return Some(text[span.start..span.end].to_string());
        }
    }
    None
}

/// Replace the leading `(symbol "<old>"` name atom with `(symbol "<new>"`,
/// touching only the first occurrence (the top-level wrapper). Nested
/// sub-blocks are left intact.
fn replace_top_name(block: &str, old: &str, new: &str) -> String {
    let from = format!("(symbol \"{old}\"");
    let to = format!("(symbol \"{new}\"");
    block.replacen(&from, &to, 1)
}

/// Map a `kiutils_kicad::Error` to an `io::Error`, preserving I/O variants.
fn map_kiutils_err(e: kiutils_kicad::Error) -> io::Error {
    match e {
        kiutils_kicad::Error::Io(io) => io,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}
