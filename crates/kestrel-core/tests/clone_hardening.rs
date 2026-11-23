//! Issue #52: defence in depth for the host side of per-run git clones.
//!
//! Scenarios (all `s<N>_...`):
//!
//! 1. an object that fails fsck is rejected by `fetch_back`; the source repo gets no new refs
//! 2. host git calls run with a scrubbed environment
//! 3. `<clone>/.git` that is a file or a symlink makes `fetch_back` refuse
//! 4. `--` separates paths from options; a force-rewritten clone branch errors, source intact
//! 5. a relative repo passed to `prepare` is refused before any git call
//!
//! Pure argv tests use a fake runner. The rest use the real `git` binary and skip with a
//! message when it is missing. Scenario 2 sets hostile variables in a child copy of this test
//! binary, because edition 2024 makes `std::env::set_var` unsafe and this workspace forbids it.

#![allow(clippy::expect_used, clippy::unwrap_used)]
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use kestrel_core::workspace::{WorkspaceError, Workspaces, git_env_scrub};
use kestrel_sandbox::{CommandOutput, CommandRunner, EnvScrub, ProcessRunner, SpawnedProcess};
use kestrel_types::{RunId, WorkspaceSpec};
use tempfile::TempDir;
use uuid::Uuid;

fn run_a() -> RunId {
    RunId::from_uuid(Uuid::parse_str("0192f0a1-2b3c-7d4e-8f50-123456789abc").unwrap())
}

const BRANCH_A: &str = "kestrel/run-123456789abc";

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

fn commit_file(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
    git(dir, &["add", name]);
    git(dir, &["commit", "-q", "-m", &format!("add {name}")]);
}

struct Fixture {
    _tmp: TempDir,
    repo: PathBuf,
    root: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("source");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    commit_file(&repo, "base.txt", "base\n");
    let root = tmp.path().join("workspaces");
    Fixture {
        _tmp: tmp,
        repo,
        root,
    }
}

fn clone_spec(repo: &Path) -> WorkspaceSpec {
    WorkspaceSpec::GitClone {
        repo: repo.to_path_buf(),
        depth: None,
    }
}

/// Refs under `refs/heads/kestrel/` in `repo` (empty when the host fetched nothing).
fn kestrel_refs(repo: &Path) -> String {
    git(
        repo,
        &["for-each-ref", "--format=%(refname) %(objectname)", "refs/"],
    )
    .lines()
    .filter(|l| l.contains("kestrel/"))
    .collect::<Vec<_>>()
    .join("\n")
}

/// One recorded call: the argv, and the env the runner was given (`None` for a plain `run`).
type Call = (Vec<String>, Option<EnvScrub>);

/// Records every call, then runs the real command with the same environment.
#[derive(Default)]
struct Recording {
    calls: Mutex<Vec<Call>>,
}

impl Recording {
    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    fn argvs(&self) -> Vec<Vec<String>> {
        self.calls().into_iter().map(|(a, _)| a).collect()
    }
}

#[async_trait]
impl CommandRunner for Recording {
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((argv.to_vec(), None));
        ProcessRunner.run(argv).await
    }

    async fn run_with_env(
        &self,
        argv: &[String],
        env: &EnvScrub,
    ) -> std::io::Result<CommandOutput> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((argv.to_vec(), Some(env.clone())));
        ProcessRunner.run_with_env(argv, env).await
    }

    async fn spawn(&self, argv: &[String]) -> std::io::Result<SpawnedProcess> {
        panic!("workspaces never spawn: {argv:?}");
    }
}

/// A fake that spawns nothing: every command succeeds, and `git clone` leaves `<dest>/.git/`.
#[derive(Default)]
struct FakeOk {
    calls: Mutex<Vec<Call>>,
}

