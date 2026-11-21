//! M2 acceptance scenarios (issue #45): spawn trees on real Docker and gVisor. The parent and its
//! children are the echo agent (`tests/e2e/echo-agent/claude.sh`), driven by markers in their input
//! (`fanout=6`, `c.commit=1`, ...): the parent calls `kestrel.spawn` and `kestrel.join` at the
//! Kestrel server URL in its `--mcp-config` file, and each child makes one metered model call
//! against the mock provider, does what its markers say, and exits.
//!
//! Every test runs `kestrel up` (serve plus the HTTP API) and reads real state: `kestrel ps`,
//! `kestrel watch` events, the store (parents, budgets, capability tokens), `git` in the source repo
//! and the clones, and `docker ps` / `docker network ls` filtered by the `kestrel.instance` label.
//! Scenario 11 (Travsr) lands with issue #44; scenarios 12 and 13 (the UI) are in `tests/ui.rs`.
//!
//! Needs what `tests/e2e.rs` needs. Run with
//! `cargo test -p kestrel-e2e --features e2e --test m2 -- --test-threads=1 --nocapture`.

// Helpers panic on purpose: a panic is how a test fails.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

use common::{
    Container, Env, ONE_CALL_MICRO_USD, RUN_DEADLINE, Sampler, USAGE_CACHE_READ, USAGE_CACHE_WRITE,
    USAGE_IN, USAGE_OUT, branch_of, containers, git, network_info, networks, of_kind, run_id,
    status_event, status_name, ts,
};

/// A child that only does what its markers say: no Kestrel tools.
const WORKER_CAPS: &str = "";

/// A parent that may spawn `n` children of `agents`, one level down.
fn lead_caps(n: u32, agents: &str) -> String {
    format!(
        "mcp: [kestrel.spawn, kestrel.join], spawn: {{ max_children: {n}, max_depth: 1, agents: [{agents}] }}"
    )
}

/// Three more tracked files in the source repo, so the clones have something to clone. Returns
/// the commit `main` is on.
fn seed(env: &Env) -> String {
    std::fs::create_dir_all(env.repo.join("src")).expect("mkdir src");
    std::fs::write(env.repo.join("src").join("lib.txt"), "fn main() {}\n").expect("write");
    std::fs::write(env.repo.join("notes.txt"), "notes\n").expect("write");
    std::fs::write(env.repo.join("data.csv"), "a,b\n1,2\n").expect("write");
    git(&env.repo, &["add", "-A"]);
    git(
        &env.repo,
        &[
            "-c",
            "user.name=e2e",
            "-c",
            "user.email=e2e@example.invalid",
            "commit",
            "-q",
            "-m",
            "seed",
        ],
    );
    git(&env.repo, &["rev-parse", "HEAD"]).trim().to_string()
}

/// `N` from the `kid=N` word of a child's input.
fn kid_number(input: &str) -> String {
    input
        .split_whitespace()
        .find_map(|w| w.strip_prefix("kid="))
        .unwrap_or_else(|| panic!("no kid= in the child input {input:?}"))
        .to_string()
}

/// The part of an echo agent report after the line `== <title>`, up to the next `== ` line.
fn section<'a>(text: &'a str, title: &str) -> &'a str {
    let head = format!("== {title}\n");
    let start = text
        .find(&head)
        .map(|i| i + head.len())
        .unwrap_or_else(|| panic!("no `== {title}` section in:\n{text}"));
    let rest = &text[start..];
    match rest.find("\n== ") {
        Some(end) => &rest[..end],
        None => rest,
    }
}

