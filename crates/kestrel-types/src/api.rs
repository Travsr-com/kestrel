//! JSON bodies of the HTTP and WebSocket API that `kestrel-server` serves to the UI and the CLI.
//!
//! `ui/tests/fixtures/contract.json` holds one sample of each. The Rust tests round-trip it through
//! these types and the UI tests type-check it, so both sides change together.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{ApprovalId, Event, MicroUsd, RunId, RunStatus, SandboxTier};

/// One run, as `GET /api/runs` and `GET /api/runs/{id}` return it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub id: RunId,
    pub agent: String,
    pub status: RunStatus,
    /// The run that spawned this one, `null` for a root.
    #[serde(default)]
    pub parent: Option<RunId>,
    /// The root of this run's tree. A root is its own root.
    pub root: RunId,
    /// 0 for a root, 1 for its children, and so on.
    pub depth: u32,
    /// Spend so far.
    pub spent: MicroUsd,
    /// The budget, `null` when the run has none.
    #[serde(default)]
    pub limit: Option<MicroUsd>,
    /// The sandbox tier the run asked for, `null` before it is known.
    #[serde(default)]
    pub sandbox_tier: Option<SandboxTier>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// One node of `GET /api/runs/{id}/tree`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeNode {
    pub id: RunId,
    #[serde(default)]
    pub parent: Option<RunId>,
    pub agent: String,
    pub status: RunStatus,
    pub spent: MicroUsd,
    #[serde(default)]
    pub limit: Option<MicroUsd>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    /// Undecided, and its run is not waiting on it (the run ended, or it waits on another
    /// approval), so it cannot be decided now.
    Expired,
}

/// One approval, as `GET /api/approvals` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub id: ApprovalId,
    pub run_id: RunId,
    /// What the agent wants to do, in its own words.
    pub action: String,
    pub status: ApprovalStatus,
    #[serde(with = "time::serde::rfc3339")]
    pub requested_at: OffsetDateTime,
    /// A question for a person (`ask_human`), answered with text, rather than an action to
    /// approve. `false` when an older server leaves it out.
    #[serde(default)]
    pub question: bool,
}

/// The subprotocol every event socket speaks. The server echoes it back in the handshake.
///
/// Browsers cannot set headers on a WebSocket, so the client sends its token as a second
/// subprotocol, [`WS_TOKEN_PREFIX`] followed by the token: `["kestrel.v1", "kestrel.token.<token>"]`.
/// A subprotocol may only hold RFC 7230 token characters, so API tokens must be hex or base64url
/// without padding.
pub const WS_PROTOCOL: &str = "kestrel.v1";

/// Prefix of the subprotocol that carries the API token. See [`WS_PROTOCOL`].
pub const WS_TOKEN_PREFIX: &str = "kestrel.token.";

/// The close code for a token refused after a socket opened. The client stops reconnecting and
/// asks for a token instead. (A missing or wrong token at the upgrade is an HTTP 401.)
pub const WS_CLOSE_UNAUTHORIZED: u16 = 4401;

/// One message on an event socket (`/ws/runs/{id}` or `/ws/tree/{root}`).
///
/// `cursor` is what the client sends back as `after=` when it reconnects: the event's `seq` on a
/// run socket, the global event cursor on a tree socket. Cursors only grow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsFrame {
    Event {
        cursor: u64,
        event: Event,
    },
    /// Sent every 15 s so the client can tell a quiet run from a dead connection.
    Heartbeat,
}

/// The body of every error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}
