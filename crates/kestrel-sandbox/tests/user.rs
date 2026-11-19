//! Issue #24: the sandbox runs as the host user, so a non-root agent image can write its
//! host-owned workspace. Unit tests with a fake `CommandRunner`; no real processes.
//!
//! Rules under test, for gvisor, docker-runc and Apple `container` alike:
//! * With a workspace, `create` adds `--user <uid>:<gid>` (owner of the workspace directory)
//!   right after `--network <n>` and before `--mount`, and `--env HOME=/tmp` unless the spec
//!   already sets `HOME`. `HOME` joins the sorted spec env, so it comes before `RUST_LOG`.
//! * Without a workspace, nothing is added.
//! * A workspace owned by uid 0 adds neither `--user` nor `HOME` (Kestrel runs as root).
//! * A workspace that cannot be inspected is `SandboxError::WorkspaceUnreadable`, before any
//!   runner call.
//!
//! Apple `container create` takes `-u, --user <user>` (name|uid[:gid]) and `--uid`/`--gid`:
//! https://github.com/apple/container/blob/main/docs/command-reference.md (process options of
//! `container create` and `container run`). `--user uid:gid` is pinned for Apple too.

// Shared helpers below are not `#[test]` fns, but a panic is still the right way to fail here.
#![allow(clippy::expect_used)]
#![cfg(unix)]

mod common;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use common::{FakeRunner, s, spec, ws, ws_user};
use kestrel_sandbox::{
    AppleContainerDriver, DockerRuncDriver, GvisorDriver, LaunchSpec, NetworkPolicy, SandboxDriver,
    SandboxError,
};
use url::Url;

type Lookup = Arc<dyn Fn(&Path) -> io::Result<(u32, u32)> + Send + Sync>;

/// Fixed owner, as if the workspace belonged to uid 1234, gid 5678.
fn owner(uid: u32, gid: u32) -> Lookup {
    Arc::new(move |_| Ok((uid, gid)))
}

/// One driver per CLI, with the argv prefix each one starts `create` with (for the shared
/// spec: 1500 millicpus, 512 MB).
fn drivers(
    runner: &Arc<FakeRunner>,
    lookup: Option<Lookup>,
) -> Vec<(Vec<String>, Box<dyn SandboxDriver>)> {
    let with = |l: &Option<Lookup>| l.clone();
    let mut g = GvisorDriver::new(runner.clone());
    let mut d = DockerRuncDriver::new(runner.clone());
    let mut a = AppleContainerDriver::new(runner.clone());
    if let Some(l) = with(&lookup) {
        let (l1, l2, l3) = (l.clone(), l.clone(), l);
        g = g.with_workspace_owner(move |p| l1(p));
        d = d.with_workspace_owner(move |p| l2(p));
        a = a.with_workspace_owner(move |p| l3(p));
    }
    vec![
        (
            s(&[
                "docker",
                "create",
                "--runtime=runsc",
                "--cpus",
                "1.5",
                "--memory",
                "512m",
            ]),
            Box::new(g),
        ),
        (
            s(&[
                "docker",
                "create",
                "--runtime=runc",
                "--cpus",
                "1.5",
                "--memory",
                "512m",
            ]),
            Box::new(d),
        ),
        (
            s(&["container", "create", "--cpus", "2", "--memory", "512M"]),
            Box::new(a),
        ),
    ]
}

/// Expected `create` argv for the shared spec with workspace `ws`, `user` and env `envs`.
fn expected(
    prefix: &[String],
    network: &str,
    user: Option<&str>,
    ws: &Path,
    envs: &[&str],
) -> Vec<String> {
    let mut argv = prefix.to_vec();
    argv.extend(s(&["--network", network]));
    if let Some(user) = user {
        argv.extend(s(&["--user", user]));
    }
    argv.extend(s(&[
        "--mount",
        &format!("type=bind,source={},target=/workspace", ws.display()),
        "--workdir",
        "/workspace",
    ]));
    for e in envs {
        argv.extend(s(&["--env", e]));
    }
    argv.extend(s(&["alpine:3.20", "sh", "-c", "echo hi"]));
    argv
}

fn count(argv: &[String], flag: &str) -> usize {
    argv.iter().filter(|a| *a == flag).count()
}

// 1: the user comes from the real owner of a real directory, for every driver.

#[tokio::test]
async fn user_is_the_owner_of_a_real_workspace_dir() {
    let user = ws_user();
    for (prefix, driver) in drivers(&FakeRunner::ok(), None) {
        let runner = FakeRunner::ok();
        let driver_runner = drivers(&runner, None)
            .into_iter()
            .find(|(p, _)| *p == prefix)
            .expect("same driver");
        driver_runner.1.create(&spec()).await.expect("create");

        assert_eq!(
            runner.runs(),
            vec![expected(
                &prefix,
                "none",
                Some(&user),
                ws(),
                &["HOME=/tmp", "RUST_LOG=info", "TASK=review PR 42"],
            )],
            "{}",
            driver.name()
        );
    }
}

