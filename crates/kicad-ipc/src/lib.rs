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
pub use session::{Session, SessionManager};

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

use proto::kiapi::board::commands::RefillZones;
use proto::kiapi::board::types::{FootprintInstance, Pad, Track, Via, Zone};
use proto::kiapi::common::commands::{
    BeginCommit, BeginCommitResponse, CommitAction, CreateItems, CreateItemsResponse, DeleteItems,
    DeleteItemsResponse, EndCommit, GetItems, GetItemsResponse, GetOpenDocuments,
    GetOpenDocumentsResponse, ItemDeletionStatus, ItemStatusCode, SaveDocument, UpdateItems,
    UpdateItemsResponse,
};
use proto::kiapi::common::types::{DocumentType, ItemHeader, KiCadObjectType, Kiid};

/// Turn a per-item `ItemStatus` into an error unless it is `ISC_OK`.
fn check_item_status(
    status: &Option<proto::kiapi::common::commands::ItemStatus>,
) -> Result<(), Error> {
    if let Some(s) = status
        && s.code != ItemStatusCode::IscOk as i32
    {
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

    /// Find and cache the open PCB document whose filename matches `board`.
    pub fn open_board_path(&mut self, board: &std::path::Path) -> Result<(), Error> {
        let resp: GetOpenDocumentsResponse = self.call(&GetOpenDocuments {
            r#type: DocumentType::DoctypePcb as i32,
        })?;
        self.board_doc = resp
            .documents
            .into_iter()
            .find(|doc| document_matches_board(doc, board));
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

    fn header_with_mask(&self, paths: &[&str]) -> Result<ItemHeader, Error> {
        Ok(ItemHeader {
            document: Some(self.board_doc.clone().ok_or(Error::NoBoard)?),
            container: None,
            field_mask: Some(prost_types::FieldMask {
                paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            }),
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
            .map(|a| {
                a.to_msg::<Track>()
                    .map_err(|_| Error::TypeMismatch("Track"))
            })
            .collect()
    }

    /// All vias on the board.
    pub fn vias(&mut self) -> Result<Vec<Via>, Error> {
        self.get_items(&[KiCadObjectType::KotPcbVia])?
            .into_iter()
            .map(|a| a.to_msg::<Via>().map_err(|_| Error::TypeMismatch("Via")))
            .collect()
    }

    /// All zones on the board.
    pub fn zones(&mut self) -> Result<Vec<Zone>, Error> {
        self.get_items(&[KiCadObjectType::KotPcbZone])?
            .into_iter()
            .map(|a| a.to_msg::<Zone>().map_err(|_| Error::TypeMismatch("Zone")))
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
        self.update_items_with_header(header, items)
    }

    /// Update existing board items with an explicit field mask.
    pub fn update_items_masked(
        &mut self,
        paths: &[&str],
        items: Vec<prost_types::Any>,
    ) -> Result<(), Error> {
        let header = self.header_with_mask(paths)?;
        self.update_items_with_header(header, items)
    }

    fn update_items_with_header(
        &mut self,
        header: ItemHeader,
        items: Vec<prost_types::Any>,
    ) -> Result<(), Error> {
        let resp: UpdateItemsResponse = self.call(&UpdateItems {
            header: Some(header),
            items,
        })?;
        for r in &resp.updated_items {
            check_item_status(&r.status)?;
        }
        Ok(())
    }

    /// Delete board items by KIID.
    pub fn delete_items(&mut self, item_ids: Vec<Kiid>) -> Result<(), Error> {
        if item_ids.is_empty() {
            return Ok(());
        }
        let header = self.header()?;
        let resp: DeleteItemsResponse = self.call(&DeleteItems {
            header: Some(header),
            item_ids,
        })?;
        for r in &resp.deleted_items {
            if r.status != ItemDeletionStatus::IdsOk as i32 {
                return Err(Error::Item {
                    code: r.status,
                    message: format!(
                        "could not delete item {}",
                        r.id.as_ref()
                            .map(|id| id.value.as_str())
                            .unwrap_or("<unknown>")
                    ),
                });
            }
        }
        Ok(())
    }

    /// Delete packed board items that carry a supported KIID-bearing type.
    pub fn delete_packed_items(&mut self, items: &[prost_types::Any]) -> Result<(), Error> {
        let ids = items.iter().filter_map(item_id_from_any).collect();
        self.delete_items(ids)
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

    /// Refill all zones on the open board.
    pub fn refill_zones(&mut self) -> Result<(), Error> {
        let board = Some(self.board_doc.clone().ok_or(Error::NoBoard)?);
        self.call_void(&RefillZones {
            board,
            zones: Vec::new(),
        })
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
                track_width: Some(Distance {
                    value_nm: track_width_nm,
                }),
                clearance: (clearance_nm > 0).then_some(Distance {
                    value_nm: clearance_nm,
                }),
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

#[derive(Debug, Clone)]
pub struct FootprintMove {
    pub reference: String,
    pub x_nm: i64,
    pub y_nm: i64,
    pub rotation_deg: Option<f64>,
}

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
    pub fn net_list(&mut self) -> Result<Vec<Net>, Error> {
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
        let old = fp.position.clone().unwrap_or_default();
        translate_footprint_pads(&mut fp, x_nm - old.x_nm, y_nm - old.y_nm)?;
        fp.position = Some(Vector2 { x_nm, y_nm });
        if let Some(deg) = rotation_deg {
            fp.orientation = Some(Angle { value_degrees: deg });
        }
        self.commit(&format!("move {reference}"), |k| {
            k.update_items_masked(
                &["position", "orientation"],
                vec![prost_types::Any::from_msg(&fp)?],
            )
        })
    }

    /// Move a set of footprints in one KiCAD undoable commit.
    pub fn move_footprints(&mut self, moves: &[FootprintMove]) -> Result<(), Error> {
        if moves.is_empty() {
            return Ok(());
        }
        let by_ref: std::collections::BTreeMap<&str, &FootprintMove> =
            moves.iter().map(|m| (m.reference.as_str(), m)).collect();
        let mut updates = Vec::new();
        for mut fp in self.footprints()? {
            let reference = footprint_reference(&fp);
            let Some(mv) = by_ref.get(reference.as_str()) else {
                continue;
            };
            let old = fp.position.clone().unwrap_or_default();
            translate_footprint_pads(&mut fp, mv.x_nm - old.x_nm, mv.y_nm - old.y_nm)?;
            fp.position = Some(Vector2 {
                x_nm: mv.x_nm,
                y_nm: mv.y_nm,
            });
            if let Some(deg) = mv.rotation_deg {
                fp.orientation = Some(Angle { value_degrees: deg });
            }
            updates.push(prost_types::Any::from_msg(&fp)?);
        }
        if updates.len() != moves.len() {
            let found: std::collections::BTreeSet<String> = updates
                .iter()
                .filter_map(|any| {
                    any.to_msg::<FootprintInstance>()
                        .ok()
                        .map(|fp| footprint_reference(&fp))
                })
                .collect();
            let missing: Vec<&str> = moves
                .iter()
                .map(|m| m.reference.as_str())
                .filter(|r| !found.contains(*r))
                .collect();
            return Err(Error::NotFound(format!(
                "footprint(s) {}",
                missing.join(", ")
            )));
        }
        self.commit("place board", |k| {
            k.update_items_masked(&["position", "orientation"], updates)
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
            start: Some(Vector2 {
                x_nm: start_nm.0,
                y_nm: start_nm.1,
            }),
            end: Some(Vector2 {
                x_nm: end_nm.0,
                y_nm: end_nm.1,
            }),
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

fn translate_footprint_pads(
    fp: &mut FootprintInstance,
    dx_nm: i64,
    dy_nm: i64,
) -> Result<(), Error> {
    if dx_nm == 0 && dy_nm == 0 {
        return Ok(());
    }
    let Some(definition) = fp.definition.as_mut() else {
        return Ok(());
    };
    for item in &mut definition.items {
        let Ok(mut pad) = item.to_msg::<Pad>() else {
            continue;
        };
        if let Some(position) = pad.position.as_mut() {
            position.x_nm += dx_nm;
            position.y_nm += dy_nm;
        }
        *item = prost_types::Any::from_msg(&pad)?;
    }
    Ok(())
}

fn document_matches_board(
    doc: &proto::kiapi::common::types::DocumentSpecifier,
    board: &std::path::Path,
) -> bool {
    let Some(proto::kiapi::common::types::document_specifier::Identifier::BoardFilename(name)) =
        doc.identifier.as_ref()
    else {
        return false;
    };
    if board.file_name().and_then(|s| s.to_str()) != Some(name.as_str()) {
        return false;
    }
    let Some(project) = &doc.project else {
        return true;
    };
    if project.path.is_empty() {
        return true;
    }
    same_pathish(
        &std::path::PathBuf::from(&project.path),
        board.parent().unwrap_or_else(|| std::path::Path::new("")),
    )
}

fn same_pathish(a: &std::path::Path, b: &std::path::Path) -> bool {
    if let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize()) {
        return a == b;
    }
    a == b
}

fn item_id_from_any(any: &prost_types::Any) -> Option<proto::kiapi::common::types::Kiid> {
    any.to_msg::<proto::kiapi::board::types::Track>()
        .ok()
        .and_then(|track| track.id)
        .or_else(|| {
            any.to_msg::<proto::kiapi::board::types::Via>()
                .ok()
                .and_then(|via| via.id)
        })
        .or_else(|| {
            any.to_msg::<proto::kiapi::board::types::Zone>()
                .ok()
                .and_then(|zone| zone.id)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::kiapi::board::types::{Track, Via, Zone};
    use proto::kiapi::common::types::document_specifier::Identifier;
    use proto::kiapi::common::types::{DocumentSpecifier, DocumentType, Kiid, ProjectSpecifier};

    #[test]
    fn extracts_item_id_from_packed_track() {
        let track = Track {
            id: Some(Kiid {
                value: "11111111-2222-3333-4444-555555555555".to_string(),
            }),
            ..Default::default()
        };
        let any = prost_types::Any::from_msg(&track).unwrap();

        let id = item_id_from_any(&any).expect("track id");

        assert_eq!(id.value, "11111111-2222-3333-4444-555555555555");
    }

    #[test]
    fn extracts_item_id_from_packed_via() {
        let via = Via {
            id: Some(Kiid {
                value: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
            }),
            ..Default::default()
        };
        let any = prost_types::Any::from_msg(&via).unwrap();

        let id = item_id_from_any(&any).expect("via id");

        assert_eq!(id.value, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    }

    #[test]
    fn extracts_item_id_from_packed_zone() {
        let zone = Zone {
            id: Some(Kiid {
                value: "99999999-8888-7777-6666-555555555555".to_string(),
            }),
            ..Default::default()
        };
        let any = prost_types::Any::from_msg(&zone).unwrap();

        let id = item_id_from_any(&any).expect("zone id");

        assert_eq!(id.value, "99999999-8888-7777-6666-555555555555");
    }

    #[test]
    fn document_match_rejects_same_filename_different_project() {
        let doc = DocumentSpecifier {
            r#type: DocumentType::DoctypePcb as i32,
            identifier: Some(Identifier::BoardFilename("design.kicad_pcb".to_string())),
            project: Some(ProjectSpecifier {
                name: "other".to_string(),
                path: "/tmp/other_project".to_string(),
            }),
        };

        assert!(!document_matches_board(
            &doc,
            std::path::Path::new("/tmp/this_project/design.kicad_pcb")
        ));
    }

    #[test]
    fn document_match_accepts_matching_project_and_filename() {
        let doc = DocumentSpecifier {
            r#type: DocumentType::DoctypePcb as i32,
            identifier: Some(Identifier::BoardFilename("design.kicad_pcb".to_string())),
            project: Some(ProjectSpecifier {
                name: "this_project".to_string(),
                path: "/tmp/this_project".to_string(),
            }),
        };

        assert!(document_matches_board(
            &doc,
            std::path::Path::new("/tmp/this_project/design.kicad_pcb")
        ));
    }
}
