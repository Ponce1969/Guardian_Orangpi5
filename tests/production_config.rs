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
    ("DISCORD_WEBHOOK_URL", "https://example.invalid/webhook"),
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
}
