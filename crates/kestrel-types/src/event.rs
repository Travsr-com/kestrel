//! Run events.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{ApprovalId, MicroUsd, RunId, RunStatus};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCallRecord {
    pub run_id: RunId,
    pub provider: String,
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_write: u64,
    pub cost: Option<MicroUsd>,
    pub latency_ms: u64,
    pub status: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    RunCreated {
        agent: String,
        /// The run that spawned this one. `None` for a root, and for events written before the
        /// field existed. Left out of the JSON when `None`, so a root's event reads as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<RunId>,
    },
    StatusChanged {
        from: RunStatus,
        to: RunStatus,
    },
    SandboxProvisioned {
        driver: String,
        sandbox_id: String,
    },
    AgentMessage {
        text: String,
    },
    ToolCall {
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        name: String,
        ok: bool,
        output: String,
    },
    ModelCall(ModelCallRecord),
    BudgetWarning {
        spent: MicroUsd,
        limit: MicroUsd,
    },
    ApprovalRequested {
        id: ApprovalId,
        action: String,
        /// A question for a person (`ask_human`): `action` holds the question, and the answer comes
        /// back as the approval's text. Left out of the JSON when `false`, so older events decode.
        #[serde(default, skip_serializing_if = "is_false")]
        question: bool,
    },
    ApprovalResolved {
        id: ApprovalId,
        approved: bool,
        /// Text given with the decision: the edited action for "edit and approve", or the
        /// reason for a rejection. Left out of the JSON when `None`, so older events decode.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<String>,
    },
    AgentLog {
        line: String,
    },
    RunFinished {
        summary: Option<String>,
    },
    /// On the parent: it spawned `child`, which got `budget` carved out of the parent's.
    ChildSpawned {
        child: RunId,
        agent: String,
        budget: MicroUsd,
    },
    /// On a spawned child: what its spec asked for that its parent could not pass on. A child
    /// holds at most its parent's capabilities, so these were left out of its token.
    CapabilitiesDropped {
        #[serde(default)]
        tools: Vec<String>,
        #[serde(default)]
        hosts: Vec<String>,
        #[serde(default)]
        secrets: Vec<String>,
        #[serde(default)]
        agents: Vec<String>,
    },
    /// A person sent the run a message (`kestrel message`, the API or the UI). It waits for the
    /// agent to take it with `kestrel.messages`.
    MessageSent {
        text: String,
    },
    /// The agent took messages with `kestrel.messages`: the `seq`s of their `MessageSent` events.
    /// Each message is delivered once.
    MessageDelivered {
        seqs: Vec<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub run_id: RunId,
    pub seq: u64,
    /// Stored and sent as an RFC 3339 string, e.g. `2026-11-19T06:33:10.548Z`.
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    pub kind: EventKind,
}

/// For `skip_serializing_if`: a `false` flag is left out of the JSON.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(flag: &bool) -> bool {
    !*flag
}
