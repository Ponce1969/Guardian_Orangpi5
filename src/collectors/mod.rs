pub mod cpu;
pub mod temperature;

pub use cpu::CpuCollector;
pub use temperature::TemperatureCollector;

use async_trait::async_trait;
use thiserror::Error;

use crate::metrics::MetricSnapshot;

/// Errors that can occur during metric collection.
///
/// Collectors MUST NOT panic on failure. They return a `CollectorError`
/// so the orchestrator can log it and continue the collection loop.
#[derive(Debug, Error)]
pub enum CollectorError {
    #[error("Failed to read {metric}: {source}")]
    ReadFailed {
        metric: String,
        source: std::io::Error,
    },

    #[error("Failed to parse {metric}: {details}")]
    ParseFailed { metric: String, details: String },

    #[error("Collector '{name}' temporarily unavailable: {reason}")]
    Unavailable { name: String, reason: String },
}

/// The Collector contract.
///
/// A Collector owns a single responsibility: gathering one kind of
/// infrastructure metric from the host system.
///
/// # Ownership Rules
///
/// - Collectors MUST NOT trigger alerts.
/// - Collectors MUST NOT send notifications.
/// - Collectors MUST NOT execute remediation.
/// - Collectors MUST NOT contain business rules.
/// - Collectors MUST NOT persist application state.
///
/// Collectors only gather data.
#[async_trait]
pub trait Collector: Send + Sync {
    /// Human-readable name for this collector (e.g., "cpu", "memory", "disk").
    fn name(&self) -> &str;

    /// Collect a metric snapshot from the host system.
    ///
    /// Implementations must be safe to call concurrently and must not
    /// perform blocking I/O inside the async runtime — use
    /// `tokio::task::spawn_blocking` for filesystem reads that
    /// cannot be made async.
    async fn collect(&self) -> Result<MetricSnapshot, CollectorError>;
}
