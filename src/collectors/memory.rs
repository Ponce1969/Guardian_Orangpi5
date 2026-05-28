use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use tracing::debug;

use crate::collectors::{Collector, CollectorError};
use crate::metrics::{MetricKind, MetricSnapshot};

/// Default path to meminfo on Linux.
const DEFAULT_MEMINFO: &str = "/proc/meminfo";

/// Parsed memory values from `/proc/meminfo`.
///
/// Only the fields needed for utilization calculation are stored.
/// We prefer `MemAvailable` over naive `MemFree` because:
/// - `MemAvailable` accounts for buffers/cache that can be reclaimed
/// - `MemFree` only reports truly free pages, which understates real capacity
/// - On Docker-heavy systems, cache is often reclaimable and `MemAvailable`
///   reflects actual application-usable memory accurately
#[derive(Debug, Clone, Copy)]
struct MemInfo {
    total_kb: u64,
    available_kb: u64,
}

impl MemInfo {
    /// Parse `/proc/meminfo` content into structured values.
    ///
    /// Reads `MemTotal` and `MemAvailable` (or falls back to
    /// `MemFree` + `Buffers` + `Cached` on older kernels).
    fn parse(content: &str) -> Option<Self> {
        let fields = parse_meminfo_fields(content);

        let total_kb = *fields.get("MemTotal")?;
        let available_kb = fields
            .get("MemAvailable")
            .copied()
            // Fallback: MemFree + Buffers + Cached (approximates MemAvailable
            // on kernels < 3.14 where MemAvailable is absent)
            .or_else(|| {
                let free = fields.get("MemFree").copied().unwrap_or(0);
                let buffers = fields.get("Buffers").copied().unwrap_or(0);
                let cached = fields.get("Cached").copied().unwrap_or(0);
                Some(free + buffers + cached)
            })?;

        if total_kb == 0 || available_kb > total_kb {
            return None;
        }

        Some(Self {
            total_kb,
            available_kb,
        })
    }

    /// Calculate memory usage as a percentage.
    ///
    /// Uses `MemAvailable` for the denominator: `100 * (1 - available/total)`.
    /// This gives the real pressure metric — what percentage of memory
    /// is actually unavailable to applications.
    fn usage_percent(&self) -> f64 {
        if self.total_kb == 0 {
            return 0.0;
        }
        let used_ratio = 1.0 - (self.available_kb as f64 / self.total_kb as f64);
        (used_ratio * 100.0).clamp(0.0, 100.0)
    }
}

/// Parse key-value pairs from `/proc/meminfo` into a HashMap.
///
/// Format: `KeyName:      12345 kB`
fn parse_meminfo_fields(content: &str) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, ':');
        let key = match parts.next() {
            Some(k) => k.trim(),
            None => continue,
        };
        let value_part = match parts.next() {
            Some(v) => v.trim(),
            None => continue,
        };
        let num_str = match value_part.split_whitespace().next() {
            Some(n) => n,
            None => continue,
        };
        if let Ok(num) = num_str.parse::<u64>() {
            map.insert(key.to_string(), num);
        }
    }
    map
}

/// Collects memory utilization from Linux `/proc/meminfo`.
///
/// Calculates real memory usage percentage using `MemAvailable`
/// (which accounts for reclaimable buffers/cache), falling back to
/// `MemFree + Buffers + Cached` on older kernels.
///
/// Uses `tokio::task::spawn_blocking` for filesystem reads and
/// `std::sync::Mutex` for interior mutability. The mutex is held
/// for microseconds — no contention risk.
///
/// # Ownership Rules
///
/// This collector only gathers data. It does NOT evaluate alerts,
/// send notifications, or execute remediation.
pub struct MemoryCollector {
    path: PathBuf,
    state: Mutex<Option<MemInfo>>,
}

impl MemoryCollector {
    /// Create a collector that reads from the default `/proc/meminfo`.
    pub fn new() -> Self {
        Self {
            path: PathBuf::from(DEFAULT_MEMINFO),
            state: Mutex::new(None),
        }
    }

    /// Create a collector that reads from a custom path.
    ///
    /// Useful for testing with mock meminfo content.
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(None),
        }
    }
}

impl Default for MemoryCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Collector for MemoryCollector {
    fn name(&self) -> &str {
        "memory"
    }

