use std::path::PathBuf;

use async_trait::async_trait;
use tracing::debug;

use crate::collectors::{Collector, CollectorError};
use crate::metrics::{MetricKind, MetricSnapshot};

/// Default mount point to monitor.
const DEFAULT_MOUNT: &str = "/";

/// Parsed disk usage from filesystem statistics.
///
/// Uses available bytes (for unprivileged users) for calculation —
/// this matches `df` output on Linux and represents actual usable
/// space after reserving ~5% for root.
struct DiskUsage {
    total_bytes: u64,
    available_bytes: u64,
    used_bytes: u64,
}

impl DiskUsage {
    /// Calculate disk usage percentage.
    ///
    /// Uses `used / total` to match `df` output, which accounts
    /// for reserved blocks in the percentage shown.
    fn usage_percent(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.used_bytes as f64 / self.total_bytes as f64 * 100.0).clamp(0.0, 100.0)
    }
}

#[cfg(unix)]
impl DiskUsage {
    /// Read disk usage via `statfs` syscall (Linux/macOS).
    ///
    /// Uses `f_bavail` instead of `f_bfree` because:
    /// - `f_bfree` includes reserved blocks (root reserve)
    /// - `f_bavail` excludes them, matching what `df` reports
    /// - On Docker-heavy systems, available space matters more than free
    fn from_path(path: &str) -> Result<Self, CollectorError> {
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        let c_path = std::ffi::CString::new(path).map_err(|e| CollectorError::ParseFailed {
            metric: "disk_usage".to_string(),
            details: format!("invalid path {}: {}", path, e),
        })?;

        let result = unsafe { libc::statfs(c_path.as_ptr(), &mut stat) };
        if result != 0 {
            return Err(CollectorError::ReadFailed {
                metric: "disk_usage".to_string(),
                source: std::io::Error::last_os_error(),
            });
        }

        let block_size = stat.f_bsize as u64;
        let total_bytes = stat.f_blocks.saturating_mul(block_size);
        let available_bytes = stat.f_bavail.saturating_mul(block_size);
        let used_bytes = total_bytes.saturating_sub(available_bytes);

        Ok(Self {
            total_bytes,
            available_bytes,
            used_bytes,
        })
    }
}

#[cfg(not(unix))]
impl DiskUsage {
    /// Fallback for non-Unix: use `std::fs::metadata` for total,
    /// available is estimated (not available on Windows without Win32 APIs).
    ///
    /// This is a development-time fallback. Production runs on Linux ARM.
    fn from_path(path: &str) -> Result<Self, CollectorError> {
        let metadata = std::fs::metadata(path).map_err(|e| CollectorError::ReadFailed {
            metric: "disk_usage".to_string(),
            source: e,
        })?;

        // On non-Unix, we can't get filesystem stats easily.
        // Return a placeholder that won't crash — tests use with_mount
        // and the real collector always runs on Linux.
        let total_bytes = if metadata.is_dir() { 1 } else { 0 };

        Ok(Self {
            total_bytes,
            available_bytes: 0,
            used_bytes: total_bytes,
        })
    }
}

/// Collects disk usage from the filesystem.
///
/// On Linux: uses `statfs` syscall for accurate disk usage matching `df`.
/// On other platforms: provides a stub that won't crash but returns
/// limited data — production runs exclusively on Linux ARM.
///
/// Uses `spawn_blocking` for the syscall and carries no mutable state.
///
/// # Ownership Rules
///
/// This collector only gathers data. It does NOT evaluate alerts,
/// send notifications, or execute remediation.
pub struct DiskCollector {
    mount: PathBuf,
}

impl DiskCollector {
    /// Create a collector that monitors the root filesystem.
    pub fn new() -> Self {
        Self {
            mount: PathBuf::from(DEFAULT_MOUNT),
        }
    }

    /// Create a collector that monitors a specific mount point.
    ///
    /// Useful for testing or monitoring a non-root filesystem.
    pub fn with_mount(mount: PathBuf) -> Self {
        Self { mount }
    }
}

impl Default for DiskCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Collector for DiskCollector {
    fn name(&self) -> &str {
        "disk"
    }

