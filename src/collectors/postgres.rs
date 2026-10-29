//! PostgreSQL health collector.
//!
//! Reports how many connectivity probes failed in one cycle. An outage has to
//! become a *metric*, not an error: a collector that returned `Unavailable`
//! when the database is down would be swallowed as "warming up" by the
//! orchestrator, and the alert would never fire. This collector therefore
//! always succeeds and puts the failure count in the value.
//!
//! Several probes are attempted per cycle so a single transient refusal does
//! not raise an alarm, and probing stops at the first success so a healthy
//! database costs exactly one connection.
//!
//! Credentials never reach this file's output. The password lives inside the
//! adapter's client config and is never placed in a snapshot label.

use async_trait::async_trait;
use tracing::{debug, warn};

use crate::collectors::{Collector, CollectorError};
use crate::metrics::{MetricKind, MetricSnapshot};

/// Probes attempted per cycle before the database is considered unreachable.
pub const MAX_PROBES: usize = 3;

/// The single PostgreSQL operation this collector needs.
///
/// Kept as our own trait so the collector never depends on a PostgreSQL driver
/// and the unit tests need no running server.
#[async_trait]
pub trait PostgresApi: Send + Sync {
    /// Run one connectivity probe. `Ok` means the database answered.
    async fn probe(&self) -> Result<(), String>;
}

/// Collects PostgreSQL connectivity as a failure count.
///
/// # Ownership Rules
///
/// This collector only gathers data. It does NOT evaluate alerts, send
/// notifications, or execute remediation.
pub struct PostgresCollector {
    api: Box<dyn PostgresApi>,
    host: String,
    database: String,
}

impl PostgresCollector {
    /// Build a collector from configuration.
    ///
    /// # Errors
    ///
    /// Returns a reason when the password was never resolved, which means the
    /// configuration was not loaded through [`crate::config::load`].
    pub fn from_config(config: &crate::config::PostgresConfig) -> Result<Self, String> {
        let password = config.password().map_err(|e| e.to_string())?;

        let mut pg = tokio_postgres::Config::new();
        pg.host(&config.host)
            .port(config.port)
            .user(&config.user)
            .dbname(&config.database)
            .password(password)
            .connect_timeout(std::time::Duration::from_secs(config.connect_timeout_secs));

        Ok(Self {
            api: Box::new(TokioPostgres { config: pg }),
            host: config.host.clone(),
            database: config.database.clone(),
        })
    }

    /// Build a collector over a custom [`PostgresApi`].
    ///
    /// Exists for tests, which supply a scripted probe and never open a socket.
    pub fn with_api(api: Box<dyn PostgresApi>, host: &str, database: &str) -> Self {
        Self {
            api,
            host: host.to_string(),
            database: database.to_string(),
        }
    }

    /// Probe until one succeeds or the budget is spent, returning the number of
    /// failures.
    async fn count_failed_probes(&self) -> f64 {
        for attempt in 0..MAX_PROBES {
            match self.api.probe().await {
                // Probing stops here: a healthy database costs one connection.
                Ok(()) => return attempt as f64,
                Err(reason) => {
                    debug!(
                        attempt = attempt + 1,
                        probes = MAX_PROBES,
                        reason = %reason,
                        "PostgreSQL probe failed"
                    );
                }
            }
        }

        MAX_PROBES as f64
    }

    /// Build the snapshot.
    fn snapshot(&self, failures: f64) -> MetricSnapshot {
        MetricSnapshot::new(MetricKind::PostgresProbeFailures, failures)
            .with_label("host", self.host.clone())
            .with_label("database", self.database.clone())
            .with_label("probes", MAX_PROBES.to_string())
    }
}

#[async_trait]
impl Collector for PostgresCollector {
    fn name(&self) -> &str {
        "postgres"
    }

    async fn collect(&self) -> Result<MetricSnapshot, CollectorError> {
        let failures = self.count_failed_probes().await;

        if failures >= MAX_PROBES as f64 {
            warn!(
                host = %self.host,
                probes = MAX_PROBES,
                "PostgreSQL did not answer any probe this cycle"
            );
        } else {
            debug!(
                host = %self.host,
                failures = failures,
                "PostgreSQL probe completed"
            );
        }

        Ok(self.snapshot(failures))
    }
}

/// The only place that mentions `tokio_postgres`.
struct TokioPostgres {
    config: tokio_postgres::Config,
}

