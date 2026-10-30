use std::time::{Duration, Instant};

use chrono::Local;
use futures::future::join_all;
use tokio::sync::watch;
use tokio::time;
use tracing::{error, info, warn};

use crate::alerts::AlertEvaluator;
use crate::collectors::Collector;
use crate::collectors::CollectorError;
use crate::notifiers::Notifier;
use crate::services::maintenance::{
    Executor, MaintenanceOutcome, MaintenanceScheduler, MaintenanceStatus,
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

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    tick += 1;

                    if tick.is_multiple_of(10) {
                        info!(tick, "Collection cycle");
                    }

                    // A collector that is timing out can hold this tick for longer
                    // than systemd is willing to wait, so the tick itself races the
                    // signal instead of only being checked between ticks.
                    let mut tick_shutdown = shutdown.clone();
                    let mut cancel = tick_shutdown.clone();

                    tokio::select! {
                        _ = self.tick(tick, &mut tick_shutdown) => {}
                        _ = wait_for_shutdown(&mut cancel) => {
                            info!("Shutdown signal received mid-cycle, stopping GuardianRS");
                            return Ok(());
                        }
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

    /// Execute one full cycle: the metric pipeline, then at most one maintenance
    /// task.
    ///
    /// Maintenance runs outside the metric pipeline on purpose. Folding it inside
    /// would mean a tick that collected no metrics also skipped a due backup,
    /// which is the opposite of what a backup schedule promises.
    async fn tick(&mut self, tick: u64, shutdown: &mut watch::Receiver<bool>) {
        self.collect_and_alert().await;
        self.maintain(shutdown, tick).await;
    }

    /// Run at most one due maintenance task and dispatch its outcome.
    ///
    /// At most one, always: a reboot can leave several tasks overdue together and
    /// starting them all puts a `pg_dump` and everything else into one I/O window.
    /// The others are taken by later ticks.
    async fn maintain(&mut self, shutdown: &mut watch::Receiver<bool>, tick: u64) {
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
            None => MaintenanceOutcome {
                task: name.clone(),
                status: MaintenanceStatus::Skipped,
                exit_code: None,
                duration: Duration::ZERO,
                detail: "no runnable implementation for this task kind".to_string(),
            },
        };

        let should_announce = self
            .maintenance
            .record(index, outcome.status, now_wall, now_mono);

        info!(
            task = %outcome.task,
            status = %outcome.status,
            duration_secs = outcome.duration.as_secs(),
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
            if let Err(error) = notifier.send_maintenance(&outcome).await {
                error!(
                    notifier = notifier.name(),
                    error = %error,
                    "Maintenance notification failed"
                );
            }
        }
    }

    /// Collect metrics, evaluate alerts and dispatch them.
    async fn collect_and_alert(&mut self) {
        // === Collect ===
        let mut handles = Vec::with_capacity(self.collectors.len());
        for collector in &self.collectors {
            handles.push(collector.collect_all());
        }

        let results = join_all(handles).await;
        let snapshots: Vec<_> = results
            .into_iter()
            .flat_map(|r| match r {
                Ok(snaps) => snaps,
                Err(e @ CollectorError::Unavailable { .. }) => {
                    warn!(error = %e, "Collector warming up");
                    Vec::new()
                }
                Err(e) => {
                    error!(error = %e, "Collection failed");
                    Vec::new()
                }
            })
            .collect();

        if snapshots.is_empty() {
            warn!("No metrics collected this cycle");
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
    use crate::notifiers::NotifierError;
    use crate::services::maintenance::{
        BackupSettings, BackupTask, DailyTime, ExecutionError, ExecutionOutput, ExecutionRequest,
        ScheduledTask, TaskSchedule,
    };
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

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
    }

    #[async_trait]
    impl Notifier for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }

        async fn send(&self, _event: &crate::alerts::AlertEvent) -> Result<(), NotifierError> {
            Ok(())
        }

        async fn send_maintenance(
            &self,
            outcome: &MaintenanceOutcome,
        ) -> Result<(), NotifierError> {
            self.outcomes.lock().expect("lock").push(outcome.clone());
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
        });

        let (_tx, mut rx) = watch::channel(false);
        orchestrator.maintain(&mut rx, 1).await;

        assert!(
            seen.lock().expect("lock").is_empty(),
            "a task that already ran today stays quiet"
        );
    }
}
