use std::collections::BTreeMap;

use crate::{Error, Kicad, proto};

use proto::kiapi::board::commands::{
    GetNetClassForNets, GetNets, NetClassForNetsResponse, NetsResponse,
};
use proto::kiapi::board::types::Net;
use proto::kiapi::common::commands::{GetNetClasses, NetClassesResponse, SetNetClasses};
use proto::kiapi::common::project::{NetClass, NetClassBoardSettings, NetClassType};
use proto::kiapi::common::types::{Distance, MapMergeMode};

impl Kicad {
    /// Names of all nets on the open board.
    pub fn nets(&mut self) -> Result<Vec<String>, Error> {
        Ok(self.net_list()?.into_iter().map(|n| n.name).collect())
    }

    /// Full `Net` objects (name + code) for the open board.
    pub fn net_list(&mut self) -> Result<Vec<Net>, Error> {
        let board = Some(self.board_doc.clone().ok_or(Error::NoBoard)?);
        let resp: NetsResponse = self.call(&GetNets {
            board,
            netclass_filter: vec![],
        })?;
        Ok(resp.nets)
    }

    /// All explicit project net classes known to KiCAD.
    pub fn net_classes(&mut self) -> Result<Vec<NetClass>, Error> {
        let resp: NetClassesResponse = self.call(&GetNetClasses {})?;
        Ok(resp.net_classes)
    }

    /// Effective net classes for the provided board nets after KiCAD's class merge.
    pub fn net_classes_for_nets(
        &mut self,
        nets: Vec<Net>,
    ) -> Result<BTreeMap<String, NetClass>, Error> {
        let resp: NetClassForNetsResponse = self.call(&GetNetClassForNets { net: nets })?;
        Ok(resp.classes.into_iter().collect())
    }

    /// Define (or update, by name) a net class with a given track width + clearance
    /// (mm -> nanometers) and assign the named nets to it - the idiomatic "wide copper
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
