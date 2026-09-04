//! What a finished placement reports: its emitted crossing counts ([`Crossings`]) and
//! the diagnostics that ride with them ([`PlaceResult`]).

use serde::{Deserialize, Serialize};

/// The three "a wire runs through something" counts of a placement as it would
/// SHIP. They are NOT interchangeable, so they are named rather than a positional
/// triple (mirrors `crate::result::EmitOutput`'s `*_crossings` fields).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Crossings {
    /// Wires routed THROUGH a 2-pin part body (transverse / collinear / parallel).
    pub body: usize,
    /// Wires routed through a 3+-pin IC package body.
    pub ic: usize,
    /// Wire-wire crossings between different nets.
    pub wire: usize,
}

/// What a finished placement measures. Purely DIAGNOSTIC: the geometry itself lives in
/// the placed items, so this never carries a second, driftable copy of the layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceResult {
    /// Truthfulness breaks of the shipped placement — any value > 0 mis-wires.
    pub truthfulness_breaks: usize,
    /// Readability warnings on the shipped sheet.
    pub warnings: usize,
    /// The body / IC / wire crossing triple of the shipped sheet.
    pub crossings: Crossings,
}
