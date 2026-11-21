//! Issue #36: `Engine::spawn_child`. The store side of budget carving is tested in
//! `kestrel-store/tests/spawn_carving.rs`; these tests cover authorisation, attenuation, limits,
//! queueing, the model tier, the workspace and restart, through the engine.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::BTreeSet;

use common::*;
use kestrel_core::caps::{self, CapToken, Capability, RootKey, SpawnLimits};
use kestrel_core::{Engine, SpawnError};
use kestrel_store::ControlCommand;
use kestrel_types::{
    AgentSpec, EventKind, MicroUsd, RunId, RunStatus, SuspendReason, Tier, WorkspaceSpec,
};
use time::OffsetDateTime;

const DOLLAR: i64 = 1_000_000;

/// An agent spec. `model_policy` is a whole YAML line (or empty), `caps` a flow mapping.
fn agent(name: &str, model_policy: &str, caps: &str, usd: &str, workspace: &str) -> AgentSpec {
    let yaml = format!(
        "name: {name}\nruntime: claude-code\nrole: r\n{model_policy}\
         capabilities: {caps}\n\
         sandbox: {{ tier: auto, image: fake-image, cpu: 1, memory: 512Mi, workspace: {workspace} }}\n\
         budget: {{ usd: \"{usd}\", wall_clock: 1h }}\n"
    );
    AgentSpec::from_yaml(&yaml).expect("fixture spec parses")
}

const LEAD_CAPS: &str = "{ mcp: [kestrel.spawn, fs.read], network: [crates.io], \
     spawn: { max_children: 10, max_depth: 1, agents: [worker] } }";

/// A frontier-tier parent that may spawn `worker` one level deep.
fn lead(usd: &str) -> AgentSpec {
    agent(
        "lead",
        "model_policy: { default: tier:frontier }\n",
        LEAD_CAPS,
        usd,
        "empty-dir",
    )
}

/// A child with no `model_policy`, asking for more than `lead` holds: `fs.write`,
/// `evil.example`, the secret `npm_token`, a deeper spawn and the agent `deployer`.
fn worker() -> AgentSpec {
    agent(
        "worker",
        "",
        "{ mcp: [kestrel.spawn, fs.read, fs.write], network: [crates.io, evil.example], \
         secrets: [npm_token], spawn: { max_children: 2, max_depth: 2, agents: [worker, deployer] } }",
        "5",
        "empty-dir",
    )
}

/// A harness whose engine holds `key`.
fn harness(max: usize, key: &RootKey) -> Harness {
    let mut h = Harness::new(max);
    let bytes = key.to_bytes();
    h.rebuild(|e| e.with_root_key(RootKey::from_bytes(&bytes).unwrap()));
    h
}

/// Starts the engine and a running `lead` with budget `usd`; returns its id and session.
async fn running_lead(h: &mut Harness, usd: &str) -> (RunId, SessionCtl) {
    let session = h.driver.live();
    h.start();
    let id = h.submit(lead(usd)).await;
    until("lead running", || h.is(id, &RunStatus::Running)).await;
    (id, session)
}

async fn spawn(h: &Harness, parent: RunId, budget: Option<i64>) -> Result<RunId, SpawnError> {
    h.engine
        .spawn_child(parent, &worker(), "help", budget.map(MicroUsd))
        .await
}

fn token(h: &Harness, run: RunId) -> CapToken {
    CapToken::from_bytes(h.store.run_cap(run).unwrap().expect("run has a token"))
}

fn runs(h: &Harness) -> usize {
    h.store.list_runs(Default::default()).unwrap().len()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// 1. Given a parent with $2 remaining and a spawn of $0.50, then the child's budget is $0.50 and
//    the parent's remaining is $1.50.
#[tokio::test(start_paused = true)]
async fn s1_spawn_carves_the_child_budget() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let (lead, _s) = running_lead(&mut h, "2").await;
    let child = spawn(&h, lead, Some(DOLLAR / 2)).await.unwrap();
    assert_eq!(h.store.limit(child).unwrap(), MicroUsd(DOLLAR / 2));
    assert_eq!(h.store.remaining(lead).unwrap(), MicroUsd(3 * DOLLAR / 2));
    let row = h.store.get_run(child).unwrap().unwrap();
    assert_eq!((row.parent, row.root, row.depth), (Some(lead), lead, 1));
    assert!(h.has_event(lead, |e| matches!(e,
        EventKind::ChildSpawned { child: c, budget, .. } if *c == child && *budget == MicroUsd(DOLLAR / 2))));
    assert!(h.has_event(child, |e| matches!(e,
        EventKind::RunCreated { parent: Some(p), .. } if *p == lead)));
}

