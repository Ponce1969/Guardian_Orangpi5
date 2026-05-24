use async_trait::async_trait;
use tracing::warn;

use crate::alerts::AlertEvent;
use crate::notifiers::{Notifier, NotifierError};

/// Dispatches alert events to structured logs via the `tracing` framework.
///
/// This is the fallback notifier — it always succeeds because it only
/// writes to the local log stream. Use this alongside Discord so alerts
/// are visible even when the webhook endpoint is unavailable.
///
/// # Ownership Rules
///
/// - Notifiers MUST NOT evaluate rules.
/// - Notifiers MUST NOT inspect infrastructure.
/// - Notifiers MUST NOT mutate alert state.
///
/// Notifiers only transport messages.
pub struct LogNotifier;

impl LogNotifier {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LogNotifier {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Notifier for LogNotifier {
    fn name(&self) -> &str {
        "log"
    }

    async fn send(&self, event: &AlertEvent) -> Result<(), NotifierError> {
        warn!(
            metric = %event.metric,
            severity = %event.severity,
            value = event.value,
            threshold = event.threshold,
            message = %event.message,
            "Alert event"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::state::AlertSeverity;
    use crate::metrics::MetricKind;
    use chrono::Utc;

    #[tokio::test]
    async fn log_notifier_always_succeeds() {
        let notifier = LogNotifier::new();
        let event = AlertEvent {
            timestamp: Utc::now(),
            metric: MetricKind::Temperature,
            severity: AlertSeverity::Warning,
            message: "temperature WARNING is 72.0°C".to_string(),
            value: 72.0,
            threshold: 70.0,
        };

        let result = notifier.send(&event).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn log_notifier_name() {
        let notifier = LogNotifier::new();
        assert_eq!(notifier.name(), "log");
    }
}
