//! Capability grants and tokens: what a run may do, and why a child never gets more.
//!
//! A run's grant is a [`Capability`]: the tools, hosts and secrets it may use, whether and what it
//! may spawn, its budget ceiling and its expiry. A child's grant is always a subset of its parent's.
//!
//! Grants travel as Biscuit tokens ([`CapToken`]). The server signs a root token with its
//! [`RootKey`]; [`attenuate`] narrows a token for a child by appending one block, also signed with
//! the root key; [`verify`] answers allow or deny for one [`Request`]. This module is pure logic:
//! no store, no network, no clock (the caller passes `now`).
//!
//! # Who can attenuate
//!
//! Only the holder of the root key, which is the server. [`attenuate`] takes the [`RootKey`] and
//! appends the child's block as a Biscuit *third-party block* signed by it
//! (`Biscuit::third_party_request`, `ThirdPartyRequest::create_block`,
//! `Biscuit::append_third_party`). The external signature covers the block and the signature of
//! the block before it, so a signed block cannot be moved to another token.
//!
//! A token holder can still append a block with the plain Biscuit API, since a token carries the
//! private key for its next block. Such a token is refused:
//!
//! * At parse time, `DeserializationLimits::allow_attenuations_without_external_signatures` is
//!   false, so any attenuation block without an external signature fails to deserialize.
//! * In [`grant`] and [`verify`], every attenuation block's external key must be the root public
//!   key (a third-party block signed by any other key is refused), and its grant must pass
//!   `check_subset_of` against the grant before it. A signed but widening chain fails closed.
//!
//! Every refusal is [`CapError::InvalidToken`] from [`grant`] and [`attenuate`], and a Deny for
//! every run from [`verify`].
//!
//! **Run binding is a boundary.** A token is bound to the run of its last block. Since only the
//! server writes blocks, only the server chooses that run: no token can be bound to a run the
//! server has not attenuated for, and binding never adds rights. (A holder of an ancestor token who
//! also holds a descendant's token can rebuild a token for that existing descendant run, with
//! exactly the descendant's rights; the gateway reads each run's token from the store, so a token
//! presented by a sandbox is never trusted.) The run check in each block holds only when the block
//! is the last one
//! (`$last != {index} || ...`), because a block cannot name the run of a child that does not exist
//! yet. The authorizer supplies `last_block` from the verified token.
//!
//! # Token encoding
//!
//! Every block, the authority block included, carries the same two things for the grant it
//! describes:
//!
//! * **Checks** that encode its narrowing, enforced by Biscuit's own authorizer. A block can only
//!   add checks, so a later block can never undo an earlier one:
//!
//!   ```text
//!   check if run($run), last_block($last), $last != {index} || $run == {run};
//!   check if time($time), $time < {expires};
//!   check if action($kind, $name, $amount), $kind != "tool"   || {tools}.contains($name);
//!   check if action($kind, $name, $amount), $kind != "host"   || {hosts}.contains($name);
//!   check if action($kind, $name, $amount), $kind != "secret" || {secrets}.contains($name);
//!   check if action($kind, $name, $amount), $kind != "spawn"  || {agents}.contains($name);
//!   check if action($kind, $name, $amount), $kind != "spend"
//!       || ($amount >= 0 && $amount <= {budget});
//!   ```
//!
//!   Every `{...}` is a Biscuit parameter (a `Term`), never text pasted into the source, so a name
//!   such as `a") or true` stays one string. The sets come from [`Capability::allows`].
//! * **Context**: canonical JSON of the block's [`Capability`] (version, run, sets, spawn limits,
//!   budget, expiry to the nanosecond). It is signed with the block, and [`grant`] reads it back.
//!
//! The authorizer adds the request as facts and one policy:
//!
//! ```text
//! run({request run}); time({now}); last_block({block count - 1});
//! action({kind}, {name}, {amount});   // amount is 0 except for spending; name is "" for spending
//! allow if action($kind, $name, $amount);
//! ```
//!
//! Kestrel's blocks hold checks only, no facts or rules. Biscuit's default scope lets a check (or
//! the policy) read only its own block, the authority block and the authorizer, so facts in an
//! appended block never satisfy anyone else's check.
//!
//! **Spawning needs [`SPAWN_TOOL`].** `Action::Spawn` is allowed only when `kestrel.spawn` is in
//! `tools`, `spawn` is Some with `max_depth >= 1`, and the agent is in `agents`. The tool itself
//! also needs spawn rights, even when listed. In Datalog, `{tools}` leaves out `kestrel.spawn`
//! and `{agents}` is empty whenever the tool is not allowed.
//!
//! **Spending** is allowed for `0 <= amount <= budget`; a negative amount is denied.
//!
//! **Expiry.** Biscuit dates are whole seconds. The check compares floor(now) with
//! floor(expires_at) and denies when they are equal, so it can only deny early within the expiry
//! second, never allow late. The exact instant is in the context, and [`verify`] also denies any
//! request with `now >= expires_at`.
//!
//! **Stripping a block fails.** A Biscuit token ends with a proof: the private key matching the
//! `next_key` of its last block. A child token carries the child block's key. Remove that block
//! and the proof no longer matches the new last block, so the token fails to verify for everyone.
//! Resealing does not help either: sealed tokens are refused.
//!
//! # Limits
//!
//! * Parsing: at most [`MAX_BLOCKS`] blocks (authority included) and 1 MiB, Ed25519 keys only
//!   (`allowed_internal_signing_algorithms` and `allowed_external_signing_algorithms`), no sealed
//!   tokens, every attenuation block third-party. [`attenuate`] refuses to add a block past the
//!   limit.
//! * Authorizing: at most 10 000 facts, 100 iterations and 100 000 steps, which bound the work
//!   deterministically. The 1 s wall clock limit is only a backstop.
//!
//! # Verification
//!
//! [`grant`] and [`verify`] check every signature against the root public key, then decode the
//! context of every block and check the chain as above (a block with missing or malformed data
//! makes the whole token invalid). The grant is the last block's. [`verify`] then checks the
//! request in Rust with [`Capability::allows`] to build a precise deny reason, and only allows
//! when Biscuit's authorizer also allows. The Rust check never allows what the authorizer denies.
//!
//! `max_children` is carried and narrowed, but counting a run's children needs the store, so the
//! engine enforces it, not this module.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::time::Duration as StdDuration;

