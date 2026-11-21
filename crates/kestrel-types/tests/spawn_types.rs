//! Issue #36, types side: the model tier a child defaults to, and the spawn events.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use kestrel_types::{AgentSpec, EventKind, MicroUsd, ModelPolicy, RunId, Tier};

fn yaml(model_policy: &str) -> String {
    format!(
        "name: a\nruntime: opencode\nrole: r\n{model_policy}\
         sandbox: {{ tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }}\n\
         budget: {{ usd: 1.00, wall_clock: 10m }}\n"
    )
}

#[test]
fn tier_below_steps_down_and_stops_at_cheap() {
    assert_eq!(Tier::Frontier.below(), Tier::Mid);
    assert_eq!(Tier::Mid.below(), Tier::Cheap);
    assert_eq!(Tier::Cheap.below(), Tier::Cheap);
}

#[test]
fn a_spec_that_states_a_tier_records_that_it_did() {
    let spec = AgentSpec::from_yaml(&yaml("model_policy: { default: tier:frontier }\n")).unwrap();
    assert_eq!(spec.model_policy.default, Tier::Frontier);
    assert!(spec.model_policy.stated);
}

#[test]
fn model_policy_may_be_left_out_and_then_is_mid_and_not_stated() {
    let spec = AgentSpec::from_yaml(&yaml("")).unwrap();
    assert_eq!(
        spec.model_policy,
        ModelPolicy {
            default: Tier::Mid,
            escalate: None,
            stated: false
        }
    );
}

#[test]
fn a_spec_stored_before_stated_existed_reads_as_stated() {
    // Specs are stored as JSON in the database; older rows have no `stated` field.
    let spec = AgentSpec::from_yaml(&yaml("model_policy: { default: tier:cheap }\n")).unwrap();
    let mut json = serde_json::to_value(&spec).unwrap();
    json["model_policy"]
        .as_object_mut()
        .unwrap()
        .remove("stated");
    let back: AgentSpec = serde_json::from_value(json).unwrap();
    assert!(back.model_policy.stated);
}

#[test]
fn child_spawned_round_trips() {
    let kind = EventKind::ChildSpawned {
        child: RunId::new(),
        agent: "fixer".into(),
        budget: MicroUsd(500_000),
    };
    let json = serde_json::to_value(&kind).unwrap();
    assert_eq!(json["kind"], "child_spawned");
    assert_eq!(json["budget"], 500_000);
    assert_eq!(serde_json::from_value::<EventKind>(json).unwrap(), kind);
}

#[test]
fn capabilities_dropped_round_trips_and_defaults_missing_lists() {
    let kind = EventKind::CapabilitiesDropped {
        tools: vec!["kestrel.spawn".into()],
        hosts: vec!["evil.example".into()],
        secrets: vec![],
        agents: vec!["other".into()],
    };
    let json = serde_json::to_value(&kind).unwrap();
    assert_eq!(json["kind"], "capabilities_dropped");
    assert_eq!(serde_json::from_value::<EventKind>(json).unwrap(), kind);
    let sparse: EventKind =
        serde_json::from_str(r#"{"kind":"capabilities_dropped","hosts":["h"]}"#).unwrap();
    assert_eq!(
        sparse,
        EventKind::CapabilitiesDropped {
            tools: vec![],
            hosts: vec!["h".into()],
            secrets: vec![],
            agents: vec![],
        }
    );
}
