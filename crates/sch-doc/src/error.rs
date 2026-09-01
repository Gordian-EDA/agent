//! Failures the document layer can report.

/// Everything that can go wrong parsing, editing or saving a schematic.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("malformed s-expression: {0}")]
    Syntax(#[from] kiutils_sexpr::ParseError),
    #[error("not a schematic: expected a `kicad_sch` root, got `{0}`")]
    NotSchematic(String),
    #[error("no symbol with uuid {0}")]
    UnknownSymbol(String),
    #[error("no symbol with reference {0}")]
    UnknownReference(String),
    #[error("{0} names several units of one symbol; address a unit by its uuid")]
    AmbiguousReference(String),
    #[error("no snapshot {0}")]
    UnknownSnapshot(usize),
    #[error(
        "{0} is annotated by the parent hierarchy, not by this sheet; \
         rename it where the sheet is instantiated"
    )]
    ForeignInstances(String),
    #[error("symbol library: {0}")]
    Library(String),
}

/// Result alias for the document layer.
pub type Result<T> = std::result::Result<T, Error>;
