# SPEC_SECURITY_HARDENING.md

## Baseline Verification Header

- **Verified against commit:** `a4348d7e59e6eaabe5955d5530f392e92787aeda` (`a4348d7`)
- **Branch:** `fix/security-audit-hardening`
- **Rule of truth:** This document distinguishes what the codebase **does at the
  baseline commit** (`VIOLATED`, `ABSENT`, `DIVERGES`, `DEBT`) from the target
  invariants (`I1`–`I5`). Every normative `MUST` maps to at least one named
  automated test in the Baseline Audit Table below. No `MUST` is valid without a
  test that fails when the invariant is broken.

Derived from and complementary to `AGENTS.md`, `SPEC_PRODUCT.md`,
`SPEC_ARCHITECTURE.md`, `SPEC_ENGINEERING.md`, and `SPEC_MAINTENANCE.md`.

---

## Baseline Audit Table (at `a4348d7e59e6eaabe5955d5530f392e92787aeda`)

| ID | Rule Summary | Baseline Status | Evidence in Code (`a4348d7`) | Required Verification Test(s) |
|----|--------------|-----------------|------------------------------|-------------------------------|
| **I1.1** | `AlertKey` hashes static identity labels only | **VIOLATED** (live bug) | `src/alerts/state.rs:66-79` hashes all `snapshot.labels`; `memory.rs:190`, `disk.rs:164`, `docker.rs:155-169` attach per-tick telemetry (`available_kb`, `available_bytes`, `running`, `*_gb`). | `alerts::state::tests::telemetry_labels_do_not_change_alert_key`, `tests/per_instance_alerts.rs::collector_telemetry_labels_deduplicate_and_recover` |
| **I1.2** | Zero tracker retention for `Normal` series | **VIOLATED** (live leak) | `src/alerts/state.rs:123-140` inserts every unseen key into `HashMap` even when `new_state == Normal`. | `alerts::state::tests::normal_state_retains_zero_entries_in_tracker` |
| **I1.3** | `Critical -> Warning` emits `Warning`, never `Recovered` | **VIOLATED** (inverted) | `src/alerts/engine.rs:109` maps `(Critical, Warning) => AlertSeverity::Recovered` ("recovered below warning" while above warning). | `alerts::engine::tests::critical_to_warning_emits_warning_not_recovered` |
| **I2.1** | Every external socket, query, and HTTP call has a deadline | **ABSENT** | `src/notifiers/discord.rs:100` uses unbounded `reqwest::Client::new()`; `postgres.rs:166` awaits `simple_query` without deadline; `docker.rs:283,294` await Bollard without deadline. | `notifiers::discord::tests::stalled_webhook_times_out_within_deadline`, `collectors::postgres::tests::stalled_query_times_out_as_failed_probe`, `collectors::docker::tests::stalled_docker_calls_time_out_within_deadline` |
| **I2.2** | Durable at-least-once delivery outbox with supersession and bounded capacity | **ABSENT** | `src/notifiers/discord.rs:173-197` fires once and drops on `5xx`/`429`/network blip after `AlertEngine` already consumed the transition. | `notifiers::discord::tests::outbox_supersedes_by_key_and_records_skipped_transitions`, `notifiers::discord::tests::outbox_persists_across_restart_and_enforces_capacity` |
| **I2.3** | Independent orchestrator `flush` phase on every tick | **ABSENT** | `src/services/orchestrator.rs:324-353` returns early on empty snapshots, startup grace, or `events.is_empty()`, preventing queued alerts from draining when collectors fail or state is steady. | `services::orchestrator::tests::flush_runs_even_when_collectors_fail_or_emit_no_events` |
| **I2.4** | Fail-fast startup config bounds (`poll_interval_secs >= 1`, ordered finite thresholds, valid `digest.daily_at`, path-safe DB names) | **ABSENT** | `src/services/orchestrator.rs:101` panics `tokio::time::interval` when `poll_interval_secs == 0`; `src/config.rs:235-286` accepts `NaN` or `warning >= critical`; `database` allows `/` or `..`. | `config::tests::zero_poll_interval_is_rejected`, `config::tests::inverted_or_non_finite_thresholds_are_rejected`, `config::tests::path_separators_in_database_or_instance_are_rejected`, `config::tests::invalid_digest_daily_at_is_rejected`, `config::tests::negative_non_temperature_thresholds_are_rejected`, `config::tests::duplicate_maintenance_task_names_are_rejected` |
| **I3.1** | Transport errors strip secret URLs and cap response bodies | **VIOLATED** (active leak) | `src/notifiers/discord.rs:180-193` formats `reqwest::Error` with URL/token into `journald` and reads unbounded `response.text()`. | `notifiers::discord::tests::transport_failure_never_contains_webhook_url_or_token`, `notifiers::discord::tests::webhook_error_body_is_bounded` |
| **I3.2** | Secret-bearing config structs redact `Debug` | **DEBT** (no active `{:?}` in `src/`) | `src/config.rs:34,315,357,362` derive `Debug` over `resolved_password` and `resolved_webhook_url`. Zero `{:?}` call sites exist in `src/` at `a4348d7`, so this is latent debt rather than an active leak. | `config::tests::debug_output_redacts_webhook_url_and_postgres_passwords` |
| **I3.3** | HTTPS Discord webhook validation and config-paired `.env` | **ABSENT / DIVERGES** | `src/config.rs:415-423` accepts plaintext `http://` or arbitrary hosts; `src/main.rs:26` loads CWD `.env` unconditionally (systemd `EnvironmentFile` works, manual CLI from another directory diverges). | `config::tests::webhook_url_rejects_non_https_or_foreign_hosts`, `config::tests::startup_and_persistence_paths_resolve_deterministically` |
| **I4.1** | Non-amplifying `pg_restore` verification (`--file -` to null) and real task identity | **DIVERGES** | `src/services/maintenance/backup_task.rs:246` hardcodes `"postgres_backup"`; `backup.rs:264-272` writes cleartext SQL to `<temp>.restore` (pinned by existing test `verification_reads_the_file_without_credentials` at `backup.rs:560`); `backup_task.rs:136` skips `discard()` on verify spawn error. | `services::maintenance::backup::tests::verification_streams_to_null_without_temp_file`, `services::maintenance::backup_task::tests::outcome_carries_configured_task_name_and_cleans_partials` |
| **I4.2** | Daily human liveness digest (`08:00` local, DST-safe, persisted, late catch-up, failure-only collector summary) | **ABSENT** | No heartbeat or daily digest exists; silence on Discord is indistinguishable from a dead daemon or broken webhook. | `services::digest::tests::daily_digest_handles_dst_transitions_once_per_date`, `services::digest::tests::digest_persists_across_restart_and_marks_late_catchup`, `services::digest::tests::digest_reports_unrun_tasks_as_did_not_run_and_failed_collectors_only` |
| **I5.1** | Docker opt-in default (`false`) | **DIVERGES** (inverted) | `src/config.rs:64-71` defaults `docker.enabled` to `true` when omitted. | `config::tests::docker_collector_is_disabled_by_default_when_omitted` |
| **I5.2** | Systemd `UMask=0077`, clean `ReadWritePaths`, safe `deploy.sh` & `verify-restore.sh`, LF `.gitattributes` | **DIVERGES / ABSENT** | `systemd/guardianrs.service:25` grants `ReadWritePaths=/proc /sys` and lacks `UMask=0077`; `deploy.sh` omits `0700` state/backup dirs and overwrites `.env`; `verify-restore.sh:38,60` uses legacy var and `sh -c` path interpolation. | `tests/production_config.rs::systemd_unit_enforces_umask_and_minimal_writable_paths`, `tests/production_config.rs::deployment_scripts_and_gitattributes_enforce_security_hygiene` |