impl FakeOk {
    fn record(&self, argv: &[String], env: Option<&EnvScrub>) -> std::io::Result<CommandOutput> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((argv.to_vec(), env.cloned()));
        if argv.get(1).is_some_and(|a| a == "clone") {
            let dest = PathBuf::from(argv.last().expect("clone target"));
            std::fs::create_dir_all(dest.join(".git"))?;
            std::fs::write(dest.join(".git").join("HEAD"), "ref: refs/heads/main\n")?;
        }
        Ok(CommandOutput::default())
    }
    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl CommandRunner for FakeOk {
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
        self.record(argv, None)
    }
    async fn run_with_env(
        &self,
        argv: &[String],
        env: &EnvScrub,
    ) -> std::io::Result<CommandOutput> {
        self.record(argv, Some(env))
    }
    async fn spawn(&self, argv: &[String]) -> std::io::Result<SpawnedProcess> {
        panic!("unexpected spawn: {argv:?}");
    }
}

fn fake_root() -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspaces");
    (tmp, root)
}

fn has_pair(argv: &[String], a: &str, b: &str) -> bool {
    argv.windows(2).any(|w| w[0] == a && w[1] == b)
}

// ---------------------------------------------------------------------------------------------
// Scenario 1: an object that fails fsck is rejected at fetch time.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn s1_fetch_argv_pins_fsck_objects_and_disallows_the_ext_transport() {
    let (_tmp, root) = fake_root();
    let fake = Arc::new(FakeOk::default());
    let ws = Workspaces::new(root, fake.clone());
    ws.prepare(run_a(), &clone_spec(Path::new("/srv/repos/app")))
        .await
        .unwrap();

    ws.fetch_back(run_a(), Path::new("/srv/repos/app"))
        .await
        .unwrap();

    let calls = fake.calls();
    let fetch = &calls.last().unwrap().0;
    let at = fetch.iter().position(|a| a == "fetch").expect("fetch");
    let options = &fetch[..at];
    assert!(
        has_pair(options, "-c", "transfer.fsckObjects=true"),
        "{fetch:?}"
    );
    assert!(
        has_pair(options, "-c", "protocol.ext.allow=never"),
        "{fetch:?}"
    );
    assert!(
        has_pair(options, "-c", "core.hooksPath=/dev/null"),
        "kept: {fetch:?}"
    );
}

/// Writes a commit on `branch` in `clone` whose tree has two entries named `a`. A tree like
/// that is valid enough to be stored and committed, but `git fsck` calls it an error.
fn plant_bad_commit(clone: &Path, branch: &str) {
    let blob_file = clone.join("..").join("blob.bin");
    std::fs::write(&blob_file, b"hi").unwrap();
    let hex = git(clone, &["hash-object", "-w", blob_file.to_str().unwrap()]);
    let raw_id: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let mut entry = b"100644 a\0".to_vec();
    entry.extend(&raw_id);
    let tree_bytes = [entry.clone(), entry].concat();
    let tree_file = clone.join("..").join("bad-tree.bin");
    std::fs::write(&tree_file, tree_bytes).unwrap();
    let tree = git(
        clone,
        &[
            "hash-object",
            "-t",
            "tree",
            "-w",
            "--literally",
            tree_file.to_str().unwrap(),
        ],
    );
    let commit = git(
        clone,
        &["commit-tree", &tree, "-p", "HEAD", "-m", "bad object"],
    );
    git(
        clone,
        &["update-ref", &format!("refs/heads/{branch}"), &commit],
    );
    let fsck = git_raw(clone, &["fsck", "--strict"]);
    assert!(!fsck.status.success(), "test is broken: the object is fine");
}

#[tokio::test]
async fn s1_an_object_that_fails_fsck_makes_fetch_back_fail_and_the_source_gets_no_new_refs() {
    need_git!();
    let f = fixture();
    let ws = Workspaces::new(f.root.clone(), Arc::new(ProcessRunner));
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    plant_bad_commit(&clone, BRANCH_A);
    let before = git(&f.repo, &["for-each-ref"]);

    let err = ws.fetch_back(run_a(), &f.repo).await.unwrap_err();

    assert!(matches!(err, WorkspaceError::Git { .. }), "{err:?}");
    assert_eq!(kestrel_refs(&f.repo), "", "no kestrel ref in the source");
    assert_eq!(git(&f.repo, &["for-each-ref"]), before, "refs unchanged");
}

