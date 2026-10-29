use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tracing::{info, warn};

use crate::alerts::state::AlertSeverity;
use crate::alerts::AlertEvent;
use crate::metrics::MetricKind;
use crate::notifiers::{Notifier, NotifierError};

/// Discord embed color codes mapped to severity.
mod color {
    pub const WARNING: u32 = 0xF0B429; // Amber/yellow — attention without alarm
    pub const CRITICAL: u32 = 0xE5534B; // Red — immediate action required
    pub const RECOVERED: u32 = 0x30A14E; // Green — back to normal
}

/// Emoji and title prefix for severity levels.
struct SeverityDisplay {
    emoji: &'static str,
    title_prefix: &'static str,
}

impl AlertSeverity {
    fn display(&self) -> SeverityDisplay {
        match self {
            Self::Warning => SeverityDisplay {
                emoji: "⚠️",
                title_prefix: "Warning",
            },
            Self::Critical => SeverityDisplay {
                emoji: "🚨",
                title_prefix: "Critical",
            },
            Self::Recovered => SeverityDisplay {
                emoji: "✅",
                title_prefix: "Recovered",
            },
        }
    }

    fn color(&self) -> u32 {
        match self {
            Self::Warning => color::WARNING,
            Self::Critical => color::CRITICAL,
            Self::Recovered => color::RECOVERED,
        }
    }
}

impl MetricKind {
    /// Map metric kind to a short icon for embed footers.
    fn icon(&self) -> &'static str {
        match self {
            Self::CpuUsage => "🖥️",
            Self::MemoryUsage => "🧠",
            Self::DiskUsage => "💾",
            Self::Temperature => "🌡️",
            Self::NetworkThroughput => "🌐",
        }
    }
}

/// Dispatches alert events to Discord via webhook.
///
/// Constructs rich embeds with severity-based colors, clear titles,
/// and compact operational fields for at-a-glance readability.
///
/// # Cooldown
///
/// After sending an alert for a metric, subsequent alerts for the same
/// metric are suppressed for `cooldown` duration. This prevents Discord
/// spam when a metric oscillates around a threshold (flapping).
///
/// **Critical alerts always bypass cooldown** — they represent genuine
/// emergencies that must be dispatched immediately.
///
/// The `LogNotifier` is not affected by cooldown; it always logs all events.
///
/// # Ownership Rules
///
/// - Notifiers MUST NOT evaluate rules.
/// - Notifiers MUST NOT inspect infrastructure.
/// - Notifiers MUST NOT mutate alert state.
///
/// Notifiers only transport messages.
pub struct DiscordNotifier {
    webhook_url: String,
    hostname: String,
    client: reqwest::Client,
    cooldown: Duration,
    last_sent: Mutex<HashMap<MetricKind, DateTime<Utc>>>,
}

impl DiscordNotifier {
    /// Create a new Discord notifier.
    ///
    /// `hostname` is included in alert embeds to identify which server
    /// generated the alert. Pass the machine's hostname.
    /// `cooldown` is the minimum duration between notifications for
    /// the same metric. Set to `Duration::ZERO` to disable cooldown.
    pub fn new(webhook_url: String, hostname: String, cooldown: Duration) -> Self {
        Self {
            webhook_url,
            hostname,
            client: reqwest::Client::new(),
            cooldown,
            last_sent: Mutex::new(HashMap::new()),
        }
    }

    /// Check whether an alert event should be suppressed due to cooldown.
    ///
    /// Returns `true` if the event should be sent, `false` if suppressed.
    /// Critical alerts always return `true` regardless of cooldown state.
    fn should_send(&self, event: &AlertEvent) -> bool {
        // Critical alerts always bypass cooldown
        if event.severity == AlertSeverity::Critical {
            return true;
        }

        if self.cooldown.is_zero() {
            return true;
        }

        // A poisoned cooldown map carries no invariant worth panicking for:
        // recovering may send one extra notification, which is the safe direction.
        let last_sent = self
            .last_sent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match last_sent.get(&event.metric) {
            Some(last_time) => {
                let elapsed = Utc::now() - *last_time;
                let elapsed_duration = elapsed.to_std().unwrap_or(Duration::ZERO);
                elapsed_duration >= self.cooldown
            }
            None => true,
        }
    }

    /// Record that an alert was sent for a metric, updating cooldown state.
    fn record_sent(&self, metric: MetricKind) {
        let mut last_sent = self
            .last_sent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        last_sent.insert(metric, Utc::now());
    }

