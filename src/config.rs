use std::collections::HashSet;
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

    #[error("Invalid configuration: {details}")]
    Invalid { details: String },
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
    /// Docker monitoring. Reading the daemon socket grants a lot: a member of the
    /// docker group can start a privileged container and own the host, so a
    /// hardened deployment turns it off.
    #[serde(default)]
    pub docker: DockerConfig,
    /// Scheduled maintenance. An absent section means disabled.
    #[serde(default)]
    pub maintenance: MaintenanceConfig,
}

/// Docker monitoring settings.
///
/// Disabled by default (`I5.1`): reading `/var/run/docker.sock` requires
/// root-equivalent `docker` group membership, so socket access must be opted into
/// explicitly rather than attempted whenever the `docker:` block is absent.
#[derive(Debug, Deserialize, Default)]
pub struct DockerConfig {
    /// Whether the Docker collector runs at all.
    #[serde(default)]
    pub enabled: bool,
}

/// Scheduled maintenance, as it appears in the configuration file.
///
/// This is only the shape. Turning it into runnable tasks is the job of
/// `services::maintenance::from_config`, which is the one place allowed to know
/// both the configuration types and the task types.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceConfig {
    /// Whether the scheduler runs at all.
    #[serde(default)]
    pub enabled: bool,
    /// Where completed runs are recorded, so a restart does not repeat them.
    ///
    /// Absent keeps the record in memory, which means every restart re-runs every
    /// due task and the retention window fills with runs nobody scheduled.
    #[serde(default)]
    pub state_path: Option<String>,
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
#[serde(deny_unknown_fields)]
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
    /// Which entry of `postgres.instances` this task backs up.
    ///
    /// By name, so the connection exists once in the file. A name that is not
    /// declared is a startup error rather than a task that fails at 03:30.
    pub instance: String,
    /// Database to back up, when it differs from the instance's own.
    #[serde(default)]
    pub database: Option<String>,
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
    /// How long after start-up alerts are not evaluated.
    ///
    /// A host that just booted has PostgreSQL containers that are not answering
    /// yet, and reporting them every time trains the operator to ignore the
    /// channel. The window is bounded and it defers evaluation instead of
    /// discarding it, so an outage still present when it ends is reported.
    #[serde(default = "default_startup_grace_secs")]
    pub startup_grace_secs: u64,
}

fn default_startup_grace_secs() -> u64 {
    180
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

/// Default connect timeout for a single probe, in seconds.
///
/// Three failed probes are attempted per cycle, so this bounds the worst case
/// at three times this value and keeps it under the default poll interval.
fn default_postgres_connect_timeout_secs() -> u64 {
    3
}

/// PostgreSQL monitoring configuration.
///
/// The password is never part of the serialized form: each instance carries the
/// *name* of the environment variable that holds it, exactly like the Discord
/// webhook. A configuration file is therefore safe to commit.
#[derive(Debug, Deserialize, Default)]
pub struct PostgresConfig {
    /// Whether the PostgreSQL probes run at all.
    #[serde(default)]
    pub enabled: bool,
    /// One entry per database to watch.
    ///
    /// Empty while `enabled` is true is a startup error: it would mean the operator
    /// asked for monitoring and configured nothing to monitor.
    #[serde(default)]
    pub instances: Vec<PostgresInstanceConfig>,
}

/// One database to watch.
#[derive(Deserialize)]
pub struct PostgresInstanceConfig {
    /// What alerts call this database. Use the project name.
    ///
    /// Unique across instances: two entries sharing a name would produce alerts
    /// nobody can tell apart.
    pub name: String,
    /// Host of the server or container. Explicit, with no default: this decides a
    /// network destination, and an implicit destination is one nobody reviewed.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Role used for the probe. It only needs permission to connect.
    pub user: String,
    /// Database the probe connects to.
    pub database: String,
    /// Name of the environment variable holding the password.
    pub password_env: String,
    /// Seconds to wait for a single connection attempt.
    #[serde(default = "default_postgres_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// Resolved password, populated from the environment during config load.
    #[serde(skip)]
    resolved_password: Option<String>,
}

impl std::fmt::Debug for PostgresInstanceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresInstanceConfig")
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("database", &self.database)
            .field("password_env", &self.password_env)
            .field("connect_timeout_secs", &self.connect_timeout_secs)
            .field("resolved_password", &"***")
            .finish()
    }
}

