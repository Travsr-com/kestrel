//! Edge cases beyond the issue #4 scenarios: cost rounding through the gateway and requests
//! the gateway cannot read.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use kestrel_types::MicroUsd;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// One cached OpenAI token at $0.075 per million tokens is 0.075 micro-dollars. A fractional
/// micro-dollar is rounded up so budgets never under-count.
#[tokio::test]
async fn fractional_micro_dollar_cost_rounds_up() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            openai_response(OPENAI_MODEL, 1, 0, 1).into_bytes(),
            "application/json",
        ))
        .mount(&upstream)
        .await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.openai_url(RUN_TOKEN))
        .header("content-type", "application/json")
        .body(openai_request(OPENAI_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.unwrap();

    let recs = h.sink.wait_for(1).await;
    assert_eq!(recs.len(), 1);
    assert_eq!((recs[0].tokens_in, recs[0].tokens_cache_read), (0, 1));
    assert_eq!(recs[0].cost, Some(MicroUsd(1)));
}

/// A request body that is not JSON is still forwarded unchanged; the record has an empty
/// model, which is not in the price table, so `cost` is `None`.
#[tokio::test]
async fn non_json_request_is_forwarded_and_unpriced() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            anthropic_response(CLAUDE_MODEL, 3, 4, 0, 0).into_bytes(),
            "application/json",
        ))
        .mount(&upstream)
        .await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.unwrap();

    let reqs = upstream.received_requests().await.unwrap();
    assert_eq!(reqs[0].body, b"not json");
    let recs = h.sink.wait_for(1).await;
    assert_eq!(recs[0].model, "");
    assert_eq!((recs[0].tokens_in, recs[0].tokens_out), (3, 4));
    assert_eq!(recs[0].cost, None);
}
