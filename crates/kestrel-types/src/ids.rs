//! Run and approval identifiers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Run identifier (UUID v7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RunId(Uuid);

impl RunId {
    /// New v7 id, monotonic in creation order.
    pub fn new() -> Self {
        todo!()
    }
    pub fn from_uuid(_u: Uuid) -> Self {
        todo!()
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        todo!()
    }
}

impl FromStr for RunId {
    type Err = uuid::Error;
    fn from_str(_s: &str) -> Result<Self, Self::Err> {
        todo!()
    }
}

/// Approval identifier (UUID v7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ApprovalId(Uuid);

impl ApprovalId {
    pub fn new() -> Self {
        todo!()
    }
    pub fn from_uuid(_u: Uuid) -> Self {
        todo!()
    }
}

impl Default for ApprovalId {
    fn default() -> Self {
        Self::new()
    }
}
