//! Capability tokens (issue #33), security review fixes: only the root key attenuates, so run
//! binding is a real boundary; spawning needs `kestrel.spawn`; block and spend limits; the public
//! `Capability::allows`; and a `CapToken` Debug that hides the bytes.
//!
//! The forged tokens here use the low-level biscuit-auth API directly, the way an attacker (or a
//! buggy server) could, bypassing `caps::attenuate` and its subset check.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod caps_support;

use biscuit_auth::{Algorithm, Biscuit, BlockBuilder, PrivateKey};
use caps_support::*;
use kestrel_core::caps::{self, Action, CapError, CapToken, Capability, Decision, RootKey};
use kestrel_types::{MicroUsd, RunId};
use serde_json::Value;
use time::Duration;

/// The root key as a biscuit-auth key, for signing blocks by hand.
fn biscuit_key(key: &RootKey) -> PrivateKey {
    PrivateKey::from_bytes(&key.to_bytes(), Algorithm::Ed25519).expect("root key converts")
}

/// `token` parsed with every signature checked, but none of Kestrel's limits.
fn parsed(fx: &Fx, token: &CapToken) -> Biscuit {
    Biscuit::from(token.as_bytes(), biscuit_key(&fx.key).public()).expect("token verifies")
}

/// The capability data of the last block of `token`, as JSON to edit.
fn last_context(fx: &Fx, token: &CapToken) -> Value {
    let context = parsed(fx, token)
        .context()
        .pop()
        .flatten()
        .expect("last block has capability data");
    serde_json::from_str(&context).expect("capability data is JSON")
}

/// `token` plus a third-party block holding `context` and nothing else, signed by `signer`.
fn append_signed(fx: &Fx, token: &CapToken, signer: &PrivateKey, context: &Value) -> CapToken {
    let biscuit = parsed(fx, token);
    let block = biscuit
        .third_party_request()
        .expect("third-party request")
        .create_block(signer, BlockBuilder::new().context(context.to_string()))
        .expect("block signs");
    let appended = biscuit
        .append_third_party(signer.public(), block)
        .expect("block appends");
    CapToken::from_bytes(appended.to_vec().expect("token serializes"))
}

/// `context` re-bound to a fresh run, which is returned with it.
fn rebound(mut context: Value) -> (RunId, Value) {
    let run = RunId::new();
    context["run"] = serde_json::to_value(run).expect("run id serializes");
    (run, context)
}

/// A child of [`Fx::new`] with one tool plus `kestrel.spawn`, one host, $1 and depth 1.
fn narrow_child(fx: &Fx) -> (Capability, CapToken) {
    fx.child(|c| {
        c.tools = set(&["fs.read", "kestrel.spawn"]);
        c.hosts = set(&["crates.io"]);
        c.secrets = set(&["npm_token"]);
        c.spawn = Some(limits(2, 1, &["reviewer"]));
        c.budget = usd(1);
    })
}

fn every_action() -> Vec<Action> {
    vec![
        tool("fs.read"),
        tool("fs.write"),
        tool("kestrel.spawn"),
        host("crates.io"),
        host("api.github.com"),
        secret("npm_token"),
        secret("github_token"),
        spawn("reviewer"),
        spawn("test-writer"),
        Action::Spend(MicroUsd(0)),
        Action::Spend(usd(1)),
    ]
}

