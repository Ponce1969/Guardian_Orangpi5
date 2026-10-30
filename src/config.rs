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

    #[error("Webhook URL was not resolved during config load")]
    WebhookNotResolved,
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
    /// Optional PostgreSQL health probe. An absent section means disabled.
    #[serde(default)]
    pub postgres: PostgresConfig,
    /// Scheduled maintenance. An absent section means disabled.
    #[serde(default)]
    pub maintenance: MaintenanceConfig,
}

/// Scheduled maintenance, as it appears in the configuration file.
///
/// This is only the shape. Turning it into runnable tasks is the job of
/// `services::maintenance::from_config`, which is the one place allowed to know
/// both the configuration types and the task types.
#[derive(Debug, Deserialize, Default)]
pub struct MaintenanceConfig {
    /// Whether the scheduler runs at all.
    #[serde(default)]
    pub enabled: bool,
    /// Tasks the scheduler may run.
    #[serde(default)]
    pub tasks: Vec<MaintenanceTaskConfig>,
}

/// One scheduled task.
///
/// The connection (host, port, user and the password variable) comes from the
/// `postgres` block, so a backup names only what is specific to it: the database
/// and where the file goes. Writing the credentials twice would create a second
/// place to keep in sync and a second place to leak.
#[derive(Debug, Deserialize)]
pub struct MaintenanceTaskConfig {
    pub name: String,
    /// The operation to run. Unknown kinds are a startup error, never a silent
    /// skip: a typo must not look like a scheduled backup.
    pub kind: String,
    /// Interval in seconds. Mutually exclusive with `every_hours` and `daily_at`.
    #[serde(default)]
    pub every_secs: Option<u64>,
    /// Interval in hours. Mutually exclusive with the other two.
    #[serde(default)]
    pub every_hours: Option<u64>,
    /// Local time of day, `HH:MM`. Mutually exclusive with the intervals.
    #[serde(default)]
    pub daily_at: Option<String>,
    /// Database this task backs up.
    pub database: String,
    /// Directory the dump is written to.
    #[serde(default = "default_destination_dir")]
    pub destination_dir: String,
    /// How many backups to keep, including the one just written.
    #[serde(default = "default_keep_backups")]
    pub keep_backups: usize,
    /// Deadline for the dump.
    #[serde(default = "default_task_timeout_secs")]
    pub timeout_secs: u64,
    /// Window between asking the process group to stop and forcing it.
    #[serde(default = "default_task_grace_secs")]
    pub grace_secs: u64,
    /// Deadline for `pg_restore --list`.
    #[serde(default = "default_verify_timeout_secs")]
    pub verify_timeout_secs: u64,
}

fn default_destination_dir() -> String {
    "/var/backups/guardianrs/postgres".to_string()
}

fn default_keep_backups() -> usize {
    7
}

fn default_task_timeout_secs() -> u64 {
    1800
}

fn default_task_grace_secs() -> u64 {
    30
}

fn default_verify_timeout_secs() -> u64 {
    120
}

#[derive(Debug, Deserialize)]
pub struct DaemonConfig {
    /// Seconds between each collection cycle.
    pub poll_interval_secs: u64,
}

/// Default network warning threshold in MB/s.
///
/// Sized for a saturated 1 GbE link (~119 MB/s theoretical), so the warning
/// fires before the link is fully consumed. Override it in the configuration
/// once real traffic is measured.
fn default_network_warning() -> f64 {
    80.0
}

/// Default network critical threshold in MB/s.
fn default_network_critical() -> f64 {
    110.0
}

/// Default warning for unhealthy Docker containers.
///
/// One broken container is worth telling the operator about immediately, so the
/// warning sits at a single container.
fn default_docker_unhealthy_warning() -> f64 {
    1.0
}

/// Default critical for unhealthy Docker containers.
fn default_docker_unhealthy_critical() -> f64 {
    2.0
}

/// Default warning for total Docker disk usage in GB.
///
/// Placeholder sized for a 128 GB root device shared with images, containers,
/// volumes and the build cache. Re-measure it on the target host.
fn default_docker_disk_warning() -> f64 {
    40.0
}

/// Default critical for total Docker disk usage in GB.
fn default_docker_disk_critical() -> f64 {
    80.0
}

/// Default warning for failed PostgreSQL probes.
///
/// Two of three probes failing means the database is mostly unreachable, while
/// a single failure is a transient the retry already absorbed.
fn default_postgres_failures_warning() -> f64 {
    2.0
}

