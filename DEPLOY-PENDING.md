# Deploy pending: maintenance state persistence

**Status:** code complete and tested locally. Not deployed.
**Repo:** `main` at `5bf4774`. **Host still runs:** `1cd004e`.
**Delete this file once the deploy is done.**

## Why this exists

The maintenance scheduler kept its last-run record in memory, so every restart made
every due task run again. On the host that meant four PostgreSQL dumps in
sixty-one minutes for a task scheduled once a day at 03:30 — one per restart. With
`keep_backups: 7`, seven restarts in one day replace a week of history with a day of
it, because retention prunes by age and cannot tell a scheduled backup from one
nobody asked for.

`5bf4774` records completed runs in `/var/lib/guardianrs/state.json` and restores
them at startup.

## Ready in the repository

| Item | Where |
| --- | --- |
| State read/write, atomic replace | `src/services/maintenance/state.rs` |
| `export_state` / `restore` | `src/services/maintenance/scheduler.rs` |
| Load at startup, save after each run | `src/services/orchestrator.rs` |
| `state_path` setting | `src/config.rs`, `configs/guardian.yaml` |
| `ReadWritePaths` incl. `/var/lib/guardianrs` | `systemd/guardianrs.service` |
| Tests | 254 passing, `cargo fmt --check` and `cargo clippy --all-targets --all-features` clean |

A bundle of `main` is prepared at `/tmp/g3.bundle` (184 KB) for transferring without
pushing to GitHub.

## Steps, once at the desk

### 1. Root, cannot be done remotely

`/var/lib/guardianrs` was already created with `750 guardianrs:guardianrs`; confirm
it still is.

```bash
ls -ld /var/lib/guardianrs
sudo cp ~/Gonzalo_codigo/Guardian_Rust/Guardian_Orangpi5/systemd/guardianrs.service \
  /etc/systemd/system/
sudo systemctl daemon-reload
```

This also removes the `Unknown key name 'StartLimitIntervalSec' in section 'Service'`
warning that appears on every start: the directive belongs to `[Unit]`, and while it
sits in `[Service]` the crash-loop rate limit is silently not applied.

Without this step the daemon cannot write the state file. It will log
`Could not record maintenance state` and keep running — persistence would be a no-op.

### 2. Build and install the binary

```bash
# from Windows
git bundle create /tmp/g3.bundle main
scp /tmp/g3.bundle orangepi:/tmp/

# on the Pi
cd ~/Gonzalo_codigo/Guardian_Rust/Guardian_Orangpi5
git fetch /tmp/g3.bundle main:pi-deploy --force
git merge --ff-only pi-deploy
~/.cargo/bin/cargo build --release

sudo systemctl stop guardianrs          # required: cp fails with "Text file busy" otherwise
cp target/release/guardian-rs /opt/guardianrs/guardian-rs
cp configs/guardian.yaml /opt/guardianrs/guardian.yaml
chmod 755 /opt/guardianrs/guardian-rs
sudo systemctl start guardianrs
```

Note: `git pull` does not work here. `origin/main` is at `c849ed5`; the commits exist
only locally, so the bundle is the transport. Do not put a pipe on the fetch or merge
— `git fetch … | tail` returns the exit status of `tail`, not of `git`, and a failed
pull once compiled the old code while reporting success.

### 3. Prove it works

The check is a subtraction, and it must run twice because the first restart has no
record yet to restore:

```bash
sudo ls -1 /var/backups/guardianrs/postgres/ | wc -l
sudo systemctl restart guardianrs
sudo ls -1 /var/backups/guardianrs/postgres/ | wc -l   # one more: first run under the new binary
sudo systemctl restart guardianrs
sudo ls -1 /var/backups/guardianrs/postgres/ | wc -l   # unchanged: the record survived
```

The third count equals the second. If it grew, the state file is not being read or
written — check `journalctl -u guardianrs | grep -i "maintenance state"`.

## Already verified in production at `1cd004e`

- PostgreSQL probe against `auditor_familiar` on `127.0.0.1:5442`: zero failed probes.
- A real backup: `status=success`, meaning exit 0, file non-empty, and
  `pg_restore --list` read it.
- `docker.enabled: false` honoured: `Collectors initialized count=6`, no socket access.
- SIGTERM: `Deactivated successfully` 196 µs after the signal, against 90 seconds and a
  SIGKILL before `eb0bb2f`.

## Still unverified

- The kill escalation: no test covers it. Run
  `timeout 5 sh -c 'sleep 300 & wait'`, then `pgrep -af "sleep 300"` must be empty.
- Nothing has been restored from a dump. The backups are proven readable, not proven
  restorable end to end.
- The `Every(interval)` schedule form has no production user yet; only `DailyAt` is
  exercised.
