//! The configuration this repository ships must actually load.
//!
//! `configs/guardian.yaml` is copied to the host by `scripts/deploy.sh`, so a
//! mistake in it reaches production without ever being compiled. A renamed field,
//! a dropped block or a typo in a schedule would only surface as a daemon that
//! refuses to start — at 03:30, on a machine nobody is watching.
//!
//! Loading it here turns that into a build failure.

use guardian_rs::config;
use guardian_rs::services::maintenance::scheduled_tasks;

/// The secrets the shipped configuration names in its `_env` fields.
const SECRETS: [(&str, &str); 4] = [
    (
        "DISCORD_WEBHOOK_URL",
        "https://discord.com/api/webhooks/000000000000000000/not-a-real-token",
    ),
    ("GUARDIAN_PG_AUDITOR_PASSWORD", "not-a-real-password"),
    ("GUARDIAN_PG_PEDIDOS_PASSWORD", "not-a-real-password"),
    ("GUARDIAN_PG_ECOMMERCE_PASSWORD", "not-a-real-password"),
];

#[test]
fn the_shipped_configuration_loads_and_its_tasks_are_runnable() {
    for (name, value) in SECRETS {
        std::env::set_var(name, value);
    }

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/configs/guardian.yaml");

    let config = config::load(path).expect("the shipped configuration must load");

    assert!(config.postgres.enabled, "the probe is part of the default");
    assert!(
        config.maintenance.enabled,
        "maintenance is part of the default"
    );
    assert!(
        !config.maintenance.tasks.is_empty(),
        "enabling maintenance with no tasks would be a no-op"
    );

    let tasks = scheduled_tasks(&config.maintenance.tasks, &config.postgres)
        .expect("the shipped tasks must pass validation");

    assert_eq!(
        config.postgres.instances.len(),
        3,
        "the shipped configuration watches the three projects"
    );
    assert_eq!(tasks.len(), config.maintenance.tasks.len());
    assert!(
        tasks.iter().all(|task| task.backup.is_some()),
        "every shipped task needs its payload attached"
    );
    assert!(config.digest.enabled, "daily digest is enabled by default");
    assert_eq!(config.digest.daily_at, "08:00");
}

#[test]
fn systemd_unit_enforces_umask_and_minimal_writable_paths() {
    let unit_path = concat!(env!("CARGO_MANIFEST_DIR"), "/systemd/guardianrs.service");
    let unit = std::fs::read_to_string(unit_path).expect("systemd unit exists");

    for required in [
        "After=network-online.target",
        "EnvironmentFile=-/opt/guardianrs/.env",
        "UMask=0077",
        "ProtectSystem=strict",
        "ProtectHome=true",
        "PrivateTmp=true",
        "NoNewPrivileges=true",
        "ProtectKernelTunables=true",
        "ProtectKernelModules=true",
        "ProtectControlGroups=true",
        "RestrictSUIDSGID=true",
        "LockPersonality=true",
        "ReadWritePaths=/var/backups/guardianrs /var/lib/guardianrs",
    ] {
        assert!(
            unit.lines().any(|line| line.trim() == required),
            "missing required systemd hardening directive: {required}"
        );
    }

    let lines: Vec<&str> = unit.lines().collect();
    let unit_section_end = lines
        .iter()
        .position(|line| line.trim() == "[Service]")
        .expect("[Service] section exists");
    assert!(
        lines[..unit_section_end]
            .iter()
            .any(|line| line.trim() == "StartLimitBurst=3"),
        "StartLimitBurst=3 must appear in [Unit] before [Service]"
    );

    assert!(
        !unit.contains("SupplementaryGroups=docker"),
        "docker group is root-equivalent and must not be enabled in the default unit"
    );

    let rw_line = unit
        .lines()
        .find(|line| line.trim_start().starts_with("ReadWritePaths="))
        .expect("ReadWritePaths directive");
    assert!(
        !rw_line.contains("/proc") && !rw_line.contains("/sys"),
        "kernel pseudo-filesystems must not be writable: {rw_line}"
    );
}