/// Default critical for failed PostgreSQL probes: every probe failed.
fn default_postgres_failures_critical() -> f64 {
    3.0
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
    /// Network throughput (MB/s) that triggers a warning alert.
    ///
    /// Defaulted so a configuration written before network monitoring existed
    /// keeps loading unchanged.
    #[serde(default = "default_network_warning")]
    pub network_warning: f64,
    /// Network throughput (MB/s) that triggers a critical alert.
    #[serde(default = "default_network_critical")]
    pub network_critical: f64,
    /// Unhealthy Docker containers that trigger a warning alert.
    ///
    /// A container counts as unhealthy when it reports a failing healthcheck
    /// or is stuck restarting. An `exited` container does not count: a job
    /// that finishes is not an incident.
    #[serde(default = "default_docker_unhealthy_warning")]
    pub docker_unhealthy_warning: f64,
    /// Unhealthy Docker containers that trigger a critical alert.
    #[serde(default = "default_docker_unhealthy_critical")]
    pub docker_unhealthy_critical: f64,
    /// Docker disk usage (GB) that triggers a warning alert.
    #[serde(default = "default_docker_disk_warning")]
    pub docker_disk_warning: f64,
    /// Docker disk usage (GB) that triggers a critical alert.
    #[serde(default = "default_docker_disk_critical")]
    pub docker_disk_critical: f64,
    /// Failed PostgreSQL probes that trigger a warning alert.
    ///
    /// One failed probe out of three is absorbed on purpose: the database still
    /// answered, so it is degraded but alive.
    #[serde(default = "default_postgres_failures_warning")]
    pub postgres_failures_warning: f64,
    /// Failed PostgreSQL probes that trigger a critical alert.
    #[serde(default = "default_postgres_failures_critical")]
    pub postgres_failures_critical: f64,
}

fn default_postgres_host() -> String {
    "127.0.0.1".to_string()
}

fn default_postgres_port() -> u16 {
    5432
}

fn default_postgres_user() -> String {
    "guardian".to_string()
}

fn default_postgres_database() -> String {
    "postgres".to_string()
}

fn default_postgres_password_env() -> String {
    "GUARDIAN_PG_PASSWORD".to_string()
}

/// Default connect timeout for a single probe, in seconds.
///
/// Three failed probes are attempted per cycle, so this bounds the worst case
/// at three times this value and keeps it under the default poll interval.
fn default_postgres_connect_timeout_secs() -> u64 {
    3
}

/// PostgreSQL health-check configuration.
///
/// The password is never part of this struct's serialized form: it carries the
/// *name* of the environment variable that holds it, exactly like the Discord
/// webhook. A configuration file is therefore safe to commit.
#[derive(Debug, Deserialize)]
pub struct PostgresConfig {
    /// Whether the PostgreSQL probe runs at all.
    #[serde(default)]
    pub enabled: bool,
    /// Host of the server or container.
    #[serde(default = "default_postgres_host")]
    pub host: String,
    /// TCP port.
    #[serde(default = "default_postgres_port")]
    pub port: u16,
    /// Role used for the probe. It only needs permission to connect.
    #[serde(default = "default_postgres_user")]
    pub user: String,
    /// Database the probe connects to.
    #[serde(default = "default_postgres_database")]
    pub database: String,
    /// Name of the environment variable holding the password.
    #[serde(default = "default_postgres_password_env")]
    pub password_env: String,
    /// Seconds to wait for a single connection attempt.
    #[serde(default = "default_postgres_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// Resolved password, populated from the environment during config load.
    #[serde(skip)]
    resolved_password: Option<String>,
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_postgres_host(),
            port: default_postgres_port(),
            user: default_postgres_user(),
            database: default_postgres_database(),
            password_env: default_postgres_password_env(),
            connect_timeout_secs: default_postgres_connect_timeout_secs(),
            resolved_password: None,
        }
    }
}

