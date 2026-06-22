//! Native Rust client for the KiCAD IPC API (KiCAD 9.0+).
//!
//! KiCAD hosts a per-instance API server (protobuf messages over an NNG REQ/REP
//! socket); this crate speaks that protocol directly — no Python. The generated
//! message types live under [`proto`]; the high-level board client is layered on
//! top (added incrementally). See the validated wire protocol in the project
//! memory `kicad-ipc-protocol`.

/// Generated protobuf types — the full `kiapi::{common, board, ...}` module tree.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/_proto.rs"));
}

use std::time::Duration;

use prost::{Message, Name};
use proto::kiapi::common::commands::{GetVersion, GetVersionResponse};
use proto::kiapi::common::{ApiRequest, ApiRequestHeader, ApiResponse};

/// Default IPC socket when `KICAD_API_SOCKET` is unset (Linux/macOS).
const DEFAULT_SOCKET: &str = "ipc:///tmp/kicad/api.sock";
/// `ApiStatusCode::AS_OK`.
const AS_OK: i32 = 1;

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
}

/// A connection to a running KiCAD instance's IPC API server.
///
/// Speaks the protobuf-over-NNG REQ/REP protocol directly. The `kicad_token` is
/// bootstrapped from the first reply (an external client may start empty), so no
/// out-of-band token is needed. One `Kicad` owns one NNG REQ socket.
pub struct Kicad {
    socket: nng::Socket,
    token: String,
    client_name: String,
}

impl Kicad {
    /// Connect to the server at `$KICAD_API_SOCKET` (else the default path).
    pub fn connect() -> Result<Self, Error> {
        let path = std::env::var("KICAD_API_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_string());
        Self::connect_to(&path)
    }

    /// Connect to a specific NNG socket URL (e.g. `ipc:///tmp/kicad/api.sock`).
    pub fn connect_to(socket_url: &str) -> Result<Self, Error> {
        let socket = nng::Socket::new(nng::Protocol::Req0)?;
        // Don't hang forever if KiCAD is busy / wedged.
        use nng::options::Options;
        let _ = socket.set_opt::<nng::options::RecvTimeout>(Some(Duration::from_secs(15)));
        let _ = socket.set_opt::<nng::options::SendTimeout>(Some(Duration::from_secs(15)));
        socket.dial(socket_url)?;
        Ok(Self {
            socket,
            token: String::new(),
            client_name: format!("auto-pcb-agent-{}", std::process::id()),
        })
    }

    /// Send a command (wrapped in `google.protobuf.Any`) and decode the typed reply.
    /// Bootstraps the instance token from the first reply and surfaces API errors.
    pub fn call<C, R>(&mut self, cmd: &C) -> Result<R, Error>
    where
        C: Message + Name,
        R: Message + Name + Default,
    {
        let any = prost_types::Any::from_msg(cmd)?;
        let req = ApiRequest {
            header: Some(ApiRequestHeader {
                kicad_token: self.token.clone(),
                client_name: self.client_name.clone(),
            }),
            message: Some(any),
        };
        let bytes = req.encode_to_vec();
        self.socket
            .send(nng::Message::from(&bytes[..]))
            .map_err(|(_, e)| Error::Nng(e))?;
        let reply = self.socket.recv()?;
        let resp = ApiResponse::decode(reply.as_slice())?;

        if self.token.is_empty() {
            if let Some(h) = &resp.header {
                if !h.kicad_token.is_empty() {
                    self.token = h.kicad_token.clone();
                }
            }
        }
        if let Some(st) = &resp.status {
            if st.status != AS_OK {
                return Err(Error::Api {
                    code: st.status,
                    message: st.error_message.clone(),
                });
            }
        }
        let any = resp.message.ok_or(Error::EmptyResponse)?;
        any.to_msg::<R>().map_err(|_| Error::TypeMismatch(R::NAME))
    }

    /// `(major, minor, patch, full_version)` of the connected KiCAD.
    pub fn version(&mut self) -> Result<(u32, u32, u32, String), Error> {
        let r: GetVersionResponse = self.call(&GetVersion {})?;
        let v = r.version.unwrap_or_default();
        Ok((v.major, v.minor, v.patch, v.full_version))
    }
}
