//! Which maintenance task runs on a given tick.
//!
//! Two rules live here, and both are decisions rather than mechanisms:
//!
//! - a task is due according to the two clocks described in [`super::schedule`],
//!   so an interval never moves when the wall clock jumps and a daily time never
//!   drifts when the monotonic clock does;
//! - **at most one task runs per tick**. A reboot can leave several tasks
//!   overdue together, and starting them in the same tick puts a `pg_dump` and
//!   everything else into one I/O window. The rest wait for the next tick, and
//!   the reported [`DueReason`] makes the spread visible instead of looking like
//!   a delay.
//!
//! Nothing here reads a clock or starts a process: the caller supplies both
//! instants and runs what it is told to.

use std::time::{Duration, Instant};

use chrono::NaiveDateTime;

use super::outcome::{should_notify, MaintenanceStatus};
use super::schedule::{daily_is_due, interval_is_due, DailyTime, DueReason};

/// How often a task wants to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskSchedule {
    /// Every `every`, measured on a monotonic clock.
    Every(Duration),
    /// Once per day at a local wall-clock time.
    DailyAt(DailyTime),
}

/// A scheduled task and what is known about its last run.
#[derive(Debug, Clone)]
pub struct ScheduledTask {
    pub name: String,
    pub schedule: TaskSchedule,
    last_run_wall: Option<NaiveDateTime>,
    last_run_mono: Option<Instant>,
    last_status: Option<MaintenanceStatus>,
}

impl ScheduledTask {
    /// A task that has never run.
    pub fn new(name: impl Into<String>, schedule: TaskSchedule) -> Self {
        Self {
            name: name.into(),
            schedule,
            last_run_wall: None,
            last_run_mono: None,
            last_status: None,
        }
    }

    /// The last status this task reported, if it has run.
    pub fn last_status(&self) -> Option<MaintenanceStatus> {
        self.last_status
    }

    /// Whether this task is due, and why.
    fn due(&self, now_wall: NaiveDateTime, now_mono: Instant) -> Option<DueReason> {
        match self.schedule {
            // The monotonic clock decides intervals: an NTP correction or a
            // daylight-saving jump must not move when the next run is due.
            TaskSchedule::Every(every) => {
                let elapsed = self
                    .last_run_mono
                    .map(|last| now_mono.saturating_duration_since(last));

                interval_is_due(every, elapsed)
            }
            // The wall clock decides daily times: 03:30 means half past three in
            // the morning, not an elapsed instant.
            TaskSchedule::DailyAt(at) => daily_is_due(at, self.last_run_wall, now_wall),
        }
    }
}

/// The set of maintenance tasks and the single-at-a-time dispatch rule.
#[derive(Debug, Default)]
pub struct MaintenanceScheduler {
    tasks: Vec<ScheduledTask>,
}

impl MaintenanceScheduler {
    pub fn new(tasks: Vec<ScheduledTask>) -> Self {
        Self { tasks }
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// The one task that should run this tick, with the reason, or `None`.
    ///
    /// At most one task is returned no matter how many are overdue; the others
    /// are picked up on later ticks. Declaration order breaks ties, so the
    /// choice is deterministic rather than dependent on iteration details.
    pub fn pick_due(
        &self,
        now_wall: NaiveDateTime,
        now_mono: Instant,
    ) -> Option<(usize, DueReason)> {
        self.tasks
            .iter()
            .enumerate()
            .find_map(|(index, task)| task.due(now_wall, now_mono).map(|reason| (index, reason)))
    }

    /// Record a finished run and report whether it deserves a notification.
    ///
    /// The notification rule itself lives in [`should_notify`]; this only feeds
    /// it the previous status and stores the new one.
    pub fn record(
        &mut self,
        index: usize,
        status: MaintenanceStatus,
        now_wall: NaiveDateTime,
        now_mono: Instant,
    ) -> bool {
        let Some(task) = self.tasks.get_mut(index) else {
            return false;
        };

        let notify = should_notify(task.last_status, status);

        task.last_run_wall = Some(now_wall);
        task.last_run_mono = Some(now_mono);
        task.last_status = Some(status);

        notify
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 3, day)
            .and_then(|date| date.and_hms_opt(hour, minute, 0))
            .expect("valid test timestamp")
    }

    fn daily(hour: u32, minute: u32) -> DailyTime {
        DailyTime::new(hour, minute).expect("valid daily time")
    }

    fn mono() -> Instant {
        Instant::now()
    }

    #[test]
    fn an_empty_scheduler_never_picks_anything() {
        let scheduler = MaintenanceScheduler::default();

        assert!(scheduler.pick_due(at(1, 3, 30), mono()).is_none());
        assert!(scheduler.is_empty());
    }

    #[test]
    fn a_task_that_never_ran_is_picked_with_its_reason() {
        let scheduler = MaintenanceScheduler::new(vec![ScheduledTask::new(
            "backup",
            TaskSchedule::DailyAt(daily(3, 30)),
        )]);

        assert_eq!(
            scheduler.pick_due(at(1, 3, 30), mono()),
            Some((0, DueReason::NeverRan))
        );
    }

