use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use tracing::debug;

use crate::collectors::{Collector, CollectorError};
use crate::metrics::{MetricKind, MetricSnapshot};

/// Default path to CPU statistics on Linux.
const DEFAULT_PROC_STAT: &str = "/proc/stat";

/// Exponential Moving Average smoothing factor.
///
/// A value of 0.5 means each new reading contributes 50% to the
/// smoothed value. This provides good anti-flapping behavior at
/// 30-second polling intervals — a brief CPU spike won't immediately
/// trigger an alert, but sustained high usage will.
const EMA_ALPHA: f64 = 0.5;

/// Parsed CPU time values from a single `/proc/stat` reading.
///
/// Only the fields needed for utilization calculation are stored.
#[derive(Clone, Copy, Debug)]
struct ProcCpuTime {
    total: u64,
    idle: u64,
}

impl ProcCpuTime {
    /// Parse the aggregate `cpu ` line from `/proc/stat`.
    ///
    /// The line format is:
    /// ```text
    /// cpu  user nice system idle iowait irq softirq steal guest guest_nice
    /// ```
    ///
    /// Fields after `idle` may be missing on older kernels and default to 0.
    fn parse(line: &str) -> Option<Self> {
        let fields: Vec<u64> = line
            .split_whitespace()
            .skip(1) // skip "cpu" label
            .filter_map(|f| f.parse().ok())
            .collect();

        // Minimum: user, nice, system, idle (4 fields)
        if fields.len() < 4 {
            return None;
        }

        let idle = fields[3] + fields.get(4).copied().unwrap_or(0); // idle + iowait
        let total: u64 = fields.iter().sum();

        // Total must be positive to avoid division by zero.
        if total == 0 {
            return None;
        }

        Some(Self { total, idle })
    }
}

/// Internal state held between collection cycles.
///
/// The first reading establishes a baseline — no utilization can be
/// calculated until a second reading is available. After that, each
/// reading produces a delta utilization value that is smoothed with
/// an EMA to prevent alert flapping.
#[derive(Debug)]
struct CpuState {
    prev_time: Option<ProcCpuTime>,
    ema: Option<f64>,
}

/// Collects CPU utilization from Linux `/proc/stat`.
///
/// Calculates real CPU usage as a percentage between consecutive
/// readings, then applies an Exponential Moving Average (EMA) to
/// smooth out brief spikes and prevent alert flapping.
///
/// The first `collect()` call establishes a baseline and returns an
/// `Unavailable` error. Subsequent calls return smoothed utilization.
///
/// Uses `tokio::task::spawn_blocking` for filesystem reads and
/// `std::sync::Mutex` for interior mutability of the state between
/// reads. The mutex is held for microseconds — no contention risk.
///
/// # Ownership Rules
///
/// This collector only gathers data. It does NOT evaluate alerts,
/// send notifications, or execute remediation.
pub struct CpuCollector {
    path: PathBuf,
    state: Mutex<CpuState>,
    ema_alpha: f64,
}

impl CpuCollector {
    /// Create a collector that reads from the default `/proc/stat`.
    pub fn new() -> Self {
        Self {
            path: PathBuf::from(DEFAULT_PROC_STAT),
            state: Mutex::new(CpuState {
                prev_time: None,
                ema: None,
            }),
            ema_alpha: EMA_ALPHA,
        }
    }

    /// Create a collector that reads from a custom path.
    ///
    /// Useful for testing with mock `/proc/stat` content.
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(CpuState {
                prev_time: None,
                ema: None,
            }),
            ema_alpha: EMA_ALPHA,
        }
    }

    /// Find the aggregate `cpu ` line in `/proc/stat` content.
    ///
    /// The file contains per-CPU lines (`cpu0`, `cpu1`, ...) followed
    /// by the aggregate `cpu ` line. We want the aggregate.
    fn find_aggregate_cpu_line(content: &str) -> Option<&str> {
        content
            .lines()
            .find(|line| line.starts_with("cpu ") || line.starts_with("cpu\t"))
    }

    /// Calculate CPU utilization percentage from two consecutive readings.
    ///
    /// Returns `None` if total delta is zero (would cause division by zero).
    fn calculate_usage(prev: ProcCpuTime, curr: ProcCpuTime) -> Option<f64> {
        let total_delta = curr.total.saturating_sub(prev.total);
        let idle_delta = curr.idle.saturating_sub(prev.idle);

        if total_delta == 0 {
            return None;
        }

        let usage = 100.0 * (1.0 - (idle_delta as f64 / total_delta as f64));
        Some(usage.clamp(0.0, 100.0))
    }

    /// Apply EMA smoothing to a raw utilization value.
    ///
    /// On the first reading, the EMA is initialized with the raw value.
    /// Subsequent readings blend the new value with the previous EMA
    /// using the configured alpha factor.
    fn apply_ema(current_ema: Option<f64>, raw_usage: f64, alpha: f64) -> f64 {
        match current_ema {
            Some(prev) => alpha * raw_usage + (1.0 - alpha) * prev,
            None => raw_usage,
        }
    }
}