use biscuit_auth::builder::Term;
use biscuit_auth::{
    Algorithm, AuthorizerBuilder, AuthorizerLimits, Biscuit, BiscuitBuilder, BlockBuilder,
    DeserializationLimits, PrivateKey, PublicKey,
};
use kestrel_types::{AgentSpec, MicroUsd, RunId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// The MCP tool that spawns a child run. It is denied whenever the grant may not spawn, even when
/// the spec lists it, and [`Action::Spawn`] is denied unless it is listed.
pub const SPAWN_TOOL: &str = "kestrel.spawn";

/// What a run may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    /// The run this grant was issued to.
    pub run: RunId,
    /// MCP tools, from `spec.capabilities.mcp`.
    pub tools: BTreeSet<String>,
    /// Network hosts, from `spec.capabilities.network`.
    pub hosts: BTreeSet<String>,
    /// Secret names, from `spec.capabilities.secrets`.
    pub secrets: BTreeSet<String>,
    /// `None`: the run may not spawn (`kestrel.spawn` is denied).
    pub spawn: Option<SpawnLimits>,
    /// Budget ceiling, from `spec.budget.limit`.
    pub budget: MicroUsd,
    /// Every request at or after this instant is denied.
    pub expires_at: OffsetDateTime,
}

/// How a run may spawn children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnLimits {
    /// Most children this run may have.
    pub max_children: u32,
    /// How many levels may still be spawned below this run. 0 means it may not spawn.
    pub max_depth: u32,
    /// Agents this run may spawn. Empty means none (fail closed).
    pub agents: BTreeSet<String>,
}

impl Capability {
    /// The root grant for `run` from its spec.
    pub fn from_spec(run: RunId, spec: &AgentSpec, expires_at: OffsetDateTime) -> Capability {
        let caps = &spec.capabilities;
        Capability {
            run,
            tools: caps.mcp.iter().cloned().collect(),
            hosts: caps.network.iter().cloned().collect(),
            secrets: caps.secrets.iter().cloned().collect(),
            spawn: caps.spawn.as_ref().map(|s| SpawnLimits {
                max_children: s.max_children,
                max_depth: s.max_depth,
                agents: s.agents.iter().cloned().collect(),
            }),
            budget: spec.budget.limit,
            expires_at,
        }
    }

    /// Ok when `self` (a child) asks for no more than `parent` allows: tools, hosts and secrets are
    /// subsets; budget and expiry are no larger; spawn is `None`, or the parent may spawn
    /// (`max_depth >= 1`) and the child's depth is at most the parent's minus one, its
    /// `max_children` no larger and its agents a subset. `run` is not compared.
    pub fn check_subset_of(&self, parent: &Capability) -> Result<(), CapError> {
        let escalation = |what: String| Err(CapError::Escalation(what));
        for (kind, mine, theirs) in [
            ("tool", &self.tools, &parent.tools),
            ("host", &self.hosts, &parent.hosts),
            ("secret", &self.secrets, &parent.secrets),
        ] {
            if let Some(extra) = mine.difference(theirs).next() {
                return escalation(format!("{kind} `{extra}` is not granted to the parent"));
            }
        }
        if self.budget > parent.budget {
            return escalation(format!(
                "budget {} is more than the parent's {}",
                self.budget, parent.budget
            ));
        }
        if self.expires_at > parent.expires_at {
            return escalation(format!(
                "expires_at {} is later than the parent's {}",
                self.expires_at, parent.expires_at
            ));
        }
        let Some(child) = &self.spawn else {
            return Ok(());
        };
        let Some(theirs) = &parent.spawn else {
            return escalation("spawn: the parent may not spawn".to_string());
        };
        let Some(depth_left) = theirs.max_depth.checked_sub(1) else {
            return escalation("spawn depth: the parent has max_depth 0 and may not spawn".into());
        };
        if child.max_depth > depth_left {
            return escalation(format!(
                "spawn max_depth {} is more than the parent's {} minus one",
                child.max_depth, theirs.max_depth
            ));
        }
        if child.max_children > theirs.max_children {
            return escalation(format!(
                "spawn max_children {} is more than the parent's {}",
                child.max_children, theirs.max_children
            ));
        }
        if let Some(extra) = child.agents.difference(&theirs.agents).next() {
            return escalation(format!(
                "spawnable agent `{extra}` is not granted to the parent"
            ));
        }
        Ok(())
    }

