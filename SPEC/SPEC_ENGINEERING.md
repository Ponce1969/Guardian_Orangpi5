# SPEC_ENGINEERING.md

## Engineering Philosophy

GuardianRS prioritizes:

- correctness
- simplicity
- operational reliability
- low resource usage
- maintainability
- explicit architecture boundaries

The codebase must remain understandable and production-oriented.

---

# Language & Runtime

## Primary Language

- Rust stable

## Runtime

- Tokio async runtime

## Platform Targets

Primary targets:

- Linux
- ARM64
- Debian-based systems

Windows support is out of scope.

---

# Approved Rust Crates

Examples of preferred crates:

- tokio
- tracing
- serde
- serde_yaml
- reqwest
- sysinfo
- bollard
- anyhow
- thiserror

New dependencies must remain lightweight and justified.

Avoid unnecessary framework complexity.

---

# Async Rules

- Use async/await consistently.
- Blocking operations inside async tasks are forbidden.
- Use `tokio::time` instead of thread sleeping.
- Long-running tasks must be supervised.
- Failures inside one task must not terminate the daemon.

---

# Error Handling

## Rules

- `unwrap()` is forbidden in production code.
- `expect()` is forbidden in production code.
- Errors must be propagated explicitly.
- Infrastructure failures must degrade gracefully.
- Monitoring loops must remain resilient.

## Error Types

- `thiserror` preferred for typed domain errors
- `anyhow` allowed at application boundaries

---

# Logging

## Logging Requirements

- Use structured logging.
- Use `tracing`.
- Logs must remain machine-readable.
- Silent failures are forbidden.

Examples:

- collector failure
- notifier failure
- Docker API unavailable
- remediation execution failure

---

# Configuration

Configuration must remain externalized.

Examples:

- YAML
- environment variables

Hardcoded operational values are forbidden.

Examples:

- webhook URLs
- thresholds
- polling intervals
- container names

---

# Python Scripts

Python is allowed only for operational scripts.

Examples:

- Docker cleanup
- PostgreSQL backups
- maintenance helpers

## Python Rules

- Use `uv`
- `pip` is forbidden
- Ruff mandatory
- Mypy mandatory
- fully typed code preferred

Python scripts must remain isolated from Rust runtime logic.

---

# Code Organization

## Principles

- Small focused modules
- Explicit ownership
- Minimal coupling
- Clear boundaries
- Predictable responsibilities

## Forbidden

- God objects
- hidden mutable globals
- circular dependencies
- business logic inside notifiers
- alert logic inside collectors

---

# Testing

Minimum expectations:

- unit tests for alert rules
- unit tests for collectors where possible
- deterministic alert evaluation
- isolated module testing

Critical infrastructure logic must remain testable.

---

# Operational Requirements

GuardianRS must:

- run continuously
- tolerate subsystem failures
- survive unavailable dependencies
- remain lightweight on ARM systems
- avoid excessive CPU usage
- avoid excessive memory usage

Operational resilience is mandatory.

---

# Maintenance Rules

Maintenance execution must:

- remain observable
- generate logs/events
- avoid destructive default behavior

Dangerous operations must never execute silently.

Examples:

- Docker prune
- volume cleanup
- destructive filesystem operations

---

# Security

- Principle of least privilege
- Avoid unnecessary root usage
- Secrets must not be hardcoded
- Webhook URLs must remain externalized
- Shell execution must remain controlled

---

# Future Engineering Goals

Potential future additions:

- REST API
- Prometheus exporter
- persistent event history
- self-healing workflows
- metrics dashboard

Future evolution must preserve simplicity
and operational maintainability.