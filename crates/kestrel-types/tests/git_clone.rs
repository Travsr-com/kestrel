//! Issue #22: `workspace: git-clone` and `sandbox.clone_depth` in the Agent YAML.

#![allow(clippy::expect_used)]

use kestrel_types::{AgentSpec, WorkspaceSpec};

fn yaml(sandbox_extra: &str, workspace: &str) -> String {
    format!(
        "name: a\nruntime: claude-code\nrole: r\nmodel_policy: {{ default: tier:cheap }}\n\
         sandbox: {{ tier: auto, cpu: 1, memory: 1Gi, workspace: {workspace}{sandbox_extra} }}\n\
         budget: {{ usd: 1.00, wall_clock: 10m }}\n"
    )
}

fn parse(sandbox_extra: &str, workspace: &str) -> Result<AgentSpec, String> {
    AgentSpec::from_yaml(&yaml(sandbox_extra, workspace)).map_err(|e| e.to_string())
}

fn workspace(sandbox_extra: &str) -> WorkspaceSpec {
    parse(sandbox_extra, "git-clone")
        .expect("spec parses")
        .sandbox
        .workspace
}

#[test]
fn s1_git_clone_parses_with_repo_dot_and_no_depth() {
    assert_eq!(
        workspace(""),
        WorkspaceSpec::GitClone {
            repo: ".".into(),
            depth: None
        }
    );
}

#[test]
fn s1_clone_depth_sets_the_depth() {
    for (text, want) in [("1", 1), ("2", 2), ("50", 50), ("4294967295", u32::MAX)] {
        assert_eq!(
            workspace(&format!(", clone_depth: {text}")),
            WorkspaceSpec::GitClone {
                repo: ".".into(),
                depth: Some(want)
            },
            "clone_depth: {text}"
        );
    }
}

#[test]
fn s1_git_worktree_is_no_longer_accepted_and_the_error_lists_the_supported_values() {
    let err = parse("", "git-worktree").unwrap_err();
    assert!(err.contains("git-worktree"), "{err}");
    assert!(err.contains("git-clone"), "{err}");
    assert!(err.contains("empty-dir"), "{err}");
    assert!(err.contains("workspace"), "names the field: {err}");
}

/// Guards the rejection tests: they would pass for the wrong reason (an unknown key) if
/// `clone_depth: 1` itself did not parse.
fn assert_clone_depth_is_a_known_key() {
    assert_eq!(
        workspace(", clone_depth: 1"),
        WorkspaceSpec::GitClone {
            repo: ".".into(),
            depth: Some(1)
        }
    );
}

#[test]
fn s1_clone_depth_zero_is_rejected_naming_the_field() {
    assert_clone_depth_is_a_known_key();
    let err = parse(", clone_depth: 0", "git-clone").unwrap_err();
    assert!(err.contains("clone_depth"), "{err}");
}

#[test]
fn s1_clone_depth_that_is_not_a_positive_integer_is_rejected_naming_the_field() {
    assert_clone_depth_is_a_known_key();
    for bad in ["abc", "\"3\"", "-1", "1.5", "true", "[1]", "4294967296"] {
        let err = parse(&format!(", clone_depth: {bad}"), "git-clone")
            .expect_err(&format!("clone_depth: {bad} must be rejected"));
        assert!(err.contains("clone_depth"), "clone_depth: {bad}: {err}");
    }
}

#[test]
fn s1_unknown_sandbox_keys_are_still_rejected() {
    // `clone_depth` must not loosen the strictness of the sandbox block.
    assert_clone_depth_is_a_known_key();
    assert!(parse(", clone_dept: 1", "git-clone").is_err());
}

#[test]
fn s1_git_clone_with_depth_round_trips_through_json() {
    let spec = parse(", clone_depth: 7", "git-clone").expect("spec parses");
    let json = serde_json::to_string(&spec).expect("to json");
    let back: AgentSpec = serde_json::from_str(&json).expect("from json");
    assert_eq!(back, spec);
    assert_eq!(
        back.sandbox.workspace,
        WorkspaceSpec::GitClone {
            repo: ".".into(),
            depth: Some(7)
        }
    );
}

#[test]
fn s1_variant_serializes_as_git_clone() {
    let w = WorkspaceSpec::GitClone {
        repo: "/srv/app".into(),
        depth: Some(1),
    };
    let v = serde_json::to_value(&w).expect("to json");
    assert!(v.get("git-clone").is_some(), "{v}");
    assert_eq!(
        serde_json::from_value::<WorkspaceSpec>(v).expect("from json"),
        w
    );
}

#[test]
fn clone_depth_with_empty_dir_is_rejected_naming_the_field() {
    let err = parse(", clone_depth: 1", "empty-dir").expect_err("depth needs git-clone");
    assert!(err.contains("sandbox.clone_depth"), "{err}");
}