// 2. Given a parent with $0.40 remaining and a spawn asking $0.50, then `SpawnError::Budget` and
//    nothing is created or reserved.
#[tokio::test(start_paused = true)]
async fn s2_over_budget_is_refused_and_creates_nothing() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let (lead, _s) = running_lead(&mut h, "0.40").await;
    let err = spawn(&h, lead, Some(DOLLAR / 2)).await.unwrap_err();
    assert!(
        matches!(err, SpawnError::Budget { asked, remaining }
            if asked == MicroUsd(DOLLAR / 2) && remaining == MicroUsd(4 * DOLLAR / 10)),
        "{err:?}"
    );
    assert_eq!(runs(&h), 1);
    assert_eq!(h.store.remaining(lead).unwrap(), MicroUsd(4 * DOLLAR / 10));
}

// 3. Given concurrent spawns that together ask for more than the parent has, then exactly the
//    spawns that fit succeed (no overspend). The race itself is tested in the store
//    (`spawn_carving::s3_*`, eight threads on separate connections); the paused clock needs a
//    single-threaded runtime, so here the tasks interleave and this checks the engine's mapping of
//    the refusals.
#[tokio::test(start_paused = true)]
async fn s3_concurrent_spawns_never_overspend() {
    let key = RootKey::generate();
    let mut h = harness(16, &key);
    let (lead, _s) = running_lead(&mut h, "1").await;
    let engine = h.engine.clone();
    let tasks: Vec<_> = (0..5)
        .map(|_| {
            let engine = engine.clone();
            tokio::spawn(async move {
                engine
                    .spawn_child(lead, &worker(), "help", Some(MicroUsd(3 * DOLLAR / 10)))
                    .await
            })
        })
        .collect();
    let mut ok = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => ok += 1,
            Err(SpawnError::Budget { .. }) => {}
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(ok, 3);
    assert_eq!(h.store.remaining(lead).unwrap(), MicroUsd(DOLLAR / 10));
}

// 4. Given a child that spent $0.20 of $0.50 and then finished, then $0.30 returns to the parent.
#[tokio::test(start_paused = true)]
async fn s4_a_finished_child_returns_its_unspent_budget() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let (lead, _s) = running_lead(&mut h, "2").await;
    let child_session = h.driver.live();
    let child = spawn(&h, lead, Some(DOLLAR / 2)).await.unwrap();
    until("child running", || h.is(child, &RunStatus::Running)).await;
    h.store
        .record_model_call(&model_call(child, DOLLAR / 5))
        .unwrap();
    child_session.exit(0);
    until("child completed", || h.is(child, &RunStatus::Completed)).await;
    assert_eq!(h.store.remaining(lead).unwrap(), MicroUsd(18 * DOLLAR / 10));
}

// The parent's own budget check uses its current limit, not the one it started with: after a
// child spent its whole carve, the parent may only spend what it kept.
#[tokio::test(start_paused = true)]
async fn a_parent_is_suspended_at_the_limit_it_kept_after_carving() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let (lead, session) = running_lead(&mut h, "1").await;
    let child_session = h.driver.live();
    let child = spawn(&h, lead, Some(9 * DOLLAR / 10)).await.unwrap();
    until("child running", || h.is(child, &RunStatus::Running)).await;
    h.store
        .record_model_call(&model_call(child, 9 * DOLLAR / 10))
        .unwrap();
    child_session.exit(0);
    until("child completed", || h.is(child, &RunStatus::Completed)).await;
    assert_eq!(h.store.limit(lead).unwrap(), MicroUsd(DOLLAR / 10));
    h.store
        .record_model_call(&model_call(lead, 15 * DOLLAR / 100))
        .unwrap();
    session.line("msg: still working");
    until("lead suspended for budget", || {
        h.is(lead, &RunStatus::Suspended(SuspendReason::Budget))
    })
    .await;
}

