//! M2 acceptance scenarios 12 and 13 (issue #45): the UI in a real browser. This test starts a real
//! `kestrel up` with the embedded UI build, starts two trees, and runs the Playwright suite in
//! `tests/e2e/ui` against it, passing the server URL, the API token and the run ids in the
//! environment. Then it checks the agent side: every run finished, and the child that asked got the
//! answer typed in the browser.
//!
//! Needs what `tests/e2e.rs` needs, a UI build (`npm ci && npm run build` in `ui/`, before
//! building `kestrel`), and the Playwright suite installed (`npm ci && npx playwright install
//! chromium` in `tests/e2e/ui`). Run with
//! `cargo test -p kestrel-e2e --features e2e --test ui -- --test-threads=1 --nocapture`.
//! On a failure, the Playwright trace and screenshots are in `tests/e2e/ui/test-results`.

// Helpers panic on purpose: a panic is how a test fails.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::path::Path;
use std::process::Command;

use common::Env;

/// The question the `ask=1` child asks (see the echo agent), and the answer the browser gives.
const QUESTION: &str = "Which colour should kid 1 use?";
const ANSWER: &str = "teal from the browser";
/// Children of the tree the browser watches grow.
const CHILDREN: usize = 6;

#[test]
fn ui_s12_s13_live_tree_and_approval_in_a_browser() {
    let env = Env::with_max(8);
    env.spawnable("worker", "1", "");
    env.spawnable("asker", "1", "mcp: [kestrel.ask_human]");
    let lead = env.root_agent(
        "lead",
        "5",
        "mcp: [kestrel.spawn, kestrel.join, kestrel.ask_human], \
         spawn: { max_children: 6, max_depth: 1, agents: [worker, asker] }",
    );
    let up = env.up();

    // Scenario 12: the parent waits 20 s, so the page is open before its first child exists, then
    // spawns a child every 4 s; each child runs for a while, so it shows as running before the
    // last one exists.
    let root = env.submit_file(
        &lead,
        &format!("fanout={CHILDREN} each=0.20 first=20 gap=4 c.nap=8"),
    );
    // Scenario 13: a child that asks a person and waits for the answer.
    let ask_root = env.submit_file(&lead, "fanout=1 kids=asker each=0.20 c.ask=1");

    let suite = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    let status = Command::new("npx")
        .args(["playwright", "test"])
        .current_dir(&suite)
        .env("KESTREL_URL", format!("http://{}", up.api))
        .env("KESTREL_TOKEN", &up.token)
        .env("KESTREL_ROOT", &root)
        .env("KESTREL_CHILDREN", CHILDREN.to_string())
        .env("KESTREL_ASK_ROOT", &ask_root)
        .env("KESTREL_QUESTION", QUESTION)
        .env("KESTREL_ANSWER", ANSWER)
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "could not start `npx playwright test` in {} ({e}): install Node, then run \
                 `npm ci && npx playwright install chromium` there",
                suite.display()
            )
        });
    if !status.success() {
        eprintln!(
            "events of the tree root {root}:\n{}",
            env.dump_events(&root)
        );
        eprintln!(
            "events of the asking root {ask_root}:\n{}",
            env.dump_events(&ask_root)
        );
    }
    assert!(
        status.success(),
        "the Playwright suite failed ({status}); its output is above, and its trace and \
         screenshots are in {}",
        suite.join("test-results").display()
    );

    // Scenario 13, the agent side: the child went on with the text typed in the browser.
    let asker = env.children(&ask_root);
    assert_eq!(asker.len(), 1, "one asking child");
    let asker = asker[0].id.to_string();
    env.wait_status(&asker, "Completed");
    assert_eq!(env.read_out(&asker, "answer.out").trim(), ANSWER);
    env.wait_status(&ask_root, "Completed");

    // Scenario 12, the API side: the tree the browser watched is complete.
    env.wait_status(&root, "Completed");
    let kids = env.children(&root);
    assert_eq!(kids.len(), CHILDREN);
    for kid in kids {
        env.wait_status(&kid.id.to_string(), "Completed");
    }
}
