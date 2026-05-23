use std::env;
use std::fs;
use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to read config file {path}: {source}")]
    ReadFailed {
        path: String,
        source: std::io::Error,
    },

    #[error("Failed to parse config file {path}: {details}")]
    ParseFailed { path: String, details: String },

    #[error("Missing required environment variable: {var}")]
    MissingEnv { var: String },
}

/// Top-level GuardianRS configuration.
///
/// Loaded from a YAML file with secrets sourced from environment variables.
/// No hardcoded values — all operational parameters are externalized.
#[derive(Debug, Deserialize)]
pub struct GuardianConfig {
    pub daemon: DaemonConfig,
    pub thresholds: ThresholdsConfig,
    pub notification: NotificationConfig,
}

#[derive(Debug, Deserialize)]
pub struct DaemonConfig {
    /// Seconds between each collection cycle.
    pub poll_interval_secs: u64,
}

#[derive(Debug, Deserialize)]
pub struct ThresholdsConfig {
    /// CPU usage percent that triggers a warning alert.
    pub cpu_warning: f64,
    /// CPU usage percent that triggers a critical alert.
    pub cpu_critical: f64,
    /// Memory usage percent that triggers a warning alert.
    pub memory_warning: f64,
    /// Memory usage percent that triggers a critical alert.
    pub memory_critical: f64,
    /// Disk usage percent that triggers a warning alert.
    pub disk_warning: f64,
    /// Disk usage percent that triggers a critical alert.
    pub disk_critical: f64,
    /// SoC temperature (°C) that triggers a warning alert.
    pub temp_warning: f64,
    /// SoC temperature (°C) that triggers a critical alert.
    pub temp_critical: f64,
}

#[derive(Debug, Deserialize)]
pub struct NotificationConfig {
    pub discord: DiscordConfig,
}

#[derive(Debug)]
pub struct DiscordConfig {
    /// Whether Discord notifications are enabled.
    pub enabled: bool,
    /// Name of the environment variable holding the webhook URL.
    /// The URL itself is never stored in YAML or code.
    pub webhook_url_env: String,
    /// Minimum seconds between identical notification dispatches (dedup cooldown).
    pub min_interval_secs: u64,
    /// Resolved webhook URL, populated from the environment during config load.
    /// Not deserialized from YAML — secrets never touch the config file.
    resolved_webhook_url: Option<String>,
}

impl<'de> Deserialize<'de> for DiscordConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            enabled: bool,
            webhook_url_env: String,
            min_interval_secs: u64,
        }

        let raw = Raw::deserialize(deserializer)?;
        Ok(DiscordConfig {
            enabled: raw.enabled,
            webhook_url_env: raw.webhook_url_env,
            min_interval_secs: raw.min_interval_secs,
            resolved_webhook_url: None,
        })
    }
}

/// Load configuration from a YAML file, then resolve secrets from environment variables.
///
/// # Errors
///
/// Returns `ConfigError::ReadFailed` if the file cannot be read.
/// Returns `ConfigError::ParseFailed` if the YAML is invalid.
/// Returns `ConfigError::MissingEnv` if a required environment variable is unset.
pub fn load(path: &str) -> Result<GuardianConfig, ConfigError> {
    let path = Path::new(path);
    let raw = fs::read_to_string(path).map_err(|e| ConfigError::ReadFailed {
        path: path.display().to_string(),
        source: e,
    })?;

    let mut config: GuardianConfig =
        serde_yaml::from_str(&raw).map_err(|e| ConfigError::ParseFailed {
            path: path.display().to_string(),
            details: e.to_string(),
        })?;

    // Resolve secrets from environment variables
    if config.notification.discord.enabled {
        let env_var = &config.notification.discord.webhook_url_env;
        let url = env::var(env_var).map_err(|_| ConfigError::MissingEnv {
            var: env_var.clone(),
        })?;
        // Store the resolved URL inside the config for internal use.
        // This avoids repeated env lookups during dispatch.
        config.notification.discord.resolved_webhook_url = Some(url);
    }

    Ok(config)
}

impl DiscordConfig {
    /// Returns the resolved webhook URL.
    ///
    /// # Panics
    ///
    /// Panics if called before `load()` resolves the URL from the environment.
    /// In practice this cannot happen because `load()` sets the URL before
    /// any notifier is constructed.
    pub fn webhook_url(&self) -> &str {
        self.resolved_webhook_url
            .as_deref()
            .expect("webhook URL must be resolved during config load")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_yaml() {
        let yaml = r#"
daemon:
  poll_interval_secs: 30
thresholds:
  cpu_warning: 80
  cpu_critical: 95
  memory_warning: 85
  memory_critical: 95
  disk_warning: 80
  disk_critical: 90
  temp_warning: 70
  temp_critical: 80
notification:
  discord:
    enabled: false
    webhook_url_env: DISCORD_WEBHOOK_URL
    min_interval_secs: 300
"#;
        let config: GuardianConfig = serde_yaml::from_str(yaml).expect("valid YAML");
        assert_eq!(config.daemon.poll_interval_secs, 30);
        assert_eq!(config.thresholds.cpu_warning, 80.0);
        assert!(!config.notification.discord.enabled);
    }
}
