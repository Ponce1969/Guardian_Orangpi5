use std::time::Duration;

use anyhow::Result;
use tracing::info;

use guardian_rs::alerts::engine::AlertEngine;
use guardian_rs::alerts::rules::rules_from_config;
use guardian_rs::collectors::temperature::TemperatureCollector;
use guardian_rs::collectors::Collector;
use guardian_rs::config;
use guardian_rs::notifiers::discord::DiscordNotifier;
use guardian_rs::notifiers::log::LogNotifier;
use guardian_rs::notifiers::Notifier;
use guardian_rs::services::orchestrator::Orchestrator;

fn main() -> Result<()> {
    // Load .env file (does not crash if missing — production uses system env vars)
    dotenvy::dotenv().ok();

    // Initialize tracing subscriber
    // Default to debug level for pipeline observability.
    // Override with RUST_LOG=info or RUST_LOG=warn for less verbosity.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .init();

    // Load configuration from YAML + env vars
    let config = config::load("configs/guardian.yaml")?;
    info!("GuardianRS configuration loaded");
    info!(
        poll_interval = config.daemon.poll_interval_secs,
        "Poll interval configured"
    );
    tracing::debug!(
        cpu_warning = config.thresholds.cpu_warning,
        cpu_critical = config.thresholds.cpu_critical,
        memory_warning = config.thresholds.memory_warning,
        memory_critical = config.thresholds.memory_critical,
        disk_warning = config.thresholds.disk_warning,
        disk_critical = config.thresholds.disk_critical,
        temp_warning = config.thresholds.temp_warning,
        temp_critical = config.thresholds.temp_critical,
        "Threshold configuration"
    );

    // === Collectors ===
    let temperature_collector = TemperatureCollector::new();
    let collectors: Vec<Box<dyn Collector>> = vec![Box::new(temperature_collector)];
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
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(orchestrator.run())?;

    Ok(())
}