    /// The spawn limits, when this grant may actually spawn (`max_depth >= 1`).
    fn spawnable(&self) -> Option<&SpawnLimits> {
        self.spawn.as_ref().filter(|s| s.max_depth >= 1)
    }

    /// Ok when this grant allows `action`, or a reason a human can read that names what was asked
    /// for. The one rule set: [`verify`] uses it for the Rust half of its check, the Datalog sets
    /// of every block are built from it, and callers such as the gateway's `tools/list` can filter
    /// with it. It does not look at the run or the expiry, which need a [`Request`].
    ///
    /// * `Tool`: listed in `tools`; [`SPAWN_TOOL`] also needs spawn rights (`spawn` Some with
    ///   `max_depth >= 1`) and at least one spawnable agent, since without one it cannot succeed.
    /// * `Host`, `Secret`: listed.
    /// * `Spawn`: [`SPAWN_TOOL`] listed in `tools`, spawn rights, and the agent in `agents`.
    /// * `Spend`: `0 <= amount <= budget`.
    pub fn allows(&self, action: &Action) -> Result<(), String> {
        let listed = |kind: &str, set: &BTreeSet<String>, name: &str| {
            if set.contains(name) {
                Ok(())
            } else {
                Err(format!("{kind} `{name}` is not granted"))
            }
        };
        match action {
            Action::Tool(name) => {
                listed("tool", &self.tools, name)?;
                if name == SPAWN_TOOL && self.spawnable().is_none() {
                    return Err(format!(
                        "tool `{name}` needs spawn rights, and this grant may not spawn"
                    ));
                }
                if name == SPAWN_TOOL && self.spawnable().is_some_and(|s| s.agents.is_empty()) {
                    return Err(format!(
                        "tool `{name}` needs a spawnable agent, and the agent list is empty"
                    ));
                }
                Ok(())
            }
            Action::Host(name) => listed("host", &self.hosts, name),
            Action::Secret(name) => listed("secret", &self.secrets, name),
            Action::Spawn { agent } => {
                if !self.tools.contains(SPAWN_TOOL) {
                    return Err(format!(
                        "spawn of agent `{agent}`: tool `{SPAWN_TOOL}` is not granted"
                    ));
                }
                match (&self.spawn, self.spawnable()) {
                    (None, _) => Err(format!(
                        "spawn of agent `{agent}`: this grant may not spawn"
                    )),
                    (Some(_), None) => Err(format!(
                        "spawn of agent `{agent}`: no spawn depth left (max_depth 0)"
                    )),
                    (Some(_), Some(limits)) => listed("spawnable agent", &limits.agents, agent),
                }
            }
            Action::Spend(amount) if amount.0 < 0 => Err(format!("spending {amount} is negative")),
            Action::Spend(amount) if *amount > self.budget => Err(format!(
                "spending {amount} is more than the budget {}",
                self.budget
            )),
            Action::Spend(_) => Ok(()),
        }
    }
}

/// Something a run asks to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Call an MCP tool.
    Tool(String),
    /// Reach a network host.
    Host(String),
    /// Read a secret.
    Secret(String),
    /// `kestrel.spawn` of the given agent: needs [`SPAWN_TOOL`] in tools, spawn Some,
    /// max_depth >= 1, agent in agents.
    Spawn { agent: String },
    /// Spending this much in total: allowed while 0 <= amount <= budget.
    Spend(MicroUsd),
}

/// One request to check against a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The run making the request (from the gateway path token).
    pub run: RunId,
    /// What it asks to do.
    pub action: Action,
    /// When it asks.
    pub now: OffsetDateTime,
}

/// The answer to a [`Request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// A reason a human can read. It names what was asked for (the tool, host, secret or agent
    /// name, or "expired", or the run id mismatch).
    Deny(String),
}

/// Errors from building or reading capability tokens.
#[derive(Debug, thiserror::Error)]
pub enum CapError {
    /// The child asks for more than the parent has. The text names what.
    #[error("capability escalation: {0}")]
    Escalation(String),
    /// Bad bytes, bad signature, wrong root key, malformed blocks.
    #[error("invalid capability token: {0}")]
    InvalidToken(String),
    /// Anything else from the token library (key generation, serialization).
    #[error("capability token error: {0}")]
    Token(String),
}

/// The server-held key that signs root tokens. `Debug` never prints the private key.
pub struct RootKey(PrivateKey);

impl RootKey {
    /// A new random Ed25519 key.
    pub fn generate() -> RootKey {
        RootKey(PrivateKey::new_with_algorithm(Algorithm::Ed25519))
    }

