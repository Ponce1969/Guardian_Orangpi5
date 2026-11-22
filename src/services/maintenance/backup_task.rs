//! One PostgreSQL backup run, start to finish.
//!
//! The ordering in [`BackupTask::run`] is the design, and it is deliberately
//! paranoid about what counts as success:
//!
//! 1. `pg_dump` must exit zero;
//! 2. the file it claims to have written must exist and be non-empty;
//! 3. `pg_restore --list` must read its table of contents;
//! 4. `pg_restore` must read its data back without error;
//! 5. only then are older backups pruned.
//!
//! Each step exists because the previous one can lie. A dump can exit zero and
//! write nothing; it can write a truncated file; and a file can be present and
//! unreadable. Pruning before the replacement is proven is how a working backup
//! gets deleted by a broken one.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::NaiveDateTime;

use super::backup::{
    backups_to_prune, belongs_to_database, plan_dump, redact, BackupPlan, BackupSettings,
};
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

        if let Err(error) = std::fs::create_dir_all(&self.destination) {
            return self.outcome(
                started,
                MaintenanceStatus::Failed,
                None,
                &format!("cannot create {}: {error}", self.destination.display()),
            );
        }

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
                self.discard(&plan);
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
            self.discard(&plan);
            return self.outcome(started, dump_status, dump.exit_code, &self.detail(&dump));
        }

        // 2. The file it claims to have written. An exit code of zero is not
        // evidence that bytes reached the disk.
        let bytes = std::fs::metadata(&plan.partial)
            .map(|metadata| metadata.len())
            .unwrap_or(0);

        if bytes == 0 {
            self.discard(&plan);
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
                self.discard(&plan);
                return self.outcome(
                    started,
                    MaintenanceStatus::Failed,
                    None,
                    &format!("the verification process could not be collected: {reason}"),
                );
            }
        };

        if classify_execution(&verify) != MaintenanceStatus::Success {
            self.discard(&plan);
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

        let restore = match executor.run(&plan.restore).await {
            Ok(output) => output,
            Err(error) => {
                self.discard(&plan);
                let reason = match error {
                    ExecutionError::Spawn { reason, .. } | ExecutionError::Collect { reason } => {
                        reason
                    }
                };
                return self.outcome(
                    started,
                    MaintenanceStatus::Failed,
                    None,
                    &format!("the dump data cannot be read: {reason}"),
                );
            }
        };

        if classify_execution(&restore) != MaintenanceStatus::Success {
            self.discard(&plan);
            return self.outcome(
                started,
                MaintenanceStatus::Failed,
                restore.exit_code,
                &format!("the dump is not restorable: {}", self.detail(&restore)),
            );
        }

        let _ = std::fs::remove_file(&plan.restore_temp);

        if let Err(error) = std::fs::rename(&plan.partial, &plan.destination) {
            self.discard(&plan);
            return self.outcome(
                started,
                MaintenanceStatus::Failed,
                None,
                &format!("cannot publish the backup: {error}"),
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

    /// Remove whatever a failed run may have left behind.
    fn discard(&self, plan: &BackupPlan) {
        let _ = std::fs::remove_file(&plan.partial);
        let _ = std::fs::remove_file(&plan.restore_temp);
    }

    /// Delete the backups beyond the retention limit, returning how many went.
    fn prune(&self) -> Result<usize, String> {
        let entries = std::fs::read_dir(&self.destination)
            .map_err(|error| format!("cannot list {}: {error}", self.destination.display()))?;

        let mut names = Vec::new();
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                // Only this database's files: the destination is shared, so pruning
                // the whole directory would let one project evict another's backup.
                // Matching rebuilds the name rather than comparing prefixes.
                if belongs_to_database(name, &self.settings.database) {
                    names.push(name.to_string());
                }
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
        std::fs::write(&plan.partial, bytes).expect("dump written");
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
        let (executor, _) = executor_with(vec![Ok(ok(0)), Ok(ok(0)), Ok(ok(0))]);

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
        let (executor, _) = executor_with(vec![Ok(ok(0)), Ok(ok(0)), Ok(ok(0))]);

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

    /// The destination directory is shared by every database, so retention must only
    /// count its own files. Counting all of them lets the project with more history
    /// delete the only backup another project has — which is what happened on the
    /// host: the first run of a newly added project pruned a file it never wrote.
    #[test]
    fn pruning_one_database_leaves_another_untouched() {
        let dir = scratch_dir("shared-destination");

        let moment = |hour: u32| {
            chrono::NaiveDate::from_ymd_opt(2026, 11, 1)
                .and_then(|date| date.and_hms_opt(hour, 0, 0))
                .expect("valid timestamp")
        };

        for hour in [1, 2] {
            for database in ["uno", "dos"] {
                std::fs::write(
                    dir.join(super::super::backup::backup_filename(
                        database,
                        moment(hour),
                    )),
                    b"dump",
                )
                .expect("write");
            }
        }

        let task = BackupTask {
            settings: super::super::backup::BackupSettings::new(
                "host", 5432, "user", "uno", "secret",
            ),
            destination: dir.clone(),
            keep_backups: 1,
            timeout: std::time::Duration::from_secs(10),
            grace: std::time::Duration::from_secs(1),
            verify_timeout: std::time::Duration::from_secs(10),
        };

        let pruned = task.prune().expect("prune works");

        assert_eq!(pruned, 1, "only uno's older dump is over the limit");

        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .expect("list")
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .collect();
        names.sort();

        assert_eq!(
            names.iter().filter(|name| name.contains("-dos-")).count(),
            2,
            "the other database keeps both of its dumps: {names:?}"
        );
        assert_eq!(
            names.iter().filter(|name| name.contains("-uno-")).count(),
            1,
            "this database keeps only the newest: {names:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `uno-2`'s file name starts with `uno`'s prefix, so a plain prefix check makes
    /// the shorter database's retention count — and delete — the longer one's dumps.
    /// The names that exist today do not collide, which is exactly why this has to be
    /// pinned: the bug is latent, not absent.
    #[test]
    fn pruning_a_database_ignores_one_whose_name_extends_it() {
        let dir = scratch_dir("colliding-names");

        let moment = |hour: u32| {
            chrono::NaiveDate::from_ymd_opt(2026, 11, 1)
                .and_then(|date| date.and_hms_opt(hour, 0, 0))
                .expect("valid timestamp")
        };

        for hour in [1, 2] {
            for database in ["uno", "uno-2"] {
                std::fs::write(
                    dir.join(super::super::backup::backup_filename(
                        database,
                        moment(hour),
                    )),
                    b"dump",
                )
                .expect("write");
            }
        }

        let task = BackupTask {
            settings: super::super::backup::BackupSettings::new(
                "host", 5432, "user", "uno", "secret",
            ),
            destination: dir.clone(),
            keep_backups: 1,
            timeout: std::time::Duration::from_secs(10),
            grace: std::time::Duration::from_secs(1),
            verify_timeout: std::time::Duration::from_secs(10),
        };

        let pruned = task.prune().expect("prune works");

        assert_eq!(
            pruned, 1,
            "only uno's older dump is over the limit; uno-2's two are not uno's business"
        );

        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .expect("list")
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .collect();
        names.sort();

        assert_eq!(
            names.iter().filter(|name| name.contains("-uno-2-")).count(),
            2,
            "the database whose name extends the other keeps both dumps: {names:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn the_destination_directory_is_created_before_the_dump() {
        let dir = scratch_dir("mkdir");
        let nested = dir.join("postgres");
        let task = task_in(&nested, 7);
        let (executor, _) = executor_with(vec![Ok(ok(0))]);

        let outcome = task.run(&executor, now()).await;

        assert_eq!(outcome.status, MaintenanceStatus::Failed);
        assert!(nested.is_dir(), "the destination directory was not created");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_failed_data_pass_keeps_the_previous_backup_and_leaves_nothing() {
        let dir = scratch_dir("data-fail");
        let task = task_in(&dir, 1);
        let yesterday = now() - chrono::Duration::days(1);
        let previous = plan_dump(
            &task.settings,
            &dir,
            yesterday,
            task.timeout,
            task.grace,
            task.verify_timeout,
        );
        std::fs::write(&previous.destination, b"previous archive").expect("previous");
        let current = plan_dump(
            &task.settings,
            &dir,
            now(),
            task.timeout,
            task.grace,
            task.verify_timeout,
        );
        std::fs::write(&current.partial, b"new archive").expect("new dump");

        let (executor, _) = executor_with(vec![
            Ok(ok(0)),
            Ok(ok(0)),
            Ok(ok_with_stderr(1, "corrupt data")),
        ]);
        let outcome = task.run(&executor, now()).await;

        assert_eq!(outcome.status, MaintenanceStatus::Failed);
        assert!(
            previous.destination.exists(),
            "a failed verification must not prune the previous backup"
        );
        assert!(
            !current.partial.exists() && !current.restore_temp.exists(),
            "a failed run left its scratch files behind"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
