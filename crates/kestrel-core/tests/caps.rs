//! Capability tokens (issue #33), part 1: root grants from a spec, attenuation rules and spawn
//! limits. Scenarios 1 to 6 and 12, plus the root key and `grant` round trips.
//! Token tampering, expiry and run binding are in `caps_tokens.rs`; the property test is in
//! `caps_prop.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod caps_support;

use caps_support::*;
use kestrel_core::caps::{self, CapError, Capability, RootKey, SpawnLimits};
use kestrel_types::{MicroUsd, RunId};
use time::Duration;

// ---------------------------------------------------------------------------------------------
// Scenario 1: what the spec lists is allowed.

#[test]
fn s1_root_capability_mirrors_the_spec() {
    let run = RunId::new();
    let cap = Capability::from_spec(run, &root_spec(), expiry());
    assert_eq!(cap.run, run);
    assert_eq!(cap.tools, set(&TOOLS));
    assert_eq!(cap.hosts, set(&HOSTS));
    assert_eq!(cap.secrets, set(&SECRETS));
    assert_eq!(cap.spawn, Some(limits(4, 2, &AGENTS)));
    assert_eq!(cap.budget, usd(2));
    assert_eq!(cap.expires_at, expiry());
}

#[test]
fn s1_root_capability_from_a_spec_with_no_capabilities_is_empty() {
    let spec = spec_from("{}", "0.5");
    let cap = Capability::from_spec(RunId::new(), &spec, expiry());
    assert!(cap.tools.is_empty());
    assert!(cap.hosts.is_empty());
    assert!(cap.secrets.is_empty());
    assert_eq!(cap.spawn, None);
    assert_eq!(cap.budget, MicroUsd(500_000));
}

#[test]
fn s1_duplicate_spec_entries_collapse_into_one() {
    let spec = spec_from(
        "{ mcp: [fs.read, fs.read], network: [crates.io, crates.io], secrets: [k, k], \
           spawn: { max_children: 1, max_depth: 1, agents: [x, x] } }",
        "1",
    );
    let cap = Capability::from_spec(RunId::new(), &spec, expiry());
    assert_eq!(cap.tools, set(&["fs.read"]));
    assert_eq!(cap.hosts, set(&["crates.io"]));
    assert_eq!(cap.secrets, set(&["k"]));
    assert_eq!(cap.spawn, Some(limits(1, 1, &["x"])));
}

#[test]
fn s1_listed_tools_hosts_and_secrets_are_allowed() {
    let fx = Fx::new();
    let run = fx.cap.run;
    for t in TOOLS {
        assert_allow(&fx.check(&fx.token, run, tool(t)), t);
    }
    for h in HOSTS {
        assert_allow(&fx.check(&fx.token, run, host(h)), h);
    }
    for s in SECRETS {
        assert_allow(&fx.check(&fx.token, run, secret(s)), s);
    }
}

#[test]
fn s1_listed_agents_may_be_spawned_and_the_budget_may_be_spent() {
    let fx = Fx::new();
    let run = fx.cap.run;
    for a in AGENTS {
        assert_allow(&fx.check(&fx.token, run, spawn(a)), a);
    }
    assert_allow(
        &fx.check(&fx.token, run, caps::Action::Spend(MicroUsd(0))),
        "spending nothing",
    );
    assert_allow(
        &fx.check(&fx.token, run, caps::Action::Spend(usd(2))),
        "spending exactly the budget",
    );
}

