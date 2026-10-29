//! Network throughput collector.
//!
//! Reads the cumulative byte counters published in `/proc/net/dev` and reports
//! the aggregate throughput in MB/s between two consecutive collection cycles.
//!
//! `/proc/net/dev` is a single small file that the kernel regenerates on read,
//! so one read per cycle yields the counters of every interface at once. The
//! per-interface alternative under `/sys/class/net/<iface>/statistics/` would
//! cost one syscall per counter per interface, which is strictly worse for the
//! same information.
//!
//! Only the aggregate of eligible interfaces is reported: packets travelling
//! from a physical interface into a container bridge and then into a veth pair
//! are counted at every hop, so the virtual hops must be excluded or the same
//! byte would be counted several times.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;
use tracing::debug;

use crate::collectors::{Collector, CollectorError};
use crate::metrics::{MetricKind, MetricSnapshot};

/// Default counter source for Linux network interfaces.
pub const DEFAULT_PROC_NET_DEV: &str = "/proc/net/dev";

/// Interface name prefixes excluded from the aggregate.
///
/// The loopback never leaves the host; `docker`, `veth`, `br-` and `virbr` are
/// container and bridge hops that duplicate traffic already seen on the
/// physical interface; `tailscale` and `tun` are tunnel hops with the same
/// duplication problem.
const EXCLUDED_PREFIXES: [&str; 7] = ["lo", "docker", "veth", "br-", "virbr", "tailscale", "tun"];

/// Cumulative byte counters for one interface, as reported by `/proc/net/dev`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterfaceCounters {
    /// Total bytes received since boot.
    pub rx_bytes: u64,
    /// Total bytes transmitted since boot.
    pub tx_bytes: u64,
}

impl InterfaceCounters {
    /// Parse one `/proc/net/dev` data line into an interface name and counters.
    ///
    /// Data lines have the shape `iface: rx_bytes rx_packets ... tx_bytes ...`
    /// with sixteen counters per direction. Header lines never contain a colon
    /// and are therefore rejected by the split, which is all the discrimination
    /// the format needs.
    fn parse(line: &str) -> Option<(String, InterfaceCounters)> {
        let (name, counters) = line.split_once(':')?;
        let name = name.trim();
        if name.is_empty() {
            return None;
        }

        let mut fields = counters.split_whitespace();
        let rx_bytes = fields.next()?.parse::<u64>().ok()?;
        // `tx_bytes` is the ninth counter of the line: eight receive counters
        // precede it, and `rx_bytes` already consumed the first one.
        let tx_bytes = fields.nth(7)?.parse::<u64>().ok()?;

        Some((name.to_string(), InterfaceCounters { rx_bytes, tx_bytes }))
    }
}

/// State kept between collection cycles.
///
/// The first reading only establishes a baseline: byte counters are cumulative,
/// so no rate exists until a second reading pins down a time delta.
#[derive(Debug)]
struct NetworkState {
    previous: Option<HashMap<String, InterfaceCounters>>,
    previous_at: Option<Instant>,
}

/// Collects aggregate network throughput from Linux `/proc/net/dev`.
///
/// The first `collect()` call establishes a baseline and returns an
/// `Unavailable` error. Subsequent calls report the sum of received and
/// transmitted megabytes per second across eligible interfaces.
///
/// Uses `tokio::task::spawn_blocking` for the filesystem read and
/// `std::sync::Mutex` for interior mutability of the baseline between reads.
/// The mutex is held for microseconds: no contention risk.
///
/// # Ownership Rules
///
/// This collector only gathers data. It does NOT evaluate alerts, send
/// notifications, or execute remediation.
pub struct NetworkCollector {
    path: PathBuf,
    state: Mutex<NetworkState>,
}

impl NetworkCollector {
    /// Create a collector that reads from the default `/proc/net/dev`.
    pub fn new() -> Self {
        Self::with_path(PathBuf::from(DEFAULT_PROC_NET_DEV))
    }

