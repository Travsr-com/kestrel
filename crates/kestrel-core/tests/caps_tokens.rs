//! Capability tokens (issue #33), part 2: tampering, stripped and forged blocks, expiry and run
//! binding. Scenarios 7 to 10.
//!
//! Scenario 8 decision (from the contract): removing the attenuation block from a child token
//! makes the token FAIL verification for everyone. Biscuit's proof is the private key of the
//! last block's `next_key`; the child token only carries the child's key, so without the child's
//! block the proof no longer matches. The parent's rights stay with the holder of the parent
//! token, whose own token keeps working.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod caps_support;

use biscuit_auth::BlockBuilder;
use caps_support::*;
use kestrel_core::caps::{self, Action, CapToken, Decision, RootKey};
use kestrel_types::{MicroUsd, RunId};
use time::Duration;

/// A child of [`Fx::new`] narrowed to one tool (plus `kestrel.spawn`, which spawning needs), one
/// host, one secret, $1 and depth 1.
fn narrow_child(fx: &Fx) -> (kestrel_core::caps::Capability, CapToken) {
    fx.child(|c| {
        c.tools = set(&["fs.read", "kestrel.spawn"]);
        c.hosts = set(&["crates.io"]);
        c.secrets = set(&["npm_token"]);
        c.spawn = Some(limits(2, 1, &["reviewer"]));
        c.budget = usd(1);
    })
}

/// Every kind of action, each one granted to [`narrow_child`].
fn child_actions() -> Vec<Action> {
    vec![
        tool("fs.read"),
        host("crates.io"),
        secret("npm_token"),
        spawn("reviewer"),
        Action::Spend(usd(1)),
    ]
}

/// Every kind of action, each one granted to the root of [`Fx::new`] but not to [`narrow_child`].
fn parent_only_actions() -> Vec<Action> {
    vec![
        tool("fs.write"),
        host("api.github.com"),
        secret("github_token"),
        spawn("test-writer"),
        Action::Spend(usd(2)),
    ]
}

/// Positions to flip in a token of `len` bytes: start, middle, end and about 40 spread between.
fn flip_positions(len: usize) -> Vec<usize> {
    let step = (len / 40).max(1);
    let mut positions: Vec<usize> = (0..len).step_by(step).collect();
    positions.extend([0, 1, len / 2, len - 2, len - 1]);
    positions.sort_unstable();
    positions.dedup();
    positions
}

// ---------------------------------------------------------------------------------------------
// Scenario 7: a changed byte or a foreign key fails.

#[test]
fn s7_sanity_the_untouched_child_token_is_allowed() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    for a in child_actions() {
        assert_allow(&fx.check(&token, child.run, a.clone()), &format!("{a:?}"));
    }
}

#[test]
fn s7_one_changed_byte_anywhere_is_denied() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    let bytes = token.as_bytes().to_vec();
    assert!(bytes.len() > 64, "token is only {} bytes", bytes.len());
    for pos in flip_positions(bytes.len()) {
        for mask in [0x01u8, 0x80, 0xff] {
            let mut tampered = bytes.clone();
            tampered[pos] ^= mask;
            let tampered = CapToken::from_bytes(tampered);
            let what = format!("byte {pos} of {} xor {mask:#04x}", bytes.len());
            assert_deny(&fx.check(&tampered, child.run, tool("fs.read")), &what);
            assert_deny(&fx.check(&tampered, fx.cap.run, tool("fs.write")), &what);
            assert_invalid_token(caps::grant(&tampered, &fx.pk), &what);
            let grandchild = narrowed(&child, |g| g.tools.clear());
            assert_invalid_token(caps::attenuate(&tampered, &fx.key, &grandchild), &what);
        }
    }
}

#[test]
fn s7_one_changed_byte_in_a_root_token_is_denied() {
    let fx = Fx::new();
    let bytes = fx.token.as_bytes().to_vec();
    for pos in [0, bytes.len() / 2, bytes.len() - 1] {
        let mut tampered = bytes.clone();
        tampered[pos] ^= 0x01;
        let tampered = CapToken::from_bytes(tampered);
        assert_deny(
            &fx.check(&tampered, fx.cap.run, tool("fs.read")),
            &format!("root byte {pos}"),
        );
    }
}