#[tokio::test]
async fn s1_a_clean_clone_still_fetches_back_with_the_hardened_flags() {
    need_git!();
    let f = fixture();
    let ws = Workspaces::new(f.root.clone(), Arc::new(ProcessRunner));
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    commit_file(&clone, "agent.txt", "work\n");

    ws.fetch_back(run_a(), &f.repo).await.unwrap();

    let want = git(&clone, &["rev-parse", "HEAD"]);
    assert_eq!(git(&f.repo, &["rev-parse", BRANCH_A]), want);
}

// ---------------------------------------------------------------------------------------------
// Scenario 2: host git calls scrub the environment.
// ---------------------------------------------------------------------------------------------

/// True when `env` removes the variable `name` (exactly, or through a `PREFIX*` entry).
fn removes(env: &EnvScrub, name: &str) -> bool {
    env.remove.iter().any(|r| match r.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => r == name,
    })
}

const HOSTILE_NAMES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_KEY_0",
    "GIT_CONFIG_VALUE_0",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_EXEC_PATH",
    // Added by the second review round: tracing, object redirects and helper programs.
    "GIT_TRACE",
    "GIT_TRACE_PACKET",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
    "GIT_ALLOW_PROTOCOL",
    "GIT_PROTOCOL_FROM_USER",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "GIT_PROXY_COMMAND",
    "GIT_EXTERNAL_DIFF",
    "GIT_PAGER",
];

fn assert_scrubs_everything(env: &EnvScrub) {
    for &name in HOSTILE_NAMES {
        assert!(removes(env, name), "{name} not removed: {env:?}");
    }
    let has = |k: &str, v: &str| env.set.iter().any(|(a, b)| a == k && b == v);
    assert!(has("GIT_CONFIG_NOSYSTEM", "1"), "{env:?}");
    assert!(has("GIT_TERMINAL_PROMPT", "0"), "{env:?}");
}

#[test]
fn s2_git_env_scrub_removes_the_hostile_variables_and_sets_the_two_safe_ones() {
    assert_scrubs_everything(&git_env_scrub());
}

#[tokio::test]
async fn s2_every_host_git_call_runs_through_the_scrubbed_environment() {
    let (_tmp, root) = fake_root();
    let fake = Arc::new(FakeOk::default());
    let ws = Workspaces::new(root, fake.clone());
    ws.prepare(run_a(), &clone_spec(Path::new("/srv/repos/app")))
        .await
        .unwrap();
    ws.fetch_back(run_a(), Path::new("/srv/repos/app"))
        .await
        .unwrap();

    let calls = fake.calls();
    // rev-parse, clone, checkout, fetch.
    assert_eq!(calls.len(), 4, "{calls:?}");
    for (argv, env) in calls {
        let env = env.unwrap_or_else(|| panic!("`{argv:?}` ran without the scrubbed env"));
        assert_scrubs_everything(&env);
    }
}

const CHILD_STEP: &str = "s2_child_step_runs_with_hostile_git_env";

/// Marker scripts the hostile variables point at; any of them running is a failure.
struct Traps {
    dir: PathBuf,
}

impl Traps {
    fn new(dir: PathBuf) -> Self {
        std::fs::create_dir_all(dir.join("exec")).unwrap();
        let t = Self { dir };
        for name in [
            "ssh",
            "pack-hook",
            "exec/git-upload-pack",
            "exec/git-receive-pack",
        ] {
            let script = t.dir.join(name);
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\ntouch '{}'\nexit 1\n",
                    t.marker(&name.replace('/', "_")).display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        t
    }
    fn marker(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.marker"))
    }
    fn fired(&self) -> Vec<String> {
        std::fs::read_dir(&self.dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".marker"))
            .collect()
    }
}

