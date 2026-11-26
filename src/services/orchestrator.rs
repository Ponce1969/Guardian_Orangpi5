use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{Local, NaiveDate, Utc};
use futures::future::join_all;
use tokio::sync::watch;
use tokio::time;
use tracing::{error, info, warn};

use crate::alerts::AlertEvaluator;
use crate::collectors::Collector;
use crate::collectors::CollectorError;
use crate::notifiers::Notifier;
use crate::services::digest::{
    evaluate_digest_due, format_alert_series, summarize_tasks, DigestAlertSummary, DigestReport,
    DEFAULT_LATE_GRACE,
};
use crate::services::maintenance::{
    state, DailyTime, Executor, MaintenanceOutcome, MaintenanceScheduler, MaintenanceStatus,
};

/// Everything the orchestrator needs to run.
///
/// Grouped rather than passed positionally: five parameters would make every test
/// spell out all of them, and a helper can now build the whole set and override
/// only the dependency a given test cares about.
pub struct OrchestratorDeps {
    pub collectors: Vec<Box<dyn Collector>>,
    pub alert_engine: Box<dyn AlertEvaluator>,
    pub notifiers: Vec<Box<dyn Notifier>>,
    pub poll_interval: Duration,
    pub maintenance: MaintenanceScheduler,
    pub executor: Box<dyn Executor>,
    /// Where completed runs are recorded. `None` keeps them in memory only, which
    /// is what a test wants and what a host without a writable directory gets.
    pub state_path: Option<PathBuf>,
    /// Alerts are not evaluated during this window after construction.
    pub startup_grace: Duration,
    /// Optional host-local daily digest time (`None` when `digest.enabled: false`).
    pub digest_at: Option<DailyTime>,
}

/// The Orchestrator wires collectors, the alert engine, and notifiers
/// into a single collection -> evaluate -> dispatch loop.
///
/// # Ownership
///
/// The Orchestrator owns all components directly. There is no
/// `Arc<Mutex<>>` needed because the entire loop runs in a single
/// tokio task.
///
/// # Lifecycle
///
/// `run()` consumes `self` and starts the daemon loop. It never
/// returns under normal operation. Use `tokio::select!` or a
/// cancellation token to shut down.
pub struct Orchestrator {
    collectors: Vec<Box<dyn Collector>>,
    alert_engine: Box<dyn AlertEvaluator>,
    notifiers: Vec<Box<dyn Notifier>>,
    poll_interval: Duration,
    maintenance: MaintenanceScheduler,
    executor: Box<dyn Executor>,
    state_path: Option<PathBuf>,
    startup_grace: Duration,
    digest_at: Option<DailyTime>,
    last_digest_date: Option<NaiveDate>,
    collector_failures: BTreeMap<String, u64>,
    started: Instant,
    grace_announced: bool,
}

impl Orchestrator {
    pub fn new(deps: OrchestratorDeps) -> Self {
        Self {
            collectors: deps.collectors,
            alert_engine: deps.alert_engine,
            notifiers: deps.notifiers,
            poll_interval: deps.poll_interval,
            maintenance: deps.maintenance,
            executor: deps.executor,
            state_path: deps.state_path,
            startup_grace: deps.startup_grace,
            digest_at: deps.digest_at,
            last_digest_date: None,
            collector_failures: BTreeMap::new(),
            started: Instant::now(),
            grace_announced: false,
        }
    }

    /// Start the daemon loop. Collects metrics, evaluates alerts,
    /// and dispatches notifications on every tick.
    ///
    /// This method consumes `self` because the loop is intended to
    /// run for the lifetime of the process.
    ///
    /// # Arguments
    ///
    /// The argument is a `watch` receiver rather than a oneshot because shutdown
    /// has to be observable at more than one await point: between ticks and
    /// inside one. A oneshot is consumed by its first observer, so a tick that had
    /// already consumed the signal left the outer loop waiting for a second one
    /// that never came, and systemd eventually killed the process.
    ///
    /// * `shutdown` - Becomes `true` when the daemon must stop.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
        info!(
            interval_secs = self.poll_interval.as_secs(),
            "Starting GuardianRS orchestrator"
        );

        let mut ticker = time::interval(self.poll_interval);
        let mut tick: u64 = 0;

        // Before the first tick, which fires immediately: a task restored here must
        // not be seen as never having run.
        self.restore_state(Local::now().naive_local(), Instant::now())
            .await;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    tick += 1;

                    if tick.is_multiple_of(10) {
                        info!(tick, "Collection cycle");
                    }

