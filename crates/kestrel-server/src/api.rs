//! The JSON routes under `/api`. Shapes are the `kestrel-types` API types.

use std::collections::HashMap;
use std::str::FromStr;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use kestrel_store::{ApprovalRecord, ControlCommand, RunRow, Store, StoreError};
use kestrel_types::{
    ApiError, Approval, ApprovalId, ApprovalStatus, MicroUsd, RunId, RunStatus, RunSummary,
    SuspendReason, TreeNode,
};
use serde::Deserialize;

use crate::AppState;

type Params = Query<HashMap<String, String>>;

/// A JSON error body with `status`.
pub(crate) fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ApiError {
            error: message.into(),
        }),
    )
        .into_response()
}

fn internal(e: StoreError) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn accepted() -> Response {
    StatusCode::ACCEPTED.into_response()
}

/// An early answer, boxed because a `Response` is large.
pub(crate) type Reject = Box<Response>;

/// The run `id` names, or the 404 to answer with. A malformed id is a 404 too: no run has it.
pub(crate) fn find_run(store: &Store, id: &str) -> Result<RunRow, Reject> {
    let missing = || Box::new(error(StatusCode::NOT_FOUND, format!("no run {id}")));
    let id = RunId::from_str(id).map_err(|_| missing())?;
    store
        .get_run(id)
        .map_err(|e| Box::new(internal(e)))?
        .ok_or_else(missing)
}

/// The lower-case name `?status=` filters on.
fn status_name(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Queued => "queued",
        RunStatus::Provisioning => "provisioning",
        RunStatus::Running => "running",
        RunStatus::Suspended(_) => "suspended",
        RunStatus::Completed => "completed",
        RunStatus::Failed(_) => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

const STATUS_NAMES: [&str; 7] = [
    "queued",
    "provisioning",
    "running",
    "suspended",
    "completed",
    "failed",
    "cancelled",
];

fn summary(store: &Store, row: RunRow) -> Result<RunSummary, StoreError> {
    let spec = store.run_spec(row.id)?;
    Ok(RunSummary {
        id: row.id,
        spent: store.spent(row.id)?,
        limit: spec.as_ref().map(|s| s.budget.limit),
        sandbox_tier: spec.map(|s| s.sandbox.tier),
        agent: row.agent,
        status: row.status,
        parent: row.parent,
        root: row.root,
        depth: row.depth,
        created_at: row.created_at,
    })
}

fn node(store: &Store, row: RunRow) -> Result<TreeNode, StoreError> {
    let limit = store.run_spec(row.id)?.map(|s| s.budget.limit);
    Ok(TreeNode {
        id: row.id,
        spent: store.spent(row.id)?,
        limit,
        parent: row.parent,
        agent: row.agent,
        status: row.status,
    })
}

/// `GET /api/runs?status=&agent=&root=`: newest first. Filters combine.
pub(crate) async fn list_runs(State(s): State<AppState>, Query(q): Params) -> Response {
    let status = q.get("status").map(String::as_str);
    if let Some(name) = status
        && !STATUS_NAMES.contains(&name)
    {
        return error(
            StatusCode::BAD_REQUEST,
            format!(
                "unknown status `{name}` (one of {})",
                STATUS_NAMES.join(", ")
            ),
        );
    }
    let root = match q.get("root").map(|r| RunId::from_str(r)).transpose() {
        Ok(root) => root,
        Err(_) => return error(StatusCode::BAD_REQUEST, "root must be a run id"),
    };
    let rows = match s.store.list_runs(Default::default()) {
        Ok(rows) => rows,
        Err(e) => return internal(e),
    };
    let mut rows: Vec<RunRow> = rows
        .into_iter()
        .filter(|r| status.is_none_or(|n| status_name(&r.status) == n))
        .filter(|r| q.get("agent").is_none_or(|a| r.agent == *a))
        .filter(|r| root.is_none_or(|root| r.root == root))
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.created_at));
    match rows
        .into_iter()
        .map(|r| summary(&s.store, r))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(runs) => Json(runs).into_response(),
        Err(e) => internal(e),
    }
}

