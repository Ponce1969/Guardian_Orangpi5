use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::alerts::state::AlertSeverity;
use crate::alerts::AlertEvent;
use crate::metrics::MetricKind;
use crate::notifiers::outbox::{
    self, decorate_payload, DeliveryOutbox, OutboxKey, OutboxPriority, DEFAULT_OUTBOX_CAPACITY,
};
use crate::notifiers::{Notifier, NotifierError};
use crate::services::digest::DigestReport;
use crate::services::maintenance::{MaintenanceOutcome, MaintenanceStatus};

/// Default TCP/TLS handshake deadline for Discord webhook calls (`I2.1`).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Default total HTTP request/response deadline for Discord webhook calls (`I2.1`).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum error body characters retained for logging (`I3.1`).
pub const DEFAULT_MAX_ERROR_BODY_CHARS: usize = 512;

/// Maximum `Retry-After` delay honored inline before spooling to the outbox.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(2);

/// Maximum raw bytes read from a failing HTTP response stream.
const MAX_ERROR_BODY_BYTES: usize = 4096;

/// Maximum outbox entries flushed in a single tick so draining stays bounded.
const MAX_FLUSH_BATCH_PER_TICK: usize = 8;

/// Maximum wall time spent draining the outbox in a single tick.
const MAX_FLUSH_DURATION: Duration = Duration::from_secs(15);

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
    client: Result<reqwest::Client, String>,
    retry_delays: Vec<Duration>,
    outbox_capacity: usize,
    outbox_path: Option<PathBuf>,
    outbox: Mutex<DeliveryOutbox>,
}

