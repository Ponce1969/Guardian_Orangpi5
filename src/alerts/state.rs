use std::collections::HashMap;

use crate::metrics::{MetricKind, MetricSnapshot};

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

/// Label names that identify an independent resource series rather than
/// point-in-time telemetry.
///
/// Collectors attach dynamic measurements (`available_kb`, `available_bytes`,
/// `running`, per-category Docker gigabytes) to `MetricSnapshot.labels` for
/// diagnostic context. Including those in [`AlertKey`] would create a distinct
/// key on every cycle, defeating deduplication, suppressing `Recovered`, and
/// growing tracker memory without bound (`SPEC_SECURITY_HARDENING.md` I1.1).
pub const IDENTITY_LABELS: &[&str] = &["database", "host", "instance", "mount", "zone"];

/// What an alert is about: a metric kind plus the static identity labels that
/// tell one resource series from another.
///
/// Keying state by `MetricKind` alone was invisible with one PostgreSQL instance and
/// wrong with three: the second instance going critical produced no transition
/// because the first already had, and the first recovering emitted a `Recovered`
/// while the second was still down.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AlertKey {
    kind: MetricKind,
    /// Sorted at construction so two snapshots carrying the same labels in a
    /// different iteration order produce the same key.
    labels: Vec<(String, String)>,
}

impl AlertKey {
    /// The key for a metric with no labels: one state per kind.
    pub fn for_kind(kind: MetricKind) -> Self {
        Self {
            kind,
            labels: Vec::new(),
        }
    }

    /// The key a snapshot belongs to, keeping only static identity labels.
    pub fn from_snapshot(snapshot: &MetricSnapshot) -> Self {
        let mut labels: Vec<(String, String)> = snapshot
            .labels
            .iter()
            .filter(|(name, _)| IDENTITY_LABELS.contains(&name.as_str()))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        labels.sort();

        Self {
            kind: snapshot.kind,
            labels,
        }
    }

    /// The metric this key belongs to.
    pub fn kind(&self) -> MetricKind {
        self.kind
    }

    /// The labels that make this key distinct, in a stable order.
    pub fn labels(&self) -> &[(String, String)] {
        &self.labels
    }
}

impl From<MetricKind> for AlertKey {
    fn from(kind: MetricKind) -> Self {
        Self::for_kind(kind)
    }
}

/// Tracks the current alert state per metric kind.
///
/// The tracker owns the state. The AlertEngine queries it to detect
/// transitions and decides whether to emit an `AlertEvent`.
#[derive(Default)]
pub struct AlertStateTracker {
    states: HashMap<AlertKey, AlertState>,
}

impl AlertStateTracker {
    /// Create a new tracker with all metrics in Normal state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the current state for a metric. Returns Normal if never seen.
    pub fn get(&self, key: impl Into<AlertKey>) -> AlertState {
        self.states.get(&key.into()).copied().unwrap_or_default()
    }

    /// Number of currently tracked non-normal series.
    pub fn active_count(&self) -> usize {
        self.states.len()
    }

    /// All currently active (`Warning` or `Critical`) series in deterministic order.
    pub fn active_alerts(&self) -> Vec<(AlertKey, AlertState)> {
        let mut active: Vec<(AlertKey, AlertState)> = self
            .states
            .iter()
            .map(|(key, state)| (key.clone(), *state))
            .collect();
        active.sort_by(|(left, _), (right, _)| left.cmp(right));
        active
    }

    /// Transition to a new state. Returns `Some(old_state)` if the
    /// state actually changed, `None` if it remained the same.
    ///
    /// Healthy (`Normal`) series are evicted rather than retained so tracker
    /// memory stays `O(active alerts)` regardless of uptime (`I1.2`).
    pub fn transition(
        &mut self,
        key: impl Into<AlertKey>,
        new_state: AlertState,
    ) -> Option<AlertState> {
        let key = key.into();
        if new_state == AlertState::Normal {
            return self.states.remove(&key);
        }

        let old_state = self.states.insert(key, new_state);
        match old_state {
            // Same state — no transition.
            Some(prev) if prev == new_state => None,
            // Different state — real transition.
            Some(prev) => Some(prev),
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

    #[test]
    fn telemetry_labels_do_not_change_alert_key() {
        let first = MetricSnapshot::new(MetricKind::DiskUsage, 84.0)
            .with_label("mount", "/")
            .with_label("total_bytes", "100000")
            .with_label("available_bytes", "16000");
        let second = MetricSnapshot::new(MetricKind::DiskUsage, 85.0)
            .with_label("mount", "/")
            .with_label("total_bytes", "100000")
            .with_label("available_bytes", "15000");
        let other_mount = MetricSnapshot::new(MetricKind::DiskUsage, 85.0)
            .with_label("mount", "/var/backups")
            .with_label("available_bytes", "15000");

        let first_key = AlertKey::from_snapshot(&first);
        let second_key = AlertKey::from_snapshot(&second);
        let other_key = AlertKey::from_snapshot(&other_mount);

        assert_eq!(
            first_key, second_key,
            "volatile telemetry labels must not split one resource into distinct keys"
        );
        assert_eq!(
            first_key.labels(),
            &[("mount".to_string(), "/".to_string())]
        );
        assert_ne!(
            first_key, other_key,
            "static identity labels still separate distinct resources"
        );
    }

    #[test]
    fn normal_state_retains_zero_entries_in_tracker() {
        let mut tracker = AlertStateTracker::new();

        assert!(tracker
            .transition(MetricKind::MemoryUsage, AlertState::Normal)
            .is_none());
        assert_eq!(
            tracker.active_count(),
            0,
            "entering Normal from implicit Normal must not allocate a map entry"
        );

        assert_eq!(
            tracker.transition(MetricKind::MemoryUsage, AlertState::Warning),
            Some(AlertState::Normal)
        );
        assert_eq!(tracker.active_count(), 1);

        assert_eq!(
            tracker.transition(MetricKind::MemoryUsage, AlertState::Normal),
            Some(AlertState::Warning)
        );
        assert_eq!(
            tracker.active_count(),
            0,
            "returning to Normal must evict the entry from the tracker"
        );
    }
}
