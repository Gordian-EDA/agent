//! Pin flow: the one thing about a pin the layout stages need that the symbol library
//! alone does not say in their vocabulary.

/// A pin's electrical flow direction, resolved from the symbol library by the caller so
/// the layout stages never open one themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PinFlow {
    /// An output pin (drives the net).
    Source,
    /// An input pin (listens to the net).
    Sink,
}
