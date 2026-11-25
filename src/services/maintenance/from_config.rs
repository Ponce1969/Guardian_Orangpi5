//! Turning configured maintenance tasks into runnable ones.
//!
//! The configuration module owns the shape; this module owns the meaning. Being
//! the only place that knows both sides keeps `config` free of service types and
//! `services` free of serde attributes.
//!
//! Everything here fails loudly. A typo in a schedule or a kind must stop the
//! daemon at startup, because the alternative is a monitor that runs for a week
//! with a backup silently missing from its schedule.

use std::path::PathBuf;
use std::time::Duration;

use crate::config::{MaintenanceTaskConfig, PostgresConfig};

use super::backup::BackupSettings;
use super::backup_task::BackupTask;
use super::schedule::DailyTime;
use super::scheduler::{ScheduledTask, TaskSchedule};

/// The only operation this build knows how to run.
pub const POSTGRES_BACKUP_KIND: &str = "postgres_backup";

/// Build every configured task, stopping at the first problem.
pub fn scheduled_tasks(
    tasks: &[MaintenanceTaskConfig],
    postgres: &PostgresConfig,
) -> Result<Vec<ScheduledTask>, String> {
    tasks.iter().map(|task| one_task(task, postgres)).collect()
}

fn one_task(
    task: &MaintenanceTaskConfig,
    postgres: &PostgresConfig,
) -> Result<ScheduledTask, String> {
    if task.kind != POSTGRES_BACKUP_KIND {
        return Err(format!(
            "task {}: unknown kind {:?}, this build supports only {:?}",
            task.name, task.kind, POSTGRES_BACKUP_KIND
        ));
    }

    let schedule = schedule_of(task)?;

    let instance = postgres
        .instances
        .iter()
        .find(|instance| instance.name == task.instance)
        .ok_or_else(|| {
            format!(
                "task {}: instance {:?} is not declared in postgres.instances",
                task.name, task.instance
            )
        })?;

    // The instance names the database; the task overrides it only when it backs up
    // something other than what the probe watches.
    let database = task
        .database
        .clone()
        .unwrap_or_else(|| instance.database.clone());

    if database.trim().is_empty()
        || database.contains('/')
        || database.contains('\\')
        || database.contains("..")
        || database.chars().any(char::is_control)
    {
        return Err(format!(
            "task {}: database must be a non-empty path-safe identifier",
            task.name
        ));
    }

    let password = instance
        .password()
        .map_err(|error| format!("task {}: {error}", task.name))?;

    let backup = BackupTask {
        task_name: task.name.clone(),
        settings: BackupSettings::new(
            &instance.host,
            instance.port,
            &instance.user,
            database,
            password,
        ),
        destination: PathBuf::from(&task.destination_dir),
        keep_backups: task.keep_backups,
        timeout: Duration::from_secs(task.timeout_secs),
        grace: Duration::from_secs(task.grace_secs),
        verify_timeout: Duration::from_secs(task.verify_timeout_secs),
    };

    Ok(ScheduledTask::new(task.name.clone(), schedule).with_backup(backup))
}

/// Exactly one schedule form: never none, never two.
fn schedule_of(task: &MaintenanceTaskConfig) -> Result<TaskSchedule, String> {
    let forms = [
        task.every_secs.is_some(),
        task.every_hours.is_some(),
        task.daily_at.is_some(),
    ];

    match forms.iter().filter(|is_set| **is_set).count() {
        0 => {
            return Err(format!(
                "task {}: needs exactly one of every_secs, every_hours or daily_at",
                task.name
            ))
        }
        1 => {}
        _ => {
            return Err(format!(
                "task {}: every_secs, every_hours and daily_at are mutually exclusive",
                task.name
            ))
        }
    }

    if let Some(secs) = task.every_secs {
        // A zero interval is a busy loop, not a schedule.
        if secs == 0 {
            return Err(format!(
                "task {}: every_secs must be greater than zero",
                task.name
            ));
        }
        return Ok(TaskSchedule::Every(Duration::from_secs(secs)));
    }

    if let Some(hours) = task.every_hours {
        if hours == 0 {
            return Err(format!(
                "task {}: every_hours must be greater than zero",
                task.name
            ));
        }

        let secs = hours.checked_mul(3600).ok_or_else(|| {
            format!(
                "task {}: every_hours is too large to express in seconds",
                task.name
            )
        })?;

        return Ok(TaskSchedule::Every(Duration::from_secs(secs)));
    }

    let raw = task.daily_at.as_deref().unwrap_or("");
    parse_daily(raw).map(TaskSchedule::DailyAt).ok_or_else(|| {
        format!(
            "task {}: daily_at {raw:?} is not a valid HH:MM time",
            task.name
        )
    })
}

