# SPEC_ARCHITECTURE.md

## Overview

GuardianRS is a lightweight infrastructure monitoring and alerting daemon
designed for ARM Linux servers and Docker-based homelabs.

The system is event-oriented and modular.

The architecture intentionally separates:

- metric collection
- alert evaluation
- event notification
- orchestration
- maintenance execution

Each subsystem owns a single responsibility.

The architecture must remain composable, observable, testable,
and operationally resilient.

---

# Architectural Principles

## Core Principles

- Prefer composition over monolithic services.
- Keep subsystem ownership explicit.
- Every module must have a single operational responsibility.
- Monitoring must remain independent from Discord or external transports.
- Infrastructure concerns must not leak into domain logic.
- All modules must be testable in isolation.
- Async execution must remain non-blocking.
- Avoid hidden side effects between layers.

---

# High-Level Architecture

```text
Collectors
    ↓
Metrics
    ↓
Alert Engine
    ↓
Alert Events
    ↓
Notifiers

Additional orchestration services may coordinate workflows,
but must not violate ownership boundaries.

Bounded Contexts
Collectors Context

Responsible for gathering raw infrastructure metrics.

Collectors MUST NOT:

trigger alerts
send Discord messages
execute remediation
contain business rules
persist application state

Collectors only gather data.

Examples:

CPU usage
Memory usage
Disk usage
Docker container status
PostgreSQL health
Network throughput
Temperature metrics
Ownership

Owns:

infrastructure inspection
Linux system reads
Docker API reads
metric snapshots

Does NOT own:

alert logic
transport logic
orchestration
Alerts Context

Responsible for evaluating metric state and generating alert events.

Alerts MUST NOT:

inspect infrastructure directly
call Docker APIs
send notifications
execute shell commands

Alerts only evaluate rules.

Examples:

CPU critical threshold
disk usage warning
container unhealthy
PostgreSQL unavailable
excessive Docker cache usage
Ownership

Owns:

threshold rules
alert lifecycle
alert severity
alert deduplication
alert state transitions

Does NOT own:

Discord transport
infrastructure collection
remediation actions
Notifiers Context

Responsible for dispatching alert events to external systems.

Notifiers MUST NOT:

evaluate rules
inspect infrastructure
mutate alert state

Notifiers only transport messages.

Examples:

Discord webhook
structured logs
future Telegram integration
future email integration
Ownership

Owns:

message formatting
webhook transport
retry strategy
outbound delivery

Does NOT own:

monitoring logic
thresholds
remediation
Services Context

Responsible for orchestration and operational workflows.

Services coordinate system behavior.

Examples:

scheduler loop
maintenance execution
cleanup orchestration
remediation workflows
Ownership

Owns:

runtime orchestration
task scheduling
maintenance coordination
service lifecycle

Does NOT own:

metric collection internals
alert rules
notifier implementations

Alert Lifecycle

The system must model alert transitions explicitly.

NORMAL
  ↓
WARNING
  ↓
CRITICAL
  ↓
RECOVERED

Repeated identical alerts must be suppressed.

State transitions are mandatory.

Event Flow
Metric Flow
Collectors
    → MetricSnapshot
    → Alert Engine
Alert Flow
Alert Engine
    → AlertEvent
    → Notifiers
Maintenance Flow
Scheduler
    → Maintenance Service
    → Shell Scripts
    → Result Events
    → Notifiers