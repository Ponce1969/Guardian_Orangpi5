use std::time::Duration;

use anyhow::Result;
use tokio::sync::watch;
use tracing::{error, info};

use guardian_rs::alerts::engine::AlertEngine;
use guardian_rs::alerts::rules::rules_from_config;
use guardian_rs::collectors::cpu::CpuCollector;
use guardian_rs::collectors::disk::DiskCollector;
use guardian_rs::collectors::docker::DockerCollector;
use guardian_rs::collectors::memory::MemoryCollector;
use guardian_rs::collectors::network::NetworkCollector;
use guardian_rs::collectors::postgres::PostgresCollector;
use guardian_rs::collectors::temperature::TemperatureCollector;
use guardian_rs::collectors::Collector;
use guardian_rs::config;
use guardian_rs::notifiers::discord::DiscordNotifier;
use guardian_rs::notifiers::log::LogNotifier;
use guardian_rs::notifiers::Notifier;
use guardian_rs::services::maintenance::{scheduled_tasks, MaintenanceScheduler, ProcessExecutor};
use guardian_rs::services::orchestrator::{Orchestrator, OrchestratorDeps};

fn main() -> Result<()> {
    // Load .env file (does not crash if missing — production uses systemd EnvironmentFile)
    dotenvy::dotenv().ok();

    // Initialize tracing subscriber.
    // Default level is info — clean, production-ready output.
    // Override with RUST_LOG=debug for detailed pipeline observability.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load configuration from YAML + env vars.
    // Try production path first (/opt/guardianrs/), then development path.
    let config_path = if std::path::Path::new("/opt/guardianrs/guardian.yaml").exists() {
        "/opt/guardianrs/guardian.yaml"
    } else {
        "configs/guardian.yaml"
    };
    let config = config::load(config_path)?;
    info!("GuardianRS configuration loaded");
    info!(
        poll_interval = config.daemon.poll_interval_secs,
        "Poll interval configured"
    );

    // === Collectors ===
    let cpu_collector = CpuCollector::new();
    let memory_collector = MemoryCollector::new();
    let disk_collector = DiskCollector::new();
    let temperature_collector = TemperatureCollector::new();
    let network_collector = NetworkCollector::new();
    let docker_collector = DockerCollector::new();
    let mut collectors: Vec<Box<dyn Collector>> = vec![
        Box::new(cpu_collector),
        Box::new(memory_collector),
        Box::new(disk_collector),
        Box::new(temperature_collector),
        Box::new(network_collector),
        Box::new(docker_collector),
    ];

    // The PostgreSQL probe is optional: it needs credentials the other
    // collectors do not, so a configuration without them must not stop the
    // rest of the monitoring.
    if config.postgres.enabled {
        match PostgresCollector::from_config(&config.postgres) {
            Ok(collector) => {
                info!(host = %config.postgres.host, database = %config.postgres.database, "PostgreSQL probe enabled");
                collectors.push(Box::new(collector));
            }
            Err(reason) => error!(
                reason = %reason,
                "PostgreSQL probe disabled: configuration incomplete"
            ),
        }
    } else {
        info!("PostgreSQL probe disabled");
    }
    info!(count = collectors.len(), "Collectors initialized");

    // === Alert Engine ===
    let rules = rules_from_config(&config.thresholds);
    let alert_engine = AlertEngine::new(rules);
    info!(
        rules = alert_engine.rule_count(),
        "Alert engine initialized"
    );

    // === Notifiers ===
    let mut notifiers: Vec<Box<dyn Notifier>> = vec![Box::new(LogNotifier::new())];

    if config.notification.discord.enabled {
        let webhook_url = config.notification.discord.webhook_url()?.to_string();
        let hostname = gethostname::gethostname()
            .into_string()
            .unwrap_or_else(|_| "unknown".to_string());
        let discord = DiscordNotifier::new(webhook_url, hostname);
        notifiers.push(Box::new(discord));
        info!("Discord notifier enabled");
    } else {
        info!("Discord notifier disabled");
    }

    // === Orchestrator ===
    let poll_interval = Duration::from_secs(config.daemon.poll_interval_secs);

    // Maintenance is built from configuration and fails fast: a typo in a
    // schedule must stop the daemon at startup, not become a monitor that runs
    // for a week with a backup silently missing from its schedule.
    let maintenance = if !config.maintenance.enabled {
        info!("Maintenance disabled by configuration");
        MaintenanceScheduler::default()
    } else {
        match scheduled_tasks(&config.maintenance.tasks, &config.postgres) {
            Ok(tasks) => {
                info!(tasks = tasks.len(), "Maintenance scheduled");
                MaintenanceScheduler::new(tasks)
            }
            Err(reason) => {
                error!(reason = %reason, "Invalid maintenance configuration");
                return Err(anyhow::anyhow!(
                    "invalid maintenance configuration: {reason}"
                ));
            }
        }
    };

    let orchestrator = Orchestrator::new(OrchestratorDeps {
        collectors,
        alert_engine: Box::new(alert_engine),
        notifiers,
        poll_interval,
        maintenance,
        executor: Box::new(ProcessExecutor::new()),
    });

    info!("Starting GuardianRS daemon");

    // Create shutdown channel — first signal wins, duplicates are ignored
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Build multi-thread runtime for signal handling.
    // worker_threads=2 is sufficient: one for signal listener, one for orchestrator tick.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    rt.block_on(async {
        // Spawn signal handler(s).
        // On Unix: listen for both SIGTERM (systemd) and Ctrl+C (interactive).
        // On other platforms: listen for Ctrl+C only.
        // A single watch channel: the first signal wins and every listener sees it.
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            tokio::spawn(async move {
                match signal(SignalKind::terminate()) {
                    Ok(mut sigterm) => {
                        tokio::select! {
                            _ = sigterm.recv() => {
                                info!("SIGTERM received, initiating shutdown");
                            }
                            _ = tokio::signal::ctrl_c() => {
                                info!("Ctrl+C received, initiating shutdown");
                            }
                        }
                        let _ = shutdown_tx.send(true);
                    }
                    Err(error) => {
                        // A failure inside one task must not terminate the daemon,
                        // and a silent failure is forbidden (SPEC_ENGINEERING).
                        error!(
                            %error,
                            "Failed to register SIGTERM handler; falling back to Ctrl+C only"
                        );
                        match tokio::signal::ctrl_c().await {
                            Ok(()) => {
                                info!("Ctrl+C received, initiating shutdown");
                                let _ = shutdown_tx.send(true);
                            }
                            Err(error) => error!(
                                %error,
                                "Failed to listen for Ctrl+C; graceful shutdown is unavailable"
                            ),
                        }
                    }
                }
            });
        }

        #[cfg(not(unix))]
        {
            tokio::spawn(async move {
                match tokio::signal::ctrl_c().await {
                    Ok(()) => {
                        info!("Ctrl+C received, initiating shutdown");
                        let _ = shutdown_tx.send(true);
                    }
                    Err(error) => error!(
                        %error,
                        "Failed to listen for Ctrl+C; graceful shutdown is unavailable"
                    ),
                }
            });
        }

        // Run orchestrator with shutdown receiver
        orchestrator.run(shutdown_rx).await
    })?;

    Ok(())
}
