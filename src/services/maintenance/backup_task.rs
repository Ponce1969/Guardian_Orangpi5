//! One PostgreSQL backup run, start to finish.
//!
//! The ordering in [`BackupTask::run`] is the design, and it is deliberately
//! paranoid about what counts as success:
//!
//! 1. `pg_dump` must exit zero;
//! 2. the file it claims to have written must exist and be non-empty;
//! 3. `pg_restore --list` must be able to read it as an archive;
//! 4. only then are older backups pruned.
//!
//! Each step exists because the previous one can lie. A dump can exit zero and
//! write nothing; it can write a truncated file; and a file can be present and
//! unreadable. Pruning before the replacement is proven is how a working backup
//! gets deleted by a broken one.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::NaiveDateTime;

use super::backup::{backups_to_prune, plan_dump, redact, BackupSettings};
use super::executor::{ExecutionError, ExecutionOutput, Executor};
use super::outcome::{classify_execution, summarize, MaintenanceOutcome, MaintenanceStatus};

/// Where backups live when nothing else is configured.
pub const DEFAULT_BACKUP_DIR: &str = "/var/backups/guardianrs/postgres";

/// How many backups to keep when nothing else is configured.
pub const DEFAULT_KEEP_BACKUPS: usize = 7;

/// A configured backup task.
#[derive(Debug, Clone)]
pub struct BackupTask {
    pub settings: BackupSettings,
    pub destination: PathBuf,
    pub keep_backups: usize,
    pub timeout: Duration,
    pub grace: Duration,
    pub verify_timeout: Duration,
}

impl BackupTask {
    /// A task with the documented defaults and the standard deadines.
    pub fn new(settings: BackupSettings) -> Self {
        Self {
            settings,
            destination: PathBuf::from(DEFAULT_BACKUP_DIR),
            keep_backups: DEFAULT_KEEP_BACKUPS,
            // A dump may legitimately take many minutes on a small board.
            timeout: Duration::from_secs(1800),
            grace: Duration::from_secs(30),
            // Listing an archive is a metadata read: seconds, not minutes.
            verify_timeout: Duration::from_secs(120),
        }
    }

    /// Run the task, returning an outcome rather than an error.
    ///
    /// Every failure mode becomes a status, because an alert is the point: a
    /// backup that did not happen and reports nothing is worse than one that
    /// fails loudly.
    pub async fn run(&self, executor: &dyn Executor, now: NaiveDateTime) -> MaintenanceOutcome {
        let started = Instant::now();
        let plan = plan_dump(
            &self.settings,
            &self.destination,
            now,
            self.timeout,
            self.grace,
            self.verify_timeout,
        );

        // 1. The dump itself.
        let dump = match executor.run(&plan.dump).await {
            Ok(output) => output,
            // A missing binary is a configuration problem, not a failed backup, so
            // it is reported once rather than failing every cycle forever.
            Err(ExecutionError::Spawn { reason, .. }) => {
                return self.outcome(
                    started,
                    MaintenanceStatus::Skipped,
                    None,
                    &format!("pg_dump is unavailable: {reason}"),
                );
            }
            Err(ExecutionError::Collect { reason }) => {
                return self.outcome(
                    started,
                    MaintenanceStatus::Failed,
                    None,
                    &format!("the dump process could not be collected: {reason}"),
                );
            }
        };

        let dump_status = classify_execution(&dump);
        if dump_status != MaintenanceStatus::Success {
            return self.outcome(started, dump_status, dump.exit_code, &self.detail(&dump));
        }

        // 2. The file it claims to have written. An exit code of zero is not
        // evidence that bytes reached the disk.
        let bytes = std::fs::metadata(&plan.destination)
            .map(|metadata| metadata.len())
            .unwrap_or(0);

        if bytes == 0 {
            return self.outcome(
                started,
                MaintenanceStatus::Failed,
                dump.exit_code,
                "pg_dump reported success but the backup file is missing or empty",
            );
        }

        // 3. The archive must be readable, not merely large.
        let verify = match executor.run(&plan.verify).await {
            Ok(output) => output,
            Err(ExecutionError::Spawn { reason, .. }) => {
                return self.outcome(
                    started,
                    MaintenanceStatus::Failed,
                    None,
                    &format!("the backup cannot be verified, pg_restore is unavailable: {reason}"),
                );
            }
            Err(ExecutionError::Collect { reason }) => {
                return self.outcome(
                    started,
                    MaintenanceStatus::Failed,
                    None,
                    &format!("the verification process could not be collected: {reason}"),
                );
            }
        };

        if classify_execution(&verify) != MaintenanceStatus::Success {
            return self.outcome(
                started,
                MaintenanceStatus::Failed,
                verify.exit_code,
                &format!(
                    "the dump is not a readable archive: {}",
                    self.detail(&verify)
                ),
            );
        }

        // 4. Retention, and only now. Pruning before the replacement is proven is
        // how a working backup gets deleted by a broken one.
        let detail = match self.prune() {
            Ok(0) => String::new(),
            Ok(count) => format!("pruned {count} older backup(s)"),
            // The backup is valid; a pruning failure is a separate problem and
            // must not be reported as a failed backup.
            Err(reason) => format!("the backup is valid but pruning failed: {reason}"),
        };

        self.outcome(started, MaintenanceStatus::Success, Some(0), &detail)
    }