    /// Private key bytes, for keeping the key across restarts. Round-trips with `from_bytes`.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.0.to_bytes().to_vec()
    }

    /// The key from [`RootKey::to_bytes`].
    pub fn from_bytes(bytes: &[u8]) -> Result<RootKey, CapError> {
        PrivateKey::from_bytes(bytes, Algorithm::Ed25519)
            .map(RootKey)
            .map_err(|e| CapError::Token(e.to_string()))
    }

    /// The public key that verifies tokens signed by this key.
    pub fn public(&self) -> RootPublicKey {
        RootPublicKey(self.0.public())
    }
}

impl fmt::Debug for RootKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootKey")
            .field("public", &self.0.public().to_string())
            .finish_non_exhaustive()
    }
}

/// The public half of a [`RootKey`]: what verifies tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootPublicKey(PublicKey);

/// A serialized token. `from_bytes` does not verify anything; `grant`, `verify`, `attenuate` do.
/// `Debug` prints only the length: the bytes are a bearer credential.
#[derive(Clone, PartialEq, Eq)]
pub struct CapToken(Vec<u8>);

impl fmt::Debug for CapToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapToken")
            .field("len", &self.0.len())
            .finish()
    }
}

impl CapToken {
    /// Wraps bytes as they are.
    pub fn from_bytes(bytes: Vec<u8>) -> CapToken {
        CapToken(bytes)
    }

