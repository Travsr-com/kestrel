//! Scenario 6: RunStatus::can_transition_to over every pair.

use kestrel_types::{ApprovalId, RunStatus, SuspendReason};

fn states() -> Vec<(&'static str, RunStatus)> {
    vec![
        ("Queued", RunStatus::Queued),
        ("Provisioning", RunStatus::Provisioning),
        ("Running", RunStatus::Running),
        (
            "Susp(Approval)",
            RunStatus::Suspended(SuspendReason::Approval(ApprovalId::new())),
        ),
        ("Susp(Budget)", RunStatus::Suspended(SuspendReason::Budget)),
        (
            "Susp(Error)",
            RunStatus::Suspended(SuspendReason::Error("e".into())),
        ),
        ("Susp(Manual)", RunStatus::Suspended(SuspendReason::Manual)),
        ("Completed", RunStatus::Completed),
        ("Failed", RunStatus::Failed("boom".into())),
        ("Cancelled", RunStatus::Cancelled),
    ]
}

// Rows = from, columns = to, same order as `states()`.
// Suspended -> Suspended is treated as NOT allowed (the issue lists only Suspended -> Running).
// Provisioning -> Suspended(Error) is how recovery suspends a run that a restart cut off before its
// sandbox started (#92). No other suspension happens before a run is running.
#[rustfmt::skip]
const T: bool = true;
const F: bool = false;
#[rustfmt::skip]
const TABLE: [[bool; 10]; 10] = [
    //            Q  P  R  SA SB SE SM Co Fa Ca
    /* Queued  */ [F, T, F, F, F, F, F, F, T, T],
    /* Provis. */ [F, F, T, F, F, T, F, F, T, T],
    /* Running */ [F, F, F, T, T, T, T, T, T, T],
    /* SuspApp */ [F, F, T, F, F, F, F, F, T, T],
    /* SuspBud */ [F, F, T, F, F, F, F, F, T, T],
    /* SuspErr */ [F, F, T, F, F, F, F, F, T, T],
    /* SuspMan */ [F, F, T, F, F, F, F, F, T, T],
    /* Complet */ [F, F, F, F, F, F, F, F, F, F],
    /* Failed  */ [F, F, F, F, F, F, F, F, F, F],
    /* Cancell */ [F, F, F, F, F, F, F, F, F, F],
];

#[test]
fn s6_transition_table_matches_for_all_pairs() {
    let s = states();
    assert_eq!(s.len() * s.len(), 100);
    for (i, (from_name, from)) in s.iter().enumerate() {
        for (j, (to_name, to)) in s.iter().enumerate() {
            assert_eq!(
                from.can_transition_to(to),
                TABLE[i][j],
                "{from_name} -> {to_name}"
            );
        }
    }
}

#[test]
fn s6_terminal_states_have_no_exits() {
    let s = states();
    for (name, from) in s
        .iter()
        .filter(|(n, _)| matches!(*n, "Completed" | "Failed" | "Cancelled"))
    {
        for (to_name, to) in &s {
            assert!(
                !from.can_transition_to(to),
                "{name} -> {to_name} must be false"
            );
        }
    }
}

#[test]
fn s6_failed_payload_does_not_matter() {
    assert!(RunStatus::Running.can_transition_to(&RunStatus::Failed("a".into())));
    assert!(RunStatus::Running.can_transition_to(&RunStatus::Failed(String::new())));
}