#[test]
fn s7_truncated_empty_and_garbage_tokens_are_denied() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    let bytes = token.as_bytes();
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("one byte", bytes[..1].to_vec()),
        ("first half", bytes[..bytes.len() / 2].to_vec()),
        ("all but the last byte", bytes[..bytes.len() - 1].to_vec()),
        ("one extra byte", [bytes, &[0u8][..]].concat()),
        ("garbage", vec![0xa5; 300]),
        ("text", b"not a token".to_vec()),
    ];
    for (what, b) in cases {
        let t = CapToken::from_bytes(b);
        assert_deny(&fx.check(&t, child.run, tool("fs.read")), what);
        assert_invalid_token(caps::grant(&t, &fx.pk), what);
    }
}

#[test]
fn s7_a_token_signed_by_another_key_is_rejected() {
    let fx = Fx::new();
    let other = RootKey::generate();
    let forged = caps::issue(&other, &fx.cap).expect("issues with another key");
    for a in parent_only_actions().into_iter().chain(child_actions()) {
        assert_deny(&fx.check(&forged, fx.cap.run, a.clone()), &format!("{a:?}"));
    }
    assert_invalid_token(caps::grant(&forged, &fx.pk), "grant");
    let child = fx.child_cap(|c| c.tools = set(&["fs.read"]));
    assert_invalid_token(caps::attenuate(&forged, &fx.key, &child), "attenuate");
    // Still a good token for its own key.
    assert_allow(
        &check(&forged, &other.public(), fx.cap.run, tool("fs.read")),
        "own key",
    );
}

#[test]
fn s7_a_child_of_a_token_signed_by_another_key_is_rejected() {
    let fx = Fx::new();
    let other = Fx::issued(RootKey::generate(), fx.cap.clone());
    let (child, token) = narrow_child(&other);
    assert_deny(
        &fx.check(&token, child.run, tool("fs.read")),
        "foreign child",
    );
    assert_invalid_token(caps::grant(&token, &fx.pk), "grant");
}

// ---------------------------------------------------------------------------------------------
// Scenario 8: stripping the attenuation block fails; the parent's rights stay with the parent
// token holder.

#[test]
fn s8_sanity_the_strip_helper_removes_exactly_one_block() {
    let fx = Fx::new();
    let (_, token) = narrow_child(&fx);
    assert_eq!(attenuation_blocks(fx.token.as_bytes()), 0);
    assert_eq!(attenuation_blocks(token.as_bytes()), 1);
    let stripped = strip_last_block(&token);
    // Still a well-formed biscuit with the parent's block layout: only the proof is wrong.
    assert_eq!(attenuation_blocks(&stripped), 0);
    assert_ne!(stripped, fx.token.as_bytes());
}

#[test]
fn s8_a_child_token_with_its_block_stripped_fails_for_everyone() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    let stripped = CapToken::from_bytes(strip_last_block(&token));
    for a in parent_only_actions().into_iter().chain(child_actions()) {
        assert_deny(
            &fx.check(&stripped, child.run, a.clone()),
            &format!("{a:?} as the child run"),
        );
        assert_deny(
            &fx.check(&stripped, fx.cap.run, a.clone()),
            &format!("{a:?} as the parent run"),
        );
    }
    assert_invalid_token(caps::grant(&stripped, &fx.pk), "grant");
    let again = fx.child_cap(|c| c.tools = set(&["fs.write"]));
    assert_invalid_token(caps::attenuate(&stripped, &fx.key, &again), "attenuate");
}

#[test]
fn s8_stripping_and_resealing_with_the_child_key_also_fails() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    let sealed = CapToken::from_bytes(strip_last_block_and_seal(&token));
    assert_deny(
        &fx.check(&sealed, fx.cap.run, tool("fs.write")),
        "parent run",
    );
    assert_deny(&fx.check(&sealed, child.run, tool("fs.write")), "child run");
    assert_invalid_token(caps::grant(&sealed, &fx.pk), "grant");
}

