//! Allowed workspace roots (issue #15): a workspace is mounted only when its resolved path
//! (symlinks and `..` followed) is inside an allowed root, and the resolved path is what gets
//! mounted. Uses real temp dirs and symlinks and a fake runner.

#![allow(clippy::expect_used, clippy::unwrap_used)]
#![cfg(unix)]

mod common;

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{FakeRunner, spec};
use kestrel_sandbox::{
    AppleContainerDriver, Arch, DockerRuncDriver, GvisorDriver, HostInfo, NetworkPolicy, Os,
    RootsError, SandboxDriver, SandboxError, detect_with_roots, resolve_workspace,
};
use url::Url;

/// A temp dir with its own canonical path, so assertions are not fooled by `/tmp` symlinks.
struct Tmp {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

fn tmp() -> Tmp {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical");
    Tmp { _dir: dir, root }
}

fn mkdir(p: &Path) -> PathBuf {
    std::fs::create_dir_all(p).expect("mkdir");
    p.to_path_buf()
}

fn roots(r: &[&Path]) -> Vec<PathBuf> {
    r.iter().map(|p| p.to_path_buf()).collect()
}

// Function level: resolve_workspace.

#[test]
fn a_workspace_inside_a_root_resolves_to_its_canonical_path() {
    let t = tmp();
    let ws = mkdir(&t.root.join("ws/run1"));
    let got = resolve_workspace(&ws, &roots(&[&t.root.join("ws")])).expect("allowed");
    assert_eq!(got, ws);
}

#[test]
fn dot_segments_and_trailing_slashes_are_normalised() {
    let t = tmp();
    let ws = mkdir(&t.root.join("ws/run1"));
    let messy = t.root.join("ws/./run1/../run1/");
    let got = resolve_workspace(&messy, &roots(&[&t.root.join("ws")])).expect("allowed");
    assert_eq!(got, ws);
}

#[test]
fn a_path_equal_to_a_root_is_allowed() {
    let t = tmp();
    let root = mkdir(&t.root.join("ws"));
    assert_eq!(
        resolve_workspace(&root, &roots(&[&root])).expect("ok"),
        root
    );
}

#[test]
fn dotdot_that_leaves_the_root_is_refused() {
    let t = tmp();
    let root = mkdir(&t.root.join("ws"));
    mkdir(&t.root.join("etc"));
    let sneaky = root.join("../etc");
    let err = resolve_workspace(&sneaky, &roots(&[&root])).expect_err("escape");
    assert!(
        matches!(err, RootsError::OutsideAllowedRoots { .. }),
        "{err:?}"
    );
}

#[test]
fn dotdot_that_stays_inside_the_root_is_fine() {
    let t = tmp();
    let root = mkdir(&t.root.join("ws"));
    let a = mkdir(&root.join("a"));
    mkdir(&root.join("b"));
    let got = resolve_workspace(&a.join("../b"), &roots(&[&root])).expect("inside");
    assert_eq!(got, root.join("b"));
}

#[test]
fn a_symlink_inside_the_root_pointing_outside_is_refused() {
    let t = tmp();
    let root = mkdir(&t.root.join("ws"));
    let outside = mkdir(&t.root.join("outside"));
    symlink(&outside, root.join("link")).expect("symlink");
    let err = resolve_workspace(&root.join("link"), &roots(&[&root])).expect_err("escape");
    assert!(
        matches!(err, RootsError::OutsideAllowedRoots { .. }),
        "{err:?}"
    );
}

#[test]
fn a_symlink_in_the_middle_of_the_path_is_followed_too() {
    let t = tmp();
    let root = mkdir(&t.root.join("ws"));
    let outside = mkdir(&t.root.join("outside"));
    mkdir(&outside.join("deep"));
    symlink(&outside, root.join("link")).expect("symlink");
    let err = resolve_workspace(&root.join("link/deep"), &roots(&[&root])).expect_err("escape");
    assert!(
        matches!(err, RootsError::OutsideAllowedRoots { .. }),
        "{err:?}"
    );
}

#[test]
fn a_symlink_that_stays_inside_the_root_is_allowed_and_resolved() {
    let t = tmp();
    let root = mkdir(&t.root.join("ws"));
    let real = mkdir(&root.join("real"));
    symlink(&real, root.join("alias")).expect("symlink");
    let got = resolve_workspace(&root.join("alias"), &roots(&[&root])).expect("inside");
    assert_eq!(got, real, "the target, not the link, is returned");
}

#[test]
fn a_symlinked_root_is_resolved_before_comparing() {
    let t = tmp();
    let real = mkdir(&t.root.join("real-root"));
    let sub = mkdir(&real.join("sub"));
    let link = t.root.join("root-link");
    symlink(&real, &link).expect("symlink");
    // Reached through the link and through the real path, both against the link as root.
    assert_eq!(
        resolve_workspace(&link.join("sub"), &roots(&[&link])).expect("ok"),
        sub
    );
    assert_eq!(resolve_workspace(&sub, &roots(&[&link])).expect("ok"), sub);
}

#[test]
fn a_sibling_with_the_root_as_a_name_prefix_is_refused() {
    let t = tmp();
    let root = mkdir(&t.root.join("ws"));
    let evil = mkdir(&t.root.join("ws-evil"));
    let err = resolve_workspace(&evil, &roots(&[&root])).expect_err("prefix trick");
    assert!(
        matches!(err, RootsError::OutsideAllowedRoots { .. }),
        "{err:?}"
    );
}

#[test]
fn a_missing_workspace_is_a_typed_not_found_naming_the_path() {
    let t = tmp();
    let root = mkdir(&t.root.join("ws"));
    let err = resolve_workspace(&root.join("nope"), &roots(&[&root])).expect_err("missing");
    assert!(
        matches!(&err, RootsError::NotFound { path } if path.contains("nope")),
        "{err:?}"
    );
    assert!(err.to_string().contains("nope"), "{err}");
}

#[test]
fn a_relative_workspace_is_refused() {
    let t = tmp();
    let root = mkdir(&t.root.join("ws"));
    let err = resolve_workspace(Path::new("ws/run1"), &roots(&[&root])).expect_err("relative");
    assert!(matches!(err, RootsError::NotAbsolute { .. }), "{err:?}");
}

#[test]
fn no_roots_means_nothing_is_allowed() {
    let t = tmp();
    let err = resolve_workspace(&t.root, &[]).expect_err("no roots");
    assert_eq!(err, RootsError::NoRootsConfigured);
}

#[test]
fn a_missing_root_is_skipped_when_another_root_is_usable() {
    let t = tmp();
    let gone = t.root.join("gone");
    let root = mkdir(&t.root.join("ws"));
    let ws = mkdir(&root.join("run1"));
    assert_eq!(
        resolve_workspace(&ws, &roots(&[&gone, &root])).expect("ok"),
        ws
    );
}

#[test]
fn when_no_root_exists_the_error_says_so() {
    let t = tmp();
    let ws = mkdir(&t.root.join("elsewhere"));
    let err = resolve_workspace(&ws, &roots(&[&t.root.join("a"), &t.root.join("b")]))
        .expect_err("no usable root");
    assert_eq!(err, RootsError::NoUsableRoot);
}

#[test]
fn any_of_several_roots_may_contain_the_workspace() {
    let t = tmp();
    let a = mkdir(&t.root.join("a"));
    let b = mkdir(&t.root.join("b"));
    let ws = mkdir(&b.join("run"));
    assert_eq!(resolve_workspace(&ws, &roots(&[&a, &b])).expect("ok"), ws);
}

// Driver level: the three docker-style drivers share the check; Apple is covered explicitly.

fn drivers(allowed: &[&Path]) -> Vec<(Box<dyn SandboxDriver>, Arc<FakeRunner>)> {
    let g = FakeRunner::ok();
    let a = FakeRunner::ok();
    let d = FakeRunner::ok();
    vec![
        (
            Box::new(GvisorDriver::new(g.clone()).with_allowed_roots(roots(allowed))),
            g,
        ),
        (
            Box::new(AppleContainerDriver::new(a.clone()).with_allowed_roots(roots(allowed))),
            a,
        ),
        (
            Box::new(DockerRuncDriver::new(d.clone()).with_allowed_roots(roots(allowed))),
            d,
        ),
    ]
}

fn mount_of(argv: &[String]) -> String {
    argv.iter()
        .find(|a| a.starts_with("type=bind,"))
        .cloned()
        .expect("a --mount value")
}

/// A spec for `ws` that would also show a network create call if a check came too late.
fn spec_for(ws: &Path) -> kestrel_sandbox::LaunchSpec {
    let mut s = spec();
    s.workspace = Some(ws.to_path_buf());
    s.network = NetworkPolicy::GatewayOnly {
        gateway: Url::parse("http://gw:1").expect("url"),
    };
    s
}

#[tokio::test]
async fn a_workspace_inside_a_root_is_created_with_the_canonical_path() {
    let t = tmp();
    let root = mkdir(&t.root.join("ws"));
    let real = mkdir(&root.join("real"));
    symlink(&real, root.join("alias")).expect("symlink");
    for (driver, runner) in drivers(&[&root]) {
        driver
            .create(&spec_for(&root.join("alias/../real")))
            .await
            .expect("created");
        let create = runner.runs().into_iter().last().expect("create call");
        assert_eq!(
            mount_of(&create),
            format!("type=bind,source={},target=/workspace", real.display()),
            "{}",
            driver.name()
        );
    }
}

#[tokio::test]
async fn an_escaping_workspace_is_refused_and_the_runner_is_never_called() {
    let t = tmp();
    let root = mkdir(&t.root.join("ws"));
    mkdir(&t.root.join("etc"));
    let outside = mkdir(&t.root.join("outside"));
    symlink(&outside, root.join("link")).expect("symlink");
    for ws in [root.join("../etc"), root.join("link"), t.root.join("etc")] {
        for (driver, runner) in drivers(&[&root]) {
            let err = driver.create(&spec_for(&ws)).await.expect_err("refused");
            assert!(
                matches!(
                    err,
                    SandboxError::Roots(RootsError::OutsideAllowedRoots { .. })
                ),
                "{}: {err:?}",
                driver.name()
            );
            assert!(
                runner.all_calls().is_empty(),
                "{}: {:?}",
                driver.name(),
                runner.all_calls()
            );
        }
    }
}

#[tokio::test]
async fn a_missing_workspace_is_a_clear_error_not_a_docker_error() {
    let t = tmp();
    let root = mkdir(&t.root.join("ws"));
    for (driver, runner) in drivers(&[&root]) {
        let err = driver
            .create(&spec_for(&root.join("missing")))
            .await
            .expect_err("missing");
        assert!(
            matches!(err, SandboxError::Roots(RootsError::NotFound { .. })),
            "{}: {err:?}",
            driver.name()
        );
        assert!(runner.all_calls().is_empty(), "{}", driver.name());
    }
}

#[tokio::test]
async fn an_empty_root_list_refuses_every_workspace() {
    let t = tmp();
    for (driver, runner) in drivers(&[]) {
        let err = driver
            .create(&spec_for(&t.root))
            .await
            .expect_err("refused");
        assert!(
            matches!(err, SandboxError::Roots(RootsError::NoRootsConfigured)),
            "{}: {err:?}",
            driver.name()
        );
        assert!(runner.all_calls().is_empty(), "{}", driver.name());
    }
}

#[tokio::test]
async fn a_relative_workspace_is_still_refused_before_anything_runs() {
    let t = tmp();
    let root = mkdir(&t.root.join("ws"));
    for (driver, runner) in drivers(&[&root]) {
        let err = driver
            .create(&spec_for(Path::new("ws/run")))
            .await
            .expect_err("relative");
        assert!(
            matches!(err, SandboxError::InvalidWorkspace { .. }),
            "{}: {err:?}",
            driver.name()
        );
        assert!(runner.all_calls().is_empty(), "{}", driver.name());
    }
}

#[tokio::test]
async fn a_resolved_path_that_would_break_the_mount_options_is_refused() {
    let t = tmp();
    let root = mkdir(&t.root.join("ws"));
    let evil = mkdir(&t.root.join("a,b"));
    symlink(&evil, root.join("link")).expect("symlink");
    // Allow the comma directory as a second root so containment alone would pass.
    for (driver, runner) in drivers(&[&root, &evil]) {
        let err = driver
            .create(&spec_for(&root.join("link")))
            .await
            .expect_err("refused");
        assert!(
            matches!(err, SandboxError::InvalidWorkspace { .. }),
            "{}: {err:?}",
            driver.name()
        );
        assert!(runner.all_calls().is_empty(), "{}", driver.name());
    }
}

#[tokio::test]
async fn without_allowed_roots_behaviour_is_unchanged() {
    let t = tmp();
    let ws = mkdir(&t.root.join("anywhere"));
    let runner = FakeRunner::ok();
    let driver = GvisorDriver::new(runner.clone());
    driver
        .create(&spec_for(&ws))
        .await
        .expect("old behaviour: any absolute path");
    let create = runner.runs().into_iter().last().expect("call");
    assert!(mount_of(&create).contains(&format!("source={}", ws.display())));
}

#[tokio::test]
async fn detected_drivers_carry_the_roots() {
    let t = tmp();
    let root = mkdir(&t.root.join("ws"));
    let host = HostInfo {
        os: Os::Linux,
        arch: Arch::X86_64,
        docker_runtimes: vec!["runsc".into(), "runc".into()],
        apple_container: false,
    };
    let found = detect_with_roots(&host, true, None, None, Some(roots(&[&root]))).expect("drivers");
    assert_eq!(found.len(), 2);
    for driver in found {
        // The check fails before any real process would start.
        let err = driver
            .create(&spec_for(&t.root))
            .await
            .expect_err("outside");
        assert!(
            matches!(
                err,
                SandboxError::Roots(RootsError::OutsideAllowedRoots { .. })
            ),
            "{}: {err:?}",
            driver.name()
        );
    }
}