    /// Bounded, single-line, secret-free text for the outcome.
    fn detail(&self, output: &ExecutionOutput) -> String {
        redact(&summarize(&output.stderr), self.settings.password())
    }

    fn outcome(
        &self,
        started: Instant,
        status: MaintenanceStatus,
        exit_code: Option<i32>,
        detail: &str,
    ) -> MaintenanceOutcome {
        MaintenanceOutcome {
            task: "postgres_backup".to_string(),
            status,
            exit_code,
            duration: started.elapsed(),
            detail: detail.to_string(),
        }
    }

    /// Delete the backups beyond the retention limit, returning how many went.
    fn prune(&self) -> Result<usize, String> {
        let entries = std::fs::read_dir(&self.destination)
            .map_err(|error| format!("cannot list {}: {error}", self.destination.display()))?;

        let mut names = Vec::new();
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }

        let doomed = backups_to_prune(&names, self.keep_backups);

        for name in &doomed {
            std::fs::remove_file(self.destination.join(name))
                .map_err(|error| format!("cannot remove {name}: {error}"))?;
        }

        Ok(doomed.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::super::executor::ExecutionRequest;

    /// Scripted stand-in for a real process. Records the requests it saw so a
    /// test can assert that verification ran or that pruning did not.
    #[derive(Default)]
    struct Script {
        answers: Mutex<Vec<Result<ExecutionOutput, ExecutionError>>>,
        programs: Mutex<Vec<String>>,
    }

    impl Script {
        fn with(answers: Vec<Result<ExecutionOutput, ExecutionError>>) -> Self {
            Self {
                answers: Mutex::new(answers),
                programs: Mutex::new(Vec::new()),
            }
        }

        fn programs(&self) -> Vec<String> {
            self.programs.lock().expect("lock").clone()
        }
    }

    struct ScriptedExecutor {
        script: std::sync::Arc<Script>,
    }

    #[async_trait]
    impl Executor for ScriptedExecutor {
        async fn run(&self, request: &ExecutionRequest) -> Result<ExecutionOutput, ExecutionError> {
            self.script
                .programs
                .lock()
                .expect("lock")
                .push(request.program().to_string());

            let mut answers = self.script.answers.lock().expect("lock");
            if answers.is_empty() {
                return Err(ExecutionError::Collect {
                    reason: "script exhausted".to_string(),
                });
            }
            answers.remove(0)
        }
    }

    fn ok(exit_code: i32) -> ExecutionOutput {
        ExecutionOutput {
            exit_code: Some(exit_code),
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        }
    }

    fn ok_with_stderr(exit_code: i32, stderr: &str) -> ExecutionOutput {
        ExecutionOutput {
            exit_code: Some(exit_code),
            stdout: String::new(),
            stderr: stderr.to_string(),
            timed_out: false,
        }
    }

    /// A unique directory per test, removed by the caller when it matters.
    fn scratch_dir(label: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);

        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "guardian-backup-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn task_in(dir: &std::path::Path, keep: usize) -> BackupTask {
        let mut task = BackupTask::new(BackupSettings::new(
            "127.0.0.1",
            5432,
            "guardian",
            "bookgraph",
            "s3cret",
        ));
        task.destination = dir.to_path_buf();
        task.keep_backups = keep;
        task
    }

    fn executor_with(
        script: Vec<Result<ExecutionOutput, ExecutionError>>,
    ) -> (ScriptedExecutor, std::sync::Arc<Script>) {
        let script = std::sync::Arc::new(Script::with(script));
        (
            ScriptedExecutor {
                script: std::sync::Arc::clone(&script),
            },
            script,
        )
    }

    /// Writes a file where the dump is expected, so the file check passes.
    fn write_dump(task: &BackupTask, now: NaiveDateTime, bytes: &[u8]) {
        let plan = plan_dump(
            &task.settings,
            &task.destination,
            now,
            task.timeout,
            task.grace,
            task.verify_timeout,
        );
        std::fs::write(&plan.destination, bytes).expect("dump written");
    }

