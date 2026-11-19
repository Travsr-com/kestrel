//! Scenario 7: Event JSON round trip (property test).

use kestrel_types::{
    ApprovalId, Event, EventKind, MicroUsd, ModelCallRecord, RunId, RunStatus, SuspendReason,
};
use proptest::prelude::*;
use time::OffsetDateTime;
use uuid::Uuid;

fn run_id() -> impl Strategy<Value = RunId> {
    any::<u128>().prop_map(|n| RunId::from_uuid(Uuid::from_u128(n)))
}
fn approval_id() -> impl Strategy<Value = ApprovalId> {
    any::<u128>().prop_map(|n| ApprovalId::from_uuid(Uuid::from_u128(n)))
}
fn money() -> impl Strategy<Value = MicroUsd> {
    any::<i64>().prop_map(MicroUsd)
}
fn text() -> impl Strategy<Value = String> {
    ".{0,24}"
}
fn status() -> impl Strategy<Value = RunStatus> {
    prop_oneof![
        Just(RunStatus::Queued),
        Just(RunStatus::Provisioning),
        Just(RunStatus::Running),
        approval_id().prop_map(|a| RunStatus::Suspended(SuspendReason::Approval(a))),
        Just(RunStatus::Suspended(SuspendReason::Budget)),
        text().prop_map(|e| RunStatus::Suspended(SuspendReason::Error(e))),
        Just(RunStatus::Suspended(SuspendReason::Manual)),
        Just(RunStatus::Completed),
        text().prop_map(RunStatus::Failed),
        Just(RunStatus::Cancelled),
    ]
}
fn json_value() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::from),
        any::<i64>().prop_map(serde_json::Value::from),
        text().prop_map(serde_json::Value::from),
    ];
    leaf.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::from),
            prop::collection::hash_map("[a-z]{1,6}", inner, 0..4)
                .prop_map(|m| serde_json::Value::Object(m.into_iter().collect())),
        ]
    })
}
fn model_call() -> impl Strategy<Value = ModelCallRecord> {
    (
        run_id(),
        text(),
        text(),
        (any::<u64>(), any::<u64>(), any::<u64>(), any::<u64>()),
        prop::option::of(money()),
        any::<u64>(),
        any::<u16>(),
    )
        .prop_map(
            |(run_id, provider, model, t, cost, latency_ms, status)| ModelCallRecord {
                run_id,
                provider,
                model,
                tokens_in: t.0,
                tokens_out: t.1,
                tokens_cache_read: t.2,
                tokens_cache_write: t.3,
                cost,
                latency_ms,
                status,
            },
        )
}

/// Returns (strategy for a kind, expected snake_case tag) for all 12 variants.
fn kind_with_tag() -> impl Strategy<Value = (EventKind, &'static str)> {
    prop_oneof![
        text().prop_map(|agent| (EventKind::RunCreated { agent }, "run_created")),
        (status(), status())
            .prop_map(|(from, to)| (EventKind::StatusChanged { from, to }, "status_changed")),
        (text(), text()).prop_map(|(driver, sandbox_id)| (
            EventKind::SandboxProvisioned { driver, sandbox_id },
            "sandbox_provisioned"
        )),
        text().prop_map(|text| (EventKind::AgentMessage { text }, "agent_message")),
        (text(), json_value())
            .prop_map(|(name, input)| (EventKind::ToolCall { name, input }, "tool_call")),
        (text(), any::<bool>(), text()).prop_map(|(name, ok, output)| (
            EventKind::ToolResult { name, ok, output },
            "tool_result"
        )),
        model_call().prop_map(|m| (EventKind::ModelCall(m), "model_call")),
        (money(), money()).prop_map(|(spent, limit)| (
            EventKind::BudgetWarning { spent, limit },
            "budget_warning"
        )),
        (approval_id(), text()).prop_map(|(id, action)| (
            EventKind::ApprovalRequested { id, action },
            "approval_requested"
        )),
        (approval_id(), any::<bool>()).prop_map(|(id, approved)| (
            EventKind::ApprovalResolved { id, approved },
            "approval_resolved"
        )),
        text().prop_map(|line| (EventKind::AgentLog { line }, "agent_log")),
        prop::option::of(text())
            .prop_map(|summary| (EventKind::RunFinished { summary }, "run_finished")),
    ]
}

fn event() -> impl Strategy<Value = (Event, &'static str)> {
    (
        run_id(),
        any::<u64>(),
        0i64..4_000_000_000i64,
        0u32..1_000_000u32,
        kind_with_tag(),
    )
        .prop_map(|(run_id, seq, secs, micros, (kind, tag))| {
            let ts = OffsetDateTime::from_unix_timestamp(secs)
                .map(|t| t + time::Duration::microseconds(i64::from(micros)))
                .unwrap_or(OffsetDateTime::UNIX_EPOCH);
            (
                Event {
                    run_id,
                    seq,
                    ts,
                    kind,
                },
                tag,
            )
        })
}

proptest! {
    #[test]
    fn s7_event_json_round_trip((ev, _tag) in event()) {
        let json = serde_json::to_string(&ev).expect("serialize");
        let back: Event = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(back, ev);
    }

    #[test]
    fn s7_kind_is_snake_case_string_tag((ev, tag) in event()) {
        let v = serde_json::to_value(&ev).expect("serialize");
        let kind = v["kind"]["kind"].as_str().expect("kind.kind is a string");
        prop_assert_eq!(kind, tag);
        prop_assert!(!kind.is_empty());
        prop_assert!(kind.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        prop_assert!(!kind.starts_with('_') && !kind.ends_with('_'));
    }
}

#[test]
fn s7_model_call_without_price_keeps_cost_none() {
    let rec = ModelCallRecord {
        run_id: RunId::new(),
        provider: "p".into(),
        model: "unknown-model".into(),
        tokens_in: 1,
        tokens_out: 2,
        tokens_cache_read: 3,
        tokens_cache_write: 4,
        cost: None,
        latency_ms: 5,
        status: 200,
    };
    let json = serde_json::to_string(&rec).expect("serialize");
    let back: ModelCallRecord = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.cost, None);
    assert_eq!(back, rec);
}