impl PostgresInstanceConfig {
    /// Returns the resolved password.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::MissingEnv`] when the password was never resolved.
    /// [`load`] resolves it whenever the PostgreSQL probes are enabled.
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

pub struct DiscordConfig {
    /// Whether Discord notifications are enabled.
    pub enabled: bool,
    /// Name of the environment variable holding the webhook URL.
    /// The URL itself is never stored in YAML or code.
    pub webhook_url_env: String,
    /// Optional durable outbox path for queued notifications (`I2.2`).
    pub outbox_path: Option<String>,
    /// Resolved webhook URL, populated from the environment during config load.
    /// Not deserialized from YAML — secrets never touch the config file.
    resolved_webhook_url: Option<String>,
}

impl std::fmt::Debug for DiscordConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordConfig")
            .field("enabled", &self.enabled)
            .field("webhook_url_env", &self.webhook_url_env)
            .field("outbox_path", &self.outbox_path)
            .field("resolved_webhook_url", &"***")
            .finish()
    }
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
            #[serde(default)]
            outbox_path: Option<String>,
        }

        let raw = Raw::deserialize(deserializer)?;
        Ok(DiscordConfig {
            enabled: raw.enabled,
            webhook_url_env: raw.webhook_url_env,
            outbox_path: raw.outbox_path,
            resolved_webhook_url: None,
        })
    }
}

const ALLOWED_DISCORD_HOSTS: &[&str] = &[
    "discord.com",
    "discordapp.com",
    "ptb.discord.com",
    "canary.discord.com",
    "ptb.discordapp.com",
    "canary.discordapp.com",
];

fn validate_safe_identifier(field: &str, value: &str) -> Result<(), ConfigError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Invalid {
            details: format!("{field} must not be empty"),
        });
    }
    if value.contains('/')
        || value.contains('\\')
        || value.contains("..")
        || value.chars().any(char::is_control)
    {
        return Err(ConfigError::Invalid {
            details: format!(
                "{field} {:?} must not contain path separators, '..' or control characters",
                value
            ),
        });
    }
    Ok(())
}

fn validate_threshold_pair(
    name: &str,
    warning: f64,
    critical: f64,
    is_percentage: bool,
    allow_negative: bool,
) -> Result<(), ConfigError> {
    if !warning.is_finite() || !critical.is_finite() {
        return Err(ConfigError::Invalid {
            details: format!("{name} thresholds must be finite numbers"),
        });
    }
    if !allow_negative && (warning < 0.0 || critical < 0.0) {
        return Err(ConfigError::Invalid {
            details: format!("{name} thresholds must be non-negative"),
        });
    }
    if is_percentage && (warning > 100.0 || critical > 100.0) {
        return Err(ConfigError::Invalid {
            details: format!("{name} percentage thresholds must not exceed 100"),
        });
    }
    if warning >= critical {
        return Err(ConfigError::Invalid {
            details: format!(
                "{name} warning threshold ({warning}) must be strictly less than critical ({critical})"
            ),
        });
    }
    Ok(())
}

