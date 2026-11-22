//! Issue #93: `kestrel fetch <run-id>` brings a finished run's branch `kestrel/run-<id>` home into
//! its source repository on demand, for a run whose automatic fetch failed or that ended before
//! Kestrel fetched on its own.
//!
//! Exit 0 when the branch is there afterwards or there was nothing to fetch, 1 when the run is
//! unknown, not ended, has no clone, or git fails, 2 on a usage error. `--json` prints one object:
//! `run_id`, `branch`, `repo`, `result` (`fetched`, `already_fetched`, `nothing_new`,
//! `no_branch`) and `commit`.
//!
//! Every test uses real git in a temp dir and skips with a message when it is missing.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{Fixture, agent, json_doc, running_run, walk};
use kestrel_store::Store;
use kestrel_types::{AgentSpec, RunId, RunStatus, WorkspaceSpec};
use predicates::prelude::*;

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

macro_rules! need_git {
    () => {
        if !git_available() {
            eprintln!("SKIPPED {}: `git` is not on PATH", module_path!());
            return;
        }
    };
}

fn git_raw(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Kestrel Test",
            "-c",
            "user.email=test@kestrel.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .output()
        .expect("git runs")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_raw(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit_file(dir: &Path, name: &str, body: &str) -> String {
    std::fs::write(dir.join(name), body).unwrap();
    git(dir, &["add", name]);
    git(dir, &["commit", "-q", "-m", &format!("write {name}")]);
    git(dir, &["rev-parse", "HEAD"])
}

fn branch_for(id: RunId) -> String {
    let text = id.to_string();
    format!("kestrel/run-{}", &text[text.len() - 12..])
}

fn branch_tip(repo: &Path, branch: &str) -> Option<String> {
    let out = git_raw(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    );
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A source repo with one commit next to the database.
fn source(f: &Fixture) -> PathBuf {
    let repo = f.dir.path().join("source");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    commit_file(&repo, "README.md", "hello\n");
    repo
}

fn clone_agent(repo: &Path) -> AgentSpec {
    let mut spec = agent();
    spec.sandbox.workspace = WorkspaceSpec::GitClone {
        repo: repo.to_path_buf(),
        depth: None,
    };
    spec
}

/// A run of `spec` moved along to `end`.
fn ended_run(store: &Store, spec: &AgentSpec, end: RunStatus) -> RunId {
    let id = store.create_run(spec, "task").unwrap();
    walk(
        store,
        id,
        RunStatus::Queued,
        &[RunStatus::Provisioning, RunStatus::Running, end],
    );
    id
}

/// The run's clone where `kestrel serve` keeps it (`<db dir>/workspaces/<id>`), on its branch.
fn clone_for(f: &Fixture, repo: &Path, id: RunId) -> PathBuf {
    let clone = f.dir.path().join("workspaces").join(id.to_string());
    std::fs::create_dir_all(clone.parent().unwrap()).unwrap();
    let out = Command::new("git")
        .args(["clone", "-q", "--local", "--no-hardlinks", "--"])
        .arg(repo)
        .arg(&clone)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    git(&clone, &["checkout", "-q", "-b", &branch_for(id)]);
    clone
}

struct Done {
    f: Fixture,
    repo: PathBuf,
    id: RunId,
    clone: PathBuf,
}

/// A completed run whose clone holds one new commit. Returns the commit too.
fn completed_with_commit() -> (Done, String) {
    let f = Fixture::new();
    let repo = source(&f);
    let id = ended_run(&f.store(), &clone_agent(&repo), RunStatus::Completed);
    let clone = clone_for(&f, &repo, id);
    let tip = commit_file(&clone, "agent.txt", "work\n");
    (Done { f, repo, id, clone }, tip)
}

#[test]
fn fetch_brings_the_branch_home_and_says_so() {
    need_git!();
    let (d, tip) = completed_with_commit();
    let branch = branch_for(d.id);

    let out =
        d.f.kestrel()
            .args(["fetch", &d.id.to_string()])
            .assert()
            .success();

    let text = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(text.contains(&format!("fetched {branch}")), "{text}");
    assert!(text.contains(&tip[..12]), "{text}");
    assert_eq!(branch_tip(&d.repo, &branch), Some(tip));
    assert_eq!(git(&d.repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    assert_eq!(git(&d.repo, &["status", "--porcelain"]), "");
}

#[test]
fn fetch_json_names_the_branch_repo_result_and_commit() {
    need_git!();
    let (d, tip) = completed_with_commit();

    let out =
        d.f.kestrel()
            .args(["fetch", "--json", &d.id.to_string()])
            .assert()
            .success();

    let doc = json_doc(&out.get_output().stdout);
    assert_eq!(doc["run_id"], d.id.to_string());
    assert_eq!(doc["branch"], branch_for(d.id));
    assert_eq!(doc["repo"], d.repo.to_str().unwrap());
    assert_eq!(doc["result"], "fetched");
    assert_eq!(doc["commit"], tip);
}

#[test]
fn fetch_twice_reports_already_fetched() {
    need_git!();
    let (d, tip) = completed_with_commit();
    let id = d.id.to_string();
    d.f.kestrel().args(["fetch", &id]).assert().success();

    let out =
        d.f.kestrel()
            .args(["fetch", "--json", &id])
            .assert()
            .success();
    let doc = json_doc(&out.get_output().stdout);
    assert_eq!(doc["result"], "already_fetched");
    assert_eq!(doc["commit"], tip);

    d.f.kestrel()
        .args(["fetch", &id])
        .assert()
        .success()
        .stdout(predicate::str::contains("already"));
}

#[test]
fn fetch_works_for_failed_and_cancelled_runs() {
    need_git!();
    for end in [RunStatus::Failed("boom".into()), RunStatus::Cancelled] {
        let f = Fixture::new();
        let repo = source(&f);
        let id = ended_run(&f.store(), &clone_agent(&repo), end.clone());
        let clone = clone_for(&f, &repo, id);
        let tip = commit_file(&clone, "agent.txt", "partial\n");

        f.kestrel()
            .args(["fetch", &id.to_string()])
            .assert()
            .success();

        assert_eq!(branch_tip(&repo, &branch_for(id)), Some(tip), "{end:?}");
    }
}

#[test]
fn fetch_of_a_run_without_new_commits_fetches_nothing_and_exits_0() {
    need_git!();
    let f = Fixture::new();
    let repo = source(&f);
    let id = ended_run(&f.store(), &clone_agent(&repo), RunStatus::Completed);
    clone_for(&f, &repo, id);

    let out = f
        .kestrel()
        .args(["fetch", "--json", &id.to_string()])
        .assert()
        .success();
    let doc = json_doc(&out.get_output().stdout);
    assert_eq!(doc["result"], "nothing_new");
    assert_eq!(doc["commit"], git(&repo, &["rev-parse", "main"]));

    f.kestrel()
        .args(["fetch", &id.to_string()])
        .assert()
        .success()
        .stdout(predicate::str::contains("no new commits"));
    assert_eq!(branch_tip(&repo, &branch_for(id)), None);
}

#[test]
fn fetch_of_a_clone_without_its_branch_fetches_nothing_and_exits_0() {
    need_git!();
    let (d, _) = completed_with_commit();
    git(&d.clone, &["checkout", "-q", "-b", "elsewhere"]);
    git(&d.clone, &["branch", "-q", "-D", &branch_for(d.id)]);

    let out =
        d.f.kestrel()
            .args(["fetch", "--json", &d.id.to_string()])
            .assert()
            .success();
    let doc = json_doc(&out.get_output().stdout);
    assert_eq!(doc["result"], "no_branch");
    assert!(doc["commit"].is_null());

    d.f.kestrel()
        .args(["fetch", &d.id.to_string()])
        .assert()
        .success()
        .stdout(predicate::str::contains("has no branch"));
}

#[test]
fn fetch_that_git_refuses_exits_1_and_keeps_the_users_branch() {
    need_git!();
    let (d, _) = completed_with_commit();
    let branch = branch_for(d.id);
    // A branch of that name already exists, at a commit the clone does not build on.
    git(&d.repo, &["checkout", "-q", "-b", "scratch"]);
    let theirs = commit_file(&d.repo, "their.txt", "theirs\n");
    git(&d.repo, &["branch", "-q", &branch, &theirs]);

    d.f.kestrel()
        .args(["fetch", &d.id.to_string()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(&branch));

    assert_eq!(branch_tip(&d.repo, &branch), Some(theirs));
}

#[test]
fn fetch_of_an_unknown_run_exits_1() {
    let f = Fixture::new();
    let id = RunId::new();
    f.kestrel()
        .args(["fetch", &id.to_string()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(format!("no such run: {id}")));
}

#[test]
fn fetch_of_a_run_still_going_exits_1_and_runs_no_git() {
    let f = Fixture::new();
    let store = f.store();
    let id = running_run(&store);
    f.kestrel()
        .args(["fetch", &id.to_string()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("has not ended"));
}

#[test]
fn fetch_of_an_empty_dir_run_exits_1() {
    let f = Fixture::new();
    let mut spec = agent();
    spec.sandbox.workspace = WorkspaceSpec::EmptyDir;
    let id = ended_run(&f.store(), &spec, RunStatus::Completed);
    f.kestrel()
        .args(["fetch", &id.to_string()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no git-clone workspace"));
}

#[test]
fn fetch_of_a_run_whose_clone_is_gone_exits_1() {
    need_git!();
    let f = Fixture::new();
    let repo = source(&f);
    let id = ended_run(&f.store(), &clone_agent(&repo), RunStatus::Completed);
    f.kestrel()
        .args(["fetch", &id.to_string()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no workspace"));
}

#[test]
fn fetch_with_a_bad_run_id_is_a_usage_error() {
    let f = Fixture::new();
    f.kestrel().args(["fetch", "not-a-run"]).assert().code(2);
}
