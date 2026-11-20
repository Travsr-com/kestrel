//! Fixtures for the capability token tests (issue #33): agent specs built from YAML, fixed
//! instants, decision assertions and raw Biscuit surgery for the tamper scenarios. Nothing here
//! reads the system clock.

// Helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the right
// way to fail here.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeSet;

use biscuit_auth::{BlockBuilder, UnverifiedBiscuit};
use kestrel_core::caps::{
    self, Action, CapError, CapToken, Capability, Decision, Request, RootKey, RootPublicKey,
    SpawnLimits,
};
use kestrel_types::{AgentSpec, MicroUsd, RunId};
use time::{Duration, OffsetDateTime};

/// The tools, hosts, secrets and spawnable agents of [`root_spec`].
/// `kestrel.spawn` is listed because spawning needs it (issue #36 scenario 9).
pub const TOOLS: [&str; 3] = ["fs.read", "fs.write", "kestrel.spawn"];
pub const HOSTS: [&str; 2] = ["api.github.com", "crates.io"];
pub const SECRETS: [&str; 2] = ["github_token", "npm_token"];
pub const AGENTS: [&str; 2] = ["reviewer", "test-writer"];

/// A fixed instant every request is made at, unless a test says otherwise.
pub fn t0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("fixed instant is valid")
}

/// When the root capability of every fixture expires: one hour after [`t0`], on a whole second.
pub fn expiry() -> OffsetDateTime {
    t0() + Duration::hours(1)
}

/// An agent spec whose `capabilities` is the given YAML flow mapping and whose budget is `usd`.
pub fn spec_from(capabilities: &str, usd: &str) -> AgentSpec {
    let yaml = format!(
        "name: caps\nruntime: claude-code\nrole: tester\n\
         model_policy: {{ default: tier:mid }}\n\
         capabilities: {capabilities}\n\
         sandbox: {{ tier: auto, image: fake-image, cpu: 1, memory: 512Mi, workspace: empty-dir }}\n\
         budget: {{ usd: \"{usd}\", wall_clock: 30m }}\n"
    );
    AgentSpec::from_yaml(&yaml).expect("fixture spec parses")
}

/// Two tools plus `kestrel.spawn`, two hosts, two secrets, may spawn 4 children 2 levels deep, $2
/// budget.
pub fn root_spec() -> AgentSpec {
    spec_from(
        "{ mcp: [fs.read, fs.write, kestrel.spawn], network: [api.github.com, crates.io], \
           secrets: [github_token, npm_token], \
           spawn: { max_children: 4, max_depth: 2, agents: [test-writer, reviewer] } }",
        "2.00",
    )
}

pub fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

pub fn usd(dollars: i64) -> MicroUsd {
    MicroUsd(dollars * 1_000_000)
}

pub fn limits(max_children: u32, max_depth: u32, agents: &[&str]) -> SpawnLimits {
    SpawnLimits {
        max_children,
        max_depth,
        agents: set(agents),
    }
}

pub fn tool(name: &str) -> Action {
    Action::Tool(name.to_string())
}

pub fn host(name: &str) -> Action {
    Action::Host(name.to_string())
}

pub fn secret(name: &str) -> Action {
    Action::Secret(name.to_string())
}

pub fn spawn(agent: &str) -> Action {
    Action::Spawn {
        agent: agent.to_string(),
    }
}

pub fn req_at(run: RunId, action: Action, now: OffsetDateTime) -> Request {
    Request { run, action, now }
}

/// `verify` of `action` by `run` at [`t0`].
pub fn check(token: &CapToken, key: &RootPublicKey, run: RunId, action: Action) -> Decision {
    caps::verify(token, key, &req_at(run, action, t0()))
}

#[track_caller]
pub fn assert_allow(d: &Decision, what: &str) {
    assert_eq!(d, &Decision::Allow, "{what} should be allowed");
}

#[track_caller]
pub fn assert_deny(d: &Decision, what: &str) {
    assert!(
        matches!(d, Decision::Deny(_)),
        "{what} should be denied, got {d:?}"
    );
}