fn validate_thresholds(thresholds: &ThresholdsConfig) -> Result<(), ConfigError> {
    validate_threshold_pair(
        "cpu",
        thresholds.cpu_warning,
        thresholds.cpu_critical,
        true,
        false,
    )?;
    validate_threshold_pair(
        "memory",
        thresholds.memory_warning,
        thresholds.memory_critical,
        true,
        false,
    )?;
    validate_threshold_pair(
        "disk",
        thresholds.disk_warning,
        thresholds.disk_critical,
        true,
        false,
    )?;
    validate_threshold_pair(
        "temperature",
        thresholds.temp_warning,
        thresholds.temp_critical,
        false,
        true,
    )?;
    validate_threshold_pair(
        "network",
        thresholds.network_warning,
        thresholds.network_critical,
        false,
        false,
    )?;
    validate_threshold_pair(
        "docker_unhealthy",
        thresholds.docker_unhealthy_warning,
        thresholds.docker_unhealthy_critical,
        false,
        false,
    )?;
    validate_threshold_pair(
        "docker_disk",
        thresholds.docker_disk_warning,
        thresholds.docker_disk_critical,
        false,
        false,
    )?;
    validate_threshold_pair(
        "postgres_failures",
        thresholds.postgres_failures_warning,
        thresholds.postgres_failures_critical,
        false,
        false,
    )?;
    Ok(())
}

