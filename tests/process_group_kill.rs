//! Cancelling a run must take the whole process group with it.
//!
//! `kill_on_drop` reaps the direct child only. A cancelled `pg_dump` can leave the
//! children it forked still running, holding the database connection and its locks,
//! which is worse than the run never having started: the next backup blocks on a
//! process nobody can explain.
//!
//! `GroupKillGuard` closes that by killing the process group when the run future is
//! dropped. This test drops it the way a shutdown does and looks at the grandchild.
//!
//! Unix only: it forks a shell and signals a process group.

#![cfg(unix)]

use std::time::Duration;

use guardian_rs::services::maintenance::{ExecutionRequest, Executor, ProcessExecutor};

/// Whether `pid` still exists. Signal 0 does no work and only checks permission.
fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn pidfile() -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("guardian-group-{}.pid", std::process::id()));
    path
}

#[tokio::test]
async fn cancelling_a_run_kills_its_grandchildren() {
    let path = pidfile();
    let _ = std::fs::remove_file(&path);

    // The shell reports the pid of the process it forked, so the test knows exactly
    // which grandchild to look at. Matching a command line with `pgrep -f` would
    // also match this test's own invocation.
    let script = format!("sleep 137 & echo $! > {} ; wait", path.display());

    let request = ExecutionRequest::new(
        "sh",
        vec!["-c".to_string(), script],
        Duration::from_secs(600),
    );

    let handle = tokio::spawn(async move {
        let executor = ProcessExecutor::new();
        executor.run(&request).await
    });

    // Long enough for the shell to fork and record the pid.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let pid = std::fs::read_to_string(&path)
        .expect("the shell records the grandchild pid")
        .trim()
        .to_string();

    assert!(!pid.is_empty(), "the pid file is not empty");
    assert!(
        alive(&pid),
        "the grandchild is running before cancellation, otherwise the rest proves nothing"
    );

    // Aborting drops the run future, which is exactly what a shutdown does.
    handle.abort();
    let _ = handle.await;

    // The guard runs on drop; the signal and the reap need a moment.
    tokio::time::sleep(Duration::from_millis(800)).await;

    assert!(
        !alive(&pid),
        "the grandchild survived the cancellation: it keeps the connection and the locks"
    );

    let _ = std::fs::remove_file(&path);
}