// 6. Given `max_depth` 1, when a child tries to spawn a grandchild, then `SpawnError::Depth`.
#[tokio::test(start_paused = true)]
async fn s6_a_child_cannot_spawn_past_max_depth() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let (lead, _s) = running_lead(&mut h, "2").await;
    let child = spawn(&h, lead, Some(DOLLAR / 2)).await.unwrap();
    until("child running", || h.is(child, &RunStatus::Running)).await;
    let err = spawn(&h, child, Some(DOLLAR / 10)).await.unwrap_err();
    assert!(matches!(err, SpawnError::Depth), "{err:?}");
    assert_eq!(runs(&h), 2);
}

// 7. Given `max_children` 2, when a third spawn is attempted, then `SpawnError::TooManyChildren`.
#[tokio::test(start_paused = true)]
async fn s7_a_third_child_past_max_children_is_refused() {
    let key = RootKey::generate();
    let mut h = harness(8, &key);
    let caps =
        "{ mcp: [kestrel.spawn], spawn: { max_children: 2, max_depth: 1, agents: [worker] } }";
    let session = h.driver.live();
    let _keep = session;
    h.start();
    let lead = h.submit(agent("lead", "", caps, "2", "empty-dir")).await;
    until("lead running", || h.is(lead, &RunStatus::Running)).await;
    spawn(&h, lead, Some(DOLLAR / 10)).await.unwrap();
    spawn(&h, lead, Some(DOLLAR / 10)).await.unwrap();
    let err = spawn(&h, lead, Some(DOLLAR / 10)).await.unwrap_err();
    assert!(matches!(err, SpawnError::TooManyChildren(2)), "{err:?}");
}

// 8. Given a child agent not in the parent's allowed `agents`, then `SpawnError::AgentNotAllowed`.
#[tokio::test(start_paused = true)]
async fn s8_an_agent_the_parent_may_not_spawn_is_refused() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let (lead, _s) = running_lead(&mut h, "2").await;
    let other = agent("deployer", "", "{}", "1", "empty-dir");
    let err = h
        .engine
        .spawn_child(lead, &other, "x", None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SpawnError::AgentNotAllowed(a) if a == "deployer"),
        "{err:?}"
    );
    assert_eq!(runs(&h), 1);
}

// 9. Given a parent whose capability lacks `kestrel.spawn`, then `SpawnError::NotPermitted`.
#[tokio::test(start_paused = true)]
async fn s9_a_parent_without_kestrel_spawn_is_not_permitted() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let caps = "{ mcp: [fs.read], spawn: { max_children: 2, max_depth: 1, agents: [worker] } }";
    let _s = h.driver.live();
    h.start();
    let lead = h.submit(agent("lead", "", caps, "2", "empty-dir")).await;
    until("lead running", || h.is(lead, &RunStatus::Running)).await;
    let err = spawn(&h, lead, None).await.unwrap_err();
    assert!(matches!(err, SpawnError::NotPermitted(_)), "{err:?}");
    // No spawn rights at all is the same refusal.
    let none = h
        .submit(agent(
            "solo",
            "",
            "{ mcp: [kestrel.spawn] }",
            "2",
            "empty-dir",
        ))
        .await;
    let _t = h.driver.live();
    until("solo running", || h.is(none, &RunStatus::Running)).await;
    let err = spawn(&h, none, None).await.unwrap_err();
    assert!(matches!(err, SpawnError::NotPermitted(_)), "{err:?}");
    assert_eq!(runs(&h), 2);
}

