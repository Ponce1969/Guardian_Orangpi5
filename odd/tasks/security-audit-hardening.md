# Security Audit, Durable Delivery & Liveness Plan

- **Baseline commit:** `a4348d7e59e6eaabe5955d5530f392e92787aeda` (`a4348d7`)
- **Governing spec:** `SPEC/SPEC_SECURITY_HARDENING.md` (`I1`–`I5`)

## Dependency-Ordered Work Units

Ordered strictly so no downstream feature inherits an upstream bug:

| Order | Priority | Spec | Scope | Deliverable & Named Tests |
|-------|----------|------|-------|---------------------------|
| **U0** | Gate | Spec | `SPEC/`, `odd/` | Baseline audit table signed at `a4348d7`, closed `I1`–`I5` invariants, aligned `SPEC_MAINTENANCE.md`. |
| **U1** | **P0** | `I1.1`–`I1.3`, `I5.1`, `I3.2` | `src/alerts/`, `src/config.rs`, `tests/per_instance_alerts.rs` | Fix `AlertKey` static identity allowlist (`database`, `host`, `instance`, `mount`, `zone`), evict `Normal` from `AlertStateTracker`, emit `Warning` on `Critical -> Warning`, set `DockerConfig` default `enabled: false`, and redact `Debug` on `PostgresInstanceConfig` & `DiscordConfig`. *(Unblocks outbox & digest keying).* |
| **U2** | **P1** | `I2.4`, `I3.3`, `I4.1` | `src/config.rs`, `src/main.rs`, `src/services/maintenance/` | Reject `poll_interval_secs == 0`, invalid/inverted thresholds, path-unsafe DB/instance names, and non-HTTPS/non-Discord webhook URLs; pair `.env` fallback with active config root; propagate real `ScheduledTask.name` into `MaintenanceOutcome.task`; replace cleartext `--file <restore_temp>` with `--file -` + `Stdio::null()` (**explicitly updating divergent test `verification_reads_the_file_without_credentials`**) and clean stale `.dump.partial` files. *(Unblocks honest digest reporting).* |
| **U3** | **P1** | `I2.1`–`I2.3`, `I3.1` | `src/notifiers/`, `src/collectors/postgres.rs`, `src/collectors/docker.rs`, `src/services/orchestrator.rs` | Add explicit deadlines (`DiscordNotifier`, `PostgresCollector` full probe, `DockerCollector`), strip URLs via `without_url()` + bound error bodies, implement durable `/var/lib/guardianrs/outbox.json` with `AlertKey`/`task` supersession, intermediate-transition counter, capacity cap (`64`), and independent `Orchestrator::tick` `flush` phase + loopback HTTP `TcpListener` tests. |
| **U4** | **P1** | `I4.2` | `src/config.rs`, `src/services/maintenance/state.rs`, `src/services/orchestrator.rs`, `src/notifiers/` | Add `digest` (`enabled: true`, `daily_at: "08:00"` in host local time, zero new dependencies), DST-safe once-per-local-date scheduling persisted in `state.json`, post-startup-grace late catch-up note, failed-only collector counter, active alerts summary, and explicit `"pending / did not run"` per pending/running/skipped task. |
| **U5** | **P2** | `I5.2` | `systemd/`, `scripts/`, `.env.example`, `.gitattributes`, `tests/production_config.rs` | Harden `guardianrs.service` (`UMask=0077`, remove `/proc /sys` from `ReadWritePaths`, kernel/SUID directives), create `0700` dirs & preserve `.env` in `deploy.sh`, fix `GUARDIAN_PG_AUDITOR_PASSWORD` & shell-quoting in `verify-restore.sh`, document all env vars in `.env.example`, add LF `.gitattributes`, and pin unit assertions in `tests/production_config.rs`. |

## Explicit Receipt / Test-Change Declarations

1. `services::maintenance::backup::tests::verification_reads_the_file_without_credentials` (`src/services/maintenance/backup.rs:560`): currently asserts `plan.restore.args().contains(&plan.restore_temp)`. Replaced in **U2** by `verification_streams_to_null_without_temp_file` because writing `<temp>.restore` is the exact disk-amplification and cleartext-SQL defect outlawed by `I4.1`.
2. `alerts::engine::tests::memory_critical_to_warning_recovers` and `disk_critical_to_warning_recovers` (`src/alerts/engine.rs:395,558`): currently assert `AlertSeverity::Recovered` when still above warning threshold. Replaced in **U1** by `critical_to_warning_emits_warning_not_recovered` per `I1.3`.