#[test]
fn s8_removing_any_block_of_a_grandchild_token_fails() {
    let fx = Fx::new();
    let (child, child_token) = narrow_child(&fx);
    let g = narrowed(&child, |g| g.tools.clear());
    let g_token = caps::attenuate(&child_token, &fx.key, &g).expect("grandchild");
    assert_eq!(attenuation_blocks(g_token.as_bytes()), 2);
    for index in [0, 1] {
        let t = CapToken::from_bytes(remove_block(&g_token, index));
        let what = format!("block {index} removed");
        assert_deny(&fx.check(&t, g.run, tool("fs.read")), &what);
        assert_deny(&fx.check(&t, child.run, tool("fs.read")), &what);
        assert_deny(&fx.check(&t, fx.cap.run, tool("fs.write")), &what);
        assert_invalid_token(caps::grant(&t, &fx.pk), &what);
    }
}

#[test]
fn s8_the_parent_token_keeps_the_parent_rights_for_the_parent_run() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    for a in parent_only_actions().into_iter().chain(child_actions()) {
        assert_allow(
            &fx.check(&fx.token, fx.cap.run, a.clone()),
            &format!("parent {a:?}"),
        );
    }
    // And the child token alone never carries them.
    for a in parent_only_actions() {
        assert_deny(
            &fx.check(&token, child.run, a.clone()),
            &format!("child {a:?}"),
        );
        assert_deny(
            &fx.check(&token, fx.cap.run, a.clone()),
            &format!("child token as parent run {a:?}"),
        );
    }
}

// Not a numbered scenario, but the reason scenarios 4 and 8 hold: anyone can append a block with
// the raw Biscuit API. Only the root key may attenuate, so such a block makes the token invalid:
// it is denied for every run and grants nothing, so it can neither widen nor re-bind the token.

/// `forged` is dead: `grant` fails and every action is denied for the child, the parent and an
/// unrelated run.
#[track_caller]
fn assert_dead(fx: &Fx, child: &kestrel_core::caps::Capability, forged: &CapToken) {
    assert_invalid_token(caps::grant(forged, &fx.pk), "grant");
    for run in [child.run, fx.cap.run, RunId::new()] {
        for a in parent_only_actions().into_iter().chain(child_actions()) {
            assert_deny(
                &fx.check(forged, run, a.clone()),
                &format!("{a:?} as {run}"),
            );
        }
    }
}

#[test]
fn s4_a_raw_empty_block_appended_by_the_holder_makes_the_token_invalid() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    let forged = append_raw_block(&token, BlockBuilder::new());
    assert_dead(&fx, &child, &forged);
}

#[test]
fn s4_a_raw_block_replaying_the_root_block_makes_the_token_invalid() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    // The replayed block names the parent's run; it must not re-bind the token to it.
    let forged = append_raw_block(&token, replay_block(&token, 0));
    assert_dead(&fx, &child, &forged);
}

#[test]
fn s4_a_raw_block_with_a_junk_context_makes_the_token_invalid() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    let forged = append_raw_block(
        &token,
        BlockBuilder::new().context("{\"tools\":[\"fs.write\"]} not json".to_string()),
    );
    assert_dead(&fx, &child, &forged);
}

// ---------------------------------------------------------------------------------------------
// Scenario 9: an expired token denies everything.

fn every_root_action() -> Vec<Action> {
    vec![
        tool("fs.read"),
        host("crates.io"),
        secret("github_token"),
        spawn("reviewer"),
        Action::Spend(MicroUsd(0)),
        Action::Spend(usd(1)),
    ]
}

#[test]
fn s9_every_request_is_allowed_just_before_expiry() {
    let fx = Fx::new();
    let now = expiry() - Duration::seconds(1);
    for a in every_root_action() {
        let d = caps::verify(&fx.token, &fx.pk, &req_at(fx.cap.run, a.clone(), now));
        assert_allow(&d, &format!("{a:?} one second before expiry"));
    }
}

