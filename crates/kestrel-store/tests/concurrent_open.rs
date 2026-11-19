//! Several processes and threads opening one brand-new database file at the same moment.
//!
//! The test re-runs its own binary as child processes (selected by `CHILD_ENV`), so the opens
//! really come from separate processes, like the CLI and the engine starting together.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use kestrel_store::Store;
use tempfile::TempDir;

const TEST_NAME: &str = "first_open_from_many_processes_and_threads_at_once";
const CHILD_ENV: &str = "KESTREL_STORE_OPEN_CHILD_DIR";
const ROUNDS: usize = 20;
const PROCESSES: usize = 4;
const THREADS: usize = 4;

fn db_path(dir: &Path) -> PathBuf {
    dir.join("kestrel.db")
}

fn start_file(dir: &Path) -> PathBuf {
    dir.join("start")
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {path:?}");
        thread::sleep(Duration::from_micros(200));
    }
}

/// Child side: announce readiness, wait for the start signal, then do a first-time open.
fn run_child(dir: &Path, id: &str) {
    std::fs::write(dir.join(format!("ready-{id}")), b"").expect("ready file");
    wait_for(&start_file(dir));
    Store::open(db_path(dir)).expect("child open");
}

fn spawn_child(dir: &Path, i: usize) -> Child {
    Command::new(std::env::current_exe().expect("test binary"))
        .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, dir)
        .env("KESTREL_STORE_OPEN_CHILD_ID", i.to_string())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn child")
}

#[test]
fn first_open_from_many_processes_and_threads_at_once() {
    if let Some(dir) = std::env::var_os(CHILD_ENV) {
        let id = std::env::var("KESTREL_STORE_OPEN_CHILD_ID").expect("child id");
        run_child(Path::new(&dir), &id);
        return;
    }

    for round in 0..ROUNDS {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path();
        let children: Vec<Child> = (0..PROCESSES).map(|i| spawn_child(dir, i)).collect();
        for i in 0..PROCESSES {
            wait_for(&dir.join(format!("ready-{i}")));
        }

        let go = Arc::new(AtomicBool::new(false));
        let threads: Vec<_> = (0..THREADS)
            .map(|_| {
                let path = db_path(dir);
                let go = Arc::clone(&go);
                thread::spawn(move || {
                    while !go.load(Ordering::SeqCst) {
                        std::hint::spin_loop();
                    }
                    Store::open(&path).map(drop)
                })
            })
            .collect();

        std::fs::write(start_file(dir), b"").expect("start file");
        go.store(true, Ordering::SeqCst);

        for t in threads {
            if let Err(e) = t.join().expect("thread panicked") {
                panic!("round {round}: thread open failed: {e}");
            }
        }
        for mut c in children {
            let status = c.wait().expect("wait child");
            assert!(
                status.success(),
                "round {round}: child open failed: {status}"
            );
        }

        let conn = rusqlite::Connection::open(db_path(dir)).expect("raw open");
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("journal_mode");
        assert_eq!(mode, "wal", "round {round}: database must end in WAL mode");
    }
}
