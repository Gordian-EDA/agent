//! `sch-engine` — turns a [`circuit_lang::Design`] into a real `.kicad_sch`
//! file (and back), deterministically.
//!
//! This crate is built incrementally per the sch-engine plan. Task 2
//! establishes the deterministic primitives every later stage relies on:
//!
//! - [`ids`] — content-derived (UUIDv5) identifiers so re-emitting the same
//!   `Design` yields byte-identical output (spec §5.1).
//! - [`grid`] — snapping coordinates onto KiCAD's 1.27 mm schematic grid.

pub mod emit;
pub mod grid;
pub mod ids;
pub mod place;
pub mod reconcile;

use std::io;

use circuit_lang::Design;
use kicad_bridge::env::KicadEnv;

pub use crate::reconcile::emit_design_reconciled;

/// Emit a complete `circuit_lang::Design` as a deterministic, ERC-clean
/// `.kicad_sch` document.
///
/// This is **MVP acceptance criterion #1**: the validated bluepill design must
/// pass through here into a schematic KiCAD 10 loads and ERCs with zero errors.
/// The pipeline:
///
/// 1. **Placement.** [`place::place`] assigns every component a grid-snapped
///    sheet position deterministically (same `Design` → same positions).
/// 2. **Symbols.** Each component (across all blocks) becomes a placed symbol
///    via [`SchematicWriter::add_symbol`] at its placed position, angle 0.
/// 3. **Connectivity & no-connects.** For every pin (component-level *and*
///    per-unit): a `PinTarget::Net` pin gets a net-name label at its endpoint
///    ([`SchematicWriter::add_pin_label`]); a `PinTarget::NoConnect` pin — the
///    kernel auto-NCs every unmentioned symbol pin — gets a `(no_connect)`
///    marker at the same endpoint ([`SchematicWriter::add_no_connect`]) so ERC
///    treats the disconnection as intentional.
/// 4. **Power sources.** A net carrying a power-*input* pin (e.g. GND, VBUS, or
///    a regulator's VI) is reported undriven by KiCAD ERC unless a power *source*
///    sits on it. One `power:PWR_FLAG` per such net supplies the missing source,
///    clearing `power_pin_not_driven`. The flags are placed in their own column
///    to the right of the placed design, away from any component.
/// 5. **Assemble.** [`SchematicWriter::finish`] emits the document.
///
/// ## Which nets get a power flag, and why by *etype*
///
/// Plain net-name labels already connect every pin (the kernel guarantees power
/// inputs are bound to nets). The residual ERC error class is "power input not
/// driven by a power output" (`power_pin_not_driven`, ERROR severity) on any net
/// whose pins include a power *input* but no power *output*. A `PWR_FLAG` (a
/// `power_out` graphic with no footprint, excluded from the netlist by its `#FLG`
/// reference) is the minimal, standard fix and supplies the missing source.
///
/// The set of nets to flag is computed by **pin etype**, not by the author's
/// `rails:` declarations. Lint only requires a power-input pin to be on *some*
/// net — not a power-declared one — so an LLM can legitimately put a power-input
/// pin (e.g. a regulator's VI) on a net it forgot to list under `rails:`. Were we
/// to flag only declared power nets, that net would ERC-fail. So we query pin
/// types from the symbol libraries and flag
/// `((power_input_nets ∪ declared_power_used_nets) − driven_nets)`:
///   - every net carrying a power-input pin (the general case), plus
///   - every *declared* power net that's referenced (a declared GND rail with
///     only passive connections still wants a flag),
///   - minus every net already driven by a real power *output* pin. The AMS1117
///     regulator's `VO` pin is a power-output driving 3V3; a flag there would put
///     two power outputs on one net — itself an ERC error (`pin_to_pin`).
///
/// The set is deduplicated and iterated in sorted (`BTreeSet`) order for stable
/// `#FLG` numbering. Declared-but-unused rails, and nets with no power pins at
/// all, get no flag.
///
/// Returns the first I/O error from symbol resolution / endpoint computation, or
/// the assembled schematic text.
pub fn emit_design(env: &KicadEnv, design: &Design) -> io::Result<String> {
    // The from-scratch emit is reconciliation against an empty prior: every
    // component is "new", so all positions come from the placer. Keeping a single
    // code path means `ap_*` identity tags and the placement/power-flag logic can
    // never drift between the one-shot and the re-emit cases.
    emit_design_reconciled(env, design, None)
}
