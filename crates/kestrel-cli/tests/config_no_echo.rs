//! Config errors never echo user-supplied text, only real config names, so a key pasted in the
//! wrong place never reaches the terminal.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{DEADLINE, Fixture, path_str};
use predicates::prelude::*;

const KEY: &str = "sk-ant-NOECHO-5f3a9c";
const NUM: &str = "73519284617";

/// Runs `kestrel serve` on `config` and returns stderr. It must exit 2 and never show a secret.
fn stderr_of(config: &str) -> String {
    let f = Fixture::new();
    let path = f.write("config.toml", config);
    let out = f
        .kestrel()
        .args([
            "serve",
            "--config",
            path_str(&path),
            "--sandbox-driver",
            "fake",
        ])
        .timeout(DEADLINE)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("NOECHO").not())
        .stderr(predicate::str::contains(NUM).not());
    String::from_utf8_lossy(&out.get_output().stderr).to_string()
}

fn has(stderr: &str, parts: &[&str]) {
    for p in parts {
        assert!(stderr.contains(p), "missing {p:?} in: {stderr}");
    }
}

#[test]
fn numeric_key_with_a_comment_is_masked() {
    let e = stderr_of(&format!(
        "[providers.anthropic]\napi_key = {NUM} # my key\n"
    ));
    has(&e, &["config.toml:2:"]);
}

#[test]
fn numeric_key_in_an_inline_table_is_masked() {
    let e = stderr_of(&format!("[providers]\nanthropic = {{ api_key = {NUM} }}\n"));
    has(&e, &["config.toml:2:"]);
}

#[test]
fn a_key_used_as_a_field_or_table_name_is_masked() {
    let e = stderr_of(&format!("[providers.anthropic]\n{KEY} = 1\n"));
    has(&e, &["config.toml:2:", "`base_url`", "`api_key`"]);
    let e = stderr_of(&format!("[providers.{KEY}]\napi_key = \"x\"\n"));
    has(&e, &["config.toml:1:", "`anthropic`", "`openai`"]);
}

#[test]
fn a_typo_of_a_real_field_is_masked_but_the_expected_names_stay() {
    let e = stderr_of("[providers.anthropic]\napi_kye = \"x\"\n");
    has(
        &e,
        &[
            "config.toml:2:",
            "`***`",
            "expected `base_url` or `api_key`",
        ],
    );
}

#[test]
fn a_key_as_base_url_names_the_field_only() {
    let e = stderr_of(&format!(
        "[providers.anthropic]\napi_key = \"x\"\nbase_url = \"{KEY}\"\n"
    ));
    has(
        &e,
        &["providers.anthropic.base_url", "not a valid absolute URL"],
    );
}

#[test]
fn a_key_as_a_price_names_the_field_only() {
    let e = stderr_of(&format!(
        "[providers.anthropic]\napi_key = \"x\"\n[prices.m]\ninput_per_mtok = \"{KEY}\"\n\
         output_per_mtok = \"1\"\ncache_read_per_mtok = \"1\"\ncache_write_per_mtok = \"1\"\n"
    ));
    has(&e, &["prices", "input_per_mtok"]);
}

#[test]
fn a_key_as_a_tier_name_names_the_section_only() {
    let e = stderr_of(&format!(
        "[providers.anthropic]\napi_key = \"x\"\n[models]\n{KEY} = \"m\"\n"
    ));
    has(&e, &["models", "unknown tier name"]);
}
