//! Pin geometry and `(lib_symbols)` definition extraction for a single symbol.
//!
//! The schematic writer (`sch-io::write`) needs two things per used symbol that the
//! geometry-free `symbol_contract::PinMeta` deliberately omits:
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
use std::path::PathBuf;

use kiutils_kicad::{SymPin, Symbol, SymbolLibFile};
use kiutils_sexpr::{Atom, Node, parse_one};

use kicad_cli::env::KicadEnv;

/// Local geometry of a single symbol pin, in symbol coordinates.
///
/// `at` is the pin's connection-point root in millimetres (symbol Y grows
/// upward); the pin line extends `length` mm from there along `angle`
/// (degrees). The schematic-space endpoint is computed by `sch-io::write` after
/// applying the instance's position/rotation/mirror.
#[derive(Debug, Clone, PartialEq)]
pub struct PinGeom {
    /// Pin number (e.g. `"1"`, `"2"`, `"A3"`).
    pub number: String,
    /// Pin name (e.g. `"~"`, `"VCC"`, `"GND"`).
    pub name: String,
    /// Local position of the pin's connection root, `[x, y]` in mm.
    pub at: [f64; 2],
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
}

/// Geometry plus the embeddable `(lib_symbols)` definition for one symbol.
#[derive(Debug, Clone, PartialEq)]
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
    pub fn load(env: &KicadEnv, lib_id: &str) -> io::Result<SymbolGeometry> {
        let key = (env.symbol_dir.to_string_lossy().into_owned(), lib_id.to_string());
        if let Some(g) = GEOM_CACHE.with(|c| c.borrow().get(&key).cloned()) {
            return Ok(g);
        }
        let g = Self::load_uncached(env, lib_id)?;
        GEOM_CACHE.with(|c| c.borrow_mut().insert(key, g.clone()));
        Ok(g)
    }

    /// The actual library read + resolve (uncached). See [`Self::load`].
    fn load_uncached(env: &KicadEnv, lib_id: &str) -> io::Result<SymbolGeometry> {
        let (lib, name) = lib_id.split_once(':').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("lib_id must be 'Lib:Name', got {lib_id:?}"),
            )
        })?;

        let path: PathBuf = env.symbol_dir.join(format!("{lib}.kicad_sym"));
        let text = std::fs::read_to_string(&path)?;

        // Typed AST: pins, units, and the `extends` target per symbol.
        let doc = SymbolLibFile::read(&path).map_err(map_kiutils_err)?;
        let symbols = &doc.ast().symbols;

        let sym = find_symbol(symbols, name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("symbol {name:?} not in {}", path.display()),
            )
        })?;

        // Resolve the symbol that actually carries the geometry/body: follow
        // `extends` while the current symbol has no pins of its own.
        let body = resolve_body(symbols, sym);

        let pins = collect_pins(body);

        // Raw definition: the parent body's block, retargeted to `lib_id` and
        // (when derived) with nested sub-block prefixes rewritten.
        let raw_definition = build_definition(&text, lib_id, name, body)?;

        Ok(SymbolGeometry {
            lib_id: lib_id.to_string(),
            pins,
            raw_definition,
        })
    }

    /// The balanced `(symbol "Lib:Name" …)` block for `(lib_symbols)`.
    pub fn definition_sexpr(&self) -> &str {
        &self.raw_definition
    }

    /// Approximate body extents `[width, height]` in mm, derived from pin
    /// connection points (pins bound the drawn body closely for almost every
    /// KiCAD symbol). Floors at 5.08 mm and pads 2.54 mm per side so even a
    /// bare two-pin passive gets a sane footprint.
    pub fn approx_size(&self) -> [f64; 2] {
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for p in &self.pins {
            min_x = min_x.min(p.at[0]);
            max_x = max_x.max(p.at[0]);
            min_y = min_y.min(p.at[1]);
            max_y = max_y.max(p.at[1]);
        }
        [
            (max_x - min_x).max(5.08) + 5.08,
            (max_y - min_y).max(5.08) + 5.08,
        ]
    }
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
fn collect_pins(sym: &Symbol) -> Vec<PinGeom> {
    let mut out: Vec<PinGeom> = sym.pins.iter().filter_map(|p| pin_geom(p, 1)).collect();
    for unit in &sym.units {
        let unit_no = unit
            .name
            .as_deref()
            .and_then(unit_number)
            // Unit 0 holds graphics / pins common to all units; PinGeom units
            // are 1-based, so fold it into unit 1.
            .map_or(1, |u| u.max(1));
        out.extend(unit.pins.iter().filter_map(|p| pin_geom(p, unit_no)));
    }
    out
}

/// Build a single [`PinGeom`] for `unit`, dropping pins lacking number/at/length.
fn pin_geom(p: &SymPin, unit: u8) -> Option<PinGeom> {
    Some(PinGeom {
        number: p.number.clone()?,
        name: p.name.clone().unwrap_or_default(),
        at: p.at?,
        angle: p.angle.unwrap_or(0.0),
        length: p.length?,
        unit,
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
/// balanced block out of the original `.kicad_sym` text via the CST span, then
/// rewrite names so the block is valid inside `(lib_symbols)`.
fn build_definition(
    text: &str,
    lib_id: &str,
    requested_name: &str,
    body: &Symbol,
) -> io::Result<String> {
    let body_name = body
        .name
        .as_deref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "symbol body has no name"))?;

    let block = symbol_block(text, body_name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not locate (symbol {body_name:?} …) block in source"),
        )
    })?;

    // 1) Retarget the top-level name atom to the fully-qualified lib_id.
    let mut out = replace_top_name(&block, body_name, lib_id);

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