#[test]
fn deployment_scripts_and_gitattributes_enforce_security_hygiene() {
    let deploy = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/deploy.sh"))
        .expect("deploy.sh exists");
    assert!(
        deploy.contains(
            r#"install -m 0600 -o "${SERVICE_NAME}" -g "${SERVICE_NAME}" "$ENV_SOURCE" "${INSTALL_DIR}/.env""#
        ),
        "deploy.sh must install .env atomically with mode 0600 and service ownership"
    );
    assert!(
        !deploy.contains(r#"cp "$ENV_SOURCE" "${INSTALL_DIR}/.env""#),
        "deploy.sh must not create .env world-readable via cp before chmod"
    );
    assert!(
        deploy.contains("FORCE_ENV_OVERWRITE"),
        "deploy.sh must preserve existing production .env unless explicitly overridden"
    );
    assert!(
        deploy.contains(r#"chmod 700 "$BACKUP_DIR" "${BACKUP_DIR}/postgres" "$STATE_DIR""#),
        "deploy.sh must enforce 0700 on backup and state directories"
    );

    let verify_restore = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/verify-restore.sh"
    ))
    .expect("verify-restore.sh exists");
    assert!(verify_restore.contains("GUARDIAN_PG_AUDITOR_PASSWORD"));
    assert!(verify_restore.contains("GUARDIAN_PG_PASSWORD"));
    assert!(verify_restore.contains("strip_outer_quotes"));
    assert!(verify_restore.contains("guardianrs-${PROTECTED_DB}-"));
    assert!(
        verify_restore.contains("{{println $name}}") && verify_restore.contains("| head -1"),
        "verify-restore.sh must isolate a single Docker network name"
    );
    assert!(
        verify_restore.contains(r#"-v "$DUMP_DIR_ARG:$DUMP_DIR_ARG:ro""#)
            && verify_restore.contains(r#"test -f "$DUMP_PATH" && test -s "$DUMP_PATH""#),
        "verify-restore.sh must mount parent dir read-only and verify regular non-empty file"
    );
    assert!(
        !verify_restore.contains(r#"-v "$DUMP:$DUMP:ro""#),
        "mounting a missing file path directly makes dockerd create a root:root directory"
    );

    let verify_creds = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/verify-credentials.sh"
    ))
    .expect("verify-credentials.sh exists");
    assert!(verify_creds.contains("strip_outer_quotes"));

    let gitattributes =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/.gitattributes"))
            .expect(".gitattributes exists");
    for required_rule in [
        "*.sh text eol=lf",
        "*.service text eol=lf",
        "*.yaml text eol=lf",
        "*.yml text eol=lf",
    ] {
        assert!(
            gitattributes
                .lines()
                .any(|line| line.trim() == required_rule),
            "missing .gitattributes LF rule: {required_rule}"
        );
    }
}

async fn read_http_body(socket: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;

    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 2048];
    let mut header_end: Option<usize> = None;
    let mut content_length: usize = 0;

    loop {
        let n = match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);

        if header_end.is_none() {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let end = pos + 4;
                header_end = Some(end);
                let headers = String::from_utf8_lossy(&buf[..pos]);
                for line in headers.lines() {
                    if let Some((name, val)) = line.split_once(':') {
                        if name.trim().eq_ignore_ascii_case("content-length") {
                            content_length = val.trim().parse::<usize>().unwrap_or(0);
                        }
                    }
                }
            }
        }

        if let Some(end) = header_end {
            if buf.len() >= end + content_length {
                return String::from_utf8_lossy(&buf[end..end + content_length]).to_string();
            }
        }
    }

    if let Some(end) = header_end {
        String::from_utf8_lossy(&buf[end..]).to_string()
    } else {
        String::new()
    }
}