// 10. Given a child spec that asks for a tool or host the parent lacks, then the child's token does
//     not include it (child gets the intersection), and the child records what was dropped.
#[tokio::test(start_paused = true)]
async fn s10_the_child_gets_the_intersection_and_records_what_was_dropped() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let (lead, _s) = running_lead(&mut h, "2").await;
    let child = spawn(&h, lead, Some(DOLLAR / 2)).await.unwrap();
    let grant = caps::grant(&token(&h, child), &key.public()).unwrap();
    assert_eq!(grant.run, child);
    assert_eq!(grant.tools, set(&["fs.read", "kestrel.spawn"]));
    assert_eq!(grant.hosts, set(&["crates.io"]));
    assert!(grant.secrets.is_empty());
    assert_eq!(grant.budget, MicroUsd(DOLLAR / 2));
    let spawn = grant.spawn.expect("child keeps spawn limits");
    assert_eq!(spawn.max_depth, 0);
    assert_eq!(spawn.agents, set(&["worker"]));
    // The stored spec holds only what was granted, so nothing reading it later sees more.
    let stored = h.store.run_spec(child).unwrap().unwrap().capabilities;
    assert_eq!(stored.mcp, ["fs.read", "kestrel.spawn"]);
    assert_eq!(stored.network, ["crates.io"]);
    assert!(stored.secrets.is_empty());
    let policy = stored.spawn.expect("spawn policy kept");
    assert_eq!((policy.max_children, policy.max_depth), (2, 0));
    assert_eq!(policy.agents, ["worker"]);
    assert!(h.has_event(child, |e| *e
        == EventKind::CapabilitiesDropped {
            tools: vec!["fs.write".into()],
            hosts: vec!["evil.example".into()],
            secrets: vec!["npm_token".into()],
            agents: vec!["deployer".into()],
        }));
}

#[tokio::test(start_paused = true)]
async fn a_child_asking_for_nothing_extra_records_no_drop() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let (lead, _s) = running_lead(&mut h, "2").await;
    let modest = agent("worker", "", "{ mcp: [fs.read] }", "1", "empty-dir");
    let child = h
        .engine
        .spawn_child(lead, &modest, "x", None)
        .await
        .unwrap();
    assert!(!h.has_event(child, |e| matches!(
        e,
        EventKind::CapabilitiesDropped { .. }
    )));
    let grant = caps::grant(&token(&h, child), &key.public()).unwrap();
    assert_eq!(grant.spawn, None);
}

// 11. Given a cap and more spawns than free slots, then the extra children stay `Queued` and start
//     as slots free. The parent holds one slot, so a cap of 3 leaves 2 for children.
#[tokio::test(start_paused = true)]
async fn s11_children_over_the_cap_queue_and_start_as_slots_free() {
    let key = RootKey::generate();
    let mut h = harness(3, &key);
    let (lead, _s) = running_lead(&mut h, "5").await;
    let sessions: Vec<SessionCtl> = (0..5).map(|_| h.driver.live()).collect();
    let mut children = Vec::new();
    for _ in 0..5 {
        children.push(spawn(&h, lead, Some(DOLLAR / 2)).await.unwrap());
    }
    let count = |h: &Harness, s: &RunStatus| children.iter().filter(|c| h.is(**c, s)).count();
    until("two children running", || {
        count(&h, &RunStatus::Running) == 2
    })
    .await;
    settle().await;
    assert_eq!(count(&h, &RunStatus::Running), 2);
    assert_eq!(count(&h, &RunStatus::Queued), 3);
    // Children start in spawn order, so the first session belongs to the first child.
    let mut sessions = sessions.into_iter();
    sessions.next().unwrap().exit(0);
    until("a queued child starts", || {
        count(&h, &RunStatus::Queued) == 2
    })
    .await;
    assert_eq!(count(&h, &RunStatus::Running), 2);
}

// 12. Given a parent in `Suspended(Budget)`, then spawning is refused until it is resumed.
#[tokio::test(start_paused = true)]
async fn s12_a_parent_suspended_for_budget_cannot_spawn_until_resumed() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let (lead, session) = running_lead(&mut h, "1").await;
    h.store
        .record_model_call(&model_call(lead, DOLLAR))
        .unwrap();
    session.line("msg: spending");
    let suspended = RunStatus::Suspended(SuspendReason::Budget);
    until("lead suspended", || h.is(lead, &suspended)).await;
    let err = spawn(&h, lead, Some(DOLLAR / 10)).await.unwrap_err();
    assert!(
        matches!(&err, SpawnError::ParentNotRunning(id, s) if *id == lead && *s == suspended),
        "{err:?}"
    );
    let _resumed = h.driver.live();
    h.control(lead, ControlCommand::SetBudget(MicroUsd(2 * DOLLAR)));
    h.control(lead, ControlCommand::Resume);
    until("lead running again", || h.is(lead, &RunStatus::Running)).await;
    spawn(&h, lead, Some(DOLLAR / 10)).await.unwrap();
}

