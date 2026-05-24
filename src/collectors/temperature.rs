use std::path::PathBuf;

use async_trait::async_trait;
use tracing::debug;

use crate::collectors::{Collector, CollectorError};
use crate::metrics::{MetricKind, MetricSnapshot};

/// Default path to the SoC thermal zone on Linux ARM systems.
const DEFAULT_THERMAL_ZONE: &str = "/sys/class/thermal/thermal_zone0/temp";

/// Collects SoC temperature from a Linux thermal zone file.
///
/// Reads the file content as millidegrees Celsius and converts to
/// degrees Celsius. Uses `tokio::task::spawn_blocking` for the
/// filesystem read to avoid blocking the async runtime.
///
/// # Ownership Rules
///
/// This collector only gathers data. It does NOT evaluate alerts,
/// send notifications, or execute remediation.
pub struct TemperatureCollector {
    path: PathBuf,
}

impl TemperatureCollector {
    /// Create a collector that reads from the default thermal zone.
    pub fn new() -> Self {
        Self {
            path: PathBuf::from(DEFAULT_THERMAL_ZONE),
        }
    }

    /// Create a collector that reads from a custom path.
    ///
    /// Useful for testing or for systems with thermal zones at
    /// non-standard locations.
    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    /// Parse the raw contents of a thermal zone file.
    ///
    /// The file contains a single integer in millidegrees Celsius
    /// (e.g., `45000` = 45.0°C).
    ///
    /// Separated from `collect()` for testability — parsing logic
    /// can be verified without a real filesystem.
    fn parse_millidegrees(raw: &str) -> Result<f64, CollectorError> {
        let trimmed = raw.trim();
        let millidegrees: i64 = trimmed.parse().map_err(|e| CollectorError::ParseFailed {
            metric: "temperature".to_string(),
            details: format!("cannot parse '{}' as millidegrees: {}", trimmed, e),
        })?;

        Ok(millidegrees as f64 / 1000.0)
    }
}

impl Default for TemperatureCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Collector for TemperatureCollector {
    fn name(&self) -> &str {
        "temperature"
    }

    async fn collect(&self) -> Result<MetricSnapshot, CollectorError> {
        let path = self.path.clone();

        // Filesystem reads are blocking — offload to a blocking thread.
        let raw = tokio::task::spawn_blocking(move || std::fs::read_to_string(&path))
            .await
            .map_err(|e| CollectorError::Unavailable {
                name: "temperature".to_string(),
                reason: format!("spawn_blocking failed: {}", e),
            })?
            .map_err(|e| CollectorError::ReadFailed {
                metric: "temperature".to_string(),
                source: e,
            })?;

        debug!(raw = %raw.trim(), "Read thermal zone");

        let celsius = Self::parse_millidegrees(&raw)?;

        debug!(
            metric = %MetricKind::Temperature,
            value = celsius,
            path = %self.path.display(),
            "Collected temperature"
        );

        Ok(MetricSnapshot::new(MetricKind::Temperature, celsius)
            .with_label("zone", "thermal_zone0"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_normal_temperature() {
        // 45000 millidegrees = 45.0°C
        let result = TemperatureCollector::parse_millidegrees("45000");
        assert!(result.is_ok());
        let temp = result.unwrap();
        assert!((temp - 45.0).abs() < 0.01);
    }

    #[test]
    fn parse_temperature_with_newline() {
        let result = TemperatureCollector::parse_millidegrees("38500\n");
        assert!(result.is_ok());
        let temp = result.unwrap();
        assert!((temp - 38.5).abs() < 0.01);
    }

    #[test]
    fn parse_zero_temperature() {
        let result = TemperatureCollector::parse_millidegrees("0");
        assert!(result.is_ok());
        assert!((result.unwrap()).abs() < 0.01);
    }

    #[test]
    fn parse_high_temperature() {
        // 85000 millidegrees = 85.0°C (critical zone)
        let result = TemperatureCollector::parse_millidegrees("85000");
        assert!(result.is_ok());
        assert!((result.unwrap() - 85.0).abs() < 0.01);
    }

    #[test]
    fn parse_invalid_input_returns_error() {
        let result = TemperatureCollector::parse_millidegrees("not_a_number");
        assert!(result.is_err());
    }

    #[test]
    fn parse_empty_input_returns_error() {
        let result = TemperatureCollector::parse_millidegrees("");
        assert!(result.is_err());
    }

    #[test]
    fn with_path_creates_custom_collector() {
        let collector = TemperatureCollector::with_path(PathBuf::from("/tmp/fake_temp"));
        assert_eq!(collector.name(), "temperature");
        assert_eq!(collector.path, PathBuf::from("/tmp/fake_temp"));
    }

    #[test]
    fn default_uses_standard_path() {
        let collector = TemperatureCollector::default();
        assert_eq!(collector.path, PathBuf::from(DEFAULT_THERMAL_ZONE));
    }
}