/// Runs `step` (`prepare` or `fetch`) in a child copy of this binary whose environment is
/// hostile: it points git at a decoy repo and at scripts that drop a marker when run.
fn run_child(step: &str, f: &Fixture, decoy: &Path, traps: &Traps) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([CHILD_STEP, "--exact", "--ignored", "--nocapture"])
        .env("KESTREL_S2_STEP", step)
        .env("KESTREL_S2_REPO", &f.repo)
        .env("KESTREL_S2_ROOT", &f.root)
        .env("GIT_DIR", decoy.join(".git"))
        .env("GIT_WORK_TREE", decoy)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "uploadpack.packObjectsHook")
        .env("GIT_CONFIG_VALUE_0", traps.dir.join("pack-hook"))
        .env("GIT_SSH_COMMAND", traps.dir.join("ssh"))
        .env("GIT_SSH", traps.dir.join("ssh"))
        .env("GIT_EXEC_PATH", traps.dir.join("exec"))
        .env("GIT_TRACE", traps.dir.join("trace.log"))
        .env("GIT_OBJECT_DIRECTORY", traps.dir.join("redirected-objects"))
        .output()
        .expect("child spawns");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "child `{step}` failed:\n{stdout}\n{stderr}"
    );
}

#[test]
fn s2_hostile_git_variables_in_kestrels_environment_change_nothing() {
    need_git!();
    let f = fixture();
    let decoy = f.repo.parent().unwrap().join("decoy");
    std::fs::create_dir(&decoy).unwrap();
    git(&decoy, &["init", "-q"]);
    commit_file(&decoy, "decoy.txt", "decoy\n");
    let decoy_refs = git(&decoy, &["for-each-ref"]);
    let traps = Traps::new(f.repo.parent().unwrap().join("traps"));

    run_child("prepare", &f, &decoy, &traps);

    let clone = f.root.join(run_a().to_string());
    let clone_head = git(&clone, &["rev-parse", "HEAD"]);
    assert_eq!(
        clone_head,
        git(&f.repo, &["rev-parse", "HEAD"]),
        "cloned the intended repo, not the decoy"
    );
    assert_eq!(git(&clone, &["branch", "--show-current"]), BRANCH_A);
    commit_file(&clone, "agent.txt", "work\n");

    run_child("fetch", &f, &decoy, &traps);

    assert_eq!(
        git(&f.repo, &["rev-parse", BRANCH_A]),
        git(&clone, &["rev-parse", "HEAD"]),
        "the branch landed in the intended repo"
    );
    assert_eq!(
        git(&decoy, &["for-each-ref"]),
        decoy_refs,
        "decoy untouched"
    );
    assert_eq!(traps.fired(), Vec::<String>::new(), "a marker command ran");
    assert!(
        !traps.dir.join("trace.log").exists(),
        "GIT_TRACE took effect"
    );
    assert!(
        !traps.dir.join("redirected-objects").exists(),
        "GIT_OBJECT_DIRECTORY took effect"
    );
}

/// Child half of the test above. Ignored so a normal run skips it; the parent runs it with
/// the hostile variables set. It asserts the env the runner received, then does the step.
#[tokio::test]
#[ignore = "run by s2_hostile_git_variables_in_kestrels_environment_change_nothing"]
async fn s2_child_step_runs_with_hostile_git_env() {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} not set"));
    assert!(std::env::var("GIT_DIR").is_ok(), "child env is not hostile");
    let recorder = Arc::new(Recording::default());
    let ws = Workspaces::new(PathBuf::from(var("KESTREL_S2_ROOT")), recorder.clone());
    let repo = PathBuf::from(var("KESTREL_S2_REPO"));
    match var("KESTREL_S2_STEP").as_str() {
        "prepare" => {
            ws.prepare(run_a(), &clone_spec(&repo)).await.unwrap();
        }
        "fetch" => ws.fetch_back(run_a(), &repo).await.unwrap(),
        other => panic!("unknown step {other}"),
    }
    let calls = recorder.calls();
    assert!(!calls.is_empty());
    for (argv, env) in calls {
        let env = env.unwrap_or_else(|| panic!("`{argv:?}` ran without the scrubbed env"));
        assert_scrubs_everything(&env);
    }
}

// ---------------------------------------------------------------------------------------------
// Scenario 3: `<clone>/.git` must be a real directory.
// ---------------------------------------------------------------------------------------------

