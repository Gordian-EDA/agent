//! The crate-local error contract.
//!
//! Library operations that can fail for a *reason the caller must
//! distinguish* (a malformed id, a missing footprint, an unreadable file, a
//! parse failure) return [`Error`] rather than collapsing every failure into
//! `Option::None` or a bare [`std::io::Error`]. Pure lookups where absence is
//! expected (e.g. [`crate::FootprintCatalog::contains`]) still return `bool`.

use std::path::{Path, PathBuf};

use crate::id::FootprintId;

/// Convenience alias for fallible footprint-library operations.
pub type Result<T> = std::result::Result<T, Error>;

/// What went wrong while reading a footprint library.
#[derive(Debug)]
pub enum Error {
    /// A library nickname was empty or contained a `:` separator.
    InvalidLibraryId { value: String },
    /// A `Nickname:Name` id was malformed (no separator, empty half, …).
    InvalidFootprintId { value: String },
    /// The id is well-formed but no such footprint is indexed.
    NotFound { id: FootprintId },
    /// A filesystem error, tagged with the path it happened on.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The `.kicad_mod` text could not be parsed into a footprint.
    Parse { path: PathBuf, message: String },
}

impl Error {
    /// The filesystem path this error concerns, if any.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Error::Io { path, .. } | Error::Parse { path, .. } => Some(path),
            _ => None,
        }
    }

    /// Whether this is a [`Error::NotFound`] (well-formed but absent id).
    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::NotFound { .. })
    }

    /// Whether this is a [`Error::Parse`] (malformed footprint content).
    pub fn is_parse(&self) -> bool {
        matches!(self, Error::Parse { .. })
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::InvalidLibraryId { value } => write!(f, "invalid library id `{value}`"),
            Error::InvalidFootprintId { value } => write!(f, "invalid footprint id `{value}`"),
            Error::NotFound { id } => write!(f, "unknown footprint `{id}`"),
            Error::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Error::Parse { path, message } => write!(f, "{}: {message}", path.display()),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