fn parse_daily(raw: &str) -> Option<DailyTime> {
    let (hour, minute) = raw.split_once(':')?;

    let hour = hour.trim().parse::<u32>().ok()?;
    let minute = minute.trim().parse::<u32>().ok()?;

    DailyTime::new(hour, minute)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::config::{self, GuardianConfig};

    const PASSWORD_VAR: &str = "GUARDIAN_TEST_MAINTENANCE_PASSWORD";

    /// Load a full configuration through the real loader, so the tests exercise
    /// deserialization and the secret contract rather than a hand-built struct.
    fn loaded(maintenance: &str) -> Result<GuardianConfig, String> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);

        std::env::set_var(PASSWORD_VAR, "s3cret");

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
    enabled: false
    webhook_url_env: DISCORD_WEBHOOK_URL
postgres:
  enabled: true
  instances:
    - name: principal
      host: 127.0.0.1
      port: 5432
      database: postgres
      user: guardian
      password_env: {PASSWORD_VAR}
{maintenance}
"#
        );

        let mut path = std::env::temp_dir();
        path.push(format!(
            "guardian-maint-{}-{}.yaml",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::write(&path, yaml).expect("temp config");

        let result =
            config::load(path.to_str().expect("utf-8 path")).map_err(|error| error.to_string());
        let _ = std::fs::remove_file(&path);

        result
    }

    /// A task names the instance it backs up. The per-task connection keys this
    /// schema used to carry are gone, and an unknown key is a parse error now: this
    /// fixture kept writing them and only passed because they were dropped in silence.
    #[test]
    fn a_task_selects_its_instance() {
        let yaml = r#"
maintenance:
  enabled: true
  tasks:
    - name: auditor
      kind: postgres_backup
      instance: principal
      daily_at: "03:30"
      database: auditor_familiar
"#;
        let config = loaded(yaml).expect("valid configuration");

        let tasks = scheduled_tasks(&config.maintenance.tasks, &config.postgres)
            .expect("a task naming an instance is valid");

        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].backup.is_some());
    }

    /// Naming a variable that does not exist must stop the daemon rather than
    /// produce a task that fails at 03:30 with an error nobody is awake to read.
    /// A task naming an instance nobody declared must stop the daemon at startup,
    /// not become a run that fails at 03:30 with nobody awake to read it.
    #[test]
    fn an_unknown_instance_is_rejected() {
        let yaml = r#"
maintenance:
  enabled: true
  tasks:
    - name: auditor
      kind: postgres_backup
      instance: does_not_exist
      daily_at: "03:30"
"#;
        let error = loaded(yaml).expect_err("an unknown instance must be fatal at load");

        assert!(
            error.contains("does_not_exist"),
            "the error names the instance: {error}"
        );
    }

    const COMPLETE_TASK: &str = r#"
maintenance:
  enabled: true
  tasks:
    - name: nightly
      kind: postgres_backup
      instance: principal
      daily_at: "03:30"
      database: bookgraph
      destination_dir: /var/backups/guardianrs/postgres
      keep_backups: 7
      timeout_secs: 180