    /// Build the Discord webhook payload as a JSON value.
    ///
    /// Separated from `send()` for testability — payload construction
    /// can be verified without making HTTP calls.
    fn build_payload(event: &AlertEvent, hostname: &str) -> serde_json::Value {
        let severity_display = event.severity.display();
        let color = event.severity.color();
        let unit = event.metric.unit();
        let metric_name = event.metric.display_name();
        let metric_icon = event.metric.icon();

        let title = format!(
            "{} {} {} — {}",
            severity_display.emoji, metric_name, severity_display.title_prefix, hostname
        );

        let value_field = format!("{:.1}{}", event.value, unit);
        let threshold_field = if event.severity == AlertSeverity::Recovered {
            format!("recovered below {:.1}{}", event.threshold, unit)
        } else {
            format!("{:.1}{}", event.threshold, unit)
        };

        serde_json::json!({
            "embeds": [{
                "title": title,
                "color": color,
                "timestamp": event.timestamp.to_rfc3339(),
                "fields": [
                    {
                        "name": "Current",
                        "value": value_field,
                        "inline": true
                    },
                    {
                        "name": "Threshold",
                        "value": threshold_field,
                        "inline": true
                    },
                    {
                        "name": "Host",
                        "value": hostname,
                        "inline": true
                    }
                ],
                "footer": {
                    "text": format!("{} GuardianRS", metric_icon)
                }
            }]
        })
    }
}

#[async_trait]
impl Notifier for DiscordNotifier {
    fn name(&self) -> &str {
        "discord"
    }