/// `token` is dead: `grant` and `attenuate` fail with `InvalidToken`, and every action is denied
/// for every one of `runs`.
#[track_caller]
fn assert_dead(fx: &Fx, token: &CapToken, runs: &[RunId], what: &str) {
    assert_invalid_token(caps::grant(token, &fx.pk), what);
    let nothing = fx.child_cap(|c| {
        c.tools.clear();
        c.spawn = None;
    });
    assert_invalid_token(caps::attenuate(token, &fx.key, &nothing), what);
    for run in runs {
        for a in every_action() {
            assert_deny(
                &fx.check(token, *run, a.clone()),
                &format!("{what}: {a:?} as {run}"),
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Only the root key attenuates.

#[test]
fn every_attenuation_block_is_a_third_party_block_signed_by_the_root_key() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    let g = narrowed(&child, |g| g.tools = set(&["fs.read"]));
    let g_token = caps::attenuate(&token, &fx.key, &g).expect("grandchild attenuates");
    let root = biscuit_key(&fx.key).public();
    assert_eq!(
        parsed(&fx, &g_token).external_public_keys(),
        vec![None, Some(root), Some(root)]
    );
}

#[test]
fn a_correctly_attenuated_grandchild_still_works() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    let g = narrowed(&child, |g| g.tools = set(&["fs.read"]));
    let g_token = caps::attenuate(&token, &fx.key, &g).expect("grandchild attenuates");
    assert_eq!(caps::grant(&g_token, &fx.pk).expect("verifies"), g);
    assert_allow(
        &fx.check(&g_token, g.run, tool("fs.read")),
        "grandchild fs.read",
    );
    assert_allow(
        &fx.check(&g_token, g.run, host("crates.io")),
        "grandchild host",
    );
    assert_deny_naming(&fx.check(&g_token, g.run, tool("fs.write")), "fs.write");
    for run in [child.run, fx.cap.run, RunId::new()] {
        assert_deny_naming(&fx.check(&g_token, run, tool("fs.read")), "run");
    }
}

#[test]
fn a_raw_block_without_any_key_cannot_rebind_the_token() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    // The old attack: copy the child's own data, name another run, append with no key at all.
    let (other, context) = rebound(last_context(&fx, &token));
    let forged = append_raw_block(&token, BlockBuilder::new().context(context.to_string()));
    assert_dead(
        &fx,
        &forged,
        &[other, child.run, fx.cap.run],
        "raw rebinding block",
    );
    // A raw block naming the child's own run, with no narrowing at all, is refused the same way.
    let same = append_raw_block(&token, replay_block(&token, 1));
    assert_dead(&fx, &same, &[child.run, fx.cap.run], "raw replayed block");
}

#[test]
fn a_third_party_block_signed_by_another_key_is_rejected() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    let (other, mut context) = rebound(last_context(&fx, &token));
    context["tools"] = serde_json::json!(["fs.read"]);
    context["spawn"] = Value::Null;
    for (what, signer) in [
        (
            "another Ed25519 key",
            PrivateKey::new_with_algorithm(Algorithm::Ed25519),
        ),
        (
            "a P-256 key",
            PrivateKey::new_with_algorithm(Algorithm::Secp256r1),
        ),
        (
            "another server's root key",
            biscuit_key(&RootKey::generate()),
        ),
    ] {
        let forged = append_signed(&fx, &token, &signer, &context);
        assert_dead(&fx, &forged, &[other, child.run, fx.cap.run], what);
    }
    // The same block signed by the root key is accepted: the signer is what was refused.
    let signed = append_signed(&fx, &token, &biscuit_key(&fx.key), &context);
    assert_eq!(caps::grant(&signed, &fx.pk).expect("verifies").run, other);
}

/// One change to a block's capability data.
type Edit = Box<dyn Fn(&mut Value)>;

#[test]
fn a_root_signed_block_that_widens_is_rejected() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    let root = biscuit_key(&fx.key);
    let later = expiry() + Duration::seconds(1);
    let edits: Vec<(&str, Edit)> = vec![
        (
            "a tool the child lacks",
            Box::new(|c| c["tools"] = serde_json::json!(["fs.read", "fs.write"])),
        ),
        (
            "a host the child lacks",
            Box::new(|c| c["hosts"] = serde_json::json!(["api.github.com"])),
        ),
        (
            "a secret the child lacks",
            Box::new(|c| c["secrets"] = serde_json::json!(["github_token"])),
        ),
        (
            "a bigger budget",
            Box::new(|c| c["budget"] = usd(2).0.into()),
        ),
        (
            "a later expiry",
            Box::new(move |c| c["expires_unix"] = later.unix_timestamp().into()),
        ),
        (
            "the same spawn depth",
            Box::new(|c| c["spawn"]["max_depth"] = 1.into()),
        ),
        (
            "more children",
            Box::new(|c| c["spawn"]["max_children"] = 3.into()),
        ),
        (
            "an agent the child lacks",
            Box::new(|c| c["spawn"]["agents"] = serde_json::json!(["test-writer"])),
        ),
    ];
    for (what, edit) in edits {
        let (other, mut context) = rebound(last_context(&fx, &token));
        context["spawn"]["max_depth"] = 0.into();
        edit(&mut context);
        let forged = append_signed(&fx, &token, &root, &context);
        assert_dead(&fx, &forged, &[other, child.run, fx.cap.run], what);
    }
}

#[test]
fn stripping_a_server_attenuated_block_still_fails() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    let g = narrowed(&child, |g| g.tools.clear());
    let g_token = caps::attenuate(&token, &fx.key, &g).expect("grandchild attenuates");
    let stripped = CapToken::from_bytes(strip_last_block(&g_token));
    assert_dead(&fx, &stripped, &[g.run, child.run, fx.cap.run], "stripped");
}

// ---------------------------------------------------------------------------------------------
// Block limit.

#[test]
fn a_token_holds_at_most_32_blocks() {
    let fx = Fx::from_spec(&spec_from("{ mcp: [fs.read] }", "1"));
    assert_eq!(fx.cap.spawn, None);
    let (mut cap, mut token) = (fx.cap.clone(), fx.token.clone());
    for _ in 1..32 {
        cap = narrowed(&cap, |_| {});
        token = caps::attenuate(&token, &fx.key, &cap).expect("attenuates");
    }
    assert_eq!(attenuation_blocks(token.as_bytes()), 31);
    assert_eq!(caps::grant(&token, &fx.pk).expect("32 blocks verify"), cap);
    assert_allow(&fx.check(&token, cap.run, tool("fs.read")), "32 blocks");

    let next = narrowed(&cap, |_| {});
    let refused = caps::attenuate(&token, &fx.key, &next);
    assert!(
        matches!(&refused, Err(CapError::Token(why)) if why.contains("32")),
        "{refused:?}"
    );

    // Built by hand with the root key, a 33rd block makes the token unreadable.
    let (other, context) = rebound(last_context(&fx, &token));
    let forged = append_signed(&fx, &token, &biscuit_key(&fx.key), &context);
    assert_eq!(attenuation_blocks(forged.as_bytes()), 32);
    assert_invalid_token(caps::grant(&forged, &fx.pk), "33 blocks");
    assert_deny(&fx.check(&forged, other, tool("fs.read")), "33 blocks");
}

