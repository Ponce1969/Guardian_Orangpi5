//! Docker collector.
//!
//! Reports two SPEC measurements from one socket connection: how many
//! containers are unhealthy, and how much disk Docker is holding.
//!
//! The collector never names `bollard` outside the adapter at the bottom of
//! this file. Everything above it talks to [`DockerApi`], which is our own
//! contract: unit tests inject a fake and therefore need no running daemon, and
//! a future Docker client swap touches only the adapter.
//!
//! Degradation is deliberate. A host without Docker — or with the daemon
//! stopped — must not turn into a monitor that reports nothing else, so a
//! failed connection becomes a typed `Unavailable` error that the orchestrator
//! logs and retries on the next cycle. A daemon that answers the container list
//! but not the disk query still reports container health.

use async_trait::async_trait;
use tracing::{debug, warn};

use crate::collectors::{Collector, CollectorError};
use crate::metrics::{MetricKind, MetricSnapshot};

/// The state of one container, reduced to what the guardian reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    /// Up and, when a healthcheck exists, reporting healthy.
    Running,
    /// Up but failing its healthcheck.
    Unhealthy,
    /// Stuck in a restart loop, which no operator intends.
    Restarting,
    /// Not running. A container that finished its work lands here and is
    /// normal: one-shot jobs are supposed to exit.
    Stopped,
    /// A state this collector does not model.
    Other,
}

/// One container, independent of any Docker client type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub name: String,
    pub state: ContainerState,
}

/// Docker disk usage in gigabytes, split by category.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DockerDiskUsage {
    pub images_gb: f64,
    pub containers_gb: f64,
    pub volumes_gb: f64,
    pub build_cache_gb: f64,
}

impl DockerDiskUsage {
    /// Total gigabytes Docker holds across every category.
    pub fn total_gb(&self) -> f64 {
        self.images_gb + self.containers_gb + self.volumes_gb + self.build_cache_gb
    }
}

/// The Docker operations this collector needs.
///
/// Kept as our own trait so the collector never depends on a specific Docker
/// client. `Err` carries a human-readable reason, because the caller only ever
/// logs it.
#[async_trait]
pub trait DockerApi: Send + Sync {
    /// Every container known to the daemon, running or not.
    async fn containers(&self) -> Result<Vec<Container>, String>;

    /// Disk usage per category.
    async fn disk_usage(&self) -> Result<DockerDiskUsage, String>;
}

/// Collects Docker container health and disk usage.
///
/// # Ownership Rules
///
/// This collector only gathers data. It does NOT evaluate alerts, send
/// notifications, or execute remediation. It never starts, stops or prunes a
/// container.
pub struct DockerCollector {
    api: Box<dyn DockerApi>,
}

impl DockerCollector {
    /// Create a collector backed by the local Docker socket.
    pub fn new() -> Self {
        Self::with_api(Box::new(BollardDocker))
    }

    /// Create a collector over a custom [`DockerApi`].
    ///
    /// The constructor exists for tests, which supply a fake so the suite never
    /// needs a running daemon.
    pub fn with_api(api: Box<dyn DockerApi>) -> Self {
        Self { api }
    }

    /// Classify a container from the daemon's lifecycle state and status text.
    ///
    /// The list endpoint exposes health only inside the human-readable `status`
    /// string, which reads `Up 2 hours (healthy)` or `Up 5 minutes (unhealthy)`.
    fn classify(state: Option<&str>, status: Option<&str>) -> ContainerState {
        // The list endpoint publishes health only inside the human status text,
        // which reads `Up 2 hours (healthy)` or `Up 5 minutes (unhealthy)`.
        // A container still starting its first healthcheck is not unhealthy.
        if let Some(status) = status {
            if status.contains("(unhealthy)") {
                return ContainerState::Unhealthy;
            }
        }

        match state {
            Some("running") => ContainerState::Running,
            Some("restarting") => ContainerState::Restarting,
            Some("exited") | Some("dead") => ContainerState::Stopped,
            _ => ContainerState::Other,
        }
    }

