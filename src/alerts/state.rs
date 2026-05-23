use std::collections::HashMap;

use crate::metrics::MetricKind;

/// The severity level of an alert event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlertSeverity {
    Warning,
    Critical,
    Recovered,
}

impl std::fmt::Display for AlertSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Warning => write!(f, "WARNING"),
            Self::Critical => write!(f, "CRITICAL"),
            Self::Recovered => write!(f, "RECOVERED"),
        }
    }
}

/// The operational state of a single metric within the alert lifecycle.
///
/// The lifecycle is:
/// ```text
/// Normal → Warning → Critical → Recovered → Normal
/// ```
///
/// State transitions are mandatory. Repeated measurements that stay
/// in the same state produce no event — this is the deduplication
/// mechanism that prevents notification spam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlertState {
    #[default]
    Normal,
    Warning,
    Critical,
}

/// Tracks the current alert state per metric kind.
///
/// The tracker owns the state. The AlertEngine queries it to detect
/// transitions and decides whether to emit an `AlertEvent`.
#[derive(Default)]
pub struct AlertStateTracker {
    states: HashMap<MetricKind, AlertState>,
}

impl AlertStateTracker {
    /// Create a new tracker with all metrics in Normal state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the current state for a metric. Returns Normal if never seen.
    pub fn get(&self, kind: MetricKind) -> AlertState {
        self.states.get(&kind).copied().unwrap_or_default()
    }

    /// Transition to a new state. Returns `Some(old_state)` if the
    /// state actually changed, `None` if it remained the same.
    ///
    /// Special case: transitioning from implicit Normal (never seen)
    /// to explicit Normal is NOT a transition — no event should fire.
    pub fn transition(&mut self, kind: MetricKind, new_state: AlertState) -> Option<AlertState> {
        let old_state = self.states.insert(kind, new_state);
        match old_state {
            // Same state — no transition.
            Some(prev) if prev == new_state => None,
            // Different state — real transition.
            Some(prev) => Some(prev),
            // First time seeing this metric.
            // Entering Normal from implicit Normal is not a transition.
            None if new_state == AlertState::Normal => None,
            // Entering Warning/Critical from implicit Normal IS a transition.
            None => Some(AlertState::Normal),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_normal() {
        let tracker = AlertStateTracker::new();
        assert_eq!(tracker.get(MetricKind::CpuUsage), AlertState::Normal);
    }

    #[test]
    fn transition_from_normal_to_warning_emits() {
        let mut tracker = AlertStateTracker::new();
        let old = tracker.transition(MetricKind::CpuUsage, AlertState::Warning);
        assert_eq!(old, Some(AlertState::Normal));
    }

    #[test]
    fn repeated_warning_does_not_emit() {
        let mut tracker = AlertStateTracker::new();
        tracker.transition(MetricKind::CpuUsage, AlertState::Warning);
        let old = tracker.transition(MetricKind::CpuUsage, AlertState::Warning);
        assert!(old.is_none());
    }

    #[test]
    fn warning_to_critical_emits() {
        let mut tracker = AlertStateTracker::new();
        tracker.transition(MetricKind::CpuUsage, AlertState::Warning);
        let old = tracker.transition(MetricKind::CpuUsage, AlertState::Critical);
        assert_eq!(old, Some(AlertState::Warning));
    }

    #[test]
    fn critical_to_warning_emits_recovery_not_recovered() {
        let mut tracker = AlertStateTracker::new();
        tracker.transition(MetricKind::CpuUsage, AlertState::Critical);
        let old = tracker.transition(MetricKind::CpuUsage, AlertState::Warning);
        assert_eq!(old, Some(AlertState::Critical));
    }

    #[test]
    fn independent_metrics_tracked_separately() {
        let mut tracker = AlertStateTracker::new();
        tracker.transition(MetricKind::CpuUsage, AlertState::Warning);
        assert_eq!(tracker.get(MetricKind::MemoryUsage), AlertState::Normal);
    }
}
