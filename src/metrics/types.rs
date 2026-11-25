use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The kind of infrastructure metric being measured.
///
/// Each variant corresponds to exactly one type of measurement.
/// Labels provide additional context (e.g., mount point for disk, zone for temperature).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MetricKind {
    CpuUsage,
    MemoryUsage,
    DiskUsage,
    Temperature,
    NetworkThroughput,
    DockerUnhealthyContainers,
    DockerDiskUsed,
    PostgresProbeFailures,
}

impl std::fmt::Display for MetricKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CpuUsage => write!(f, "cpu_usage"),
            Self::MemoryUsage => write!(f, "memory_usage"),
            Self::DiskUsage => write!(f, "disk_usage"),
            Self::Temperature => write!(f, "temperature"),
            Self::NetworkThroughput => write!(f, "network_throughput"),
            Self::DockerUnhealthyContainers => write!(f, "docker_unhealthy_containers"),
            Self::DockerDiskUsed => write!(f, "docker_disk_used"),
            Self::PostgresProbeFailures => write!(f, "postgres_probe_failures"),
        }
    }
}

impl MetricKind {
    /// Returns the display unit for this metric kind.
    ///
    /// Used in alert messages to show correct units:
    /// - Percentage metrics use `%`
    /// - Temperature uses `°C`
    /// - Network uses appropriate throughput units
    pub fn unit(&self) -> &str {
        match self {
            Self::CpuUsage => "%",
            Self::MemoryUsage => "%",
            Self::DiskUsage => "%",
            Self::Temperature => "°C",
            Self::NetworkThroughput => "MB/s",
            Self::DockerUnhealthyContainers => "containers",
            Self::DockerDiskUsed => "GB",
            Self::PostgresProbeFailures => "failures",
        }
    }

    /// Returns a human-readable display name for this metric kind.
    ///
    /// Used in alert titles and Discord embed fields.
    pub fn display_name(&self) -> &str {
        match self {
            Self::CpuUsage => "CPU Usage",
            Self::MemoryUsage => "Memory Usage",
            Self::DiskUsage => "Disk Usage",
            Self::Temperature => "Temperature",
            Self::NetworkThroughput => "Network Throughput",
            Self::DockerUnhealthyContainers => "Docker Unhealthy Containers",
            Self::DockerDiskUsed => "Docker Disk Used",
            Self::PostgresProbeFailures => "PostgreSQL Probe Failures",
        }
    }
}

/// A single metric measurement produced by a Collector.
///
/// Collectors emit one `MetricSnapshot` per collection cycle.
/// The `labels` map carries context that distinguishes otherwise
/// identical metric kinds (e.g., which mount point, which thermal zone).
#[derive(Debug, Clone)]
pub struct MetricSnapshot {
    pub timestamp: DateTime<Utc>,
    pub kind: MetricKind,
    pub value: f64,
    pub labels: HashMap<String, String>,
}

impl MetricSnapshot {
    /// Create a new snapshot with the current timestamp and no labels.
    pub fn new(kind: MetricKind, value: f64) -> Self {
        Self {
            timestamp: Utc::now(),
            kind,
            value,
            labels: HashMap::new(),
        }
    }

    /// Attach a descriptive label to this snapshot.
    ///
    /// Intended for builder-style usage:
    /// ```
    /// use guardian_rs::metrics::{MetricKind, MetricSnapshot};
    ///
    /// let snap = MetricSnapshot::new(MetricKind::DiskUsage, 72.3)
    ///     .with_label("mount", "/");
    /// ```
    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }
}

/// How a unit attaches to a number.
///
/// A symbol belongs against the digits (`80.0%`, `72.0°C`); a word needs a space or
/// the message reads `3.0failures`. This lives here, next to `unit()`, because the
/// alert message and the Discord embed each formatted it themselves and only one of
/// them was fixed.
pub fn unit_suffix(unit: &str) -> String {
    if unit.chars().all(char::is_alphabetic) {
        format!(" {unit}")
    } else {
        unit.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_kind_display() {
        assert_eq!(MetricKind::CpuUsage.to_string(), "cpu_usage");
        assert_eq!(MetricKind::DiskUsage.to_string(), "disk_usage");
        assert_eq!(MetricKind::Temperature.to_string(), "temperature");
    }

    #[test]
    fn snapshot_with_labels() {
        let snap = MetricSnapshot::new(MetricKind::DiskUsage, 85.0)
            .with_label("mount", "/")
            .with_label("device", "/dev/mmcblk0p2");

        assert_eq!(snap.kind, MetricKind::DiskUsage);
        assert_eq!(snap.labels["mount"], "/");
        assert_eq!(snap.labels["device"], "/dev/mmcblk0p2");
    }
}