                    // Phase-aware shutdown inside `tick` lets slow collectors and
                    // outbound HTTP yield immediately on SIGTERM without ever
                    // interrupting `record` + `persist_state` after a backup finishes.
                    let mut tick_shutdown = shutdown.clone();
                    if self.tick(tick, &mut tick_shutdown).await {
                        info!("Shutdown signal received mid-cycle, stopping GuardianRS");
                        return Ok(());
                    }
                }
                _ = wait_for_shutdown(&mut shutdown) => {
                    info!("Shutdown signal received, stopping GuardianRS");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Execute one full cycle across fault-isolated phases (`I2.3`, `I4.2`).
    ///
    /// Returns `true` if a shutdown signal was observed during the cycle:
    ///
    /// 1. outbound transport flush (always runs first, even when collection
    ///    failed, startup grace deferred evaluation, or no new events fired);
    /// 2. metric collection and alert evaluation;
    /// 3. at most one due maintenance task;
    /// 4. daily human liveness digest when due.
    async fn tick(&mut self, tick: u64, shutdown: &mut watch::Receiver<bool>) -> bool {
        if *shutdown.borrow() {
            return true;
        }

        // Phase 1: Outbound transport flush.
        {
            let mut cancel = shutdown.clone();
            tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut cancel) => return true,
                _ = self.flush_notifications() => {}
            }
        }
        if *shutdown.borrow() {
            return true;
        }

        // Phase 2: Metric collection and alert evaluation.
        {
            let mut cancel = shutdown.clone();
            tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut cancel) => return true,
                _ = self.collect_and_alert(tick) => {}
            }
        }
        if *shutdown.borrow() {
            return true;
        }

        // Phase 3: At most one due maintenance task (`maintain` races `backup.run`
        // internally and completes `record` + `persist_state` uninterrupted).
        self.maintain(shutdown, tick).await;
        if *shutdown.borrow() {
            return true;
        }

        // Phase 4: Daily human liveness digest.
        {
            let mut cancel = shutdown.clone();
            tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut cancel) => return true,
                _ = self.maybe_send_digest() => {}
            }
        }

        *shutdown.borrow()
    }

    async fn maybe_send_digest(&mut self) {
        let Some(at) = self.digest_at else {
            return;
        };

        // Defer catch-up until startup grace finishes so post-reboot database
        // probes have settled before the digest summarizes open alerts (`I4.2`).
        if self.started.elapsed() < self.startup_grace {
            return;
        }

        let now_local = Local::now();

        // Wait until all overdue maintenance tasks have executed (one per tick)
        // before emitting a catch-up digest so completed catch-up runs are not
        // misreported as "pending / did not run" (`I4.2`).
        if self
            .maintenance
            .pick_due(now_local.naive_local(), Instant::now())
            .is_some()
        {
            return;
        }

        let Some(trigger) = evaluate_digest_due(
            &Local,
            at,
            self.last_digest_date,
            &now_local,
            DEFAULT_LATE_GRACE,
        ) else {
            return;
        };

        let open_alerts = self
            .alert_engine
            .active_alerts()
            .into_iter()
            .map(|(key, state)| DigestAlertSummary {
                series: format_alert_series(&key),
                state,
            })
            .collect();

        let collector_failures: Vec<(String, u64)> = self
            .collector_failures
            .iter()
            .map(|(name, count)| (name.clone(), *count))
            .collect();

        let tasks = summarize_tasks(self.maintenance.tasks(), trigger.target_date, at);

        let report = DigestReport {
            generated_at: Utc::now(),
            local_offset: now_local.offset().to_string(),
            target_date: trigger.target_date,
            scheduled_slot: trigger.scheduled_slot,
            late_by: trigger.late_by,
            collector_failures,
            open_alerts,
            tasks,
        };

        for notifier in &self.notifiers {
            if let Err(error) = notifier.send_digest(&report).await {
                error!(
                    notifier = notifier.name(),
                    error = %error,
                    "Daily digest notification failed (spooled if retryable)"
                );
            }
        }

        self.last_digest_date = Some(trigger.target_date);
        self.collector_failures.clear();
        self.persist_state().await;
    }

    async fn flush_notifications(&mut self) {
        for notifier in &self.notifiers {
            if let Err(error) = notifier.flush().await {
                error!(
                    notifier = notifier.name(),
                    error = %error,
                    "Notification outbox flush failed"
                );
            }
        }
    }

    /// Run at most one due maintenance task and dispatch its outcome.
    ///
    /// At most one, always: a reboot can leave several tasks overdue together and
    /// starting them all puts a `pg_dump` and everything else into one I/O window.
    /// The others are taken by later ticks.
    async fn maintain(&mut self, shutdown: &mut watch::Receiver<bool>, tick: u64) {
        // Defer overdue post-reboot backups until startup grace finishes so
        // PostgreSQL containers have finished starting before `pg_dump` runs.
        if self.started.elapsed() < self.startup_grace {
            return;
        }

        let now_wall = Local::now().naive_local();
        let now_mono = Instant::now();

        let Some((index, reason)) = self.maintenance.pick_due(now_wall, now_mono) else {
            return;
        };

        // Cloned so the scheduler borrow ends before the run, which needs `&mut`
        // for recording afterwards.
        let Some((name, backup)) = self
            .maintenance
            .task(index)
            .map(|task| (task.name.clone(), task.backup.clone()))
        else {
            return;
        };

        info!(tick, task = %name, reason = %reason, "Maintenance task is due");

        let outcome = match backup {
            Some(backup) => {
                let run = backup.run(self.executor.as_ref(), now_wall);
                tokio::pin!(run);

                tokio::select! {
                    // Polled first on purpose. `select!` chooses randomly among ready
                    // branches, so a run that finishes on the same poll as the signal
                    // could be discarded: the dump exists, nothing records it, and the
                    // log claims it was cancelled. A finished outcome always wins.
                    biased;
                    outcome = &mut run => outcome,
                    _ = wait_for_shutdown(&mut *shutdown) => {
                        // Dropping `run` fires the executor's group guard, so the
                        // child and everything it forked are killed rather than
                        // left holding a connection and its locks.
                        warn!(
                            task = %name,
                            "Shutdown during a maintenance run; cancelling it"
                        );
                        return;
                    }
                }
            }
            None => {
                // Recorded as skipped so it does not retry every tick, but loud: a
                // task kind with no implementation is a configuration error, and
                // recording it silently hid that for a whole interval.
                warn!(
                    task = %name,
                    "A task kind with no runnable implementation is recorded as skipped"
                );
                MaintenanceOutcome {
                    task: name.clone(),
                    status: MaintenanceStatus::Skipped,
                    exit_code: None,
                    duration: Duration::ZERO,
                    detail: "no runnable implementation for this task kind".to_string(),
                }
            }
        };

        let should_announce = self
            .maintenance
            .record(index, outcome.status, now_wall, now_mono);

        self.persist_state().await;

        info!(
            task = %outcome.task,
            status = %outcome.status,
            duration_secs = outcome.duration.as_secs(),
            detail = %outcome.detail,
            "Maintenance run finished"
        );

        if !should_announce {
            info!(
                task = %outcome.task,
                "Maintenance outcome not announced: the state did not change"
            );
            return;
        }

        for notifier in &self.notifiers {
            let mut cancel = shutdown.clone();
            tokio::select! {
                biased;
                result = notifier.send_maintenance(&outcome) => {
                    if let Err(error) = result {
                        error!(
                            notifier = notifier.name(),
                            error = %error,
                            "Maintenance notification failed"
                        );
                    }
                }
                _ = wait_for_shutdown(&mut cancel) => {
                    return;
                }
            }
        }
    }

    /// Adopt the runs recorded by a previous process without blocking the async worker.
    ///
    /// An unreadable record is a warning and not a failure: refusing to start would
    /// cost every metric to protect a scheduling hint, and the cost of carrying on
    /// is exactly one extra run.
    async fn restore_state(&mut self, now_wall: chrono::NaiveDateTime, now_mono: Instant) {
        let Some(path) = self.state_path.clone() else {
            return;
        };

        let path_for_load = path.clone();
        let load_result = tokio::task::spawn_blocking(move || state::load(&path_for_load)).await;

        match load_result {
            Ok(Ok(recorded)) => {
                self.maintenance.restore(&recorded, now_wall, now_mono);
                self.last_digest_date = recorded.last_digest_date();

                info!(
                    runs = recorded.len(),
                    last_digest = ?self.last_digest_date,
                    path = %path.display(),
                    "Maintenance and digest state restored"
                );
            }
            Ok(Err(reason)) => warn!(
                reason = %reason,
                "Ignoring unreadable maintenance state; every task will run once"
            ),
            Err(error) => warn!(
                %error,
                path = %path.display(),
                "Maintenance state restore task failed; every task will run once"
            ),
        }
    }

    /// Remember what has run without blocking the async worker.
    ///
    /// A failed write is a warning, never an error: the run already happened and
    /// the record is a convenience, while failing here would report a successful
    /// backup as a failed cycle.
    async fn persist_state(&mut self) {
        let Some(path) = self.state_path.clone() else {
            return;
        };

        let snapshot = self
            .maintenance
            .export_state()
            .with_last_digest_date(self.last_digest_date);

        let path_for_save = path.clone();
        let save_result =
            tokio::task::spawn_blocking(move || state::save(&path_for_save, &snapshot)).await;

        match save_result {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => warn!(
                reason = %reason,
                path = %path.display(),
                "Could not record maintenance state"
            ),
            Err(error) => warn!(
                %error,
                path = %path.display(),
                "Maintenance state persistence task failed"
            ),
        }
    }

    /// Collect metrics, evaluate alerts and dispatch them.
    async fn collect_and_alert(&mut self, tick: u64) {
        // === Collect ===
        let mut handles = Vec::with_capacity(self.collectors.len());
        let names: Vec<String> = self
            .collectors
            .iter()
            .map(|c| c.name().to_string())
            .collect();
        for collector in &self.collectors {
            handles.push(collector.collect_all());
        }

        let results = join_all(handles).await;
        let mut snapshots = Vec::new();
        for (name, result) in names.into_iter().zip(results) {
            match result {
                Ok(snaps) => snapshots.extend(snaps),
                Err(e @ CollectorError::Unavailable { .. }) => {
                    warn!(collector = %name, error = %e, "Collector warming up or unavailable");
                    // First-tick baseline establishment in CPU/network is expected,
                    // whereas unavailability on later ticks is recorded for the digest.
                    if tick > 1 {
                        let counter = self.collector_failures.entry(name).or_insert(0);
                        *counter = counter.saturating_add(1);
                    }
                }
                Err(e) => {
                    error!(collector = %name, error = %e, "Collection failed");
                    let counter = self.collector_failures.entry(name).or_insert(0);
                    *counter = counter.saturating_add(1);
                }
            }
        }

        if snapshots.is_empty() {
            warn!("No metrics collected this cycle");
            return;
        }

        // A host that just booted has its databases starting too, and reporting
        // that transition means six messages (three critical, three recovered) for
        // a condition that resolves itself. Evaluation is deferred rather than the
        // dispatch: the tracker is left untouched, so the first cycle after the
        // window sees the transition and a real outage is reported late, not lost.
        if self.started.elapsed() < self.startup_grace {
            if !self.grace_announced {
                self.grace_announced = true;
                info!(
                    grace_secs = self.startup_grace.as_secs(),
                    "Startup grace active: alerts deferred, collection continues"
                );
            }
            return;
        }

        // === Evaluate ===
        let mut events = Vec::new();
        for snapshot in &snapshots {
            events.extend(self.alert_engine.evaluate(snapshot));
        }

        if events.is_empty() {
            return;
        }

        info!(count = events.len(), "Alert events produced");

        // === Dispatch ===
        for event in &events {
            for notifier in &self.notifiers {
                if let Err(e) = notifier.send(event).await {
                    error!(notifier = notifier.name(), error = %e, "Notification failed");
                }
            }
        }
    }
}

