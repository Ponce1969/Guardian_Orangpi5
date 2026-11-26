//! Durable outbound delivery spool for transient webhook failures (`I2.2`).
//!
//! Architectural boundary (`AGENTS.md`, `SPEC_ARCHITECTURE.md`):
//! - `Alerts` decides *whether* an alert transition occurred.
//! - This module owns **outbound transport spooling** of already-formatted
//!   messages when the remote webhook endpoint is unreachable or rate-limited.
//!   It never evaluates thresholds and never mutates `AlertStateTracker`.

use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::alerts::state::IDENTITY_LABELS;
use crate::alerts::AlertEvent;
use crate::metrics::MetricKind;

const FORMAT_VERSION: u32 = 1;

/// Default maximum number of pending messages kept on disk.
pub const DEFAULT_OUTBOX_CAPACITY: usize = 64;

/// Logical identity used to supersede stale undelivered messages (`I2.2`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutboxKey {
    Alert {
        metric: MetricKind,
        labels: Vec<(String, String)>,
    },
    Maintenance {
        task: String,
    },
    Digest,
}

impl OutboxKey {
    /// Build an alert outbox key keeping only static identity labels (`I1.1`, `I2.2`).
    ///
    /// `AlertEvent.labels` retains volatile telemetry (`available_kb`, `images_gb`,
    /// etc.) for display and logging; stripping non-identity labels here ensures
    /// supersession matches `AlertKey` even when telemetry fluctuates across ticks.
    pub fn from_alert_event(event: &AlertEvent) -> Self {
        let mut labels: Vec<(String, String)> = event
            .labels
            .iter()
            .filter(|(name, _)| IDENTITY_LABELS.contains(&name.as_str()))
            .cloned()
            .collect();
        labels.sort();

        Self::Alert {
            metric: event.metric,
            labels,
        }
    }
}

/// Eviction priority when the outbox reaches its capacity bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxPriority {
    /// `Recovered` notifications: evicted first on overflow so active outages
    /// survive prolonged network partitions.
    Recovery,
    /// Active `Warning`/`Critical` alerts, failed/announced maintenance runs,
    /// and the daily liveness digest.
    Active,
}

/// One pending webhook delivery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxEntry {
    pub key: OutboxKey,
    pub priority: OutboxPriority,
    pub payload: serde_json::Value,
    #[serde(default)]
    pub superseded_count: usize,
    pub queued_at: DateTime<Utc>,
}

/// Persistent transport queue stored atomically under `/var/lib/guardianrs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryOutbox {
    #[serde(default = "current_version")]
    version: u32,
    #[serde(default)]
    pub dropped_count: usize,
    #[serde(default)]
    entries: Vec<OutboxEntry>,
    #[serde(skip, default = "default_capacity")]
    capacity: usize,
}

fn current_version() -> u32 {
    FORMAT_VERSION
}

fn default_capacity() -> usize {
    DEFAULT_OUTBOX_CAPACITY
}

impl Default for DeliveryOutbox {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_OUTBOX_CAPACITY)
    }
}

