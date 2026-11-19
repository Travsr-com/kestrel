//! Shared data types: agent spec, run, event, budget.

mod event;
mod ids;
mod money;
mod spec;
mod status;

pub use event::{Event, EventKind, ModelCallRecord};
pub use ids::{ApprovalId, RunId};
pub use money::MicroUsd;
pub use spec::{
    AgentSpec, Approvals, Budget, Capabilities, ModelPolicy, Runtime, SandboxSpec, SandboxTier,
    SpawnPolicy, SpecError, Tier, WorkspaceSpec,
};
pub use status::{RunStatus, SuspendReason};