---

## Core Invariants

### I1 — Alert Identity, Memory Bounds, and True Recovery (`P0`)

1. **Identity allowlist (`MUST`):** `AlertKey::from_snapshot` MUST include only
   static resource-identity labels from the closed allowlist
   `["database", "host", "instance", "mount", "zone"]`. Volatile telemetry
   labels remain on `MetricSnapshot` and `AlertEvent` for display only.
2. **Zero retention when healthy (`MUST`):** `AlertStateTracker` MUST remove a
   key when its state returns to `AlertState::Normal`, allocating zero entries
   for healthy series (`O(active alerts)` memory).
3. **De-escalation is not recovery (`MUST`):** `AlertEngine` MUST map
   `(AlertState::Critical, AlertState::Warning)` to `AlertSeverity::Warning`
   against `rule.warning`. `AlertSeverity::Recovered` MUST be emitted only when
   entering `AlertState::Normal`.

---

### I2 — Bounded Cycle Deadlines, Durable Delivery Outbox, and Fail-Fast Startup (`P0`/`P1`)

#### Architectural Boundary (explicit compliance with `AGENTS.md`)

- `Alerts` (`AlertEngine` / `AlertStateTracker`) owns **rule evaluation and
  state transitions** — deciding *whether* an alert event exists.
- `Notifiers` owns **message formatting, outbound transport, and retry/spooling
  of undelivered payloads** (`SPEC_ARCHITECTURE.md`: *"Owns: message formatting,
  webhook transport, retry strategy, outbound delivery"*). Queuing a failed HTTP
  delivery does NOT evaluate rules or mutate `AlertStateTracker`.
- `Services` (`Orchestrator`) owns **tick phase scheduling**, invoking transport
  `flush` as an independent phase on every cycle.

#### Rules

1. **No unbounded external await (`MUST`):** Every network connect, HTTP
   request, SQL probe (`connect` + `SELECT 1`), and Docker socket call MUST
   execute under an explicit timeout whose worst-case retry sequence stays
   strictly smaller than the `30s` poll cycle budget (`3s` connect, `5s`
   request, `500ms`/`1s` backoff, `2s` `Retry-After` cap -> `<= 19s` worst case).
2. **Durable delivery outbox (`MUST`):** When `DiscordNotifier` fails to deliver
   an alert, maintenance outcome, or digest after quick transport retries on
   transient errors (`5xx`, `429`, timeout/network error; non-`429` `4xx` errors
   are logged as permanent configuration errors and not re-queued), it MUST
   persist the pending entry to an atomic disk outbox in `/var/lib/guardianrs`
   governed by five rules:
   - **Supersession by logical key:** Keyed by `OutboxKey::Alert(AlertKey)` for
     alerts, `OutboxKey::Maintenance { task }` for announced maintenance
     outcomes (interacting cleanly with `should_announce`: silent steady
     successes never enter the outbox and are covered by the daily digest), and
     `OutboxKey::Digest` for the daily digest. A newer entry for the same key
     replaces the older undelivered entry and increments `superseded_count`,
     appending a note of intermediate transitions to the embed so stale states
     (e.g. a 40-minute-old `Critical` already followed by `Recovered`) are never
     delivered out of context.
   - **Bounded capacity and overflow policy:** The outbox holds up to a
     configured entry cap. On overflow, `Recovered` entries are evicted oldest
     first before active `Warning`/`Critical`/failed-maintenance entries, and an
     explicit `dropped_count` counter is surfaced on the next delivered message
     and logged as `error!`.
   - **Crash and restart durability:** Written atomically (`temp` + `sync_all` +
     `rename` + `sync_parent`, same contract as `maintenance::state`) under
     `/var/lib/guardianrs`, surviving `systemctl restart` and power loss; an
     unreadable file logs `warn!` and starts empty rather than crashing the
     daemon.
   - **Declared `at-least-once` semantics:** Discord webhooks offer no
     idempotency key, so an ambiguous read timeout after server receipt may
     deliver a duplicate. Every payload MUST carry the original event/transition
     UTC `timestamp` so duplicates are unambiguously identifiable.
3. **Independent `flush` phase (`MUST`):** `Orchestrator::tick` MUST execute
   notification flushing as its own fault-isolated phase on **every tick**,
   independent of collector failures, empty metric snapshots, `startup_grace`,
   or `events.is_empty()`.
4. **Fail-fast startup bounds (`MUST`):** `config::load` MUST reject
   `daemon.poll_interval_secs == 0`, non-finite or inverted thresholds
   (`warning >= critical`, negative non-temperature values, or percentage
   thresholds `> 100.0`), invalid `digest.daily_at` (`HH:MM`), and empty or
   path-traversing (`/`, `\`, `..`, control chars) `instance` or `database`
   identifiers with `ConfigError::Invalid`.

---

### I3 — Zero Secret Egress (`P0`/`P1`)

1. **URL-free transport errors and bounded error bodies (`MUST`):**
   `DiscordNotifier` MUST strip request URLs before formatting transport errors
   (`error.without_url()`) and truncate HTTP error response bodies before
   logging or returning `NotifierError`.
2. **Manual redacted `Debug` on secret holders (`MUST`):** `PostgresInstanceConfig`
   and `DiscordConfig` MUST implement `fmt::Debug` manually with `"***"` in
   place of `resolved_password` and `resolved_webhook_url` (closing the latent
   `GuardianConfig` `Debug` debt).
3. **Webhook destination validation and deterministic `.env` (`MUST`):** When
   Discord is enabled, `config::load` MUST trim and require an `https://` URL on
   `discord.com` or `discordapp.com` (including `ptb.`/`canary.`) under
   `/api/webhooks/<id>/<token>`. When `/opt/guardianrs/guardian.yaml` is active,
   optional `.env` fallback MUST read `/opt/guardianrs/.env` rather than the
   caller's working directory.

---

### I4 — Human Liveness Digest and Safe Nightly Backup Verification (`P1`)

1. **Zero-amplification backup verification and real task identity (`MUST`):**
   - `MaintenanceOutcome.task` MUST carry the configured `ScheduledTask.name`
     (`auditor_nightly`, `pedidos_nightly`, `ecommerce_nightly`,
     `nutrition_nightly`), never a hardcoded `"postgres_backup"` literal.
   - `BackupPlan` MUST verify archive data by running
     `pg_restore --file - <partial>` with child `stdout` attached to
     `Stdio::null()` (capturing `stderr` only), writing **zero** temporary
     cleartext SQL files to disk. *(Explicit test migration note: existing unit
     test `verification_reads_the_file_without_credentials` in
     `src/services/maintenance/backup.rs:560` asserts the divergent
     `--file <restore_temp>` flag and MUST be updated to assert `--file -` with
     discarded stdout before the digest can truthfully report verified backups).*
   - Every failure path in `BackupTask::run` (including `verify` spawn errors)
     MUST remove `<partial>`, and `BackupTask` MUST sweep stale
     `guardianrs-<database>-*.dump.partial` files for its own database.
2. **Daily human liveness digest (`MUST`):**
   - **Separation of concerns:** Nightly maintenance (`03:30`–`03:45`) is machine
     time and remains untouched. The digest is a **human signal** defaulting to
     `08:00` local time after the maintenance window closes.
   - **Opt-out only:** Enabled by default (`digest.enabled: true`,
     `digest.daily_at: "08:00"` when omitted from YAML); disabling requires
     explicit `digest.enabled: false` in configuration.
   - **Host local time & deterministic DST handling:** `digest.daily_at` is
     evaluated in the **host's local timezone** (`chrono::Local`, rendered with
     its UTC offset in the digest). Scheduling tracks
     `last_digest_date: Option<NaiveDate>` (the local calendar date already
     reported) persisted through `maintenance::state`:
     - *Autumn fall-back (repeated local hour):* Evaluates against the first
       occurrence (`MappedLocalTime::Ambiguous(earliest, _)`); once sent,
       `last_digest_date == Some(today)` prevents firing a second time.
     - *Spring forward (skipped local hour):* Resolves the gap to the first
       valid local instant after the jump (`MappedLocalTime::None`), firing
       exactly once for `today`.
   - **Restart persistence & late catch-up:** Persisting `last_digest_date` via
     `state::load` / `state::save` ensures a restart at `08:05` does not re-send
     and a restart at `07:59` still sends at `08:00`. If the daemon was down at
     the scheduled moment (`last_digest_date < Some(today)` after `daily_at`, or
     missed the previous day before `daily_at`), it sends **once** after
     `startup_grace` with an explicit late-delivery note naming the missed
     scheduled time.
   - **High-signal content (no green noise):**
     1. *Collectors:* Never report `"N collectors active"`. Report **only**
        collectors that failed since the previous digest with their failure
        counts, or one line (`No collector failures`) when none failed.
     2. *Active alerts:* Any metric series currently in `Warning` or `Critical`
        (or `No open alerts`).
     3. *Nightly maintenance:* Assumes the overnight maintenance window closed
        before `08:00` and renders one explicit line **per configured task**. A
        task that is still pending, still running, or was `Skipped` at digest
        time MUST be reported explicitly as `"pending / did not run"` (never
        omitted, because a missing line reads like success).
   - **Production acceptance gate:** Automated tests prove the code paths,
     whereas operational closure requires observing a real `08:00` digest
     message delivered to Discord on the target host carrying all 4 configured
     backups, collector failure status, and open alerts. Until that message is
     observed, the end-to-end chain (`collect -> transition -> delivery ->
     digest`) remains unproven in production.

---

### I5 — Host Privilege and Deployment Hygiene (`P2`)

1. **Docker disabled by default (`MUST`):** `DockerConfig::default()` MUST set
   `enabled: false`.
2. **Systemd & script hygiene (`MUST`):** `systemd/guardianrs.service` MUST set
   `UMask=0077`, remove `/proc` and `/sys` from `ReadWritePaths=`, and enable
   `ProtectKernelTunables=true`, `ProtectKernelModules=true`,
   `ProtectControlGroups=true`, `RestrictSUIDSGID=true`, and
   `LockPersonality=true` (without `SupplementaryGroups=docker`).
   `scripts/deploy.sh` MUST create `/var/backups/guardianrs` and
   `/var/lib/guardianrs` (`0700`, `guardianrs:guardianrs`) without overwriting
   an existing `/opt/guardianrs/.env`. `scripts/verify-restore.sh` MUST read
   `GUARDIAN_PG_AUDITOR_PASSWORD` (with `GUARDIAN_PG_PASSWORD` fallback) and pass
   paths via environment/argv rather than `sh -c` string interpolation.
   `.gitattributes` MUST enforce `eol=lf` on shell, systemd, and YAML files.

---

## Operational Defaults (Configurable / Non-Normative Constants)

Numbers below are default operational parameters, kept separate from the
structural `MUST` invariants above so tuning them never invalidates the spec:

| Parameter | Default | Scope |
|-----------|---------|-------|
| Discord HTTP `connect_timeout` | `3s` | `DiscordNotifier` |
| Discord HTTP `request_timeout` | `5s` | `DiscordNotifier` |
| Discord immediate retry attempts | `2` (`500ms`, `1s` backoff; honors `Retry-After` capped at `2s`) before spooling to durable outbox | `DiscordNotifier` |
| Discord error body log cap | `512` chars | `DiscordNotifier` |
| Durable outbox capacity | `64` entries (`/var/lib/guardianrs/outbox.json`) | `DiscordNotifier` / `Orchestrator` |
| Docker `list_containers` / `df` timeout | `5s` / `15s` | `DockerCollector` |
| `digest.enabled` / `digest.daily_at` | `true` / `"08:00"` | `DigestConfig` |
| `digest.late_grace_secs` | `300s` (beyond which catch-up marks `"late delivery"`) | `DigestScheduler` |