impl DeliveryOutbox {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            version: FORMAT_VERSION,
            dropped_count: 0,
            entries: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        while self.entries.len() > self.capacity {
            self.evict_one();
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn contains_key(&self, key: &OutboxKey) -> bool {
        self.entries.iter().any(|entry| &entry.key == key)
    }

    pub fn entries(&self) -> &[OutboxEntry] {
        &self.entries
    }

    /// Insert or supersede an entry for `key`.
    ///
    /// When an older undelivered message for `key` already exists, it is
    /// replaced in-place with the latest payload and its `superseded_count`
    /// increments so the operator learns how many intermediate transitions
    /// occurred while connectivity was down (`I2.2`).
    pub fn upsert(
        &mut self,
        key: OutboxKey,
        priority: OutboxPriority,
        payload: serde_json::Value,
        now: DateTime<Utc>,
    ) {
        if let Some(existing) = self.entries.iter_mut().find(|entry| entry.key == key) {
            existing.superseded_count = existing.superseded_count.saturating_add(1);
            existing.priority = priority;
            existing.payload = payload;
            existing.queued_at = now;
            return;
        }

        if self.entries.len() >= self.capacity {
            self.evict_one();
        }

        self.entries.push(OutboxEntry {
            key,
            priority,
            payload,
            superseded_count: 0,
            queued_at: now,
        });
    }

    /// Peek at the next pending entry in FIFO order together with any
    /// accumulated overflow count.
    pub fn peek_front(&self) -> Option<(&OutboxEntry, usize)> {
        self.entries
            .first()
            .map(|entry| (entry, self.dropped_count))
    }

    /// Acknowledge successful delivery of the front entry, clearing
    /// `dropped_count` once it has been reported.
    pub fn pop_front_delivered(&mut self) {
        if !self.entries.is_empty() {
            self.entries.remove(0);
            self.dropped_count = 0;
        }
    }

    /// Discard a permanently rejected (`4xx`) front entry without clearing
    /// `dropped_count`, so accumulated overflow notices still surface on the
    /// next successfully delivered message.
    pub fn pop_front_discarded(&mut self) {
        if !self.entries.is_empty() {
            self.entries.remove(0);
        }
    }

    fn evict_one(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let index = self
            .entries
            .iter()
            .position(|entry| entry.priority == OutboxPriority::Recovery)
            .unwrap_or(0);
        self.entries.remove(index);
        self.dropped_count = self.dropped_count.saturating_add(1);
    }
}

/// Decorate `base` payload with supersession or overflow context when needed.
pub fn decorate_payload_counts(
    base: &serde_json::Value,
    superseded_count: usize,
    dropped_count: usize,
) -> serde_json::Value {
    if superseded_count == 0 && dropped_count == 0 {
        return base.clone();
    }

    let mut notes = Vec::new();
    if superseded_count > 0 {
        notes.push(format!(
            "{} intermediate transition(s) superseded while offline",
            superseded_count
        ));
    }
    if dropped_count > 0 {
        notes.push(format!(
            "{} queued notification(s) dropped due to outbox capacity",
            dropped_count
        ));
    }
    let note_text = notes.join("; ");

    let mut payload = base.clone();
    if let Some(embed) = payload
        .get_mut("embeds")
        .and_then(serde_json::Value::as_array_mut)
        .and_then(|embeds| embeds.first_mut())
    {
        if let Some(fields) = embed
            .get_mut("fields")
            .and_then(serde_json::Value::as_array_mut)
        {
            fields.push(serde_json::json!({
                "name": "Delivery note",
                "value": note_text,
                "inline": false
            }));
        } else {
            embed["fields"] = serde_json::json!([{
                "name": "Delivery note",
                "value": note_text,
                "inline": false
            }]);
        }
    }
    payload
}

/// Decorate `entry.payload` with supersession or overflow context when needed.
pub fn decorate_payload(entry: &OutboxEntry, dropped_count: usize) -> serde_json::Value {
    decorate_payload_counts(&entry.payload, entry.superseded_count, dropped_count)
}

/// Load persisted outbox entries, treating a missing file as an empty queue.
pub fn load(path: &Path, capacity: usize) -> Result<DeliveryOutbox, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DeliveryOutbox::with_capacity(capacity))
        }
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };

    let mut outbox: DeliveryOutbox = serde_json::from_str(&raw)
        .map_err(|error| format!("{} is not a valid outbox: {error}", path.display()))?;

    if outbox.version > FORMAT_VERSION {
        return Err(format!(
            "{} was written by a newer build (version {} > {FORMAT_VERSION})",
            path.display(),
            outbox.version
        ));
    }

    outbox.set_capacity(capacity);
    Ok(outbox)
}

