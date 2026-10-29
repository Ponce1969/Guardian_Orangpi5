# SPEC_MAINTENANCE.md

## Overview

GuardianRS executes operational maintenance on the hosts it monitors, and today
that means one thing: PostgreSQL logical backups. This document specifies how a
task is scheduled, executed, sandboxed and reported.

Maintenance is the only part of GuardianRS that *changes* a host instead of
observing it, so every rule below exists to keep that power bounded, visible and
safe to get wrong. Docker cleanup and log rotation were both considered and are
deliberately out; the governing rule below explains why.

Derived from `SPEC_PRODUCT.md` (Maintenance), `SPEC_ARCHITECTURE.md`
(Maintenance Flow) and `SPEC_ENGINEERING.md` (Maintenance Rules, Security).

## Governing rule

**Observability and auditing are autonomous. Destructive mutation is human.**

Nothing in this daemon deletes containers, images, build cache or volumes on a
schedule, and nothing deletes a file it did not itself create. On a host running
several databases and projects under active development, one wrong prune costs
more than every gigabyte it could ever reclaim; the benefit is bounded and the
loss is not.

GuardianRS therefore reports the space and names the manual command, and the
operator decides. Adding a destructive task later is not an implementation
detail — it is a change to this document, with its own justification.

## Scope

In scope:

- PostgreSQL logical backups

Deferred: rotation of log files. GuardianRS logs through journald, which already
bounds its own storage with `SystemMaxUse`; rotating files the daemon does not own
adds destructive reach for no current need. It returns only together with an
explicit decision about *which* files and by what criterion.

Out of scope, and staying out (see the `SPEC_PRODUCT.md` non-goals):

- **destructive mutation of any kind**: pruning Docker images, containers,
  volumes or build cache; deleting files other than the backups this service
  created for its own retention window
- provisioning, deploying, upgrading or restarting services
- arbitrary scripting or a shell command supplied by configuration
- self-healing on arbitrary triggers
- anything requiring root

### What replaces Docker cleanup

`DockerDiskUsed` from the Docker collector already reports what the daemon holds,
split by images, containers, volumes and build cache. When it crosses a threshold
the notification states the figure and suggests the manual command. The
information is kept and the risk is dropped.

---

## M1 — Lifecycle and reporting

### Rule: never silent

`SPEC_ENGINEERING.md` forbids silent failures and requires that dangerous
operations never execute silently. A maintenance run therefore always produces
exactly two things: one structured `info!` record, always, and at most one
notification.

A run that produces neither does not exist as far as the operator is concerned,
so the design must make that state unreachable.

### Data model

```rust
pub enum MaintenanceStatus {
    /// Completed with exit code 0.
    Success,
    /// Completed with a non-zero exit code.
    Failed,
    /// Exceeded `timeout_secs` and was terminated.
    TimedOut,
    /// Not attempted: a precondition was not met.
    Skipped,
}

pub struct MaintenanceOutcome {
    pub task: String,
    pub status: MaintenanceStatus,
    pub exit_code: Option<i32>,
    pub duration: Duration,
    /// Bounded, single-line, secret-redacted summary of stderr.
    pub detail: String,
}
```

`Skipped` exists so that "we decided not to run it" is distinguishable from
"it failed", and so a precondition failure is still reported.

### Notification policy

| Previous run | Current run | Discord |
| --- | --- | --- |
| `Success` | `Success` | **silent** |
| anything | `Failed` or `TimedOut` | notify |
| `Failed` / `TimedOut` | `Failed` / `TimedOut` | **notify again** |
| `Failed` / `TimedOut` / `Skipped` | `Success` | notify once (recovery) |
| anything (other than `Skipped`) | `Skipped` | notify once, on entering the skip |
| `Skipped` | `Skipped` | **silent** |

Rationale: the noise to suppress is a stable *success* — a backup that worked
again for the two-hundredth time. Failures are never suppressed, because
"notify only on change" would let a backup fail once and then stay broken and
silent for a week. The rule is deliberately asymmetric: silence is only ever
applied in the safe direction.

`Skipped` follows the same asymmetry. Entering a skip is a state change worth
knowing about: it means maintenance stopped happening, and the reason is a
precondition the operator can fix. Staying skipped is a stable state, so it is
silent — otherwise a host that legitimately has no Docker would be told so every
cycle, forever.

### Bounded detail

`detail` carries the **last 1500 characters** of stderr, collapsed to a single
line, with the resolved secret replaced by `***` (see M5). Unbounded output is
a denial-of-service on the notification channel and on the journal.

### Rejected alternatives

- **Emitting from the notifier.** A notifier that decides whether to send is
  mutating policy and re-opens the layering violation closed by the
  `DockerNotifier` refactor. Suppression belongs to the service that owns the
  run history.