// 13. Given an engine restart after a spawn, then the child, its link, its carved budget and its
//     token are all still there.
#[tokio::test(start_paused = true)]
async fn s13_a_spawn_survives_an_engine_restart() {
    let key = RootKey::generate();
    let bytes = key.to_bytes();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let mut h = Harness::on(&db, 4);
    h.rebuild(|e| e.with_root_key(RootKey::from_bytes(&bytes).unwrap()));
    let (lead, _s) = running_lead(&mut h, "2").await;
    let child = spawn(&h, lead, Some(DOLLAR / 2)).await.unwrap();
    h.shutdown.cancel();
    if let Some(task) = h.task.take() {
        task.await.unwrap().unwrap();
    }
    drop(h);

    let mut again = Harness::on(&db, 4);
    again.rebuild(|e| e.with_root_key(RootKey::from_bytes(&bytes).unwrap()));
    again.engine.recover().await.unwrap();
    let row = again.store.get_run(child).unwrap().unwrap();
    assert_eq!(row.parent, Some(lead));
    assert_eq!(again.store.limit(child).unwrap(), MicroUsd(DOLLAR / 2));
    assert_eq!(
        again.store.remaining(lead).unwrap(),
        MicroUsd(3 * DOLLAR / 2)
    );
    let grant = caps::grant(&token(&again, child), &key.public()).unwrap();
    assert_eq!(grant.run, child);
    drop(dir);
}

// 14. Given a parent that does not exist or is already finished, then it fails and creates
//     nothing.
#[tokio::test(start_paused = true)]
async fn s14_a_missing_or_finished_parent_spawns_nothing() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let ghost = RunId::new();
    let err = spawn(&h, ghost, None).await.unwrap_err();
    assert!(
        matches!(err, SpawnError::ParentNotFound(id) if id == ghost),
        "{err:?}"
    );
    let (lead, session) = running_lead(&mut h, "2").await;
    session.exit(0);
    until("lead completed", || h.is(lead, &RunStatus::Completed)).await;
    let err = spawn(&h, lead, None).await.unwrap_err();
    assert!(
        matches!(&err, SpawnError::ParentNotRunning(_, RunStatus::Completed)),
        "{err:?}"
    );
    assert_eq!(runs(&h), 1);
}

#[tokio::test(start_paused = true)]
async fn spawning_needs_a_root_key() {
    let mut h = Harness::new(4);
    let (lead, _s) = running_lead(&mut h, "2").await;
    let err = spawn(&h, lead, None).await.unwrap_err();
    assert!(matches!(err, SpawnError::NoRootKey), "{err:?}");
    assert_eq!(runs(&h), 1);
}

#[tokio::test(start_paused = true)]
async fn a_non_root_run_without_a_token_may_not_spawn() {
    let key = RootKey::generate();
    let h = harness(4, &key);
    let root = h.store.create_run(&lead("2"), "x").unwrap();
    let orphan = h.store.create_child_run(root, &lead("1"), "y").unwrap();
    h.store
        .transition(orphan, &RunStatus::Queued, RunStatus::Provisioning)
        .unwrap();
    h.store
        .transition(orphan, &RunStatus::Provisioning, RunStatus::Running)
        .unwrap();
    let err = spawn(&h, orphan, None).await.unwrap_err();
    assert!(matches!(err, SpawnError::NotPermitted(_)), "{err:?}");
}

// A root's token is made from its spec, so an expired one is simply made again.
#[tokio::test(start_paused = true)]
async fn an_expired_root_token_is_issued_again() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let (lead, _s) = running_lead(&mut h, "2").await;
    let cap = Capability::from_spec(lead, &lead_spec_for_token(), OffsetDateTime::UNIX_EPOCH);
    let old = caps::issue(&key, &cap).unwrap();
    h.store.set_run_cap(lead, old.as_bytes()).unwrap();
    spawn(&h, lead, Some(DOLLAR / 10)).await.unwrap();
    let fresh = caps::grant(&token(&h, lead), &key.public()).unwrap();
    assert!(fresh.expires_at > OffsetDateTime::now_utc());
}

