//! Capability tokens (issue #33), the spawn axis, exhaustively. The random property test rarely
//! draws a grant that may spawn, so this walks a small grid of spawn grants through parent, child
//! and grandchild. For every attenuation it checks, for each spawn action:
//!
//! * **The rule**: when the child's grant `allows` it, the parent's grant allows it too. This is
//!   the check that fails on a fault in one layer, such as `check_subset_of` letting the depth or
//!   the agents grow, because it compares the grants themselves.
//! * **The tokens**: when `verify` allows it for the child, it allows it for the parent, and the
//!   child's grant agrees. A child token carries every check of its parent, so this part can only
//!   fail when the Rust rules and the Datalog checks break together.
//!
//! It also counts the allowed spawns, so it fails if the grid ever stops reaching an allowed spawn
//! at the child or the grandchild level.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod caps_support;

use std::collections::BTreeSet;

use caps_support::*;
use kestrel_core::caps::{self, Action, CapToken, Capability, Decision, RootKey, SpawnLimits};
use kestrel_types::RunId;

/// Every spawn grant in the grid: with or without the spawn tool, crossed with no spawn rights,
/// depth 0 to 2, and no, one or two spawnable agents.
fn grid() -> Vec<(BTreeSet<String>, Option<SpawnLimits>)> {
    let tools = [set(&["fs.read"]), set(&["fs.read", "kestrel.spawn"])];
    let mut spawns = vec![None];
    for depth in 0..=2 {
        for agents in [&[][..], &["reviewer"][..], &["reviewer", "test-writer"][..]] {
            spawns.push(Some(limits(2, depth, agents)));
        }
    }
    let mut out = Vec::new();
    for t in &tools {
        for s in &spawns {
            out.push((t.clone(), s.clone()));
        }
    }
    out
}

/// `base` with the given tools and spawn grant and a fresh run id.
fn with_spawn(
    base: &Capability,
    tools: &BTreeSet<String>,
    spawn: &Option<SpawnLimits>,
) -> Capability {
    let mut c = base.clone();
    c.run = RunId::new();
    c.tools = tools.clone();
    c.spawn = spawn.clone();
    c
}

/// The spawn actions every level is asked for.
fn spawn_actions() -> Vec<Action> {
    vec![
        spawn("reviewer"),
        spawn("test-writer"),
        spawn("someone-else"),
        tool("kestrel.spawn"),
    ]
}

/// One level of the chain: its token and the run it is bound to.
type Level<'a> = (&'a CapToken, RunId);

/// The rule and token checks of the module docs for `lower` under `upper`. Returns how many spawn
/// actions `lower` was allowed.
fn inside(upper: Level<'_>, lower: Level<'_>, pk: &caps::RootPublicKey) -> usize {
    let upper_grant = caps::grant(upper.0, pk).expect("upper token verifies");
    let lower_grant = caps::grant(lower.0, pk).expect("lower token verifies");
    let mut allowed = 0;
    for a in spawn_actions() {
        if lower_grant.allows(&a).is_ok() {
            assert_eq!(
                upper_grant.allows(&a),
                Ok(()),
                "the lower grant allows {a:?} and the grant above does not:\n  lower {lower_grant:?}\n  upper {upper_grant:?}"
            );
        }
        if check(lower.0, pk, lower.1, a.clone()) == Decision::Allow {
            allowed += 1;
            assert_eq!(
                lower_grant.allows(&a),
                Ok(()),
                "verify allowed {a:?} that the lower grant does not"
            );
            assert_eq!(
                check(upper.0, pk, upper.1, a.clone()),
                Decision::Allow,
                "the lower token allowed {a:?} and the token above denied it"
            );
        }
    }
    allowed
}

/// Whether a grant has spawn limits at all. Without them no child of it may have spawn limits
/// (`check_subset_of` refuses before any depth arithmetic), so its grandchildren add nothing to the
/// spawn axis. With them, even at depth 0, a broken depth rule could hand spawn rights on, so they
/// must be walked.
fn has_spawn_limits(cap: &Capability) -> bool {
    cap.spawn.is_some()
}

#[test]
fn every_allowed_spawn_is_allowed_one_level_up_across_the_grid() {
    let key = RootKey::generate();
    let pk = key.public();
    let base = Capability::from_spec(RunId::new(), &root_spec(), expiry());
    let grid = grid();
    // Parents get room for two more levels (depth 3), plus a few that cannot pass spawning on, so
    // both an allowed grandchild spawn and the refusals are reachable. Children and grandchildren
    // walk the whole grid.
    let spawner = set(&["fs.read", "kestrel.spawn"]);
    let parents = [
        (
            spawner.clone(),
            Some(limits(2, 3, &["reviewer", "test-writer"])),
        ),
        (spawner.clone(), Some(limits(2, 2, &["reviewer"]))),
        (spawner, None),
        (
            set(&["fs.read"]),
            Some(limits(2, 3, &["reviewer", "test-writer"])),
        ),
    ];

    let (mut child_spawns, mut grandchild_spawns, mut chains) = (0, 0, 0);
    for (pt, ps) in &parents {
        let parent = with_spawn(&base, pt, ps);
        let parent_token = caps::issue(&key, &parent).expect("root token issues");
        for (ct, cs) in &grid {
            let child = with_spawn(&parent, ct, cs);
            // `attenuate` refuses exactly what `check_subset_of` refuses (it calls it); only the
            // allowed attenuations have a token to check.
            if child.check_subset_of(&parent).is_err() {
                continue;
            }
            let child_token =
                caps::attenuate(&parent_token, &key, &child).expect("a subset child attenuates");
            child_spawns += inside((&parent_token, parent.run), (&child_token, child.run), &pk);

            // See `has_spawn_limits`: skipping only these keeps the test fast without hiding a
            // broken depth rule.
            if !has_spawn_limits(&child) {
                continue;
            }
            for (gt, gs) in &grid {
                let grandchild = with_spawn(&child, gt, gs);
                if grandchild.check_subset_of(&child).is_err() {
                    continue;
                }
                let grandchild_token = caps::attenuate(&child_token, &key, &grandchild)
                    .expect("a subset grandchild attenuates");
                chains += 1;
                grandchild_spawns += inside(
                    (&child_token, child.run),
                    (&grandchild_token, grandchild.run),
                    &pk,
                );
            }
        }
    }
    // The grid must reach allowed spawns at both levels, or it tests nothing.
    assert!(child_spawns > 0, "no child was ever allowed to spawn");
    assert!(
        grandchild_spawns > 0,
        "no grandchild was ever allowed to spawn"
    );
    assert!(chains > 0, "no grandchild chain was valid");
}
