use async_trait::async_trait;
use tracing::info;

use crate::alerts::state::AlertSeverity;
use crate::alerts::AlertEvent;
use crate::notifiers::{Notifier, NotifierError};

/// Discord embed color codes mapped to severity.
mod color {
    pub const WARNING: u32 = 0xFFA500; // Orange
    pub const CRITICAL: u32 = 0xFF0000; // Red
    pub const RECOVERED: u32 = 0x00FF00; // Green
}

/// Emoji prefixes for severity levels.
mod emoji {
    pub const WARNING: &str = "⚠️";
    pub const CRITICAL: &str = "🔴";
    pub const RECOVERED: &str = "✅";
}

/// Dispatches alert events to Discord via webhook.
///
/// Constructs rich embeds with severity-based colors, timestamps,
/// and field-based layout for operational readability.
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
    client: reqwest::Client,
}

impl DiscordNotifier {
    pub fn new(webhook_url: String) -> Self {
        Self {
            webhook_url,
            client: reqwest::Client::new(),
        }
    }

    fn severity_color(severity: AlertSeverity) -> u32 {
        match severity {
            AlertSeverity::Warning => color::WARNING,
            AlertSeverity::Critical => color::CRITICAL,
            AlertSeverity::Recovered => color::RECOVERED,
        }
    }

    fn severity_emoji(severity: AlertSeverity) -> &'static str {
        match severity {
            AlertSeverity::Warning => emoji::WARNING,
            AlertSeverity::Critical => emoji::CRITICAL,
            AlertSeverity::Recovered => emoji::RECOVERED,
        }
    }

    /// Build the Discord webhook payload as a JSON value.
    ///
    /// Separated from `send()` for testability — payload construction
    /// can be verified without making HTTP calls.
    fn build_payload(event: &AlertEvent) -> serde_json::Value {
        let emoji = Self::severity_emoji(event.severity);
        let color = Self::severity_color(event.severity);
        let unit = event.metric.unit();
        let title = format!("{} GuardianRS Alert", emoji);

        serde_json::json!({
            "embeds": [{
                "title": title,
                "description": event.message,
                "color": color,
                "timestamp": event.timestamp.to_rfc3339(),
                "fields": [
                    {
                        "name": "Metric",
                        "value": event.metric.to_string(),
                        "inline": true
                    },
                    {
                        "name": "Severity",
                        "value": event.severity.to_string(),
                        "inline": true
                    },
                    {
                        "name": "Value",
                        "value": format!("{:.1}{}", event.value, unit),
                        "inline": true
                    },
                    {
                        "name": "Threshold",
                        "value": format!("{:.1}{}", event.threshold, unit),
                        "inline": true
                    }
                ]
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
        let payload = Self::build_payload(event);

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

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::MetricKind;
    use chrono::Utc;

    fn make_event(severity: AlertSeverity, value: f64, threshold: f64) -> AlertEvent {
        let message = format!(
            "temperature {} is {:.1}°C (threshold: {:.1}°C)",
            severity, value, threshold
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
        let event = make_event(AlertSeverity::Warning, 72.5, 70.0);
        let payload = DiscordNotifier::build_payload(&event);

        // Verify top-level structure
        assert!(payload.get("embeds").is_some());
        let embeds = payload.get("embeds").unwrap().as_array().unwrap();
        assert_eq!(embeds.len(), 1);

        let embed = &embeds[0];
        assert_eq!(embed["title"], "⚠️ GuardianRS Alert");
        assert_eq!(embed["color"], 0xFFA500); // Orange for warning
        assert!(embed.get("timestamp").is_some());

        // Verify fields
        let fields = embed["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 4);
        assert_eq!(fields[0]["name"], "Metric");
        assert_eq!(fields[1]["name"], "Severity");
        assert_eq!(fields[2]["name"], "Value");
        assert_eq!(fields[3]["name"], "Threshold");
    }

    #[test]
    fn build_payload_critical_embed() {
        let event = make_event(AlertSeverity::Critical, 85.0, 80.0);
        let payload = DiscordNotifier::build_payload(&event);
        let embed = &payload["embeds"][0];

        assert_eq!(embed["color"], 0xFF0000); // Red for critical
        assert_eq!(embed["title"], "🔴 GuardianRS Alert");
    }

    #[test]
    fn build_payload_recovered_embed() {
        let event = make_event(AlertSeverity::Recovered, 45.0, 70.0);
        let payload = DiscordNotifier::build_payload(&event);
        let embed = &payload["embeds"][0];

        assert_eq!(embed["color"], 0x00FF00); // Green for recovered
        assert_eq!(embed["title"], "✅ GuardianRS Alert");
    }

    #[test]
    fn severity_color_mapping() {
        assert_eq!(
            DiscordNotifier::severity_color(AlertSeverity::Warning),
            0xFFA500
        );
        assert_eq!(
            DiscordNotifier::severity_color(AlertSeverity::Critical),
            0xFF0000
        );
        assert_eq!(
            DiscordNotifier::severity_color(AlertSeverity::Recovered),
            0x00FF00
        );
    }

    #[test]
    fn payload_includes_rfc3339_timestamp() {
        let event = make_event(AlertSeverity::Warning, 72.0, 70.0);
        let payload = DiscordNotifier::build_payload(&event);
        let timestamp = payload["embeds"][0]["timestamp"].as_str().unwrap();
        // RFC 3339 timestamps contain 'T' and end with '+00:00' or 'Z'
        assert!(timestamp.contains('T'));
    }
}
