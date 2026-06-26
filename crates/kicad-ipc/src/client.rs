use std::time::Duration;

use prost::{Message, Name};

use crate::{
    Error,
    error::{AS_BUSY, AS_NOT_READY},
    proto,
};

use proto::kiapi::common::commands::{GetVersion, GetVersionResponse};
use proto::kiapi::common::{ApiRequest, ApiRequestHeader, ApiResponse};

/// Default IPC socket (Linux/macOS).
const DEFAULT_SOCKET: &str = "ipc:///tmp/kicad/api.sock";
/// `ApiStatusCode::AS_OK`.
const AS_OK: i32 = 1;
/// How long to keep retrying a transient NOT_READY/BUSY before giving up.
const RETRY_BUDGET: Duration = Duration::from_secs(40);

/// A connection to a running KiCAD instance's IPC API server.
///
/// Speaks the protobuf-over-NNG REQ/REP protocol directly. The `kicad_token` is
/// bootstrapped from the first reply (an external client may start empty), so no
/// out-of-band token is needed. One `Kicad` owns one NNG REQ socket.
pub struct Kicad {
    socket: nng::Socket,
    token: String,
    client_name: String,
    /// The open PCB document, cached by [`Kicad::open_board`].
    pub(crate) board_doc: Option<proto::kiapi::common::types::DocumentSpecifier>,
}

impl Kicad {
    /// Connect to the server at the default socket path.
    pub fn connect() -> Result<Self, Error> {
        Self::connect_to(DEFAULT_SOCKET)
    }

    /// Connect to a specific NNG socket URL (e.g. `ipc:///tmp/kicad/api.sock`).
    pub fn connect_to(socket_url: &str) -> Result<Self, Error> {
        let socket = nng::Socket::new(nng::Protocol::Req0)?;
        // Don't hang forever if KiCAD is wedged, but allow slow ops (save with
        // zone refill, autoroute) to complete.
        use nng::options::Options;
        let _ = socket.set_opt::<nng::options::RecvTimeout>(Some(Duration::from_secs(120)));
        let _ = socket.set_opt::<nng::options::SendTimeout>(Some(Duration::from_secs(30)));
        socket.dial(socket_url)?;
        Ok(Self {
            socket,
            token: String::new(),
            client_name: format!("gordian-agent-{}", std::process::id()),
            board_doc: None,
        })
    }

    /// Send a command (wrapped in `Any`), return the raw [`ApiResponse`]. Bootstraps
    /// the instance token from the first reply and surfaces API-level errors.
    fn send_request<C: Message + Name>(&mut self, cmd: &C) -> Result<ApiResponse, Error> {
        let any = prost_types::Any::from_msg(cmd)?;
        let deadline = std::time::Instant::now() + RETRY_BUDGET;
        loop {
            let req = ApiRequest {
                header: Some(ApiRequestHeader {
                    kicad_token: self.token.clone(),
                    client_name: self.client_name.clone(),
                }),
                message: Some(any.clone()),
            };
            let bytes = req.encode_to_vec();
            self.socket
                .send(nng::Message::from(&bytes[..]))
                .map_err(|(_, e)| Error::Nng(e))?;
            let reply = self.socket.recv()?;
            let resp = ApiResponse::decode(reply.as_slice())?;
            if self.token.is_empty()
                && let Some(h) = &resp.header
                && !h.kicad_token.is_empty()
            {
                self.token = h.kicad_token.clone();
            }
            if let Some(st) = &resp.status {
                // KiCAD just started / is mid-operation - back off and retry.
                if (st.status == AS_NOT_READY || st.status == AS_BUSY)
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(400));
                    continue;
                }
                if st.status != AS_OK {
                    return Err(Error::Api {
                        code: st.status,
                        message: st.error_message.clone(),
                    });
                }
            }
            return Ok(resp);
        }
    }

    /// Send a command and decode the typed reply (unpacked from the response `Any`).
    pub fn call<C, R>(&mut self, cmd: &C) -> Result<R, Error>
    where
        C: Message + Name,
        R: Message + Name + Default,
    {
        let resp = self.send_request(cmd)?;
        let any = resp.message.ok_or(Error::EmptyResponse)?;
        any.to_msg::<R>().map_err(|_| Error::TypeMismatch(R::NAME))
    }

    /// Send a command whose response carries no payload (just check status).
    pub fn call_void<C: Message + Name>(&mut self, cmd: &C) -> Result<(), Error> {
        self.send_request(cmd)?;
        Ok(())
    }

    /// `(major, minor, patch, full_version)` of the connected KiCAD.
    pub fn version(&mut self) -> Result<(u32, u32, u32, String), Error> {
        let r: GetVersionResponse = self.call(&GetVersion {})?;
        let v = r.version.unwrap_or_default();
        Ok((v.major, v.minor, v.patch, v.full_version))
    }

    pub(crate) fn ensure_footprint_update_supported(&mut self) -> Result<(), Error> {
        let (major, minor, patch, full) = self.version()?;
        if !footprint_update_supported(major, minor, patch) {
            return Err(Error::Unsupported(format!(
                "KiCAD {full} has unstable IPC FootprintInstance UpdateItems; \
                 footprint placement requires KiCAD 9.0.3+ or KiCAD 10"
            )));
        }
        Ok(())
    }
}

pub(crate) fn footprint_update_supported(major: u32, minor: u32, patch: u32) -> bool {
    !(major == 9 && minor == 0 && patch <= 2)
}

#[cfg(test)]
mod tests {
    use super::footprint_update_supported;

    #[test]
    fn footprint_updates_gate_known_unstable_kicad_902() {
        assert!(!footprint_update_supported(9, 0, 0));
        assert!(!footprint_update_supported(9, 0, 2));
        assert!(footprint_update_supported(9, 0, 3));
        assert!(footprint_update_supported(10, 0, 0));
    }
}