    async fn collect(&self) -> Result<MetricSnapshot, CollectorError> {
        let path = self.path.clone();

        let raw = tokio::task::spawn_blocking(move || std::fs::read_to_string(&path))
            .await
            .map_err(|e| CollectorError::Unavailable {
                name: "memory".to_string(),
                reason: format!("spawn_blocking failed: {}", e),
            })?
            .map_err(|e| CollectorError::ReadFailed {
                metric: "memory_usage".to_string(),
                source: e,
            })?;

        let parsed = MemInfo::parse(&raw).ok_or_else(|| CollectorError::ParseFailed {
            metric: "memory_usage".to_string(),
            details: "failed to parse /proc/meminfo: missing MemTotal or MemAvailable".to_string(),
        })?;

        let usage = parsed.usage_percent();

        let mut state = self.state.lock().map_err(|e| CollectorError::Unavailable {
            name: "memory".to_string(),
            reason: format!("state lock poisoned: {}", e),
        })?;

        debug!(
            metric = %MetricKind::MemoryUsage,
            usage_pct = usage,
            total_kb = parsed.total_kb,
            available_kb = parsed.available_kb,
            "Memory utilization calculated"
        );

        *state = Some(parsed);

        Ok(MetricSnapshot::new(MetricKind::MemoryUsage, usage)
            .with_label("total_kb", parsed.total_kb.to_string())
            .with_label("available_kb", parsed.available_kb.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_meminfo(total_kb: u64, available_kb: u64) -> String {
        format!(
            "MemTotal:       {} kB\n\
             MemFree:         {} kB\n\
             MemAvailable:   {} kB\n\
             Buffers:         100000 kB\n\
             Cached:          500000 kB\n\
             SwapCached:      0 kB\n",
            total_kb,
            available_kb.saturating_sub(500000), // MemFree is approximate
            available_kb,
        )
    }

    #[test]
    fn parse_standard_meminfo() {
        let content = make_meminfo(16000000, 10000000);
        let info = MemInfo::parse(&content);
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.total_kb, 16000000);
        assert_eq!(info.available_kb, 10000000);
    }

    #[test]
    fn usage_percent_calculation() {
        let info = MemInfo {
            total_kb: 16000000,
            available_kb: 4000000,
        };
        // usage = 100 * (1 - 4M/16M) = 100 * 0.75 = 75%
        let pct = info.usage_percent();
        assert!((pct - 75.0).abs() < 0.01);
    }

    #[test]
    fn usage_percent_full_memory() {
        // 0 available = 100% usage
        let info = MemInfo {
            total_kb: 16000000,
            available_kb: 0,
        };
        let pct = info.usage_percent();
        assert!((pct - 100.0).abs() < 0.01);
    }

    #[test]
    fn usage_percent_idle_system() {
        // All memory available = 0% usage
        let info = MemInfo {
            total_kb: 16000000,
            available_kb: 16000000,
        };
        let pct = info.usage_percent();
        assert!(pct.abs() < 0.01);
    }

    #[test]
    fn usage_percent_typical_server() {
        // 15.3GB total, 10.2GB available
        let info = MemInfo {
            total_kb: 15360000,
            available_kb: 10240000,
        };
        // usage = 100 * (1 - 10240/15360) = 33.3%
        let pct = info.usage_percent();
        assert!((pct - 33.33).abs() < 0.5);
    }

    #[test]
    fn parse_rejects_zero_total() {
        let content = "MemTotal:       0 kB\nMemAvailable:   1000 kB\n";
        let info = MemInfo::parse(content);
        assert!(info.is_none());
    }

    #[test]
    fn parse_rejects_available_exceeding_total() {
        let content = "MemTotal:       1000 kB\nMemAvailable:   2000 kB\n";
        let info = MemInfo::parse(content);
        assert!(info.is_none());
    }

    #[test]
    fn parse_fallback_to_memfree_when_no_memavailable() {
        // Kernel < 3.14: no MemAvailable line
        let content = "MemTotal:       16000000 kB\n\
                       MemFree:         3000000 kB\n\
                       Buffers:         500000 kB\n\
                       Cached:          4000000 kB\n";
        let info = MemInfo::parse(content);
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.total_kb, 16000000);
        // available = MemFree + Buffers + Cached = 3000000 + 500000 + 4000000 = 7500000
        assert_eq!(info.available_kb, 7500000);
    }

    #[test]
    fn parse_meminfo_fields_basic() {
        let content = "MemTotal:       16000000 kB\nMemFree:         8000000 kB\n";
        let fields = parse_meminfo_fields(content);
        assert_eq!(fields.get("MemTotal"), Some(&16000000_u64));
        assert_eq!(fields.get("MemFree"), Some(&8000000_u64));
    }

    #[test]
    fn parse_meminfo_fields_ignores_malformed_lines() {
        let content = "MemTotal:       16000000 kB\n\
                       MalformedLineWithoutColon\n\
                       MemFree:         8000000 kB\n";
        let fields = parse_meminfo_fields(content);
        assert_eq!(fields.get("MemTotal"), Some(&16000000_u64));
        assert_eq!(fields.get("MemFree"), Some(&8000000_u64));
        assert!(!fields.contains_key("MalformedLineWithoutColon"));
    }

    #[test]
    fn usage_is_clamped_to_range() {
        let info = MemInfo {
            total_kb: 16000000,
            available_kb: 4000000,
        };
        let pct = info.usage_percent();
        assert!(pct >= 0.0);
        assert!(pct <= 100.0);
    }

    #[test]
    fn with_path_creates_custom_collector() {
        let collector = MemoryCollector::with_path(PathBuf::from("/tmp/fake_meminfo"));
        assert_eq!(collector.name(), "memory");
    }

    #[test]
    fn default_uses_standard_path() {
        let collector = MemoryCollector::default();
        assert_eq!(collector.path, PathBuf::from(DEFAULT_MEMINFO));
    }
}
