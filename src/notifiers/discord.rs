use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tracing::{error, info, warn};

use crate::alerts::state::AlertSeverity;
use crate::alerts::AlertEvent;
use crate::metrics::MetricKind;
use crate::notifiers::outbox::{
    self, decorate_payload, DeliveryOutbox, OutboxKey, OutboxPriority, DEFAULT_OUTBOX_CAPACITY,
};
use crate::notifiers::{Notifier, NotifierError};
use crate::services::digest::DigestReport;
use crate::services::maintenance::{tail_chars, MaintenanceOutcome, MaintenanceStatus};

/// Default TCP/TLS handshake deadline for Discord webhook calls (`I2.1`).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Default total HTTP request/response deadline for Discord webhook calls (`I2.1`).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum error body characters retained for logging (`I3.1`).
pub const DEFAULT_MAX_ERROR_BODY_CHARS: usize = 512;

/// Maximum `Retry-After` delay honored inline before spooling to the outbox.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(5);

/// Maximum raw bytes read from a failing HTTP response stream.
const MAX_ERROR_BODY_BYTES: usize = 4096;

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
    retry_delays: Vec<Duration>,
    outbox_path: Option<PathBuf>,
    outbox: Mutex<DeliveryOutbox>,
}

impl std::fmt::Debug for DiscordNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordNotifier")
            .field("webhook_url", &"***")
            .field("hostname", &self.hostname)
            .field("outbox_path", &self.outbox_path)
            .finish()
    }
}

enum DispatchFailure {
    Retryable {
        error: NotifierError,
        retry_after: Option<Duration>,
    },
    Permanent(NotifierError),
}

impl DiscordNotifier {
    /// Create a new Discord notifier with bounded HTTP deadlines (`I2.1`).
    ///
    /// `hostname` is included in alert embeds to identify which server
    /// generated the alert. Pass the machine's hostname.
    pub fn new(webhook_url: String, hostname: String) -> Self {
        let client = Self::build_client(DEFAULT_CONNECT_TIMEOUT, DEFAULT_REQUEST_TIMEOUT);
        Self {
            webhook_url,
            hostname,
            client,
            retry_delays: vec![Duration::from_secs(1), Duration::from_secs(2)],
            outbox_path: None,
            outbox: Mutex::new(DeliveryOutbox::with_capacity(DEFAULT_OUTBOX_CAPACITY)),
        }
    }

    /// Override connect and request deadlines (used by deterministic tests).
    pub fn with_timeouts(mut self, connect: Duration, request: Duration) -> Self {
        self.client = Self::build_client(connect, request);
        self
    }

    /// Override immediate backoff delays before spooling to the durable outbox.
    pub fn with_retry_delays(mut self, delays: Vec<Duration>) -> Self {
        self.retry_delays = delays;
        self
    }

    /// Override durable outbox capacity.
    pub fn with_outbox_capacity(self, capacity: usize) -> Self {
        if let Ok(mut guard) = self.outbox.lock() {
            guard.set_capacity(capacity);
        }
        self
    }

    /// Attach a durable disk outbox and restore any previously queued messages.
    pub fn with_outbox_path(mut self, path: PathBuf) -> Self {
        let capacity = self
            .outbox
            .lock()
            .map(|guard| guard.entries().len().max(DEFAULT_OUTBOX_CAPACITY))
            .unwrap_or(DEFAULT_OUTBOX_CAPACITY);

        match outbox::load(&path, capacity) {
            Ok(loaded) => {
                if !loaded.is_empty() {
                    info!(
                        pending = loaded.len(),
                        path = %path.display(),
                        "Restored pending Discord notifications from outbox"
                    );
                }
                if let Ok(mut guard) = self.outbox.lock() {
                    *guard = loaded;
                }
            }
            Err(reason) => {
                warn!(
                    reason = %reason,
                    path = %path.display(),
                    "Ignoring unreadable Discord outbox; starting with empty queue"
                );
            }
        }
        self.outbox_path = Some(path);
        self
    }

