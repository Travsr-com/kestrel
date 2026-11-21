//! Issue #37: the API token is made on first start and kept in a file only its owner can read.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use kestrel_server::load_or_create_token;

#[test]
fn the_token_is_created_once_and_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.token");
    let first = load_or_create_token(&path).unwrap();
    assert_eq!(first.len(), 64);
    assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(load_or_create_token(&path).unwrap(), first);
    // A trailing newline (a hand-edited file) is not part of the token.
    std::fs::write(&path, format!("{first}\n")).unwrap();
    assert_eq!(load_or_create_token(&path).unwrap(), first);
}

// A token must be long and fit in a WebSocket subprotocol.
#[test]
fn a_short_or_unsafe_token_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.token");
    for bad in [
        "feedface",
        "has space has space has space has space",
        "slash/slash/slash/slash/slash/slash/x",
    ] {
        std::fs::write(&path, bad).unwrap();
        let err = load_or_create_token(&path).unwrap_err();
        assert!(err.to_string().contains("server.token"), "{err}");
    }
    let good = "Ab0_-".repeat(7);
    std::fs::write(&path, &good).unwrap();
    assert_eq!(load_or_create_token(&path).unwrap(), good);
}

#[test]
fn an_empty_token_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.token");
    std::fs::write(&path, "  \n").unwrap();
    assert!(load_or_create_token(&path).is_err());
}

#[cfg(unix)]
#[test]
fn a_token_file_others_can_read_is_tightened() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.token");
    std::fs::write(&path, "a".repeat(64)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    load_or_create_token(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[cfg(unix)]
#[test]
fn the_token_file_is_only_readable_by_its_owner() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.token");
    load_or_create_token(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}
