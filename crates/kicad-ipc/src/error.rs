/// `AS_NOT_READY` (KiCAD just started) - transient, retry.
pub(crate) const AS_NOT_READY: i32 = 4;
/// `AS_BUSY` (KiCAD mid-operation) - transient, retry.
pub(crate) const AS_BUSY: i32 = 7;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("nng transport: {0}")]
    Nng(#[from] nng::Error),
    #[error("encoding request: {0}")]
    Encode(#[from] prost::EncodeError),
    #[error("decoding response: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("KiCAD API error (status {code}): {message}")]
    Api { code: i32, message: String },
    #[error("response carried no message payload")]
    EmptyResponse,
    #[error("response Any did not hold the expected `{0}`")]
    TypeMismatch(&'static str),
    #[error("no PCB document open in KiCAD (call open_board first; is a board loaded?)")]
    NoBoard,
    #[error("KiCAD rejected an item (status {code}): {message}")]
    Item { code: i32, message: String },
    #[error("launching KiCAD: {0}")]
    Spawn(String),
    #[error("timed out waiting for the KiCAD IPC socket to appear")]
    LaunchTimeout,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("unsupported KiCAD IPC operation: {0}")]
    Unsupported(String),
}

impl Error {
    pub fn is_transient_api_ready_error(&self) -> bool {
        matches!(self, Error::Api { code, .. } if *code == AS_NOT_READY || *code == AS_BUSY)
    }

    pub fn is_transport_timeout(&self) -> bool {
        matches!(self, Error::Nng(err) if err.to_string().contains("Timed out"))
    }

    pub fn is_type_mismatch(&self) -> bool {
        matches!(self, Error::TypeMismatch(_))
    }
}