// ---------------------------------------------------------------------------------------------
// Spawning needs `kestrel.spawn`.

#[test]
fn a_child_that_drops_the_spawn_tool_may_not_spawn() {
    let fx = Fx::new();
    let (child, token) = fx.child(|c| c.tools = set(&["fs.read"]));
    assert_eq!(child.spawn, Some(limits(4, 1, &AGENTS)));
    let d = fx.check(&token, child.run, spawn("reviewer"));
    assert_deny_naming(&d, "kestrel.spawn");
    // The parent, which lists the tool, still may.
    assert_allow(
        &fx.check(&fx.token, fx.cap.run, spawn("reviewer")),
        "parent",
    );
}

// ---------------------------------------------------------------------------------------------
// Spending.

#[test]
fn spending_a_negative_amount_is_denied_naming_it() {
    let fx = Fx::new();
    for amount in [MicroUsd(-1), MicroUsd(i64::MIN)] {
        let d = fx.check(&fx.token, fx.cap.run, Action::Spend(amount));
        assert_deny_naming(&d, &amount.to_string());
        assert!(fx.cap.allows(&Action::Spend(amount)).is_err(), "{amount}");
    }
}

// ---------------------------------------------------------------------------------------------
// `Capability::allows`.

#[test]
fn allows_refuses_the_spawn_tool_without_spawn_rights_even_when_listed() {
    let cap = Capability::from_spec(
        RunId::new(),
        &spec_from("{ mcp: [kestrel.spawn, fs.read] }", "1"),
        expiry(),
    );
    assert!(cap.tools.contains("kestrel.spawn"));
    let refused = cap.allows(&tool("kestrel.spawn"));
    assert!(
        matches!(&refused, Err(why) if why.contains("kestrel.spawn")),
        "{refused:?}"
    );
    assert_eq!(cap.allows(&tool("fs.read")), Ok(()));
}

#[test]
fn allows_needs_the_spawn_tool_spawn_depth_and_the_agent() {
    let with_tool =
        |caps: &str| Capability::from_spec(RunId::new(), &spec_from(caps, "1"), expiry());
    let ok = with_tool(
        "{ mcp: [kestrel.spawn], spawn: { max_children: 1, max_depth: 1, agents: [reviewer] } }",
    );
    assert_eq!(ok.allows(&spawn("reviewer")), Ok(()));
    assert_eq!(ok.allows(&tool("kestrel.spawn")), Ok(()));
    assert!(ok.allows(&spawn("deployer")).is_err());
    let no_tool = with_tool("{ spawn: { max_children: 1, max_depth: 1, agents: [reviewer] } }");
    let refused = no_tool.allows(&spawn("reviewer"));
    assert!(
        matches!(&refused, Err(why) if why.contains("kestrel.spawn")),
        "{refused:?}"
    );
    let no_depth = with_tool(
        "{ mcp: [kestrel.spawn], spawn: { max_children: 1, max_depth: 0, agents: [reviewer] } }",
    );
    assert!(no_depth.allows(&spawn("reviewer")).is_err());
    assert!(no_depth.allows(&tool("kestrel.spawn")).is_err());
}

#[test]
fn allows_agrees_with_verify_for_every_kind_of_action() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    let mut actions = every_action();
    actions.extend([
        Action::Spend(MicroUsd(-1)),
        Action::Spend(usd(2)),
        Action::Spend(MicroUsd(usd(2).0 + 1)),
        tool("shell.exec"),
        spawn("deployer"),
    ]);
    for (cap, token) in [(&fx.cap, &fx.token), (&child, &token)] {
        for a in &actions {
            let by_verify = fx.check(token, cap.run, a.clone()) == Decision::Allow;
            assert_eq!(
                cap.allows(a).is_ok(),
                by_verify,
                "{a:?} for {:?}",
                cap.tools
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// `CapToken` Debug.

#[test]
fn cap_token_debug_prints_only_the_length() {
    let fx = Fx::new();
    let bytes = fx.token.as_bytes();
    let expected = format!("CapToken {{ len: {} }}", bytes.len());
    assert_eq!(format!("{:?}", fx.token), expected);
    let pretty = format!("{:#?}", fx.token);
    assert!(pretty.contains(&bytes.len().to_string()), "{pretty}");
    for window in bytes.windows(4) {
        let list = window
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",");
        assert!(!pretty.replace([' ', '\n'], "").contains(&list), "{pretty}");
    }
}