    async fn send(&self, event: &AlertEvent) -> Result<(), NotifierError> {
        if !self.should_send(event) {
            warn!(
                notifier = "discord",
                metric = %event.metric,
                severity = %event.severity,
                cooldown_secs = self.cooldown.as_secs(),
                "Alert suppressed by cooldown"
            );
            return Ok(());
        }

        let payload = Self::build_payload(event, &self.hostname);

        info!(notifier = "discord", metric = %event.metric, severity = %event.severity, "Dispatching alert to Discord");

        let response = self
            .client
            .post(&self.webhook_url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| NotifierError::SendFailed {
                reason: format!("Discord webhook request failed: {}", e),
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(NotifierError::WebhookFailed {
                status: status.as_u16(),
                body,
            });
        }

        self.record_sent(event.metric);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    const TEST_HOST: &str = "orangepi5";

    fn make_event(severity: AlertSeverity, value: f64, threshold: f64) -> AlertEvent {
        let unit = MetricKind::Temperature.unit();
        let message = format!(
            "temperature {} is {:.1}{} (threshold: {:.1}{})",
            severity, value, unit, threshold, unit
        );
        AlertEvent {
            timestamp: Utc::now(),
            metric: MetricKind::Temperature,
            severity,
            message,
            value,
            threshold,
        }
    }

    #[test]
    fn build_payload_warning_embed() {
        let event = make_event(AlertSeverity::Warning, 42.5, 40.0);
        let payload = DiscordNotifier::build_payload(&event, TEST_HOST);

        let embeds = payload.get("embeds").unwrap().as_array().unwrap();
        assert_eq!(embeds.len(), 1);

        let embed = &embeds[0];
        let title = embed["title"].as_str().unwrap();
        assert!(title.contains("⚠️"));
        assert!(title.contains("Temperature Warning"));
        assert!(title.contains(TEST_HOST));
        assert_eq!(embed["color"], 0xF0B429);
        assert!(embed.get("timestamp").is_some());

        let fields = embed["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0]["name"], "Current");
        assert_eq!(fields[1]["name"], "Threshold");
        assert_eq!(fields[2]["name"], "Host");

        // Verify value formatting
        assert_eq!(fields[0]["value"], "42.5°C");
        assert_eq!(fields[2]["value"], TEST_HOST);
    }

    #[test]
    fn build_payload_critical_embed() {
        let event = make_event(AlertSeverity::Critical, 85.0, 80.0);
        let payload = DiscordNotifier::build_payload(&event, TEST_HOST);
        let embed = &payload["embeds"][0];

        assert_eq!(embed["color"], 0xE5534B);
        let title = embed["title"].as_str().unwrap();
        assert!(title.contains("🚨"));
        assert!(title.contains("Temperature Critical"));
    }

    #[test]
    fn build_payload_recovered_embed() {
        let event = make_event(AlertSeverity::Recovered, 61.0, 70.0);
        let payload = DiscordNotifier::build_payload(&event, TEST_HOST);
        let embed = &payload["embeds"][0];

        assert_eq!(embed["color"], 0x30A14E);
        let title = embed["title"].as_str().unwrap();
        assert!(title.contains("✅"));
        assert!(title.contains("Temperature Recovered"));

        // Recovered threshold should show "recovered below 70.0°C"
        let fields = embed["fields"].as_array().unwrap();
        let threshold_value = fields[1]["value"].as_str().unwrap();
        assert!(threshold_value.contains("recovered below"));
        assert!(threshold_value.contains("70.0°C"));
    }

    #[test]
    fn severity_colors() {
        assert_eq!(AlertSeverity::Warning.color(), 0xF0B429);
        assert_eq!(AlertSeverity::Critical.color(), 0xE5534B);
        assert_eq!(AlertSeverity::Recovered.color(), 0x30A14E);
    }

    #[test]
    fn payload_includes_rfc3339_timestamp() {
        let event = make_event(AlertSeverity::Warning, 72.0, 70.0);
        let payload = DiscordNotifier::build_payload(&event, TEST_HOST);
        let timestamp = payload["embeds"][0]["timestamp"].as_str().unwrap();
        assert!(timestamp.contains('T'));
    }

    #[test]
    fn payload_includes_footer() {
        let event = make_event(AlertSeverity::Warning, 72.0, 70.0);
        let payload = DiscordNotifier::build_payload(&event, TEST_HOST);
        let footer = payload["embeds"][0]["footer"]["text"].as_str().unwrap();
        assert!(footer.contains("GuardianRS"));
        assert!(footer.contains("🌡️"));
    }

    // === Cooldown tests ===

    fn make_event_with_metric(
        metric: MetricKind,
        severity: AlertSeverity,
        value: f64,
        threshold: f64,
    ) -> AlertEvent {
        let unit = metric.unit();
        let message = format!(
            "{} {} is {:.1}{} (threshold: {:.1}{})",
            metric, severity, value, unit, threshold, unit,
        );
        AlertEvent {
            timestamp: Utc::now(),
            metric,
            severity,
            message,
            value,
            threshold,
        }
    }

    #[test]
    fn cooldown_zero_always_sends() {
        let notifier = DiscordNotifier::new(
            "https://example.com".to_string(),
            TEST_HOST.to_string(),
            Duration::ZERO,
        );
        let event = make_event(AlertSeverity::Warning, 72.0, 70.0);
        assert!(notifier.should_send(&event));
    }

    #[test]
    fn cooldown_first_alert_always_sends() {
        let notifier = DiscordNotifier::new(
            "https://example.com".to_string(),
            TEST_HOST.to_string(),
            Duration::from_secs(300),
        );
        let event = make_event(AlertSeverity::Warning, 72.0, 70.0);
        assert!(notifier.should_send(&event));
    }

    #[test]
    fn cooldown_suppresses_same_metric_within_window() {
        let notifier = DiscordNotifier::new(
            "https://example.com".to_string(),
            TEST_HOST.to_string(),
            Duration::from_secs(300),
        );
        let event = make_event(AlertSeverity::Warning, 72.0, 70.0);
        // First send: allowed
        assert!(notifier.should_send(&event));
        notifier.record_sent(MetricKind::Temperature);
        // Second send within cooldown: suppressed
        assert!(!notifier.should_send(&event));
    }

    #[test]
    fn cooldown_critical_bypasses_cooldown() {
        let notifier = DiscordNotifier::new(
            "https://example.com".to_string(),
            TEST_HOST.to_string(),
            Duration::from_secs(300),
        );
        notifier.record_sent(MetricKind::Temperature);
        // Warning is suppressed
        let warning_check = make_event(AlertSeverity::Warning, 72.0, 70.0);
        assert!(!notifier.should_send(&warning_check));
        // Critical always sends regardless of cooldown
        let critical = make_event(AlertSeverity::Critical, 85.0, 80.0);
        assert!(notifier.should_send(&critical));
    }

    #[test]
    fn cooldown_independent_per_metric() {
        let notifier = DiscordNotifier::new(
            "https://example.com".to_string(),
            TEST_HOST.to_string(),
            Duration::from_secs(300),
        );
        let temp_event =
            make_event_with_metric(MetricKind::Temperature, AlertSeverity::Warning, 72.0, 70.0);
        let cpu_event =
            make_event_with_metric(MetricKind::CpuUsage, AlertSeverity::Warning, 85.0, 80.0);
        // Send temperature alert
        notifier.record_sent(MetricKind::Temperature);
        // Temperature suppressed within cooldown
        assert!(!notifier.should_send(&temp_event));
        // CPU still allowed — independent tracking
        assert!(notifier.should_send(&cpu_event));
    }

    #[test]
    fn cooldown_recovered_respects_cooldown() {
        let notifier = DiscordNotifier::new(
            "https://example.com".to_string(),
            TEST_HOST.to_string(),
            Duration::from_secs(300),
        );
        notifier.record_sent(MetricKind::Temperature);
        // Recovered also respects cooldown — prevents Warning→Recovered→Warning spam
        let recovered = make_event(AlertSeverity::Recovered, 60.0, 70.0);
        assert!(!notifier.should_send(&recovered));
    }

    #[test]
    fn record_sent_updates_last_sent_time() {
        let notifier = DiscordNotifier::new(
            "https://example.com".to_string(),
            TEST_HOST.to_string(),
            Duration::from_secs(300),
        );
        // Before record: no entry
        {
            let last_sent = notifier.last_sent.lock().expect("lock");
            assert!(!last_sent.contains_key(&MetricKind::Temperature));
        }
        notifier.record_sent(MetricKind::Temperature);
        // After record: entry exists
        {
            let last_sent = notifier.last_sent.lock().expect("lock");
            assert!(last_sent.contains_key(&MetricKind::Temperature));
        }
    }
}
