use async_trait::async_trait;
use tracing::info;

use crate::alerts::state::AlertSeverity;
use crate::alerts::AlertEvent;
use crate::metrics::MetricKind;
use crate::notifiers::{Notifier, NotifierError};
use crate::services::maintenance::{MaintenanceOutcome, MaintenanceStatus};

/// Discord embed color codes mapped to severity.
mod color {
    pub const WARNING: u32 = 0xF0B429; // Amber/yellow — attention without alarm
    pub const CRITICAL: u32 = 0xE5534B; // Red — immediate action required
    pub const RECOVERED: u32 = 0x30A14E; // Green — back to normal
    pub const SKIPPED: u32 = 0x808080; // Grey — nothing happened, by decision
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
            Self::DockerUnhealthyContainers => "🐳",
            Self::DockerDiskUsed => "📦",
            Self::PostgresProbeFailures => "🐘",
        }
    }
}

/// Dispatches alert events to Discord via webhook.
///
/// Constructs rich embeds with severity-based colors, clear titles,
/// and compact operational fields for at-a-glance readability.
///
/// Suppression is deliberately not this type's job. The Alert Engine emits an
/// event only when a metric changes state, and that transition deduplication
/// (see `AlertStateTracker`) is the single layer that prevents notification
/// spam. A notifier that also suppressed would be mutating alert state, which
/// the `Notifier` contract forbids, and it silently swallowed the `Recovered`
/// event that closes an alert.
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
}

impl DiscordNotifier {
    /// Create a new Discord notifier.
    ///
    /// `hostname` is included in alert embeds to identify which server
    /// generated the alert. Pass the machine's hostname.
    pub fn new(webhook_url: String, hostname: String) -> Self {
        Self {
            webhook_url,
            hostname,
            client: reqwest::Client::new(),
        }
    }

    /// Build the Discord webhook payload as a JSON value.
    ///
    /// Separated from `send()` for testability — payload construction
    /// can be verified without making HTTP calls.
    /// Icon, label and colour for a maintenance status.
    ///
    /// The match is exhaustive, so a status added later cannot be dispatched with
    /// whichever style happened to be written last.
    fn maintenance_style(status: MaintenanceStatus) -> (&'static str, &'static str, u32) {
        match status {
            MaintenanceStatus::Success => ("✅", "SUCCESS", color::RECOVERED),
            MaintenanceStatus::Failed => ("❌", "FAILED", color::CRITICAL),
            MaintenanceStatus::TimedOut => ("⏳", "TIMED OUT", color::WARNING),
            MaintenanceStatus::Skipped => ("⏭️", "SKIPPED", color::SKIPPED),
        }
    }

    /// Build the maintenance embed.
    ///
    /// `detail` arrives already bounded and redacted by the service that produced
    /// the outcome: a notifier formats, it does not investigate.
    fn build_maintenance_payload(
        outcome: &MaintenanceOutcome,
        hostname: &str,
    ) -> serde_json::Value {
        let (emoji, label, colour) = Self::maintenance_style(outcome.status);

        let mut fields = vec![
            serde_json::json!({
                "name": "Duration",
                "value": format!("{:.1}s", outcome.duration.as_secs_f64()),
                "inline": true
            }),
            serde_json::json!({
                "name": "Host",
                "value": hostname,
                "inline": true
            }),
        ];

        if let Some(code) = outcome.exit_code {
            fields.push(serde_json::json!({
                "name": "Exit code",
                "value": code.to_string(),
                "inline": true
            }));
        }

        let mut embed = serde_json::json!({
            "title": format!("{emoji} {} — {label}", outcome.task),
            "color": colour,
            "fields": fields,
            "footer": { "text": "🛠️ GuardianRS maintenance" }
        });

        // An empty description renders as a blank line in Discord, which reads as
        // a formatting bug rather than a successful run with nothing to say.
        if !outcome.detail.is_empty() {
            embed["description"] = serde_json::json!(outcome.detail);
        }

        serde_json::json!({ "embeds": [embed] })
    }

