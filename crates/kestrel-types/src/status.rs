//! Run status and transitions.

use serde::{Deserialize, Serialize};

use crate::ApprovalId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuspendReason {
    Approval(ApprovalId),
    Budget,
    Error(String),
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Queued,
    Provisioning,
    Running,
    Suspended(SuspendReason),
    Completed,
    Failed(String),
    Cancelled,
}

impl RunStatus {
    pub fn can_transition_to(&self, _next: &RunStatus) -> bool {
        todo!()
    }
}