#[test]
fn s9_every_request_is_denied_at_and_after_expiry() {
    let fx = Fx::new();
    for now in [
        expiry(),
        expiry() + Duration::seconds(1),
        expiry() + Duration::days(365),
    ] {
        for a in every_root_action() {
            let d = caps::verify(&fx.token, &fx.pk, &req_at(fx.cap.run, a.clone(), now));
            assert_deny_naming(&d, "expired");
        }
    }
}

#[test]
fn s9_a_token_issued_already_expired_denies_everything() {
    let key = RootKey::generate();
    let cap = kestrel_core::caps::Capability::from_spec(RunId::new(), &root_spec(), t0());
    let fx = Fx::issued(key, cap);
    for a in every_root_action() {
        assert_deny_naming(&fx.check(&fx.token, fx.cap.run, a), "expired");
    }
}

#[test]
fn s9_a_child_expires_at_its_own_earlier_expiry() {
    let fx = Fx::new();
    let child_expiry = expiry() - Duration::minutes(30);
    let (child, token) = fx.child(|c| c.expires_at = child_expiry);
    let before = child_expiry - Duration::seconds(1);
    for a in every_root_action() {
        let ok = caps::verify(&token, &fx.pk, &req_at(child.run, a.clone(), before));
        assert_allow(&ok, &format!("child {a:?} before its expiry"));
        let late = caps::verify(&token, &fx.pk, &req_at(child.run, a.clone(), child_expiry));
        assert_deny_naming(&late, "expired");
        // The parent is still inside its own window at that instant.
        let parent = caps::verify(
            &fx.token,
            &fx.pk,
            &req_at(fx.cap.run, a.clone(), child_expiry),
        );
        assert_allow(&parent, &format!("parent {a:?} at the child's expiry"));
    }
}

#[test]
fn s9_a_child_cannot_outlive_its_parent() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    assert_eq!(child.expires_at, expiry());
    let d = caps::verify(
        &token,
        &fx.pk,
        &req_at(child.run, tool("fs.read"), expiry()),
    );
    assert_deny_naming(&d, "expired");
}

// ---------------------------------------------------------------------------------------------
// Scenario 10: a token only works for the run it was issued to.

#[test]
fn s10_a_child_token_works_for_the_child_run_only() {
    let fx = Fx::new();
    let (child, token) = narrow_child(&fx);
    assert_allow(&fx.check(&token, child.run, tool("fs.read")), "child run");
    let stranger = RunId::new();
    for (who, run) in [("parent run", fx.cap.run), ("unrelated run", stranger)] {
        for a in child_actions() {
            let d = fx.check(&token, run, a.clone());
            assert_deny(&d, &format!("{a:?} as the {who}"));
        }
        assert_deny_naming(&fx.check(&token, run, tool("fs.read")), "run");
    }
}

#[test]
fn s10_a_root_token_works_for_the_root_run_only() {
    let fx = Fx::new();
    let (child, _) = narrow_child(&fx);
    for run in [child.run, RunId::new()] {
        assert_deny_naming(&fx.check(&fx.token, run, tool("fs.read")), "run");
    }
}

#[test]
fn s10_a_grandchild_token_works_for_the_grandchild_run_only() {
    let fx = Fx::new();
    let (child, child_token) = narrow_child(&fx);
    let g = narrowed(&child, |_| {});
    let g_token = caps::attenuate(&child_token, &fx.key, &g).expect("grandchild");
    assert_allow(
        &fx.check(&g_token, g.run, tool("fs.read")),
        "grandchild run",
    );
    for run in [child.run, fx.cap.run, RunId::new()] {
        assert_deny(&fx.check(&g_token, run, tool("fs.read")), "other run");
    }
}

#[test]
fn s10_deny_for_a_wrong_run_is_a_deny_not_an_allow_even_when_expired() {
    // Both reasons apply; either may be reported, but it must be a deny.
    let fx = Fx::new();
    let d = caps::verify(
        &fx.token,
        &fx.pk,
        &req_at(RunId::new(), tool("fs.read"), expiry()),
    );
    assert!(matches!(d, Decision::Deny(_)), "got {d:?}");
}
