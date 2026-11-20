//! Capability tokens (issue #33): the `kestrel.spawn` tool needs spawn rights as well as being
//! listed. A spec with `spawn: None` denies `kestrel.spawn` even when `mcp` lists it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod caps_support;

use caps_support::*;
use kestrel_core::caps::{self, SPAWN_TOOL};

#[test]
fn the_spawn_tool_constant_is_kestrel_spawn() {
    assert_eq!(SPAWN_TOOL, "kestrel.spawn");
}

#[test]
fn listed_spawn_tool_without_spawn_limits_is_denied_naming_spawn() {
    let fx = Fx::from_spec(&spec_from("{ mcp: [kestrel.spawn, fs.read] }", "1"));
    assert_eq!(fx.cap.spawn, None);
    let d = fx.check(&fx.token, fx.cap.run, tool("kestrel.spawn"));
    assert_deny_naming(&d, "spawn");
    assert_deny_naming(&d, "kestrel.spawn");
    // The other listed tool is unaffected, and the grant still lists the tool as the spec says.
    assert_allow(&fx.check(&fx.token, fx.cap.run, tool("fs.read")), "fs.read");
    let granted = caps::grant(&fx.token, &fx.pk).expect("verifies");
    assert!(granted.tools.contains("kestrel.spawn"));
}

#[test]
fn listed_spawn_tool_with_spawn_limits_is_allowed() {
    let fx = Fx::from_spec(&spec_from(
        "{ mcp: [kestrel.spawn], spawn: { max_children: 1, max_depth: 1, agents: [reviewer] } }",
        "1",
    ));
    assert_allow(
        &fx.check(&fx.token, fx.cap.run, tool("kestrel.spawn")),
        "kestrel.spawn with spawn limits",
    );
}

#[test]
fn unlisted_spawn_tool_with_spawn_limits_is_denied() {
    let fx = Fx::new();
    assert!(fx.cap.spawn.is_some());
    assert!(!fx.cap.tools.contains("kestrel.spawn"));
    assert_deny_naming(
        &fx.check(&fx.token, fx.cap.run, tool("kestrel.spawn")),
        "kestrel.spawn",
    );
}

#[test]
fn listed_spawn_tool_at_depth_zero_is_denied() {
    // max_depth 0 means the run may not spawn, so the tool is as useless as with `spawn: None`.
    let fx = Fx::from_spec(&spec_from(
        "{ mcp: [kestrel.spawn], spawn: { max_children: 1, max_depth: 0, agents: [reviewer] } }",
        "1",
    ));
    assert_deny_naming(
        &fx.check(&fx.token, fx.cap.run, tool("kestrel.spawn")),
        "spawn",
    );
}

#[test]
fn a_child_without_spawn_loses_the_spawn_tool_but_its_parent_keeps_it() {
    let fx = Fx::from_spec(&spec_from(
        "{ mcp: [kestrel.spawn, fs.read], \
           spawn: { max_children: 2, max_depth: 2, agents: [reviewer] } }",
        "1",
    ));
    let (child, token) = fx.child(|c| c.spawn = None);
    assert!(child.tools.contains("kestrel.spawn"));
    assert_deny_naming(&fx.check(&token, child.run, tool("kestrel.spawn")), "spawn");
    assert_allow(&fx.check(&token, child.run, tool("fs.read")), "fs.read");
    assert_allow(
        &fx.check(&fx.token, fx.cap.run, tool("kestrel.spawn")),
        "parent kestrel.spawn",
    );
    // A child at depth 1 may still spawn, so it keeps the tool.
    let (deep, deep_token) = fx.child(|_| {});
    assert_allow(
        &fx.check(&deep_token, deep.run, tool("kestrel.spawn")),
        "child at depth 1",
    );
    // Its own child (depth 0) may not.
    let grandchild = narrowed(&deep, |_| {});
    let g_token = caps::attenuate(&deep_token, &fx.pk, &grandchild).expect("grandchild");
    assert_deny_naming(
        &fx.check(&g_token, grandchild.run, tool("kestrel.spawn")),
        "spawn",
    );
}
