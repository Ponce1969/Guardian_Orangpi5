use crate::metrics::MetricKind;

/// A single threshold rule that maps a metric kind to warning/critical boundaries.
///
/// Threshold values are compared directly against the metric snapshot value.
/// The AlertEngine evaluates rules in order; the most severe matching rule wins.
#[derive(Debug, Clone)]
pub struct ThresholdRule {
    pub metric: MetricKind,
    pub warning: f64,
    pub critical: f64,
}

/// Build threshold rules from the configuration.
///
/// This keeps the conversion logic in one place so the AlertEngine
/// stays focused on evaluation, not config mapping.
pub fn rules_from_config(config: &crate::config::ThresholdsConfig) -> Vec<ThresholdRule> {
    let rules = vec![
        ThresholdRule {
            metric: MetricKind::CpuUsage,
            warning: config.cpu_warning,
            critical: config.cpu_critical,
        },
        ThresholdRule {
            metric: MetricKind::MemoryUsage,
            warning: config.memory_warning,
            critical: config.memory_critical,
        },
        ThresholdRule {
            metric: MetricKind::DiskUsage,
            warning: config.disk_warning,
            critical: config.disk_critical,
        },
        ThresholdRule {
            metric: MetricKind::Temperature,
            warning: config.temp_warning,
            critical: config.temp_critical,
        },
    ];

    for rule in &rules {
        tracing::debug!(
            metric = %rule.metric,
            warning = rule.warning,
            critical = rule.critical,
            "Loaded threshold rule"
        );
    }

    rules
}

/// Re-export ThresholdsConfig as a type alias for clarity.
/// The actual config struct lives in crate::config.
pub type ThresholdsConfig = crate::config::ThresholdsConfig;