/// Polls `check` until it is true, or panics with `what` after `limit`.
fn wait_until(limit: Duration, what: &str, mut check: impl FnMut() -> bool) {
    let start = Instant::now();
    while !check() {
        assert!(start.elapsed() < limit, "{what} not true after {limit:?}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The `capabilities_dropped` event of a child, which must exist.
fn dropped(env: &Env, child: &str) -> Value {
    let events = env.events_now(child);
    of_kind(&events, "capabilities_dropped")
        .first()
        .map(|e| e["kind"].clone())
        .unwrap_or_else(|| panic!("no capabilities_dropped event on {child}: {events:?}"))
}

/// Each `model_call` event of `id` cost exactly the mock's usage times the configured prices, and
/// the store's spend of `id` is their sum. Returns how many calls there were.
fn assert_exact_cost(env: &Env, id: &str) -> usize {
    let events = env.events_now(id);
    let calls = of_kind(&events, "model_call");
    for call in &calls {
        let k = &call["kind"];
        assert_eq!(k["tokens_in"].as_u64(), Some(USAGE_IN), "{call}");
        assert_eq!(k["tokens_out"].as_u64(), Some(USAGE_OUT), "{call}");
        assert_eq!(
            k["tokens_cache_read"].as_u64(),
            Some(USAGE_CACHE_READ),
            "{call}"
        );
        assert_eq!(
            k["tokens_cache_write"].as_u64(),
            Some(USAGE_CACHE_WRITE),
            "{call}"
        );
        assert_eq!(k["cost"].as_i64(), Some(ONE_CALL_MICRO_USD), "{call}");
    }
    let n = calls.len();
    assert_eq!(
        env.spent(id),
        ONE_CALL_MICRO_USD * i64::try_from(n).expect("count"),
        "run {id}: the store's spend is its {n} model calls at the configured prices"
    );
    n
}

/// The source repo still has `main` checked out at `main_sha`, a clean worktree, and the tracked
/// files of `files` (`git ls-files -s`).
fn assert_source_untouched(env: &Env, main_sha: &str, files: &str) {
    assert_eq!(
        git(&env.repo, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "main",
        "the source repo's checked-out branch moved"
    );
    assert_eq!(git(&env.repo, &["rev-parse", "main"]).trim(), main_sha);
    assert_eq!(
        git(&env.repo, &["status", "--porcelain"]).trim(),
        "",
        "the source repo's worktree changed"
    );
    assert_eq!(git(&env.repo, &["ls-files", "-s"]), files);
}

/// Brings the branch of `run` home into the source repo (`fetch_back`, as in M1 scenario 6).
fn fetch_back(env: &Env, run: &str) {
    let workspaces = kestrel_core::Workspaces::new(
        env.dir.path().join("workspaces"),
        Arc::new(kestrel_sandbox::ProcessRunner),
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(workspaces.fetch_back(run_id(run), &env.repo))
        .unwrap_or_else(|e| panic!("fetch_back of {run}: {e}"));
}

/// Scenarios 1 and 2: a parent spawns 6 children that each commit in their own clone (`same`:
/// all to the same path), and joins.
fn fan_out_and_commit(same: bool) {
    let env = Env::with_max(8);
    let main_sha = seed(&env);
    let files = git(&env.repo, &["ls-files", "-s"]);
    env.spawnable("worker", "1", WORKER_CAPS);
    let lead = env.root_agent("lead", "5", &lead_caps(6, "worker"));
    let _up = env.up();
    let markers = if same { " c.same=1" } else { "" };
    let root = env.submit_file(
        &lead,
        &format!("fanout=6 each=0.20 c.commit=1 c.nap=3{markers}"),
    );
    env.wait_status(&root, "Completed");

    let kids = env.children(&root);
    assert_eq!(kids.len(), 6, "6 children spawned");
    let mut numbers = BTreeSet::new();
    let mut clones = BTreeSet::new();
    let mut branches = BTreeSet::new();
    for kid in &kids {
        let id = kid.id.to_string();
        let row = env.wait_status(&id, "Completed");
        assert_eq!(row["agent"], "worker");
        assert_eq!(assert_exact_cost(&env, &id), 1, "one model call per child");
        let n = kid_number(&kid.input);
        assert!(numbers.insert(n.clone()), "kid {n} twice");

        // Its own clone, on its own branch, holding exactly its one commit on top of main.
        let ws = env.workspace(&id);
        let top = git(&ws, &["rev-parse", "--show-toplevel"]);
        let top = std::fs::canonicalize(top.trim()).expect("clone top level");
        assert_eq!(top, std::fs::canonicalize(&ws).expect("workspace"));
        assert!(clones.insert(top), "two children share a working directory");
        // A real clone with its own objects, not one that borrows the source repo's.
        assert!(
            !ws.join(".git")
                .join("objects")
                .join("info")
                .join("alternates")
                .exists(),
            "kid {n}'s clone borrows objects through .git/objects/info/alternates"
        );
        // Nor shares them through hardlinks: every object file has a link count of 1.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let objects = files_under(&ws.join(".git").join("objects"));
            assert!(!objects.is_empty(), "kid {n}'s clone has no object files");
            for file in &objects {
                let links = std::fs::metadata(file).expect("object file").nlink();
                assert_eq!(
                    links,
                    1,
                    "{} in kid {n}'s clone is hardlinked ({links} links)",
                    file.display()
                );
            }
        }
        let branch = branch_of(&id);
        assert!(
            branches.insert(branch.clone()),
            "two children share {branch}"
        );
        assert_eq!(
            git(&ws, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            branch
        );
        let report = env.read_out(&id, "commit.out");
        assert!(report.contains("commit_rc=0"), "kid {n}: {report}");
        assert_eq!(
            git(&ws, &["status", "--porcelain"]).trim(),
            "",
            "kid {n}'s clone is not clean"
        );
        assert_eq!(
            git(&ws, &["log", "-1", "--format=%s"]).trim(),
            format!("kid {n}")
        );
        assert_eq!(git(&ws, &["rev-parse", "HEAD^"]).trim(), main_sha);
        let file = if same {
            "shared.txt".to_string()
        } else {
            format!("kid-{n}.txt")
        };
        assert_eq!(
            std::fs::read_to_string(ws.join(&file)).expect("the child's file"),
            format!("work of kid {n}\n")
        );
    }
    let want: BTreeSet<String> = (1..=6).map(|n| n.to_string()).collect();
    assert_eq!(numbers, want, "kids 1 to 6, each once");

    // Nothing reached the source repo while the children worked.
    assert_source_untouched(&env, &main_sha, &files);
    assert_eq!(
        git(&env.repo, &["branch", "--format=%(refname:short)"]).trim(),
        "main"
    );

    // Each branch comes home on its own: no conflict, each holds its own version.
    let mut versions = BTreeSet::new();
    for kid in &kids {
        let id = kid.id.to_string();
        let n = kid_number(&kid.input);
        fetch_back(&env, &id);
        let branch = branch_of(&id);
        let file = if same {
            "shared.txt".to_string()
        } else {
            format!("kid-{n}.txt")
        };
        let content = git(&env.repo, &["show", &format!("{branch}:{file}")]);
        assert_eq!(content, format!("work of kid {n}\n"), "{branch}:{file}");
        assert!(
            versions.insert(content),
            "two branches hold the same version"
        );
        assert_eq!(
            git(&env.repo, &["rev-parse", &format!("{branch}^")]).trim(),
            main_sha,
            "{branch} is one commit on top of main"
        );
    }
    let listed = git(&env.repo, &["branch", "--format=%(refname:short)"]);
    assert_eq!(
        listed.lines().count(),
        7,
        "main and 6 run branches: {listed}"
    );
    // fetch_back never moves the user's checkout or touches its files.
    assert_source_untouched(&env, &main_sha, &files);
    assert!(!env.repo.join("shared.txt").exists());
}

/// Scenario 1: 6 children complete, each in its own clone on `kestrel/run-<id>`, each commit
/// visible on its own branch in the source repo, which keeps its branch and files.
#[test]
fn m2_s01_fan_out_to_six_children_in_their_own_clones() {
    fan_out_and_commit(false);
}

/// Scenario 2: all 6 children edit the same path. Every branch holds its own version and nothing
/// conflicts.
#[test]
fn m2_s02_same_file_edits_never_conflict() {
    fan_out_and_commit(true);
}

/// The parent of scenarios 3 to 5: it holds `kestrel.spawn`, `kestrel.join`,
/// `travsr.search_symbol`, the host `api.github.com` and the secret `github_token`.
const BOSS_CAPS: &str = "mcp: [kestrel.spawn, kestrel.join, travsr.search_symbol], \
    network: [api.github.com], secrets: [github_token], \
    spawn: { max_children: 2, max_depth: 1, agents: [probe] }";

/// The child of scenarios 3 to 5 asks for more of each.
const PROBE_CAPS: &str = "mcp: [kestrel.join, kestrel.ask_human, travsr.search_symbol, travsr.get_callers], \
    network: [api.github.com, evil.example], secrets: [github_token, aws_key]";

/// Runs the parent with one `probe` child that gets `markers`, to the end. Returns (parent, child).
fn probe_tree(env: &Env, markers: &str) -> (String, String) {
    env.spawnable("probe", "1", PROBE_CAPS);
    let boss = env.root_agent("boss", "5", BOSS_CAPS);
    let root = env.submit_file(&boss, &format!("fanout=1 kids=probe each=0.20 {markers}"));
    env.wait_status(&root, "Completed");
    let kids = env.children(&root);
    assert_eq!(kids.len(), 1, "one child");
    let child = kids[0].id.to_string();
    env.wait_status(&child, "Completed");
    (root, child)
}

/// The capabilities the store recorded for a run: its granted spec.
fn granted(env: &Env, id: &str) -> kestrel_types::Capabilities {
    env.store()
        .run_spec(run_id(id))
        .expect("read spec")
        .unwrap_or_else(|| panic!("no stored spec for {id}"))
        .capabilities
}

/// Names as a set, so order does not matter.
fn set<S: AsRef<str>>(names: &[S]) -> BTreeSet<String> {
    names.iter().map(|s| s.as_ref().to_string()).collect()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap_or_else(|| panic!("not a list: {v}"))
        .iter()
        .map(|s| s.as_str().expect("a string").to_string())
        .collect()
}

/// Scenario 3: a child asking for tools its parent lacks does not get them. `tools/list` does not
/// show them, and a direct call is refused with `not permitted` and recorded. A call of a tool it
/// holds (`kestrel.join`) on the same route succeeds.
///
/// The gateway serves only the `kestrel.*` tools until the Travsr route lands (#44), so the
/// direct call is made to `kestrel.ask_human`; `travsr.get_callers` is checked in the grant.
#[test]
fn m2_s03_child_cannot_use_a_tool_its_parent_lacks() {
    let env = Env::with_max(8);
    let _up = env.up();
    let (root, child) = probe_tree(&env, "c.tools=1");

    assert_eq!(
        set(&granted(&env, &child).mcp),
        set(&["kestrel.join", "travsr.search_symbol"]),
        "the child holds only the tools both asked for and the parent holds"
    );
    let cut = dropped(&env, &child);
    assert_eq!(
        set(&strings(&cut["tools"])),
        set(&["kestrel.ask_human", "travsr.get_callers"])
    );
    // The parent's own grant is unchanged by its child's spec.
    assert_eq!(
        set(&granted(&env, &root).mcp),
        set(&["kestrel.spawn", "kestrel.join", "travsr.search_symbol"])
    );

    let report = env.read_out(&child, "tools.out");
    let allowed = report.lines().next().unwrap_or("");
    assert!(allowed.contains("mcp__kestrel__kestrel_join"), "{report}");
    assert!(allowed.contains("mcp__travsr__search_symbol"), "{report}");
    assert!(!allowed.contains("get_callers"), "{report}");
    assert!(!allowed.contains("ask_human"), "{report}");
    let listed = section(&report, "tools/list");
    assert!(listed.contains(r#""name":"kestrel.join""#), "{report}");
    assert!(!listed.contains("kestrel.ask_human"), "{report}");
    assert!(!listed.contains("kestrel.spawn"), "{report}");
    let call = section(&report, "kestrel.ask_human");
    assert!(call.contains("not permitted"), "{report}");
    assert!(call.contains(r#""isError":true"#), "{report}");
    // The control: the same child, the same route, a tool it holds. It has no children, so the
    // join answers at once with none.
    let call = section(&report, "kestrel.join");
    assert!(call.contains(r#""isError":false"#), "{report}");
    assert!(!call.contains(r#""isError":true"#), "{report}");
    assert!(call.contains(r#""children":[]"#), "{report}");

    let events = env.events(&child);
    assert!(
        of_kind(&events, "agent_log")
            .iter()
            .any(|e| e["kind"]["line"]
                .as_str()
                .is_some_and(|l| l.contains("kestrel.ask_human is not permitted"))),
        "the refused call is recorded on the child: {events:?}"
    );
    assert!(
        of_kind(&events, "approval_requested").is_empty(),
        "the refused question never reached a person"
    );
}

/// Scenario 4: a child asking for a host its parent lacks does not get it, and from its sandbox
/// every connection but the gateway fails, granted host or not (as in M1: there is no egress
/// proxy yet).
#[test]
fn m2_s04_child_cannot_reach_a_host_its_parent_lacks() {
    let env = Env::with_max(8);
    let _up = env.up();
    let (_root, child) = probe_tree(&env, "c.hosts=1");

    // The proof that the child does not get `evil.example` is its grant: the host is dropped.
    assert_eq!(granted(&env, &child).network, ["api.github.com"]);
    assert_eq!(strings(&dropped(&env, &child)["hosts"]), ["evil.example"]);

    // `evil.example` is under the reserved `.example` domain and never resolves anywhere, so its
    // failed probe proves nothing. The network is shown closed by the probes that answer on an open
    // network: 1.1.1.1 (no DNS needed) and api.github.com (a host the child was granted).
    let egress = env.read_out(&child, "egress.txt");
    let lines: Vec<&str> = egress.lines().collect();
    assert_eq!(lines.len(), 4, "four hosts tried: {egress:?}");
    assert!(egress.contains("https://evil.example"), "{egress}");
    assert!(egress.contains("https://api.github.com"), "{egress}");
    // An address needs no DNS, so its probe can only fail on the connection itself.
    let by_address = lines
        .iter()
        .find(|l| l.ends_with(" https://1.1.1.1"))
        .unwrap_or_else(|| panic!("no probe of 1.1.1.1: {egress:?}"));
    assert!(
        by_address.starts_with("blocked 7 ") || by_address.starts_with("blocked 28 "),
        "1.1.1.1 should fail to connect or time out: {by_address}"
    );
    for line in lines {
        // As in M1 scenario 3: only a DNS failure (6), a refused connection (7) or a timeout (28)
        // proves the network is closed.
        let mut parts = line.split(' ');
        assert_eq!(parts.next(), Some("blocked"), "egress should fail: {line}");
        let code = parts.next().and_then(|c| c.parse::<i32>().ok());
        assert!(
            matches!(code, Some(6 | 7 | 28)),
            "egress failed for the wrong reason: {line}"
        );
    }
    // The same sandbox reached the gateway, so the block is selective.
    assert_eq!(assert_exact_cost(&env, &child), 1);
}

/// The server's root key, from `root.key` next to the database (hex).
fn root_public_key(env: &Env) -> kestrel_core::caps::RootPublicKey {
    let text = std::fs::read_to_string(env.dir.path().join("root.key")).expect("root.key");
    let text = text.trim();
    let bytes: Vec<u8> = (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect();
    kestrel_core::caps::RootKey::from_bytes(&bytes)
        .expect("a root key")
        .public()
}

/// Scenario 5: a child asking for a secret its parent lacks does not get it, and its token
/// refuses it. Secret injection is M3 work, so this checks the grant, not a live injection.
#[test]
fn m2_s05_child_gets_no_secret_its_parent_lacks() {
    use kestrel_core::caps::{Action, CapToken, Decision, Request, grant, verify};

    let env = Env::with_max(8);
    let _up = env.up();
    let (root, child) = probe_tree(&env, "");

    assert_eq!(granted(&env, &child).secrets, ["github_token"]);
    assert_eq!(strings(&dropped(&env, &child)["secrets"]), ["aws_key"]);

    let key = root_public_key(&env);
    let bytes = env
        .store()
        .run_cap(run_id(&child))
        .expect("read token")
        .expect("the child has a capability token");
    let token = CapToken::from_bytes(bytes);
    let cap = grant(&token, &key).expect("the child's token verifies against the root key");
    assert_eq!(
        cap.secrets.iter().map(String::as_str).collect::<Vec<_>>(),
        ["github_token"]
    );
    let check = |run: &str, secret: &str| {
        verify(
            &token,
            &key,
            &Request {
                run: run_id(run),
                action: Action::Secret(secret.to_string()),
                now: time::OffsetDateTime::now_utc(),
            },
        )
    };
    assert_eq!(check(&child, "github_token"), Decision::Allow);
    match check(&child, "aws_key") {
        Decision::Deny(reason) => assert!(reason.contains("aws_key"), "{reason}"),
        Decision::Allow => panic!("the child's token allows aws_key"),
    }
    // The token is bound to the child: presented for the parent it allows nothing.
    assert!(matches!(check(&root, "github_token"), Decision::Deny(_)));
}

/// Scenario 6: a child cannot spawn with more budget than it has, a grandchild at max_depth 0
/// cannot spawn at all, and no sandbox holds any token but its own: not in its environment, under
/// `/kestrel` or its mounts, nor in any file of its workspace.
#[test]
fn m2_s06_child_cannot_escalate_budget_depth_or_token() {
    let env = Env::with_max(8);
    env.spawnable(
        "esc",
        "1",
        "mcp: [kestrel.spawn, kestrel.join], spawn: { max_children: 3, max_depth: 5, agents: [esc] }",
    );
    let lead = env.root_agent(
        "lead",
        "5",
        "mcp: [kestrel.spawn, kestrel.join], spawn: { max_children: 2, max_depth: 2, agents: [esc] }",
    );
    let _up = env.up();
    let root = env.submit_file(
        &lead,
        "fanout=1 kids=esc each=0.10 c.escalate=1 c.kids=esc c.dump=1",
    );
    env.wait_status(&root, "Completed");
    let child = env.children(&root)[0].id.to_string();
    env.wait_status(&child, "Completed");
    let grandkids = env.children(&child);
    assert_eq!(
        grandkids.len(),
        1,
        "the over-budget spawn created nothing, the other one a grandchild"
    );
    let grandchild = grandkids[0].id.to_string();
    env.wait_status(&grandchild, "Completed");

    // Depth: asked for 5, recorded as what is left below the parent.
    let depth = |id: &str| granted(&env, id).spawn.map(|s| s.max_depth);
    assert_eq!(depth(&child), Some(1));
    assert_eq!(depth(&grandchild), Some(0));

    let report = env.read_out(&child, "escalate.out");
    let over = section(&report, "spawn over budget");
    assert!(over.contains(r#""isError":true"#), "{report}");
    assert!(over.contains("asked for"), "{report}");
    let spawned = section(&report, "spawn grandchild");
    assert!(spawned.contains(&grandchild), "{report}");

    let report = env.read_out(&grandchild, "escalate.out");
    let listed = section(&report, "tools/list");
    assert!(listed.contains(r#""name":"kestrel.join""#), "{report}");
    assert!(!listed.contains("kestrel.spawn"), "{report}");
    let deeper = section(&report, "spawn below max_depth 0");
    assert!(deeper.contains("not permitted"), "{report}");
    assert!(env.children(&grandchild).is_empty());
    let events = env.events(&grandchild);
    assert!(
        of_kind(&events, "agent_log")
            .iter()
            .any(|e| e["kind"]["line"]
                .as_str()
                .is_some_and(|l| l.contains("kestrel.spawn is not permitted"))),
        "the refused spawn is recorded: {events:?}"
    );

    // Tokens: each sandbox's environment, mounts and workspace hold its own run token and no other.
    let store = env.store();
    let token = |id: &str| {
        store
            .run_token(run_id(id))
            .expect("read token")
            .unwrap_or_else(|| panic!("no run token for {id}"))
    };
    let tokens = [
        (root.clone(), token(&root)),
        (child.clone(), token(&child)),
        (grandchild.clone(), token(&grandchild)),
    ];
    for (id, own) in &tokens[1..] {
        let dump = env.read_out(id, "dump.out");
        // /kestrel was read: a file under it holds the run's own MCP URL. An empty or unreadable
        // /kestrel fails here, not only in the token search below.
        let mcp = format!("/r/{own}/mcp");
        assert!(
            sections(&dump)
                .iter()
                .any(|(title, body)| title.starts_with("/kestrel/") && body.contains(&mcp)),
            "no file under /kestrel in the dump of {id} holds {mcp}:\n{dump}"
        );
        assert_mounts(id, &dump);
        let seen = format!("{}\n{dump}", env.read_out(id, "env.txt"));
        assert!(
            seen.contains(&format!("/r/{own}")),
            "run {id} reaches the gateway with its own token"
        );
        for (other, theirs) in &tokens {
            if other != id {
                assert!(
                    !seen.contains(theirs.as_str()),
                    "the sandbox of {id} holds the token of {other}"
                );
            }
        }
    }
    // Every file of every workspace, `.git` included, read on the host. Git compresses the objects
    // it stores, so every object is also read uncompressed, reachable or not.
    for (id, _) in &tokens {
        let ws = env.workspace(id);
        let files = files_under(&ws);
        assert!(
            files.iter().any(|f| f.ends_with("env.txt")),
            "the walk of {} found the agent's files",
            ws.display()
        );
        let objects = all_objects(&ws);
        for (other, theirs) in &tokens {
            if other == id {
                continue;
            }
            for file in &files {
                let bytes =
                    std::fs::read(file).unwrap_or_else(|e| panic!("read {} ({e})", file.display()));
                assert!(
                    !contains_bytes(&bytes, theirs.as_bytes()),
                    "{} in the workspace of {id} holds the token of {other}",
                    file.display()
                );
            }
            assert!(
                !contains_bytes(&objects, theirs.as_bytes()),
                "a git object in the workspace of {id} holds the token of {other}"
            );
        }
    }
}

/// Every `== <title>` section of an echo agent report, as (title, body), in order.
fn sections(text: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("== ") {
        if i > 0 && !rest[..i].ends_with('\n') {
            rest = &rest[i + 3..];
            continue;
        }
        let after = &rest[i + 3..];
        let (title, body_on) = after.split_once('\n').unwrap_or((after, ""));
        let end = body_on.find("\n== ").map_or(body_on.len(), |e| e + 1);
        out.push((title, &body_on[..end]));
        rest = &body_on[end..];
    }
    out
}

/// File systems the kernel provides, which hold no host files.
const KERNEL_FS: [&str; 12] = [
    "proc",
    "sysfs",
    "tmpfs",
    "devtmpfs",
    "devpts",
    "mqueue",
    "cgroup",
    "cgroup2",
    "securityfs",
    "debugfs",
    "tracefs",
    "bpf",
];

/// The `/proc/mounts` of a dump lists only the mounts the Docker driver of `kestrel-sandbox` makes
/// (`/workspace` and `/kestrel`, both bind mounts) and the ones Docker adds to every container
/// (`/etc/hosts`, `/etc/hostname`, `/etc/resolv.conf`), besides the root, `/dev` (every runtime
/// mounts it; gVisor emulates it under its own file system name, with no host files) and the
/// kernel's file systems. The echo agent read every one of them but `/workspace`, which the test reads on the host.
fn assert_mounts(id: &str, dump: &str) {
    let mounted: BTreeSet<&str> = section(dump, "/proc/mounts")
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (_, point, fs) = (f.next()?, f.next()?, f.next()?);
            (point != "/" && !KERNEL_FS.contains(&fs)).then_some(point)
        })
        .collect();
    let driver = ["/workspace", "/kestrel"];
    let docker = ["/dev", "/etc/hosts", "/etc/hostname", "/etc/resolv.conf"];
    for point in driver {
        assert!(
            mounted.contains(point),
            "{point} is not in the /proc/mounts of {id}: {mounted:?}"
        );
    }
    for point in &mounted {
        assert!(
            driver.contains(point) || docker.contains(point),
            "the sandbox of {id} has a mount the driver does not make: {point} ({mounted:?})"
        );
    }
    let read: BTreeSet<&str> = sections(dump)
        .iter()
        .filter_map(|(title, _)| title.strip_prefix("mount "))
        .collect();
    // `/dev` holds the runtime's emulated devices, not host files, so it is not read.
    for point in mounted.iter().filter(|p| !driver.contains(p) && **p != "/dev") {
        assert!(
            read.contains(point),
            "the agent of {id} did not read the mount {point}"
        );
    }
}

/// Every regular file under `dir`, at any depth, `.git` included. Symlinks are not followed.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        let entries =
            std::fs::read_dir(&d).unwrap_or_else(|e| panic!("list {} ({e})", d.display()));
        for entry in entries {
            let entry = entry.expect("a directory entry");
            let kind = entry.file_type().expect("its file type");
            if kind.is_dir() {
                todo.push(entry.path());
            } else if kind.is_file() {
                files.push(entry.path());
            }
        }
    }
    files
}

/// Every object of the git repo at `dir`, uncompressed, as `git cat-file --batch-all-objects
/// --batch` prints them: loose and packed, reachable or not. Bytes, since trees hold binary ids.
fn all_objects(dir: &Path) -> Vec<u8> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["cat-file", "--batch-all-objects", "--batch"])
        .output()
        .expect("start git");
    assert!(
        out.status.success(),
        "git cat-file in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.stdout.is_empty(),
        "no git objects in {}",
        dir.display()
    );
    out.stdout
}

/// True when `needle` occurs in `haystack`.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Scenario 7: a root of $1.00 carves $0.15 for each of 6 children that each try to spend $0.30.
/// Each is held to its own $0.15 plus at most one call and suspended, the tree stays within the
/// root budget, and every cost is exact to the micro-dollar.
///
/// The overshoot is by design (architecture section 3.3, Budget): the gateway checks the limit
/// before each call, so the call that crosses it goes through and the next one is refused.
#[test]
fn m2_s07_budget_carving_holds_each_child_to_its_share() {
    let env = Env::with_max(8);
    env.spawnable("worker", "1", WORKER_CAPS);
    let lead = env.root_agent("lead", "1.00", &lead_caps(6, "worker"));
    let up = env.up();
    // $0.30 is 47 calls of 6465 micro-USD.
    let root = env.submit_file(&lead, "fanout=6 each=0.15 c.spend=47");
    let kids = env.wait_children(&root, 6);
    let share = 150_000;
    // The gateway refuses a call once the spend reaches the limit, so a child stops after the
    // first call that crosses it: $0.15 plus less than one call (155160 = 24 calls).
    let at_share = (share + ONE_CALL_MICRO_USD - 1) / ONE_CALL_MICRO_USD * ONE_CALL_MICRO_USD;
    assert!(at_share - share < ONE_CALL_MICRO_USD);
    let mut children_spent = 0;
    let mut calls = 0;
    for kid in &kids {
        let id = kid.id.to_string();
        env.wait_status(&id, "Suspended(Budget)");
        let n = assert_exact_cost(&env, &id);
        calls += n;
        let spent = env.spent(&id);
        assert_eq!(
            spent, at_share,
            "child {id} is held to its $0.15 plus at most one call ({n} calls)"
        );
        // What the agent recorded. It asked for 47 calls and writes spend.out after each one, but
        // a budget tick can stop its sandbox between a call and that write, so the refusal (or
        // the last call) may be missing. The exact spend above, 24 calls and not 47, already
        // proves the cut; the file must only agree with it.
        let progress = env.read_out(&id, "spend.out");
        let made: usize = progress
            .split_whitespace()
            .find_map(|w| w.strip_prefix("calls="))
            .and_then(|c| c.parse().ok())
            .unwrap_or_else(|| panic!("child {id}: no calls= in spend.out: {progress:?}"));
        assert!(n < 47, "child {id} made all {n} calls it asked for");
        assert!(
            !progress.contains("refused=no"),
            "child {id} made all its calls: {progress}"
        );
        if progress.contains("refused=yes") {
            assert_eq!(made, n, "child {id}: {progress}");
        } else {
            assert!(
                made == n || made + 1 == n,
                "child {id} recorded {made} of its {n} calls: {progress}"
            );
        }
        children_spent += spent;
    }
    let events = env.events_now(&root);
    let carved: Vec<i64> = of_kind(&events, "child_spawned")
        .iter()
        .map(|e| e["kind"]["budget"].as_i64().expect("budget"))
        .collect();
    assert_eq!(carved, vec![share; 6], "each child got $0.15 carved out");
    calls += assert_exact_cost(&env, &root);
    let parent_spent = env.spent(&root);
    // Given each child's pin above, this cannot fail on its own: the tree stays within $1.00
    // because $0.10 of it is not carved out, and the overshoots and the parent's call fit in it.
    assert!(
        children_spent <= 1_000_000 - parent_spent,
        "children spent {children_spent}, more than $1.00 minus the parent's {parent_spent}"
    );
    // Every call the mock answered is counted once, and no refused call reached it.
    assert_eq!(env.mock.keys_seen().len(), calls);
    eprintln!(
        "m2-result: budget carving: 6 children held at {at_share} micro-USD each, tree spend {} of 1000000",
        children_spent + parent_spent
    );

    // Clean up through the API: the tree is cancelled and the spend stays as recorded.
    let (status, body) = up.http("POST", &format!("/api/runs/{root}/cancel"), None);
    assert_eq!(status, 202, "cancel: {body}");
    env.wait_status(&root, "Cancelled");
    for kid in &kids {
        let id = kid.id.to_string();
        env.wait_status(&id, "Cancelled");
        assert_eq!(env.spent(&id), at_share);
    }
}

/// Max number of intervals open at once. An interval that ends at the instant another starts
/// does not overlap it.
fn max_overlap(intervals: &[(time::OffsetDateTime, time::OffsetDateTime)]) -> usize {
    let mut edges: Vec<(time::OffsetDateTime, i32)> = Vec::new();
    for (start, end) in intervals {
        edges.push((*start, 1));
        edges.push((*end, -1));
    }
    edges.sort();
    let (mut open, mut most) = (0i32, 0i32);
    for (_, delta) in edges {
        open += delta;
        most = most.max(open);
    }
    usize::try_from(most).unwrap_or(0)
}

/// Scenario 8: with a host cap of 2, a parent spawns 5 children and joins. All 5 finish within
/// 60 s, never more than 2 children run at once, and the parent holds no slot while it waits.
///
/// The parent's sandbox keeps running inside its join (architecture section 3.3), so up to
/// cap + 1 sandboxes run: the cap counts the runs that hold a slot.
#[test]
fn m2_s08_full_cap_does_not_deadlock() {
    let cap = 2;
    let env = Env::with_max(cap);
    env.spawnable("worker", "1", WORKER_CAPS);
    let lead = env.root_agent("lead", "5", &lead_caps(5, "worker"));
    let _up = env.up();
    let instance = env.instance();
    let sampler = Sampler::start(Duration::from_millis(200), move || containers(&instance));
    let root = env.submit_file(&lead, "fanout=5 each=0.20 c.nap=4");
    env.wait_status(&root, "Completed");
    let samples = sampler.finish();

    let kids = env.children(&root);
    assert_eq!(kids.len(), 5);
    let root_events = env.events(&root);
    let started = ts(status_event(&root_events, "Running").expect("the parent ran"));
    let mut intervals = Vec::new();
    let mut last = started;
    for kid in &kids {
        let id = kid.id.to_string();
        env.wait_status(&id, "Completed");
        let events = env.events(&id);
        let from = ts(status_event(&events, "Provisioning").expect("provisioned"));
        let to = ts(status_event(&events, "Completed").expect("completed"));
        intervals.push((from, to));
        last = last.max(to);
    }
    let took = last - started;
    eprintln!(
        "m2-result: 5 children under a cap of {cap} finished {took} after the parent started"
    );
    assert!(
        took <= time::Duration::seconds(60),
        "the 5 children took {took}, more than 60 s"
    );

    // The event log: a run holds its slot from Provisioning to its end, so at most `cap` children
    // overlap. Exactly `cap` overlap only if the waiting parent gave its slot back.
    let overlap = max_overlap(&intervals);
    assert!(overlap <= cap, "{overlap} children held a slot at once");
    assert_eq!(
        overlap, cap,
        "with the parent in its join, {cap} children run at once (the parent holds no slot)"
    );

    // docker ps, sampled: never more than `cap` children running, and never more than the cap
    // plus the parent's own sandbox.
    let kid_ids: HashSet<String> = kids.iter().map(|k| k.id.to_string()).collect();
    let running = |cs: &[Container], only_kids: bool| {
        cs.iter()
            .filter(|c| c.state == "running")
            .filter(|c| !only_kids || kid_ids.contains(&c.run))
            .count()
    };
    let most_kids = samples
        .iter()
        .map(|(_, cs)| running(cs.as_slice(), true))
        .max()
        .unwrap_or(0);
    let most_all = samples
        .iter()
        .map(|(_, cs)| running(cs.as_slice(), false))
        .max()
        .unwrap_or(0);
    eprintln!(
        "m2-result: {} docker samples, at most {most_kids} children and {most_all} sandboxes running at once",
        samples.len()
    );
    assert!(!samples.is_empty(), "docker ps was sampled");
    assert!(most_kids <= cap, "{most_kids} children ran at once");
    assert!(most_all <= cap + 1, "{most_all} sandboxes ran at once");
}

/// Scenario 9: cancelling the root over the API cancels every run below it, and leaves no sandbox
/// container and no run network.
#[test]
fn m2_s09_cancel_removes_the_whole_subtree() {
    let env = Env::with_max(8);
    env.spawnable("worker", "1", WORKER_CAPS);
    env.spawnable("mid", "1", &lead_caps(2, "worker"));
    let lead = env.root_agent(
        "lead",
        "5",
        "mcp: [kestrel.spawn, kestrel.join], spawn: { max_children: 2, max_depth: 2, agents: [mid, worker] }",
    );
    let up = env.up();
    let instance = env.instance();
    // Two children, each with a grandchild that sleeps for minutes.
    let root = env.submit_file(
        &lead,
        "fanout=2 kids=mid each=0.50 c.fanout=1 c.kids=worker c.each=0.10 c.c.nap=300",
    );
    let mut tree = vec![root.clone()];
    for mid in env.wait_children(&root, 2) {
        let mid = mid.id.to_string();
        let leaf = env.wait_children(&mid, 1)[0].id.to_string();
        tree.push(mid);
        tree.push(leaf);
    }
    for id in &tree {
        env.wait_status(id, "Running");
    }
    let before: HashSet<String> = containers(&instance).into_iter().map(|c| c.run).collect();
    for id in &tree {
        assert!(
            before.contains(id),
            "run {id} has a sandbox before the cancel"
        );
    }
    assert!(
        !networks(&instance).is_empty(),
        "run networks before the cancel"
    );

    let (status, body) = up.http("POST", &format!("/api/runs/{root}/cancel"), None);
    assert_eq!(status, 202, "cancel: {body}");
    for id in &tree {
        env.wait_status(id, "Cancelled");
    }
    wait_until(
        Duration::from_secs(60),
        "no labelled container and no run network",
        || containers(&instance).is_empty() && networks(&instance).is_empty(),
    );
}

/// What a sampler of scenario 10 keeps: the sandbox containers, and what `docker network inspect`
/// said about each network the first time it was seen.
type NetSample = (Vec<Container>, Vec<(String, Option<(Vec<String>, String)>)>);

/// Samples every 300 ms and counts each sample it has taken in `taken`.
fn network_sampler(instance: String, taken: Arc<AtomicUsize>) -> Sampler<NetSample> {
    let mut known: HashSet<String> = HashSet::new();
    Sampler::start(Duration::from_millis(300), move || {
        let cs = containers(&instance);
        let mut new = Vec::new();
        for name in cs.iter().flat_map(|c| c.networks.iter()) {
            if known.insert(name.clone()) {
                new.push((name.clone(), network_info(name)));
            }
        }
        taken.fetch_add(1, Ordering::SeqCst);
        (cs, new)
    })
}

/// A file in the workspace of `id`, on the host.
fn in_workspace(env: &Env, id: &str, name: &str) -> PathBuf {
    env.dir.path().join("workspaces").join(id).join(name)
}

/// Waits until the agent of every run in `runs` holds (`hold=N`: it wrote `holding.out` and waits
/// for a `go` file), then until the sampler has taken a sample that started after that, and only
/// then writes `go` into every workspace. So the samples hold one in which every run's sandbox
/// was running, whatever the timing of the runner.
fn hold_then_go(env: &Env, runs: &[String], taken: &AtomicUsize) {
    wait_until(Duration::from_secs(300), "every run holds", || {
        runs.iter()
            .all(|id| in_workspace(env, id, "holding.out").exists())
    });
    // The sample that ends next may have started before the last run held; the one after it
    // started once all of them held.
    let seen = taken.load(Ordering::SeqCst);
    wait_until(Duration::from_secs(60), "two more docker samples", || {
        taken.load(Ordering::SeqCst) >= seen + 2
    });
    for id in runs {
        std::fs::write(in_workspace(env, id, "go"), "go\n").expect("write go");
    }
}

/// The agent of `id` went on because the test said go, not because its hold timed out.
fn assert_went_on_go(env: &Env, id: &str) {
    assert_eq!(
        env.read_out(id, "held.out").trim(),
        "go",
        "run {id} stopped holding before the test said go"
    );
}

/// Checks that every run in `runs` was seen on exactly one network of its own, labelled with its
/// run id, on a `/29` of the Kestrel pool. Returns the most run networks seen at once.
fn assert_own_networks(samples: &[(Instant, NetSample)], runs: &[String]) -> usize {
    let mut nets_of: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    let mut info: HashMap<&str, Option<&(Vec<String>, String)>> = HashMap::new();
    let mut most = 0;
    for (_, (cs, new)) in samples {
        for (name, i) in new {
            info.insert(name.as_str(), i.as_ref());
        }
        for c in cs {
            for n in &c.networks {
                nets_of
                    .entry(c.run.as_str())
                    .or_default()
                    .insert(n.as_str());
            }
        }
        let live: BTreeSet<&str> = cs
            .iter()
            .filter(|c| c.state == "running")
            .flat_map(|c| c.networks.iter().map(String::as_str))
            .collect();
        most = most.max(live.len());
    }
    let mut owners: BTreeMap<&str, &str> = BTreeMap::new();
    for run in runs {
        let nets = nets_of
            .get(run.as_str())
            .unwrap_or_else(|| panic!("run {run} was never seen by docker ps"));
        assert_eq!(nets.len(), 1, "run {run} on networks {nets:?}");
        let net = *nets.iter().next().expect("one network");
        if let Some(other) = owners.insert(net, run.as_str()) {
            panic!("runs {other} and {run} share network {net}");
        }
        let (subnets, label) = info
            .get(net)
            .copied()
            .flatten()
            .unwrap_or_else(|| panic!("network {net} of run {run} could not be inspected"));
        assert_eq!(label, run, "network {net} is labelled with its run");
        assert_eq!(subnets.len(), 1, "network {net}: {subnets:?}");
        assert!(
            subnets[0].starts_with("10.200.") && subnets[0].ends_with("/29"),
            "network {net} is on {}, not a /29 of the Kestrel pool 10.200.0.0/16",
            subnets[0]
        );
    }
    most
}

/// Scenario 10: in the 6-way fan-out every run gets its own network from the Kestrel pool, and a
/// separate group of 20 short sandboxes starts without "address pools exhausted", with the CI
/// host's default Docker pools as they are. Both groups have every network up at once: 7 for the
/// fan-out, 20 for the group. The test holds every run until all of them are up, so the overlap
/// does not depend on timing.
#[test]
fn m2_s10_each_run_gets_its_own_network_and_twenty_start() {
    let env = Env::with_max(20);
    env.spawnable("worker", "1", WORKER_CAPS);
    let lead = env.root_agent("lead", "5", &lead_caps(6, "worker"));
    let short = env.root_agent("short", "1", WORKER_CAPS);
    let _up = env.up();

    let taken = Arc::new(AtomicUsize::new(0));
    let sampler = network_sampler(env.instance(), taken.clone());
    let root = env.submit_file(&lead, "fanout=6 each=0.20 c.hold=900");
    let kids: Vec<String> = env
        .wait_children(&root, 6)
        .iter()
        .map(|k| k.id.to_string())
        .collect();
    hold_then_go(&env, &kids, &taken);
    env.wait_status(&root, "Completed");
    let mut tree = vec![root.clone()];
    for id in &kids {
        env.wait_status(id, "Completed");
        assert_went_on_go(&env, id);
        tree.push(id.clone());
    }
    let samples = sampler.finish();
    assert_eq!(tree.len(), 7);
    let most = assert_own_networks(&samples, &tree);
    eprintln!("m2-result: fan-out of 6, 7 runs on 7 networks, at most {most} at once");
    // Every child holds until the test says go, and the test says it only after a sample taken
    // while all 6 held. The parent is in its spawns or its join (its sandbox keeps running) until
    // its children end. So that sample saw all 7 networks.
    assert_eq!(
        most, 7,
        "the parent and its 6 children had their networks at once"
    );

    let taken = Arc::new(AtomicUsize::new(0));
    let sampler = network_sampler(env.instance(), taken.clone());
    let group: Vec<String> = (0..20)
        .map(|_| env.submit_file(&short, "hold=900"))
        .collect();
    hold_then_go(&env, &group, &taken);
    for id in &group {
        let row = env.wait_status_within(id, "Completed", Duration::from_secs(300));
        assert_eq!(status_name(&row["status"]), "Completed");
        assert_went_on_go(&env, id);
        for e in env.events(id) {
            let text = e.to_string();
            assert!(
                !text.contains("address pools") && !text.contains("exhausted"),
                "run {id}: {text}"
            );
        }
    }
    let samples = sampler.finish();
    let most = assert_own_networks(&samples, &group);
    eprintln!("m2-result: 20 short sandboxes on 20 networks, at most {most} at once");
    // The point of the group: 20 run networks exist at once on the default Docker pools. Under a
    // cap of 20, every run holds until a sample was taken while all 20 held, so that sample saw
    // all 20 (CI run 44940626625, which timed the overlap with naps instead, also saw 20). Each
    // run has one network of its own, so 20 is also the most possible.
    assert_eq!(
        most, 20,
        "the 20 short sandboxes had their networks at once"
    );
}

/// Scenario 14: `kill -9` of serve while the parent waits in its join. After the restart every run
/// is suspended (as in M1). The parent is resumed alone and waits in its join again while its
/// children stay suspended; then the children are resumed, they finish, the join counts each once,
/// and every model call is counted once.
///
/// That the parent spawns no child again is the echo agent's doing (it keeps its children in
/// `kids.out`), not a Kestrel guarantee: spawn de-dup by JSON-RPC id is in memory only.
#[test]
fn m2_s14_restart_mid_join_resumes_and_counts_once() {
    let env = Env::with_max(8);
    env.spawnable("worker", "1", WORKER_CAPS);
    let lead = env.root_agent("lead", "5", &lead_caps(6, "worker"));
    let up = env.up();
    let root = env.submit_file(&lead, "fanout=6 each=0.20 c.nap=30");
    let kids: Vec<String> = env
        .wait_children(&root, 6)
        .iter()
        .map(|k| k.id.to_string())
        .collect();
    let joining = env
        .dir
        .path()
        .join("workspaces")
        .join(&root)
        .join("joining.out");
    // Kill only once every child has made its model call (none is in flight) and the parent is in
    // its join.
    wait_until(
        RUN_DEADLINE,
        "the parent joins and every child made its call",
        || {
            joining.exists()
                && kids
                    .iter()
                    .all(|k| !of_kind(&env.events_now(k), "model_call").is_empty())
        },
    );
    std::thread::sleep(Duration::from_secs(2));
    up.kill();

    let _up = env.up();
    let mut tree = vec![root.clone()];
    tree.extend(kids.iter().cloned());
    for id in &tree {
        assert_eq!(
            status_name(&env.row(id)["status"]),
            "Suspended(Error(engine restarted))",
            "run {id} after the restart"
        );
        assert_eq!(env.spent(id), ONE_CALL_MICRO_USD, "spend of {id} survives");
    }
    // The parent alone first. A resumed child would finish at once (its nap is done), so the
    // children stay suspended until the parent is back in its join: a join that took a suspended
    // child for an ended one would answer now.
    std::fs::remove_file(&joining).expect("remove joining.out");
    env.ok(&["resume", root.as_str()]);
    wait_until(RUN_DEADLINE, "the resumed parent joins again", || {
        joining.exists()
    });
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(
        status_name(&env.row(&root)["status"]),
        "Running",
        "the parent waits in its join"
    );
    for kid in &kids {
        assert_eq!(
            status_name(&env.row(kid)["status"]),
            "Suspended(Error(engine restarted))",
            "child {kid} while only its parent was resumed"
        );
    }
    let waiting = env.read_out(&root, "join.out");
    assert!(
        !waiting.contains("outcome"),
        "the join answered while every child was suspended: {waiting}"
    );

    for kid in &kids {
        env.ok(&["resume", kid.as_str()]);
    }
    for id in &tree {
        env.wait_status(id, "Completed");
    }

    // The echo agent remembers the children it spawned (kids.out in its workspace) and spawns
    // only the rest when it starts again. Kestrel does not guarantee this: its spawn de-dup by
    // JSON-RPC id is kept in memory and does not survive a restart.
    assert_eq!(
        env.read_out(&root, "kids.out").lines().count(),
        6,
        "the echo agent's own list of its children"
    );
    assert_eq!(
        env.children(&root).len(),
        6,
        "the echo agent read its kids.out after the restart and spawned no child again"
    );
    let joined = env.read_out(&root, "join.out");
    for kid in &kids {
        assert!(
            joined.contains(kid.as_str()),
            "join answer lacks {kid}: {joined}"
        );
    }
    assert_eq!(
        joined.matches(r#""outcome":"completed""#).count(),
        6,
        "the join answer: {joined}"
    );

    let mut calls = 0;
    for id in &tree {
        let n = assert_exact_cost(&env, id);
        assert_eq!(n, 2, "run {id}: one call before the restart, one after");
        calls += n;
    }
    assert_eq!(
        env.mock.keys_seen().len(),
        calls,
        "every call the mock answered is counted once"
    );
}
