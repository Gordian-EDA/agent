//! The nm↔mm conversion pair for the KiCAD IPC coordinate domain.

/// Millimetres to the API's integer nanometres, rounding to the nearest.
pub fn mm_to_nm(value: f64) -> i64 {
    (value * 1_000_000.0).round() as i64
}

/// The API's integer nanometres back to millimetres.
pub fn nm_to_mm(nm: i64) -> f64 {
    nm as f64 / 1_000_000.0
}
