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
        /// field existed.
        #[serde(default)]
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
    },
    ApprovalResolved {
        id: ApprovalId,
        approved: bool,
    },
    AgentLog {
        line: String,
    },
    RunFinished {
        summary: Option<String>,
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
