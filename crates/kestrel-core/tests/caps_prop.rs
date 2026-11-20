//! Capability tokens (issue #33), part 3. Scenario 11: for random parents, child requests and
//! actions, a verified child allowance is always a subset of the parent allowance.
//!
//! Names come from a four-letter alphabet shared by tools, hosts, secrets and agents, so sets
//! overlap often and a name granted as one kind is regularly asked for as another. Child requests
//! are mostly clamped into the parent (so `attenuate` succeeds and the actions are exercised) and
//! sometimes left raw (so escalations are tried). 48 cases: every case signs and verifies
//! Biscuit tokens, which is not free in a debug build.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod caps_support;

use std::collections::BTreeSet;

use caps_support::*;
use kestrel_core::caps::{self, Action, Capability, Decision, RootKey, SpawnLimits};
use kestrel_types::{MicroUsd, RunId};
use proptest::prelude::*;
use time::Duration;

const NAMES: [&str; 4] = ["a", "b", "c", "d"];
const CASES: u32 = 48;

fn names() -> impl Strategy<Value = BTreeSet<String>> {
    proptest::sample::subsequence(NAMES.to_vec(), 0..=NAMES.len())
        .prop_map(|v| v.into_iter().map(String::from).collect())
}

fn spawn_limits() -> impl Strategy<Value = Option<SpawnLimits>> {
    proptest::option::of((0u32..=3, 0u32..=3, names()).prop_map(
        |(max_children, max_depth, agents)| SpawnLimits {
            max_children,
            max_depth,
            agents,
        },
    ))
}

/// Everything but the run id, which each case assigns.
#[derive(Debug, Clone)]
struct Parts {
    tools: BTreeSet<String>,
    hosts: BTreeSet<String>,
    secrets: BTreeSet<String>,
    spawn: Option<SpawnLimits>,
    budget: i64,
    /// Seconds after `t0`, at least 1 so `t0` is always inside the window.
    expires_in: i64,
}

fn parts() -> impl Strategy<Value = Parts> {
    (
        names(),
        names(),
        names(),
        spawn_limits(),
        0i64..=4_000_000,
        1i64..=7_200,
    )
        .prop_map(|(tools, hosts, secrets, spawn, budget, expires_in)| Parts {
            tools,
            hosts,
            secrets,
            spawn,
            budget,
            expires_in,
        })
}

fn cap(run: RunId, p: &Parts) -> Capability {
    Capability {
        run,
        tools: p.tools.clone(),
        hosts: p.hosts.clone(),
        secrets: p.secrets.clone(),
        spawn: p.spawn.clone(),
        budget: MicroUsd(p.budget),
        expires_at: t0() + Duration::seconds(p.expires_in),
    }
}

/// `child` pulled inside `parent` on every axis, so it is a valid attenuation.
fn clamp(child: &Parts, parent: &Parts) -> Parts {
    let meet = |c: &BTreeSet<String>, p: &BTreeSet<String>| c.intersection(p).cloned().collect();
    let spawn = match (&child.spawn, &parent.spawn) {
        (Some(c), Some(p)) if p.max_depth >= 1 => Some(SpawnLimits {
            max_children: c.max_children.min(p.max_children),
            max_depth: c.max_depth.min(p.max_depth - 1),
            agents: meet(&c.agents, &p.agents),
        }),
        _ => None,
    };
    Parts {
        tools: meet(&child.tools, &parent.tools),
        hosts: meet(&child.hosts, &parent.hosts),
        secrets: meet(&child.secrets, &parent.secrets),
        spawn,
        budget: child.budget.min(parent.budget),
        expires_in: child.expires_in.min(parent.expires_in),
    }
}

fn action() -> impl Strategy<Value = Action> {
    let name = || proptest::sample::select(NAMES.to_vec()).prop_map(String::from);
    prop_oneof![
        name().prop_map(Action::Tool),
        name().prop_map(Action::Host),
        name().prop_map(Action::Secret),
        name().prop_map(|agent| Action::Spawn { agent }),
        (0i64..=5_000_000).prop_map(|m| Action::Spend(MicroUsd(m))),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn s11_a_verified_child_allowance_is_a_subset_of_the_parent_allowance(
        parent_parts in parts(),
        child_raw in parts(),
        escalate in proptest::bool::weighted(0.3),
        actions in proptest::collection::vec(action(), 8),
    ) {
        let key = RootKey::generate();
        let pk = key.public();
        let parent = cap(RunId::new(), &parent_parts);
        let parent_token = caps::issue(&key, &parent).expect("root token issues");

        let child_parts = if escalate { child_raw } else { clamp(&child_raw, &parent_parts) };
        let child = cap(RunId::new(), &child_parts);
        let subset = child.check_subset_of(&parent);
        if !escalate {
            prop_assert!(subset.is_ok(), "clamped child refused: {:?}", subset);
        }

        let attenuated = caps::attenuate(&parent_token, &key, &child);
        prop_assert_eq!(
            subset.is_ok(),
            attenuated.is_ok(),
            "check_subset_of and attenuate disagree: {:?} vs {:?}",
            subset,
            attenuated
        );
        let Ok(child_token) = attenuated else {
            return Ok(());
        };
        prop_assert_eq!(&caps::grant(&child_token, &pk).expect("child verifies"), &child);

        for a in actions {
            let by_child = caps::verify(
                &child_token,
                &pk,
                &req_at(child.run, a.clone(), t0()),
            );
            if by_child == Decision::Allow {
                let by_parent = caps::verify(
                    &parent_token,
                    &pk,
                    &req_at(parent.run, a.clone(), t0()),
                );
                prop_assert_eq!(by_parent, Decision::Allow, "child allowed {:?}", a);
            }
        }
    }
}
