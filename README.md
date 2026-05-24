<div align="center">

# 🛡️ GuardianRS

**Lightweight infrastructure monitoring & alerting daemon for ARM Linux servers**

[![Rust](https://img.shields.io/badge/Rust-1.75%2B-000000?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Tokio](https://img.shields.io/badge/Tokio-async%20runtime-000000?style=flat-square)](https://tokio.rs/)
[![Platform](https://img.shields.io/badge/Platform-ARM64%20Linux-FF8C00?style=flat-square&logo=linux&logoColor=white)](https://www.arm.com/)
[![License](https://img.shields.io/badge/License-All%20Rights%20Reserved-red?style=flat-square)](./LICENSE)

[![CI](https://img.shields.io/badge/tests-16%20passing-brightgreen?style=flat-square)](./)
[![Clippy](https://img.shields.io/badge/clippy-clean-brightgreen?style=flat-square)](./)
[![fmt](https://img.shields.io/badge/cargo%20fmt-clean-brightgreen?style=flat-square)](./)

Built for the **Orange Pi 5 Plus** · Sends alerts to **Discord** via webhooks

</div>

---

## Architecture

```
Collectors ──→ MetricSnapshot ──→ Alert Engine ──→ AlertEvent ──→ Notifiers
```

| Subsystem | Responsibility | Owns | Does NOT own |
|-----------|---------------|------|--------------|
| **Collectors** | Gather infrastructure data | System reads, metric snapshots | Alert logic, transport, orchestration |
| **Alert Engine** | Evaluate thresholds & state | Rules, alert lifecycle, deduplication | Discord transport, infrastructure, remediation |
| **Notifiers** | Dispatch events externally | Message formatting, webhook transport, retry | Monitoring logic, thresholds, remediation |
| **Orchestrator** | Coordinate the loop | Runtime scheduling, service lifecycle | Metric internals, alert rules, notifier implementations |

Alerts follow a **state machine** with built-in deduplication — only state transitions produce events, so you never get spammed with repeated notifications for the same condition.

```
         ┌──────────┐
         │  Normal   │
         └────┬─────┘
              │ value ≥ warning
              ▼
         ┌──────────┐
         │  Warning  │
         └────┬─────┘
              │ value ≥ critical
              ▼
         ┌──────────┐
         │ Critical  │
         └────┬─────┘
              │ value < warning
              ▼
         ┌──────────┐
         │ Recovered │ ──→ Normal
         └──────────┘
```

---

## Tech Stack

| Layer | Technology |
|-------|------------|
| Language | ![Rust](https://img.shields.io/badge/Rust-stable-000000?style=flat-square&logo=rust&logoColor=white) |
| Async Runtime | ![Tokio](https://img.shields.io/badge/Tokio-1.x-000000?style=flat-square) |
| Error Handling | ![thiserror](https://img.shields.io/badge/thiserror-2.x-orange?style=flat-square) + ![anyhow](https://img.shields.io/badge/anyhow-1.x-orange?style=flat-square) |
| Configuration | ![serde](https://img.shields.io/badge/serde%20%2B%20YAML-config-blue?style=flat-square) |
| HTTP Client | ![reqwest](https://img.shields.io/badge/reqwest-0.12-blue?style=flat-square) |
| Logging | ![tracing](https://img.shields.io/badge/tracing-0.1-blue?style=flat-square) |
| Serialization | ![serde](https://img.shields.io/badge/serde-1.x-blue?style=flat-square) |
| Env Vars | ![dotenvy](https://img.shields.io/badge/dotenvy-0.15-green?style=flat-square) |

---

## MVP Scope

### Collectors

| Collector | Source | Metric |
|-----------|--------|--------|
| CPU | `/proc/stat` | Usage percentage |
| Memory | `/proc/meminfo` | Usage percentage |
| Disk | `statfs` on `/` | Usage percentage |
| Temperature | `/sys/class/thermal/thermal_zone0/temp` | SoC temperature (°C) |

### Thresholds

| Metric | WARNING | CRITICAL |
|--------|---------|----------|
| CPU | 80% | 95% |
| Memory | 85% | 95% |
| Disk | 80% | 90% |
| Temperature | 70°C | 80°C |

All thresholds are configurable via `configs/guardian.yaml`.

---

## Quick Start

### Clone

```bash
git clone https://github.com/Ponce1969/Guardian_Orangpi5.git
cd Guardian_Orangpi5
```

### Prerequisites

- Rust stable (1.75+)
- Linux ARM64 (Orange Pi / Raspberry Pi)

### Build

```bash
cargo build --release
```

### Configure

1. Create your `.env` from the example:

```bash
cp .env.example .env
```

2. Set your Discord webhook URL:

```bash
# .env (NEVER commit this file)
DISCORD_WEBHOOK_URL=https://discord.com/api/webhooks/your_webhook_here
```

3. Adjust thresholds if needed in `configs/guardian.yaml`.

### Run

```bash
cargo run --release
```

Or run the binary directly:

```bash
./target/release/guardian-rs
```

---

## Configuration

Configuration is split between **YAML** (thresholds, intervals) and **environment variables** (secrets). No webhooks, passwords, or tokens are ever stored in YAML or code.

```yaml
# configs/guardian.yaml
daemon:
  poll_interval_secs: 30

thresholds:
  cpu_warning: 80
  cpu_critical: 95
  memory_warning: 85
  memory_critical: 95
  disk_warning: 80
  disk_critical: 90
  temp_warning: 70
  temp_critical: 80

notification:
  discord:
    enabled: true
    webhook_url_env: DISCORD_WEBHOOK_URL   # env var name, not the URL itself
    min_interval_secs: 300                  # dedup cooldown between identical alerts
```

> **Security**: The `webhook_url_env` field stores the **name** of the environment variable, not the URL. The actual webhook URL is resolved at runtime from `.env` or system env vars and is excluded from version control via `.gitignore`.

---

## Project Structure

```
src/
├── main.rs              # Entry point, config loading, tracing setup
├── config.rs            # YAML + env var configuration
├── metrics/
│   ├── mod.rs
│   └── types.rs         # MetricSnapshot, MetricKind
├── collectors/
│   └── mod.rs           # Collector trait, CollectorError
├── alerts/
│   ├── mod.rs           # AlertEvent
│   ├── engine.rs        # AlertEvaluator trait, AlertEngine impl
│   ├── rules.rs         # ThresholdRule, rules_from_config()
│   └── state.rs         # AlertSeverity, AlertState, AlertStateTracker
├── notifiers/
│   └── mod.rs           # Notifier trait, NotifierError
└── services/
    ├── mod.rs
    └── orchestrator.rs  # collect → evaluate → dispatch loop
```

### Core Traits

| Trait | Method | Responsibility |
|-------|--------|----------------|
| `Collector` | `async fn collect(&self) → Result<MetricSnapshot, CollectorError>` | Read system metrics |
| `AlertEvaluator` | `fn evaluate(&mut self, snapshot) → Vec<AlertEvent>` | Evaluate thresholds + state transitions |
| `Notifier` | `async fn send(&self, event) → Result<(), NotifierError>` | Dispatch to external systems |

---

## Roadmap

### Phase 1 — MVP *(current)*

- [x] Core contracts and traits (Collector, AlertEvaluator, Notifier)
- [x] Alert state machine with deduplication
- [x] YAML + env configuration system
- [x] 16 unit tests passing
- [ ] Concrete collectors (CPU, Memory, Disk, Temperature)
- [ ] Discord notifier with embed formatting
- [ ] Log notifier (structured tracing)
- [ ] Wire everything in `main.rs`
- [ ] systemd unit file
- [ ] Cross-compilation for `aarch64-unknown-linux-gnu`

### Phase 2 — Extended Monitoring

- [ ] Docker container health monitoring
- [ ] PostgreSQL connectivity checks
- [ ] Network throughput collection

### Phase 3 — Observability

- [ ] Prometheus exporter
- [ ] REST API
- [ ] Web dashboard

---

## Validation

```bash
cargo fmt --check          # Format check
cargo clippy --all-targets  # Lint check
cargo test                  # Run 16 unit tests
```

---

## License

Private project. All rights reserved.