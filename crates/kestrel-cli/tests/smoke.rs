//! The binary builds and exits cleanly.

#[test]
fn binary_runs_and_exits_zero() {
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_kestrel"))
        .status()
        .expect("kestrel binary should start");
    assert!(status.success());
}
