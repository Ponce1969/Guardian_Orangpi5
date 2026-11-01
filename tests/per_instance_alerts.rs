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