#[test]
fn s1_names_with_quotes_and_spaces_round_trip_and_are_allowed() {
    // Names end up inside the token's Datalog; quoting must not break or widen anything.
    let spec = spec_from(
        r#"{ mcp: ['say "hi"', 'a) or true', 'two  spaces'], network: ['h"st'], secrets: ['s\x'] }"#,
        "1",
    );
    let fx = Fx::from_spec(&spec);
    let run = fx.cap.run;
    for t in ["say \"hi\"", "a) or true", "two  spaces"] {
        assert_allow(&fx.check(&fx.token, run, tool(t)), t);
    }
    assert_allow(&fx.check(&fx.token, run, host("h\"st")), "quoted host");
    assert_allow(
        &fx.check(&fx.token, run, secret("s\\x")),
        "backslash secret",
    );
    assert_deny(
        &fx.check(&fx.token, run, tool("say")),
        "prefix of a quoted name",
    );
    assert_deny(
        &fx.check(&fx.token, run, tool("a")),
        "prefix of a quoted name",
    );
    let granted = caps::grant(&fx.token, &fx.pk).expect("token verifies");
    assert_eq!(granted, fx.cap);
}

// ---------------------------------------------------------------------------------------------
// Scenario 2: what the spec does not list is denied, and the reason names it.

#[test]
fn s2_unlisted_tool_is_denied_naming_it() {
    let fx = Fx::new();
    let d = fx.check(&fx.token, fx.cap.run, tool("shell.exec"));
    assert_deny_naming(&d, "shell.exec");
}

#[test]
fn s2_unlisted_host_is_denied_naming_it() {
    let fx = Fx::new();
    let d = fx.check(&fx.token, fx.cap.run, host("evil.example.com"));
    assert_deny_naming(&d, "evil.example.com");
}

#[test]
fn s2_unlisted_secret_is_denied_naming_it() {
    let fx = Fx::new();
    let d = fx.check(&fx.token, fx.cap.run, secret("aws_root_key"));
    assert_deny_naming(&d, "aws_root_key");
}

#[test]
fn s2_near_misses_are_denied_exact_match_only() {
    let fx = Fx::new();
    let run = fx.cap.run;
    for t in ["fs", "fs.read.all", "FS.READ", "fs.read ", "", "*", "fs.*"] {
        assert_deny(&fx.check(&fx.token, run, tool(t)), &format!("tool {t:?}"));
    }
    for h in [
        "evil.api.github.com",
        "api.github.com.evil.io",
        "github.com",
        "*.github.com",
        "",
    ] {
        assert_deny(&fx.check(&fx.token, run, host(h)), &format!("host {h:?}"));
    }
    for s in ["github_token2", "github", "", "*"] {
        assert_deny(
            &fx.check(&fx.token, run, secret(s)),
            &format!("secret {s:?}"),
        );
    }
}

#[test]
fn s2_names_that_look_like_datalog_are_denied() {
    let fx = Fx::new();
    let run = fx.cap.run;
    for t in [
        "fs.read\") or true or (\"",
        "fs.write\"), tool(\"x",
        "\"",
        "\\",
        "x\n allow if true",
    ] {
        assert_deny(&fx.check(&fx.token, run, tool(t)), &format!("tool {t:?}"));
    }
}

#[test]
fn s2_a_name_granted_as_one_kind_is_not_granted_as_another() {
    let fx = Fx::new();
    let run = fx.cap.run;
    assert_deny_naming(&fx.check(&fx.token, run, host("fs.read")), "fs.read");
    assert_deny_naming(&fx.check(&fx.token, run, secret("fs.read")), "fs.read");
    assert_deny_naming(
        &fx.check(&fx.token, run, tool("api.github.com")),
        "api.github.com",
    );
    assert_deny_naming(
        &fx.check(&fx.token, run, tool("github_token")),
        "github_token",
    );
    assert_deny_naming(
        &fx.check(&fx.token, run, host("github_token")),
        "github_token",
    );
    assert_deny_naming(&fx.check(&fx.token, run, tool("reviewer")), "reviewer");
}

#[test]
fn s2_spending_over_the_budget_is_denied() {
    let fx = Fx::new();
    let over = MicroUsd(usd(2).0 + 1);
    assert_deny(
        &fx.check(&fx.token, fx.cap.run, caps::Action::Spend(over)),
        "one micro-dollar over the budget",
    );
}