/// Resolve as soon as the shutdown flag is set.
///
/// A loop rather than a single `changed()`: the flag may already be set when this
/// is first polled, and `changed()` would then wait for the *next* change and never
/// return. A closed channel counts as shutdown too, so a dropped sender cannot
/// wedge the daemon.
async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }

    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::engine::AlertEngine;
    use crate::alerts::ThresholdRule;
    use crate::collectors::CollectorError;
    use crate::metrics::{MetricKind, MetricSnapshot};
    use crate::notifiers::NotifierError;
    use crate::services::maintenance::{
        BackupSettings, BackupTask, DailyTime, ExecutionError, ExecutionOutput, ExecutionRequest,
        ScheduledTask, TaskSchedule,
    };
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    /// A collector that always reports a value above the critical threshold.
    struct HotCpu;

    #[async_trait]
    impl Collector for HotCpu {
        fn name(&self) -> &str {
            "cpu"
        }

        async fn collect(&self) -> Result<MetricSnapshot, CollectorError> {
            Ok(MetricSnapshot::new(MetricKind::CpuUsage, 99.0))
        }
    }

    fn hot_cpu_engine() -> AlertEngine {
        AlertEngine::new(vec![ThresholdRule {
            metric: MetricKind::CpuUsage,
            warning: 80.0,
            critical: 95.0,
        }])
    }

    /// A collector that outlives any reasonable shutdown deadline.
    ///
    /// It stands in for a probe against a host that stopped answering: the report
    /// from the Orange Pi showed ticks continuing for a minute after SIGTERM while
    /// three probes timed out, which is exactly this shape.
    struct SlowCollector;

    #[async_trait]
    impl Collector for SlowCollector {
        fn name(&self) -> &str {
            "slow"
        }

        async fn collect(
            &self,
        ) -> Result<crate::metrics::MetricSnapshot, crate::collectors::CollectorError> {
            tokio::time::sleep(Duration::from_secs(600)).await;

            panic!("a collection cycle must be cancelled, not awaited to completion")
        }
    }

    /// A failure is not a completed run, so a restart must retry it. Before the state
    /// file existed a restart cleared the record and the next poll ran the task again;
    /// remembering a failure removed the only retry a wedged backup gets until the
    /// next scheduled slot.
    ///
    /// The schedule is an hour-long interval rather than a daily time so the test
    /// does not depend on what time it is when it runs.
    #[tokio::test]
    async fn a_restart_retries_a_task_whose_last_run_failed() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("guardian-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let path = dir.join("state.json");

        let backup = BackupTask::new(BackupSettings::new("host", 5432, "user", "db", "secret"));

        let mut first = Orchestrator::new(OrchestratorDeps {
            collectors: Vec::new(),
            alert_engine: Box::new(AlertEngine::new(Vec::new())),
            notifiers: Vec::new(),
            poll_interval: Duration::from_secs(30),
            maintenance: MaintenanceScheduler::new(vec![ScheduledTask::new(
                "postgres_backup",
                TaskSchedule::Every(Duration::from_secs(3600)),
            )
            .with_backup(backup.clone())]),
            executor: Box::new(Scripted(Arc::new(Mutex::new(vec![failure()])))),
            state_path: Some(path.clone()),
            startup_grace: Duration::ZERO,
            digest_at: None,
        });

        let (_tx, mut rx) = watch::channel(false);
        first.maintain(&mut rx, 1).await;

        assert!(path.exists(), "the state file is still written");

        // A new process: fresh scheduler, same record on disk.
        let recorder = Recorder::default();
        let seen = Arc::clone(&recorder.outcomes);

        let mut second = Orchestrator::new(OrchestratorDeps {
            collectors: Vec::new(),
            alert_engine: Box::new(AlertEngine::new(Vec::new())),
            notifiers: vec![Box::new(recorder)],
            poll_interval: Duration::from_secs(30),
            maintenance: MaintenanceScheduler::new(vec![ScheduledTask::new(
                "postgres_backup",
                TaskSchedule::Every(Duration::from_secs(3600)),
            )
            .with_backup(backup)]),
            executor: Box::new(Scripted(Arc::new(Mutex::new(vec![])))),
            state_path: Some(path.clone()),
            startup_grace: Duration::ZERO,
            digest_at: None,
        });

        second
            .restore_state(Local::now().naive_local(), Instant::now())
            .await;
        second.maintain(&mut rx, 1).await;

        assert!(
            !seen.lock().expect("lock").is_empty(),
            "a failed run is retried after a restart"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The regression: shutdown used to be observed only between ticks, so a tick
    /// blocked inside a slow collector kept the daemon alive until systemd lost
    /// patience and sent SIGKILL — which is not a graceful stop at all, and can
    /// leave a half-written dump behind.
    #[tokio::test]
    async fn shutdown_interrupts_a_collection_cycle_in_progress() {
        let orchestrator = Orchestrator::new(OrchestratorDeps {
            collectors: vec![Box::new(SlowCollector)],
            alert_engine: Box::new(AlertEngine::new(Vec::new())),
            notifiers: Vec::new(),
            poll_interval: Duration::from_millis(10),
            maintenance: MaintenanceScheduler::default(),
            executor: Box::new(Scripted(Arc::new(Mutex::new(Vec::new())))),
            state_path: None,
            startup_grace: Duration::ZERO,
            digest_at: None,
        });

        let (tx, rx) = watch::channel(false);

        let daemon = tokio::spawn(async move { orchestrator.run(rx).await });

        // Let the first tick reach the collector before signalling.
        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.send(true).expect("the daemon is listening");

        tokio::time::timeout(Duration::from_secs(2), daemon)
            .await
            .expect("run must return when the signal fires, not when the collector does")
            .expect("the daemon must not panic")
            .expect("a clean shutdown is not an error");
    }

    /// Captures the outcomes the orchestrator dispatches.
    #[derive(Default)]
    struct Recorder {
        outcomes: Arc<Mutex<Vec<MaintenanceOutcome>>>,
        digests: Arc<Mutex<Vec<DigestReport>>>,
        alerts: Arc<Mutex<usize>>,
        flushes: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl Notifier for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }

        async fn send(&self, _event: &crate::alerts::AlertEvent) -> Result<(), NotifierError> {
            *self.alerts.lock().expect("lock") += 1;
            Ok(())
        }

        async fn send_maintenance(
            &self,
            outcome: &MaintenanceOutcome,
        ) -> Result<(), NotifierError> {
            self.outcomes.lock().expect("lock").push(outcome.clone());
            Ok(())
        }

        async fn send_digest(&self, digest: &DigestReport) -> Result<(), NotifierError> {
            self.digests.lock().expect("lock").push(digest.clone());
            Ok(())
        }

        async fn flush(&self) -> Result<(), NotifierError> {
            *self.flushes.lock().expect("lock") += 1;
            Ok(())
        }
    }

    /// Replays one scripted executor result per call.
    struct Scripted(Arc<Mutex<Vec<ExecutionOutput>>>);

    #[async_trait]
    impl Executor for Scripted {
        async fn run(
            &self,
            _request: &ExecutionRequest,
        ) -> Result<ExecutionOutput, ExecutionError> {
            let mut script = self.0.lock().expect("lock");
            if script.is_empty() {
                return Err(ExecutionError::Collect {
                    reason: "script exhausted".to_string(),
                });
            }
            Ok(script.remove(0))
        }
    }

    fn failure() -> ExecutionOutput {
        ExecutionOutput {
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "connection refused".to_string(),
            timed_out: false,
        }
    }

    /// A task that is always due, so the test needs no clock arithmetic.
    fn always_due(recorder: Recorder) -> (Orchestrator, Arc<Mutex<Vec<MaintenanceOutcome>>>) {
        let seen = Arc::clone(&recorder.outcomes);

        let backup = BackupTask::new(BackupSettings::new("host", 5432, "user", "db", "secret"));

        let orchestrator = Orchestrator::new(OrchestratorDeps {
            collectors: Vec::new(),
            alert_engine: Box::new(AlertEngine::new(Vec::new())),
            notifiers: vec![Box::new(recorder)],
            poll_interval: Duration::from_secs(30),
            maintenance: MaintenanceScheduler::new(vec![ScheduledTask::new(
                "postgres_backup",
                TaskSchedule::Every(Duration::ZERO),
            )
            .with_backup(backup)]),
            executor: Box::new(Scripted(Arc::new(Mutex::new(vec![failure(), failure()])))),
            state_path: None,
            startup_grace: Duration::ZERO,
            digest_at: None,
        });

        (orchestrator, seen)
    }

    #[tokio::test]
    async fn a_due_task_runs_and_its_outcome_reaches_the_notifiers() {
        let (mut orchestrator, seen) = always_due(Recorder::default());
        let (_tx, mut rx) = watch::channel(false);

        orchestrator.maintain(&mut rx, 1).await;

        let outcomes = seen.lock().expect("lock");
        assert_eq!(outcomes.len(), 1, "the outcome is dispatched once");
        assert_eq!(outcomes[0].task, "postgres_backup");
        assert_eq!(outcomes[0].status, MaintenanceStatus::Failed);
    }

    /// The asymmetry, end to end and through the real wiring: a failure that
    /// persists is announced every time, because a backup that stayed broken must
    /// not go quiet.
    #[tokio::test]
    async fn a_persistent_failure_is_dispatched_on_every_run() {
        let (mut orchestrator, seen) = always_due(Recorder::default());
        let (_tx, mut rx) = watch::channel(false);

        orchestrator.maintain(&mut rx, 1).await;
        orchestrator.maintain(&mut rx, 2).await;

        assert_eq!(
            seen.lock().expect("lock").len(),
            2,
            "a repeated failure is not silenced"
        );
    }

    #[tokio::test]
    async fn nothing_is_dispatched_before_a_task_is_due() {
        let recorder = Recorder::default();
        let seen = Arc::clone(&recorder.outcomes);

        let daily = DailyTime::new(3, 30).expect("valid time");
        let backup = BackupTask::new(BackupSettings::new("host", 5432, "user", "db", "secret"));

        let mut scheduler = MaintenanceScheduler::new(vec![ScheduledTask::new(
            "postgres_backup",
            TaskSchedule::DailyAt(daily),
        )
        .with_backup(backup)]);

        let now = Local::now().naive_local();
        let mono = Instant::now();
        scheduler.record(0, MaintenanceStatus::Success, now, mono);

        let mut orchestrator = Orchestrator::new(OrchestratorDeps {
            collectors: Vec::new(),
            alert_engine: Box::new(AlertEngine::new(Vec::new())),
            notifiers: vec![Box::new(recorder)],
            poll_interval: Duration::from_secs(30),
            maintenance: scheduler,
            executor: Box::new(Scripted(Arc::new(Mutex::new(Vec::new())))),
            state_path: None,
            startup_grace: Duration::ZERO,
            digest_at: None,
        });

        let (_tx, mut rx) = watch::channel(false);
        orchestrator.maintain(&mut rx, 1).await;

        assert!(
            seen.lock().expect("lock").is_empty(),
            "a task that already ran today stays quiet"
        );
    }

    /// A host that just booted has its databases starting too. Reporting that
    /// transition sends six messages (three critical, three recovered) for a
    /// condition that resolves itself, which is how a channel gets ignored.
    #[tokio::test]
    async fn alerts_are_not_dispatched_during_the_startup_grace() {
        let recorder = Recorder::default();
        let alerts = Arc::clone(&recorder.alerts);

        let mut orchestrator = Orchestrator::new(OrchestratorDeps {
            collectors: vec![Box::new(HotCpu)],
            alert_engine: Box::new(hot_cpu_engine()),
            notifiers: vec![Box::new(recorder)],
            poll_interval: Duration::from_secs(30),
            maintenance: MaintenanceScheduler::default(),
            executor: Box::new(Scripted(Arc::new(Mutex::new(Vec::new())))),
            state_path: None,
            startup_grace: Duration::from_secs(3600),
            digest_at: None,
        });

        orchestrator.collect_and_alert(1).await;

        assert_eq!(
            *alerts.lock().expect("lock"),
            0,
            "nothing is dispatched inside the grace window"
        );
    }

    /// The deferral must not swallow the alert: with no window, the same snapshot
    /// is reported, which is what the first cycle after the window sees. This is
    /// why the grace defers evaluation instead of filtering the dispatch.
    #[tokio::test]
    async fn the_same_alert_is_dispatched_once_the_grace_is_over() {
        let recorder = Recorder::default();
        let alerts = Arc::clone(&recorder.alerts);

        let mut orchestrator = Orchestrator::new(OrchestratorDeps {
            collectors: vec![Box::new(HotCpu)],
            alert_engine: Box::new(hot_cpu_engine()),
            notifiers: vec![Box::new(recorder)],
            poll_interval: Duration::from_secs(30),
            maintenance: MaintenanceScheduler::default(),
            executor: Box::new(Scripted(Arc::new(Mutex::new(Vec::new())))),
            state_path: None,
            startup_grace: Duration::ZERO,
            digest_at: None,
        });

        orchestrator.collect_and_alert(1).await;

        assert_eq!(
            *alerts.lock().expect("lock"),
            1,
            "outside the window the transition is reported"
        );
    }

    struct BrokenCollector;

    #[async_trait]
    impl Collector for BrokenCollector {
        fn name(&self) -> &str {
            "broken"
        }

        async fn collect(&self) -> Result<MetricSnapshot, CollectorError> {
            Err(CollectorError::ReadFailed {
                metric: "cpu_usage".to_string(),
                source: std::io::Error::other("proc unreadable"),
            })
        }
    }

    #[tokio::test]
    async fn flush_runs_even_when_collectors_fail_or_emit_no_events() {
        let recorder = Recorder::default();
        let flushes = Arc::clone(&recorder.flushes);
        let alerts = Arc::clone(&recorder.alerts);

        let mut orchestrator = Orchestrator::new(OrchestratorDeps {
            collectors: vec![Box::new(HotCpu)],
            alert_engine: Box::new(hot_cpu_engine()),
            notifiers: vec![Box::new(recorder)],
            poll_interval: Duration::from_secs(30),
            maintenance: MaintenanceScheduler::default(),
            executor: Box::new(Scripted(Arc::new(Mutex::new(Vec::new())))),
            state_path: None,
            startup_grace: Duration::from_secs(3600),
            digest_at: None,
        });

        let (_tx, mut rx) = watch::channel(false);
        // Tick 1: startup_grace suppresses alert evaluation, yet flush MUST run.
        orchestrator.tick(1, &mut rx).await;
        assert_eq!(*flushes.lock().expect("lock"), 1);
        assert_eq!(*alerts.lock().expect("lock"), 0);

        // Tick 2: end startup grace with BrokenCollector (`snapshots.is_empty()`), yet flush MUST run.
        orchestrator.startup_grace = Duration::ZERO;
        orchestrator.collectors = vec![Box::new(BrokenCollector)];
        orchestrator.tick(2, &mut rx).await;
        assert_eq!(*flushes.lock().expect("lock"), 2);

        // Replace with a steady-state collector: Tick 3 fires Critical, Tick 4 emits 0 events.
        orchestrator.collectors = vec![Box::new(HotCpu)];
        orchestrator.tick(3, &mut rx).await;
        orchestrator.tick(4, &mut rx).await;
        assert_eq!(
            *flushes.lock().expect("lock"),
            4,
            "flush must execute as its own phase on every tick"
        );
        assert_eq!(*alerts.lock().expect("lock"), 1);
    }

    /// `DailyAt` is what production runs; the interval form had only ever been
    /// exercised by the scheduler's own unit tests. This drives it end to end on a
    /// real monotonic clock: due, run, recorded, and due again once it elapses.
    #[tokio::test]
    async fn an_interval_task_becomes_due_again_once_the_interval_elapses() {
        let recorder = Recorder::default();
        let seen = Arc::clone(&recorder.outcomes);

        let backup = BackupTask::new(BackupSettings::new("host", 5432, "user", "db", "secret"));

        let mut orchestrator = Orchestrator::new(OrchestratorDeps {
            collectors: Vec::new(),
            alert_engine: Box::new(AlertEngine::new(Vec::new())),
            notifiers: vec![Box::new(recorder)],
            poll_interval: Duration::from_secs(30),
            maintenance: MaintenanceScheduler::new(vec![ScheduledTask::new(
                "por_intervalo",
                TaskSchedule::Every(Duration::from_millis(30)),
            )
            .with_backup(backup)]),
            executor: Box::new(Scripted(Arc::new(Mutex::new(vec![failure(), failure()])))),
            state_path: None,
            startup_grace: Duration::ZERO,
            digest_at: None,
        });

        let (_tx, mut rx) = watch::channel(false);

        orchestrator.maintain(&mut rx, 1).await;
        assert_eq!(
            seen.lock().expect("lock").len(),
            1,
            "the first cycle runs it"
        );

        orchestrator.maintain(&mut rx, 2).await;
        assert_eq!(
            seen.lock().expect("lock").len(),
            1,
            "the interval has not elapsed, so it is not due"
        );

        tokio::time::sleep(Duration::from_millis(40)).await;

        orchestrator.maintain(&mut rx, 3).await;
        assert_eq!(
            seen.lock().expect("lock").len(),
            2,
            "past the interval it is due again"
        );
    }

    /// `select!` chooses randomly among ready branches. If a run finishes on the same
    /// poll as the shutdown signal, the finished backup could be discarded: the file
    /// exists, nothing records it, and the log says it was cancelled.
    #[tokio::test]
    async fn a_run_that_finishes_as_shutdown_arrives_is_still_recorded() {
        struct SimultaneousFinishExecutor {
            arrived: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
            release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        }

        #[async_trait]
        impl Executor for SimultaneousFinishExecutor {
            async fn run(
                &self,
                _request: &ExecutionRequest,
            ) -> Result<ExecutionOutput, ExecutionError> {
                let arrived = self.arrived.lock().expect("lock").take();
                if let Some(tx) = arrived {
                    let _ = tx.send(());
                }
                let release = self.release.lock().expect("lock").take();
                if let Some(rx) = release {
                    let _ = rx.await;
                }
                Err(ExecutionError::Spawn {
                    program: "pg_dump".to_string(),
                    reason: "missing".to_string(),
                })
            }
        }

        // Without the ordered poll this pass rate would be 2^-20; with it, every run wins.
        for attempt in 0..20 {
            let dir = std::env::temp_dir().join(format!(
                "guardian-shutdown-race-{}-{attempt}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("scratch directory");

            let mut backup =
                BackupTask::new(BackupSettings::new("host", 5432, "user", "db", "secret"));
            backup.destination = dir.clone();

            let recorder = Recorder::default();
            let seen = Arc::clone(&recorder.outcomes);

            let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let (shutdown_tx, mut rx) = watch::channel(false);

            let mut orchestrator = Orchestrator::new(OrchestratorDeps {
                collectors: Vec::new(),
                alert_engine: Box::new(AlertEngine::new(Vec::new())),
                notifiers: vec![Box::new(recorder)],
                poll_interval: Duration::from_secs(30),
                maintenance: MaintenanceScheduler::new(vec![ScheduledTask::new(
                    "postgres_backup",
                    TaskSchedule::Every(Duration::from_secs(3600)),
                )
                .with_backup(backup)]),
                executor: Box::new(SimultaneousFinishExecutor {
                    arrived: Mutex::new(Some(arrived_tx)),
                    release: Mutex::new(Some(release_rx)),
                }),
                state_path: None,
                startup_grace: Duration::ZERO,
                digest_at: None,
            });

            // Once `BackupTask::run` reaches `executor.run`, release both the finished
            // outcome and the shutdown signal in the same turn so both `select!`
            // branches are `Poll::Ready` on the next poll.
            tokio::spawn(async move {
                let _ = arrived_rx.await;
                let _ = release_tx.send(());
                let _ = shutdown_tx.send(true);
            });

            orchestrator.maintain(&mut rx, 1).await;

            assert!(
                !seen.lock().expect("lock").is_empty(),
                "a completed run was discarded when the shutdown signal arrived"
            );

            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[tokio::test]
    async fn maintenance_waits_out_startup_grace() {
        let recorder = Recorder::default();
        let seen = Arc::clone(&recorder.outcomes);

        let backup = BackupTask::new(BackupSettings::new("host", 5432, "user", "db", "secret"));

        let mut orchestrator = Orchestrator::new(OrchestratorDeps {
            collectors: Vec::new(),
            alert_engine: Box::new(AlertEngine::new(Vec::new())),
            notifiers: vec![Box::new(recorder)],
            poll_interval: Duration::from_secs(30),
            maintenance: MaintenanceScheduler::new(vec![ScheduledTask::new(
                "auditor_nightly",
                TaskSchedule::Every(Duration::ZERO),
            )
            .with_backup(backup)]),
            executor: Box::new(Scripted(Arc::new(Mutex::new(vec![failure()])))),
            state_path: None,
            startup_grace: Duration::from_secs(3600),
            digest_at: None,
        });

        let (_tx, mut rx) = watch::channel(false);
        orchestrator.maintain(&mut rx, 1).await;
        assert!(
            seen.lock().expect("lock").is_empty(),
            "maintenance must defer overdue tasks during startup_grace"
        );

        // Once startup_grace elapses, the overdue task executes immediately.
        orchestrator.startup_grace = Duration::ZERO;
        orchestrator.maintain(&mut rx, 2).await;
        assert_eq!(seen.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn digest_waits_until_overdue_maintenance_tasks_finish() {
        let recorder = Recorder::default();
        let digests = Arc::clone(&recorder.digests);
        let midnight = DailyTime::new(0, 0).expect("00:00");

        let mut orchestrator = Orchestrator::new(OrchestratorDeps {
            collectors: Vec::new(),
            alert_engine: Box::new(AlertEngine::new(Vec::new())),
            notifiers: vec![Box::new(recorder)],
            poll_interval: Duration::from_secs(30),
            maintenance: MaintenanceScheduler::new(vec![
                ScheduledTask::new("first_nightly", TaskSchedule::DailyAt(midnight)),
                ScheduledTask::new("second_nightly", TaskSchedule::DailyAt(midnight)),
            ]),
            executor: Box::new(Scripted(Arc::new(Mutex::new(Vec::new())))),
            state_path: None,
            startup_grace: Duration::ZERO,
            digest_at: Some(midnight),
        });

        let (_tx, mut rx) = watch::channel(false);

        // Tick 1 runs `first_nightly`, while `second_nightly` is still overdue -> digest waits!
        orchestrator.tick(1, &mut rx).await;
        assert!(
            digests.lock().expect("lock").is_empty(),
            "digest must wait while second_nightly is still overdue"
        );

        // Tick 2 runs `second_nightly`; with zero overdue tasks left, digest fires on Tick 2!
        orchestrator.tick(2, &mut rx).await;
        assert_eq!(
            digests.lock().expect("lock").len(),
            1,
            "digest fires once all overdue maintenance tasks have completed"
        );
    }
}
