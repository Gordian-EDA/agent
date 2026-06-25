//! KiCAD footprint `.kicad_mod` library access.
//!
//! Board files are owned by KiCAD IPC/CLI in the PCB flow; this crate only
//! indexes footprint libraries.

pub mod footlib;

/// KiCAD coordinate number formatting.
pub fn fmt_num(v: f64) -> String {
    let v = if v == 0.0 { 0.0 } else { v };
    format!("{v}")
}
