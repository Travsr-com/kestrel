//! Tier to model mapping, random per-run gateway tokens, and budget changes that survive a
//! restart.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::*;
use kestrel_core::{Engine, Limits, ModelMap, default_model, default_models};
use kestrel_store::ControlCommand;
use kestrel_types::{MicroUsd, RunStatus, SuspendReason, Tier};
use url::Url;

#[tokio::test(start_paused = true)]
async fn tier_mid_launches_with_the_default_mid_model() {
    let mut h = Harness::new(4);
    h.driver.script(&["ok:done"], 0);
    h.start();
    let id = h.submit(spec()).await;
    until("completed", || h.is(id, &RunStatus::Completed)).await;

    let plans = h.adapters.plans();
    assert_eq!(plans[0][0].model, default_model(Tier::Mid));
    assert_eq!(plans[0][0].model, "claude-sonnet-5-5");
    assert_eq!(default_models().len(), 3);
    assert_eq!(default_model(Tier::Cheap), "claude-haiku-5-5");
    assert_eq!(default_model(Tier::Frontier), "claude-opus-5-5");
}

#[tokio::test(start_paused = true)]
async fn configured_models_win_and_missing_tiers_fall_back_to_defaults() {
    let mut h = Harness::new(4);
    let models: ModelMap = [(Tier::Mid, "my-mid-model".to_string())].into();
    h.engine = Arc::new(Engine::with_models(
        h.store.clone(),
        vec![Box::new(h.driver.clone())],
        adapter_factory(h.adapters.clone()),
        Url::parse(GATEWAY).unwrap(),
        Limits {
            max_concurrent_runs: 1,
            poll_interval: POLL,
        },
        models,
    ));
    h.driver.script(&["ok:mid"], 0);
    h.driver.script(&["ok:cheap"], 0);
    h.start();
    let mid = h.submit(spec()).await;
    let mut cheap_spec = spec();
    cheap_spec.model_policy.default = Tier::Cheap;
    let cheap = h.submit(cheap_spec).await;
    until("both completed", || {
        h.is(mid, &RunStatus::Completed) && h.is(cheap, &RunStatus::Completed)
    })
    .await;

    let models: Vec<String> = h
        .adapters
        .plans()
        .into_iter()
        .map(|p| p[0].model.clone())
        .collect();
    assert_eq!(models, vec!["my-mid-model", "claude-haiku-5-5"]);
}

#[tokio::test(start_paused = true)]
async fn gateway_url_carries_the_random_run_token_not_the_run_id() {
    let mut h = Harness::new(4);
    h.driver.script(&["ok:done"], 0);
    h.start();
    let id = h.submit(spec()).await;
    until("completed", || h.is(id, &RunStatus::Completed)).await;

    let token = h.store.run_token(id).unwrap().expect("token stored");
    let url = h.adapters.plans()[0][0].gateway_url.clone();
    assert_eq!(url.as_str(), format!("{GATEWAY}r/{token}"));
    assert!(!url.as_str().contains(&id.to_string()), "{url}");
    assert_eq!(h.store.run_by_token(&token).unwrap(), Some(id));
}

#[tokio::test(start_paused = true)]
async fn set_budget_survives_an_engine_restart() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("kestrel.db");
    let budget = RunStatus::Suspended(SuspendReason::Budget);

    let id = {
        let mut first = Harness::on(&db, 4);
        let _s = first.driver.live();
        first.start();
        let id = first.submit(spec_with("1", "1h")).await;
        until("running", || first.is(id, &RunStatus::Running)).await;
        first
            .store
            .record_model_call(&model_call(id, 1_000_000))
            .unwrap();
        until("budget suspension", || first.is(id, &budget)).await;

        first.control(id, ControlCommand::SetBudget(MicroUsd(3_000_000)));
        until("budget stored", || {
            first
                .store
                .run_spec(id)
                .unwrap()
                .expect("spec")
                .budget
                .limit
                == MicroUsd(3_000_000)
        })
        .await;
        id
    };

    // A new engine on the same database: resume alone is enough, the raised limit applies.
    let mut h = Harness::on(&db, 4);
    let report = h.engine.recover().await.expect("recover");
    assert_eq!(report.suspended, vec![id]);
    let _resumed = h.driver.live();
    h.start();
    h.control(id, ControlCommand::Resume);
    until("running again", || h.is(id, &RunStatus::Running)).await;
}