    /// The serialized token.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A root token for `cap`, signed by `root`.
pub fn issue(root: &RootKey, cap: &Capability) -> Result<CapToken, CapError> {
    let block = BiscuitBuilder::new()
        .code_with_params(BLOCK_CHECKS, block_params(cap, 0), HashMap::new())
        .map_err(token_error)?
        .context(encode(cap)?);
    let biscuit = block.build(&root.0).map_err(token_error)?;
    sized(biscuit.to_vec().map_err(token_error)?)
}

/// `bytes` as a token, or [`CapError::Token`] when they are over [`MAX_TOKEN_BYTES`]. Reading such
/// a token fails, so it is refused when written instead of being handed back dead on arrival.
fn sized(bytes: Vec<u8>) -> Result<CapToken, CapError> {
    if bytes.len() > MAX_TOKEN_BYTES {
        return Err(CapError::Token(format!(
            "the token would be {} bytes, more than the {} MiB limit",
            bytes.len(),
            MAX_TOKEN_BYTES >> 20
        )));
    }
    Ok(CapToken(bytes))
}

/// Verifies `parent` against `root`, checks `child.check_subset_of(parent grant)`, and appends one
/// third-party block signed by `root`. The new token is issued to `child.run`. Only the server,
/// which holds the root key, can attenuate.
///
/// Fails with [`CapError::Token`] when the parent already holds [`MAX_BLOCKS`] blocks.
pub fn attenuate(
    parent: &CapToken,
    root: &RootKey,
    child: &Capability,
) -> Result<CapToken, CapError> {
    let public = root.public();
    let biscuit = parse(parent, &public)?;
    child.check_subset_of(&effective(&biscuit, &public)?)?;
    if biscuit.block_count() >= MAX_BLOCKS {
        return Err(CapError::Token(format!(
            "a token holds at most {MAX_BLOCKS} blocks, and this one is full"
        )));
    }
    let block = BlockBuilder::new()
        .code_with_params(
            BLOCK_CHECKS,
            block_params(child, biscuit.block_count()),
            HashMap::new(),
        )
        .map_err(token_error)?
        .context(encode(child)?);
    append_signed(&biscuit, root, block)
}

/// `biscuit` plus `block` as a third-party block signed by `root`. The signature covers the
/// block and the signature of the block before it, so it cannot be moved to another token.
fn append_signed(
    biscuit: &Biscuit,
    root: &RootKey,
    block: BlockBuilder,
) -> Result<CapToken, CapError> {
    let signed = biscuit
        .third_party_request()
        .and_then(|request| request.create_block(&root.0, block))
        .map_err(token_error)?;
    let appended = biscuit
        .append_third_party(root.0.public(), signed)
        .map_err(token_error)?;
    sized(appended.to_vec().map_err(token_error)?)
}

/// The verified effective grant of a token: its last block, after checking that every block
/// narrows the one before it.
pub fn grant(token: &CapToken, key: &RootPublicKey) -> Result<Capability, CapError> {
    effective(&parse(token, key)?, key)
}

/// Allow or deny `req`. Any token problem (bad signature, wrong key, stripped block) is a Deny with
/// a reason, never a panic. Deny when `req.run` is not the token's run, when `req.now >=
/// expires_at`, or when the action is not granted.
pub fn verify(token: &CapToken, key: &RootPublicKey, req: &Request) -> Decision {
    match check_request(token, key, req) {
        Ok(()) => Decision::Allow,
        Err(reason) => Decision::Deny(reason),
    }
}

fn check_request(token: &CapToken, key: &RootPublicKey, req: &Request) -> Result<(), String> {
    let biscuit = parse(token, key).map_err(|e| e.to_string())?;
    let cap = effective(&biscuit, key).map_err(|e| e.to_string())?;
    if req.run != cap.run {
        return Err(format!(
            "token is bound to run {}, not to run {}",
            cap.run, req.run
        ));
    }
    if req.now >= cap.expires_at {
        return Err(format!("token expired at {}", cap.expires_at));
    }
    cap.allows(&req.action)?;
    authorize(&biscuit, req).map_err(|e| format!("denied by the token's checks: {e}"))
}

/// Runs Biscuit's authorizer for `req` over every block's checks.
fn authorize(biscuit: &Biscuit, req: &Request) -> Result<(), biscuit_auth::error::Token> {
    let (kind, name, amount) = match &req.action {
        Action::Tool(name) => ("tool", name.as_str(), 0),
        Action::Host(name) => ("host", name.as_str(), 0),
        Action::Secret(name) => ("secret", name.as_str(), 0),
        Action::Spawn { agent } => ("spawn", agent.as_str(), 0),
        Action::Spend(amount) => ("spend", "", amount.0),
    };
    let last = biscuit.block_count().saturating_sub(1);
    let params = HashMap::from([
        ("run".to_string(), Term::Str(req.run.to_string())),
        ("now".to_string(), date(req.now)),
        ("last".to_string(), integer(last)),
        ("kind".to_string(), Term::Str(kind.to_string())),
        ("name".to_string(), Term::Str(name.to_string())),
        ("amount".to_string(), Term::Integer(amount)),
    ]);
    let mut authorizer = AuthorizerBuilder::new()
        .code_with_params(AUTHORIZER, params, HashMap::new())?
        .set_limits(run_limits())
        .build(biscuit)?;
    authorizer.authorize_with_limits(run_limits()).map(|_| ())
}

/// The checks of every block. See the module docs.
const BLOCK_CHECKS: &str = r#"
check if run($run), last_block($last), $last != {index} || $run == {run};
check if time($time), $time < {expires};
check if action($kind, $name, $amount), $kind != "tool" || {tools}.contains($name);
check if action($kind, $name, $amount), $kind != "host" || {hosts}.contains($name);
check if action($kind, $name, $amount), $kind != "secret" || {secrets}.contains($name);
check if action($kind, $name, $amount), $kind != "spawn" || {agents}.contains($name);
check if action($kind, $name, $amount), $kind != "spend" || ($amount >= 0 && $amount <= {budget});
"#;

/// The request facts and the one allow policy.
const AUTHORIZER: &str = r#"
run({run});
time({now});
last_block({last});
action({kind}, {name}, {amount});
allow if action($kind, $name, $amount);
"#;

/// The Datalog sets come from [`Capability::allows`]: a tool is in `{tools}` only when it is
/// allowed, and `{agents}` is empty unless [`SPAWN_TOOL`] is allowed (listed, with spawn rights).
fn block_params(cap: &Capability, index: usize) -> HashMap<String, Term> {
    let allowed_tool = |t: &&String| cap.allows(&Action::Tool((*t).clone())).is_ok();
    let tools = strings(cap.tools.iter().filter(allowed_tool));
    let may_spawn = cap.allows(&Action::Tool(SPAWN_TOOL.to_string())).is_ok();
    let agents = strings(
        cap.spawnable()
            .filter(|_| may_spawn)
            .into_iter()
            .flat_map(|s| s.agents.iter()),
    );
    HashMap::from([
        ("index".to_string(), integer(index)),
        ("run".to_string(), Term::Str(cap.run.to_string())),
        ("expires".to_string(), date(cap.expires_at)),
        ("tools".to_string(), tools),
        ("hosts".to_string(), strings(&cap.hosts)),
        ("secrets".to_string(), strings(&cap.secrets)),
        ("agents".to_string(), agents),
        ("budget".to_string(), Term::Integer(cap.budget.0)),
    ])
}

/// A Datalog set of strings. Each name is one `Term::Str`, never parsed.
fn strings<'a>(names: impl IntoIterator<Item = &'a String>) -> Term {
    Term::Set(names.into_iter().map(|n| Term::Str(n.clone())).collect())
}

/// A Biscuit date: whole seconds since the epoch, rounded down. Instants before 1970 become 0,
/// which only makes the expiry check stricter.
fn date(at: OffsetDateTime) -> Term {
    Term::Date(u64::try_from(at.unix_timestamp()).unwrap_or(0))
}

fn integer(n: usize) -> Term {
    Term::Integer(i64::try_from(n).unwrap_or(i64::MAX))
}

/// Runtime limits for the authorizer. The step, iteration and fact counts are the real bound: they
/// are deterministic and cap the work of [`MAX_BLOCKS`] blocks of fixed checks with room to
/// spare. The wall clock limit is only a backstop. Biscuit's default of 1 ms is too tight for a
/// debug or coverage build, which would then wrongly deny, so it is 1 s.
fn run_limits() -> AuthorizerLimits {
    AuthorizerLimits {
        max_facts: 10_000,
        max_iterations: 100,
        max_time: StdDuration::from_secs(1),
        max_steps: 100_000,
    }
}

/// Most blocks a token may hold, the authority block included: a run tree at most 31 levels deep.
pub const MAX_BLOCKS: usize = 32;