fn validate_discord_webhook_url(raw: &str) -> Result<String, ConfigError> {
    let trimmed = raw.trim();
    let Some(rest) = trimmed.strip_prefix("https://") else {
        return Err(ConfigError::Invalid {
            details: "Discord webhook URL must start with https://".to_string(),
        });
    };

    let Some((host, path_and_query)) = rest.split_once('/') else {
        return Err(ConfigError::Invalid {
            details: "Discord webhook URL is missing its path".to_string(),
        });
    };

    let host_lower = host.to_ascii_lowercase();
    if !ALLOWED_DISCORD_HOSTS.contains(&host_lower.as_str()) {
        return Err(ConfigError::Invalid {
            details: format!(
                "Discord webhook host {:?} is not an official discord.com/discordapp.com endpoint",
                host
            ),
        });
    }

    let path = path_and_query.split(['?', '#']).next().unwrap_or("");
    let Some(segments) = path.strip_prefix("api/webhooks/") else {
        return Err(ConfigError::Invalid {
            details: "Discord webhook URL path must begin with /api/webhooks/<id>/<token>"
                .to_string(),
        });
    };

    let mut parts = segments.split('/');
    let id = parts.next().unwrap_or("");
    let token = parts.next().unwrap_or("");
    if id.is_empty()
        || token.is_empty()
        || parts.next().is_some()
        || id.chars().any(char::is_whitespace)
        || token.chars().any(char::is_whitespace)
    {
        return Err(ConfigError::Invalid {
            details: "Discord webhook URL must carry non-empty /api/webhooks/<id>/<token> segments"
                .to_string(),
        });
    }

    Ok(trimmed.to_string())
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

    if config.daemon.poll_interval_secs == 0 {
        return Err(ConfigError::Invalid {
            details: "daemon.poll_interval_secs must be greater than zero".to_string(),
        });
    }

    validate_thresholds(&config.thresholds)?;

    // Resolve secrets from environment variables
    if config.notification.discord.enabled {
        let env_var = &config.notification.discord.webhook_url_env;
        let url = env::var(env_var).map_err(|_| ConfigError::MissingEnv {
            var: env_var.clone(),
        })?;
        let validated = validate_discord_webhook_url(&url)?;
        // Store the resolved URL inside the config for internal use.
        // This avoids repeated env lookups during dispatch.
        config.notification.discord.resolved_webhook_url = Some(validated);
    }

    // The password follows the same contract as the webhook: the file carries
    // the variable name, never the value, and a missing variable is an
    // explicit load error rather than a runtime surprise.
    if config.postgres.enabled {
        if config.postgres.instances.is_empty() {
            return Err(ConfigError::Invalid {
                details: "postgres.enabled is true but postgres.instances is empty".to_string(),
            });
        }

        let mut names: HashSet<String> = HashSet::new();

        for instance in &mut config.postgres.instances {
            validate_safe_identifier("postgres instance name", &instance.name)?;
            validate_safe_identifier("postgres instance database", &instance.database)?;
            if instance.connect_timeout_secs == 0 {
                return Err(ConfigError::Invalid {
                    details: format!(
                        "postgres instance {:?}: connect_timeout_secs must be greater than zero",
                        instance.name
                    ),
                });
            }

            if !names.insert(instance.name.clone()) {
                return Err(ConfigError::Invalid {
                    details: format!("postgres instance {:?} is declared twice", instance.name),
                });
            }

            let password =
                env::var(&instance.password_env).map_err(|_| ConfigError::MissingEnv {
                    var: instance.password_env.clone(),
                })?;
            instance.resolved_password = Some(password);
        }
    }

    // Referential check: a task naming an instance that does not exist would only
    // fail at 03:30, when nobody is awake to read it.
    if config.maintenance.enabled && !config.maintenance.tasks.is_empty() {
        if !config.postgres.enabled {
            return Err(ConfigError::Invalid {
                details: "maintenance tasks take their connection from postgres.instances, so postgres.enabled must be true".to_string(),
            });
        }

        let declared: HashSet<&str> = config
            .postgres
            .instances
            .iter()
            .map(|instance| instance.name.as_str())
            .collect();

        for task in &config.maintenance.tasks {
            validate_safe_identifier("maintenance task name", &task.name)?;
            if let Some(database) = &task.database {
                validate_safe_identifier("maintenance task database", database)?;
            }
            if !declared.contains(task.instance.as_str()) {
                return Err(ConfigError::Invalid {
                    details: format!(
                        "task {:?}: instance {:?} is not declared in postgres.instances",
                        task.name, task.instance
                    ),
                });
            }
        }
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
            config.postgres.instances.is_empty(),
            "a disabled probe must declare no instances"
        );
    }

    /// The file carries the variable name, never the value, so parsing a
    /// configuration cannot produce a secret.
    #[test]
    fn postgres_yaml_cannot_carry_a_password() {
        let yaml = format!(
            "{LEGACY_YAML}\npostgres:\n  enabled: true\n  instances:\n    - name: principal\n      host: 127.0.0.1\n      port: 5432\n      database: guardian\n      user: guardian\n      password_env: GUARDIAN_UNUSED_PG_VAR\n"
        );

        let config: GuardianConfig = serde_yaml::from_str(&yaml).expect("parses");

        assert!(config.postgres.enabled);
        assert!(
            config.postgres.instances[0].password().is_err(),
            "reading YAML alone must never resolve a secret"
        );
    }

    #[test]
    fn load_resolves_the_postgres_password_from_the_environment() {
        const VAR: &str = "GUARDIAN_TEST_PG_PASSWORD_LOAD";
        std::env::set_var(VAR, "s3cret");

        let yaml = format!("{LEGACY_YAML}\npostgres:\n  enabled: true\n  instances:\n    - name: principal\n      host: 127.0.0.1\n      port: 5432\n      database: guardian\n      user: guardian\n      password_env: {VAR}\n");
        let config = load_from_temp(&yaml).expect("configuration loads");

        assert_eq!(config.postgres.instances[0].password().ok(), Some("s3cret"));

        std::env::remove_var(VAR);
    }

    #[test]
    fn load_fails_when_the_postgres_password_is_missing() {
        let yaml = format!(
            "{LEGACY_YAML}\npostgres:\n  enabled: true\n  instances:\n    - name: principal\n      host: 127.0.0.1\n      port: 5432\n      database: guardian\n      user: guardian\n      password_env: GUARDIAN_SURELY_UNSET_PG_VAR\n"
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
            outbox_path: None,
            resolved_webhook_url: None,
        };

        let resolved = config.webhook_url();

        assert!(
            matches!(resolved, Err(ConfigError::WebhookNotResolved)),
            "webhook_url() must report the unresolved URL instead of panicking, got {resolved:?}"
        );
    }

    /// Asking for PostgreSQL monitoring and configuring nothing to monitor is an
    /// operator mistake that must stop the daemon, not a no-op that reports healthy
    /// monitoring forever.
    #[test]
    fn postgres_enabled_with_no_instances_is_rejected() {
        let yaml = format!(
            "{LEGACY_YAML}
postgres:
  enabled: true
  instances: []
"
        );

        let error = load_from_temp(&yaml).expect_err("no instances must be fatal");

        assert!(
            error.to_string().contains("instances"),
            "error was: {error}"
        );
    }

    /// Two instances sharing a name would produce alerts nobody can tell apart.
    #[test]
    fn duplicate_instance_names_are_rejected() {
        const VAR: &str = "GUARDIAN_TEST_DUPLICATE_PG";
        std::env::set_var(VAR, "s3cret");

        let yaml = format!(
            "{LEGACY_YAML}
postgres:
  enabled: true
  instances:
    - name: proyecto
      host: 127.0.0.1
      port: 5432
      database: uno
      user: guardian
      password_env: {VAR}
    - name: proyecto
      host: 127.0.0.1
      port: 5433
      database: dos
      user: guardian
      password_env: {VAR}
"
        );

        let error = load_from_temp(&yaml).expect_err("a duplicate name must be fatal");

        assert!(error.to_string().contains("twice"), "error was: {error}");

        std::env::remove_var(VAR);
    }

    /// A maintenance task whose instance nobody declared would only fail at 03:30,
    /// when nobody is awake to read the error.
    #[test]
    fn a_task_naming_an_undeclared_instance_is_rejected() {
        const VAR: &str = "GUARDIAN_TEST_TASK_INSTANCE_PG";
        std::env::set_var(VAR, "s3cret");

        let yaml = format!(
            "{LEGACY_YAML}
postgres:
  enabled: true
  instances:
    - name: auditor
      host: 127.0.0.1
      port: 5432
      database: auditor
      user: guardian
      password_env: {VAR}
maintenance:
  enabled: true
  tasks:
    - name: nocturno
      kind: postgres_backup
      daily_at: '03:30'
      instance: fantasma
"
        );

        let error = load_from_temp(&yaml).expect_err("an undeclared instance must be fatal");

        assert!(error.to_string().contains("fantasma"), "error was: {error}");

        std::env::remove_var(VAR);
    }

    /// A misspelled key under `maintenance` used to be swallowed: the daemon logged
    /// "Maintenance scheduled" and ran no backups at all, for as long as nobody looked.
    #[test]
    fn a_misspelled_maintenance_key_is_a_parse_error() {
        let yaml = format!(
            "{LEGACY_YAML}
maintenance:
  enabled: true
  task:
    - name: backup
      kind: postgres_backup
      daily_at: \"03:30\"
      database: db
"
        );

        let error = serde_yaml::from_str::<GuardianConfig>(&yaml)
            .expect_err("a misspelled tasks key must not be ignored");
        let message = error.to_string();

        assert!(
            message.contains("task"),
            "the error must name the offending key, got: {message}"
        );
    }

    #[test]
    fn docker_collector_is_disabled_by_default_when_omitted() {
        let config: GuardianConfig = serde_yaml::from_str(LEGACY_YAML).expect("legacy YAML parses");

        assert!(
            !config.docker.enabled,
            "omitting docker must default to disabled rather than probing the socket"
        );
    }

    #[test]
    fn debug_output_redacts_webhook_url_and_postgres_passwords() {
        const WEBHOOK_VAR: &str = "GUARDIAN_TEST_REDACT_WEBHOOK";
        const PG_VAR: &str = "GUARDIAN_TEST_REDACT_PG_PASS";
        const SECRET_URL: &str = "https://discord.com/api/webhooks/123456/super-secret-token";
        const SECRET_PASS: &str = "ultra-secret-pg-password";

        std::env::set_var(WEBHOOK_VAR, SECRET_URL);
        std::env::set_var(PG_VAR, SECRET_PASS);

        let yaml = format!(
            r#"
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
    enabled: true
    webhook_url_env: {WEBHOOK_VAR}
postgres:
  enabled: true
  instances:
    - name: auditor
      host: 127.0.0.1
      port: 5432
      database: auditor_familiar
      user: guardian
      password_env: {PG_VAR}
"#
        );

        let config = load_from_temp(&yaml).expect("valid config");
        let rendered = format!("{config:?}");

        std::env::remove_var(WEBHOOK_VAR);
        std::env::remove_var(PG_VAR);

        assert!(
            !rendered.contains(SECRET_URL) && !rendered.contains("super-secret-token"),
            "Debug leaked webhook URL: {rendered}"
        );
        assert!(
            !rendered.contains(SECRET_PASS),
            "Debug leaked Postgres password: {rendered}"
        );
        assert!(rendered.contains("auditor"));
    }

    #[test]
    fn zero_poll_interval_is_rejected() {
        let yaml = LEGACY_YAML.replace("poll_interval_secs: 30", "poll_interval_secs: 0");

        let error = load_from_temp(&yaml).expect_err("zero poll interval panics tokio::time");

        assert!(
            error.to_string().contains("poll_interval_secs"),
            "error was: {error}"
        );
    }

    #[test]
    fn inverted_or_non_finite_thresholds_are_rejected() {
        let inverted = LEGACY_YAML
            .replace("cpu_warning: 80", "cpu_warning: 95")
            .replace("cpu_critical: 95", "cpu_critical: 80");
        assert!(load_from_temp(&inverted).is_err());

        let over_hundred = LEGACY_YAML.replace("disk_critical: 90", "disk_critical: 120");
        assert!(load_from_temp(&over_hundred).is_err());

        let nan_yaml = LEGACY_YAML.replace("memory_warning: 85", "memory_warning: .nan");
        assert!(load_from_temp(&nan_yaml).is_err());
    }

    #[test]
    fn path_separators_in_database_or_instance_are_rejected() {
        const VAR: &str = "GUARDIAN_TEST_PATH_SAFE_PG";
        std::env::set_var(VAR, "s3cret");

        let traversal_db = format!(
            "{LEGACY_YAML}\npostgres:\n  enabled: true\n  instances:\n    - name: auditor\n      host: 127.0.0.1\n      port: 5432\n      database: ../etc/passwd\n      user: guardian\n      password_env: {VAR}\n"
        );
        assert!(load_from_temp(&traversal_db).is_err());

        let slash_instance = format!(
            "{LEGACY_YAML}\npostgres:\n  enabled: true\n  instances:\n    - name: bad/name\n      host: 127.0.0.1\n      port: 5432\n      database: auditor\n      user: guardian\n      password_env: {VAR}\n"
        );
        assert!(load_from_temp(&slash_instance).is_err());

        std::env::remove_var(VAR);
    }

    #[test]
    fn webhook_url_rejects_non_https_or_foreign_hosts() {
        const VAR: &str = "GUARDIAN_TEST_WEBHOOK_VALIDATION";
        let enabled_yaml = LEGACY_YAML.replace(
            "enabled: false\n    webhook_url_env: DISCORD_WEBHOOK_URL",
            &format!("enabled: true\n    webhook_url_env: {VAR}"),
        );

        for invalid in [
            "http://discord.com/api/webhooks/123/token",
            "https://evil.example.com/api/webhooks/123/token",
            "https://discord.com/api/webhooks/123/",
            "https://discord.com/other/path",
        ] {
            std::env::set_var(VAR, invalid);
            assert!(
                load_from_temp(&enabled_yaml).is_err(),
                "expected invalid webhook URL {invalid:?} to fail"
            );
        }

        std::env::set_var(
            VAR,
            "  https://discord.com/api/webhooks/123456789/valid-token  ",
        );
        let loaded = load_from_temp(&enabled_yaml).expect("valid Discord webhook URL loads");
        assert_eq!(
            loaded.notification.discord.webhook_url().ok(),
            Some("https://discord.com/api/webhooks/123456789/valid-token")
        );

        std::env::remove_var(VAR);
    }
}