- **Modelling a run as a metric with thresholds.** A failed backup is not a
  measurement crossing a threshold; expressing it that way would need a
  synthetic metric kind whose only purpose is to smuggle an event.

---

## M2 — Scheduler

### Rule: schedules live in configuration, and no new dependency

```yaml
maintenance:
  enabled: true
  tasks:
    - name: postgres_backup
      kind: postgres_backup
      schedule: { daily_at: "03:30" }
      timeout_secs: 1800
      grace_secs: 30
```

Two schedule forms are supported:

- `every_hours` / `every_secs` — an interval measured from the previous run
- `daily_at: "HH:MM"` — once per day at local time, catching up at the next
  tick if the daemon was down at that moment

These cover every task the SPEC names — today a single nightly backup — and they
exist so that a weekly or a daily task needs no new code. Evaluation runs on the
orchestrator's existing tick, using `tokio::time`; no additional runtime is
introduced.

### Rule: two clocks, deliberately

An interval is measured on a **monotonic** clock, so an NTP correction or a
daylight-saving change cannot move when the next prune is due. A daily time is
measured on the **local wall clock**, because `03:30` names half past three in
the morning, not an elapsed instant.

Collapsing both onto one clock produces the classic failure where a nightly job
fires twice in October and not at all in March.

Due-ness is measured against *the most recent moment the schedule named*, not
against "the previous run plus an interval". That single choice makes catch-up
safe with no special cases: a host that was off for three days produces one run,
because after that run the last run is later than the most recent moment. The
same property settles both daylight-saving cases without any code that mentions
daylight saving.

### Rule: at most one overdue task per tick

A reboot can leave several tasks overdue at once. Running them together puts a
`pg_dump` and every other task into the same window, competing for I/O on the
same storage — a burst that no individual task caused.

The scheduler therefore runs **at most one due task per tick**. With the default
poll interval that spreads three overdue tasks over ninety seconds. The
`DueReason` reported for each run makes the spread visible instead of looking
like a mysterious delay.

### Rejected alternatives

- **`tokio-cron-scheduler`.** It brings a cron parser plus its own task
  registry, duplicating scheduling state the orchestrator already owns.
  `SPEC_ENGINEERING.md` requires new dependencies to stay lightweight and
  justified, and nothing in the SPEC needs cron's expressiveness.
- **systemd timers.** The conventional Unix answer, and wrong here: a timer
  would run the task outside GuardianRS, so there would be no `MaintenanceOutcome`,
  no Result Event and no notification. It would violate the Maintenance Flow in
  `SPEC_ARCHITECTURE.md`.

Arbitrary cron expressions are **rejected**, not deferred. An interval plus a
daily rollover covers every task GuardianRS has, and a cron parser is dependency
weight for expressiveness nothing asks for.

---

## M3 — Timeouts and orphan processes

### Rule: every task has a hard deadline, and the deadline kills the tree

- `timeout_secs` is **required**. There is no default, because a default is a
  runaway operation nobody decided to allow.
- `grace_secs` defaults to `5`: the window between asking a process to stop and
  forcing it.

Execution sequence:

1. Spawn with `tokio::process::Command`, `process_group(0)` on Unix, and
   `kill_on_drop(true)`.
2. On deadline expiry send `SIGTERM` to the **process group**.
3. Wait `grace_secs`.
4. Send `SIGKILL` to the **process group**.
5. Record `MaintenanceStatus::TimedOut` with whatever stderr arrived.

### Why the process group

`pg_dump` and `docker` may fork. Signalling only the direct child leaves
grandchildren alive, holding locks and continuing to write — which is precisely
the orphan case this section exists to prevent. `kill_on_drop(true)` alone does
not cover it: it reaps the child, not the tree. `process_group(0)` makes the
child a group leader so one `kill(-pgid, …)` reaches everything it started.

`libc` is already a dependency; no new crate is introduced.

### Platform

Unix only. `SPEC_ENGINEERING.md` puts Linux/ARM64 in scope and Windows out of
it, so on other platforms the task degrades to `kill_on_drop` and the limitation
is documented rather than papered over.

---

## M4 — Permissions and sandbox

### Rule: configuration cannot name a command

Configuration selects a `kind` from a **closed enum** implemented in Rust, with
typed parameters:

```rust
pub enum MaintenanceKind {
    PostgresBackup,
}
```

One variant is the point, not an omission. The closed set is the guarantee: adding
a second kind — and above all a destructive one — is a deliberate change to this
document, reviewed as such, rather than a line of configuration someone appends.

There is no `command:` field, and there never will be. Every execution is argv,
never a shell and never a string that reaches one:

