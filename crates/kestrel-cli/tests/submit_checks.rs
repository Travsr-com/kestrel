//! Issue #25, scenario 5: `kestrel run` of a `git-clone` agent resolves the repo with
//! `git rev-parse --show-toplevel` in the current dir, at submit time.
//!
//! * Not inside a git repo: exit 2, a message saying so, no run created.
//! * Inside a repo, even from a subdirectory: the stored repo is the repo ROOT, absolute.
//! * `git` missing from PATH: exit 2 with a message naming git, no run created.
//! * `empty-dir` agents never need git.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{DEADLINE, Fixture, GOOD_YAML, path_str};
use kestrel_store::RunFilter;
use kestrel_types::{RunId, WorkspaceSpec};
use predicates::prelude::*;

/// A scratch dir that is certainly not inside a git repo: git must not look above it.
fn plain_dir(f: &Fixture) -> PathBuf {
    let d = f.dir.path().join("plain");
    std::fs::create_dir(&d).expect("plain dir");
    d
}

fn ceiling(f: &Fixture) -> String {
    f.dir.path().to_str().expect("utf-8").to_string()
}

fn git_init(dir: &Path) {
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(status.success());
}

fn run_from(f: &Fixture, cwd: &Path, agent: &Path) -> assert_cmd::assert::Assert {
    f.kestrel()
        .current_dir(cwd)
        .env("GIT_CEILING_DIRECTORIES", ceiling(f))
        .args(["run", path_str(agent), "--input", "x"])
        .timeout(DEADLINE)
        .assert()
}

fn no_runs(f: &Fixture) {
    assert!(
        f.store()
            .list_runs(RunFilter::default())
            .expect("list")
            .is_empty(),
        "a refused submit must not leave a run behind"
    );
}

#[test]
fn a_git_clone_agent_outside_a_git_repo_exits_2_at_submit_time() {
    let f = Fixture::new();
    let agent = f.good_agent();
    let cwd = plain_dir(&f);
    run_from(&f, &cwd, &agent)
        .code(2)
        .stderr(predicate::str::contains("git repository"))
        .stdout(predicate::str::is_empty());
    no_runs(&f);
}

#[test]
fn inside_a_git_repo_the_stored_repo_is_the_repo_root_even_from_a_subdirectory() {
    let f = Fixture::new();
    let agent = f.good_agent();
    let root = f.dir.path().join("repo");
    std::fs::create_dir_all(root.join("src/deep")).expect("dirs");
    git_init(&root);
    let top = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&root)
        .output()
        .expect("rev-parse");
    let top = PathBuf::from(String::from_utf8(top.stdout).expect("utf-8").trim());

    let out = run_from(&f, &root.join("src/deep"), &agent).success();
    let id: RunId = String::from_utf8_lossy(&out.get_output().stdout)
        .trim()
        .parse()
        .expect("run id");
    let spec = f.store().run_spec(id).expect("spec").expect("stored");
    let WorkspaceSpec::GitClone { repo, .. } = spec.sandbox.workspace else {
        panic!("not a git clone")
    };
    assert!(repo.is_absolute());
    assert_eq!(repo, top, "the root, not the subdirectory");
}

#[test]
fn from_the_repo_root_itself_the_stored_repo_is_that_root() {
    let f = Fixture::new();
    let agent = f.good_agent();
    let root = f.dir.path().join("repo");
    std::fs::create_dir_all(&root).expect("dir");
    git_init(&root);
    let out = run_from(&f, &root, &agent).success();
    let id: RunId = String::from_utf8_lossy(&out.get_output().stdout)
        .trim()
        .parse()
        .expect("run id");
    let spec = f.store().run_spec(id).expect("spec").expect("stored");
    let WorkspaceSpec::GitClone { repo, .. } = spec.sandbox.workspace else {
        panic!("not a git clone")
    };
    assert_eq!(
        repo.canonicalize().expect("exists"),
        root.canonicalize().expect("root")
    );
}

#[test]
fn git_missing_from_path_exits_2_with_a_clear_message() {
    let f = Fixture::new();
    let agent = f.good_agent();
    let empty = f.dir.path().join("empty-bin");
    std::fs::create_dir(&empty).expect("dir");
    let root = f.dir.path().join("repo");
    std::fs::create_dir_all(&root).expect("dir");
    f.kestrel()
        .current_dir(&root)
        .env("PATH", &empty)
        .args(["run", path_str(&agent), "--input", "x"])
        .timeout(DEADLINE)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("git"));
    no_runs(&f);
}

#[test]
fn an_empty_dir_agent_needs_no_git_and_no_repo() {
    let f = Fixture::new();
    let agent = f.write(
        "empty.yaml",
        &GOOD_YAML.replace("workspace: git-clone", "workspace: empty-dir"),
    );
    let cwd = plain_dir(&f);
    run_from(&f, &cwd, &agent).success();
}