/// `GET /api/runs/{id}`.
pub(crate) async fn get_run(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    let row = match find_run(&s.store, &id) {
        Ok(row) => row,
        Err(res) => return *res,
    };
    match summary(&s.store, row) {
        Ok(run) => Json(run).into_response(),
        Err(e) => internal(e),
    }
}

/// `GET /api/runs/{id}/tree`: the whole tree of `id`'s root, each parent before its children.
pub(crate) async fn tree(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    let row = match find_run(&s.store, &id) {
        Ok(row) => row,
        Err(res) => return *res,
    };
    let rows = (|| -> Result<Vec<RunRow>, StoreError> {
        let mut rows = match s.store.get_run(row.root)? {
            Some(root) => vec![root],
            None => Vec::new(),
        };
        rows.extend(s.store.subtree(row.root)?);
        Ok(rows)
    })();
    match rows.and_then(|rows| {
        rows.into_iter()
            .map(|r| node(&s.store, r))
            .collect::<Result<Vec<_>, _>>()
    }) {
        Ok(nodes) => Json(nodes).into_response(),
        Err(e) => internal(e),
    }
}

/// `GET /api/runs/{id}/events?after=<seq>`.
pub(crate) async fn events(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Params,
) -> Response {
    let row = match find_run(&s.store, &id) {
        Ok(row) => row,
        Err(res) => return *res,
    };
    let after = match after(&q) {
        Ok(after) => after,
        Err(res) => return *res,
    };
    match s.store.events_since(row.id, after) {
        Ok(events) => Json(events).into_response(),
        Err(e) => internal(e),
    }
}

/// `?after=`, 0 when absent.
pub(crate) fn after(q: &HashMap<String, String>) -> Result<u64, Reject> {
    q.get("after")
        .map_or(Ok(0), |a| a.parse::<u64>())
        .map_err(|_| {
            Box::new(error(
                StatusCode::BAD_REQUEST,
                "after must be a whole number",
            ))
        })
}

#[derive(Deserialize)]
struct BudgetBody {
    usd: String,
}