/// Persist the outbox atomically beside `path`.
pub fn save(path: &Path, outbox: &DeliveryOutbox) -> Result<(), String> {
    let body = serde_json::to_string_pretty(outbox)
        .map_err(|error| format!("cannot encode delivery outbox: {error}"))?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
    }

    let mut temp_os = path.as_os_str().to_os_string();
    temp_os.push(".tmp");
    let temp = std::path::PathBuf::from(temp_os);

    let mut file = std::fs::File::create(&temp)
        .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
    file.write_all(body.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
    file.sync_all()
        .map_err(|error| format!("cannot flush {}: {error}", temp.display()))?;
    drop(file);

    if std::fs::rename(&temp, path).is_ok() {
        sync_parent(path);
        return Ok(());
    }

    let _ = std::fs::remove_file(path);
    std::fs::rename(&temp, path)
        .map_err(|error| format!("cannot replace {}: {error}", path.display()))?;
    sync_parent(path);
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        let dir = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if let Ok(directory) = std::fs::File::open(dir) {
            let _ = directory.sync_all();
        }
    }
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::state::AlertSeverity;

    #[test]
    fn outbox_key_from_alert_event_ignores_volatile_telemetry_labels() {
        let event_a = AlertEvent {
            timestamp: Utc::now(),
            metric: MetricKind::MemoryUsage,
            severity: AlertSeverity::Warning,
            message: "memory warning".to_string(),
            value: 86.0,
            threshold: 85.0,
            labels: vec![
                ("available_kb".to_string(), "512000".to_string()),
                ("instance".to_string(), "orangepi".to_string()),
                ("mount".to_string(), "/".to_string()),
            ],
        };

        let event_b = AlertEvent {
            timestamp: Utc::now(),
            metric: MetricKind::MemoryUsage,
            severity: AlertSeverity::Critical,
            message: "memory critical".to_string(),
            value: 96.0,
            threshold: 95.0,
            labels: vec![
                ("mount".to_string(), "/".to_string()),
                ("available_kb".to_string(), "128000".to_string()),
                ("instance".to_string(), "orangepi".to_string()),
            ],
        };

        let key_a = OutboxKey::from_alert_event(&event_a);
        let key_b = OutboxKey::from_alert_event(&event_b);
        assert_eq!(
            key_a, key_b,
            "volatile available_kb must not break outbox supersession"
        );

        let mut different_mount = event_b;
        different_mount.labels = vec![
            ("mount".to_string(), "/var/backups".to_string()),
            ("available_kb".to_string(), "128000".to_string()),
            ("instance".to_string(), "orangepi".to_string()),
        ];
        assert_ne!(
            key_a,
            OutboxKey::from_alert_event(&different_mount),
            "static identity labels (instance, mount) must remain part of OutboxKey"
        );
    }

    #[test]
    fn pop_front_discarded_preserves_dropped_count_until_delivered() {
        let mut outbox = DeliveryOutbox::with_capacity(1);
        let now = Utc::now();

        outbox.upsert(
            OutboxKey::Maintenance {
                task: "first".to_string(),
            },
            OutboxPriority::Active,
            serde_json::json!({"embeds": [{}]}),
            now,
        );
        // Overflow capacity=1 -> `first` is evicted and `dropped_count` becomes 1.
        outbox.upsert(
            OutboxKey::Maintenance {
                task: "bad_400".to_string(),
            },
            OutboxPriority::Active,
            serde_json::json!({"embeds": [{}]}),
            now,
        );
        assert_eq!(outbox.dropped_count, 1);

        // Permanently rejected 4xx entry is discarded without clearing `dropped_count`.
        outbox.pop_front_discarded();
        assert!(outbox.is_empty());
        assert_eq!(
            outbox.dropped_count, 1,
            "discarding a 4xx entry must preserve dropped_count for the next valid delivery"
        );

        outbox.upsert(
            OutboxKey::Digest,
            OutboxPriority::Active,
            serde_json::json!({"embeds": [{}]}),
            now,
        );
        let (_, dropped_seen) = outbox.peek_front().expect("digest queued");
        assert_eq!(dropped_seen, 1);

        outbox.pop_front_delivered();
        assert_eq!(
            outbox.dropped_count, 0,
            "successful delivery clears dropped_count"
        );
    }
}
