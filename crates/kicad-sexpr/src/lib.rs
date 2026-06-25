//! Small KiCAD S-expression formatting helpers.
//!
//! Footprint library access moved to `kicad-footprint`. This crate remains only
//! for board writer formatting glue until that helper is moved closer to the
//! writer.

/// KiCAD coordinate number formatting.
pub fn fmt_num(v: f64) -> String {
    let v = if v == 0.0 { 0.0 } else { v };
    format!("{v}")
}