// A spawned run's token comes from its parent's, so an expired one cannot be remade.
#[tokio::test(start_paused = true)]
async fn an_expired_child_token_is_not_permitted() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    // Two levels, so the child may spawn and only its token's expiry stops it.
    let deep = "{ mcp: [kestrel.spawn, fs.read], spawn: { max_children: 10, max_depth: 2, agents: [worker] } }";
    let _s = h.driver.live();
    h.start();
    let lead = h.submit(agent("lead", "", deep, "2", "empty-dir")).await;
    until("lead running", || h.is(lead, &RunStatus::Running)).await;
    let child = spawn(&h, lead, Some(DOLLAR / 2)).await.unwrap();
    until("child running", || h.is(child, &RunStatus::Running)).await;
    let mut cap = caps::grant(&token(&h, child), &key.public()).unwrap();
    cap.expires_at = OffsetDateTime::UNIX_EPOCH;
    let old = caps::issue(&key, &cap).unwrap();
    h.store.set_run_cap(child, old.as_bytes()).unwrap();
    let err = spawn(&h, child, Some(DOLLAR / 10)).await.unwrap_err();
    assert!(matches!(err, SpawnError::NotPermitted(_)), "{err:?}");
}

// A parent that handed out all of its own budget keeps running while its children do: it cannot
// spend (the gateway refuses it at its limit), but it can wait for them.
#[tokio::test(start_paused = true)]
async fn a_parent_that_gave_everything_away_keeps_running_while_its_children_run() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let (lead, session) = running_lead(&mut h, "1").await;
    let child_session = h.driver.live();
    let child = spawn(&h, lead, Some(DOLLAR)).await.unwrap();
    until("child running", || h.is(child, &RunStatus::Running)).await;
    session.line("msg: waiting for my child");
    settle().await;
    assert!(h.is(lead, &RunStatus::Running));
    child_session.exit(0);
    until("child completed", || h.is(child, &RunStatus::Completed)).await;
    assert_eq!(h.store.remaining(lead).unwrap(), MicroUsd(DOLLAR));
    session.line("msg: done waiting");
    settle().await;
    assert!(h.is(lead, &RunStatus::Running));
}

// `kestrel budget` on a running child takes the raise from its parent, or is refused with a log
// line when the parent cannot cover it.
#[tokio::test(start_paused = true)]
async fn raising_a_running_child_takes_from_the_parent_or_is_refused() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let (lead, _s) = running_lead(&mut h, "1").await;
    let _child_session = h.driver.live();
    let child = spawn(&h, lead, Some(DOLLAR / 2)).await.unwrap();
    until("child running", || h.is(child, &RunStatus::Running)).await;
    h.control(child, ControlCommand::SetBudget(MicroUsd(7 * DOLLAR / 10)));
    until("raise applied", || {
        h.store.limit(child).unwrap() == MicroUsd(7 * DOLLAR / 10)
    })
    .await;
    assert_eq!(h.store.remaining(lead).unwrap(), MicroUsd(3 * DOLLAR / 10));
    h.control(child, ControlCommand::SetBudget(MicroUsd(5 * DOLLAR)));
    until("refusal logged", || {
        h.has_event(
            child,
            |e| matches!(e, EventKind::AgentLog { line } if line.contains("raise the root")),
        )
    })
    .await;
    assert_eq!(h.store.limit(child).unwrap(), MicroUsd(7 * DOLLAR / 10));
    assert!(h.is(child, &RunStatus::Running));
}

// The same for a child still waiting for a slot (no actor yet).
#[tokio::test(start_paused = true)]
async fn raising_a_queued_child_past_the_parent_is_refused_with_a_log_line() {
    let key = RootKey::generate();
    // One slot: the parent holds it, so the child stays queued.
    let mut h = harness(1, &key);
    let (lead, _s) = running_lead(&mut h, "1").await;
    let child = spawn(&h, lead, Some(DOLLAR / 2)).await.unwrap();
    h.control(child, ControlCommand::SetBudget(MicroUsd(5 * DOLLAR)));
    until("refusal logged", || {
        h.has_event(
            child,
            |e| matches!(e, EventKind::AgentLog { line } if line.contains("raise the root")),
        )
    })
    .await;
    assert!(h.is(child, &RunStatus::Queued));
    assert_eq!(h.store.limit(child).unwrap(), MicroUsd(DOLLAR / 2));
}