impl Default for CpuCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Collector for CpuCollector {
    fn name(&self) -> &str {
        "cpu"
    }

    async fn collect(&self) -> Result<MetricSnapshot, CollectorError> {
        let path = self.path.clone();

        // Read /proc/stat on a blocking thread — filesystem reads must
        // not block the async runtime.
        let raw = tokio::task::spawn_blocking(move || std::fs::read_to_string(&path))
            .await
            .map_err(|e| CollectorError::Unavailable {
                name: "cpu".to_string(),
                reason: format!("spawn_blocking failed: {}", e),
            })?
            .map_err(|e| CollectorError::ReadFailed {
                metric: "cpu_usage".to_string(),
                source: e,
            })?;

        // Find the aggregate cpu line.
        let cpu_line =
            Self::find_aggregate_cpu_line(&raw).ok_or_else(|| CollectorError::ParseFailed {
                metric: "cpu_usage".to_string(),
                details: "no aggregate 'cpu ' line found in /proc/stat".to_string(),
            })?;

        let curr_time =
            ProcCpuTime::parse(cpu_line).ok_or_else(|| CollectorError::ParseFailed {
                metric: "cpu_usage".to_string(),
                details: "failed to parse cpu line in /proc/stat".to_string(),
            })?;

        // Lock state, calculate usage, and update in one scope.
        let mut state = self.state.lock().map_err(|e| CollectorError::Unavailable {
            name: "cpu".to_string(),
            reason: format!("state lock poisoned: {}", e),
        })?;

        match state.prev_time {
            Some(prev) => {
                let raw_usage = Self::calculate_usage(prev, curr_time).unwrap_or(0.0);
                let smoothed = Self::apply_ema(state.ema, raw_usage, self.ema_alpha);

                debug!(
                    metric = %MetricKind::CpuUsage,
                    raw = raw_usage,
                    ema = smoothed,
                    alpha = self.ema_alpha,
                    "CPU utilization calculated"
                );

                state.prev_time = Some(curr_time);
                state.ema = Some(smoothed);

                Ok(MetricSnapshot::new(MetricKind::CpuUsage, smoothed)
                    .with_label("source", "proc_stat"))
            }
            None => {
                // First reading — no baseline to calculate delta from.
                // Store the current reading and return an unavailable error.
                // The orchestrator will log this and retry on the next cycle.
                state.prev_time = Some(curr_time);
                Err(CollectorError::Unavailable {
                    name: "cpu".to_string(),
                    reason: "warming up — first reading established baseline".to_string(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_proc_stat_aggregate_line() {
        let line = "cpu  2255 34 2290 22625563 6290 0 235 0 0 0";
        let result = ProcCpuTime::parse(line);
        assert!(result.is_some());
        let stat = result.unwrap();
        // idle = 22625563 + 6290 = 22631853
        assert_eq!(stat.idle, 22631853);
        // total = 2255 + 34 + 2290 + 22625563 + 6290 + 0 + 235 + 0 + 0 + 0 = 22636667
        assert_eq!(stat.total, 22636667);
    }

    #[test]
    fn parse_proc_stat_minimal_line() {
        // Older kernels may only have 4 fields
        let line = "cpu  100 50 200 1000000";
        let result = ProcCpuTime::parse(line);
        assert!(result.is_some());
        let stat = result.unwrap();
        assert_eq!(stat.idle, 1000000); // idle only, no iowait
        assert_eq!(stat.total, 1000350);
    }

    #[test]
    fn parse_proc_stat_rejects_per_cpu_lines() {
        // "cpu0" should not be matched by find_aggregate_cpu_line
        let content = "cpu0 100 50 200 1000000\ncpu  2255 34 2290 22625563 6290 0 235 0 0 0\n";
        let found = CpuCollector::find_aggregate_cpu_line(content);
        assert!(found.is_some());
        assert!(found.unwrap().starts_with("cpu  "));
    }

    #[test]
    fn parse_proc_stat_rejects_too_few_fields() {
        let line = "cpu  100";
        let result = ProcCpuTime::parse(line);
        assert!(result.is_none());
    }

    #[test]
    fn calculate_usage_basic() {
        let prev = ProcCpuTime {
            total: 10000,
            idle: 9000,
        };
        let curr = ProcCpuTime {
            total: 20000,
            idle: 18000,
        };
        // total_delta=10000, idle_delta=9000, usage = 100 * (1 - 9000/10000) = 10%
        let usage = CpuCollector::calculate_usage(prev, curr);
        assert!(usage.is_some());
        assert!((usage.unwrap() - 10.0).abs() < 0.01);
    }

    #[test]
    fn calculate_usage_full_load() {
        let prev = ProcCpuTime {
            total: 10000,
            idle: 5000,
        };
        let curr = ProcCpuTime {
            total: 20000,
            idle: 5000, // idle didn't increase — full CPU usage
        };
        let usage = CpuCollector::calculate_usage(prev, curr);
        assert!(usage.is_some());
        // total_delta=10000, idle_delta=0, usage = 100%
        assert!((usage.unwrap() - 100.0).abs() < 0.01);
    }

    #[test]
    fn calculate_usage_idle_system() {
        let prev = ProcCpuTime {
            total: 10000,
            idle: 9800,
        };
        let curr = ProcCpuTime {
            total: 20000,
            idle: 19800,
        };
        // total_delta=10000, idle_delta=10000, usage = 100*(1-1) = 0%
        let usage = CpuCollector::calculate_usage(prev, curr);
        assert!(usage.is_some());
        assert!((usage.unwrap()).abs() < 0.01);
    }

    #[test]
    fn calculate_usage_zero_delta_returns_none() {
        let prev = ProcCpuTime {
            total: 10000,
            idle: 9000,
        };
        let curr = ProcCpuTime {
            total: 10000,
            idle: 9000,
        };
        let usage = CpuCollector::calculate_usage(prev, curr);
        assert!(usage.is_none());
    }

    #[test]
    fn ema_first_reading_uses_raw_value() {
        let smoothed = CpuCollector::apply_ema(None, 50.0, 0.5);
        assert!((smoothed - 50.0).abs() < 0.01);
    }

    #[test]
    fn ema_smooths_subsequent_readings() {
        // alpha=0.5: 50% new, 50% old
        let first = CpuCollector::apply_ema(None, 50.0, 0.5);
        assert!((first - 50.0).abs() < 0.01);

        let second = CpuCollector::apply_ema(Some(first), 80.0, 0.5);
        // 0.5 * 80 + 0.5 * 50 = 65
        assert!((second - 65.0).abs() < 0.01);

        let third = CpuCollector::apply_ema(Some(second), 30.0, 0.5);
        // 0.5 * 30 + 0.5 * 65 = 47.5
        assert!((third - 47.5).abs() < 0.01);
    }

    #[test]
    fn ema_dampens_spikes() {
        let alpha = 0.3; // More smoothing
        let mut ema: Option<f64> = None;

        // Baseline at 20%
        ema = Some(CpuCollector::apply_ema(ema, 20.0, alpha));
        // Spike to 95% — EMA should only go to 0.3*95 + 0.7*20 = 42.5
        ema = Some(CpuCollector::apply_ema(ema, 95.0, alpha));
        assert!((ema.unwrap() - 42.5).abs() < 0.01);
        // Back to 20% — EMA drops gradually, not immediately
        ema = Some(CpuCollector::apply_ema(ema, 20.0, alpha));
        // 0.3*20 + 0.7*42.5 = 35.75
        assert!((ema.unwrap() - 35.75).abs() < 0.01);
    }

    #[test]
    fn find_aggregate_cpu_line_skips_per_cpu() {
        let content = "cpu  2255 34 2290 22625563 6290 0 235 0 0 0\ncpu0 1234 56 78 9101112 3456 0 78 0 0 0\n";
        let found = CpuCollector::find_aggregate_cpu_line(content);
        assert!(found.is_some());
        assert!(found.unwrap().starts_with("cpu  "));
    }

    #[test]
    fn find_aggregate_returns_none_when_absent() {
        let content = "cpu0 1234 56 78 9101112 3456 0 78 0 0 0\n";
        let found = CpuCollector::find_aggregate_cpu_line(content);
        assert!(found.is_none());
    }

    #[test]
    fn usage_is_clamped_to_range() {
        // Unlikely edge case: ensure result is never negative or > 100
        let prev = ProcCpuTime {
            total: 10000,
            idle: 9000,
        };
        let curr = ProcCpuTime {
            total: 20000,
            idle: 18000,
        };
        let usage = CpuCollector::calculate_usage(prev, curr);
        assert!(usage.is_some());
        assert!(usage.unwrap() >= 0.0);
        assert!(usage.unwrap() <= 100.0);
    }
}
