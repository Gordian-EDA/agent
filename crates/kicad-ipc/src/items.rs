use crate::{Error, Kicad, proto};

use proto::kiapi::board::commands::{
    BoardEnabledLayersResponse, GetBoardEnabledLayers, RefillZones,
};
use proto::kiapi::board::types::{BoardGraphicShape, FootprintInstance, Track, Via, Zone};
use proto::kiapi::common::commands::{
    BeginCommit, BeginCommitResponse, CommitAction, CreateItems, CreateItemsResponse, DeleteItems,
    DeleteItemsResponse, EndCommit, GetItems, GetItemsResponse, ItemDeletionStatus, ItemStatusCode,
    SaveDocument, UpdateItems, UpdateItemsResponse,
};
use proto::kiapi::common::types::{ItemHeader, KiCadObjectType, Kiid};

impl Kicad {
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

    /// All board drawing shapes, including `Edge.Cuts`.
    pub fn board_shapes(&mut self) -> Result<Vec<BoardGraphicShape>, Error> {
        self.get_items(&[KiCadObjectType::KotPcbShape])?
            .into_iter()
            .map(|a| {
                a.to_msg::<BoardGraphicShape>()
                    .map_err(|_| Error::TypeMismatch("BoardGraphicShape"))
            })
            .collect()
    }

    /// Enabled board layers, including the authoritative copper-layer count.
    pub fn enabled_layers(&mut self) -> Result<BoardEnabledLayersResponse, Error> {
        let board = Some(self.board_doc.clone().ok_or(Error::NoBoard)?);
        self.call(&GetBoardEnabledLayers { board })
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
        if resp.status != proto::kiapi::common::types::ItemRequestStatus::IrsOk as i32 {
            return Err(Error::Item {
                code: resp.status,
                message: "update items request failed".to_owned(),
            });
        }
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

fn item_id_from_any(any: &prost_types::Any) -> Option<Kiid> {
    any.to_msg::<Track>()
        .ok()
        .and_then(|track| track.id)
        .or_else(|| any.to_msg::<Via>().ok().and_then(|via| via.id))
        .or_else(|| any.to_msg::<Zone>().ok().and_then(|zone| zone.id))
}

#[cfg(test)]
mod tests {
    use super::item_id_from_any;
    use crate::proto::kiapi::board::types::{Track, Via, Zone};
    use crate::proto::kiapi::common::types::Kiid;

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
}