// The 80% warning is once per limit: when a child's refund grows the parent's limit again, the
// parent is warned again at 80% of the larger limit.
#[tokio::test(start_paused = true)]
async fn the_budget_warning_comes_again_after_the_limit_grows() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let (lead, session) = running_lead(&mut h, "1").await;
    let child_session = h.driver.live();
    let child = spawn(&h, lead, Some(DOLLAR / 2)).await.unwrap();
    let warnings =
        |h: &Harness| h.count_events(lead, |e| matches!(e, EventKind::BudgetWarning { .. }));
    h.store
        .record_model_call(&model_call(lead, 45 * DOLLAR / 100))
        .unwrap();
    session.line("msg: one");
    until("first warning", || warnings(&h) == 1).await;
    until("child running", || h.is(child, &RunStatus::Running)).await;
    child_session.exit(0);
    until("child completed", || h.is(child, &RunStatus::Completed)).await;
    // The limit is back to $1: $0.45 spent is under 80%, so no warning yet.
    session.line("msg: two");
    settle().await;
    assert_eq!(warnings(&h), 1);
    h.store
        .record_model_call(&model_call(lead, 40 * DOLLAR / 100))
        .unwrap();
    session.line("msg: three");
    until("second warning", || warnings(&h) == 2).await;
}

fn lead_spec_for_token() -> AgentSpec {
    lead("2")
}

// Model tier: a child without `model_policy` runs one tier below its parent; a child that states
// one keeps it.
#[tokio::test(start_paused = true)]
async fn a_child_defaults_one_tier_below_its_parent_unless_it_states_one() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let (lead, _s) = running_lead(&mut h, "2").await;
    let inherited = spawn(&h, lead, Some(DOLLAR / 10)).await.unwrap();
    let tier = |id| h.store.run_spec(id).unwrap().unwrap().model_policy.default;
    assert_eq!(tier(inherited), Tier::Mid);
    let stated = agent(
        "worker",
        "model_policy: { default: tier:frontier }\n",
        "{}",
        "1",
        "empty-dir",
    );
    let kept = h
        .engine
        .spawn_child(lead, &stated, "x", Some(MicroUsd(DOLLAR / 10)))
        .await
        .unwrap();
    assert_eq!(tier(kept), Tier::Frontier);
}

// Workspace: a child gets its own clone of the parent's repo. A relative `repo` in the child spec
// (as written in YAML) becomes the parent's absolute repo; the clone itself is per run id.
#[tokio::test(start_paused = true)]
async fn a_child_with_a_relative_repo_clones_the_parents_repo() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let repo = std::env::temp_dir().join("kestrel-parent-repo");
    let parent_spec = {
        let mut s = lead("2");
        s.sandbox.workspace = WorkspaceSpec::GitClone {
            repo: repo.clone(),
            depth: None,
        };
        s
    };
    let _s = h.driver.live();
    h.start();
    let lead = h.submit(parent_spec).await;
    until("lead running", || h.is(lead, &RunStatus::Running)).await;
    let child_spec = agent("worker", "", "{}", "1", "git-clone");
    let child = h
        .engine
        .spawn_child(lead, &child_spec, "x", Some(MicroUsd(DOLLAR / 10)))
        .await
        .unwrap();
    let workspace = h.store.run_spec(child).unwrap().unwrap().sandbox.workspace;
    assert_eq!(workspace, WorkspaceSpec::GitClone { repo, depth: None });
}