"#;

    /// Full YAML through the loader, then through the translation.
    #[test]
    fn a_complete_configuration_builds_a_runnable_task() {
        let config = loaded(COMPLETE_TASK).expect("valid configuration");

        let tasks =
            scheduled_tasks(&config.maintenance.tasks, &config.postgres).expect("tasks are valid");

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "nightly");
        assert!(tasks[0].backup.is_some(), "the payload is attached");
    }

    /// Only the required fields; everything else falls back to the documented
    /// safe defaults.
    #[test]
    fn optional_fields_fall_back_to_safe_defaults() {
        let minimal = r#"
maintenance:
  enabled: true
  tasks:
    - name: nightly
      kind: postgres_backup
      instance: principal
      every_hours: 24
      database: bookgraph
"#;
        let config = loaded(minimal).expect("valid configuration");
        let raw = &config.maintenance.tasks[0];

        assert_eq!(raw.destination_dir, "/var/backups/guardianrs/postgres");
        assert_eq!(raw.keep_backups, 7);
        assert_eq!(raw.timeout_secs, 1800);
        assert_eq!(raw.grace_secs, 30);
        assert_eq!(raw.verify_timeout_secs, 120);

        let tasks =
            scheduled_tasks(&config.maintenance.tasks, &config.postgres).expect("tasks are valid");
        assert_eq!(tasks.len(), 1);
    }

    /// A typo in a schedule must stop the daemon, not produce a task that never
    /// fires.
    #[test]
    fn an_invalid_daily_time_is_a_startup_error() {
        let broken = r#"
maintenance:
  enabled: true
  tasks:
    - name: nightly
      kind: postgres_backup
      instance: principal
      daily_at: "25:99"
      database: bookgraph
"#;
        let config = loaded(broken).expect("the YAML itself is valid");

        let error = scheduled_tasks(&config.maintenance.tasks, &config.postgres)
            .expect_err("an invalid time must be rejected");

        assert!(error.contains("daily_at"), "error was: {error}");
    }

    #[test]
    fn an_unknown_kind_is_a_startup_error() {
        let typo = r#"
maintenance:
  enabled: true
  tasks:
    - name: nightly
      kind: postgres_bakup
      instance: principal
      daily_at: "03:30"
      database: bookgraph
"#;
        let config = loaded(typo).expect("the YAML itself is valid");

        let error = scheduled_tasks(&config.maintenance.tasks, &config.postgres)
            .expect_err("a typo in the kind must be rejected");

        assert!(error.contains("unknown kind"), "error was: {error}");
    }

    #[test]
    fn two_schedule_forms_are_rejected() {
        let both = r#"
maintenance:
  enabled: true
  tasks:
    - name: nightly
      kind: postgres_backup
      instance: principal
      daily_at: "03:30"
      every_hours: 24
      database: bookgraph
"#;
        let config = loaded(both).expect("the YAML itself is valid");

        let error = scheduled_tasks(&config.maintenance.tasks, &config.postgres)
            .expect_err("two schedules must be rejected");

        assert!(error.contains("mutually exclusive"), "error was: {error}");
    }

    #[test]
    fn no_schedule_form_is_rejected() {
        let none = r#"
maintenance:
  enabled: true
  tasks:
    - name: nightly
      kind: postgres_backup
      instance: principal
      database: bookgraph
"#;
        let config = loaded(none).expect("the YAML itself is valid");

        let error = scheduled_tasks(&config.maintenance.tasks, &config.postgres)
            .expect_err("a task with no schedule must be rejected");

        assert!(error.contains("exactly one"), "error was: {error}");
    }

    #[test]
    fn a_zero_interval_is_rejected() {
        let zero = r#"
maintenance:
  enabled: true
  tasks:
    - name: nightly
      kind: postgres_backup
      instance: principal
      every_secs: 0
      database: bookgraph
"#;
        let config = loaded(zero).expect("the YAML itself is valid");

        let error = scheduled_tasks(&config.maintenance.tasks, &config.postgres)
            .expect_err("a busy loop is not a schedule");

        assert!(error.contains("greater than zero"), "error was: {error}");
    }
}
