//! Typed library and footprint identifiers.
//!
//! KiCAD names a footprint by a `Nickname:Name` pair (the *library id*). Rather
//! than thread bare `&str` lib ids through every signature, the catalog parses
//! and validates them once into [`LibraryId`] / [`FootprintId`], so a malformed
//! id is rejected at the boundary instead of silently missing later.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A footprint library nickname, e.g. `Resistor_SMD`.
///
/// Never empty and never contains the `:` id separator.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LibraryId(String);

impl LibraryId {
    /// Validate `value` as a library nickname.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() || value.contains(':') {
            return Err(Error::InvalidLibraryId { value });
        }
        Ok(LibraryId(value))
    }

    /// The nickname as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LibraryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for LibraryId {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        LibraryId::new(s)
    }
}

impl AsRef<str> for LibraryId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A fully-qualified footprint id: a [`LibraryId`] plus a footprint name.
///
/// Round-trips with its `Nickname:Name` string form via [`Display`](fmt::Display)
/// and [`FootprintId::parse`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FootprintId {
    library: LibraryId,
    name: String,
}

impl FootprintId {
    /// Build an id from an already-validated [`LibraryId`] and a footprint name.
    pub fn new(library: LibraryId, name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.is_empty() || name.contains(':') {
            return Err(Error::InvalidFootprintId {
                value: format!("{library}:{name}"),
            });
        }
        Ok(FootprintId { library, name })
    }

    /// Parse a `Nickname:Name` string.
    pub fn parse(value: &str) -> Result<Self> {
        let invalid = || Error::InvalidFootprintId {
            value: value.to_string(),
        };
        let (lib, name) = value.split_once(':').ok_or_else(invalid)?;
        let library = LibraryId::new(lib).map_err(|_| invalid())?;
        FootprintId::new(library, name).map_err(|_| invalid())
    }

    /// The owning library nickname.
    pub fn library(&self) -> &LibraryId {
        &self.library
    }

    /// The bare footprint name (the `.kicad_mod` stem).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The canonical `Nickname:Name` string form.
    pub fn as_lib_id(&self) -> String {
        format!("{}:{}", self.library.as_str(), self.name)
    }
}

impl fmt::Display for FootprintId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.library, self.name)
    }
}

impl FromStr for FootprintId {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        FootprintId::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_lib_id() {
        let id = FootprintId::parse("Resistor_SMD:R_0603_1608Metric").unwrap();
        assert_eq!(id.library().as_str(), "Resistor_SMD");
        assert_eq!(id.name(), "R_0603_1608Metric");
        assert_eq!(id.to_string(), "Resistor_SMD:R_0603_1608Metric");
        assert_eq!(id.as_lib_id(), "Resistor_SMD:R_0603_1608Metric");
    }

    #[test]
    fn rejects_missing_separator() {
        let err = FootprintId::parse("R_0603_1608Metric").unwrap_err();
        assert!(matches!(err, Error::InvalidFootprintId { .. }));
    }

    #[test]
    fn rejects_empty_library() {
        assert!(FootprintId::parse(":R_0603").is_err());
        assert!(LibraryId::new("").is_err());
    }

    #[test]
    fn rejects_empty_name() {
        assert!(FootprintId::parse("Resistor_SMD:").is_err());
    }

    #[test]
    fn display_round_trips_through_parse() {
        let id = FootprintId::parse("Package_TO_SOT_SMD:SOT-23").unwrap();
        let reparsed: FootprintId = id.to_string().parse().unwrap();
        assert_eq!(id, reparsed);
    }
}