#[tokio::test(start_paused = true)]
async fn a_relative_repo_under_a_parent_without_one_is_left_as_is() {
    let key = RootKey::generate();
    let mut h = harness(4, &key);
    let (lead, _s) = running_lead(&mut h, "2").await;
    let child_spec = agent("worker", "", "{}", "1", "git-clone");
    let child = h
        .engine
        .spawn_child(lead, &child_spec, "x", Some(MicroUsd(DOLLAR / 10)))
        .await
        .unwrap();
    let workspace = h.store.run_spec(child).unwrap().unwrap().sandbox.workspace;
    assert!(
        matches!(workspace, WorkspaceSpec::GitClone { ref repo, .. } if repo.as_path() == std::path::Path::new(".")),
        "{workspace:?}"
    );
}

// `narrow`: the pure intersection used for the child's grant.
#[test]
fn narrow_without_parent_spawn_rights_drops_all_spawnable_agents() {
    let now = OffsetDateTime::UNIX_EPOCH;
    let mut parent = Capability::from_spec(RunId::new(), &lead("2"), now);
    parent.spawn = None;
    let wanted = Capability::from_spec(RunId::new(), &worker(), now);
    let (got, dropped) = caps::narrow(&parent, &wanted);
    assert_eq!(got.spawn, None);
    assert_eq!(
        dropped.agents,
        vec!["deployer".to_string(), "worker".to_string()]
    );
    assert!(!dropped.is_empty());
}

#[test]
fn narrow_under_a_parent_at_depth_zero_gives_no_spawn_rights() {
    let now = OffsetDateTime::UNIX_EPOCH;
    let mut parent = Capability::from_spec(RunId::new(), &lead("2"), now);
    parent.spawn = Some(SpawnLimits {
        max_children: 5,
        max_depth: 0,
        agents: set(&["worker"]),
    });
    let wanted = Capability::from_spec(RunId::new(), &worker(), now);
    let (got, _) = caps::narrow(&parent, &wanted);
    assert_eq!(got.spawn, None);
}

#[test]
fn narrow_keeps_the_smaller_budget_expiry_and_child_count() {
    let early = OffsetDateTime::UNIX_EPOCH;
    let late = early + time::Duration::hours(1);
    let parent = Capability::from_spec(RunId::new(), &lead("2"), early);
    let wanted = Capability::from_spec(RunId::new(), &worker(), late);
    let (got, dropped) = caps::narrow(&parent, &wanted);
    assert_eq!(got.run, wanted.run);
    assert_eq!(got.budget, MicroUsd(2 * DOLLAR));
    assert_eq!(got.expires_at, early);
    assert_eq!(got.spawn.as_ref().unwrap().max_children, 2);
    assert!(got.check_subset_of(&parent).is_ok());
    let (same, none) = caps::narrow(&parent, &parent.clone());
    assert!(none.is_empty());
    assert_eq!(same.tools, parent.tools);
    let _ = dropped;
}

// Every engine refusal maps from the store's.
#[test]
fn spawn_errors_read_well() {
    let id = RunId::new();
    let texts = [
        SpawnError::NoRootKey.to_string(),
        SpawnError::ParentNotFound(id).to_string(),
        SpawnError::ParentNotRunning(id, RunStatus::Completed).to_string(),
        SpawnError::NotPermitted("no".into()).to_string(),
        SpawnError::Depth.to_string(),
        SpawnError::AgentNotAllowed("x".into()).to_string(),
        SpawnError::TooManyChildren(2).to_string(),
        SpawnError::TooManyRuns(9).to_string(),
        SpawnError::Budget {
            asked: MicroUsd(2),
            remaining: MicroUsd(1),
        }
        .to_string(),
    ];
    for t in texts {
        assert!(!t.is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn the_tree_run_limit_is_enforced() {
    let key = RootKey::generate();
    let mut h = Harness::new(8);
    let bytes = key.to_bytes();
    h.rebuild(|e| {
        e.with_root_key(RootKey::from_bytes(&bytes).unwrap())
            .with_tree_limit(2)
    });
    let (lead, _s) = running_lead(&mut h, "2").await;
    spawn(&h, lead, Some(DOLLAR / 10)).await.unwrap();
    let err = spawn(&h, lead, Some(DOLLAR / 10)).await.unwrap_err();
    assert!(matches!(err, SpawnError::TooManyRuns(2)), "{err:?}");
}

#[allow(dead_code)]
fn _engine_is_send(e: Engine) -> impl Send {
    e
}
