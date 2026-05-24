use chrono::Utc;
use thiserror::Error;
use tracing::debug;

use crate::alerts::rules::ThresholdRule;
use crate::alerts::state::{AlertSeverity, AlertState, AlertStateTracker};
use crate::alerts::AlertEvent;
use crate::metrics::{MetricKind, MetricSnapshot};

/// Errors that can occur during alert evaluation.
#[derive(Debug, Error)]
pub enum AlertError {
    #[error("No threshold rule found for metric: {0}")]
    NoRuleForMetric(String),
}

/// The Alert Engine contract.
///
/// The Alert Engine owns a single responsibility: evaluating metric
/// snapshots against threshold rules and producing alert events on
/// state transitions.
///
/// # Ownership Rules
///
/// - AlertEngine MUST NOT inspect infrastructure directly.
/// - AlertEngine MUST NOT call Docker APIs.
/// - AlertEngine MUST NOT send notifications.
/// - AlertEngine MUST NOT execute shell commands.
///
/// Alerts only evaluate rules.
pub trait AlertEvaluator: Send {
    /// Evaluate a metric snapshot and return any alert events produced.
    ///
    /// Returns an empty `Vec` when there is no state transition
    /// (deduplication by design). Returns one `AlertEvent` per
    /// transition.
    fn evaluate(&mut self, snapshot: &MetricSnapshot) -> Vec<AlertEvent>;
}

/// Default alert engine implementation.
///
/// Holds threshold rules and tracks per-metric state to produce
/// events only on transitions.
pub struct AlertEngine {
    rules: Vec<ThresholdRule>,
    tracker: AlertStateTracker,
}

impl AlertEngine {
    pub fn new(rules: Vec<ThresholdRule>) -> Self {
        Self {
            rules,
            tracker: AlertStateTracker::new(),
        }
    }

    /// Returns the number of threshold rules loaded into the engine.
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Find the threshold rule for a given metric kind.
    fn find_rule(&self, kind: MetricKind) -> Option<&ThresholdRule> {
        self.rules.iter().find(|r| r.metric == kind)
    }

    /// Determine the target alert state for a metric value against
    /// its threshold rule.
    ///
    /// Returns `None` when there is no rule for this metric kind,
    /// meaning the metric should be skipped entirely.
    fn classify(&self, kind: MetricKind, value: f64) -> Option<AlertState> {
        match self.find_rule(kind) {
            Some(rule) => {
                if value >= rule.critical {
                    Some(AlertState::Critical)
                } else if value >= rule.warning {
                    Some(AlertState::Warning)
                } else {
                    Some(AlertState::Normal)
                }
            }
            None => None,
        }
    }
}

impl AlertEvaluator for AlertEngine {
    fn evaluate(&mut self, snapshot: &MetricSnapshot) -> Vec<AlertEvent> {
        let new_state = match self.classify(snapshot.kind, snapshot.value) {
            Some(state) => state,
            None => {
                debug!(
                    metric = %snapshot.kind,
                    value = snapshot.value,
                    "No threshold rule for metric — skipping"
                );
                return Vec::new();
            }
        };

        let old_state = self.tracker.get(snapshot.kind);

        debug!(
            metric = %snapshot.kind,
            value = snapshot.value,
            old_state = ?old_state,
            new_state = ?new_state,
            "Evaluated metric against thresholds"
        );

        // Only emit an event on state transitions.
        let Some(prev) = self.tracker.transition(snapshot.kind, new_state) else {
            debug!(
                metric = %snapshot.kind,
                value = snapshot.value,
                state = ?new_state,
                "No state transition — deduplication suppressing event"
            );
            return Vec::new();
        };

        debug!(
            metric = %snapshot.kind,
            from = ?prev,
            to = ?new_state,
            "State transition detected — generating alert"
        );

        // At this point, old_state != new_state (guaranteed by transition).
        let severity = match (old_state, new_state) {
            (AlertState::Normal, AlertState::Warning) => AlertSeverity::Warning,
            (AlertState::Normal, AlertState::Critical) => AlertSeverity::Critical,
            (AlertState::Warning, AlertState::Critical) => AlertSeverity::Critical,
            (AlertState::Warning, AlertState::Normal) => AlertSeverity::Recovered,
            (AlertState::Critical, AlertState::Normal) => AlertSeverity::Recovered,
            (AlertState::Critical, AlertState::Warning) => AlertSeverity::Recovered,
            // Defensive: same-state should never reach here.
            (old, new) if old == new => return Vec::new(),
            _ => AlertSeverity::Warning,
        };

        let rule = self.find_rule(snapshot.kind);
        let threshold = match severity {
            AlertSeverity::Critical => rule.map(|r| r.critical).unwrap_or(0.0),
            AlertSeverity::Warning => rule.map(|r| r.warning).unwrap_or(0.0),
            // Recovery: show the threshold we recovered FROM.
            AlertSeverity::Recovered => rule.map(|r| r.warning).unwrap_or(0.0),
        };

        let unit = snapshot.kind.unit();
        let message = format!(
            "{} {} is {:.1}{} (threshold: {:.1}{})",
            snapshot.kind, severity, snapshot.value, unit, threshold, unit,
        );

        vec![AlertEvent {
            timestamp: Utc::now(),
            metric: snapshot.kind,
            severity,
            message,
            value: snapshot.value,
            threshold,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_rules() -> Vec<ThresholdRule> {
        vec![ThresholdRule {
            metric: MetricKind::CpuUsage,
            warning: 80.0,
            critical: 95.0,
        }]
    }

    fn snap(kind: MetricKind, value: f64) -> MetricSnapshot {
        MetricSnapshot::new(kind, value)
    }

    #[test]
    fn normal_to_warning_emits_event() {
        let mut engine = AlertEngine::new(test_rules());
        let events = engine.evaluate(&snap(MetricKind::CpuUsage, 82.0));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].severity, AlertSeverity::Warning);
    }

    #[test]
    fn normal_to_critical_emits_critical() {
        let mut engine = AlertEngine::new(test_rules());
        let events = engine.evaluate(&snap(MetricKind::CpuUsage, 97.0));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].severity, AlertSeverity::Critical);
    }

    #[test]
    fn steady_warning_produces_no_event() {
        let mut engine = AlertEngine::new(test_rules());
        engine.evaluate(&snap(MetricKind::CpuUsage, 82.0));
        let events = engine.evaluate(&snap(MetricKind::CpuUsage, 85.0));
        assert!(events.is_empty());
    }

    #[test]
    fn warning_to_critical_emits_critical() {
        let mut engine = AlertEngine::new(test_rules());
        engine.evaluate(&snap(MetricKind::CpuUsage, 82.0));
        let events = engine.evaluate(&snap(MetricKind::CpuUsage, 96.0));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].severity, AlertSeverity::Critical);
    }

    #[test]
    fn warning_to_normal_emits_recovered() {
        let mut engine = AlertEngine::new(test_rules());
        engine.evaluate(&snap(MetricKind::CpuUsage, 82.0));
        let events = engine.evaluate(&snap(MetricKind::CpuUsage, 50.0));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].severity, AlertSeverity::Recovered);
    }

    #[test]
    fn no_rule_means_no_event() {
        let mut engine = AlertEngine::new(vec![]);
        let events = engine.evaluate(&snap(MetricKind::CpuUsage, 95.0));
        assert!(events.is_empty());
    }

    #[test]
    fn first_normal_reading_produces_no_event() {
        let mut engine = AlertEngine::new(test_rules());
        let events = engine.evaluate(&snap(MetricKind::CpuUsage, 50.0));
        assert!(events.is_empty());
    }
}