    /// POST one payload to the webhook.
    ///
    /// Shared by both dispatch paths so the retry and error handling cannot
    /// drift apart between alerts and maintenance.
    async fn dispatch(&self, payload: serde_json::Value) -> Result<(), NotifierError> {
        let response = self
            .client
            .post(&self.webhook_url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| NotifierError::SendFailed {
                reason: format!("Discord webhook request failed: {e}"),
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

    /// What this alert is about: the configured instance when the metric carries one,
    /// otherwise the host.
    ///
    /// Three databases share one metric kind, so without this the three alerts read
    /// identically and the operator cannot tell which one is on fire.
    fn subject<'a>(event: &'a AlertEvent, hostname: &'a str) -> &'a str {
        event
            .labels
            .iter()
            .find(|(name, _)| name == "instance")
            .map(|(_, value)| value.as_str())
            .unwrap_or(hostname)
    }

    fn build_payload(event: &AlertEvent, hostname: &str) -> serde_json::Value {
        let severity_display = event.severity.display();
        let color = event.severity.color();
        let unit = event.metric.unit();
        let metric_name = event.metric.display_name();
        let metric_icon = event.metric.icon();

        let title = format!(
            "{} {} {} — {}",
            severity_display.emoji,
            metric_name,
            severity_display.title_prefix,
            Self::subject(event, hostname)
        );

        let suffix = crate::metrics::unit_suffix(unit);
        let value_field = format!("{:.1}{suffix}", event.value);
        let threshold_field = if event.severity == AlertSeverity::Recovered {
            format!("recovered below {:.1}{suffix}", event.threshold)
        } else {
            format!("{:.1}{suffix}", event.threshold)
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
        let payload = Self::build_payload(event, &self.hostname);

        info!(notifier = "discord", metric = %event.metric, severity = %event.severity, "Dispatching alert to Discord");

        self.dispatch(payload).await
    }

    async fn send_maintenance(&self, outcome: &MaintenanceOutcome) -> Result<(), NotifierError> {
        let payload = Self::build_maintenance_payload(outcome, &self.hostname);

        info!(
            notifier = "discord",
            task = %outcome.task,
            status = %outcome.status,
            "Dispatching maintenance outcome to Discord"
        );

        self.dispatch(payload).await
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
            labels: Vec::new(),
        }
    }

    fn maintenance_outcome(
        status: MaintenanceStatus,
        exit_code: Option<i32>,
        detail: &str,
    ) -> MaintenanceOutcome {
        MaintenanceOutcome {
            task: "postgres_backup".to_string(),
            status,
            exit_code,
            duration: std::time::Duration::from_secs(12),
            detail: detail.to_string(),
        }
    }

    fn embed_for(outcome: &MaintenanceOutcome) -> serde_json::Value {
        DiscordNotifier::build_maintenance_payload(outcome, "orangepi")["embeds"][0].clone()
    }

    fn field<'a>(embed: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
        embed["fields"]
            .as_array()?
            .iter()
            .find(|field| field["name"] == name)
    }

    #[test]
    fn maintenance_payload_marks_a_failure() {
        let embed = embed_for(&maintenance_outcome(
            MaintenanceStatus::Failed,
            Some(1),
            "connection refused",
        ));

        let title = embed["title"].as_str().expect("title");
        assert!(title.contains("FAILED"), "title was {title}");
        assert!(title.contains("postgres_backup"), "title was {title}");
        assert_eq!(embed["color"], color::CRITICAL);
        assert_eq!(embed["description"], "connection refused");
    }

    #[test]
    fn maintenance_payload_marks_a_success() {
        let embed = embed_for(&maintenance_outcome(
            MaintenanceStatus::Success,
            Some(0),
            "",
        ));

        assert!(embed["title"].as_str().expect("title").contains("SUCCESS"));
        assert_eq!(embed["color"], color::RECOVERED);
    }

    /// A timeout is not a plain failure: the operator reading the channel needs to
    /// tell "it broke" from "it never finished".
    #[test]
    fn maintenance_payload_marks_a_timeout_distinctly() {
        let embed = embed_for(&maintenance_outcome(MaintenanceStatus::TimedOut, None, ""));

        assert!(embed["title"]
            .as_str()
            .expect("title")
            .contains("TIMED OUT"));
        assert_eq!(embed["color"], color::WARNING);
    }

    /// And a skip is neither: nothing happened, by decision.
    #[test]
    fn maintenance_payload_marks_a_skip_distinctly() {
        let embed = embed_for(&maintenance_outcome(MaintenanceStatus::Skipped, None, ""));

        assert!(embed["title"].as_str().expect("title").contains("SKIPPED"));
        assert_eq!(embed["color"], color::SKIPPED);
    }

    /// An empty description renders as a blank line, which reads as a formatting
    /// bug rather than a run with nothing to say.
    #[test]
    fn maintenance_payload_omits_an_empty_description() {
        let embed = embed_for(&maintenance_outcome(
            MaintenanceStatus::Success,
            Some(0),
            "",
        ));

        assert!(embed.get("description").is_none());
    }

    #[test]
    fn maintenance_payload_includes_the_exit_code_only_when_present() {
        let with_code = embed_for(&maintenance_outcome(MaintenanceStatus::Failed, Some(3), ""));
        let without_code = embed_for(&maintenance_outcome(MaintenanceStatus::TimedOut, None, ""));

        assert_eq!(
            field(&with_code, "Exit code").map(|field| field["value"].clone()),
            Some(serde_json::json!("3"))
        );
        assert!(field(&without_code, "Exit code").is_none());
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

    fn probe_event(
        labels: Vec<(String, String)>,
        value: f64,
        severity: AlertSeverity,
    ) -> AlertEvent {
        AlertEvent {
            timestamp: Utc::now(),
            metric: MetricKind::PostgresProbeFailures,
            severity,
            message: String::new(),
            value,
            threshold: 2.0,
            labels,
        }
    }

    /// Three databases share one metric kind. Without the instance in the title the
    /// three alerts read identically, which is what happened on the host: three
    /// "PostgreSQL Probe Failures Recovered" messages nobody could tell apart.
    #[test]
    fn the_title_names_the_instance_the_alert_is_about() {
        let event = probe_event(
            vec![
                ("database".to_string(), "web_2r_prod".to_string()),
                ("instance".to_string(), "ecommerce".to_string()),
            ],
            3.0,
            AlertSeverity::Critical,
        );

        let payload = DiscordNotifier::build_payload(&event, TEST_HOST);
        let title = payload["embeds"][0]["title"].as_str().unwrap_or_default();

        assert!(
            title.contains("ecommerce"),
            "the title names the project, not just the host: {title}"
        );
    }

    /// A metric with no labels keeps the old behaviour: the host is the subject.
    #[test]
    fn an_alert_without_labels_still_names_the_host() {
        let event = probe_event(Vec::new(), 80.0, AlertSeverity::Warning);

        let payload = DiscordNotifier::build_payload(&event, TEST_HOST);
        let title = payload["embeds"][0]["title"].as_str().unwrap_or_default();

        assert!(
            title.contains(TEST_HOST),
            "no labels means the host: {title}"
        );
    }

    /// The message path had this fixed and the embed path did not, so the same alert
    /// read "3.0 failures" in the log and "3.0failures" in Discord.
    #[test]
    fn a_word_unit_is_separated_from_the_number() {
        let event = probe_event(Vec::new(), 3.0, AlertSeverity::Critical);

        let payload = DiscordNotifier::build_payload(&event, TEST_HOST);
        let current = payload["embeds"][0]["fields"][0]["value"]
            .as_str()
            .unwrap_or_default();

        assert_eq!(current, "3.0 failures", "a word unit needs a space");
    }
}
