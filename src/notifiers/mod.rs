pub mod discord;
pub mod log;
pub mod outbox;

pub use discord::DiscordNotifier;
pub use log::LogNotifier;
pub use outbox::{DeliveryOutbox, OutboxEntry, OutboxKey, OutboxPriority};

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

    /// Dispatch a maintenance outcome to the external system.
    ///
    /// Required rather than defaulted. A default that quietly returned `Ok(())`
    /// would let a new notifier swallow a failed backup without anyone noticing,
    /// and a backup that fails silently is the one thing this subsystem must
    /// never do. Every implementation states what it does with the outcome, and
    /// the compiler enforces it.
    async fn send_maintenance(
        &self,
        outcome: &crate::services::maintenance::MaintenanceOutcome,
    ) -> Result<(), NotifierError>;

    /// Dispatch the daily human liveness digest (`I4.2`).
    ///
    /// Defaulted to `Ok(())` for lightweight test doubles that only record
    /// alerts or maintenance outcomes unless explicitly testing digests.
    async fn send_digest(
        &self,
        _digest: &crate::services::digest::DigestReport,
    ) -> Result<(), NotifierError> {
        Ok(())
    }

    /// Drain any queued outbound messages whose previous delivery failed due to
    /// transient network or remote endpoint errors (`I2.3`).
    ///
    /// Stateless local notifiers (`LogNotifier`) default to a no-op.
    async fn flush(&self) -> Result<(), NotifierError> {
        Ok(())
    }
}