#[tokio::test]
async fn user_matches_the_std_metadata_of_a_fresh_tempdir() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let meta = std::fs::metadata(dir.path()).expect("stat");
    let mut spec = spec();
    spec.workspace = Some(dir.path().to_path_buf());
    for (_, driver) in drivers(&FakeRunner::ok(), None) {
        let runner = FakeRunner::ok();
        let (prefix, driver2) = drivers(&runner, None)
            .into_iter()
            .find(|(_, d)| d.name() == driver.name())
            .expect("driver");
        driver2.create(&spec).await.expect("create");
        let argv = &runner.runs()[0];
        let at = argv.iter().position(|a| a == "--user").expect("--user");
        assert_eq!(
            argv[at + 1],
            format!("{}:{}", meta.uid(), meta.gid()),
            "{}",
            driver.name()
        );
        assert_eq!(argv[..at - 2], prefix[..], "{}", driver.name());
    }
}

// 2: position, with an injected owner so the numbers are known.

#[tokio::test]
async fn user_sits_after_network_and_before_mount() {
    for (prefix, driver) in drivers(&FakeRunner::ok(), None) {
        let runner = FakeRunner::ok();
        let (_, d) = drivers(&runner, Some(owner(1234, 5678)))
            .into_iter()
            .find(|(p, _)| *p == prefix)
            .expect("driver");
        d.create(&spec()).await.expect("create");

        assert_eq!(
            runner.runs(),
            vec![expected(
                &prefix,
                "none",
                Some("1234:5678"),
                ws(),
                &["HOME=/tmp", "RUST_LOG=info", "TASK=review PR 42"],
            )],
            "{}",
            driver.name()
        );
    }
}

#[tokio::test]
async fn the_lookup_receives_the_workspace_path() {
    let seen: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
    let log = seen.clone();
    let lookup: Lookup = Arc::new(move |p| {
        log.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(p.to_path_buf());
        Ok((1, 2))
    });
    for (_, driver) in drivers(&FakeRunner::ok(), None) {
        let runner = FakeRunner::ok();
        let (_, d) = drivers(&runner, Some(lookup.clone()))
            .into_iter()
            .find(|(_, x)| x.name() == driver.name())
            .expect("driver");
        d.create(&spec()).await.expect("create");
    }
    let seen = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert_eq!(seen, vec![ws().to_path_buf(); 3]);
}

#[tokio::test]
async fn gateway_only_keeps_user_after_the_gateway_network_and_gateway_env_last() {
    let mut spec = spec();
    spec.network = NetworkPolicy::GatewayOnly {
        gateway: Url::parse("http://kestrel-gateway:8080").expect("url"),
    };
    let runner = FakeRunner::ok();
    GvisorDriver::new(runner.clone())
        .with_workspace_owner(|_| Ok((1234, 5678)))
        .create(&spec)
        .await
        .expect("create");

    let runs = runner.runs();
    assert_eq!(runs.len(), 2);
    assert_eq!(
        runs[1],
        expected(
            &s(&[
                "docker",
                "create",
                "--runtime=runsc",
                "--cpus",
                "1.5",
                "--memory",
                "512m"
            ]),
            "kestrel-gw",
            Some("1234:5678"),
            ws(),
            &[
                "HOME=/tmp",
                "RUST_LOG=info",
                "TASK=review PR 42",
                "KESTREL_GATEWAY_URL=http://kestrel-gateway:8080/",
            ],
        )
    );
}

// 3: no workspace, nothing added.

#[tokio::test]
async fn no_workspace_means_no_user_and_no_home() {
    let mut spec = spec();
    spec.workspace = None;
    for (_, driver) in drivers(&FakeRunner::ok(), None) {
        let runner = FakeRunner::ok();
        let lookup: Lookup = Arc::new(|_| panic!("the owner must not be read without a workspace"));
        let (_, d) = drivers(&runner, Some(lookup))
            .into_iter()
            .find(|(_, x)| x.name() == driver.name())
            .expect("driver");
        d.create(&spec).await.expect("create");

        let argv = &runner.runs()[0];
        assert_eq!(count(argv, "--user"), 0, "{}: {argv:?}", driver.name());
        assert!(
            !argv.iter().any(|a| a.starts_with("HOME=")),
            "{}: {argv:?}",
            driver.name()
        );
        assert!(!argv.contains(&"--mount".to_string()), "{argv:?}");
    }
}

// 4: HOME already set by the caller is kept, and not duplicated.