    /// Create a collector that reads counters from a custom path.
    ///
    /// Useful for testing with mock `/proc/net/dev` content.
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(NetworkState {
                previous: None,
                previous_at: None,
            }),
        }
    }

    /// Whether an interface contributes to the aggregate.
    fn is_eligible(name: &str) -> bool {
        !EXCLUDED_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
    }

    /// Parse every eligible interface from `/proc/net/dev` content.
    fn parse_interfaces(content: &str) -> HashMap<String, InterfaceCounters> {
        content
            .lines()
            .filter_map(InterfaceCounters::parse)
            .filter(|(name, _)| Self::is_eligible(name))
            .collect()
    }

    /// Convert two counter snapshots and a time delta into megabytes per second.
    ///
    /// Returns `None` when no time elapsed or when the two readings share no
    /// interface. Counters are cumulative and expected to grow; an interface
    /// that reports a lower value than before was reset, which yields zero
    /// growth instead of an underflow.
    fn throughput_mb_per_second(
        previous: &HashMap<String, InterfaceCounters>,
        current: &HashMap<String, InterfaceCounters>,
        elapsed_secs: f64,
    ) -> Option<f64> {
        if elapsed_secs <= 0.0 {
            return None;
        }

        let mut total_bytes: u64 = 0;
        let mut common = 0usize;

        for (name, curr) in current {
            let Some(prev) = previous.get(name) else {
                continue;
            };
            common += 1;
            total_bytes += curr.rx_bytes.saturating_sub(prev.rx_bytes);
            total_bytes += curr.tx_bytes.saturating_sub(prev.tx_bytes);
        }

        if common == 0 {
            return None;
        }

        Some((total_bytes as f64 / elapsed_secs) / (1024.0 * 1024.0))
    }
}

impl Default for NetworkCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Collector for NetworkCollector {
    fn name(&self) -> &str {
        "network"
    }

