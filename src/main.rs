use std::time::Duration;

use anyhow::Result;
use tokio::sync::oneshot;
use tracing::info;

use guardian_rs::alerts::engine::AlertEngine;
use guardian_rs::alerts::rules::rules_from_config;
use guardian_rs::collectors::cpu::CpuCollector;
use guardian_rs::collectors::memory::MemoryCollector;
use guardian_rs::collectors::temperature::TemperatureCollector;
use guardian_rs::collectors::Collector;
use guardian_rs::config;
use guardian_rs::notifiers::discord::DiscordNotifier;
use guardian_rs::notifiers::log::LogNotifier;
use guardian_rs::notifiers::Notifier;
use guardian_rs::services::orchestrator::Orchestrator;

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

    // Load configuration from YAML + env vars
    let config = config::load("configs/guardian.yaml")?;
    info!("GuardianRS configuration loaded");
    info!(
        poll_interval = config.daemon.poll_interval_secs,
        "Poll interval configured"
    );

    // === Collectors ===
    let cpu_collector = CpuCollector::new();
    let memory_collector = MemoryCollector::new();
    let temperature_collector = TemperatureCollector::new();
    let collectors: Vec<Box<dyn Collector>> = vec![
        Box::new(cpu_collector),
        Box::new(memory_collector),
        Box::new(temperature_collector),
    ];
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
        let webhook_url = config.notification.discord.webhook_url().to_string();
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
    let orchestrator =
        Orchestrator::new(collectors, Box::new(alert_engine), notifiers, poll_interval);

    info!("Starting GuardianRS daemon");

    // Create shutdown channel — first signal wins, duplicates are ignored
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

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
        // A single oneshot channel ensures first signal wins — duplicates are ignored.
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            tokio::spawn(async move {
                let mut sigterm =
                    signal(SignalKind::terminate()).expect("Failed to register SIGTERM handler");
                tokio::select! {
                    _ = sigterm.recv() => {
                        info!("SIGTERM received, initiating shutdown");
                    }
                    _ = tokio::signal::ctrl_c() => {
                        info!("Ctrl+C received, initiating shutdown");
                    }
                }
                let _ = shutdown_tx.send(());
            });
        }

        #[cfg(not(unix))]
        {
            tokio::spawn(async move {
                tokio::signal::ctrl_c()
                    .await
                    .expect("Failed to listen for ctrl+c");
                info!("Ctrl+C received, initiating shutdown");
                let _ = shutdown_tx.send(());
            });
        }

        // Run orchestrator with shutdown receiver
        orchestrator.run(shutdown_rx).await
    })?;

    Ok(())
}