/// Largest serialized token, 1 MiB. Enforced when a token is read, and when [`issue`] or
/// [`attenuate`] writes one.
pub const MAX_TOKEN_BYTES: usize = 1 << 20;

/// What a token may contain to be read at all: at most [`MAX_BLOCKS`] blocks and 1 MiB, Ed25519
/// keys only, never sealed, and every attenuation block a third-party block. That the third party
/// is the root key is checked in [`effective`].
fn deserialization_limits() -> DeserializationLimits {
    let ed25519 = || Some(HashSet::from([Algorithm::Ed25519]));
    let mut limits = DeserializationLimits::allow_everything();
    limits.allow_sealed_tokens = false;
    limits.allow_attenuations_with_external_signatures = true;
    limits.allow_attenuations_without_external_signatures = false;
    limits.max_token_size = MAX_TOKEN_BYTES;
    limits.max_blocks = MAX_BLOCKS;
    limits.allowed_internal_signing_algorithms = ed25519();
    limits.allowed_external_signing_algorithms = ed25519();
    limits
}

/// The token, with every signature and its proof checked against `key`.
fn parse(token: &CapToken, key: &RootPublicKey) -> Result<Biscuit, CapError> {
    Biscuit::from_with_limits(token.as_bytes(), key.0, &deserialization_limits())
        .map_err(|e| CapError::InvalidToken(e.to_string()))
}

/// The grant of the last block, after checking the chain: every attenuation block is signed by
/// `root` (as a third-party block) and narrows the grant before it. So the last block is the
/// narrowest, and the run it names was chosen by the server.
fn effective(biscuit: &Biscuit, root: &RootPublicKey) -> Result<Capability, CapError> {
    let signers = biscuit.external_public_keys();
    let contexts = biscuit.context();
    // Both lists have one entry per block. `zip` would silently skip blocks if they ever did not.
    if contexts.len() != signers.len() {
        return Err(CapError::InvalidToken(
            "the token's blocks and signers do not match".to_string(),
        ));
    }
    let mut grant: Option<Capability> = None;
    for (index, (context, signer)) in contexts.into_iter().zip(signers).enumerate() {
        let block = decode(index, context.as_deref())?;
        if let Some(previous) = &grant {
            let invalid = |why: String| CapError::InvalidToken(format!("block {index}: {why}"));
            if signer.as_ref() != Some(&root.0) {
                return Err(invalid("not signed by the root key".to_string()));
            }
            block
                .check_subset_of(previous)
                .map_err(|e| invalid(format!("does not narrow the block before it: {e}")))?;
        }
        grant = Some(block);
    }
    grant.ok_or_else(|| CapError::InvalidToken("the token has no blocks".to_string()))
}

/// The signed data of one block: its [`Capability`], exactly.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockData {
    v: u32,
    run: RunId,
    tools: BTreeSet<String>,
    hosts: BTreeSet<String>,
    secrets: BTreeSet<String>,
    spawn: Option<SpawnData>,
    budget: i64,
    expires_unix: i64,
    expires_nanos: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnData {
    max_children: u32,
    max_depth: u32,
    agents: BTreeSet<String>,
}

const DATA_VERSION: u32 = 1;
const NANOS_PER_SECOND: u32 = 1_000_000_000;

fn encode(cap: &Capability) -> Result<String, CapError> {
    let data = BlockData {
        v: DATA_VERSION,
        run: cap.run,
        tools: cap.tools.clone(),
        hosts: cap.hosts.clone(),
        secrets: cap.secrets.clone(),
        spawn: cap.spawn.as_ref().map(|s| SpawnData {
            max_children: s.max_children,
            max_depth: s.max_depth,
            agents: s.agents.clone(),
        }),
        budget: cap.budget.0,
        // Whole seconds rounded down plus the nanoseconds within that second, so the pair is exact
        // before 1970 too.
        expires_unix: cap.expires_at.unix_timestamp(),
        expires_nanos: cap.expires_at.nanosecond(),
    };
    serde_json::to_string(&data).map_err(|e| CapError::Token(e.to_string()))
}

fn decode(index: usize, context: Option<&str>) -> Result<Capability, CapError> {
    let invalid = |why: String| CapError::InvalidToken(format!("block {index}: {why}"));
    let text = context.ok_or_else(|| invalid("no capability data".to_string()))?;
    let data: BlockData = serde_json::from_str(text)
        .map_err(|e| invalid(format!("malformed capability data: {e}")))?;
    if data.v != DATA_VERSION {
        return Err(invalid(format!(
            "unknown capability data version {}",
            data.v
        )));
    }
    if data.expires_nanos >= NANOS_PER_SECOND {
        return Err(invalid(format!(
            "bad expiry nanoseconds {}",
            data.expires_nanos
        )));
    }
    let nanos = i128::from(data.expires_unix) * i128::from(NANOS_PER_SECOND)
        + i128::from(data.expires_nanos);
    let expires_at = OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map_err(|e| invalid(format!("bad expiry: {e}")))?;
    Ok(Capability {
        run: data.run,
        tools: data.tools,
        hosts: data.hosts,
        secrets: data.secrets,
        spawn: data.spawn.map(|s| SpawnLimits {
            max_children: s.max_children,
            max_depth: s.max_depth,
            agents: s.agents,
        }),
        budget: MicroUsd(data.budget),
        expires_at,
    })
}