/// A second repo that already has `BRANCH_A`, which a hostile gitfile or symlink could point at.
fn decoy_with_branch(f: &Fixture) -> PathBuf {
    let decoy = f.repo.parent().unwrap().join("decoy");
    std::fs::create_dir(&decoy).unwrap();
    git(&decoy, &["init", "-q"]);
    commit_file(&decoy, "decoy.txt", "decoy\n");
    git(&decoy, &["branch", BRANCH_A]);
    decoy
}

async fn prepared(f: &Fixture) -> (Workspaces, Arc<Recording>, PathBuf) {
    let rec = Arc::new(Recording::default());
    let ws = Workspaces::new(f.root.clone(), rec.clone());
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    (ws, rec, clone)
}

async fn assert_refused(ws: &Workspaces, rec: &Recording, f: &Fixture, decoy: &Path, clone: &Path) {
    let decoy_before = git(decoy, &["for-each-ref"]);
    let calls_before = rec.argvs().len();

    let err = ws.fetch_back(run_a(), &f.repo).await.unwrap_err();

    match &err {
        WorkspaceError::GitDirNotADirectory { clone: c } => assert_eq!(c, clone),
        other => panic!("expected GitDirNotADirectory, got {other:?}"),
    }
    assert!(err.to_string().contains(".git"), "clear message: {err}");
    assert_eq!(rec.argvs().len(), calls_before, "no git call was made");
    assert_eq!(kestrel_refs(&f.repo), "", "source repo untouched");
    assert_eq!(
        git(decoy, &["for-each-ref"]),
        decoy_before,
        "decoy untouched"
    );
}

#[tokio::test]
async fn s3_a_gitfile_in_place_of_dot_git_is_refused() {
    need_git!();
    let f = fixture();
    let decoy = decoy_with_branch(&f);
    let (ws, rec, clone) = prepared(&f).await;
    std::fs::remove_dir_all(clone.join(".git")).unwrap();
    std::fs::write(
        clone.join(".git"),
        format!("gitdir: {}\n", decoy.join(".git").display()),
    )
    .unwrap();

    assert_refused(&ws, &rec, &f, &decoy, &clone).await;
}

#[tokio::test]
async fn s3_a_symlink_in_place_of_dot_git_is_refused() {
    need_git!();
    let f = fixture();
    let decoy = decoy_with_branch(&f);
    let (ws, rec, clone) = prepared(&f).await;
    std::fs::remove_dir_all(clone.join(".git")).unwrap();
    std::os::unix::fs::symlink(decoy.join(".git"), clone.join(".git")).unwrap();

    assert_refused(&ws, &rec, &f, &decoy, &clone).await;
}

#[tokio::test]
async fn s3_a_missing_dot_git_is_refused_the_same_way() {
    need_git!();
    let f = fixture();
    let decoy = decoy_with_branch(&f);
    let (ws, rec, clone) = prepared(&f).await;
    std::fs::remove_dir_all(clone.join(".git")).unwrap();

    assert_refused(&ws, &rec, &f, &decoy, &clone).await;
}

#[tokio::test]
async fn s3_a_real_dot_git_directory_is_accepted() {
    need_git!();
    let f = fixture();
    let (ws, _rec, clone) = prepared(&f).await;
    commit_file(&clone, "agent.txt", "work\n");

    ws.fetch_back(run_a(), &f.repo).await.unwrap();

    assert_ne!(kestrel_refs(&f.repo), "");
}

// ---------------------------------------------------------------------------------------------
// Scenario 4: `--` separates paths from options; a rewritten clone branch is an error.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn s4_clone_has_double_dash_right_before_source_and_destination() {
    for depth in [None, Some(3)] {
        let (_tmp, root) = fake_root();
        let fake = Arc::new(FakeOk::default());
        let ws = Workspaces::new(root, fake.clone());
        let spec = WorkspaceSpec::GitClone {
            repo: PathBuf::from("/srv/repos/app"),
            depth,
        };
        let path = ws.prepare(run_a(), &spec).await.unwrap();

        let calls = fake.calls();
        let clone = calls
            .iter()
            .map(|(a, _)| a)
            .find(|a| a.get(1).is_some_and(|x| x == "clone"))
            .expect("a clone call");
        let n = clone.len();
        assert_eq!(clone[n - 3], "--", "{clone:?}");
        assert_eq!(clone[n - 1], path.to_str().unwrap(), "{clone:?}");
        let source = if depth.is_some() {
            "file:///srv/repos/app"
        } else {
            "/srv/repos/app"
        };
        assert_eq!(clone[n - 2], source, "{clone:?}");
        // Everything before `--` is an option, so the paths cannot be read as one.
        assert!(clone[..n - 3].iter().all(|a| a != source), "{clone:?}");
    }
}

