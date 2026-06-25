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
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A library nickname was empty or contained a `:` separator.
    #[error("invalid library id `{value}`")]
    InvalidLibraryId { value: String },
    /// A `Nickname:Name` id was malformed (no separator, empty half, …).
    #[error("invalid footprint id `{value}`")]
    InvalidFootprintId { value: String },
    /// The id is well-formed but no such footprint is indexed.
    #[error("unknown footprint `{id}`")]
    NotFound { id: FootprintId },
    /// A filesystem error, tagged with the path it happened on.
    #[error("{}: {source}", .path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The `.kicad_mod` text could not be parsed into a footprint.
    #[error("{}: {message}", .path.display())]
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