#[tokio::test]
async fn home_from_the_spec_is_kept() {
    for home in ["/home/agent", ""] {
        let mut spec = spec();
        spec.env.insert("HOME".to_string(), home.to_string());
        for (_, driver) in drivers(&FakeRunner::ok(), None) {
            let runner = FakeRunner::ok();
            let (prefix, d) = drivers(&runner, Some(owner(1234, 5678)))
                .into_iter()
                .find(|(_, x)| x.name() == driver.name())
                .expect("driver");
            d.create(&spec).await.expect("create");

            let home_env = format!("HOME={home}");
            assert_eq!(
                runner.runs(),
                vec![expected(
                    &prefix,
                    "none",
                    Some("1234:5678"),
                    ws(),
                    &[&home_env, "RUST_LOG=info", "TASK=review PR 42"],
                )],
                "{}",
                driver.name()
            );
            assert_eq!(
                runner.runs()[0]
                    .iter()
                    .filter(|a| a.starts_with("HOME="))
                    .count(),
                1
            );
        }
    }
}

#[tokio::test]
async fn home_set_without_a_workspace_is_left_alone() {
    let mut spec = spec();
    spec.workspace = None;
    spec.env.insert("HOME".to_string(), "/root".to_string());
    let runner = FakeRunner::ok();
    GvisorDriver::new(runner.clone())
        .create(&spec)
        .await
        .expect("create");
    let argv = &runner.runs()[0];
    assert_eq!(argv.iter().filter(|a| a.starts_with("HOME=")).count(), 1);
    assert!(argv.contains(&"HOME=/root".to_string()));
}

// 5: root-owned workspace (Kestrel runs as root) adds nothing.

#[tokio::test]
async fn a_uid_0_workspace_adds_no_user_and_no_home() {
    for (_, driver) in drivers(&FakeRunner::ok(), None) {
        let runner = FakeRunner::ok();
        let (prefix, d) = drivers(&runner, Some(owner(0, 0)))
            .into_iter()
            .find(|(_, x)| x.name() == driver.name())
            .expect("driver");
        d.create(&spec()).await.expect("create");

        assert_eq!(
            runner.runs(),
            vec![expected(
                &prefix,
                "none",
                None,
                ws(),
                &["RUST_LOG=info", "TASK=review PR 42"],
            )],
            "{}",
            driver.name()
        );
    }
}

// 6: a workspace that cannot be inspected is a typed error before any runner call.

fn unreadable(e: &SandboxError) -> bool {
    matches!(e, SandboxError::WorkspaceUnreadable { .. })
}

async fn rejected_before_any_call(spec: LaunchSpec, lookup: Option<Lookup>) {
    let mut spec = spec;
    // GatewayOnly, so even the network create call would show up if the check came too late.
    spec.network = NetworkPolicy::GatewayOnly {
        gateway: Url::parse("http://gw:1").expect("url"),
    };
    for (_, driver) in drivers(&FakeRunner::ok(), None) {
        let runner = FakeRunner::ok();
        let (_, d) = drivers(&runner, lookup.clone())
            .into_iter()
            .find(|(_, x)| x.name() == driver.name())
            .expect("driver");
        let err = d.create(&spec).await.expect_err("must fail");
        assert!(unreadable(&err), "{}: {err:?}", driver.name());
        assert!(
            runner.all_calls().is_empty(),
            "{}: runner called: {:?}",
            driver.name(),
            runner.all_calls()
        );
    }
}

#[tokio::test]
async fn a_missing_workspace_dir_is_a_typed_error_before_any_runner_call() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut spec = spec();
    spec.workspace = Some(dir.path().join("never-created"));
    rejected_before_any_call(spec, None).await;
}

#[tokio::test]
async fn a_failing_lookup_is_a_typed_error_that_names_the_path() {
    let lookup: Lookup = Arc::new(|_| Err(io::Error::new(io::ErrorKind::PermissionDenied, "nope")));
    rejected_before_any_call(spec(), Some(lookup)).await;

    let runner = FakeRunner::ok();
    let err = GvisorDriver::new(runner)
        .with_workspace_owner(|_| Err(io::Error::new(io::ErrorKind::PermissionDenied, "nope")))
        .create(&spec())
        .await
        .expect_err("must fail");
    let text = err.to_string();
    assert!(text.contains(&ws().display().to_string()), "{text}");
    assert!(text.contains("nope"), "{text}");
    match err {
        SandboxError::WorkspaceUnreadable { path, source } => {
            assert_eq!(path, ws().display().to_string());
            assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
        }
        other => panic!("wrong error: {other:?}"),
    }
}

// 7: the path checks still come first, so a bad path never reaches the lookup.

#[tokio::test]
async fn an_invalid_workspace_path_is_rejected_before_the_owner_is_read() {
    let mut spec = spec();
    spec.workspace = Some(PathBuf::from("relative/ws"));
    let runner = FakeRunner::ok();
    let err = GvisorDriver::new(runner.clone())
        .with_workspace_owner(|_| panic!("must not read the owner of an invalid path"))
        .create(&spec)
        .await
        .expect_err("must fail");
    assert!(
        matches!(err, SandboxError::InvalidWorkspace { .. }),
        "{err:?}"
    );
    assert!(runner.all_calls().is_empty());
}
