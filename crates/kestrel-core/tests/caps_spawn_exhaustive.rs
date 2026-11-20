//! Capability tokens (issue #33), scenario 11 for the spawn axis, exhaustively. The random property
//! test rarely draws a grant that may spawn, so this walks every combination of a small grid of
//! spawn grants through parent, child and grandchild, and checks that each level's allowed spawns
//! are allowed one level up. It also counts the allowed spawns, so the test fails if the grid ever
//! stops reaching the case it exists for.

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

/// For each action `lower` allows, `upper` allows it too. Returns how many `lower` allowed.
fn inside(upper: (&CapToken, RunId), lower: (&CapToken, RunId), pk: &caps::RootPublicKey) -> usize {
    let mut allowed = 0;
    for a in spawn_actions() {
        if check(lower.0, pk, lower.1, a.clone()) == Decision::Allow {
            allowed += 1;
            assert_eq!(
                check(upper.0, pk, upper.1, a.clone()),
                Decision::Allow,
                "a lower level allowed {a:?} and the level above denied it"
            );
        }
    }
    allowed
}

#[test]
fn s11_every_allowed_spawn_is_allowed_one_level_up_across_the_grid() {
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
            let attenuated = caps::attenuate(&parent_token, &key, &child);
            assert_eq!(
                child.check_subset_of(&parent).is_ok(),
                attenuated.is_ok(),
                "check_subset_of and attenuate disagree for {child:?} under {parent:?}"
            );
            let Ok(child_token) = attenuated else {
                continue;
            };
            child_spawns += inside((&parent_token, parent.run), (&child_token, child.run), &pk);

            for (gt, gs) in &grid {
                let grandchild = with_spawn(&child, gt, gs);
                // `attenuate` and `check_subset_of` were shown to agree above; skip the signing
                // cost of refusals here.
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