impl PostgresConfig {
    /// Returns the resolved password.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::MissingEnv`] when the password was never
    /// resolved. [`load`] resolves it whenever `enabled` is set.
    pub fn password(&self) -> Result<&str, ConfigError> {
        self.resolved_password
            .as_deref()
            .ok_or_else(|| ConfigError::MissingEnv {
                var: self.password_env.clone(),
            })
    }
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
        }

        let raw = Raw::deserialize(deserializer)?;
        Ok(DiscordConfig {
            enabled: raw.enabled,
            webhook_url_env: raw.webhook_url_env,
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

    // The password follows the same contract as the webhook: the file carries
    // the variable name, never the value, and a missing variable is an
    // explicit load error rather than a runtime surprise.
    if config.postgres.enabled {
        let env_var = &config.postgres.password_env;
        let password = env::var(env_var).map_err(|_| ConfigError::MissingEnv {
            var: env_var.clone(),
        })?;
        config.postgres.resolved_password = Some(password);
    }

    Ok(config)
}

impl DiscordConfig {
    /// Returns the resolved webhook URL.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::WebhookNotResolved`] when the URL was never
    /// resolved. [`load`] resolves it whenever `enabled` is set, so this case
    /// is only reachable through a hand-built configuration.
    pub fn webhook_url(&self) -> Result<&str, ConfigError> {
        self.resolved_webhook_url
            .as_deref()
            .ok_or(ConfigError::WebhookNotResolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configuration written before network monitoring existed: it has no
    /// `network_*` thresholds, which is exactly what makes it the backwards
    /// compatibility fixture for deployed files.
    const LEGACY_YAML: &str = r#"
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
"#;

    #[test]
    fn parse_minimal_yaml() {
        let config: GuardianConfig = serde_yaml::from_str(LEGACY_YAML).expect("valid YAML");
        assert_eq!(config.daemon.poll_interval_secs, 30);
        assert_eq!(config.thresholds.cpu_warning, 80.0);
        assert!(!config.notification.discord.enabled);
    }

    /// Network thresholds arrived after this fixture was written, so parsing it
    /// unchanged is the backwards compatibility proof for every deployed
    /// configuration that predates network monitoring.
    fn load_from_temp(yaml: &str) -> Result<GuardianConfig, ConfigError> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);

        let mut path = std::env::temp_dir();
        path.push(format!(
            "guardian-config-{}-{}.yaml",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, yaml).expect("temp config written");

        let result = load(path.to_str().expect("utf-8 temp path"));
        let _ = std::fs::remove_file(&path);
        result
    }

    /// A configuration written before PostgreSQL existed must keep loading, and
    /// the probe must stay off instead of silently enabling itself.
    #[test]
    fn postgres_is_disabled_when_the_section_is_absent() {
        let config: GuardianConfig = serde_yaml::from_str(LEGACY_YAML).expect("legacy parses");

        assert!(!config.postgres.enabled);
        assert!(
            config.postgres.password().is_err(),
            "a disabled probe must not hold a password"
        );
    }

    /// The file carries the variable name, never the value, so parsing a
    /// configuration cannot produce a secret.
    #[test]
    fn postgres_yaml_cannot_carry_a_password() {
        let yaml = format!(
            "{LEGACY_YAML}\npostgres:\n  enabled: true\n  password_env: GUARDIAN_UNUSED_PG_VAR\n"
        );

        let config: GuardianConfig = serde_yaml::from_str(&yaml).expect("parses");

        assert!(config.postgres.enabled);
        assert!(
            config.postgres.password().is_err(),
            "reading YAML alone must never resolve a secret"
        );
    }

    #[test]
    fn load_resolves_the_postgres_password_from_the_environment() {
        const VAR: &str = "GUARDIAN_TEST_PG_PASSWORD_LOAD";
        std::env::set_var(VAR, "s3cret");

        let yaml = format!("{LEGACY_YAML}\npostgres:\n  enabled: true\n  password_env: {VAR}\n");
        let config = load_from_temp(&yaml).expect("configuration loads");

        assert_eq!(config.postgres.password().ok(), Some("s3cret"));

        std::env::remove_var(VAR);
    }

    #[test]
    fn load_fails_when_the_postgres_password_is_missing() {
        let yaml = format!(
            "{LEGACY_YAML}\npostgres:\n  enabled: true\n  password_env: GUARDIAN_SURELY_UNSET_PG_VAR\n"
        );

        let result = load_from_temp(&yaml);

        assert!(
            matches!(result, Err(ConfigError::MissingEnv { .. })),
            "a missing password must fail the load, not the first probe"
        );
    }

    #[test]
    fn network_thresholds_default_when_absent() {
        let config: GuardianConfig = serde_yaml::from_str(LEGACY_YAML).expect("legacy YAML parses");

        assert_eq!(config.thresholds.network_warning, 80.0);
        assert_eq!(config.thresholds.network_critical, 110.0);
    }

    /// An unresolved webhook URL must be reported as a recoverable condition,
    /// never as a panic: AGENTS.md and SPEC_ENGINEERING forbid `unwrap()` and
    /// `expect()` in production code.
    #[test]
    fn webhook_url_unresolved_is_recoverable() {
        let config = DiscordConfig {
            enabled: false,
            webhook_url_env: "DISCORD_WEBHOOK_URL".to_string(),
            resolved_webhook_url: None,
        };

        let resolved = config.webhook_url();

        assert!(
            matches!(resolved, Err(ConfigError::WebhookNotResolved)),
            "webhook_url() must report the unresolved URL instead of panicking, got {resolved:?}"
        );
    }
}
