//! Issue #37: the HTTP side of the API, in process with `tower::ServiceExt`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use kestrel_store::ControlCommand;
use kestrel_types::{ApprovalId, EventKind, RunId, RunStatus, SuspendReason};

fn bare(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("host", HOST)
        .body(Body::empty())
        .unwrap()
}

// 1. No token: 401 and no data. A wrong token: 401. The right token: 200.
#[tokio::test]
async fn s1_every_api_route_needs_the_token() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    let app = app(store);
    for uri in [
        "/api/runs".to_string(),
        format!("/api/runs/{id}"),
        format!("/api/runs/{id}/tree"),
        format!("/api/runs/{id}/events"),
        "/api/approvals".to_string(),
    ] {
        let (status, _, body) = send(&app, bare(&uri)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        assert!(
            !String::from_utf8_lossy(&body).contains(&id.to_string()),
            "{uri}"
        );
        let wrong = Request::builder()
            .uri(&uri)
            .header("host", HOST)
            .header("authorization", "Bearer nope")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, wrong).await.0, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(send(&app, get(&uri)).await.0, StatusCode::OK, "{uri}");
    }
    let post = Request::builder()
        .method("POST")
        .uri(format!("/api/runs/{id}/cancel"))
        .header("host", HOST)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, post).await.0, StatusCode::UNAUTHORIZED);
    let (status, body) = json(&app, bare("/api/runs")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "token required");
}

#[tokio::test]
async fn the_token_is_not_taken_from_the_query_on_http_routes() {
    let app = app(store());
    let uri = format!("/api/runs?token={TOKEN}");
    assert_eq!(send(&app, bare(&uri)).await.0, StatusCode::UNAUTHORIZED);
}