/// `POST /api/runs/{id}/{pause|resume|cancel|budget|message}`: 202 once the command is in the
/// store's control channel.
pub(crate) async fn command(
    State(s): State<AppState>,
    Path((id, action)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let row = match find_run(&s.store, &id) {
        Ok(row) => row,
        Err(res) => return *res,
    };
    let cmd = match action.as_str() {
        "pause" => ControlCommand::Pause,
        "resume" => ControlCommand::Resume,
        "cancel" => ControlCommand::Cancel,
        "budget" => {
            let usd = serde_json::from_slice::<BudgetBody>(&body)
                .ok()
                .and_then(|b| MicroUsd::parse_decimal(&b.usd));
            match usd {
                Some(limit) => ControlCommand::SetBudget(limit),
                None => {
                    return error(
                        StatusCode::BAD_REQUEST,
                        r#"the body must be {"usd": "<amount>"} with a non-negative amount like "2.50""#,
                    );
                }
            }
        }
        "message" => {
            return error(
                StatusCode::CONFLICT,
                "this run's runtime does not take injected messages",
            );
        }
        other => return error(StatusCode::NOT_FOUND, format!("no command `{other}`")),
    };
    match s.store.push_control(row.id, cmd) {
        Ok(_) => accepted(),
        Err(e) => internal(e),
    }
}

/// An undecided approval is pending only while its run waits on it; once the run has moved on
/// (cancelled, failed), it can never be decided and is expired.
fn approval_status(store: &Store, record: &ApprovalRecord) -> Result<ApprovalStatus, StoreError> {
    Ok(match record.resolved {
        Some(true) => ApprovalStatus::Approved,
        Some(false) => ApprovalStatus::Rejected,
        None => {
            let waiting = RunStatus::Suspended(SuspendReason::Approval(record.id));
            let run = store.get_run(record.run_id)?;
            if run.is_some_and(|r| r.status == waiting) {
                ApprovalStatus::Pending
            } else {
                ApprovalStatus::Expired
            }
        }
    })
}

/// `GET /api/approvals?status=pending|approved|rejected|expired`, in request order.
pub(crate) async fn approvals(State(s): State<AppState>, Query(q): Params) -> Response {
    let wanted = match q.get("status").map(String::as_str) {
        None => None,
        Some("pending") => Some(ApprovalStatus::Pending),
        Some("approved") => Some(ApprovalStatus::Approved),
        Some("rejected") => Some(ApprovalStatus::Rejected),
        Some("expired") => Some(ApprovalStatus::Expired),
        Some(other) => {
            return error(
                StatusCode::BAD_REQUEST,
                format!("unknown status `{other}` (one of pending, approved, rejected, expired)"),
            );
        }
    };
    let records = match s.store.approvals() {
        Ok(records) => records,
        Err(e) => return internal(e),
    };
    let mut list = Vec::new();
    for r in records {
        let status = match approval_status(&s.store, &r) {
            Ok(status) => status,
            Err(e) => return internal(e),
        };
        if wanted.is_none_or(|w| status == w) {
            list.push(Approval {
                status,
                id: r.id,
                run_id: r.run_id,
                action: r.action,
                requested_at: r.requested_at,
            });
        }
    }
    Json(list).into_response()
}

#[derive(Deserialize, Default)]
struct ResolveBody {
    payload: Option<String>,
    reason: Option<String>,
}

/// `POST /api/approvals/{id}/{approve|reject}`, optionally with `{"payload": ...}` (approve) or
/// `{"reason": ...}` (reject). 404 for an unknown approval, 409 when it is resolved already or its
/// run no longer waits on it.
pub(crate) async fn resolve(
    State(s): State<AppState>,
    Path((id, action)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let missing = || error(StatusCode::NOT_FOUND, format!("no approval {id}"));
    let Ok(aid) = ApprovalId::from_str(&id) else {
        return missing();
    };
    let record = match s.store.approval(aid) {
        Ok(Some(record)) => record,
        Ok(None) => return missing(),
        Err(e) => return internal(e),
    };
    if record.resolved.is_some() {
        return error(
            StatusCode::CONFLICT,
            format!("approval {aid} is resolved already"),
        );
    }
    let waiting = RunStatus::Suspended(SuspendReason::Approval(aid));
    match s.store.get_run(record.run_id) {
        Ok(Some(run)) if run.status == waiting => {}
        Ok(_) => {
            return error(
                StatusCode::CONFLICT,
                format!("run {} is not waiting on approval {aid}", record.run_id),
            );
        }
        Err(e) => return internal(e),
    }
    let text = if body.is_empty() {
        ResolveBody::default()
    } else {
        match serde_json::from_slice::<ResolveBody>(&body) {
            Ok(b) => b,
            Err(e) => return error(StatusCode::BAD_REQUEST, format!("bad body: {e}")),
        }
    };
    let cmd = match (action.as_str(), text) {
        (
            "approve",
            ResolveBody {
                payload: Some(payload),
                ..
            },
        ) => ControlCommand::ApproveWith { id: aid, payload },
        ("approve", _) => ControlCommand::Approve(aid),
        (
            "reject",
            ResolveBody {
                reason: Some(reason),
                ..
            },
        ) => ControlCommand::RejectWith { id: aid, reason },
        ("reject", _) => ControlCommand::Reject(aid),
        (other, _) => return error(StatusCode::NOT_FOUND, format!("no command `{other}`")),
    };
    match s.store.push_control(record.run_id, cmd) {
        Ok(_) => accepted(),
        Err(e) => internal(e),
    }
}

/// Any other `/api` path: a JSON 404, never the UI.
pub(crate) async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "no such API route")
}
