//! Schedule definitions and the pure rules that decide whether a task is due.
//!
//! Two schedule forms are supported, and they deliberately do **not** share a
//! clock:
//!
//! - an interval is measured on a monotonic clock, because a wall-clock jump
//!   (NTP correction, daylight saving) must not move when the next prune is due;
//! - a daily time is measured on the local wall clock, because "03:30" means
//!   half past three in the morning, not an instant in elapsed time.
//!
//! Collapsing both onto one clock produces the classic bug where a nightly job
//! fires twice in October and not at all in March.
//!
//! Nothing here reads a clock: callers pass `now` and the last run, so the rules
//! are tested against simulated instants and never wait for real time.

use std::time::Duration;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};

/// A validated time of day, in local wall-clock terms.
///
/// Backed by [`NaiveTime`] so an impossible hour or minute cannot be
/// represented: the scheduler never has to defend against `25:00`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DailyTime(NaiveTime);

impl DailyTime {
    /// Build a daily time, or `None` when the hour or minute is out of range.
    pub fn new(hour: u32, minute: u32) -> Option<Self> {
        NaiveTime::from_hms_opt(hour, minute, 0).map(Self)
    }

    /// The wall-clock time this schedule names.
    pub fn time(&self) -> NaiveTime {
        self.0
    }

    /// This time on a given date. Infallible because both halves are valid.
    fn on(&self, date: NaiveDate) -> NaiveDateTime {
        date.and_time(self.0)
    }
}

/// Why a task is due, so the log can say more than "ran".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueReason {
    /// The task has not run in this process.
    NeverRan,
    /// The configured interval has elapsed since the previous run.
    IntervalElapsed,
    /// The scheduled moment has passed since the previous run.
    ScheduledMomentReached,
}

impl std::fmt::Display for DueReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::NeverRan => "never_ran",
            Self::IntervalElapsed => "interval_elapsed",
            Self::ScheduledMomentReached => "scheduled_moment_reached",
        };

        write!(f, "{label}")
    }
}

/// Whether an interval schedule has come due.
///
/// `elapsed_since_last` comes from a monotonic clock and is `None` when the task
/// has never run.
///
/// A zero interval is due on every evaluation; configuration validation is
/// responsible for rejecting it, because a zero interval is a busy loop rather
/// than a schedule.
pub fn interval_is_due(every: Duration, elapsed_since_last: Option<Duration>) -> Option<DueReason> {
    match elapsed_since_last {
        None => Some(DueReason::NeverRan),
        Some(elapsed) if elapsed >= every => Some(DueReason::IntervalElapsed),
        Some(_) => None,
    }
}

