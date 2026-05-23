# GuardianRS

Lightweight infrastructure monitoring and alerting daemon for ARM Linux servers.
Built for the Orange Pi 5 Plus. Sends alerts to Discord via webhooks.

## Architecture

```
Collectors → MetricSnapshot → Alert Engine → AlertEvent → Notifiers
```

Each subsystem owns a single responsibility:

- **Collectors** only gather data
- **Alert Engine** only evaluates rules and tracks state transitions
- **Notifiers** only dispatch events to external systems
- **Orchestrator** wires everything into a collection loop

Alerts use a state machine (`Normal → Warning → Critical → Recovered`) with
built-in deduplication — only state transitions produce events, so you never
get spammed with repeated notifications for the same condition.

## MVP Scope

| Collector | Source |
|-----------|--------|
| CPU | `/proc/stat` |
| Memory | `/proc/meminfo` |
| Disk | `statfs` on `/` |
| Temperature | `/sys/class/thermal/thermal_zone0/temp` |

| Threshold | WARNING | CRITICAL |
|-----------|---------|----------|
| CPU | 80% | 95% |
| Memory | 85% | 95% |
| Disk | 80% | 90% |
| Temperature | 70°C | 80°C |

All thresholds configurable via YAML.

## Quick Start

### Prerequisites

- Rust stable (1.75+)
- Linux ARM64 (Orange Pi / Raspberry Pi)

### Build

```bash
cargo build --release
```

### Configure

1. Copy the example env file and add your webhook URL:

```bash
cp .env.example .env
# Edit .env and set DISCORD_WEBHOOK_URL=your_webhook_url_here
```

2. Adjust thresholds if needed in `configs/guardian.yaml`.

### Run

```bash
cargo run --release
```

Or build and run the binary directly:

```bash
./target/release/guardian-rs
```

## Configuration

Configuration is split between YAML (thresholds, intervals) and environment
variables (secrets). No webhooks, passwords, or tokens are ever stored in
YAML or code.

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
    webhook_url_env: DISCORD_WEBHOOK_URL
    min_interval_secs: 300
```

```bash
# .env (NEVER commit this file)
DISCORD_WEBHOOK_URL=https://discord.com/api/webhooks/...
```

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
│   └── state.rs        # AlertSeverity, AlertState, AlertStateTracker
├── notifiers/
│   └── mod.rs           # Notifier trait, NotifierError
└── services/
    ├── mod.rs
    └── orchestrator.rs  # collect → evaluate → dispatch loop
```

## Alert Lifecycle

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
         │ Recovered │  (event emitted, returns to Normal)
         └──────────┘
```

Only **transitions** generate events. Steady-state conditions are silent.

## Phase 2 (Not Yet Implemented)

- Docker container health monitoring
- PostgreSQL connectivity checks
- Network throughput per interface
- Prometheus exporter
- REST API
- Web dashboard

## License

Private project. All rights reserved.