//! Geometry helpers shared by the clearance and bounds rules.
//!
//! The point/segment/rect distance kernel lives in `geom`; only board-relative
//! helpers live here.

use crate::ctx::CopperItem;

pub(crate) use geom::EPS;

/// Do two items share at least one owning connection? (A pad owned by the
/// trace's net, the same net's own copper, etc. — never a clearance conflict.)
pub(crate) fn share_owner(x: &CopperItem, y: &CopperItem) -> bool {
    x.owners.iter().any(|o| y.owned_by(o))
}
