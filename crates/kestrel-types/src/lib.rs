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

/// The API key value agents get inside a sandbox. It is not a secret: the gateway holds the real
/// provider keys and the run is identified by the token in its gateway URL. Adapters set it and
/// the sandbox secret check lets exactly this value through.
pub const GATEWAY_KEY_PLACEHOLDER: &str = "kestrel-gateway-placeholder";
