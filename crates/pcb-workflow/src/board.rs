//! Machinery shared by every board mutator.
//!
//! [`guard`] is the gate all of them pass through: snapshot, edit, re-check,
//! then write or roll back.

pub(crate) mod guard;
