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
    /// True once the run can no longer change state.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed(_) | Self::Cancelled)
    }

    /// The lower-case name, without a reason or message: `queued`, `suspended`, `failed`, ...
    pub fn name(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Provisioning => "provisioning",
            Self::Running => "running",
            Self::Suspended(_) => "suspended",
            Self::Completed => "completed",
            Self::Failed(_) => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Lifecycle from `docs/architecture.md` section 3.2:
    /// `Queued -> Provisioning -> Running <-> Suspended`, `Running -> Completed`,
    /// any non-terminal state `-> Failed | Cancelled`. Terminal states have no exits.
    /// `Provisioning -> Suspended(Error)` is how a restart suspends a run it cut off before its
    /// agent started (#92).
    pub fn can_transition_to(&self, next: &RunStatus) -> bool {
        use RunStatus::*;
        if self.is_terminal() {
            return false;
        }
        matches!(
            (self, next),
            (_, Failed(_) | Cancelled)
                | (Queued, Provisioning)
                | (Provisioning, Running | Suspended(SuspendReason::Error(_)))
                | (Running, Suspended(_) | Completed)
                | (Suspended(_), Running)
        )
    }
}