    fn build_client(connect: Duration, request: Duration) -> reqwest::Client {
        reqwest::Client::builder()
            .connect_timeout(connect)
            .timeout(request)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
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
            "timestamp": Utc::now().to_rfc3339(),
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

    /// Build the daily liveness digest embed (`I4.2`).
    fn build_digest_payload(digest: &DigestReport, hostname: &str) -> serde_json::Value {
        let (emoji, colour) = match digest.severity_rank() {
            2 => ("🚨", color::CRITICAL),
            1 => ("⚠️", color::WARNING),
            _ => ("🟢", color::RECOVERED),
        };

        let mut fields = vec![
            serde_json::json!({
                "name": "Nightly Backups",
                "value": digest.format_tasks(),
                "inline": false
            }),
            serde_json::json!({
                "name": "Collectors",
                "value": digest.format_collectors(),
                "inline": false
            }),
            serde_json::json!({
                "name": "Open Alerts",
                "value": digest.format_open_alerts(),
                "inline": false
            }),
            serde_json::json!({
                "name": "Host",
                "value": hostname,
                "inline": true
            }),
            serde_json::json!({
                "name": "Scheduled Slot",
                "value": digest.scheduled_slot,
                "inline": true
            }),
        ];

        if let Some(late) = digest.late_by {
            let mins = (late.as_secs() / 60).max(1);
            fields.push(serde_json::json!({
                "name": "Catch-up Notice",
                "value": format!(
                    "Delivered ~{mins}m late (daemon was offline at scheduled time)"
                ),
                "inline": false
            }));
        }

        serde_json::json!({
            "embeds": [{
                "title": format!("{emoji} Daily Digest — {} ({hostname})", digest.target_date),
                "color": colour,
                "timestamp": digest.generated_at.to_rfc3339(),
                "fields": fields,
                "footer": {
                    "text": format!("🛡️ GuardianRS liveness ({})", digest.local_offset)
                }
            }]
        })
    }

    /// Read at most `MAX_ERROR_BODY_BYTES` from a failed HTTP response and cap
    /// the formatted text to `DEFAULT_MAX_ERROR_BODY_CHARS`, scrubbing the
    /// webhook URL if echoed (`I3.1`).
    async fn read_bounded_error_body(&self, mut response: reqwest::Response) -> String {
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(buffer.len());
                    let take = chunk.len().min(remaining);
                    buffer.extend_from_slice(&chunk[..take]);
                    if buffer.len() >= MAX_ERROR_BODY_BYTES {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        if buffer.is_empty() {
            return "<no body>".to_string();
        }

        let raw = String::from_utf8_lossy(&buffer);
        let scrubbed = if self.webhook_url.is_empty() {
            raw.to_string()
        } else {
            raw.replace(&self.webhook_url, "***")
        };
        let folded = scrubbed.split_whitespace().collect::<Vec<_>>().join(" ");
        if folded.chars().count() <= DEFAULT_MAX_ERROR_BODY_CHARS {
            folded
        } else {
            let kept = tail_chars(&folded, DEFAULT_MAX_ERROR_BODY_CHARS);
            kept.to_string()
        }
    }

    fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
        let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
        let secs: f64 = value.trim().parse().ok()?;
        if !secs.is_finite() || secs <= 0.0 {
            return None;
        }
        Some(Duration::from_secs_f64(secs).min(MAX_RETRY_AFTER))
    }

    /// Execute a single HTTP POST attempt, stripping secret URLs from errors (`I3.1`).
    async fn dispatch_once(&self, payload: &serde_json::Value) -> Result<(), DispatchFailure> {
        let response = self
            .client
            .post(&self.webhook_url)
            .json(payload)
            .send()
            .await
            .map_err(|e| {
                // `reqwest::Error` embeds the full URL (including the secret
                // webhook token) in its `Display` output unless stripped (`I3.1`).
                let clean = e.without_url();
                DispatchFailure::Retryable {
                    error: NotifierError::SendFailed {
                        reason: format!("Discord webhook request failed: {clean}"),
                    },
                    retry_after: None,
                }
            })?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let retry_after = Self::parse_retry_after(response.headers());
        let code = status.as_u16();
        let body = self.read_bounded_error_body(response).await;
        let error = NotifierError::WebhookFailed { status: code, body };

        if code == 429 || status.is_server_error() {
            Err(DispatchFailure::Retryable { error, retry_after })
        } else {
            Err(DispatchFailure::Permanent(error))
        }
    }

    /// Attempt delivery with bounded inline retries (`I2.1`, `I2.2`).
    async fn dispatch_with_retries(
        &self,
        payload: &serde_json::Value,
    ) -> Result<(), DispatchFailure> {
        let mut attempt = 0usize;
        loop {
            match self.dispatch_once(payload).await {
                Ok(()) => return Ok(()),
                Err(DispatchFailure::Permanent(error)) => {
                    return Err(DispatchFailure::Permanent(error))
                }
                Err(DispatchFailure::Retryable { error, retry_after }) => {
                    let Some(backoff) = self.retry_delays.get(attempt).copied() else {
                        return Err(DispatchFailure::Retryable { error, retry_after });
                    };
                    let delay = retry_after.unwrap_or(backoff).min(MAX_RETRY_AFTER);
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn persist_outbox(&self) {
        let Some(path) = self.outbox_path.clone() else {
            return;
        };

        let snapshot = match self.outbox.lock() {
            Ok(guard) => guard.clone(),
            Err(error) => {
                warn!(%error, "Discord outbox lock poisoned during snapshot");
                return;
            }
        };

        let write_result =
            tokio::task::spawn_blocking(move || outbox::save(&path, &snapshot)).await;
        match write_result {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => warn!(reason = %reason, "Could not persist Discord outbox"),
            Err(error) => warn!(%error, "Outbox persistence task failed"),
        }
    }

    /// Deliver `payload`, superseding any existing queued entry for `key` or
    /// spooling to the durable outbox on transient transport failure (`I2.2`).
    pub async fn deliver_keyed(
        &self,
        key: OutboxKey,
        priority: OutboxPriority,
        payload: serde_json::Value,
    ) -> Result<(), NotifierError> {
        let (already_queued, has_backlog) = self
            .outbox
            .lock()
            .map(|guard| (guard.contains_key(&key), !guard.is_empty()))
            .unwrap_or((false, false));

        if already_queued {
            // Supersede first so we never send a stale transition ahead of the
            // newer state that just arrived for the same key.
            let dropped_before = self
                .outbox
                .lock()
                .map(|guard| guard.dropped_count)
                .unwrap_or(0);
            if let Ok(mut guard) = self.outbox.lock() {
                guard.upsert(key, priority, payload, Utc::now());
                if guard.dropped_count > dropped_before {
                    error!(
                        dropped_total = guard.dropped_count,
                        "Discord outbox capacity exceeded; oldest entry evicted"
                    );
                }
            }
            self.persist_outbox().await;
            return self.flush().await;
        }

        // Drain any older queued notifications first so FIFO order is preserved
        // when connectivity returns mid-cycle.
        if has_backlog {
            if let Err(flush_err) = self.flush().await {
                if let Ok(mut guard) = self.outbox.lock() {
                    let before = guard.dropped_count;
                    guard.upsert(key, priority, payload, Utc::now());
                    if guard.dropped_count > before {
                        error!(
                            dropped_total = guard.dropped_count,
                            "Discord outbox capacity exceeded; oldest entry evicted"
                        );
                    }
                }
                self.persist_outbox().await;
                return Err(flush_err);
            }
        }

        match self.dispatch_with_retries(&payload).await {
            Ok(()) => Ok(()),
            Err(DispatchFailure::Permanent(error)) => Err(error),
            Err(DispatchFailure::Retryable { error, .. }) => {
                if let Ok(mut guard) = self.outbox.lock() {
                    let before = guard.dropped_count;
                    guard.upsert(key, priority, payload, Utc::now());
                    if guard.dropped_count > before {
                        error!(
                            dropped_total = guard.dropped_count,
                            "Discord outbox capacity exceeded; oldest entry evicted"
                        );
                    }
                }
                self.persist_outbox().await;
                Err(error)
            }
        }
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
        let key = OutboxKey::Alert {
            metric: event.metric,
            labels: event.labels.clone(),
        };
        let priority = if event.severity == AlertSeverity::Recovered {
            OutboxPriority::Recovery
        } else {
            OutboxPriority::Active
        };

        info!(notifier = "discord", metric = %event.metric, severity = %event.severity, "Dispatching alert to Discord");

        self.deliver_keyed(key, priority, payload).await
    }

    async fn send_maintenance(&self, outcome: &MaintenanceOutcome) -> Result<(), NotifierError> {
        let payload = Self::build_maintenance_payload(outcome, &self.hostname);
        let key = OutboxKey::Maintenance {
            task: outcome.task.clone(),
        };

        info!(
            notifier = "discord",
            task = %outcome.task,
            status = %outcome.status,
            "Dispatching maintenance outcome to Discord"
        );

        self.deliver_keyed(key, OutboxPriority::Active, payload)
            .await
    }

    async fn send_digest(&self, digest: &DigestReport) -> Result<(), NotifierError> {
        let payload = Self::build_digest_payload(digest, &self.hostname);

        info!(
            notifier = "discord",
            date = %digest.target_date,
            late = digest.late_by.is_some(),
            "Dispatching daily liveness digest to Discord"
        );

        self.deliver_keyed(OutboxKey::Digest, OutboxPriority::Active, payload)
            .await
    }

    async fn flush(&self) -> Result<(), NotifierError> {
        let mut changed = false;
        let mut last_error: Option<NotifierError> = None;

        loop {
            let next = match self.outbox.lock() {
                Ok(guard) => guard
                    .peek_front()
                    .map(|(entry, dropped)| decorate_payload(entry, dropped)),
                Err(error) => {
                    return Err(NotifierError::Unhealthy {
                        name: "discord".to_string(),
                        reason: format!("outbox lock poisoned: {error}"),
                    });
                }
            };

            let Some(payload) = next else {
                break;
            };

            match self.dispatch_with_retries(&payload).await {
                Ok(()) => {
                    if let Ok(mut guard) = self.outbox.lock() {
                        guard.pop_front_delivered();
                    }
                    changed = true;
                }
                Err(DispatchFailure::Permanent(error)) => {
                    // Drop unrecoverable 4xx payloads rather than blocking the
                    // entire queue forever, logging loudly.
                    error!(%error, "Dropping permanently rejected Discord outbox entry");
                    if let Ok(mut guard) = self.outbox.lock() {
                        guard.pop_front_delivered();
                    }
                    changed = true;
                    last_error = Some(error);
                }
                Err(DispatchFailure::Retryable { error, .. }) => {
                    last_error = Some(error);
                    break;
                }
            }
        }

        if changed {
            self.persist_outbox().await;
        }

        match last_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
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

    // === Transport, deadlines, secret hygiene, and durable outbox tests ===

    #[tokio::test]
    async fn stalled_webhook_times_out_within_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local listener");
        let addr = listener.local_addr().expect("addr");

        // Accept the socket and hold it open without replying.
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.expect("accept");
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let notifier = DiscordNotifier::new(
            format!("http://{addr}/api/webhooks/1/secret-token"),
            TEST_HOST.to_string(),
        )
        .with_timeouts(Duration::from_millis(80), Duration::from_millis(120))
        .with_retry_delays(Vec::new());

        let event = make_event(AlertSeverity::Critical, 85.0, 80.0);
        let bounded = tokio::time::timeout(Duration::from_secs(2), notifier.send(&event)).await;

        server.abort();
        let result = bounded.expect("send must respect request_timeout rather than hanging");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn transport_failure_never_contains_webhook_url_or_token() {
        // Bind and immediately drop to obtain a guaranteed closed local port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);

        let secret_token = "SUPER_SECRET_WEBHOOK_TOKEN_987654";
        let url = format!("http://{addr}/api/webhooks/12345/{secret_token}");
        let notifier = DiscordNotifier::new(url.clone(), TEST_HOST.to_string())
            .with_timeouts(Duration::from_millis(80), Duration::from_millis(120))
            .with_retry_delays(Vec::new());

        let debug_repr = format!("{notifier:?}");
        assert!(!debug_repr.contains(secret_token));
        assert!(!debug_repr.contains(&url));

        let err = notifier
            .send(&make_event(AlertSeverity::Critical, 90.0, 80.0))
            .await
            .expect_err("closed port must fail");
        let rendered = err.to_string();

        assert!(
            !rendered.contains(secret_token) && !rendered.contains("/api/webhooks/12345"),
            "transport error leaked webhook URL/token: {rendered}"
        );
    }

    #[tokio::test]
    async fn webhook_error_body_is_bounded() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut req_buf = [0u8; 2048];
            let _ = socket.read(&mut req_buf).await;
            let huge_body = "X".repeat(3000);
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                huge_body.len(),
                huge_body
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });

        let notifier = DiscordNotifier::new(
            format!("http://{addr}/api/webhooks/1/token"),
            TEST_HOST.to_string(),
        )
        .with_retry_delays(Vec::new());

        let err = notifier
            .send(&make_event(AlertSeverity::Warning, 75.0, 70.0))
            .await
            .expect_err("400 must fail");
        let _ = server.await;

        match err {
            NotifierError::WebhookFailed { status, body } => {
                assert_eq!(status, 400);
                assert!(
                    body.chars().count() <= DEFAULT_MAX_ERROR_BODY_CHARS,
                    "error body exceeded cap: len={}",
                    body.chars().count()
                );
            }
            other => panic!("expected WebhookFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn outbox_supersedes_by_key_and_records_skipped_transitions() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let online = Arc::new(AtomicBool::new(false));
        let received_bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let online_srv = Arc::clone(&online);
        let bodies_srv = Arc::clone(&received_bodies);
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if online_srv.load(Ordering::SeqCst) {
                    if let Some((_, body)) = text.split_once("\r\n\r\n") {
                        bodies_srv.lock().expect("lock").push(body.to_string());
                    }
                    let _ = socket
                        .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                } else {
                    let _ = socket
                        .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndown")
                        .await;
                }
            }
        });

        let notifier = DiscordNotifier::new(
            format!("http://{addr}/api/webhooks/1/token"),
            TEST_HOST.to_string(),
        )
        .with_retry_delays(Vec::new());

        // Offline: Warning -> Critical -> Recovered on the same metric key.
        assert!(notifier
            .send(&make_event(AlertSeverity::Warning, 72.0, 70.0))
            .await
            .is_err());
        assert!(notifier
            .send(&make_event(AlertSeverity::Critical, 85.0, 80.0))
            .await
            .is_err());
        assert!(notifier
            .send(&make_event(AlertSeverity::Recovered, 60.0, 70.0))
            .await
            .is_err());

        // Restore connectivity and flush.
        online.store(true, Ordering::SeqCst);
        notifier.flush().await.expect("flush succeeds once online");
        server.abort();

        let delivered = received_bodies.lock().expect("lock");
        assert_eq!(
            delivered.len(),
            1,
            "supersession must deliver only the latest state for the key"
        );
        assert!(delivered[0].contains("Recovered"));
        assert!(
            delivered[0].contains("2 intermediate transition(s) superseded while offline"),
            "payload must note skipped transitions: {}",
            delivered[0]
        );
    }

    #[tokio::test]
    async fn outbox_persists_across_restart_and_enforces_capacity() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut dir = std::env::temp_dir();
        dir.push(format!("guardian-outbox-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("outbox.json");
        let _ = std::fs::remove_file(&path);

        // Closed port to force spooling on the first process.
        let closed_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let closed_addr = closed_listener.local_addr().expect("addr");
        drop(closed_listener);

        let first = DiscordNotifier::new(
            format!("http://{closed_addr}/api/webhooks/1/token"),
            TEST_HOST.to_string(),
        )
        .with_retry_delays(Vec::new())
        .with_outbox_path(path.clone())
        .with_outbox_capacity(2);

        // Queue 1 Recovery + 2 Active entries into a capacity-2 outbox -> Recovery is evicted!
        let mut rec_event = make_event(AlertSeverity::Recovered, 55.0, 70.0);
        rec_event.metric = MetricKind::CpuUsage;
        let _ = first.send(&rec_event).await;

        let mut crit_mem = make_event(AlertSeverity::Critical, 97.0, 95.0);
        crit_mem.metric = MetricKind::MemoryUsage;
        let _ = first.send(&crit_mem).await;

        let mut crit_disk = make_event(AlertSeverity::Critical, 93.0, 90.0);
        crit_disk.metric = MetricKind::DiskUsage;
        let _ = first.send(&crit_disk).await;
        drop(first);

        assert!(path.exists(), "outbox must be written to disk");

        // Second process comes up against a healthy webhook endpoint and flushes.
        let live_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let live_addr = live_listener.local_addr().expect("addr");
        let delivered_payloads: std::sync::Arc<Mutex<Vec<String>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&delivered_payloads);

        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = live_listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some((_, body)) = text.split_once("\r\n\r\n") {
                    sink.lock().expect("lock").push(body.to_string());
                }
                let _ = socket
                    .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
            }
        });

        let restarted = DiscordNotifier::new(
            format!("http://{live_addr}/api/webhooks/1/token"),
            TEST_HOST.to_string(),
        )
        .with_retry_delays(Vec::new())
        .with_outbox_capacity(2)
        .with_outbox_path(path.clone());

        restarted.flush().await.expect("restored outbox flushes");
        server.abort();

        let sent = delivered_payloads.lock().expect("lock");
        assert_eq!(sent.len(), 2, "both Critical alerts survived restart");
        assert!(sent[0].contains("Memory Usage"));
        assert!(
            sent[0].contains("1 queued notification(s) dropped due to outbox capacity"),
            "overflow note must be surfaced: {}",
            sent[0]
        );
        assert!(sent[1].contains("Disk Usage"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