#[test]
fn s2_a_spec_with_no_capabilities_denies_everything_but_spending() {
    let fx = Fx::from_spec(&spec_from("{}", "1"));
    let run = fx.cap.run;
    assert_deny_naming(&fx.check(&fx.token, run, tool("fs.read")), "fs.read");
    assert_deny_naming(&fx.check(&fx.token, run, host("crates.io")), "crates.io");
    assert_deny_naming(&fx.check(&fx.token, run, secret("k")), "k");
    assert_deny(&fx.check(&fx.token, run, spawn("reviewer")), "spawn");
    assert_allow(
        &fx.check(&fx.token, run, caps::Action::Spend(usd(1))),
        "spending the budget",
    );
}

// ---------------------------------------------------------------------------------------------
// Scenario 3: a child narrowed to {a} can use a and not b.

#[test]
fn s3_child_narrowed_to_one_tool_uses_it_and_not_the_other() {
    let fx = Fx::new();
    let (child, token) = fx.child(|c| c.tools = set(&["fs.read"]));
    assert_allow(&fx.check(&token, child.run, tool("fs.read")), "fs.read");
    assert_deny_naming(&fx.check(&token, child.run, tool("fs.write")), "fs.write");
    // The parent keeps both with its own token.
    assert_allow(
        &fx.check(&fx.token, fx.cap.run, tool("fs.write")),
        "parent fs.write",
    );
}

#[test]
fn s3_narrowing_hosts_and_secrets_works_the_same_way() {
    let fx = Fx::new();
    let (child, token) = fx.child(|c| {
        c.hosts = set(&["crates.io"]);
        c.secrets = set(&[]);
    });
    assert_allow(&fx.check(&token, child.run, host("crates.io")), "crates.io");
    assert_deny_naming(
        &fx.check(&token, child.run, host("api.github.com")),
        "api.github.com",
    );
    assert_deny_naming(
        &fx.check(&token, child.run, secret("github_token")),
        "github_token",
    );
    // Not narrowed, still allowed.
    assert_allow(&fx.check(&token, child.run, tool("fs.write")), "fs.write");
}

// ---------------------------------------------------------------------------------------------
// Scenario 4: a child can never gain a tool, host or secret.

#[test]
fn s4_child_cannot_gain_a_tool() {
    let fx = Fx::from_spec(&spec_from("{ mcp: [fs.read] }", "1"));
    let child = fx.child_cap(|c| c.tools = set(&["fs.read", "fs.write"]));
    assert_escalation_naming(child.check_subset_of(&fx.cap), "fs.write");
    assert_escalation_naming(caps::attenuate(&fx.token, &fx.pk, &child), "fs.write");
}

#[test]
fn s4_child_cannot_gain_a_host() {
    let fx = Fx::from_spec(&spec_from("{ network: [crates.io] }", "1"));
    let child = fx.child_cap(|c| c.hosts = set(&["crates.io", "evil.example.com"]));
    assert_escalation_naming(child.check_subset_of(&fx.cap), "evil.example.com");
    assert_escalation_naming(
        caps::attenuate(&fx.token, &fx.pk, &child),
        "evil.example.com",
    );
}

#[test]
fn s4_child_cannot_gain_a_secret() {
    let fx = Fx::from_spec(&spec_from("{ secrets: [npm_token] }", "1"));
    let child = fx.child_cap(|c| c.secrets = set(&["npm_token", "aws_root_key"]));
    assert_escalation_naming(child.check_subset_of(&fx.cap), "aws_root_key");
    assert_escalation_naming(caps::attenuate(&fx.token, &fx.pk, &child), "aws_root_key");
}

#[test]
fn s4_escalation_error_text_says_escalation() {
    let fx = Fx::from_spec(&spec_from("{ mcp: [fs.read] }", "1"));
    let child = fx.child_cap(|c| c.tools = set(&["fs.write"]));
    let text = escalation(caps::attenuate(&fx.token, &fx.pk, &child));
    assert!(
        text.starts_with("capability escalation: "),
        "unexpected text {text:?}"
    );
}