/// Whether a daily schedule has come due.
///
/// `last_run` and `now` are local wall-clock timestamps.
///
/// Measurement against the most recent scheduled moment, rather than against
/// "yesterday's run", is what makes catch-up safe: a host that was off for three
/// days produces **one** run, because after that run `last_run` is later than
/// the most recent moment and the next one is a day away.
///
/// The wall-clock model also settles both daylight-saving cases without special
/// handling. A time that does not exist in the spring jump is still due once the
/// clock passes it, and a time that occurs twice in the autumn is only due for
/// the first occurrence, because the second compares equal to the first run.
pub fn daily_is_due(
    at: DailyTime,
    last_run: Option<NaiveDateTime>,
    now: NaiveDateTime,
) -> Option<DueReason> {
    let Some(last_run) = last_run else {
        return Some(DueReason::NeverRan);
    };

    // The most recent moment the schedule named, at or before now. Measuring
    // against this instead of "the previous run" is what turns a long absence
    // into one catch-up run rather than a replayed backlog.
    let today = at.on(now.date());
    let most_recent = if today <= now {
        today
    } else {
        today - chrono::Duration::days(1)
    };

    if last_run < most_recent {
        Some(DueReason::ScheduledMomentReached)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local wall-clock instant, built explicitly so no test depends on the
    /// real clock.
    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|date| date.and_hms_opt(hour, minute, 0))
            .expect("valid test timestamp")
    }

    fn daily(hour: u32, minute: u32) -> DailyTime {
        DailyTime::new(hour, minute).expect("valid test time")
    }

    // === DailyTime ===

    #[test]
    fn a_valid_time_of_day_is_accepted() {
        let time = daily(3, 30);

        assert_eq!(time.time(), NaiveTime::from_hms_opt(3, 30, 0).unwrap());
    }

    #[test]
    fn impossible_times_are_rejected() {
        assert!(DailyTime::new(24, 0).is_none(), "hour out of range");
        assert!(DailyTime::new(0, 60).is_none(), "minute out of range");
        assert!(DailyTime::new(0, 0).is_some(), "midnight is valid");
        assert!(DailyTime::new(23, 59).is_some(), "the last minute is valid");
    }

    // === interval_is_due ===

    #[test]
    fn a_task_that_never_ran_is_due_immediately() {
        assert_eq!(
            interval_is_due(Duration::from_secs(60), None),
            Some(DueReason::NeverRan)
        );
    }

    #[test]
    fn below_the_interval_is_not_due() {
        assert_eq!(
            interval_is_due(Duration::from_secs(60), Some(Duration::from_secs(59))),
            None
        );
    }

    /// The boundary belongs to due: waiting one more tick after the interval had
    /// already elapsed is late for no reason.
    #[test]
    fn exactly_at_the_interval_is_due() {
        assert_eq!(
            interval_is_due(Duration::from_secs(60), Some(Duration::from_secs(60))),
            Some(DueReason::IntervalElapsed)
        );
    }

    #[test]
    fn past_the_interval_is_due() {
        assert_eq!(
            interval_is_due(Duration::from_secs(60), Some(Duration::from_secs(600))),
            Some(DueReason::IntervalElapsed)
        );
    }

    /// A host that was off for a month produces one run, not thirty: after the
    /// run the elapsed time resets, which is the whole catch-up rule.
    #[test]
    fn a_long_absence_fires_once_and_then_waits() {
        let every = Duration::from_secs(3600);
        let a_month_off = Duration::from_secs(3600 * 24 * 30);

        assert_eq!(
            interval_is_due(every, Some(a_month_off)),
            Some(DueReason::IntervalElapsed),
            "the missed run happens once"
        );
        assert_eq!(
            interval_is_due(every, Some(Duration::ZERO)),
            None,
            "immediately after it, nothing is owed"
        );
    }

    #[test]
    fn a_zero_interval_is_due_on_every_evaluation() {
        assert_eq!(
            interval_is_due(Duration::ZERO, Some(Duration::ZERO)),
            Some(DueReason::IntervalElapsed)
        );
    }

    // === daily_is_due ===

    #[test]
    fn a_daily_task_that_never_ran_is_due() {
        let now = at(2026, 3, 10, 12, 0);

        assert_eq!(
            daily_is_due(daily(3, 30), None, now),
            Some(DueReason::NeverRan)
        );
    }

    #[test]
    fn before_the_moment_with_yesterdays_run_is_not_due() {
        let now = at(2026, 3, 10, 3, 0);

        assert_eq!(
            daily_is_due(daily(3, 30), Some(at(2026, 3, 9, 3, 30)), now),
            None
        );
    }

    #[test]
    fn after_the_moment_with_an_earlier_run_is_due() {
        let now = at(2026, 3, 10, 3, 31);

        assert_eq!(
            daily_is_due(daily(3, 30), Some(at(2026, 3, 9, 3, 30)), now),
            Some(DueReason::ScheduledMomentReached)
        );
    }

    #[test]
    fn after_the_moment_with_a_run_past_it_is_not_due() {
        let now = at(2026, 3, 10, 23, 0);

        assert_eq!(
            daily_is_due(daily(3, 30), Some(at(2026, 3, 10, 3, 31)), now),
            None
        );
    }

    /// The boundary belongs to due, matching the interval rule.
    #[test]
    fn exactly_at_the_moment_is_due() {
        let now = at(2026, 3, 10, 3, 30);

        assert_eq!(
            daily_is_due(daily(3, 30), Some(at(2026, 3, 9, 3, 30)), now),
            Some(DueReason::ScheduledMomentReached)
        );
    }

    /// A time just after midnight must compare against today's moment, not
    /// yesterday's, or a 00:05 task would never run.
    #[test]
    fn a_midnight_rollover_is_due() {
        let now = at(2026, 3, 10, 0, 10);

        assert_eq!(
            daily_is_due(daily(0, 5), Some(at(2026, 3, 9, 23, 59)), now),
            Some(DueReason::ScheduledMomentReached)
        );
    }

    /// Three days offline produce one run, and the following evaluation is
    /// already quiet. This is the anti-burst rule.
    #[test]
    fn three_days_offline_produce_one_run_not_three() {
        let schedule = daily(3, 30);
        let now = at(2026, 3, 13, 9, 0);
        let last_run = Some(at(2026, 3, 9, 3, 30));

        assert_eq!(
            daily_is_due(schedule, last_run, now),
            Some(DueReason::ScheduledMomentReached)
        );
        assert_eq!(
            daily_is_due(schedule, Some(now), now),
            None,
            "the backlog is not replayed"
        );
    }

    /// Spring forward: 02:30 does not exist, the clock jumps from 01:59 to 03:00.
    /// Once the wall clock is past the named time the task runs, exactly once.
    #[test]
    fn a_time_skipped_by_the_spring_change_still_runs() {
        let schedule = daily(2, 30);
        let before_the_jump = at(2026, 3, 29, 1, 59);
        let after_the_jump = at(2026, 3, 29, 3, 0);
        let last_run = Some(at(2026, 3, 28, 2, 30));

        assert_eq!(
            daily_is_due(schedule, last_run, before_the_jump),
            None,
            "not yet due while the clock is still before the named time"
        );
        assert_eq!(
            daily_is_due(schedule, last_run, after_the_jump),
            Some(DueReason::ScheduledMomentReached),
            "due once the clock has passed it"
        );
    }

    /// Autumn fall back: 02:30 happens twice. Running at the first occurrence
    /// must silence the second.
    #[test]
    fn a_time_repeated_by_the_autumn_change_runs_once() {
        let schedule = daily(2, 30);
        let first_occurrence = at(2026, 10, 25, 2, 30);
        let second_occurrence = at(2026, 10, 25, 2, 30);

        assert_eq!(
            daily_is_due(schedule, Some(at(2026, 10, 24, 2, 30)), first_occurrence),
            Some(DueReason::ScheduledMomentReached)
        );
        assert_eq!(
            daily_is_due(schedule, Some(first_occurrence), second_occurrence),
            None,
            "the repeated hour must not fire twice"
        );
    }

    /// A task whose moment is later today, with a run from yesterday, waits.
    #[test]
    fn an_evening_task_is_not_due_in_the_morning() {
        let schedule = daily(23, 0);

        assert_eq!(
            daily_is_due(schedule, Some(at(2026, 3, 9, 23, 0)), at(2026, 3, 10, 9, 0)),
            None
        );
    }
}
