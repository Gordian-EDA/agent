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

pub mod session;
pub use session::Session;

use std::time::Duration;

use prost::{Message, Name};
use proto::kiapi::common::commands::{GetVersion, GetVersionResponse};
use proto::kiapi::common::{ApiRequest, ApiRequestHeader, ApiResponse};

/// Default IPC socket when `KICAD_API_SOCKET` is unset (Linux/macOS).
const DEFAULT_SOCKET: &str = "ipc:///tmp/kicad/api.sock";
/// `ApiStatusCode::AS_OK`.
const AS_OK: i32 = 1;
/// `AS_NOT_READY` (KiCAD just started) — transient, retry.
const AS_NOT_READY: i32 = 4;
/// `AS_BUSY` (KiCAD mid-operation) — transient, retry.
const AS_BUSY: i32 = 7;
/// How long to keep retrying a transient NOT_READY/BUSY before giving up.
const RETRY_BUDGET: Duration = Duration::from_secs(40);

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
    /// The open PCB document, cached by [`Kicad::open_board`].
    board_doc: Option<proto::kiapi::common::types::DocumentSpecifier>,
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
        // Don't hang forever if KiCAD is wedged, but allow slow ops (save with
        // zone refill, autoroute) to complete.
        use nng::options::Options;
        let _ = socket.set_opt::<nng::options::RecvTimeout>(Some(Duration::from_secs(120)));
        let _ = socket.set_opt::<nng::options::SendTimeout>(Some(Duration::from_secs(30)));
        socket.dial(socket_url)?;
        Ok(Self {
            socket,
            token: String::new(),
            client_name: format!("auto-pcb-agent-{}", std::process::id()),
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
                    && !h.kicad_token.is_empty() {
                        self.token = h.kicad_token.clone();
                    }
            if let Some(st) = &resp.status {
                // KiCAD just started / is mid-operation — back off and retry.
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
}

// ── Board read / edit ────────────────────────────────────────────────────────

use proto::kiapi::board::types::{FootprintInstance, Track};
use proto::kiapi::common::commands::{
    BeginCommit, BeginCommitResponse, CommitAction, CreateItems, CreateItemsResponse, EndCommit,
    GetItems, GetItemsResponse, GetOpenDocuments, GetOpenDocumentsResponse, ItemStatusCode,
    SaveDocument, UpdateItems, UpdateItemsResponse,
};
use proto::kiapi::common::types::{DocumentType, ItemHeader, KiCadObjectType};

/// Turn a per-item `ItemStatus` into an error unless it is `ISC_OK`.
fn check_item_status(status: &Option<proto::kiapi::common::commands::ItemStatus>) -> Result<(), Error> {
    if let Some(s) = status
        && s.code != ItemStatusCode::IscOk as i32 {
            return Err(Error::Item {
                code: s.code,
                message: s.error_message.clone(),
            });
        }
    Ok(())
}

impl Kicad {
    /// Find and cache the open PCB document. Call once before any board op.
    pub fn open_board(&mut self) -> Result<(), Error> {
        let resp: GetOpenDocumentsResponse = self.call(&GetOpenDocuments {
            r#type: DocumentType::DoctypePcb as i32,
        })?;
        self.board_doc = resp.documents.into_iter().next();
        if self.board_doc.is_none() {
            return Err(Error::NoBoard);
        }
        Ok(())
    }

    fn header(&self) -> Result<ItemHeader, Error> {
        Ok(ItemHeader {
            document: Some(self.board_doc.clone().ok_or(Error::NoBoard)?),
            container: None,
            field_mask: None,
        })
    }

    /// Raw board items of the given object types (packed in `Any`).
    pub fn get_items(&mut self, types: &[KiCadObjectType]) -> Result<Vec<prost_types::Any>, Error> {
        let header = self.header()?;
        let resp: GetItemsResponse = self.call(&GetItems {
            header: Some(header),
            types: types.iter().map(|t| *t as i32).collect(),
        })?;
        Ok(resp.items)
    }

    /// All footprints on the board.
    pub fn footprints(&mut self) -> Result<Vec<FootprintInstance>, Error> {
        self.get_items(&[KiCadObjectType::KotPcbFootprint])?
            .into_iter()
            .map(|a| {
                a.to_msg::<FootprintInstance>()
                    .map_err(|_| Error::TypeMismatch("FootprintInstance"))
            })
            .collect()
    }

    /// All track segments on the board.
    pub fn tracks(&mut self) -> Result<Vec<Track>, Error> {
        self.get_items(&[KiCadObjectType::KotPcbTrace])?
            .into_iter()
            .map(|a| a.to_msg::<Track>().map_err(|_| Error::TypeMismatch("Track")))
            .collect()
    }

    /// Create new board items (tracks, vias, zones, ...). Pack each with
    /// `prost_types::Any::from_msg(&item)`.
    pub fn create_items(&mut self, items: Vec<prost_types::Any>) -> Result<(), Error> {
        let header = self.header()?;
        let resp: CreateItemsResponse = self.call(&CreateItems {
            header: Some(header),
            items,
            container: None,
        })?;
        for r in &resp.created_items {
            check_item_status(&r.status)?;
        }
        Ok(())
    }

    /// Update existing board items (e.g. a moved footprint).
    pub fn update_items(&mut self, items: Vec<prost_types::Any>) -> Result<(), Error> {
        let header = self.header()?;
        let resp: UpdateItemsResponse = self.call(&UpdateItems {
            header: Some(header),
            items,
        })?;
        for r in &resp.updated_items {
            check_item_status(&r.status)?;
        }
        Ok(())
    }

    /// Run `f`'s edits inside ONE KiCAD commit (a single undo step). Commits on
    /// success, drops on error.
    pub fn commit<F>(&mut self, message: &str, f: F) -> Result<(), Error>
    where
        F: FnOnce(&mut Self) -> Result<(), Error>,
    {
        let begun: BeginCommitResponse = self.call(&BeginCommit {})?;
        let id = begun.id;
        let result = f(self);
        let action = if result.is_ok() {
            CommitAction::CmaCommit
        } else {
            CommitAction::CmaDrop
        };
        self.call_void(&EndCommit {
            id,
            action: action as i32,
            message: message.to_string(),
        })?;
        result
    }

    /// Save the open board to disk.
    pub fn save(&mut self) -> Result<(), Error> {
        let document = Some(self.board_doc.clone().ok_or(Error::NoBoard)?);
        self.call_void(&SaveDocument { document })
    }
}

// ── Nets & net classes (design rules: "wide copper for power") ───────────────

use proto::kiapi::board::commands::{GetNets, NetsResponse};
use proto::kiapi::common::commands::SetNetClasses;
use proto::kiapi::common::project::{NetClass, NetClassBoardSettings, NetClassType};
use proto::kiapi::common::types::{Distance, MapMergeMode};

impl Kicad {
    /// Names of all nets on the open board.
    pub fn nets(&mut self) -> Result<Vec<String>, Error> {
        let board = Some(self.board_doc.clone().ok_or(Error::NoBoard)?);
        let resp: NetsResponse = self.call(&GetNets {
            board,
            netclass_filter: vec![],
        })?;
        Ok(resp.nets.into_iter().map(|n| n.name).collect())
    }

    /// Define (or update, by name) a net class with a given track width + clearance
    /// (mm → nanometers) and assign the named nets to it — the idiomatic "wide copper
    /// for power" lever. Merges by name; never erases other classes.
    pub fn set_net_class(
        &mut self,
        name: &str,
        track_width_nm: i64,
        clearance_nm: i64,
        nets: &[&str],
    ) -> Result<(), Error> {
        let nc = NetClass {
            name: name.to_string(),
            r#type: NetClassType::NctExplicit as i32,
            constituents: nets.iter().map(|s| s.to_string()).collect(),
            board: Some(NetClassBoardSettings {
                track_width: Some(Distance { value_nm: track_width_nm }),
                clearance: (clearance_nm > 0).then_some(Distance { value_nm: clearance_nm }),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.call_void(&SetNetClasses {
            net_classes: vec![nc],
            merge_mode: MapMergeMode::MmmMerge as i32,
        })
    }
}

// ── Geometry edit helpers (the interactive tool primitives) ──────────────────

use proto::kiapi::board::types::{BoardLayer, Net};
use proto::kiapi::common::types::{Angle, Vector2};

/// The reference designator of a footprint (e.g. "U1"), or "" if unset.
pub fn footprint_reference(fp: &FootprintInstance) -> String {
    fp.reference_field
        .as_ref()
        .and_then(|f| f.text.as_ref())
        .and_then(|t| t.text.as_ref())
        .map(|x| x.text.clone())
        .unwrap_or_default()
}

impl Kicad {
    /// Full `Net` objects (name + code) for the open board.
    fn net_list(&mut self) -> Result<Vec<Net>, Error> {
        let board = Some(self.board_doc.clone().ok_or(Error::NoBoard)?);
        let resp: NetsResponse = self.call(&GetNets {
            board,
            netclass_filter: vec![],
        })?;
        Ok(resp.nets)
    }

    /// Move a footprint (by reference designator) to `(x,y)` nm, with optional
    /// rotation in degrees. One commit. Errors if no footprint has that reference.
    pub fn move_footprint(
        &mut self,
        reference: &str,
        x_nm: i64,
        y_nm: i64,
        rotation_deg: Option<f64>,
    ) -> Result<(), Error> {
        let mut fp = self
            .footprints()?
            .into_iter()
            .find(|f| footprint_reference(f) == reference)
            .ok_or_else(|| Error::NotFound(format!("footprint {reference}")))?;
        fp.position = Some(Vector2 { x_nm, y_nm });
        if let Some(deg) = rotation_deg {
            fp.orientation = Some(Angle { value_degrees: deg });
        }
        self.commit(&format!("move {reference}"), |k| {
            k.update_items(vec![prost_types::Any::from_msg(&fp)?])
        })
    }

    /// Route a straight track segment on `layer` with `width_nm`, optionally on a
    /// named net (matched by name). One commit.
    pub fn add_track(
        &mut self,
        start_nm: (i64, i64),
        end_nm: (i64, i64),
        width_nm: i64,
        layer: BoardLayer,
        net_name: Option<&str>,
    ) -> Result<(), Error> {
        let net = match net_name {
            Some(name) => self.net_list()?.into_iter().find(|n| n.name == name),
            None => None,
        };
        let track = Track {
            start: Some(Vector2 { x_nm: start_nm.0, y_nm: start_nm.1 }),
            end: Some(Vector2 { x_nm: end_nm.0, y_nm: end_nm.1 }),
            width: Some(Distance { value_nm: width_nm }),
            layer: layer as i32,
            net,
            ..Default::default()
        };
        self.commit("add track", |k| {
            k.create_items(vec![prost_types::Any::from_msg(&track)?])
        })
    }
}