#[tokio::test]
async fn end_to_end_collect_transition_outbox_and_digest_reaches_webhook() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::watch;

    use async_trait::async_trait;
    use guardian_rs::alerts::engine::AlertEngine;
    use guardian_rs::alerts::ThresholdRule;
    use guardian_rs::collectors::{Collector, CollectorError};
    use guardian_rs::metrics::{MetricKind, MetricSnapshot};
    use guardian_rs::notifiers::DiscordNotifier;
    use guardian_rs::services::maintenance::{
        state, DailyTime, ExecutionError, ExecutionOutput, ExecutionRequest, Executor,
        MaintenanceScheduler, MaintenanceState, ScheduledTask, TaskSchedule, TaskState,
    };
    use guardian_rs::services::orchestrator::{Orchestrator, OrchestratorDeps};

    struct DegradedCpu;

    #[async_trait]
    impl Collector for DegradedCpu {
        fn name(&self) -> &str {
            "cpu"
        }

        async fn collect(&self) -> Result<MetricSnapshot, CollectorError> {
            Ok(MetricSnapshot::new(MetricKind::CpuUsage, 97.0))
        }
    }

    struct NoopExecutor;

    #[async_trait]
    impl Executor for NoopExecutor {
        async fn run(&self, _req: &ExecutionRequest) -> Result<ExecutionOutput, ExecutionError> {
            Ok(ExecutionOutput {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            })
        }
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind webhook listener");
    let addr = listener.local_addr().expect("addr");
    let online = Arc::new(AtomicBool::new(false));
    let payloads: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let online_srv = Arc::clone(&online);
    let payloads_srv = Arc::clone(&payloads);
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let body = read_http_body(&mut socket).await;
            if online_srv.load(Ordering::SeqCst) {
                payloads_srv.lock().expect("lock").push(body);
                let _ = socket
                    .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
            } else {
                let _ = socket
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndown")
                    .await;
            }
        }
    });

    let mut scratch = std::env::temp_dir();
    scratch.push(format!("guardian-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch");

    let discord = DiscordNotifier::new(
        format!("http://{addr}/api/webhooks/1/test-token"),
        "orangepi5".to_string(),
    )
    .with_retry_delays(Vec::new())
    .with_outbox_path(scratch.join("outbox.json"));

    let state_path = scratch.join("state.json");
    let now_local = chrono::Local::now().naive_local();
    // Seed wall-clock runs at `now_local` for two `DailyAt(00:00)` tasks (not due
    // again today and verified within 24h), while leaving `ecommerce_nightly`
    // unrecorded so Cycle 1 deterministically exercises Alert + Maintenance + Digest.
    state::save(
        &state_path,
        &MaintenanceState::new(vec![
            TaskState {
                name: "auditor_nightly".to_string(),
                last_run: now_local,
            },
            TaskState {
                name: "pedidos_nightly".to_string(),
                last_run: now_local,
            },
        ]),
    )
    .expect("seed state");

    let midnight = DailyTime::new(0, 0).expect("00:00");
    let scheduler = MaintenanceScheduler::new(vec![
        ScheduledTask::new("auditor_nightly", TaskSchedule::DailyAt(midnight)),
        ScheduledTask::new("pedidos_nightly", TaskSchedule::DailyAt(midnight)),
        ScheduledTask::new("ecommerce_nightly", TaskSchedule::DailyAt(midnight)),
    ]);

    let orchestrator = Orchestrator::new(OrchestratorDeps {
        collectors: vec![Box::new(DegradedCpu)],
        alert_engine: Box::new(AlertEngine::new(vec![ThresholdRule {
            metric: MetricKind::CpuUsage,
            warning: 80.0,
            critical: 95.0,
        }])),
        notifiers: vec![Box::new(discord)],
        poll_interval: Duration::from_millis(40),
        maintenance: scheduler,
        executor: Box::new(NoopExecutor),
        state_path: Some(state_path),
        startup_grace: Duration::ZERO,
        digest_at: Some(midnight),
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(async move { orchestrator.run(shutdown_rx).await });

    // Cycle 1 runs while webhook returns 503 -> both Critical alert and Digest spool to disk!
    tokio::time::sleep(Duration::from_millis(70)).await;
    assert!(
        payloads.lock().expect("lock").is_empty(),
        "nothing delivered while webhook is 503"
    );

    // Bring webhook online; next tick emits 0 new transitions, yet independent `flush` drains both!
    online.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(120)).await;

    let _ = shutdown_tx.send(true);
    let _ = handle.await;
    server.abort();

    let received = payloads.lock().expect("lock").clone();
    std::fs::remove_dir_all(&scratch).ok();

    assert_eq!(
        received.len(),
        3,
        "expected FIFO Alert + Maintenance + Daily Digest, got: {received:?}"
    );
    assert!(received[0].contains("CPU Usage Critical"));
    assert!(received[1].contains("ecommerce_nightly — SKIPPED"));
    assert!(received[2].contains("Daily Digest"));
    assert!(received[2].contains("`auditor_nightly` — verified"));
    assert!(received[2].contains("`pedidos_nightly` — verified"));
    assert!(received[2].contains("`ecommerce_nightly` — pending / did not run"));
    assert!(received[2].contains("CPU Usage"));
}
