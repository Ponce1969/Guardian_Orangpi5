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
/// Enabled by default so an existing configuration keeps behaving as it did; a
/// host that revokes socket access must say so here, or the collector will fail
/// on every cycle instead of staying quiet.
#[derive(Debug, Deserialize)]
pub struct DockerConfig {
    /// Whether the Docker collector runs at all.
    #[serde(default = "default_docker_collector_enabled")]
    pub enabled: bool,
}

fn default_docker_collector_enabled() -> bool {
    true
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
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
#[derive(Debug, Deserialize)]
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
        if config.postgres.instances.is_empty() {
            return Err(ConfigError::Invalid {
                details: "postgres.enabled is true but postgres.instances is empty".to_string(),
            });
        }

        let mut names: HashSet<String> = HashSet::new();

        for instance in &mut config.postgres.instances {
            if instance.name.trim().is_empty() {
                return Err(ConfigError::Invalid {
                    details: "every postgres instance needs a name".to_string(),
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
}