    async fn collect(&self) -> Result<MetricSnapshot, CollectorError> {
        let mount = self.mount.clone();

        let usage =
            tokio::task::spawn_blocking(move || DiskUsage::from_path(&mount.to_string_lossy()))
                .await
                .map_err(|e| CollectorError::Unavailable {
                    name: "disk".to_string(),
                    reason: format!("spawn_blocking failed: {}", e),
                })??;

        let pct = usage.usage_percent();

        debug!(
            metric = %MetricKind::DiskUsage,
            usage_pct = pct,
            total_bytes = usage.total_bytes,
            available_bytes = usage.available_bytes,
            mount = %self.mount.display(),
            "Disk utilization calculated"
        );

        Ok(MetricSnapshot::new(MetricKind::DiskUsage, pct)
            .with_label("mount", self.mount.to_string_lossy().to_string())
            .with_label("total_bytes", usage.total_bytes.to_string())
            .with_label("available_bytes", usage.available_bytes.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_percent_calculation() {
        let usage = DiskUsage {
            total_bytes: 100_000_000_000,    // 100 GB
            available_bytes: 20_000_000_000, // 20 GB available
            used_bytes: 80_000_000_000,      // 80 GB used
        };
        let pct = usage.usage_percent();
        assert!((pct - 80.0).abs() < 0.01);
    }

    #[test]
    fn usage_percent_full_disk() {
        let usage = DiskUsage {
            total_bytes: 100_000_000_000,
            available_bytes: 0,
            used_bytes: 100_000_000_000,
        };
        let pct = usage.usage_percent();
        assert!((pct - 100.0).abs() < 0.01);
    }

    #[test]
    fn usage_percent_empty_disk() {
        let usage = DiskUsage {
            total_bytes: 100_000_000_000,
            available_bytes: 100_000_000_000,
            used_bytes: 0,
        };
        let pct = usage.usage_percent();
        assert!(pct.abs() < 0.01);
    }

    #[test]
    fn usage_percent_typical_server() {
        // 500 GB disk, 125 GB used = 25%
        let usage = DiskUsage {
            total_bytes: 500_000_000_000,
            available_bytes: 375_000_000_000,
            used_bytes: 125_000_000_000,
        };
        let pct = usage.usage_percent();
        assert!((pct - 25.0).abs() < 0.5);
    }

    #[test]
    fn usage_percent_clamped_to_range() {
        let usage = DiskUsage {
            total_bytes: 100_000_000_000,
            available_bytes: 30_000_000_000,
            used_bytes: 70_000_000_000,
        };
        let pct = usage.usage_percent();
        assert!(pct >= 0.0);
        assert!(pct <= 100.0);
    }

    #[test]
    fn usage_percent_zero_total() {
        let usage = DiskUsage {
            total_bytes: 0,
            available_bytes: 0,
            used_bytes: 0,
        };
        assert_eq!(usage.usage_percent(), 0.0);
    }

    #[test]
    #[cfg(unix)]
    fn from_path_root_filesystem() {
        let usage = DiskUsage::from_path("/").expect("Failed to statfs /");
        assert!(
            usage.total_bytes > 0,
            "Root filesystem should have non-zero total"
        );
        assert!(
            usage.available_bytes <= usage.total_bytes,
            "Available should not exceed total"
        );
        let pct = usage.usage_percent();
        assert!(
            pct >= 0.0 && pct <= 100.0,
            "Usage percent should be in [0, 100]"
        );
    }

    #[test]
    fn from_path_invalid_path() {
        let result = DiskUsage::from_path("/nonexistent/path/that/does/not/exist");
        assert!(result.is_err());
    }

    #[test]
    fn default_mount_is_root() {
        let collector = DiskCollector::default();
        assert_eq!(collector.mount, PathBuf::from("/"));
    }

    #[test]
    fn with_mount_custom_path() {
        let collector = DiskCollector::with_mount(PathBuf::from("/home"));
        assert_eq!(collector.mount, PathBuf::from("/home"));
        assert_eq!(collector.name(), "disk");
    }
}