impl std::fmt::Debug for DiscordNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordNotifier")
            .field("webhook_url", &"***")
            .field("hostname", &self.hostname)
            .field("outbox_capacity", &self.outbox_capacity)
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
            retry_delays: vec![Duration::from_millis(500), Duration::from_millis(1000)],
            outbox_capacity: DEFAULT_OUTBOX_CAPACITY,
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

    /// Override durable outbox capacity regardless of builder call order.
    pub fn with_outbox_capacity(mut self, capacity: usize) -> Self {
        let cap = capacity.max(1);
        self.outbox_capacity = cap;
        self.outbox.get_mut().set_capacity(cap);
        self
    }

    /// Attach a durable disk outbox and restore any previously queued messages.
    pub fn with_outbox_path(mut self, path: PathBuf) -> Self {
        let capacity = self.outbox_capacity;

        match outbox::load(&path, capacity) {
            Ok(loaded) => {
                if !loaded.is_empty() {
                    info!(
                        pending = loaded.len(),
                        path = %path.display(),
                        "Restored pending Discord notifications from outbox"
                    );
                }
                *self.outbox.get_mut() = loaded;
            }
            Err(reason) => {
                warn!(
                    reason = %reason,
                    path = %path.display(),
                    "Ignoring unreadable Discord outbox; starting with empty queue"
                );
                *self.outbox.get_mut() = DeliveryOutbox::with_capacity(capacity);
            }
        }
        self.outbox_path = Some(path);
        self
    }

    fn build_client(connect: Duration, request: Duration) -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .connect_timeout(connect)
            .timeout(request)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("failed to initialize HTTP client: {e}"))
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

    /// Scrub the webhook URL, `/api/webhooks/<id>/<token>` path, and raw `<token>`
    /// segment if echoed by a remote server or reverse proxy (`I3.1`).
    fn scrub_webhook_secrets(&self, raw: &str) -> String {
        if self.webhook_url.is_empty() {
            return raw.to_string();
        }

        let mut scrubbed = raw.replace(&self.webhook_url, "***");

        let after_scheme = self
            .webhook_url
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(self.webhook_url.as_str());

        if let Some(slash_idx) = after_scheme.find('/') {
            let path_and_query = &after_scheme[slash_idx..];
            if path_and_query.len() > 1 {
                scrubbed = scrubbed.replace(path_and_query, "***");
            }
            let path = path_and_query.split(['?', '#']).next().unwrap_or("");
            if path.len() > 1 {
                scrubbed = scrubbed.replace(path, "***");
            }
            if let Some(segments) = path.strip_prefix("/api/webhooks/") {
                let mut parts = segments.split('/');
                let _id = parts.next();
                if let Some(token) = parts.next() {
                    if !token.is_empty() {
                        scrubbed = scrubbed.replace(token, "***");
                    }
                }
            }
        }

        scrubbed
    }

    /// Read at most `MAX_ERROR_BODY_BYTES` from a failed HTTP response and cap
    /// the formatted text to `DEFAULT_MAX_ERROR_BODY_CHARS`, scrubbing the
    /// webhook URL, path, and token if echoed (`I3.1`).
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
        let scrubbed = self.scrub_webhook_secrets(&raw);
        let folded = scrubbed.split_whitespace().collect::<Vec<_>>().join(" ");
        if folded.chars().count() <= DEFAULT_MAX_ERROR_BODY_CHARS {
            folded
        } else {
            folded.chars().take(DEFAULT_MAX_ERROR_BODY_CHARS).collect()
        }
    }

    fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
        let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
        let secs: f64 = value.trim().parse().ok()?;
        if !secs.is_finite() || secs <= 0.0 {
            return None;
        }
        let capped = secs.min(MAX_RETRY_AFTER.as_secs_f64());
        Duration::try_from_secs_f64(capped).ok()
    }

    /// Execute a single HTTP POST attempt, stripping secret URLs from errors (`I3.1`).
    async fn dispatch_once(&self, payload: &serde_json::Value) -> Result<(), DispatchFailure> {
        let client = self.client.as_ref().map_err(|reason| {
            DispatchFailure::Permanent(NotifierError::SendFailed {
                reason: reason.clone(),
            })
        })?;
        let response = client
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

    async fn persist_outbox_snapshot(&self, snapshot: DeliveryOutbox) {
        let Some(path) = self.outbox_path.clone() else {
            return;
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
        let mut guard = self.outbox.lock().await;

        if !guard.is_empty() {
            let before = guard.dropped_count;
            guard.upsert(key, priority, payload, Utc::now());
            if guard.dropped_count > before {
                error!(
                    dropped_total = guard.dropped_count,
                    "Discord outbox capacity exceeded; oldest entry evicted"
                );
            }
            self.persist_outbox_snapshot(guard.clone()).await;
            return Err(NotifierError::SendFailed {
                reason: "Discord outbox has pending backlog; notification spooled for tick flush"
                    .to_string(),
            });
        }

        let pending_dropped = guard.dropped_count;
        let wire_payload = outbox::decorate_payload_counts(&payload, 0, pending_dropped);

        match self.dispatch_with_retries(&wire_payload).await {
            Ok(()) => {
                if pending_dropped > 0 {
                    guard.dropped_count = 0;
                    self.persist_outbox_snapshot(guard.clone()).await;
                }
                Ok(())
            }
            Err(DispatchFailure::Permanent(error)) => Err(error),
            Err(DispatchFailure::Retryable { error, .. }) => {
                let before = guard.dropped_count;
                guard.upsert(key, priority, payload, Utc::now());
                if guard.dropped_count > before {
                    error!(
                        dropped_total = guard.dropped_count,
                        "Discord outbox capacity exceeded; oldest entry evicted"
                    );
                }
                self.persist_outbox_snapshot(guard.clone()).await;
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
        let key = OutboxKey::from_alert_event(event);
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
        let mut guard = self.outbox.lock().await;
        let started = std::time::Instant::now();
        let mut processed: usize = 0;
        let mut last_error: Option<NotifierError> = None;

        while processed < MAX_FLUSH_BATCH_PER_TICK {
            let remaining = MAX_FLUSH_DURATION.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }

            let Some(payload) = guard
                .peek_front()
                .map(|(entry, dropped)| decorate_payload(entry, dropped))
            else {
                break;
            };

            match tokio::time::timeout(remaining, self.dispatch_with_retries(&payload)).await {
                Ok(Ok(())) => {
                    guard.pop_front_delivered();
                    self.persist_outbox_snapshot(guard.clone()).await;
                    processed += 1;
                }
                Ok(Err(DispatchFailure::Permanent(error))) => {
                    // Drop unrecoverable 4xx payloads rather than blocking the
                    // entire queue forever, logging loudly while preserving dropped_count.
                    error!(%error, "Dropping permanently rejected Discord outbox entry");
                    guard.pop_front_discarded();
                    self.persist_outbox_snapshot(guard.clone()).await;
                    last_error = Some(error);
                    processed += 1;
                }
                Ok(Err(DispatchFailure::Retryable { error, .. })) => {
                    last_error = Some(error);
                    break;
                }
                Err(_elapsed) => {
                    last_error = Some(NotifierError::SendFailed {
                        reason: "Discord outbox flush time budget reached; remaining entries deferred to next tick".to_string(),
                    });
                    break;
                }
            }
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

    async fn read_http_body(socket: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;

        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 2048];
        let mut header_end: Option<usize> = None;
        let mut content_length: usize = 0;

        loop {
            let n = match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);

            if header_end.is_none() {
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let end = pos + 4;
                    header_end = Some(end);
                    let headers = String::from_utf8_lossy(&buf[..pos]);
                    for line in headers.lines() {
                        if let Some((name, val)) = line.split_once(':') {
                            if name.trim().eq_ignore_ascii_case("content-length") {
                                content_length = val.trim().parse::<usize>().unwrap_or(0);
                            }
                        }
                    }
                }
            }

            if let Some(end) = header_end {
                if buf.len() >= end + content_length {
                    return String::from_utf8_lossy(&buf[end..end + content_length]).to_string();
                }
            }
        }

        if let Some(end) = header_end {
            String::from_utf8_lossy(&buf[end..]).to_string()
        } else {
            String::new()
        }
    }

    #[tokio::test]
    async fn webhook_error_body_is_bounded() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let secret_token = "SECRET_BODY_ECHO_TOKEN_42";

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let _ = read_http_body(&mut socket).await;
            let huge_body = format!(
                "/api/webhooks/1/{secret_token} raw={secret_token} {}",
                "X".repeat(5000)
            );
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                huge_body.len(),
                huge_body
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });

        let notifier = DiscordNotifier::new(
            format!("http://{addr}/api/webhooks/1/{secret_token}"),
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
                assert_eq!(
                    body.chars().count(),
                    DEFAULT_MAX_ERROR_BODY_CHARS,
                    "error body must truncate at head cap: len={}",
                    body.chars().count()
                );
                assert!(
                    body.starts_with("*** raw=***"),
                    "head truncation must keep scrubbed prefix: {body}"
                );
                assert!(
                    !body.contains(secret_token) && !body.contains("/api/webhooks/1"),
                    "error body leaked webhook path or token: {body}"
                );
            }
            other => panic!("expected WebhookFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn outbox_supersedes_by_key_and_records_skipped_transitions() {
        use std::sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc, Mutex as StdMutex,
        };
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let online = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(AtomicUsize::new(0));
        let received_bodies: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));

        let online_srv = Arc::clone(&online);
        let attempts_srv = Arc::clone(&attempts);
        let bodies_srv = Arc::clone(&received_bodies);
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                attempts_srv.fetch_add(1, Ordering::SeqCst);
                let body = read_http_body(&mut socket).await;
                if online_srv.load(Ordering::SeqCst) {
                    bodies_srv.lock().expect("lock").push(body);
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

        // Offline: Warning -> Critical -> Recovered on the same metric key,
        // even with fluctuating volatile telemetry labels (`available_kb`).
        let mut warn_ev = make_event(AlertSeverity::Warning, 72.0, 70.0);
        warn_ev.labels = vec![("available_kb".to_string(), "900".to_string())];
        assert!(notifier.send(&warn_ev).await.is_err());

        let mut crit_ev = make_event(AlertSeverity::Critical, 85.0, 80.0);
        crit_ev.labels = vec![("available_kb".to_string(), "400".to_string())];
        let backlog_err = notifier.send(&crit_ev).await.expect_err("spools");
        assert!(
            backlog_err.to_string().contains("pending backlog"),
            "subsequent alerts during backlog must spool immediately without HTTP retry: {backlog_err}"
        );

        let mut rec_ev = make_event(AlertSeverity::Recovered, 60.0, 70.0);
        rec_ev.labels = vec![("available_kb".to_string(), "1500".to_string())];
        assert!(notifier.send(&rec_ev).await.is_err());

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "backlog fast-spooling must perform zero network retries once outbox is non-empty"
        );

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
        use std::sync::Mutex as StdMutex;
        use tokio::io::AsyncWriteExt;

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
        assert_eq!(first.outbox.lock().await.capacity(), 2);

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

        // Second process comes up against a healthy webhook endpoint and flushes
        // (using the opposite builder order `.with_outbox_capacity(2).with_outbox_path(...)`).
        let live_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let live_addr = live_listener.local_addr().expect("addr");
        let delivered_payloads: std::sync::Arc<StdMutex<Vec<String>>> =
            std::sync::Arc::new(StdMutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&delivered_payloads);

        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = live_listener.accept().await {
                let body = read_http_body(&mut socket).await;
                sink.lock().expect("lock").push(body);
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
        assert_eq!(
            restarted.outbox.lock().await.capacity(),
            2,
            "with_outbox_capacity before with_outbox_path must also preserve capacity=2"
        );

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

    #[tokio::test]
    async fn flush_discard_on_4xx_preserves_dropped_count_for_next_delivery() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex as StdMutex,
        };
        use tokio::io::AsyncWriteExt;

        let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let closed_addr = closed.local_addr().expect("addr");
        drop(closed);

        let mut dir = std::env::temp_dir();
        dir.push(format!("guardian-outbox-4xx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("outbox.json");
        let _ = std::fs::remove_file(&path);

        // Overflow a capacity-2 outbox offline so dropped_count == 1 and 2 entries remain.
        let offline = DiscordNotifier::new(
            format!("http://{closed_addr}/api/webhooks/1/token"),
            TEST_HOST.to_string(),
        )
        .with_retry_delays(Vec::new())
        .with_outbox_capacity(2)
        .with_outbox_path(path.clone());

        let mut rec = make_event(AlertSeverity::Recovered, 50.0, 70.0);
        rec.metric = MetricKind::CpuUsage;
        let _ = offline.send(&rec).await;

        let mut first_active = make_event(AlertSeverity::Critical, 96.0, 95.0);
        first_active.metric = MetricKind::MemoryUsage;
        let _ = offline.send(&first_active).await;

        let mut second_active = make_event(AlertSeverity::Critical, 92.0, 90.0);
        second_active.metric = MetricKind::DiskUsage;
        let _ = offline.send(&second_active).await;
        drop(offline);

        // Live server rejects call #1 with permanent 400 and accepts call #2 with 204.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let calls = Arc::new(AtomicUsize::new(0));
        let delivered: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let calls_srv = Arc::clone(&calls);
        let delivered_srv = Arc::clone(&delivered);

        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let idx = calls_srv.fetch_add(1, Ordering::SeqCst);
                let body = read_http_body(&mut socket).await;
                if idx == 0 {
                    let _ = socket
                        .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad")
                        .await;
                } else {
                    delivered_srv.lock().expect("lock").push(body);
                    let _ = socket
                        .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                }
            }
        });

        let online = DiscordNotifier::new(
            format!("http://{addr}/api/webhooks/1/token"),
            TEST_HOST.to_string(),
        )
        .with_retry_delays(Vec::new())
        .with_outbox_capacity(2)
        .with_outbox_path(path);

        let flush_res = online.flush().await;
        assert!(flush_res.is_err(), "400 is surfaced as last_error");
        server.abort();

        let ok_bodies = delivered.lock().expect("lock");
        assert_eq!(ok_bodies.len(), 1);
        assert!(
            ok_bodies[0].contains("1 queued notification(s) dropped due to outbox capacity"),
            "dropped_count must survive a discarded 400 entry and reach the next 204 delivery: {}",
            ok_bodies[0]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn empty_outbox_surfaces_and_clears_dropped_count_after_4xx_discard() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex as StdMutex,
        };
        use tokio::io::AsyncWriteExt;

        let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let closed_addr = closed.local_addr().expect("addr");
        drop(closed);

        let mut dir = std::env::temp_dir();
        dir.push(format!("guardian-empty-dropped-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("outbox.json");
        let _ = std::fs::remove_file(&path);

        // Overflow a capacity-1 outbox offline so dropped_count == 1 and 1 entry remains.
        let offline = DiscordNotifier::new(
            format!("http://{closed_addr}/api/webhooks/1/token"),
            TEST_HOST.to_string(),
        )
        .with_retry_delays(Vec::new())
        .with_outbox_capacity(1)
        .with_outbox_path(path.clone());

        let mut evicted_ev = make_event(AlertSeverity::Recovered, 50.0, 70.0);
        evicted_ev.metric = MetricKind::CpuUsage;
        let _ = offline.send(&evicted_ev).await;

        let mut bad_ev = make_event(AlertSeverity::Critical, 96.0, 95.0);
        bad_ev.metric = MetricKind::MemoryUsage;
        let _ = offline.send(&bad_ev).await;
        drop(offline);

        // Live server rejects call #0 (flushed 400 entry) and accepts call #1 (fast-path send).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let calls = Arc::new(AtomicUsize::new(0));
        let delivered: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let calls_srv = Arc::clone(&calls);
        let delivered_srv = Arc::clone(&delivered);

        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let idx = calls_srv.fetch_add(1, Ordering::SeqCst);
                let body = read_http_body(&mut socket).await;
                if idx == 0 {
                    let _ = socket
                        .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad")
                        .await;
                } else {
                    delivered_srv.lock().expect("lock").push(body);
                    let _ = socket
                        .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                }
            }
        });

        let online = DiscordNotifier::new(
            format!("http://{addr}/api/webhooks/1/token"),
            TEST_HOST.to_string(),
        )
        .with_retry_delays(Vec::new())
        .with_outbox_capacity(1)
        .with_outbox_path(path.clone());

        // Flush discards the 400 entry, leaving outbox empty with dropped_count == 1.
        assert!(online.flush().await.is_err());
        assert!(online.outbox.lock().await.is_empty());
        assert_eq!(online.outbox.lock().await.dropped_count, 1);

        // Fast-path send on empty outbox surfaces dropped_count and clears it on 204.
        let mut fresh_ev = make_event(AlertSeverity::Critical, 92.0, 90.0);
        fresh_ev.metric = MetricKind::DiskUsage;
        online
            .send(&fresh_ev)
            .await
            .expect("fast-path send succeeds");
        server.abort();

        assert_eq!(online.outbox.lock().await.dropped_count, 0);
        assert_eq!(
            outbox::load(&path, 1).expect("reload").dropped_count,
            0,
            "cleared dropped_count must persist to disk"
        );

        let ok_bodies = delivered.lock().expect("lock");
        assert_eq!(ok_bodies.len(), 1);
        assert!(
            ok_bodies[0].contains("1 queued notification(s) dropped due to outbox capacity"),
            "empty-outbox fast path must surface pending dropped_count: {}",
            ok_bodies[0]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn flush_caps_per_tick_batch_and_persists_incrementally() {
        use std::sync::{Arc, Mutex as StdMutex};
        use tokio::io::AsyncWriteExt;

        let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let closed_addr = closed.local_addr().expect("addr");
        drop(closed);

        let mut dir = std::env::temp_dir();
        dir.push(format!("guardian-flush-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("outbox.json");
        let _ = std::fs::remove_file(&path);

        let offline = DiscordNotifier::new(
            format!("http://{closed_addr}/api/webhooks/1/token"),
            TEST_HOST.to_string(),
        )
        .with_retry_delays(Vec::new())
        .with_outbox_capacity(16)
        .with_outbox_path(path.clone());

        for i in 0..10 {
            let outcome = MaintenanceOutcome {
                task: format!("task_{i:02}"),
                status: MaintenanceStatus::Failed,
                exit_code: Some(1),
                duration: Duration::from_secs(1),
                detail: format!("failure {i}"),
            };
            let _ = offline.send_maintenance(&outcome).await;
        }
        drop(offline);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let delivered: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let delivered_srv = Arc::clone(&delivered);

        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let body = read_http_body(&mut socket).await;
                delivered_srv.lock().expect("lock").push(body);
                let _ = socket
                    .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
            }
        });

        let online = DiscordNotifier::new(
            format!("http://{addr}/api/webhooks/1/token"),
            TEST_HOST.to_string(),
        )
        .with_retry_delays(Vec::new())
        .with_outbox_capacity(16)
        .with_outbox_path(path.clone());

        // First flush delivers MAX_FLUSH_BATCH_PER_TICK (8), leaving 2 persisted on disk.
        online.flush().await.expect("first flush batch succeeds");
        assert_eq!(delivered.lock().expect("lock").len(), 8);
        assert_eq!(
            outbox::load(&path, 16)
                .expect("load after first flush")
                .len(),
            2
        );

        // Second flush drains the remaining 2 entries.
        online.flush().await.expect("second flush batch succeeds");
        server.abort();

        assert_eq!(delivered.lock().expect("lock").len(), 10);
        assert!(outbox::load(&path, 16)
            .expect("load after second flush")
            .is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_retry_after_clamps_huge_values_without_panicking() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("1e30"),
        );

        assert_eq!(
            DiscordNotifier::parse_retry_after(&headers),
            Some(MAX_RETRY_AFTER)
        );
    }
}