#[async_trait]
impl PostgresApi for TokioPostgres {
    async fn probe(&self) -> Result<(), String> {
        let (client, connection) = self
            .config
            .connect(tokio_postgres::NoTls)
            .await
            .map_err(|e| format!("postgres connect failed: {e}"))?;

        // The connection future drives the socket; it ends when the client is
        // dropped, and aborting is the belt to that braces.
        let connection_task = tokio::spawn(connection);

        let result = client
            .simple_query("SELECT 1")
            .await
            .map_err(|e| format!("postgres query failed: {e}"));

        drop(client);
        connection_task.abort();

        result.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Records how often the collector probed and replays scripted answers.
    #[derive(Default)]
    struct Script {
        results: Mutex<Vec<Result<(), String>>>,
        calls: AtomicUsize,
    }

    impl Script {
        fn new(results: Vec<Result<(), String>>) -> Self {
            Self {
                results: Mutex::new(results),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    struct ScriptedPostgres {
        script: Arc<Script>,
    }

    #[async_trait]
    impl PostgresApi for ScriptedPostgres {
        async fn probe(&self) -> Result<(), String> {
            self.script.calls.fetch_add(1, Ordering::SeqCst);

            let mut results = self
                .script
                .results
                .lock()
                .map_err(|e| format!("script lock poisoned: {e}"))?;

            if results.is_empty() {
                return Err("script exhausted".to_string());
            }

            results.remove(0)
        }
    }

    fn collector_with(script: &Arc<Script>) -> PostgresCollector {
        PostgresCollector::with_api(
            Box::new(ScriptedPostgres {
                script: Arc::clone(script),
            }),
            "127.0.0.1",
            "bookgraph",
        )
    }

    #[tokio::test]
    async fn healthy_database_reports_zero_failures() {
        let script = Arc::new(Script::new(vec![Ok(())]));
        let collector = collector_with(&script);

        let snapshot = collector.collect().await.expect("collect never fails");

        assert_eq!(snapshot.kind, MetricKind::PostgresProbeFailures);
        assert_eq!(snapshot.value, 0.0);
        assert_eq!(script.calls(), 1, "one successful probe is enough");
    }

    #[tokio::test]
    async fn unreachable_database_reports_every_probe_as_failed() {
        let script = Arc::new(Script::new(vec![Err("down".to_string()); MAX_PROBES]));
        let collector = collector_with(&script);

        let snapshot = collector.collect().await.expect("collect never fails");

        assert_eq!(snapshot.value, MAX_PROBES as f64);
        assert_eq!(script.calls(), MAX_PROBES);
    }

    /// A single refusal followed by an answer must not raise an alarm.
    #[tokio::test]
    async fn transient_failure_is_absorbed_by_the_retry() {
        let script = Arc::new(Script::new(vec![Err("refused".to_string()), Ok(())]));
        let collector = collector_with(&script);

        let snapshot = collector.collect().await.expect("collect never fails");

        assert_eq!(snapshot.value, 1.0);
        assert_eq!(script.calls(), 2);
    }

    /// Probing must stop at the first success instead of always burning the
    /// whole budget on a healthy database.
    #[tokio::test]
    async fn probing_stops_at_the_first_success() {
        let script = Arc::new(Script::new(vec![Ok(()), Ok(()), Ok(())]));
        let collector = collector_with(&script);

        collector.collect().await.expect("collect never fails");

        assert_eq!(script.calls(), 1);
    }

    /// The snapshot may only carry non-secret context.
    #[tokio::test]
    async fn snapshot_labels_never_carry_a_secret() {
        let script = Arc::new(Script::new(vec![Ok(())]));
        let collector = collector_with(&script);

        let snapshot = collector.collect().await.expect("collect never fails");

        let mut keys: Vec<&str> = snapshot.labels.keys().map(String::as_str).collect();
        keys.sort_unstable();

        assert_eq!(keys, vec!["database", "host", "probes"]);
        assert_eq!(
            snapshot.labels.get("host").map(String::as_str),
            Some("127.0.0.1")
        );
        assert_eq!(snapshot.labels.get("probes").map(String::as_str), Some("3"));
    }

    /// An outage must be a metric, never an error: an `Unavailable` result is
    /// logged as "warming up" and the alert would never fire.
    #[tokio::test]
    async fn outage_is_reported_as_a_value_not_an_error() {
        let script = Arc::new(Script::new(vec![Err("no route".to_string()); MAX_PROBES]));
        let collector = collector_with(&script);

        assert!(collector.collect().await.is_ok());
    }
}
