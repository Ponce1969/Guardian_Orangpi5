use async_trait::async_trait;
use thiserror::Error;

use crate::alerts::AlertEvent;

/// Errors that can occur during notification dispatch.
#[derive(Debug, Error)]
pub enum NotifierError {
    #[error("Discord webhook returned status {status}: {body}")]
    WebhookFailed { status: u16, body: String },

    #[error("Notification dispatch failed: {reason}")]
    SendFailed { reason: String },

    #[error("Notifier '{name}' is unhealthy: {reason}")]
    Unhealthy { name: String, reason: String },
}

/// The Notifier contract.
///
/// A Notifier owns a single responsibility: dispatching alert events
/// to an external system.
///
/// # Ownership Rules
///
/// - Notifiers MUST NOT evaluate rules.
/// - Notifiers MUST NOT inspect infrastructure.
/// - Notifiers MUST NOT mutate alert state.
///
/// Notifiers only transport messages.
#[async_trait]
pub trait Notifier: Send + Sync {
    /// Human-readable name for this notifier (e.g., "discord", "log").
    fn name(&self) -> &str;

    /// Dispatch an alert event to the external system.
    ///
    /// Implementations must be safe to call concurrently and must not
    /// perform blocking I/O inside the async runtime. Network calls
    /// should use `reqwest` or equivalent async HTTP clients.
    async fn send(&self, event: &AlertEvent) -> Result<(), NotifierError>;
}
