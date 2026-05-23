use anyhow::Result;
use tracing::info;

use guardian_rs::config;

fn main() -> Result<()> {
    // Load .env file (does not crash if missing — production uses system env vars)
    dotenvy::dotenv().ok();

    // Initialize tracing subscriber
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

    // TODO: wire up collectors, alert engine, notifiers, orchestrator
    // TODO: run orchestrator loop

    info!("GuardianRS started");
    Ok(())
}