    fn now() -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 3, 7)
            .and_then(|date| date.and_hms_opt(3, 30, 0))
            .expect("valid timestamp")
    }

    #[tokio::test]
    async fn a_verified_dump_reports_success() {
        let dir = scratch_dir("success");
        let task = task_in(&dir, 7);
        write_dump(&task, now(), b"valid archive");
        let (executor, _) = executor_with(vec![Ok(ok(0)), Ok(ok(0))]);

        let outcome = task.run(&executor, now()).await;

        assert_eq!(outcome.status, MaintenanceStatus::Success);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A dump that exits zero without producing a file is the failure this
    /// ordering exists to catch.
    #[tokio::test]
    async fn a_dump_without_a_file_is_failed() {
        let dir = scratch_dir("nofile");
        let task = task_in(&dir, 7);
        let (executor, _) = executor_with(vec![Ok(ok(0))]);

        let outcome = task.run(&executor, now()).await;

        assert_eq!(outcome.status, MaintenanceStatus::Failed);
        assert!(outcome.detail.contains("empty") || outcome.detail.contains("missing"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_empty_file_is_failed() {
        let dir = scratch_dir("empty");
        let task = task_in(&dir, 7);
        write_dump(&task, now(), b"");
        let (executor, _) = executor_with(vec![Ok(ok(0))]);

        let outcome = task.run(&executor, now()).await;

        assert_eq!(outcome.status, MaintenanceStatus::Failed);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_failed_dump_never_runs_verification() {
        let dir = scratch_dir("dumpfail");
        let task = task_in(&dir, 7);
        let (executor, script) = executor_with(vec![Ok(ok_with_stderr(1, "connection refused"))]);

        let outcome = task.run(&executor, now()).await;

        assert_eq!(outcome.status, MaintenanceStatus::Failed);
        assert_eq!(
            script.programs(),
            vec!["pg_dump".to_string()],
            "pg_restore must not run after a failed dump"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A missing binary is a configuration problem, not a backup failure.
    #[tokio::test]
    async fn a_missing_binary_is_skipped() {
        let dir = scratch_dir("missing");
        let task = task_in(&dir, 7);
        let (executor, _) = executor_with(vec![Err(ExecutionError::Spawn {
            program: "pg_dump".to_string(),
            reason: "No such file or directory".to_string(),
        })]);

        let outcome = task.run(&executor, now()).await;

        assert_eq!(outcome.status, MaintenanceStatus::Skipped);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An unreadable archive is a failed backup even though the dump exited zero.
    #[tokio::test]
    async fn a_failed_verification_is_failed() {
        let dir = scratch_dir("verifyfail");
        let task = task_in(&dir, 7);
        write_dump(&task, now(), b"not really an archive");
        let (executor, _) = executor_with(vec![Ok(ok(0)), Ok(ok_with_stderr(1, "not a dump"))]);

        let outcome = task.run(&executor, now()).await;

        assert_eq!(outcome.status, MaintenanceStatus::Failed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The reason pruning waits for verification: a broken dump must not be able
    /// to delete the last good one.
    #[tokio::test]
    async fn pruning_does_not_happen_when_verification_fails() {
        let dir = scratch_dir("noprune");
        let task = task_in(&dir, 1);
        let old = super::super::backup::backup_filename(
            "bookgraph",
            chrono::NaiveDate::from_ymd_opt(2026, 3, 1)
                .and_then(|date| date.and_hms_opt(3, 30, 0))
                .expect("valid timestamp"),
        );
        std::fs::write(dir.join(&old), b"previous good backup").expect("old backup");
        write_dump(&task, now(), b"corrupt");
        let (executor, _) = executor_with(vec![Ok(ok(0)), Ok(ok_with_stderr(1, "not a dump"))]);

        task.run(&executor, now()).await;

        assert!(
            dir.join(&old).exists(),
            "the previous backup must survive a failed verification"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn old_backups_are_pruned_after_a_verified_success() {
        let dir = scratch_dir("prune");
        let task = task_in(&dir, 1);
        let old = super::super::backup::backup_filename(
            "bookgraph",
            chrono::NaiveDate::from_ymd_opt(2026, 3, 1)
                .and_then(|date| date.and_hms_opt(3, 30, 0))
                .expect("valid timestamp"),
        );
        std::fs::write(dir.join(&old), b"previous backup").expect("old backup");
        write_dump(&task, now(), b"valid archive");
        let (executor, _) = executor_with(vec![Ok(ok(0)), Ok(ok(0))]);

        task.run(&executor, now()).await;

        assert!(!dir.join(&old).exists(), "the old backup is pruned");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The secret must not reach a notification through stderr.
    #[tokio::test]
    async fn the_detail_never_contains_the_password() {
        let dir = scratch_dir("redact");
        let task = task_in(&dir, 7);
        let (executor, _) = executor_with(vec![Ok(ok_with_stderr(
            1,
            "connection to server failed: password s3cret rejected",
        ))]);

        let outcome = task.run(&executor, now()).await;

        assert_eq!(outcome.status, MaintenanceStatus::Failed);
        assert!(
            !outcome.detail.contains("s3cret"),
            "the password leaked into the detail: {}",
            outcome.detail
        );
        assert!(outcome.detail.contains("***"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