fn token_error(e: biscuit_auth::error::Token) -> CapError {
    CapError::Token(e.to_string())
}

#[cfg(test)]
mod tests {
    //! The Datalog half on its own: [`authorize`] must deny what the Rust check denies, so the
    //! Rust check is only ever a better reason, never the sole guard. Also the decode errors a
    //! forged block can trigger.

    use time::Duration;

    use super::*;

    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(secs).expect("valid instant")
    }

    fn names(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn cap(spawn: Option<SpawnLimits>) -> Capability {
        Capability {
            run: RunId::new(),
            tools: names(&["fs.read", SPAWN_TOOL]),
            hosts: names(&["crates.io"]),
            secrets: names(&["npm_token"]),
            spawn,
            budget: MicroUsd(1_000),
            expires_at: at(1_800_003_600),
        }
    }

    fn spawn(max_depth: u32) -> Option<SpawnLimits> {
        Some(SpawnLimits {
            max_children: 2,
            max_depth,
            agents: names(&["reviewer"]),
        })
    }

    fn req(run: RunId, action: Action, now: OffsetDateTime) -> Request {
        Request { run, action, now }
    }

    /// Runs only the Biscuit authorizer for `r` on `token`.
    fn datalog(token: &CapToken, key: &RootPublicKey, r: &Request) -> bool {
        let biscuit = parse(token, key).expect("token parses");
        authorize(&biscuit, r).is_ok()
    }

    fn tool(n: &str) -> Action {
        Action::Tool(n.to_string())
    }

    fn agent(n: &str) -> Action {
        Action::Spawn {
            agent: n.to_string(),
        }
    }

    #[test]
    fn datalog_alone_allows_what_is_granted_and_denies_the_rest() {
        let key = RootKey::generate();
        let pk = key.public();
        let c = cap(spawn(1));
        let token = issue(&key, &c).expect("issues");
        let now = at(1_800_000_000);
        let allowed = [
            tool("fs.read"),
            tool(SPAWN_TOOL),
            Action::Host("crates.io".into()),
            Action::Secret("npm_token".into()),
            agent("reviewer"),
            Action::Spend(MicroUsd(1_000)),
        ];
        for a in allowed {
            assert!(datalog(&token, &pk, &req(c.run, a.clone(), now)), "{a:?}");
        }
        let denied = [
            tool("fs.write"),
            tool("crates.io"),
            Action::Host("fs.read".into()),
            Action::Secret("crates.io".into()),
            agent("deployer"),
            Action::Spend(MicroUsd(1_001)),
        ];
        for a in denied {
            assert!(!datalog(&token, &pk, &req(c.run, a.clone(), now)), "{a:?}");
        }
        // Wrong run, and at expiry.
        assert!(!datalog(
            &token,
            &pk,
            &req(RunId::new(), tool("fs.read"), now)
        ));
        assert!(!datalog(
            &token,
            &pk,
            &req(c.run, tool("fs.read"), c.expires_at)
        ));
    }

    #[test]
    fn datalog_alone_denies_spawning_without_spawn_rights() {
        let key = RootKey::generate();
        let pk = key.public();
        let now = at(1_800_000_000);
        for limits in [None, spawn(0)] {
            let c = cap(limits.clone());
            let token = issue(&key, &c).expect("issues");
            let what = format!("{limits:?}");
            assert!(
                !datalog(&token, &pk, &req(c.run, tool(SPAWN_TOOL), now)),
                "{what}"
            );
            assert!(
                !datalog(&token, &pk, &req(c.run, agent("reviewer"), now)),
                "{what}"
            );
            assert!(
                datalog(&token, &pk, &req(c.run, tool("fs.read"), now)),
                "{what}"
            );
        }
    }

    #[test]
    fn datalog_alone_denies_spawning_without_the_spawn_tool() {
        let key = RootKey::generate();
        let pk = key.public();
        let mut c = cap(spawn(1));
        c.tools.remove(SPAWN_TOOL);
        let token = issue(&key, &c).expect("issues");
        let now = at(1_800_000_000);
        assert!(!datalog(&token, &pk, &req(c.run, agent("reviewer"), now)));
        assert!(datalog(&token, &pk, &req(c.run, tool("fs.read"), now)));
    }

    #[test]
    fn datalog_alone_denies_a_negative_spend() {
        let key = RootKey::generate();
        let pk = key.public();
        let c = cap(None);
        let token = issue(&key, &c).expect("issues");
        let now = at(1_800_000_000);
        for amount in [-1, i64::MIN] {
            let spend = Action::Spend(MicroUsd(amount));
            assert!(!datalog(&token, &pk, &req(c.run, spend, now)), "{amount}");
        }
        let zero = Action::Spend(MicroUsd(0));
        assert!(datalog(&token, &pk, &req(c.run, zero, now)));
    }

    #[test]
    fn datalog_alone_enforces_the_child_block_and_its_run() {
        let key = RootKey::generate();
        let pk = key.public();
        let parent = cap(spawn(2));
        let root = issue(&key, &parent).expect("issues");
        let child = Capability {
            run: RunId::new(),
            tools: names(&["fs.read"]),
            spawn: None,
            budget: MicroUsd(10),
            ..parent.clone()
        };
        let token = attenuate(&root, &key, &child).expect("attenuates");
        let now = at(1_800_000_000);
        assert!(datalog(&token, &pk, &req(child.run, tool("fs.read"), now)));
        assert!(!datalog(
            &token,
            &pk,
            &req(parent.run, tool("fs.read"), now)
        ));
        assert!(!datalog(
            &token,
            &pk,
            &req(child.run, tool(SPAWN_TOOL), now)
        ));
        let spend = Action::Spend(MicroUsd(11));
        assert!(!datalog(&token, &pk, &req(child.run, spend, now)));
    }

    #[test]
    fn a_sub_second_expiry_is_exact_in_the_grant_and_never_late_in_datalog() {
        let key = RootKey::generate();
        let pk = key.public();
        let mut c = cap(None);
        c.expires_at = at(1_800_000_000) + Duration::milliseconds(500);
        let token = issue(&key, &c).expect("issues");
        assert_eq!(grant(&token, &pk).expect("verifies"), c);

        let just_after = c.expires_at + Duration::nanoseconds(1);
        for now in [c.expires_at, just_after] {
            let d = verify(&token, &pk, &req(c.run, tool("fs.read"), now));
            assert!(
                matches!(&d, Decision::Deny(r) if r.contains("expired")),
                "{d:?}"
            );
            assert!(!datalog(&token, &pk, &req(c.run, tool("fs.read"), now)));
        }
        // Inside the expiry second the Datalog check denies early, and so does `verify`.
        let early = c.expires_at - Duration::milliseconds(100);
        assert!(!datalog(&token, &pk, &req(c.run, tool("fs.read"), early)));
        assert!(matches!(
            verify(&token, &pk, &req(c.run, tool("fs.read"), early)),
            Decision::Deny(_)
        ));
        let before = at(1_800_000_000) - Duration::milliseconds(1);
        assert_eq!(
            verify(&token, &pk, &req(c.run, tool("fs.read"), before)),
            Decision::Allow
        );
    }

    #[test]
    fn an_expiry_before_1970_round_trips_and_denies() {
        let key = RootKey::generate();
        let pk = key.public();
        let mut c = cap(None);
        c.expires_at = at(-10) + Duration::milliseconds(250);
        let token = issue(&key, &c).expect("issues");
        assert_eq!(grant(&token, &pk).expect("verifies"), c);
        let d = verify(&token, &pk, &req(c.run, tool("fs.read"), at(-20)));
        assert!(matches!(d, Decision::Deny(_)), "{d:?}");
    }

    #[test]
    fn verify_denies_what_only_an_appended_block_check_denies() {
        let key = RootKey::generate();
        let pk = key.public();
        let c = cap(None);
        let root = issue(&key, &c).expect("issues");
        let parsed = parse(&root, &pk).expect("parses");
        let block = BlockBuilder::new()
            .code("check if false;")
            .expect("parses")
            .context(encode(&c).expect("encodes"));
        let forged = append_signed(&parsed, &key, block).expect("appends");
        assert_eq!(grant(&forged, &pk).expect("verifies"), c);
        let d = verify(
            &forged,
            &pk,
            &req(c.run, tool("fs.read"), at(1_800_000_000)),
        );
        assert!(
            matches!(&d, Decision::Deny(r) if r.contains("denied by the token's checks")),
            "{d:?}"
        );
    }

    fn data(c: &Capability) -> serde_json::Value {
        serde_json::from_str(&encode(c).expect("encodes")).expect("is json")
    }

    fn decode_err(value: &serde_json::Value) -> String {
        match decode(3, Some(&value.to_string())) {
            Err(CapError::InvalidToken(why)) => why,
            other => panic!("expected InvalidToken, got {other:?}"),
        }
    }

    #[test]
    fn decode_round_trips_and_rejects_bad_block_data() {
        let c = cap(spawn(1));
        let text = encode(&c).expect("encodes");
        assert_eq!(decode(0, Some(&text)).expect("decodes"), c);

        assert!(matches!(decode(1, None), Err(CapError::InvalidToken(_))));

        let mut v = data(&c);
        v["v"] = 2.into();
        assert!(decode_err(&v).contains("version 2"));

        let mut v = data(&c);
        v["expires_nanos"] = 1_000_000_000u32.into();
        assert!(decode_err(&v).contains("nanoseconds"));

        let mut v = data(&c);
        v["expires_unix"] = i64::MAX.into();
        assert!(decode_err(&v).contains("bad expiry"));

        let mut v = data(&c);
        v["extra"] = true.into();
        assert!(decode_err(&v).starts_with("block 3: malformed"));
    }

    #[test]
    fn library_errors_map_to_token_errors() {
        let e = token_error(biscuit_auth::error::Token::InternalError);
        assert!(matches!(&e, CapError::Token(_)));
        assert!(e.to_string().starts_with("capability token error: "));
    }
}