    async fn collect(&self) -> Result<MetricSnapshot, CollectorError> {
        let path = self.path.clone();

        // Read /proc/net/dev on a blocking thread — filesystem reads must not
        // block the async runtime.
        let raw = tokio::task::spawn_blocking(move || std::fs::read_to_string(&path))
            .await
            .map_err(|e| CollectorError::Unavailable {
                name: "network".to_string(),
                reason: format!("spawn_blocking failed: {}", e),
            })?
            .map_err(|e| CollectorError::ReadFailed {
                metric: "network_throughput".to_string(),
                source: e,
            })?;

        let current = Self::parse_interfaces(&raw);

        if current.is_empty() {
            return Err(CollectorError::ParseFailed {
                metric: "network_throughput".to_string(),
                details: "no eligible interface found in /proc/net/dev".to_string(),
            });
        }

        let mut state = self.state.lock().map_err(|e| CollectorError::Unavailable {
            name: "network".to_string(),
            reason: format!("state lock poisoned: {}", e),
        })?;

        match (state.previous.as_ref(), state.previous_at) {
            (Some(previous), Some(previous_at)) => {
                let elapsed_secs = previous_at.elapsed().as_secs_f64();
                let throughput = Self::throughput_mb_per_second(previous, &current, elapsed_secs)
                    .ok_or_else(|| CollectorError::Unavailable {
                    name: "network".to_string(),
                    reason: "no measurable delta between consecutive readings".to_string(),
                })?;

                debug!(
                    metric = %MetricKind::NetworkThroughput,
                    elapsed_secs = elapsed_secs,
                    interfaces = current.len(),
                    mb_per_second = throughput,
                    "Network throughput calculated"
                );

                state.previous = Some(current);
                state.previous_at = Some(Instant::now());

                Ok(
                    MetricSnapshot::new(MetricKind::NetworkThroughput, throughput)
                        .with_label("source", "proc_net_dev")
                        .with_label("scope", "aggregate_physical"),
                )
            }
            _ => {
                // First reading — no baseline to calculate a delta from. Store
                // it and report unavailable; the orchestrator logs and retries
                // on the next cycle.
                state.previous = Some(current);
                state.previous_at = Some(Instant::now());
                Err(CollectorError::Unavailable {
                    name: "network".to_string(),
                    reason: "warming up — first reading established baseline".to_string(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `/proc/net/dev` sample: two header lines and four interfaces.
    const SAMPLE: &str = concat!(
        "Inter-|   Receive                                                |  Transmit\n",
        " face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n",
        "    lo: 1234567    8901    0    0    0     0          0         0  1234567    8901    0    0    0     0       0          0\n",
        "  eth0: 9876543   12345    0    0    0     0          0         0  1234567    8901    0    0    0     0       0          0\n",
        "docker0: 5000      50      0    0    0     0          0         0  6000       60      0    0    0     0       0          0\n",
        "veth1234: 100      1       0    0    0     0          0         0  200        2       0    0    0     0       0          0\n",
    );

    fn counters(rx: u64, tx: u64) -> InterfaceCounters {
        InterfaceCounters {
            rx_bytes: rx,
            tx_bytes: tx,
        }
    }

    #[test]
    fn parse_interface_line_reads_both_directions() {
        let line = "  eth0: 9876543   12345    0    0    0     0          0         0  1234567    8901    0    0    0     0       0          0";

        let (name, parsed) = InterfaceCounters::parse(line).expect("valid data line");

        assert_eq!(name, "eth0");
        assert_eq!(parsed.rx_bytes, 9_876_543);
        assert_eq!(parsed.tx_bytes, 1_234_567);
    }

    #[test]
    fn parse_rejects_header_lines() {
        assert!(
            InterfaceCounters::parse("Inter-|   Receive                   |  Transmit").is_none()
        );
        assert!(InterfaceCounters::parse(
            " face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets"
        )
        .is_none());
    }

    #[test]
    fn parse_rejects_truncated_line() {
        // A line with too few counters must not be read as a real interface.
        assert!(InterfaceCounters::parse("eth0: 100").is_none());
    }

    #[test]
    fn parse_interfaces_excludes_loopback_and_virtual_hops() {
        let parsed = NetworkCollector::parse_interfaces(SAMPLE);

        assert!(parsed.contains_key("eth0"), "physical interface kept");
        assert_eq!(parsed.len(), 1, "only the physical interface remains");
        assert!(!parsed.contains_key("lo"));
        assert!(!parsed.contains_key("docker0"));
        assert!(!parsed.contains_key("veth1234"));
    }

    #[test]
    fn throughput_sums_both_directions() {
        let mut previous = HashMap::new();
        previous.insert("eth0".to_string(), counters(0, 0));
        let mut current = HashMap::new();
        // 1 MiB received + 1 MiB transmitted in one second.
        current.insert("eth0".to_string(), counters(1_048_576, 1_048_576));

        let rate = NetworkCollector::throughput_mb_per_second(&previous, &current, 1.0);

        assert_eq!(rate, Some(2.0));
    }

    #[test]
    fn throughput_scales_with_elapsed_time() {
        let mut previous = HashMap::new();
        previous.insert("eth0".to_string(), counters(0, 0));
        let mut current = HashMap::new();
        current.insert("eth0".to_string(), counters(1_048_576, 0));

        // The same megabyte spread over four seconds is a quarter of the rate.
        let rate = NetworkCollector::throughput_mb_per_second(&previous, &current, 4.0);

        assert_eq!(rate, Some(0.25));
    }

    #[test]
    fn throughput_returns_none_without_elapsed_time() {
        let mut previous = HashMap::new();
        previous.insert("eth0".to_string(), counters(0, 0));
        let mut current = HashMap::new();
        current.insert("eth0".to_string(), counters(1_048_576, 0));

        assert_eq!(
            NetworkCollector::throughput_mb_per_second(&previous, &current, 0.0),
            None
        );
    }

    #[test]
    fn throughput_treats_a_counter_reset_as_zero_growth() {
        let mut previous = HashMap::new();
        previous.insert("eth0".to_string(), counters(5_000_000, 5_000_000));
        let mut current = HashMap::new();
        // The interface restarted and its cumulative counters came back at zero.
        current.insert("eth0".to_string(), counters(0, 0));

        let rate = NetworkCollector::throughput_mb_per_second(&previous, &current, 1.0);

        assert_eq!(rate, Some(0.0), "a reset must not underflow to a huge rate");
    }

    #[test]
    fn throughput_returns_none_without_common_interfaces() {
        let mut previous = HashMap::new();
        previous.insert("eth0".to_string(), counters(0, 0));
        let mut current = HashMap::new();
        current.insert("end0".to_string(), counters(1_048_576, 0));

        assert_eq!(
            NetworkCollector::throughput_mb_per_second(&previous, &current, 1.0),
            None
        );
    }
}