```rust
Command::new("pg_dump").args(["--no-password", "-Fc", "-f", destination])
```

No `sh -c`, no concatenation of configuration values into a command line. This
is the rule that turns "editing a YAML file" from remote code execution into
changing parameters of operations that were already decided.

Where `SPEC_ENGINEERING.md` permits Python for maintenance helpers, the script
path is a **fixed resource inside the repository** invoked as
`uv run <fixed-path>`. It is never configurable.

### Rule: no root

`SPEC_ENGINEERING.md` requires least privilege and no unnecessary root. The
daemon runs as `guardianrs`; each task kind documents the smallest membership it
needs:

| Kind | Requires |
| --- | --- |
| `PostgresBackup` | a PostgreSQL role with `CONNECT` and `SELECT` only, plus write access to the backup destination |

### Required unit changes

The current unit makes maintenance impossible, and not marginally:

```ini
ProtectSystem=strict
ReadWritePaths=/proc /sys
```

The entire filesystem is read-only for the process **and everything it spawns**,
so a `pg_dump` child cannot write its dump anywhere. `ProtectHome=true` rules
out a home directory, and `User=guardianrs` is not in the `docker` group, so the
Docker collector cannot reach the socket either.

The minimum addition:

```ini
SupplementaryGroups=docker
ReadWritePaths=/proc /sys /var/backups/guardianrs /var/lib/guardianrs
```

No wider path, and `ProtectSystem=strict`, `ProtectHome=true`, `PrivateTmp=true`
and `NoNewPrivileges=true` all stay. Widening the sandbox is a deployment change
and must be reviewed as one.

---

## M5 — Secrets

### Rule: the resolved secret travels to the child, never through a log

Maintenance reuses the contract established by the PostgreSQL collector: the
configuration file carries the **name** of the environment variable, and
`config::load` resolves it, failing with `MissingEnv` at startup when the task is
enabled and the variable is unset.

- The task receives the already-resolved `PostgresConfig`. It never reads the
  environment itself, so there is one resolution point and one failure point.
- The password reaches `pg_dump` through **`PGPASSWORD` in the child's
  environment**. Never argv, which any local user can read in `ps`.
- Before any log or notification, every occurrence of the password in captured
  output is replaced with `***`. This is tested, not assumed: command stderr is
  attacker-adjacent input and the one place a secret is most likely to leak.
- `PGPASSFILE` inside `PrivateTmp` is the documented alternative if the
  environment-variable route is ever considered insufficient; `/proc/<pid>/environ`
  is already `0400` and readable only by the process owner.

### Destination and retention

The backup directory defaults to `/var/backups/guardianrs/postgres` and is
configurable, so a dedicated volume or NVMe mount can be attached later without
touching the code. It must appear in the unit's `ReadWritePaths` (see M4).

`keep_backups` bounds how many backups are kept per task. Old backups are
pruned **only after a run reports `Success`**. Pruning before the replacement is
confirmed is how a working backup gets deleted by a broken one.

---

## Failure modes

| Situation | Status | Detail carries |
| --- | --- | --- |
| exit code 0 | `Success` | last stderr lines (often empty) |
| exit code ≠ 0 | `Failed` | exit code + redacted stderr tail |
| deadline exceeded | `TimedOut` | redacted stderr collected so far |
| preconditions unmet (no credentials, missing binary) | `Skipped` | the missing precondition |
| binary not found / spawn error | `Failed` | the spawn error |

A missing `pg_dump` is `Skipped`, not `Failed` on loop: a host that legitimately
has no PostgreSQL client must not spend every cycle failing.

---

## Testing requirements

- **No test may execute a real command.** Execution sits behind an
  `Executor` trait, exactly as the Docker collector sits behind `DockerApi`, so
  the suite never shells out, never touches `/var` and never needs `pg_dump`.
- Pure functions cover schedule due-ness (interval, daily rollover, catch-up
  after downtime) and the notification transition table. These are the rules
  most likely to be wrong in a way nobody notices.
- Redaction is tested with a password that also appears inside a realistic stderr
  line.
- Timeout behaviour is tested with a fake executor that never completes; the
  kill escalation itself is verified once, manually, on the target host.

---

## Resolved decisions

Recorded so they do not get reopened by accident:

| Question | Decision |
| --- | --- |
| Backup destination | `/var/backups/guardianrs/postgres`, overridable in configuration so a dedicated volume can be mounted later without a code change |
| Log rotation | Deferred. journald already bounds GuardianRS's own logs, and rotating foreign files needs its own decision about scope and criterion |
| Arbitrary cron | Rejected. Interval plus daily rollover covers every task; no cron parser dependency |
