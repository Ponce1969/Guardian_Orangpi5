use std::time::Duration;

use futures::future::join_all;
use tokio::sync::oneshot;
use tokio::time;
use tracing::{error, info, warn};

use crate::alerts::AlertEvaluator;
use crate::collectors::Collector;
use crate::collectors::CollectorError;
use crate::notifiers::Notifier;

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
}

impl Orchestrator {
    pub fn new(
        collectors: Vec<Box<dyn Collector>>,
        alert_engine: Box<dyn AlertEvaluator>,
        notifiers: Vec<Box<dyn Notifier>>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            collectors,
            alert_engine,
            notifiers,
            poll_interval,
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
    /// * `shutdown_rx` - Oneshot receiver that triggers graceful shutdown
    pub async fn run(mut self, mut shutdown_rx: oneshot::Receiver<()>) -> anyhow::Result<()> {
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

                    self.tick(tick).await;
                }
                _ = &mut shutdown_rx => {
                    info!("Shutdown signal received, stopping GuardianRS");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Execute a single collection -> evaluate -> dispatch cycle.
    async fn tick(&mut self, _tick: u64) {
        // === Collect ===
        let mut handles = Vec::with_capacity(self.collectors.len());
        for collector in &self.collectors {
            handles.push(collector.collect());
        }

        let results = join_all(handles).await;
        let snapshots: Vec<_> = results
            .into_iter()
            .filter_map(|r| match r {
                Ok(snap) => Some(snap),
                Err(e @ CollectorError::Unavailable { .. }) => {
                    warn!(error = %e, "Collector warming up");
                    None
                }
                Err(e) => {
                    error!(error = %e, "Collection failed");
                    None
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