#[tokio::test]
async fn s4_fetch_has_double_dash_right_before_the_clone_path_and_the_refspec_last() {
    let (_tmp, root) = fake_root();
    let fake = Arc::new(FakeOk::default());
    let ws = Workspaces::new(root, fake.clone());
    let path = ws
        .prepare(run_a(), &clone_spec(Path::new("/srv/repos/app")))
        .await
        .unwrap();
    ws.fetch_back(run_a(), Path::new("/srv/repos/app"))
        .await
        .unwrap();

    let calls = fake.calls();
    let fetch = &calls.last().unwrap().0;
    let n = fetch.len();
    assert_eq!(fetch[n - 3], "--", "{fetch:?}");
    assert_eq!(
        fetch[n - 2],
        path.join(".git").to_str().unwrap(),
        "{fetch:?}"
    );
    assert_eq!(
        fetch[n - 1],
        format!("refs/heads/{BRANCH_A}:refs/heads/{BRANCH_A}"),
        "{fetch:?}"
    );
}

#[tokio::test]
async fn s4_a_force_rewritten_clone_branch_errors_and_leaves_the_source_branch_intact() {
    need_git!();
    let f = fixture();
    let ws = Workspaces::new(f.root.clone(), Arc::new(ProcessRunner));
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    commit_file(&clone, "one.txt", "one\n");
    ws.fetch_back(run_a(), &f.repo).await.unwrap();
    let first = git(&f.repo, &["rev-parse", BRANCH_A]);

    // The agent rewrites history: drop its commit and make a different one.
    git(&clone, &["reset", "-q", "--hard", "HEAD~1"]);
    commit_file(&clone, "two.txt", "two\n");
    assert_ne!(git(&clone, &["rev-parse", "HEAD"]), first);

    let err = ws.fetch_back(run_a(), &f.repo).await.unwrap_err();

    assert!(matches!(err, WorkspaceError::Git { .. }), "{err:?}");
    assert_eq!(
        git(&f.repo, &["rev-parse", BRANCH_A]),
        first,
        "branch intact"
    );
}

// ---------------------------------------------------------------------------------------------
// Scenario 5: a relative repo is refused by `prepare` itself, before any git call.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn s5_a_relative_repo_is_refused_before_any_git_call() {
    for repo in ["some/repo", "./repo", ".", "repo"] {
        let (_tmp, root) = fake_root();
        let fake = Arc::new(FakeOk::default());
        let ws = Workspaces::new(root.clone(), fake.clone());

        let err = ws
            .prepare(run_a(), &clone_spec(Path::new(repo)))
            .await
            .unwrap_err();

        match &err {
            WorkspaceError::RelativeRepo { repo: r } => assert_eq!(r, Path::new(repo)),
            other => panic!("expected RelativeRepo for `{repo}`, got {other:?}"),
        }
        assert!(err.to_string().contains(repo), "names the path: {err}");
        assert!(err.to_string().contains("absolute"), "{err}");
        assert!(fake.calls().is_empty(), "git ran for `{repo}`");
        assert!(!root.exists(), "nothing created for `{repo}`");
    }
}