// 3. A request with a foreign Host or Origin: 403.
#[tokio::test]
async fn s3_a_foreign_host_or_origin_is_refused() {
    let app = app(store());
    let mut evil = get("/api/runs");
    evil.headers_mut()
        .insert("host", "evil.example".parse().unwrap());
    assert_eq!(send(&app, evil).await.0, StatusCode::FORBIDDEN);
    let mut other_port = get("/api/runs");
    other_port
        .headers_mut()
        .insert("host", "127.0.0.1:9999".parse().unwrap());
    assert_eq!(send(&app, other_port).await.0, StatusCode::FORBIDDEN);
    let mut cross = get("/api/runs");
    cross
        .headers_mut()
        .insert("origin", "http://evil.example".parse().unwrap());
    assert_eq!(send(&app, cross).await.0, StatusCode::FORBIDDEN);
    // A page served from another local port is another site too.
    let mut neighbour = get("/api/runs");
    neighbour
        .headers_mut()
        .insert("origin", "http://127.0.0.1:8080".parse().unwrap());
    assert_eq!(send(&app, neighbour).await.0, StatusCode::FORBIDDEN);
    // Even the UI is refused to a rebound name.
    let mut page = bare("/");
    page.headers_mut()
        .insert("host", "evil.example:7171".parse().unwrap());
    assert_eq!(send(&app, page).await.0, StatusCode::FORBIDDEN);
    let mut no_host = get("/api/runs");
    no_host.headers_mut().remove("host");
    assert_eq!(send(&app, no_host).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn s3_the_bound_address_localhost_and_a_same_origin_page_are_allowed() {
    let app = app(store());
    let mut same = get("/api/runs");
    same.headers_mut()
        .insert("origin", format!("http://{HOST}").parse().unwrap());
    assert_eq!(send(&app, same).await.0, StatusCode::OK);
    let mut local = get("/api/runs");
    local
        .headers_mut()
        .insert("host", "localhost:7171".parse().unwrap());
    local
        .headers_mut()
        .insert("origin", "http://localhost:7171".parse().unwrap());
    assert_eq!(send(&app, local).await.0, StatusCode::OK);
}

#[tokio::test]
async fn no_cors_headers_are_sent() {
    let app = app(store());
    let mut preflight = req("OPTIONS", "/api/runs", None);
    preflight
        .headers_mut()
        .insert("origin", format!("http://{HOST}").parse().unwrap());
    let (_, headers, _) = send(&app, preflight).await;
    assert!(headers.get("access-control-allow-origin").is_none());
}

// 4. Given 3 runs, `status=running` returns only running ones; `agent=` and `root=` combine.
#[tokio::test]
async fn s4_run_filters_combine() {
    let store = store();
    let a = store.create_run(&agent("alpha"), "1").unwrap();
    let b = store.create_run(&agent("beta"), "2").unwrap();
    let c = store.create_run(&agent("alpha"), "3").unwrap();
    run(&store, a);
    run(&store, b);
    let child = store.create_child_run(a, &agent("alpha"), "4").unwrap();
    run(&store, child);
    let app = app(store);
    let ids = |v: &serde_json::Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    let (_, running) = json(&app, get("/api/runs?status=running")).await;
    let mut got = ids(&running);
    got.sort();
    let mut want = vec![a.to_string(), b.to_string(), child.to_string()];
    want.sort();
    assert_eq!(got, want);
    let (_, alpha) = json(&app, get("/api/runs?status=running&agent=alpha")).await;
    assert_eq!(ids(&alpha).len(), 2);
    let (_, tree) = json(&app, get(&format!("/api/runs?agent=alpha&root={a}"))).await;
    let mut got = ids(&tree);
    got.sort();
    let mut want = vec![a.to_string(), child.to_string()];
    want.sort();
    assert_eq!(got, want);
    let (_, queued) = json(&app, get("/api/runs?status=queued")).await;
    assert_eq!(ids(&queued), [c.to_string()]);
    let (status, body) = json(&app, get("/api/runs?status=sleeping")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = json(&app, get("/api/runs?root=not-a-uuid")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn runs_are_newest_first_with_spend_limit_and_tier() {
    let store = store();
    let first = store.create_run(&agent("a"), "1").unwrap();
    let second = store.create_run(&agent("b"), "2").unwrap();
    let app = app(store);
    let (_, runs) = json(&app, get("/api/runs")).await;
    let ids: Vec<&str> = runs
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [second.to_string(), first.to_string()]);
    let (status, one) = json(&app, get(&format!("/api/runs/{first}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["agent"], "a");
    assert_eq!(one["status"], "Queued");
    assert_eq!(one["spent"], 0);
    assert_eq!(one["limit"], 2_000_000);
    assert_eq!(one["sandbox_tier"], "gvisor");
    assert_eq!(one["depth"], 0);
    assert_eq!(one["root"], first.to_string());
    assert_eq!(one["parent"], serde_json::Value::Null);
}

// 5. Given a root with 2 children, `GET /tree` with a child id returns the whole tree.
#[tokio::test]
async fn s5_the_tree_of_a_child_is_its_roots_whole_tree() {
    let store = store();
    let root = store.create_run(&agent("lead"), "x").unwrap();
    let one = store.create_child_run(root, &agent("w"), "1").unwrap();
    let two = store.create_child_run(root, &agent("w"), "2").unwrap();
    let app = app(store);
    let (status, tree) = json(&app, get(&format!("/api/runs/{two}/tree"))).await;
    assert_eq!(status, StatusCode::OK);
    let nodes: Vec<(String, serde_json::Value)> = tree
        .as_array()
        .unwrap()
        .iter()
        .map(|n| (n["id"].as_str().unwrap().to_string(), n["parent"].clone()))
        .collect();
    assert_eq!(
        nodes,
        [
            (root.to_string(), serde_json::Value::Null),
            (one.to_string(), root.to_string().into()),
            (two.to_string(), root.to_string().into()),
        ]
    );
}

// 6. Given 10 events on a run, `events?after=4` returns events 5 to 10.
#[tokio::test]
async fn s6_events_after_a_seq() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    for i in 1..=10 {
        store
            .append(
                id,
                EventKind::AgentLog {
                    line: format!("{i}"),
                },
            )
            .unwrap();
    }
    let app = app(store);
    let (_, events) = json(&app, get(&format!("/api/runs/{id}/events?after=4"))).await;
    let seqs: Vec<u64> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, (5..=10).collect::<Vec<_>>());
    let (_, all) = json(&app, get(&format!("/api/runs/{id}/events"))).await;
    assert_eq!(all.as_array().unwrap().len(), 10);
    let (status, _) = json(&app, get(&format!("/api/runs/{id}/events?after=x"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// 10. `POST cancel` (and pause, resume) reaches the store's control channel; 202. Each is sent
//     while the run's status allows it (#90 scenario 5: unchanged).
#[tokio::test]
async fn s10_run_commands_go_to_the_control_channel() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    run(&store, id);
    let app = app(store.clone());
    for action in ["pause", "resume", "cancel"] {
        if action == "resume" {
            store
                .transition(
                    id,
                    &RunStatus::Running,
                    RunStatus::Suspended(SuspendReason::Manual),
                )
                .unwrap();
        }
        let (status, _, body) =
            send(&app, req("POST", &format!("/api/runs/{id}/{action}"), None)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{action}");
        assert!(body.is_empty(), "{action}");
    }
    let cmds: Vec<ControlCommand> = store
        .take_control()
        .unwrap()
        .into_iter()
        .map(|(_, run, cmd)| {
            assert_eq!(run, id);
            cmd
        })
        .collect();
    assert_eq!(
        cmds,
        [
            ControlCommand::Pause,
            ControlCommand::Resume,
            ControlCommand::Cancel
        ]
    );
}

// Issue #90, scenario 1 over the API: a queued run cannot be paused or resumed (409, with the
// reason); cancel and budget still reach it.
#[tokio::test]
async fn i90_s1_a_queued_run_cannot_be_paused_but_can_be_cancelled() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    let app = app(store.clone());
    for (action, why) in [
        ("pause", "a queued run cannot be paused"),
        ("resume", "a queued run cannot be resumed"),
    ] {
        let (status, body) =
            json(&app, req("POST", &format!("/api/runs/{id}/{action}"), None)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{action}: {body}");
        assert!(
            body["error"].as_str().unwrap().contains(why),
            "{action}: {body}"
        );
    }
    assert!(
        store.take_control().unwrap().is_empty(),
        "nothing reaches the engine"
    );
    let (status, _, _) = send(
        &app,
        req(
            "POST",
            &format!("/api/runs/{id}/budget"),
            Some(r#"{"usd":"3"}"#),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _, _) = send(&app, req("POST", &format!("/api/runs/{id}/cancel"), None)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let cmds: Vec<ControlCommand> = store
        .take_control()
        .unwrap()
        .into_iter()
        .map(|(_, _, cmd)| cmd)
        .collect();
    assert_eq!(
        cmds,
        [
            ControlCommand::SetBudget(kestrel_types::MicroUsd(3_000_000)),
            ControlCommand::Cancel
        ]
    );
}

// Issue #90, scenarios 2 and 3: an ended run answers 409 with the reason as JSON on every run
// command, and nothing is written to the control channel.
#[tokio::test]
async fn i90_s3_an_ended_run_answers_409_on_every_command() {
    let store = store();
    let completed = store.create_run(&agent("a"), "x").unwrap();
    run(&store, completed);
    store
        .transition(completed, &RunStatus::Running, RunStatus::Completed)
        .unwrap();
    let failed = store.create_run(&agent("a"), "x").unwrap();
    run(&store, failed);
    store
        .transition(
            failed,
            &RunStatus::Running,
            RunStatus::Failed("boom".into()),
        )
        .unwrap();
    let cancelled = store.create_run(&agent("a"), "x").unwrap();
    store
        .transition(cancelled, &RunStatus::Queued, RunStatus::Cancelled)
        .unwrap();
    let app = app(store.clone());
    for (id, name) in [
        (completed, "completed"),
        (failed, "failed"),
        (cancelled, "cancelled"),
    ] {
        for (action, body) in [
            ("pause", None),
            ("resume", None),
            ("cancel", None),
            ("budget", Some(r#"{"usd":"3"}"#)),
        ] {
            let (status, value) =
                json(&app, req("POST", &format!("/api/runs/{id}/{action}"), body)).await;
            assert_eq!(status, StatusCode::CONFLICT, "{action} on {name}: {value}");
            let error = value["error"].as_str().unwrap();
            assert!(
                error.contains(&format!("the run has ended ({name})")),
                "{action} on {name}: {error}"
            );
        }
    }
    assert!(
        store.take_control().unwrap().is_empty(),
        "nothing reaches the engine"
    );
}

// PR #108 review: an ended root that kept a live child still takes a cancel (202, the engine
// cancels the live tree below it); other commands are 409, and so is cancel once nothing below
// is live.
#[tokio::test]
async fn i90_cancel_on_an_ended_root_with_a_live_kept_child_is_accepted() {
    let store = store();
    let root = store.create_run(&agent("lead"), "x").unwrap();
    run(&store, root);
    let kid = store.create_child_run(root, &agent("lead"), "kid").unwrap();
    run(&store, kid);
    store
        .transition(root, &RunStatus::Running, RunStatus::Completed)
        .unwrap();
    let app = app(store.clone());

    let (status, _, _) = send(&app, req("POST", &format!("/api/runs/{root}/cancel"), None)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let cmds: Vec<(RunId, ControlCommand)> = store
        .take_control()
        .unwrap()
        .into_iter()
        .map(|(_, run, cmd)| (run, cmd))
        .collect();
    assert_eq!(cmds, [(root, ControlCommand::Cancel)]);
    let (status, body) = json(&app, req("POST", &format!("/api/runs/{root}/pause"), None)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    store
        .transition(kid, &RunStatus::Running, RunStatus::Cancelled)
        .unwrap();
    let (status, body) = json(&app, req("POST", &format!("/api/runs/{root}/cancel"), None)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("the run has ended (completed)"),
        "{body}"
    );
    assert!(store.take_control().unwrap().is_empty());
}

// Issue #90: a bad budget body is still 400 on an ended run; the body is checked first.
#[tokio::test]
async fn i90_a_bad_budget_on_an_ended_run_is_still_400() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    store
        .transition(id, &RunStatus::Queued, RunStatus::Cancelled)
        .unwrap();
    let app = app(store);
    let (status, _) = json(
        &app,
        req(
            "POST",
            &format!("/api/runs/{id}/budget"),
            Some(r#"{"usd":"-1"}"#),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_unknown_run_command_is_404() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    let app = app(store);
    let (status, body) = json(&app, req("POST", &format!("/api/runs/{id}/explode"), None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string());
}

// `POST message` to a queued run whose agent does not take messages: 409 (issue #95).
#[tokio::test]
async fn message_is_409_for_an_agent_without_kestrel_messages() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    let app = app(store);
    let (status, body) = json(
        &app,
        req(
            "POST",
            &format!("/api/runs/{id}/message"),
            Some(r#"{"text":"hi"}"#),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("message"));
}

/// A run waiting on a fresh approval.
fn waiting(store: &kestrel_store::Store) -> (RunId, ApprovalId) {
    let id = store.create_run(&agent("a"), "x").unwrap();
    run(store, id);
    let aid = ApprovalId::new();
    store
        .append(
            id,
            EventKind::ApprovalRequested {
                id: aid,
                action: "git push".into(),
                question: false,
            },
        )
        .unwrap();
    store
        .transition(
            id,
            &RunStatus::Running,
            RunStatus::Suspended(SuspendReason::Approval(aid)),
        )
        .unwrap();
    (id, aid)
}

// 11. `POST approve` with a payload: the command carries it (the engine puts it on the resolved
//     event; see kestrel-core). Reject takes a reason.
#[tokio::test]
async fn s11_approve_and_reject_carry_their_text_to_the_engine() {
    let store = store();
    let (id, aid) = waiting(&store);
    let app = app(store.clone());
    let (status, _, _) = send(
        &app,
        req(
            "POST",
            &format!("/api/approvals/{aid}/approve"),
            Some(r#"{"payload":"git push origin main"}"#),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _, _) = send(
        &app,
        req("POST", &format!("/api/approvals/{aid}/approve"), None),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _, _) = send(
        &app,
        req(
            "POST",
            &format!("/api/approvals/{aid}/reject"),
            Some(r#"{"reason":"no"}"#),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _, _) = send(
        &app,
        req("POST", &format!("/api/approvals/{aid}/reject"), None),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let cmds: Vec<(RunId, ControlCommand)> = store
        .take_control()
        .unwrap()
        .into_iter()
        .map(|(_, run, cmd)| (run, cmd))
        .collect();
    assert_eq!(
        cmds,
        [
            (
                id,
                ControlCommand::ApproveWith {
                    id: aid,
                    payload: "git push origin main".into()
                }
            ),
            (id, ControlCommand::Approve(aid)),
            (
                id,
                ControlCommand::RejectWith {
                    id: aid,
                    reason: "no".into()
                }
            ),
            (id, ControlCommand::Reject(aid)),
        ]
    );
}

// 12. `POST approve` for an unknown approval: 404; for a resolved one: 409.
#[tokio::test]
async fn s12_unknown_approval_is_404_and_a_resolved_one_409() {
    let store = store();
    let (id, aid) = waiting(&store);
    let app = app(store.clone());
    let ghost = ApprovalId::new();
    let (status, _) = json(
        &app,
        req("POST", &format!("/api/approvals/{ghost}/approve"), None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = json(&app, req("POST", "/api/approvals/nope/approve", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    store
        .append(
            id,
            EventKind::ApprovalResolved {
                id: aid,
                approved: true,
                payload: None,
            },
        )
        .unwrap();
    let (status, body) = json(
        &app,
        req("POST", &format!("/api/approvals/{aid}/reject"), None),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].is_string());
    assert!(store.take_control().unwrap().is_empty());
}

#[tokio::test]
async fn an_unknown_approval_action_is_404_before_anything_else() {
    let store = store();
    let (id, aid) = waiting(&store);
    store
        .append(
            id,
            EventKind::ApprovalResolved {
                id: aid,
                approved: true,
                payload: None,
            },
        )
        .unwrap();
    let app = app(store);
    let (status, _) = json(
        &app,
        req("POST", &format!("/api/approvals/{aid}/bogus"), None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_field_that_does_not_match_the_action_is_400() {
    let store = store();
    let (_, aid) = waiting(&store);
    let app = app(store.clone());
    for (action, body) in [
        ("reject", r#"{"payload":"x"}"#),
        ("approve", r#"{"reason":"x"}"#),
        ("approve", r#"{"payload":"x","extra":1}"#),
        ("approve", "not json"),
    ] {
        let (status, _) = json(
            &app,
            req(
                "POST",
                &format!("/api/approvals/{aid}/{action}"),
                Some(body),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{action} {body}");
    }
    assert!(store.take_control().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_socket_paths_are_a_json_404_not_the_ui() {
    let app = app(store());
    let (status, body) = json(&app, get("/ws/nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string());
    assert_eq!(
        send(&app, bare("/ws/nope")).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn approving_for_a_run_no_longer_waiting_is_409() {
    let store = store();
    let (id, aid) = waiting(&store);
    store
        .transition(
            id,
            &RunStatus::Suspended(SuspendReason::Approval(aid)),
            RunStatus::Cancelled,
        )
        .unwrap();
    let app = app(store);
    let (status, _) = json(
        &app,
        req("POST", &format!("/api/approvals/{aid}/approve"), None),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

// An approval whose run ended without deciding it (cancelled, failed) can never be resolved: it
// is listed as expired, not pending, and approving it is 409.
#[tokio::test]
async fn an_approval_whose_run_ended_is_expired_not_pending() {
    let store = store();
    let (id, aid) = waiting(&store);
    store
        .transition(
            id,
            &RunStatus::Suspended(SuspendReason::Approval(aid)),
            RunStatus::Cancelled,
        )
        .unwrap();
    let app = app(store);
    let (_, pending) = json(&app, get("/api/approvals?status=pending")).await;
    assert_eq!(pending, serde_json::json!([]));
    let (_, expired) = json(&app, get("/api/approvals?status=expired")).await;
    assert_eq!(expired[0]["id"], aid.to_string());
    assert_eq!(expired[0]["status"], "expired");
    let (status, _) = json(
        &app,
        req("POST", &format!("/api/approvals/{aid}/approve"), None),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn approvals_are_listed_by_status() {
    let store = store();
    let (_, pending) = waiting(&store);
    let (other, done) = waiting(&store);
    store
        .append(
            other,
            EventKind::ApprovalResolved {
                id: done,
                approved: false,
                payload: None,
            },
        )
        .unwrap();
    let app = app(store);
    let ids = |v: serde_json::Value| -> Vec<(String, String)> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|a| {
                (
                    a["id"].as_str().unwrap().to_string(),
                    a["status"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let (_, p) = json(&app, get("/api/approvals?status=pending")).await;
    assert_eq!(ids(p), [(pending.to_string(), "pending".to_string())]);
    let (_, r) = json(&app, get("/api/approvals?status=rejected")).await;
    assert_eq!(ids(r), [(done.to_string(), "rejected".to_string())]);
    let (_, all) = json(&app, get("/api/approvals")).await;
    assert_eq!(all.as_array().unwrap().len(), 2);
    assert_eq!(all[0]["action"], "git push");
    let (_, e) = json(&app, get("/api/approvals?status=expired")).await;
    assert_eq!(e, serde_json::json!([]));
    let (status, _) = json(&app, get("/api/approvals?status=maybe")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// 13. `POST budget` with a negative or non-decimal value: 400.
#[tokio::test]
async fn s13_a_bad_budget_is_400() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    let app = app(store.clone());
    for body in [
        r#"{"usd":"-1"}"#,
        r#"{"usd":"five"}"#,
        r#"{"usd":5}"#,
        r#"{}"#,
        "not json",
    ] {
        let (status, _) = json(
            &app,
            req("POST", &format!("/api/runs/{id}/budget"), Some(body)),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    assert!(store.take_control().unwrap().is_empty());
    let (status, _, _) = send(
        &app,
        req(
            "POST",
            &format!("/api/runs/{id}/budget"),
            Some(r#"{"usd":"2.50"}"#),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (_, _, cmd) = store.take_control().unwrap().remove(0);
    assert_eq!(
        cmd,
        ControlCommand::SetBudget(kestrel_types::MicroUsd(2_500_000))
    );
}

// 14. Given a run id that does not exist, then 404 on every `{id}` route.
#[tokio::test]
async fn s14_an_unknown_run_is_404_everywhere() {
    let app = app(store());
    let ghost = RunId::new();
    for (method, uri) in [
        ("GET", format!("/api/runs/{ghost}")),
        ("GET", format!("/api/runs/{ghost}/tree")),
        ("GET", format!("/api/runs/{ghost}/events")),
        ("POST", format!("/api/runs/{ghost}/pause")),
        ("POST", format!("/api/runs/{ghost}/resume")),
        ("POST", format!("/api/runs/{ghost}/cancel")),
        ("POST", format!("/api/runs/{ghost}/budget")),
        ("POST", format!("/api/runs/{ghost}/message")),
        ("GET", "/api/runs/not-a-uuid".to_string()),
    ] {
        let body = (method == "POST").then_some(r#"{"usd":"1","text":"x"}"#);
        let (status, value) = json(&app, req(method, &uri, body)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        assert!(value["error"].is_string(), "{method} {uri}");
    }
}

// 15. A UI route gets `index.html`; an unknown API path gets a 404 JSON, not the UI.
#[tokio::test]
async fn s15_ui_routes_get_the_app_and_api_paths_do_not() {
    let app = app(store());
    for uri in ["/", "/runs/abc", "/fleet"] {
        let (status, headers, body) = send(&app, bare(uri)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body, INDEX.as_bytes(), "{uri}");
        assert!(
            headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
    }
    let (status, headers, body) = send(&app, bare("/assets/app.js")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"console.log(1)");
    assert_eq!(headers["content-type"], "text/javascript");
    let (status, _, _) = send(&app, bare("/assets/missing.js")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = json(&app, get("/api/nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string());
    let (status, _) = json(&app, get("/api")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// #35 scenario 2: without a UI build the server serves a placeholder page that says how to build.
#[tokio::test]
async fn without_a_ui_build_a_placeholder_page_says_how_to_build_it() {
    let app = app_with(store(), MemUi::empty());
    for uri in ["/", "/runs/abc"] {
        let (status, headers, body) = send(&app, bare(uri)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        let page = String::from_utf8_lossy(&body).to_string();
        assert!(page.contains("npm run build"), "{page}");
    }
}

#[tokio::test]
async fn ui_pages_are_not_cached_and_not_framed() {
    let app = app(store());
    let (_, headers, _) = send(&app, bare("/")).await;
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["x-frame-options"], "DENY");
    assert_eq!(headers["referrer-policy"], "no-referrer");
}

// The real embedded build: whatever `ui/dist` held at compile time, `/` is the app or the
// placeholder, and a file that is not in the build is not found.
#[tokio::test]
async fn the_embedded_ui_serves_its_build_or_the_placeholder() {
    use kestrel_server::{Assets, EmbeddedUi, PLACEHOLDER};
    assert_eq!(EmbeddedUi.get("no/such/file.txt"), None);
    let app = kestrel_server::router(
        store(),
        kestrel_server::Config::new(TOKEN, bind()),
        std::sync::Arc::new(EmbeddedUi),
        tokio_util::sync::CancellationToken::new(),
    );
    let (status, _, body) = send(&app, bare("/")).await;
    assert_eq!(status, StatusCode::OK);
    let page = String::from_utf8_lossy(&body);
    let built = EmbeddedUi.get("index.html").is_some();
    assert_eq!(page == PLACEHOLDER, !built, "{page}");
}

// Issue #97, the bare roots of the API and the sockets: `/api`, `/api/`, `/ws` and `/ws/`.
const ROOTS: [&str; 4] = ["/api", "/api/", "/ws", "/ws/"];

/// The status and the JSON error of a response that must not be the UI page.
async fn json_error(app: &axum::Router, request: Request<Body>) -> (StatusCode, String) {
    let target = format!("{} {}", request.method(), request.uri());
    let (status, headers, body) = send(app, request).await;
    let page = String::from_utf8_lossy(&body).to_string();
    assert_ne!(page, INDEX, "{target} served the UI page");
    assert_eq!(headers["content-type"], "application/json", "{target}");
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    (
        status,
        value["error"].as_str().unwrap_or_default().to_string(),
    )
}

// #97 scenario 1: with the token, each root is the JSON 404 every unknown API path gets, for any
// method. Without the token it is the JSON 401 every other `/api` and `/ws` path gets.
#[tokio::test]
async fn issue97_s1_api_and_ws_roots_are_a_json_404_not_the_ui() {
    let app = app(store());
    for uri in ROOTS {
        for method in ["GET", "POST", "DELETE"] {
            let (status, error) = json_error(&app, req(method, uri, None)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
            assert_eq!(error, "no such API route", "{method} {uri}");
        }
        let (status, error) = json_error(&app, bare(uri)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(error, "token required", "{uri}");
    }
    // The same answers as for a deeper unknown path.
    assert_eq!(
        json_error(&app, get("/api/nope")).await,
        json_error(&app, get("/api/")).await
    );
    assert_eq!(
        json_error(&app, get("/ws/nope")).await,
        json_error(&app, get("/ws")).await
    );
}

// #97 scenario 1: the socket roots take the token the way every socket path does, so a browser
// that sends it as `?token=` gets the 404 too. The API roots do not take it from the query.
#[tokio::test]
async fn issue97_s1_socket_roots_take_the_query_token_and_api_roots_do_not() {
    let app = app(store());
    for uri in ["/ws", "/ws/"] {
        let (status, _) = json_error(&app, bare(&format!("{uri}?token={TOKEN}"))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    for uri in ["/api", "/api/"] {
        let (status, _) = json_error(&app, bare(&format!("{uri}?token={TOKEN}"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
}

// #97 scenario 1: the Host and Origin rules still come first on the roots.
#[tokio::test]
async fn issue97_s1_roots_refuse_a_foreign_host_or_origin() {
    let app = app(store());
    for uri in ROOTS {
        let mut evil = get(uri);
        evil.headers_mut()
            .insert("host", "evil.example:7171".parse().unwrap());
        assert_eq!(send(&app, evil).await.0, StatusCode::FORBIDDEN, "{uri}");
        let mut cross = get(uri);
        cross
            .headers_mut()
            .insert("origin", "http://evil.example".parse().unwrap());
        assert_eq!(send(&app, cross).await.0, StatusCode::FORBIDDEN, "{uri}");
    }
}

// #97 scenario 2: UI routes still get the UI, including ones that only start with `api` or `ws`.
#[tokio::test]
async fn issue97_s2_ui_routes_still_get_the_ui() {
    let app = app(store());
    for uri in [
        "/",
        "/fleet",
        "/runs/x",
        "/apis",
        "/wsx",
        "/fleet/api",
        "/runs/ws",
    ] {
        let (status, _, body) = send(&app, bare(uri)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body, INDEX.as_bytes(), "{uri}");
    }
}

// Issue #95: messages to a running agent.

/// An agent that takes messages (`kestrel.messages`), named `name`.
fn listener(name: &str) -> kestrel_types::AgentSpec {
    kestrel_types::AgentSpec::from_yaml(&format!(
        "name: {name}\nruntime: claude-code\nrole: r\n\
         capabilities: {{ mcp: [kestrel.messages] }}\n\
         sandbox: {{ tier: gvisor, cpu: 1, memory: 1Gi, workspace: empty-dir }}\n\
         budget: {{ usd: 2.00, wall_clock: 10m }}\n"
    ))
    .unwrap()
}

fn post_message(id: RunId, body: &str) -> Request<Body> {
    req("POST", &format!("/api/runs/{id}/message"), Some(body))
}

fn sent_texts(store: &kestrel_store::Store, id: RunId) -> Vec<String> {
    store
        .events_since(id, 0)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::MessageSent { text } => Some(text),
            _ => None,
        })
        .collect()
}

// Scenario 1: 202, and an event records the message. Nothing goes through the control channel:
// the agent takes it from its inbox.
#[tokio::test]
async fn i95_s1_a_message_to_a_running_run_is_accepted_and_recorded() {
    let store = store();
    let id = store.create_run(&listener("l"), "x").unwrap();
    run(&store, id);
    let app = app(store.clone());
    let (status, _, body) = send(&app, post_message(id, r#"{"text":"also fix the docs"}"#)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body.is_empty());
    assert_eq!(sent_texts(&store, id), ["also fix the docs"]);
    assert!(store.take_control().unwrap().is_empty());
    // A second one is a second message.
    let (status, _, _) = send(&app, post_message(id, r#"{"text":"and the changelog"}"#)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        sent_texts(&store, id),
        ["also fix the docs", "and the changelog"]
    );
}

// Scenario 2: an agent that cannot take messages is 409, and says why.
#[tokio::test]
async fn i95_s2_a_run_whose_agent_cannot_take_messages_is_409_with_why() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    run(&store, id);
    let app = app(store.clone());
    let (status, body) = json(&app, post_message(id, r#"{"text":"hi"}"#)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(error.contains(&format!("run {id}")), "{error}");
    assert!(error.contains("cannot take messages"), "{error}");
    assert!(error.contains("kestrel.messages"), "{error}");
    assert!(sent_texts(&store, id).is_empty());
}

// Scenario 3: a run that is not running is refused with the reason.
#[tokio::test]
async fn i95_s3_a_run_that_is_not_running_is_409_with_the_reason() {
    let store = store();
    let queued = store.create_run(&listener("l"), "x").unwrap();
    let paused = store.create_run(&listener("l"), "x").unwrap();
    run(&store, paused);
    store
        .transition(
            paused,
            &RunStatus::Running,
            RunStatus::Suspended(SuspendReason::Manual),
        )
        .unwrap();
    let done = store.create_run(&listener("l"), "x").unwrap();
    run(&store, done);
    store
        .transition(done, &RunStatus::Running, RunStatus::Completed)
        .unwrap();
    let app = app(store.clone());
    for (id, says) in [
        (queued, "a queued run cannot take a message, only a running one"),
        (paused, "a suspended run cannot take a message, only a running one"),
        (done, "the run has ended (completed), so it cannot take a message"),
    ] {
        let (status, body) = json(&app, post_message(id, r#"{"text":"hi"}"#)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let error = body["error"].as_str().unwrap();
        assert!(error.contains(says), "{error}");
        assert!(sent_texts(&store, id).is_empty());
    }
}

// Text only, at most 16 KiB: a bad body is 400 and a long one 413, checked before the status.
#[tokio::test]
async fn i95_a_message_body_is_checked_first() {
    let store = store();
    let id = store.create_run(&listener("l"), "x").unwrap();
    run(&store, id);
    let ended = store.create_run(&listener("l"), "x").unwrap();
    store
        .transition(ended, &RunStatus::Queued, RunStatus::Cancelled)
        .unwrap();
    let app = app(store.clone());
    let long = format!(r#"{{"text":"{}"}}"#, "x".repeat(16 * 1024 + 1));
    for (body, want) in [
        ("", StatusCode::BAD_REQUEST),
        ("not json", StatusCode::BAD_REQUEST),
        ("{}", StatusCode::BAD_REQUEST),
        (r#"{"text":42}"#, StatusCode::BAD_REQUEST),
        (r#"{"text":"   "}"#, StatusCode::BAD_REQUEST),
        (r#"{"text":"\u001b[2J"}"#, StatusCode::BAD_REQUEST),
        (long.as_str(), StatusCode::PAYLOAD_TOO_LARGE),
    ] {
        for target in [id, ended] {
            let (status, value) = json(&app, post_message(target, body)).await;
            assert_eq!(status, want, "{body:.40}: {value}");
            assert!(value["error"].is_string(), "{body:.40}");
        }
    }
    let exactly = format!(r#"{{"text":"{}"}}"#, "x".repeat(16 * 1024));
    let (status, _, _) = send(&app, post_message(id, &exactly)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(sent_texts(&store, id).len(), 1);
}

// The UI shows the Message box only for a run whose agent takes messages.
#[tokio::test]
async fn i95_a_run_says_whether_it_takes_messages() {
    let store = store();
    let yes = store.create_run(&listener("l"), "x").unwrap();
    let no = store.create_run(&agent("a"), "x").unwrap();
    let app = app(store);
    let (_, one) = json(&app, get(&format!("/api/runs/{yes}"))).await;
    assert_eq!(one["takes_messages"], true);
    let (_, one) = json(&app, get(&format!("/api/runs/{no}"))).await;
    assert_eq!(one["takes_messages"], false);
    let (_, list) = json(&app, get("/api/runs")).await;
    let flags: Vec<(String, bool)> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_string(),
                r["takes_messages"].as_bool().unwrap(),
            )
        })
        .collect();
    assert!(flags.contains(&(yes.to_string(), true)), "{flags:?}");
    assert!(flags.contains(&(no.to_string(), false)), "{flags:?}");
}