/// A deny whose reason names `needle` (compared case-insensitively).
#[track_caller]
pub fn assert_deny_naming(d: &Decision, needle: &str) {
    match d {
        Decision::Deny(reason) => assert!(
            reason.to_lowercase().contains(&needle.to_lowercase()),
            "deny reason {reason:?} should name {needle:?}"
        ),
        Decision::Allow => panic!("expected a deny naming {needle:?}, got Allow"),
    }
}

/// The text of an `Escalation` error, failing on success or any other error.
#[track_caller]
pub fn escalation<T: std::fmt::Debug>(r: Result<T, CapError>) -> String {
    match r {
        Err(e @ CapError::Escalation(_)) => e.to_string(),
        other => panic!("expected CapError::Escalation, got {other:?}"),
    }
}

/// Asserts `r` is an `Escalation` whose text names `needle` (case-insensitively).
#[track_caller]
pub fn assert_escalation_naming<T: std::fmt::Debug>(r: Result<T, CapError>, needle: &str) {
    let text = escalation(r);
    assert!(
        text.to_lowercase().contains(&needle.to_lowercase()),
        "escalation {text:?} should name {needle:?}"
    );
}

#[track_caller]
pub fn assert_invalid_token<T: std::fmt::Debug>(r: Result<T, CapError>, what: &str) {
    assert!(
        matches!(r, Err(CapError::InvalidToken(_))),
        "{what}: expected CapError::InvalidToken, got {r:?}"
    );
}

/// A root key, its root capability for a fresh run and the token issued for it.
pub struct Fx {
    pub key: RootKey,
    pub pk: RootPublicKey,
    pub cap: Capability,
    pub token: CapToken,
}

impl Fx {
    /// The root grant of [`root_spec`], expiring at [`expiry`].
    pub fn new() -> Fx {
        Fx::from_spec(&root_spec())
    }

    pub fn from_spec(spec: &AgentSpec) -> Fx {
        let key = RootKey::generate();
        let cap = Capability::from_spec(RunId::new(), spec, expiry());
        Fx::issued(key, cap)
    }

    pub fn issued(key: RootKey, cap: Capability) -> Fx {
        let pk = key.public();
        let token = caps::issue(&key, &cap).expect("root token issues");
        Fx {
            key,
            pk,
            cap,
            token,
        }
    }

    /// The root capability with a new run id, changed by `edit`. Not attenuated yet.
    pub fn child_cap(&self, edit: impl FnOnce(&mut Capability)) -> Capability {
        narrowed(&self.cap, edit)
    }

    /// Attenuates the root token to `child_cap(edit)`, failing the test if that is refused.
    pub fn child(&self, edit: impl FnOnce(&mut Capability)) -> (Capability, CapToken) {
        let child = self.child_cap(edit);
        let token = caps::attenuate(&self.token, &self.key, &child).expect("child attenuates");
        (child, token)
    }

    /// `verify` against this fixture's public key at [`t0`].
    pub fn check(&self, token: &CapToken, run: RunId, action: Action) -> Decision {
        check(token, &self.pk, run, action)
    }
}

/// The most a child of `parent` may hold: the same sets, budget and expiry, one level less of
/// spawn depth (no spawn at all when the parent is at depth 0 or cannot spawn), and a new run id.
pub fn inherit(parent: &Capability) -> Capability {
    let mut child = parent.clone();
    child.run = RunId::new();
    child.spawn = parent.spawn.as_ref().and_then(|s| {
        s.max_depth.checked_sub(1).map(|max_depth| SpawnLimits {
            max_depth,
            ..s.clone()
        })
    });
    child
}