#[test]
fn s4_a_tool_dropped_by_the_child_cannot_come_back_in_the_grandchild() {
    let fx = Fx::new();
    let (child, child_token) = fx.child(|c| c.tools = set(&["fs.read"]));
    let grandchild = narrowed(&child, |g| g.tools = set(&TOOLS));
    assert_escalation_naming(grandchild.check_subset_of(&child), "fs.write");
    assert_escalation_naming(
        caps::attenuate(&child_token, &fx.pk, &grandchild),
        "fs.write",
    );
}

#[test]
fn s4_subset_check_accepts_equal_and_smaller_and_ignores_the_run() {
    let fx = Fx::new();
    // Equal sets, budget and expiry; spawn depth one less, as scenario 6 requires.
    let same = fx.child_cap(|_| {});
    assert_ne!(same.run, fx.cap.run);
    assert_eq!(same.tools, fx.cap.tools);
    assert_eq!(same.spawn, Some(limits(4, 1, &AGENTS)));
    assert!(same.check_subset_of(&fx.cap).is_ok(), "equal rights");
    // A capability compared with itself keeps its spawn depth, which a child may not.
    assert!(matches!(
        fx.cap.check_subset_of(&fx.cap),
        Err(CapError::Escalation(_))
    ));
    let no_spawn = Capability {
        spawn: None,
        ..fx.cap.clone()
    };
    assert!(
        no_spawn.check_subset_of(&no_spawn).is_ok(),
        "without spawn, a capability is a subset of itself"
    );
    let empty = fx.child_cap(|c| {
        c.tools.clear();
        c.hosts.clear();
        c.secrets.clear();
        c.spawn = None;
        c.budget = MicroUsd(0);
    });
    assert!(empty.check_subset_of(&fx.cap).is_ok(), "no rights at all");
    assert!(
        caps::attenuate(&fx.token, &fx.pk, &empty).is_ok(),
        "attenuate to nothing"
    );
}

#[test]
fn s4_attenuating_to_equal_rights_works() {
    let fx = Fx::new();
    let (child, token) = fx.child(|_| {});
    for t in TOOLS {
        assert_allow(&fx.check(&token, child.run, tool(t)), t);
    }
}

// ---------------------------------------------------------------------------------------------
// Scenario 5: the budget ceiling can only shrink.

#[test]
fn s5_child_budget_above_the_parent_fails() {
    let fx = Fx::new();
    assert_eq!(fx.cap.budget, usd(2));
    let child = fx.child_cap(|c| c.budget = usd(3));
    assert_escalation_naming(child.check_subset_of(&fx.cap), "budget");
    assert_escalation_naming(caps::attenuate(&fx.token, &fx.pk, &child), "budget");
}

#[test]
fn s5_child_budget_one_micro_dollar_above_the_parent_fails() {
    let fx = Fx::new();
    let child = fx.child_cap(|c| c.budget = MicroUsd(usd(2).0 + 1));
    assert_escalation_naming(caps::attenuate(&fx.token, &fx.pk, &child), "budget");
}

#[test]
fn s5_child_budget_below_or_equal_to_the_parent_works() {
    let fx = Fx::new();
    for b in [usd(1), usd(2), MicroUsd(0)] {
        let child = fx.child_cap(|c| c.budget = b);
        assert!(child.check_subset_of(&fx.cap).is_ok(), "budget {b:?}");
        assert!(
            caps::attenuate(&fx.token, &fx.pk, &child).is_ok(),
            "budget {b:?}"
        );
    }
}

#[test]
fn s5_child_spending_is_capped_by_the_child_budget() {
    let fx = Fx::new();
    let (child, token) = fx.child(|c| c.budget = usd(1));
    assert_allow(
        &fx.check(&token, child.run, caps::Action::Spend(usd(1))),
        "child spends its $1",
    );
    assert_deny(
        &fx.check(
            &token,
            child.run,
            caps::Action::Spend(MicroUsd(usd(1).0 + 1)),
        ),
        "child spends past its $1",
    );
    assert_allow(
        &fx.check(&fx.token, fx.cap.run, caps::Action::Spend(usd(2))),
        "parent spends its $2",
    );
}

