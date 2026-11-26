//! Daily human liveness digest (`I4.2`).
//!
//! Nightly maintenance (`03:30`–`03:40`) runs on machine time when nobody is
//! watching. The digest is the human signal (`08:00` host local time by default)
//! proving that collection, alert tracking, backup verification, and Discord
//! delivery are all alive after the maintenance window closes.

use std::time::Duration;

use chrono::{DateTime, Days, MappedLocalTime, NaiveDate, NaiveDateTime, TimeDelta, TimeZone, Utc};

use crate::alerts::{AlertKey, AlertState};
use crate::services::maintenance::{DailyTime, MaintenanceStatus, ScheduledTask, TaskSchedule};

/// Grace window past `daily_at` before a digest is marked as late catch-up.
pub const DEFAULT_LATE_GRACE: Duration = Duration::from_secs(300);

/// Discord rejects any embed field `value` exceeding 1024 characters with HTTP 400.
pub const MAX_DISCORD_FIELD_CHARS: usize = 1024;

fn cap_discord_field(text: String) -> String {
    if text.chars().count() <= MAX_DISCORD_FIELD_CHARS {
        return text;
    }
    const SUFFIX: &str = "\n… (truncated)";
    let budget = MAX_DISCORD_FIELD_CHARS.saturating_sub(SUFFIX.chars().count());
    let prefix: String = text.chars().take(budget).collect();
    format!("{prefix}{SUFFIX}")
}

/// Why and for which local calendar date a digest is firing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestTrigger {
    /// Local calendar date this digest covers.
    pub target_date: NaiveDate,
    /// Human-readable scheduled local slot (`YYYY-MM-DD HH:MM ±HH:MM`).
    pub scheduled_slot: String,
    /// Present when the daemon was down across the scheduled slot (`I4.2`).
    pub late_by: Option<Duration>,
}

/// Summary status of one configured maintenance task at digest time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DigestTaskState {
    Succeeded {
        ran_at: String,
    },
    Failed {
        ran_at: String,
    },
    TimedOut {
        ran_at: String,
    },
    /// Pending, still running, never ran in the current window, or `Skipped`.
    /// Never omitted so a missing task cannot masquerade as success (`I4.2`).
    PendingOrDidNotRun,
}

/// One maintenance line in the daily digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestTaskSummary {
    pub name: String,
    pub state: DigestTaskState,
}

/// One open alert line in the daily digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestAlertSummary {
    pub series: String,
    pub state: AlertState,
}

/// Complete daily liveness report dispatched through `Notifier::send_digest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestReport {
    pub generated_at: DateTime<Utc>,
    pub local_offset: String,
    pub target_date: NaiveDate,
    pub scheduled_slot: String,
    pub late_by: Option<Duration>,
    /// Only collectors that failed since the previous digest (`(name, count)`).
    pub collector_failures: Vec<(String, u64)>,
    /// Currently open `Warning` / `Critical` alerts.
    pub open_alerts: Vec<DigestAlertSummary>,
    /// Every configured maintenance task, never omitting unrun tasks.
    pub tasks: Vec<DigestTaskSummary>,
}

impl DigestReport {
    /// Format the collector health section without green-count noise (`I4.2`).
    pub fn format_collectors(&self) -> String {
        if self.collector_failures.is_empty() {
            return "No collector failures".to_string();
        }

        let rendered = self
            .collector_failures
            .iter()
            .map(|(name, count)| format!("• `{name}`: {count} failure(s)"))
            .collect::<Vec<_>>()
            .join("\n");
        cap_discord_field(rendered)
    }

    /// Format currently open alerts.
    pub fn format_open_alerts(&self) -> String {
        if self.open_alerts.is_empty() {
            return "No open alerts".to_string();
        }

        let rendered = self
            .open_alerts
            .iter()
            .map(|alert| {
                let badge = match alert.state {
                    AlertState::Critical => "🚨 CRITICAL",
                    AlertState::Warning => "⚠️ WARNING",
                    AlertState::Normal => "NORMAL",
                };
                format!("• {badge} — `{}`", alert.series)
            })
            .collect::<Vec<_>>()
            .join("\n");
        cap_discord_field(rendered)
    }

