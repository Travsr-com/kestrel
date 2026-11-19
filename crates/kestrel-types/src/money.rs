//! Money as integer millionths of a dollar.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MicroUsd(pub i64);

impl MicroUsd {
    pub const MAX: MicroUsd = MicroUsd(i64::MAX);

    pub fn checked_add(self, _other: i64) -> Option<MicroUsd> {
        todo!()
    }
    pub fn checked_sub(self, _other: i64) -> Option<MicroUsd> {
        todo!()
    }
    /// Never goes below zero.
    pub fn saturating_sub(self, _other: MicroUsd) -> MicroUsd {
        todo!()
    }
}

impl fmt::Display for MicroUsd {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        todo!()
    }
}