// ---------------------------------------------------------------------------------------------
// Scenario 6: spawn depth, children, agents and expiry can only shrink.

#[test]
fn s6_child_gets_at_most_parent_depth_minus_one() {
    let fx = Fx::new();
    assert_eq!(fx.cap.spawn.as_ref().map(|s| s.max_depth), Some(2));
    let too_deep = fx.child_cap(|c| c.spawn = Some(limits(4, 2, &AGENTS)));
    assert_escalation_naming(too_deep.check_subset_of(&fx.cap), "depth");
    assert_escalation_naming(caps::attenuate(&fx.token, &fx.pk, &too_deep), "depth");
    for depth in [1, 0] {
        let ok = fx.child_cap(|c| c.spawn = Some(limits(4, depth, &AGENTS)));
        assert!(
            caps::attenuate(&fx.token, &fx.pk, &ok).is_ok(),
            "child max_depth {depth}"
        );
    }
}

#[test]
fn s6_grandchild_cannot_ask_for_depth_two_or_one() {
    let fx = Fx::new();
    let (child, child_token) = fx.child(|c| c.spawn = Some(limits(4, 1, &AGENTS)));
    for depth in [1, 2, 3] {
        let g = narrowed(&child, |g| g.spawn = Some(limits(4, depth, &AGENTS)));
        assert_escalation_naming(g.check_subset_of(&child), "depth");
        assert_escalation_naming(caps::attenuate(&child_token, &fx.pk, &g), "depth");
    }
    let g = narrowed(&child, |g| g.spawn = Some(limits(4, 0, &AGENTS)));
    let g_token = caps::attenuate(&child_token, &fx.pk, &g).expect("grandchild at depth 0");
    let none = narrowed(&child, |g| g.spawn = None);
    assert!(caps::attenuate(&child_token, &fx.pk, &none).is_ok());

    // The child (depth 1) may spawn; the grandchild (depth 0) may not.
    assert_allow(
        &fx.check(&child_token, child.run, spawn("test-writer")),
        "child spawns",
    );
    assert_deny(
        &fx.check(&g_token, g.run, spawn("test-writer")),
        "grandchild at depth 0 spawns",
    );
}

#[test]
fn s6_a_parent_that_cannot_spawn_cannot_give_a_child_spawn() {
    let fx = Fx::from_spec(&spec_from("{ mcp: [fs.read] }", "1"));
    assert_eq!(fx.cap.spawn, None);
    let child = fx.child_cap(|c| c.spawn = Some(limits(1, 0, &[])));
    assert_escalation_naming(child.check_subset_of(&fx.cap), "spawn");
    assert_escalation_naming(caps::attenuate(&fx.token, &fx.pk, &child), "spawn");
}

#[test]
fn s6_a_parent_at_depth_zero_cannot_give_a_child_spawn_even_at_depth_zero() {
    let parent = Capability::from_spec(
        RunId::new(),
        &spec_from(
            "{ spawn: { max_children: 2, max_depth: 0, agents: [reviewer] } }",
            "1",
        ),
        expiry(),
    );
    let child = narrowed(&parent, |c| c.spawn = Some(limits(2, 0, &["reviewer"])));
    assert!(matches!(
        child.check_subset_of(&parent),
        Err(CapError::Escalation(_))
    ));
    let none = narrowed(&parent, |c| c.spawn = None);
    assert!(none.check_subset_of(&parent).is_ok());
}