    /// Format one explicit line per configured maintenance task (`I4.2`).
    pub fn format_tasks(&self) -> String {
        if self.tasks.is_empty() {
            return "Maintenance disabled".to_string();
        }

        let rendered = self
            .tasks
            .iter()
            .map(|task| match &task.state {
                DigestTaskState::Succeeded { ran_at } => {
                    format!("• ✅ `{}` — verified ({ran_at})", task.name)
                }
                DigestTaskState::Failed { ran_at } => {
                    format!("• ❌ `{}` — failed ({ran_at})", task.name)
                }
                DigestTaskState::TimedOut { ran_at } => {
                    format!("• ⏳ `{}` — timed out ({ran_at})", task.name)
                }
                DigestTaskState::PendingOrDidNotRun => {
                    format!("• ⏸️ `{}` — pending / did not run", task.name)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        cap_discord_field(rendered)
    }

    /// Overall severity rank (`2` = critical, `1` = degraded/late, `0` = healthy).
    pub fn severity_rank(&self) -> u8 {
        let has_critical_alert = self
            .open_alerts
            .iter()
            .any(|alert| alert.state == AlertState::Critical);
        let has_failed_task = self
            .tasks
            .iter()
            .any(|task| matches!(task.state, DigestTaskState::Failed { .. }));

        if has_critical_alert || has_failed_task {
            return 2;
        }

        let has_degraded_task = self.tasks.iter().any(|task| {
            matches!(
                task.state,
                DigestTaskState::TimedOut { .. } | DigestTaskState::PendingOrDidNotRun
            )
        });

        if !self.open_alerts.is_empty()
            || !self.collector_failures.is_empty()
            || has_degraded_task
            || self.late_by.is_some()
        {
            return 1;
        }

        0
    }
}

/// Format an `AlertKey` concisely for digest display.
pub fn format_alert_series(key: &AlertKey) -> String {
    if key.labels().is_empty() {
        return key.kind().display_name().to_string();
    }
    let labels = key
        .labels()
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} ({labels})", key.kind().display_name())
}

/// Summarise configured maintenance tasks anchored to `target_date` and `digest_at`.
///
/// - For `TaskSchedule::DailyAt(task_at)`, the expected scheduled slot is
///   `target_date.and_time(task_at)` when `task_at <= digest_at`, or the preceding
///   day's `task_at` when `task_at > digest_at`. A recorded run counts iff it occurred
///   at or after `expected_slot` (and within 24 hours of it), preventing yesterday's
///   late catch-up run from masquerading as today's overnight backup (`I4.2`).
/// - For `TaskSchedule::Every(_)`, the window spans from the previous day's
///   `digest_at` through the end of `target_date`.
pub fn summarize_tasks(
    tasks: &[ScheduledTask],
    target_date: NaiveDate,
    digest_at: DailyTime,
) -> Vec<DigestTaskSummary> {
    let prev_date = target_date
        .checked_sub_days(Days::new(1))
        .unwrap_or(target_date);
    let every_start = prev_date.and_time(digest_at.time());
    let every_end = target_date
        .checked_add_days(Days::new(1))
        .and_then(|next| next.and_hms_opt(0, 0, 0))
        .unwrap_or_else(|| target_date.and_time(digest_at.time()));

    tasks
        .iter()
        .map(|task| {
            let in_window = |when: NaiveDateTime| match task.schedule {
                TaskSchedule::DailyAt(task_at) => {
                    let slot_date = if task_at.time() <= digest_at.time() {
                        target_date
                    } else {
                        prev_date
                    };
                    let expected_slot = slot_date.and_time(task_at.time());
                    when >= expected_slot && when < expected_slot + TimeDelta::hours(24)
                }
                TaskSchedule::Every(_) => when > every_start && when <= every_end,
            };

            let state = match task
                .recent_runs()
                .iter()
                .rev()
                .find(|(when, _)| in_window(*when))
            {
                Some((when, status)) => {
                    let stamp = when.format("%H:%M").to_string();
                    match status {
                        MaintenanceStatus::Success => DigestTaskState::Succeeded { ran_at: stamp },
                        MaintenanceStatus::Failed => DigestTaskState::Failed { ran_at: stamp },
                        MaintenanceStatus::TimedOut => DigestTaskState::TimedOut { ran_at: stamp },
                        MaintenanceStatus::Skipped => DigestTaskState::PendingOrDidNotRun,
                    }
                }
                None => DigestTaskState::PendingOrDidNotRun,
            };

            DigestTaskSummary {
                name: task.name.clone(),
                state,
            }
        })
        .collect()
}

/// Resolve `date` + `at` in `tz` deterministically across DST transitions (`I4.2`):
///
/// - `Single(dt)` -> exact local instant;
/// - `Ambiguous(earliest, _)` (autumn fall-back) -> first occurrence;
/// - `None` (spring-forward gap) -> first valid local instant after the jump.
pub fn resolve_local_slot<Z: TimeZone>(
    tz: &Z,
    date: NaiveDate,
    at: DailyTime,
) -> Option<DateTime<Z>> {
    let naive = date.and_time(at.time());
    match tz.from_local_datetime(&naive) {
        MappedLocalTime::Single(dt) => Some(dt),
        MappedLocalTime::Ambiguous(earliest, _latest) => Some(earliest),
        MappedLocalTime::None => {
            for step_mins in 1..=180_i64 {
                let candidate = naive + chrono::TimeDelta::minutes(step_mins);
                match tz.from_local_datetime(&candidate) {
                    MappedLocalTime::Single(dt) => return Some(dt),
                    MappedLocalTime::Ambiguous(earliest, _) => return Some(earliest),
                    MappedLocalTime::None => continue,
                }
            }
            None
        }
    }
}

/// Decide whether the daily digest is due at `now`, enforcing once-per-date
/// delivery, restart persistence, and late catch-up detection (`I4.2`).
pub fn evaluate_digest_due<Z>(
    tz: &Z,
    at: DailyTime,
    last_digest_date: Option<NaiveDate>,
    now: &DateTime<Z>,
    late_grace: Duration,
) -> Option<DigestTrigger>
where
    Z: TimeZone,
    Z::Offset: std::fmt::Display,
{
    let today = now.date_naive();

    if last_digest_date.is_some_and(|sent| sent >= today) {
        return None;
    }

    let today_slot = resolve_local_slot(tz, today, at)?;
    if now >= &today_slot {
        let elapsed = now
            .clone()
            .signed_duration_since(today_slot.clone())
            .to_std()
            .unwrap_or(Duration::ZERO);
        let late_by = (elapsed > late_grace).then_some(elapsed);
        return Some(DigestTrigger {
            target_date: today,
            scheduled_slot: format!(
                "{} {} {}",
                today.format("%Y-%m-%d"),
                at.time().format("%H:%M"),
                today_slot.offset()
            ),
            late_by,
        });
    }

    // Before today's scheduled moment: if a prior day's digest was missed while
    // the daemon was offline (`last_digest_date < yesterday`), emit one late
    // catch-up for yesterday without consuming today's upcoming 08:00 slot.
    let yesterday = today.checked_sub_days(Days::new(1))?;
    if let Some(sent) = last_digest_date {
        if sent < yesterday {
            let missed_slot = resolve_local_slot(tz, yesterday, at)?;
            let elapsed = now
                .clone()
                .signed_duration_since(missed_slot.clone())
                .to_std()
                .unwrap_or(Duration::ZERO);
            return Some(DigestTrigger {
                target_date: yesterday,
                scheduled_slot: format!(
                    "{} {} {}",
                    yesterday.format("%Y-%m-%d"),
                    at.time().format("%H:%M"),
                    missed_slot.offset()
                ),
                late_by: Some(elapsed),
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::maintenance::{state, MaintenanceState, TaskSchedule};
    use chrono::FixedOffset;

    /// Synthetic DST timezone:
    /// - Spring forward on 2026-03-29: 02:00..02:59 does not exist (jumps UTC-03 -> UTC-02).
    /// - Autumn fall-back on 2026-10-25: 02:00..02:59 occurs twice (UTC-02 then UTC-03).
    #[derive(Debug, Clone, Copy)]
    struct SyntheticDstZone;

    impl TimeZone for SyntheticDstZone {
        type Offset = FixedOffset;

        fn from_offset(_offset: &Self::Offset) -> Self {
            Self
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> MappedLocalTime<Self::Offset> {
            let summer_start = NaiveDate::from_ymd_opt(2026, 3, 29).expect("date");
            let summer_end = NaiveDate::from_ymd_opt(2026, 10, 25).expect("date");
            let std_off = FixedOffset::west_opt(3 * 3600).expect("offset");
            let dst_off = FixedOffset::west_opt(2 * 3600).expect("offset");

            if *local > summer_start && *local < summer_end {
                MappedLocalTime::Single(dst_off)
            } else {
                MappedLocalTime::Single(std_off)
            }
        }

        fn offset_from_local_datetime(
            &self,
            local: &NaiveDateTime,
        ) -> MappedLocalTime<Self::Offset> {
            let std_off = FixedOffset::west_opt(3 * 3600).expect("offset");
            let dst_off = FixedOffset::west_opt(2 * 3600).expect("offset");
            let spring_date = NaiveDate::from_ymd_opt(2026, 3, 29).expect("date");
            let fall_date = NaiveDate::from_ymd_opt(2026, 10, 25).expect("date");

            let date = local.date();
            let hour = local.time().hour();
            if date == spring_date {
                if hour < 2 {
                    MappedLocalTime::Single(std_off)
                } else if hour == 2 {
                    MappedLocalTime::None
                } else {
                    MappedLocalTime::Single(dst_off)
                }
            } else if date == fall_date {
                if hour < 2 {
                    MappedLocalTime::Single(dst_off)
                } else if hour == 2 {
                    MappedLocalTime::Ambiguous(dst_off, std_off)
                } else {
                    MappedLocalTime::Single(std_off)
                }
            } else if date > spring_date && date < fall_date {
                MappedLocalTime::Single(dst_off)
            } else {
                MappedLocalTime::Single(std_off)
            }
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> Self::Offset {
            let summer_start = NaiveDate::from_ymd_opt(2026, 3, 29).expect("date");
            let summer_end = NaiveDate::from_ymd_opt(2026, 10, 25).expect("date");
            if *utc > summer_start && *utc < summer_end {
                FixedOffset::west_opt(2 * 3600).expect("offset")
            } else {
                FixedOffset::west_opt(3 * 3600).expect("offset")
            }
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> Self::Offset {
            let spring_utc = NaiveDate::from_ymd_opt(2026, 3, 29)
                .and_then(|d| d.and_hms_opt(5, 0, 0))
                .expect("dt");
            let fall_utc = NaiveDate::from_ymd_opt(2026, 10, 25)
                .and_then(|d| d.and_hms_opt(5, 0, 0))
                .expect("dt");
            if *utc >= spring_utc && *utc < fall_utc {
                FixedOffset::west_opt(2 * 3600).expect("offset")
            } else {
                FixedOffset::west_opt(3 * 3600).expect("offset")
            }
        }
    }

    use chrono::Timelike;

    #[test]
    fn daily_digest_handles_dst_transitions_once_per_date() {
        let tz = SyntheticDstZone;
        let at_0230 = DailyTime::new(2, 30).expect("02:30");

        // 1. Spring forward: 02:30 is skipped (01:59 -> 03:00). Must fire once at 03:00.
        let spring_prev = NaiveDate::from_ymd_opt(2026, 3, 28);
        let before_jump = tz.from_utc_datetime(
            &NaiveDate::from_ymd_opt(2026, 3, 29)
                .and_then(|d| d.and_hms_opt(4, 59, 0))
                .expect("utc"),
        ); // 01:59 local (-03)
        let after_jump = tz.from_utc_datetime(
            &NaiveDate::from_ymd_opt(2026, 3, 29)
                .and_then(|d| d.and_hms_opt(5, 0, 10))
                .expect("utc"),
        ); // 03:00:10 local (-02)

        assert!(
            evaluate_digest_due(&tz, at_0230, spring_prev, &before_jump, DEFAULT_LATE_GRACE)
                .is_none()
        );
        let spring_fire =
            evaluate_digest_due(&tz, at_0230, spring_prev, &after_jump, DEFAULT_LATE_GRACE)
                .expect("skipped DST hour must still fire once after the jump");
        assert_eq!(
            spring_fire.target_date,
            NaiveDate::from_ymd_opt(2026, 3, 29).expect("date")
        );
        assert!(evaluate_digest_due(
            &tz,
            at_0230,
            Some(spring_fire.target_date),
            &after_jump,
            DEFAULT_LATE_GRACE
        )
        .is_none());

        // 2. Autumn fall-back: 02:30 happens twice (04:30 UTC in -02, then 05:30 UTC in -03).
        let fall_prev = NaiveDate::from_ymd_opt(2026, 10, 24);
        let first_0230 = tz.from_utc_datetime(
            &NaiveDate::from_ymd_opt(2026, 10, 25)
                .and_then(|d| d.and_hms_opt(4, 30, 5))
                .expect("utc"),
        );
        let second_0230 = tz.from_utc_datetime(
            &NaiveDate::from_ymd_opt(2026, 10, 25)
                .and_then(|d| d.and_hms_opt(5, 30, 5))
                .expect("utc"),
        );

        let fall_fire =
            evaluate_digest_due(&tz, at_0230, fall_prev, &first_0230, DEFAULT_LATE_GRACE)
                .expect("fires on first occurrence");
        assert!(
            evaluate_digest_due(
                &tz,
                at_0230,
                Some(fall_fire.target_date),
                &second_0230,
                DEFAULT_LATE_GRACE
            )
            .is_none(),
            "repeated local hour must never send a duplicate digest"
        );
    }

    #[test]
    fn digest_persists_across_restart_and_marks_late_catchup() {
        let tz = FixedOffset::west_opt(3 * 3600).expect("offset");
        let at_0800 = DailyTime::new(8, 0).expect("08:00");

        let mut dir = std::env::temp_dir();
        dir.push(format!("guardian-digest-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let state_path = dir.join("state.json");

        let today = NaiveDate::from_ymd_opt(2026, 11, 25).expect("date");
        let yesterday = NaiveDate::from_ymd_opt(2026, 11, 24).expect("date");

        // Persist today's completed digest and simulate restart at 08:05 -> must NOT re-send.
        let saved = MaintenanceState::new(Vec::new()).with_last_digest_date(Some(today));
        state::save(&state_path, &saved).expect("save");
        let loaded = state::load(&state_path).expect("load");

        let at_0805 = tz
            .with_ymd_and_hms(2026, 11, 25, 8, 5, 0)
            .single()
            .expect("08:05");
        assert!(evaluate_digest_due(
            &tz,
            at_0800,
            loaded.last_digest_date(),
            &at_0805,
            DEFAULT_LATE_GRACE
        )
        .is_none());

        // Restart at 07:59 with yesterday's digest recorded -> waits, then fires at 08:00.
        let at_0759 = tz
            .with_ymd_and_hms(2026, 11, 25, 7, 59, 0)
            .single()
            .expect("07:59");
        assert!(
            evaluate_digest_due(&tz, at_0800, Some(yesterday), &at_0759, DEFAULT_LATE_GRACE)
                .is_none()
        );
        let at_0800_exact = tz
            .with_ymd_and_hms(2026, 11, 25, 8, 0, 10)
            .single()
            .expect("08:00:10");
        let on_time = evaluate_digest_due(
            &tz,
            at_0800,
            Some(yesterday),
            &at_0800_exact,
            DEFAULT_LATE_GRACE,
        )
        .expect("fires at 08:00");
        assert!(on_time.late_by.is_none());

        // Host was down at 08:00 and boots at 09:30 -> fires once with late catch-up note.
        let at_0930 = tz
            .with_ymd_and_hms(2026, 11, 25, 9, 30, 0)
            .single()
            .expect("09:30");
        let catchup =
            evaluate_digest_due(&tz, at_0800, Some(yesterday), &at_0930, DEFAULT_LATE_GRACE)
                .expect("late catchup fires");
        assert_eq!(catchup.late_by, Some(Duration::from_secs(90 * 60)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn digest_reports_unrun_tasks_as_did_not_run_and_failed_collectors_only() {
        let now_local = NaiveDate::from_ymd_opt(2026, 11, 25)
            .and_then(|d| d.and_hms_opt(8, 0, 0))
            .expect("08:00");
        let ran_0330 = NaiveDate::from_ymd_opt(2026, 11, 25)
            .and_then(|d| d.and_hms_opt(3, 30, 0))
            .expect("03:30");

        let mut scheduler = crate::services::maintenance::MaintenanceScheduler::new(vec![
            ScheduledTask::new(
                "auditor_nightly",
                TaskSchedule::DailyAt(DailyTime::new(3, 30).expect("t")),
            ),
            ScheduledTask::new(
                "pedidos_nightly",
                TaskSchedule::DailyAt(DailyTime::new(3, 35).expect("t")),
            ),
            ScheduledTask::new(
                "ecommerce_nightly",
                TaskSchedule::DailyAt(DailyTime::new(3, 40).expect("t")),
            ),
        ]);
        scheduler.record(
            0,
            MaintenanceStatus::Success,
            ran_0330,
            std::time::Instant::now(),
        );
        scheduler.record(
            1,
            MaintenanceStatus::Skipped,
            ran_0330,
            std::time::Instant::now(),
        );
        // Task 2 (`ecommerce_nightly`) never ran at all!

        let digest_at = DailyTime::new(8, 0).expect("08:00");
        let tasks = summarize_tasks(scheduler.tasks(), now_local.date(), digest_at);
        let healthy_collectors_report = DigestReport {
            generated_at: Utc::now(),
            local_offset: "-03:00".to_string(),
            target_date: now_local.date(),
            scheduled_slot: "2026-11-25 08:00 -03:00".to_string(),
            late_by: None,
            collector_failures: Vec::new(),
            open_alerts: Vec::new(),
            tasks: tasks.clone(),
        };

        assert_eq!(
            healthy_collectors_report.format_collectors(),
            "No collector failures",
            "must not emit 'N collectors active' green noise"
        );

        let rendered_tasks = healthy_collectors_report.format_tasks();
        assert!(rendered_tasks.contains("`auditor_nightly` — verified (03:30)"));
        assert!(
            rendered_tasks.contains("`pedidos_nightly` — pending / did not run"),
            "skipped task must render explicitly as pending / did not run: {rendered_tasks}"
        );
        assert!(
            rendered_tasks.contains("`ecommerce_nightly` — pending / did not run"),
            "unrun task must never be omitted: {rendered_tasks}"
        );

        let with_collector_failure = DigestReport {
            collector_failures: vec![("temperature".to_string(), 4)],
            ..healthy_collectors_report
        };
        assert_eq!(
            with_collector_failure.format_collectors(),
            "• `temperature`: 4 failure(s)"
        );
    }

    #[test]
    fn yesterdays_late_catchup_run_does_not_satisfy_todays_digest_slot() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 25).expect("today");
        let yesterday_catchup = NaiveDate::from_ymd_opt(2026, 11, 24)
            .and_then(|d| d.and_hms_opt(9, 0, 0))
            .expect("yesterday 09:00");

        let mut scheduler =
            crate::services::maintenance::MaintenanceScheduler::new(vec![ScheduledTask::new(
                "auditor_nightly",
                TaskSchedule::DailyAt(DailyTime::new(3, 30).expect("03:30")),
            )]);
        scheduler.record(
            0,
            MaintenanceStatus::Success,
            yesterday_catchup,
            std::time::Instant::now(),
        );

        let digest_at = DailyTime::new(8, 0).expect("08:00");
        let tasks = summarize_tasks(scheduler.tasks(), today, digest_at);
        let report = DigestReport {
            generated_at: Utc::now(),
            local_offset: "-03:00".to_string(),
            target_date: today,
            scheduled_slot: "2026-11-25 08:00 -03:00".to_string(),
            late_by: None,
            collector_failures: Vec::new(),
            open_alerts: Vec::new(),
            tasks,
        };

        let rendered = report.format_tasks();
        assert!(
            rendered.contains("`auditor_nightly` — pending / did not run"),
            "yesterday's 09:00 catch-up must not satisfy today's 03:30 slot: {rendered}"
        );
    }

    #[test]
    fn digest_fields_are_capped_to_discord_1024_char_limit() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 25).expect("today");
        let report = DigestReport {
            generated_at: Utc::now(),
            local_offset: "+00:00".to_string(),
            target_date: today,
            scheduled_slot: "2026-11-25 08:00 +00:00".to_string(),
            late_by: None,
            collector_failures: (0..80)
                .map(|i| (format!("collector_instance_number_{i}"), 3))
                .collect(),
            open_alerts: (0..80)
                .map(|i| DigestAlertSummary {
                    series: format!("PostgreSQL Probe Failures (instance=database_replica_{i})"),
                    state: AlertState::Critical,
                })
                .collect(),
            tasks: (0..80)
                .map(|i| DigestTaskSummary {
                    name: format!("nightly_backup_task_for_project_{i}"),
                    state: DigestTaskState::PendingOrDidNotRun,
                })
                .collect(),
        };

        assert!(report.format_collectors().chars().count() <= MAX_DISCORD_FIELD_CHARS);
        assert!(report.format_open_alerts().chars().count() <= MAX_DISCORD_FIELD_CHARS);
        assert!(report.format_tasks().chars().count() <= MAX_DISCORD_FIELD_CHARS);
    }

    #[test]
    fn yesterdays_catchup_digest_preserves_yesterdays_backup_after_todays_early_run() {
        let day1 = NaiveDate::from_ymd_opt(2026, 11, 24).expect("day1");
        let day2 = NaiveDate::from_ymd_opt(2026, 11, 25).expect("day2");
        let day1_run = day1.and_hms_opt(3, 30, 0).expect("03:30");
        let day2_early_run = day2.and_hms_opt(6, 3, 0).expect("06:03");
        let task_at = DailyTime::new(3, 30).expect("03:30");
        let digest_at = DailyTime::new(8, 0).expect("08:00");

        let mut scheduler =
            crate::services::maintenance::MaintenanceScheduler::new(vec![ScheduledTask::new(
                "auditor_nightly",
                TaskSchedule::DailyAt(task_at),
            )]);
        let now_mono = std::time::Instant::now();
        scheduler.record(0, MaintenanceStatus::Success, day1_run, now_mono);
        scheduler.record(0, MaintenanceStatus::Success, day2_early_run, now_mono);

        // Persisting and restoring across restart must retain both runs for catch-up digest.
        let exported = scheduler.export_state();
        let mut dir = std::env::temp_dir();
        dir.push(format!("guardian-digest-catchup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let state_path = dir.join("state.json");
        state::save(&state_path, &exported).expect("save");
        let loaded = state::load(&state_path).expect("load");
        std::fs::remove_dir_all(&dir).ok();

        let mut restored =
            crate::services::maintenance::MaintenanceScheduler::new(vec![ScheduledTask::new(
                "auditor_nightly",
                TaskSchedule::DailyAt(task_at),
            )]);
        restored.restore(&loaded, day2_early_run, now_mono);

        let catchup_summary = summarize_tasks(restored.tasks(), day1, digest_at);
        assert_eq!(
            catchup_summary,
            vec![DigestTaskSummary {
                name: "auditor_nightly".to_string(),
                state: DigestTaskState::Succeeded {
                    ran_at: "03:30".to_string(),
                },
            }],
            "Day 2 early run at 06:03 must not erase Day 1 03:30 run from Day 1 catch-up digest"
        );

        let today_summary = summarize_tasks(restored.tasks(), day2, digest_at);
        assert_eq!(
            today_summary,
            vec![DigestTaskSummary {
                name: "auditor_nightly".to_string(),
                state: DigestTaskState::Succeeded {
                    ran_at: "06:03".to_string(),
                },
            }]
        );
    }
}
