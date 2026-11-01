pub mod engine;
pub mod rules;
pub mod state;

pub use engine::AlertEngine;
pub use engine::AlertEvaluator;
pub use rules::{ThresholdRule, ThresholdsConfig};
pub use state::{AlertKey, AlertSeverity, AlertState, AlertStateTracker};

use crate::metrics::MetricKind;

/// An alert event produced by the Alert Engine when a state transition occurs.
///
/// The engine only emits events on state **transitions** — repeated
/// measurements that stay in the same severity produce no event.
/// This is the core deduplication mechanism.
#[derive(Debug, Clone)]
pub struct AlertEvent {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub metric: MetricKind,
    pub severity: AlertSeverity,
    pub message: String,
    pub value: f64,
    pub threshold: f64,
    /// The labels that identify the series this event is about, sorted.
    ///
    /// Without them a message says the metric failed and leaves the operator to
    /// work out which of three databases it was.
    pub labels: Vec<(String, String)>,
}