    #[test]
    fn an_interval_that_has_not_elapsed_is_not_picked() {
        let mut scheduler = MaintenanceScheduler::new(vec![ScheduledTask::new(
            "backup",
            TaskSchedule::Every(Duration::from_secs(3600)),
        )]);
        let now = mono();
        scheduler.record(0, MaintenanceStatus::Success, at(1, 3, 0), now);

        assert!(scheduler
            .pick_due(at(1, 3, 1), now + Duration::from_secs(60))
            .is_none());
        assert_eq!(
            scheduler.pick_due(at(1, 3, 2), now + Duration::from_secs(3600)),
            Some((0, DueReason::IntervalElapsed))
        );
    }

    /// The rule this whole type exists for: a reboot leaves several tasks
    /// overdue, and only one of them may start.
    #[test]
    fn only_one_task_is_picked_even_when_several_are_overdue() {
        let scheduler = MaintenanceScheduler::new(vec![
            ScheduledTask::new("first", TaskSchedule::DailyAt(daily(3, 30))),
            ScheduledTask::new("second", TaskSchedule::DailyAt(daily(3, 30))),
            ScheduledTask::new("third", TaskSchedule::DailyAt(daily(3, 30))),
        ]);

        let picked = scheduler.pick_due(at(1, 9, 0), mono());

        assert_eq!(
            picked,
            Some((0, DueReason::NeverRan)),
            "one task, not three"
        );
    }

    /// ...and the rest follow on later ticks rather than being forgotten.
    #[test]
    fn later_ticks_pick_the_remaining_overdue_tasks() {
        let mut scheduler = MaintenanceScheduler::new(vec![
            ScheduledTask::new("first", TaskSchedule::DailyAt(daily(3, 30))),
            ScheduledTask::new("second", TaskSchedule::DailyAt(daily(3, 30))),
        ]);
        let now = mono();
        let wall = at(1, 9, 0);

        let first = scheduler.pick_due(wall, now).expect("first is due");
        scheduler.record(first.0, MaintenanceStatus::Success, wall, now);

        assert_eq!(
            scheduler.pick_due(wall, now),
            Some((1, DueReason::NeverRan)),
            "the second task gets the next tick"
        );
    }

    #[test]
    fn a_daily_task_becomes_due_once_its_moment_passes() {
        let mut scheduler = MaintenanceScheduler::new(vec![ScheduledTask::new(
            "backup",
            TaskSchedule::DailyAt(daily(3, 30)),
        )]);
        let now = mono();
        scheduler.record(0, MaintenanceStatus::Success, at(1, 3, 30), now);

        assert!(scheduler.pick_due(at(1, 23, 0), now).is_none());
        assert_eq!(
            scheduler.pick_due(at(2, 3, 31), now),
            Some((0, DueReason::ScheduledMomentReached))
        );
    }

    // === record: the notification decision ===

    #[test]
    fn the_first_failure_is_reported() {
        let mut scheduler = MaintenanceScheduler::new(vec![ScheduledTask::new(
            "t",
            TaskSchedule::Every(Duration::from_secs(1)),
        )]);

        assert!(scheduler.record(0, MaintenanceStatus::Failed, at(1, 3, 0), mono()));
        assert_eq!(
            scheduler.tasks[0].last_status(),
            Some(MaintenanceStatus::Failed)
        );
    }

    #[test]
    fn a_stable_success_is_silent_after_the_first() {
        let mut scheduler = MaintenanceScheduler::new(vec![ScheduledTask::new(
            "t",
            TaskSchedule::Every(Duration::from_secs(1)),
        )]);
        let now = mono();

        assert!(
            scheduler.record(0, MaintenanceStatus::Success, at(1, 3, 0), now),
            "the first success confirms the task works"
        );
        assert!(
            !scheduler.record(0, MaintenanceStatus::Success, at(2, 3, 0), now),
            "a repeated success is the noise being suppressed"
        );
    }

    #[test]
    fn a_persistent_failure_is_reported_every_time() {
        let mut scheduler = MaintenanceScheduler::new(vec![ScheduledTask::new(
            "t",
            TaskSchedule::Every(Duration::from_secs(1)),
        )]);
        let now = mono();

        assert!(scheduler.record(0, MaintenanceStatus::Failed, at(1, 3, 0), now));
        assert!(
            scheduler.record(0, MaintenanceStatus::Failed, at(2, 3, 0), now),
            "a backup that stayed broken must not go quiet"
        );
    }

    #[test]
    fn recovery_is_reported() {
        let mut scheduler = MaintenanceScheduler::new(vec![ScheduledTask::new(
            "t",
            TaskSchedule::Every(Duration::from_secs(1)),
        )]);
        let now = mono();
        scheduler.record(0, MaintenanceStatus::Failed, at(1, 3, 0), now);

        assert!(scheduler.record(0, MaintenanceStatus::Success, at(2, 3, 0), now));
    }
}
