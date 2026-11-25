//! Two databases of the same kind are two alerts, not one.
//!
//! The state tracker used to be keyed by `MetricKind` alone. With one PostgreSQL
//! instance that was invisible; with three it is a correctness bug:
//!
//! - `auditor_familiar` goes critical, and the alert fires.
//! - `barraca` goes critical too. Same key, `Critical -> Critical`, no transition,
//!   so the notification is swallowed and nobody learns about the second outage.
//! - `auditor_familiar` recovers. A `Recovered` is emitted for the metric, which
//!   reads as "PostgreSQL is fine" while `barraca` is still down.
//!
//! These tests use only the public API that exists today, so they compile against
//! the current code and fail because of the behaviour, not because of a missing
//! symbol.

use guardian_rs::alerts::{AlertEngine, AlertEvaluator, ThresholdRule};
use guardian_rs::metrics::{MetricKind, MetricSnapshot};

/// One rule for the PostgreSQL probe: warning at 2 failed probes, critical at 3.
fn engine() -> AlertEngine {
    AlertEngine::new(vec![ThresholdRule {
        metric: MetricKind::PostgresProbeFailures,
        warning: 2.0,
        critical: 3.0,
    }])
}

fn failing(database: &str) -> MetricSnapshot {
    MetricSnapshot::new(MetricKind::PostgresProbeFailures, 3.0).with_label("database", database)
}

fn healthy(database: &str) -> MetricSnapshot {
    MetricSnapshot::new(MetricKind::PostgresProbeFailures, 0.0).with_label("database", database)
}

#[test]
fn a_second_instance_is_not_swallowed_by_the_first() {
    let mut engine = engine();

    assert_eq!(
        engine.evaluate(&failing("auditor_familiar")).len(),
        1,
        "the first instance going critical is reported"
    );

    assert_eq!(
        engine.evaluate(&failing("barraca")).len(),
        1,
        "a second instance going critical deserves its own alert, not silence because a different instance already is"
    );
}

/// The false recovery: one instance recovering must not read as the whole metric
/// being healthy while another instance is still down.
#[test]
fn a_recovery_of_one_instance_does_not_recover_another() {
    let mut engine = engine();

    engine.evaluate(&failing("auditor_familiar"));
    engine.evaluate(&failing("barraca"));

    let recovery = engine.evaluate(&healthy("auditor_familiar"));

    assert_eq!(
        recovery.len(),
        1,
        "the recovery of auditor is reported once"
    );

    assert!(
        !recovery[0].message.contains("barraca"),
        "the recovery is about auditor, not about barraca: {}",
        recovery[0].message
    );

    // barraca is still critical, so feeding the same state again must stay silent.
    assert_eq!(
        engine.evaluate(&failing("barraca")).len(),
        0,
        "barraca is still critical, so there is no transition to report"
    );
}

/// Without the instance in the message, an operator reading Discord learns that
/// "postgres" is failing and still has to guess which of three databases it is.
#[test]
fn an_alert_names_the_instance_it_is_about() {
    let mut engine = engine();

    let events = engine.evaluate(&failing("web_2r_prod"));

    assert_eq!(events.len(), 1);
    assert!(
        events[0].message.contains("web_2r_prod"),
        "the message must say which database: got {:?}",
        events[0].message
    );
}

#[test]
fn collector_telemetry_labels_deduplicate_and_recover() {
    use guardian_rs::alerts::AlertSeverity;

    let mut engine = AlertEngine::new(vec![
        ThresholdRule {
            metric: MetricKind::MemoryUsage,
            warning: 85.0,
            critical: 95.0,
        },
        ThresholdRule {
            metric: MetricKind::DiskUsage,
            warning: 80.0,
            critical: 90.0,
        },
        ThresholdRule {
            metric: MetricKind::DockerUnhealthyContainers,
            warning: 1.0,
            critical: 2.0,
        },
    ]);

    // Memory collector attaches volatile `available_kb` on every tick.
    let mem_tick_1 = MetricSnapshot::new(MetricKind::MemoryUsage, 88.0)
        .with_label("total_kb", "16000000")
        .with_label("available_kb", "1920000");
    let mem_tick_2 = MetricSnapshot::new(MetricKind::MemoryUsage, 89.0)
        .with_label("total_kb", "16000000")
        .with_label("available_kb", "1760000");
    let mem_healthy = MetricSnapshot::new(MetricKind::MemoryUsage, 60.0)
        .with_label("total_kb", "16000000")
        .with_label("available_kb", "6400000");

    assert_eq!(engine.evaluate(&mem_tick_1).len(), 1);
    assert!(
        engine.evaluate(&mem_tick_2).is_empty(),
        "changing available_kb while still in Warning must not fire a duplicate alert"
    );
    let mem_recovered = engine.evaluate(&mem_healthy);
    assert_eq!(mem_recovered.len(), 1);
    assert_eq!(mem_recovered[0].severity, AlertSeverity::Recovered);

    // Disk collector attaches static `mount` + volatile `available_bytes`.
    let disk_warn_1 = MetricSnapshot::new(MetricKind::DiskUsage, 82.0)
        .with_label("mount", "/")
        .with_label("total_bytes", "100000000000")
        .with_label("available_bytes", "18000000000");
    let disk_warn_2 = MetricSnapshot::new(MetricKind::DiskUsage, 83.0)
        .with_label("mount", "/")
        .with_label("total_bytes", "100000000000")
        .with_label("available_bytes", "17000000000");
    let disk_ok = MetricSnapshot::new(MetricKind::DiskUsage, 50.0)
        .with_label("mount", "/")
        .with_label("total_bytes", "100000000000")
        .with_label("available_bytes", "50000000000");

    assert_eq!(engine.evaluate(&disk_warn_1).len(), 1);
    assert!(engine.evaluate(&disk_warn_2).is_empty());
    let disk_recovered = engine.evaluate(&disk_ok);
    assert_eq!(disk_recovered.len(), 1);
    assert_eq!(disk_recovered[0].severity, AlertSeverity::Recovered);

    // Docker collector changes `running` and `unhealthy` labels when recovering.
    let docker_sick = MetricSnapshot::new(MetricKind::DockerUnhealthyContainers, 1.0)
        .with_label("total", "5")
        .with_label("running", "4")
        .with_label("unhealthy", "1");
    let docker_healed = MetricSnapshot::new(MetricKind::DockerUnhealthyContainers, 0.0)
        .with_label("total", "5")
        .with_label("running", "5")
        .with_label("unhealthy", "0");

    assert_eq!(engine.evaluate(&docker_sick).len(), 1);
    let docker_recovered = engine.evaluate(&docker_healed);
    assert_eq!(docker_recovered.len(), 1);
    assert_eq!(docker_recovered[0].severity, AlertSeverity::Recovered);
    assert!(
        engine.active_alerts().is_empty(),
        "once all three series recover, zero active entries remain"
    );
}
