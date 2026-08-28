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
    pub(crate) fn net_list(&mut self) -> Result<Vec<Net>, Error> {
        let board = Some(self.board_doc.clone().ok_or(Error::NoBoard)?);
        let resp: NetsResponse = self.call(&GetNets {
            board,
            netclass_filter: vec![],
        })?;
        Ok(resp.nets)
    }

    /// All explicit project net classes known to KiCAD.
    pub(crate) fn net_classes(&mut self) -> Result<Vec<NetClass>, Error> {
        let resp: NetClassesResponse = self.call(&GetNetClasses {})?;
        Ok(resp.net_classes)
    }

    /// Effective net classes for the provided board nets after KiCAD's class merge.
    pub(crate) fn net_classes_for_nets(
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
        self.ensure_stable_updates("net class updates")?;
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

    /// Update a net class only when one of the selected board nets has different
    /// effective width or clearance. Returns whether an update was sent.
    pub fn set_net_class_if_changed(
        &mut self,
        name: &str,
        track_width_nm: i64,
        clearance_nm: i64,
        requested_nets: &[&str],
    ) -> Result<bool, Error> {
        self.ensure_stable_updates("net class queries")?;
        let requested: std::collections::BTreeSet<&str> = requested_nets.iter().copied().collect();
        let selected: Vec<_> = self
            .net_list()?
            .into_iter()
            .filter(|net| requested.contains(net.name.as_str()))
            .collect();
        let effective = self.net_classes_for_nets(selected.clone())?;
        let changed = selected.iter().any(|net| {
            let board = effective
                .get(&net.name)
                .and_then(|class| class.board.as_ref());
            let current_width = board
                .and_then(|settings| settings.track_width.as_ref())
                .map(|distance| distance.value_nm);
            let current_clearance = board
                .and_then(|settings| settings.clearance.as_ref())
                .map(|distance| distance.value_nm)
                .unwrap_or(0);
            current_width != Some(track_width_nm) || current_clearance != clearance_nm
        });
        if changed {
            self.set_net_class(name, track_width_nm, clearance_nm, requested_nets)?;
        }
        Ok(changed)
    }
}
