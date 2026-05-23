pub mod engine;
pub mod rules;
pub mod state;

pub use engine::AlertEvaluator;
pub use rules::{ThresholdRule, ThresholdsConfig};
pub use state::{AlertSeverity, AlertState, AlertStateTracker};

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
}