    /// Whether a container's state is something the operator must act on.
    ///
    /// `Stopped` is deliberately excluded: a container that exited after
    /// finishing its job is normal, and alerting on it would fire every time a
    /// backup or migration completes.
    fn is_unhealthy(state: ContainerState) -> bool {
        matches!(
            state,
            ContainerState::Unhealthy | ContainerState::Restarting
        )
    }

    /// Number of containers the operator must act on.
    fn count_unhealthy(containers: &[Container]) -> usize {
        containers
            .iter()
            .filter(|container| Self::is_unhealthy(container.state))
            .count()
    }

    /// Number of containers currently running.
    fn count_running(containers: &[Container]) -> usize {
        containers
            .iter()
            .filter(|container| container.state == ContainerState::Running)
            .count()
    }

    /// Build the container-health snapshot.
    fn containers_snapshot(containers: &[Container]) -> MetricSnapshot {
        let unhealthy = Self::count_unhealthy(containers);

        MetricSnapshot::new(MetricKind::DockerUnhealthyContainers, unhealthy as f64)
            .with_label("total", containers.len().to_string())
            .with_label("running", Self::count_running(containers).to_string())
            .with_label("unhealthy", unhealthy.to_string())
            .with_label("source", "docker_api")
    }

    /// Build the disk-usage snapshot, with each category kept as a label.
    fn disk_snapshot(usage: &DockerDiskUsage) -> MetricSnapshot {
        MetricSnapshot::new(MetricKind::DockerDiskUsed, usage.total_gb())
            .with_label("images_gb", format!("{:.2}", usage.images_gb))
            .with_label("containers_gb", format!("{:.2}", usage.containers_gb))
            .with_label("volumes_gb", format!("{:.2}", usage.volumes_gb))
            .with_label("build_cache_gb", format!("{:.2}", usage.build_cache_gb))
            .with_label("source", "docker_api")
    }
}

impl Default for DockerCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Collector for DockerCollector {
    fn name(&self) -> &str {
        "docker"
    }

    async fn collect(&self) -> Result<MetricSnapshot, CollectorError> {
        self.collect_all()
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| CollectorError::Unavailable {
                name: "docker".to_string(),
                reason: "docker produced no metrics this cycle".to_string(),
            })
    }

    /// Report container health and disk usage from a single connection.
    async fn collect_all(&self) -> Result<Vec<MetricSnapshot>, CollectorError> {
        let containers =
            self.api
                .containers()
                .await
                .map_err(|reason| CollectorError::Unavailable {
                    name: "docker".to_string(),
                    reason,
                })?;

        let mut snapshots = vec![Self::containers_snapshot(&containers)];

        // Disk usage is secondary: a daemon that answers the container list but
        // not the disk query still leaves the operator with the health signal.
        match self.api.disk_usage().await {
            Ok(usage) => snapshots.push(Self::disk_snapshot(&usage)),
            Err(reason) => {
                warn!(reason = %reason, "Docker disk usage unavailable this cycle");
            }
        }

        debug!(
            containers = containers.len(),
            unhealthy = Self::count_unhealthy(&containers),
            metrics = snapshots.len(),
            "Docker metrics collected"
        );

        Ok(snapshots)
    }
}

/// The only place that mentions `bollard`.
///
/// A client is built per call rather than cached: `connect_with_unix_defaults`
/// only assembles a transport, the socket is opened per request, and a fresh
/// client means a Docker daemon restarted between cycles is picked up without
/// restarting the guardian.
struct BollardDocker;

impl BollardDocker {
    fn client() -> Result<bollard::Docker, String> {
        // `connect_with_local_defaults` resolves the platform's local transport:
        // the unix socket on the Linux targets and the named pipe on Windows.
        // The socket-flavoured helper does not exist outside unix, so this is
        // both the correct choice for deploy and the one that keeps a dev
        // machine able to compile and exercise the adapter.
        bollard::Docker::connect_with_local_defaults()
            .map_err(|e| format!("docker daemon unavailable: {e}"))
    }