#[test]
fn s6_child_agents_must_be_a_subset() {
    let fx = Fx::new();
    let child = fx.child_cap(|c| c.spawn = Some(limits(4, 1, &["reviewer", "deployer"])));
    assert_escalation_naming(child.check_subset_of(&fx.cap), "deployer");
    assert_escalation_naming(caps::attenuate(&fx.token, &fx.pk, &child), "deployer");
    let (narrow, token) = fx.child(|c| c.spawn = Some(limits(4, 1, &["reviewer"])));
    assert_allow(
        &fx.check(&token, narrow.run, spawn("reviewer")),
        "kept agent",
    );
    assert_deny_naming(
        &fx.check(&token, narrow.run, spawn("test-writer")),
        "test-writer",
    );
}

#[test]
fn s6_max_children_cannot_grow() {
    let fx = Fx::new();
    let child = fx.child_cap(|c| c.spawn = Some(limits(5, 1, &AGENTS)));
    assert_escalation_naming(child.check_subset_of(&fx.cap), "children");
    assert_escalation_naming(caps::attenuate(&fx.token, &fx.pk, &child), "children");
    for n in [4, 1, 0] {
        let ok = fx.child_cap(|c| c.spawn = Some(limits(n, 1, &AGENTS)));
        assert!(
            caps::attenuate(&fx.token, &fx.pk, &ok).is_ok(),
            "max_children {n}"
        );
    }
}