#[tokio::test]
async fn s5_a_relative_repo_with_a_depth_is_refused_the_same_way() {
    let (_tmp, root) = fake_root();
    let fake = Arc::new(FakeOk::default());
    let ws = Workspaces::new(root, fake.clone());
    let spec = WorkspaceSpec::GitClone {
        repo: PathBuf::from("rel/repo"),
        depth: Some(1),
    };

    let err = ws.prepare(run_a(), &spec).await.unwrap_err();

    assert!(
        matches!(err, WorkspaceError::RelativeRepo { .. }),
        "{err:?}"
    );
    assert!(fake.calls().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Round 2: object alternates, a symlinked clone directory.
// ---------------------------------------------------------------------------------------------

/// A host repo with a commit that the source repo does not have. Returns (repo, commit id).
fn secret_repo(f: &Fixture) -> (PathBuf, String) {
    let secret = f.repo.parent().unwrap().join("secret");
    std::fs::create_dir(&secret).unwrap();
    git(&secret, &["init", "-q"]);
    commit_file(&secret, "secret.txt", "private\n");
    let id = git(&secret, &["rev-parse", "HEAD"]);
    (secret, id)
}

async fn assert_alternates_refused(f: &Fixture, setup: impl FnOnce(&Path, &Path, &str)) {
    let (secret, id) = secret_repo(f);
    let (ws, rec, clone) = prepared(f).await;
    setup(&clone, &secret, &id);
    let calls_before = rec.argvs().len();

    let err = ws.fetch_back(run_a(), &f.repo).await.unwrap_err();

    match &err {
        WorkspaceError::AlternatesPresent { clone: c } => assert_eq!(c, &clone),
        other => panic!("expected AlternatesPresent, got {other:?}"),
    }
    assert!(err.to_string().contains("alternates"), "{err}");
    assert_eq!(rec.argvs().len(), calls_before, "no git call was made");
    assert_eq!(kestrel_refs(&f.repo), "", "source got no ref");
    assert!(
        !git_raw(&f.repo, &["cat-file", "-e", &id]).status.success(),
        "the secret commit reached the source repo"
    );
}

#[tokio::test]
async fn alternates_pointing_at_another_repo_make_fetch_back_refuse() {
    need_git!();
    let f = fixture();
    assert_alternates_refused(&f, |clone, secret, id| {
        let info = clone.join(".git/objects/info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(
            info.join("alternates"),
            format!("{}\n", secret.join(".git/objects").display()),
        )
        .unwrap();
        git(
            clone,
            &["update-ref", &format!("refs/heads/{BRANCH_A}"), id],
        );
    })
    .await;
}

#[tokio::test]
async fn a_dangling_alternates_symlink_also_makes_fetch_back_refuse() {
    need_git!();
    let f = fixture();
    assert_alternates_refused(&f, |clone, _secret, _id| {
        let info = clone.join(".git/objects/info");
        std::fs::create_dir_all(&info).unwrap();
        std::os::unix::fs::symlink("/nonexistent/alternates", info.join("alternates")).unwrap();
    })
    .await;
}

#[tokio::test]
async fn an_http_alternates_file_makes_fetch_back_refuse() {
    need_git!();
    let f = fixture();
    assert_alternates_refused(&f, |clone, _secret, _id| {
        let info = clone.join(".git/objects/info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("http-alternates"), "http://example.invalid/\n").unwrap();
    })
    .await;
}

#[tokio::test]
async fn a_symlinked_clone_directory_is_refused_before_any_git_call() {
    need_git!();
    let f = fixture();
    let decoy = decoy_with_branch(&f);
    let (ws, rec, clone) = prepared(&f).await;
    std::fs::remove_dir_all(&clone).unwrap();
    std::os::unix::fs::symlink(&decoy, &clone).unwrap();
    let decoy_before = git(&decoy, &["for-each-ref"]);
    let calls_before = rec.argvs().len();

    let err = ws.fetch_back(run_a(), &f.repo).await.unwrap_err();

    match &err {
        WorkspaceError::GitDirNotADirectory { clone: c } => assert_eq!(c, &clone),
        other => panic!("expected GitDirNotADirectory, got {other:?}"),
    }
    assert_eq!(rec.argvs().len(), calls_before, "no git call was made");
    assert_eq!(kestrel_refs(&f.repo), "", "source repo untouched");
    assert_eq!(git(&decoy, &["for-each-ref"]), decoy_before);
}