    fn to_container(summary: bollard::models::ContainerSummary) -> Container {
        let name = summary
            .names
            .as_ref()
            .and_then(|names| names.first())
            .map(|name| name.trim_start_matches('/').to_string())
            .unwrap_or_else(|| "<unnamed>".to_string());

        let state = summary.state.map(|state| state.to_string());

        Container {
            name,
            state: DockerCollector::classify(state.as_deref(), summary.status.as_deref()),
        }
    }

    const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

    fn to_gb(size: Option<i64>) -> f64 {
        size.unwrap_or(0).max(0) as f64 / Self::BYTES_PER_GB
    }
}

#[async_trait]
impl DockerApi for BollardDocker {
    async fn containers(&self) -> Result<Vec<Container>, String> {
        let client = Self::client()?;

        // `all: true` is required: an unhealthy container that already exited
        // is exactly the case the operator needs to see.
        let options = bollard::query_parameters::ListContainersOptions {
            all: true,
            ..Default::default()
        };

        let summaries = client
            .list_containers(Some(options))
            .await
            .map_err(|e| format!("docker list_containers failed: {e}"))?;

        Ok(summaries.into_iter().map(Self::to_container).collect())
    }

    async fn disk_usage(&self) -> Result<DockerDiskUsage, String> {
        let client = Self::client()?;

        let response = client
            .df(None)
            .await
            .map_err(|e| format!("docker df failed: {e}"))?;

        Ok(DockerDiskUsage {
            images_gb: Self::to_gb(response.image_usage.and_then(|usage| usage.total_size)),
            containers_gb: Self::to_gb(response.container_usage.and_then(|usage| usage.total_size)),
            volumes_gb: Self::to_gb(response.volume_usage.and_then(|usage| usage.total_size)),
            build_cache_gb: Self::to_gb(
                response
                    .build_cache_usage
                    .and_then(|usage| usage.total_size),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A [`DockerApi`] that answers from fixed data instead of a socket.
    struct FakeDocker {
        containers: Result<Vec<Container>, String>,
        disk: Result<DockerDiskUsage, String>,
    }

    impl FakeDocker {
        fn ok(containers: Vec<Container>, disk: DockerDiskUsage) -> Self {
            Self {
                containers: Ok(containers),
                disk: Ok(disk),
            }
        }

        fn unreachable(reason: &str) -> Self {
            Self {
                containers: Err(reason.to_string()),
                disk: Err(reason.to_string()),
            }
        }
    }

    #[async_trait]
    impl DockerApi for FakeDocker {
        async fn containers(&self) -> Result<Vec<Container>, String> {
            self.containers.clone()
        }

        async fn disk_usage(&self) -> Result<DockerDiskUsage, String> {
            self.disk.clone()
        }
    }

    fn container(name: &str, state: ContainerState) -> Container {
        Container {
            name: name.to_string(),
            state,
        }
    }

    #[test]
    fn classify_running_from_up_status() {
        assert_eq!(
            DockerCollector::classify(Some("running"), Some("Up 2 hours")),
            ContainerState::Running
        );
    }

    #[test]
    fn classify_unhealthy_from_status_suffix() {
        // The list endpoint only exposes health inside the status text.
        assert_eq!(
            DockerCollector::classify(Some("running"), Some("Up 2 hours (unhealthy)")),
            ContainerState::Unhealthy
        );
    }

    #[test]
    fn classify_restarting_from_state() {
        assert_eq!(
            DockerCollector::classify(Some("restarting"), Some("Restarting (1) 5 seconds ago")),
            ContainerState::Restarting
        );
    }

    #[test]
    fn classify_exited_is_stopped() {
        assert_eq!(
            DockerCollector::classify(Some("exited"), Some("Exited (0) 3 hours ago")),
            ContainerState::Stopped
        );
    }

    #[test]
    fn restarting_counts_as_unhealthy() {
        assert!(DockerCollector::is_unhealthy(ContainerState::Restarting));
        assert!(DockerCollector::is_unhealthy(ContainerState::Unhealthy));
        assert!(!DockerCollector::is_unhealthy(ContainerState::Running));
    }

    /// A one-shot job that finished is not an incident. Counting it would alert
    /// every time a backup or migration completes.
    #[test]
    fn exited_container_is_not_unhealthy() {
        assert!(!DockerCollector::is_unhealthy(ContainerState::Stopped));
    }

    #[test]
    fn count_unhealthy_ignores_running_and_stopped() {
        let containers = vec![
            container("web", ContainerState::Running),
            container("db", ContainerState::Unhealthy),
            container("backup", ContainerState::Stopped),
            container("worker", ContainerState::Restarting),
        ];

        assert_eq!(DockerCollector::count_unhealthy(&containers), 2);
    }

    #[test]
    fn count_running_only_counts_running() {
        let containers = vec![
            container("web", ContainerState::Running),
            container("db", ContainerState::Running),
            container("sick", ContainerState::Unhealthy),
            container("backup", ContainerState::Stopped),
        ];

        assert_eq!(DockerCollector::count_running(&containers), 2);
    }

    #[test]
    fn containers_snapshot_carries_counts_as_labels() {
        let containers = vec![
            container("web", ContainerState::Running),
            container("db", ContainerState::Unhealthy),
            container("backup", ContainerState::Stopped),
        ];

        let snapshot = DockerCollector::containers_snapshot(&containers);

        assert_eq!(snapshot.kind, MetricKind::DockerUnhealthyContainers);
        assert_eq!(snapshot.value, 1.0);
        assert_eq!(snapshot.labels.get("total").map(String::as_str), Some("3"));
        assert_eq!(
            snapshot.labels.get("running").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            snapshot.labels.get("unhealthy").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn disk_usage_total_sums_every_category() {
        let usage = DockerDiskUsage {
            images_gb: 2.5,
            containers_gb: 1.0,
            volumes_gb: 4.25,
            build_cache_gb: 0.25,
        };

        assert_eq!(usage.total_gb(), 8.0);
    }

    #[test]
    fn disk_snapshot_uses_total_and_keeps_categories() {
        let usage = DockerDiskUsage {
            images_gb: 2.5,
            containers_gb: 1.0,
            volumes_gb: 4.25,
            build_cache_gb: 0.25,
        };

        let snapshot = DockerCollector::disk_snapshot(&usage);

        assert_eq!(snapshot.kind, MetricKind::DockerDiskUsed);
        assert_eq!(snapshot.value, 8.0);
        assert_eq!(
            snapshot.labels.get("images_gb").map(String::as_str),
            Some("2.50")
        );
        assert_eq!(
            snapshot.labels.get("build_cache_gb").map(String::as_str),
            Some("0.25")
        );
    }

    #[tokio::test]
    async fn collect_all_reports_health_and_disk_from_one_api() {
        let collector = DockerCollector::with_api(Box::new(FakeDocker::ok(
            vec![
                container("web", ContainerState::Running),
                container("db", ContainerState::Unhealthy),
            ],
            DockerDiskUsage {
                images_gb: 3.0,
                ..Default::default()
            },
        )));

        let snapshots = collector.collect_all().await.expect("both metrics");

        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].kind, MetricKind::DockerUnhealthyContainers);
        assert_eq!(snapshots[0].value, 1.0);
        assert_eq!(snapshots[1].kind, MetricKind::DockerDiskUsed);
        assert_eq!(snapshots[1].value, 3.0);
    }

    #[tokio::test]
    async fn daemon_unreachable_degrades_to_unavailable() {
        let collector =
            DockerCollector::with_api(Box::new(FakeDocker::unreachable("cannot connect")));

        let result = collector.collect_all().await;

        assert!(
            matches!(result, Err(CollectorError::Unavailable { .. })),
            "a missing daemon must degrade, not fail the cycle"
        );
    }

    #[tokio::test]
    async fn disk_failure_still_reports_container_health() {
        let collector = DockerCollector::with_api(Box::new(FakeDocker {
            containers: Ok(vec![container("db", ContainerState::Unhealthy)]),
            disk: Err("df timed out".to_string()),
        }));

        let snapshots = collector.collect_all().await.expect("health survives");

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].kind, MetricKind::DockerUnhealthyContainers);
        assert_eq!(snapshots[0].value, 1.0);
    }
}