#[test]
fn s6_expiry_cannot_be_later_than_the_parent() {
    let fx = Fx::new();
    let later = fx.child_cap(|c| c.expires_at = expiry() + Duration::seconds(1));
    assert_escalation_naming(later.check_subset_of(&fx.cap), "expire");
    assert_escalation_naming(caps::attenuate(&fx.token, &fx.pk, &later), "expire");
    for at in [expiry(), expiry() - Duration::minutes(30)] {
        let ok = fx.child_cap(|c| c.expires_at = at);
        assert!(
            caps::attenuate(&fx.token, &fx.pk, &ok).is_ok(),
            "expiry {at}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Scenario 12: `spawn: None` denies `kestrel.spawn`.

#[test]
fn s12_spec_without_spawn_denies_spawn() {
    let fx = Fx::from_spec(&spec_from("{ mcp: [fs.read] }", "1"));
    assert_eq!(fx.cap.spawn, None);
    for a in ["reviewer", "test-writer", ""] {
        assert_deny(
            &fx.check(&fx.token, fx.cap.run, spawn(a)),
            &format!("spawn {a:?}"),
        );
    }
    assert_deny_naming(&fx.check(&fx.token, fx.cap.run, spawn("reviewer")), "spawn");
    // The spec does not list the tool either, so the tool call is denied too.
    assert_deny_naming(
        &fx.check(&fx.token, fx.cap.run, tool("kestrel.spawn")),
        "kestrel.spawn",
    );
}

#[test]
fn s12_spawn_of_an_agent_not_listed_is_denied_naming_it() {
    let fx = Fx::new();
    assert_deny_naming(
        &fx.check(&fx.token, fx.cap.run, spawn("deployer")),
        "deployer",
    );
}

#[test]
fn s12_spawn_at_max_depth_zero_is_denied() {
    let fx = Fx::from_spec(&spec_from(
        "{ spawn: { max_children: 3, max_depth: 0, agents: [reviewer] } }",
        "1",
    ));
    assert_eq!(fx.cap.spawn, Some(limits(3, 0, &["reviewer"])));
    assert_deny(
        &fx.check(&fx.token, fx.cap.run, spawn("reviewer")),
        "spawn at depth 0",
    );
}

#[test]
fn s12_spawn_with_an_empty_agent_list_is_denied() {
    let fx = Fx::from_spec(&spec_from(
        "{ spawn: { max_children: 3, max_depth: 2 } }",
        "1",
    ));
    assert_eq!(fx.cap.spawn, Some(limits(3, 2, &[])));
    assert_deny(
        &fx.check(&fx.token, fx.cap.run, spawn("reviewer")),
        "fail closed on no agents",
    );
}

#[test]
fn s12_spawn_of_a_listed_agent_with_depth_left_is_allowed() {
    let fx = Fx::from_spec(&spec_from(
        "{ spawn: { max_children: 1, max_depth: 1, agents: [reviewer] } }",
        "1",
    ));
    assert_allow(
        &fx.check(&fx.token, fx.cap.run, spawn("reviewer")),
        "spawn reviewer",
    );
}

// ---------------------------------------------------------------------------------------------
// Root key and `grant`.

#[test]
fn root_key_round_trips_through_bytes() {
    let key = RootKey::generate();
    let cap = Capability::from_spec(RunId::new(), &root_spec(), expiry());
    let before = caps::issue(&key, &cap).expect("issues");

    let reloaded = RootKey::from_bytes(&key.to_bytes()).expect("key reloads");
    assert_eq!(reloaded.public(), key.public());
    assert_eq!(reloaded.to_bytes(), key.to_bytes());
    assert_allow(
        &check(&before, &reloaded.public(), cap.run, tool("fs.read")),
        "old token, reloaded key",
    );
    let after = caps::issue(&reloaded, &cap).expect("issues with reloaded key");
    assert_allow(
        &check(&after, &key.public(), cap.run, tool("fs.read")),
        "new token, original key",
    );
}

#[test]
fn root_key_from_bad_bytes_is_an_error() {
    for bytes in [&[][..], &[1, 2, 3][..]] {
        assert!(RootKey::from_bytes(bytes).is_err(), "{} bytes", bytes.len());
    }
}

#[test]
fn generated_root_keys_differ() {
    let a = RootKey::generate();
    let b = RootKey::generate();
    assert_ne!(a.to_bytes(), b.to_bytes());
    assert_ne!(a.public(), b.public());
}

#[test]
fn root_key_debug_does_not_print_the_private_key() {
    let key = RootKey::generate();
    let bytes = key.to_bytes();
    assert!(bytes.len() >= 16, "private key is {} bytes", bytes.len());
    let debug = format!("{key:?} {key:#?}");
    assert!(!debug.contains(&hex(&bytes, false)), "lower hex in {debug}");
    assert!(!debug.contains(&hex(&bytes, true)), "upper hex in {debug}");
    // Nor any 8-byte window of it, in hex or as a Debug byte list.
    for window in bytes.windows(8) {
        assert!(
            !debug.contains(&hex(window, false)),
            "hex window in {debug}"
        );
        let list = window
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        assert!(!debug.contains(&list), "byte list window in {debug}");
    }
}

#[test]
fn grant_of_a_root_token_is_the_issued_capability() {
    let fx = Fx::new();
    assert_eq!(caps::grant(&fx.token, &fx.pk).expect("verifies"), fx.cap);
}

#[test]
fn grant_of_an_attenuated_token_is_the_child_capability() {
    let fx = Fx::new();
    let (child, token) = fx.child(|c| {
        c.tools = set(&["fs.read"]);
        c.hosts = set(&["crates.io"]);
        c.secrets = set(&[]);
        c.spawn = Some(SpawnLimits {
            max_children: 2,
            max_depth: 1,
            agents: set(&["reviewer"]),
        });
        c.budget = MicroUsd(1_250_000);
        c.expires_at = expiry() - Duration::minutes(10);
    });
    assert_eq!(caps::grant(&token, &fx.pk).expect("verifies"), child);

    let grandchild = narrowed(&child, |g| {
        g.tools.clear();
        g.spawn = None;
        g.budget = MicroUsd(1);
    });
    let g_token = caps::attenuate(&token, &fx.pk, &grandchild).expect("grandchild");
    assert_eq!(caps::grant(&g_token, &fx.pk).expect("verifies"), grandchild);
}

#[test]
fn cap_token_bytes_round_trip() {
    let fx = Fx::new();
    let copy = caps::CapToken::from_bytes(fx.token.as_bytes().to_vec());
    assert_eq!(copy, fx.token);
    assert_allow(
        &fx.check(&copy, fx.cap.run, tool("fs.read")),
        "copied token",
    );
}