/// `got` grants nothing `bound` does not: subsets, budget and expiry no larger, spawn limits no
/// larger (the same depth is fine here, unlike `check_subset_of`). The run is not compared.
#[track_caller]
pub fn assert_no_wider(got: &Capability, bound: &Capability) {
    let ok = got.tools.is_subset(&bound.tools)
        && got.hosts.is_subset(&bound.hosts)
        && got.secrets.is_subset(&bound.secrets)
        && got.budget <= bound.budget
        && got.expires_at <= bound.expires_at
        && match (&got.spawn, &bound.spawn) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(g), Some(b)) => {
                g.max_depth <= b.max_depth
                    && g.max_children <= b.max_children
                    && g.agents.is_subset(&b.agents)
            }
        };
    assert!(ok, "{got:?} is wider than {bound:?}");
}

/// [`inherit`] of `parent`, changed by `edit`.
pub fn narrowed(parent: &Capability, edit: impl FnOnce(&mut Capability)) -> Capability {
    let mut child = inherit(parent);
    edit(&mut child);
    child
}

// ---------------------------------------------------------------------------------------------
// Raw Biscuit surgery. Uses only public biscuit-auth 7.0.0-alpha.4 API: `UnverifiedBiscuit`
// parses without checking signatures, and its `SerializedBiscuit` container has public `blocks`
// and a public `to_vec`. So an attacker who holds a token can do all of this.

/// Number of blocks after the authority block.
pub fn attenuation_blocks(bytes: &[u8]) -> usize {
    let parsed = UnverifiedBiscuit::from(bytes).expect("token parses as a biscuit");
    parsed.container().blocks.len()
}

/// The token with attenuation block `index` (0 is the first block after the authority block)
/// removed. Everything else, the proof included, is kept as it was.
pub fn remove_block(token: &CapToken, index: usize) -> Vec<u8> {
    let parsed = UnverifiedBiscuit::from(token.as_bytes()).expect("token parses as a biscuit");
    let mut container = parsed.container().clone();
    assert!(
        index < container.blocks.len(),
        "token has no attenuation block {index}"
    );
    container.blocks.remove(index);
    container.to_vec().expect("stripped token serializes")
}

/// The token with its last attenuation block removed.
pub fn strip_last_block(token: &CapToken) -> Vec<u8> {
    let n = attenuation_blocks(token.as_bytes());
    assert!(n > 0, "token has no attenuation block to strip");
    remove_block(token, n - 1)
}

/// The last attenuation block removed, then sealed with the proof key the token carries (the
/// child's own key, which is all the holder of a child token has).
pub fn strip_last_block_and_seal(token: &CapToken) -> Vec<u8> {
    let parsed = UnverifiedBiscuit::from(token.as_bytes()).expect("token parses as a biscuit");
    let mut container = parsed.container().clone();
    assert!(
        container.blocks.pop().is_some(),
        "token has no attenuation block to strip"
    );
    let sealed = container.seal().expect("stripped container seals");
    sealed.to_vec().expect("sealed token serializes")
}

/// Appends a block built with the raw Biscuit API, the way any token holder can, bypassing
/// `caps::attenuate` and its subset check.
pub fn append_raw_block(token: &CapToken, block: BlockBuilder) -> CapToken {
    let parsed = UnverifiedBiscuit::from(token.as_bytes()).expect("token parses as a biscuit");
    let appended = parsed.append(block).expect("raw block appends");
    CapToken::from_bytes(appended.to_vec().expect("appended token serializes"))
}

/// A raw block that replays block `index` of `token` (its Datalog source when it parses back,
/// and its context string), so it claims whatever that block granted.
pub fn replay_block(token: &CapToken, index: usize) -> BlockBuilder {
    let parsed = UnverifiedBiscuit::from(token.as_bytes()).expect("token parses as a biscuit");
    let mut block = parsed
        .print_block_source(index)
        .ok()
        .and_then(|source| BlockBuilder::new().code(source).ok())
        .unwrap_or_default();
    if let Some(Some(context)) = parsed.context().get(index) {
        block = block.context(context.clone());
    }
    block
}

pub fn hex(bytes: &[u8], upper: bool) -> String {
    bytes
        .iter()
        .map(|b| {
            if upper {
                format!("{b:02X}")
            } else {
                format!("{b:02x}")
            }
        })
        .collect()
}
