//! What a finished maintenance run means, and when it deserves attention.
//!
//! `SPEC_MAINTENANCE.md` (M1) fixes two rules this module implements as pure
//! functions, so they can be tested exhaustively without a scheduler, a process
//! or a notifier:
//!
//! - a run always has a status, and a skipped run is distinguishable from a
//!   failed one;
//! - suppression is asymmetric. Silence is only ever applied to a stable
//!   success. A failure is announced every time, because "notify on change"
//!   would let a backup break once and then stay quiet for a week.

use std::time::Duration;

use super::executor::{tail_chars, ExecutionOutput, MAX_DETAIL_CHARS};

/// How a maintenance run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceStatus {
    /// Completed with exit code 0.
    Success,
    /// Completed with a non-zero exit code, or was killed before reporting one.
    Failed,
    /// Exceeded its deadline and was terminated.
    TimedOut,
    /// Not attempted: a precondition was not met.
    Skipped,
}

impl std::fmt::Display for MaintenanceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Skipped => "skipped",
        };

        write!(f, "{label}")
    }
}

/// The record of one maintenance run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceOutcome {
    /// Task name as it appears in the configuration.
    pub task: String,
    pub status: MaintenanceStatus,
    /// `None` when the process never reported one.
    pub exit_code: Option<i32>,
    pub duration: Duration,
    /// Bounded, single-line summary of stderr, with secrets already redacted by
    /// the caller that owns them.
    pub detail: String,
}

/// Map an executor result onto a status.
///
/// The deadline wins over the exit code: a process that was killed on timeout
/// may still have reported a code, and reporting that as `Failed` would hide the
/// fact that the operation never finished.
pub fn classify_execution(output: &ExecutionOutput) -> MaintenanceStatus {
    if output.timed_out {
        return MaintenanceStatus::TimedOut;
    }

    match output.exit_code {
        Some(0) => MaintenanceStatus::Success,
        _ => MaintenanceStatus::Failed,
    }
}

/// Collapse stderr into one bounded line.
///
/// Multi-line output is folded because `detail` travels through a log line and a
/// notification, both of which are single-line surfaces. The bound is applied
/// after folding, and counts characters so a multi-byte code point is never cut
/// in half.
pub fn summarize(stderr: &str) -> String {
    let folded = stderr.split_whitespace().collect::<Vec<_>>().join(" ");

    tail_chars(&folded, MAX_DETAIL_CHARS).to_string()
}

/// Whether the current outcome deserves a notification, given the previous one.
///
/// `previous` is `None` for a task that has not run yet in this process.
pub fn should_notify(previous: Option<MaintenanceStatus>, current: MaintenanceStatus) -> bool {
    match current {
        // Silence is only ever applied to a stable state: a task that keeps
        // succeeding, or keeps being skipped for the same reason.
        MaintenanceStatus::Success | MaintenanceStatus::Skipped => previous != Some(current),

        // A failure is never silenced, not even a repeated one. "Notify on
        // change" would hide a backup that broke once and stayed broken.
        MaintenanceStatus::Failed | MaintenanceStatus::TimedOut => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(exit_code: Option<i32>, timed_out: bool) -> ExecutionOutput {
        ExecutionOutput {
            exit_code,
            stdout: String::new(),
            stderr: String::new(),
            timed_out,
        }
    }

    // === classify_execution ===

    #[test]
    fn exit_zero_is_success() {
        assert_eq!(
            classify_execution(&output(Some(0), false)),
            MaintenanceStatus::Success
        );
    }

    #[test]
    fn non_zero_exit_is_failed() {
        assert_eq!(
            classify_execution(&output(Some(1), false)),
            MaintenanceStatus::Failed
        );
        assert_eq!(
            classify_execution(&output(Some(137), false)),
            MaintenanceStatus::Failed
        );
    }

    #[test]
    fn a_deadline_is_timed_out() {
        assert_eq!(
            classify_execution(&output(None, true)),
            MaintenanceStatus::TimedOut
        );
    }

    /// The deadline outranks the exit code: reporting `Failed` for a process
    /// that never finished would hide why it stopped.
    #[test]
    fn the_deadline_outranks_a_reported_exit_code() {
        assert_eq!(
            classify_execution(&output(Some(0), true)),
            MaintenanceStatus::TimedOut
        );
    }

    /// A process killed from outside reports no code and did not time out. It
    /// did not succeed, so it must not be reported as a success.
    #[test]
    fn a_missing_exit_code_without_a_deadline_is_failed() {
        assert_eq!(
            classify_execution(&output(None, false)),
            MaintenanceStatus::Failed
        );
    }

    // === summarize ===

    #[test]
    fn a_single_line_is_kept() {
        assert_eq!(
            summarize("docker: prune complete"),
            "docker: prune complete"
        );
    }

    #[test]
    fn multi_line_output_is_folded_into_one_line() {
        let stderr = "error: could not connect\ncaused by: connection refused\n";

        assert_eq!(
            summarize(stderr),
            "error: could not connect caused by: connection refused"
        );
    }

    #[test]
    fn runs_of_whitespace_collapse() {
        assert_eq!(summarize("a\t\tb    c\nd"), "a b c d");
    }

    #[test]
    fn empty_output_stays_empty() {
        assert_eq!(summarize(""), "");
        assert_eq!(summarize("   \n\t "), "");
    }

    #[test]
    fn long_output_is_bounded_to_the_detail_budget() {
        let stderr = "a".repeat(MAX_DETAIL_CHARS * 2);

        assert_eq!(summarize(&stderr).chars().count(), MAX_DETAIL_CHARS);
    }

    /// The bound counts characters: a stderr full of accented text must not
    /// panic and must respect the budget.
    #[test]
    fn bounding_counts_characters_not_bytes() {
        let stderr = "é".repeat(MAX_DETAIL_CHARS * 2);

        let detail = summarize(&stderr);

        assert_eq!(detail.chars().count(), MAX_DETAIL_CHARS);
        assert!(stderr.ends_with(&detail));
    }

    // === should_notify: the asymmetric rule ===

    /// The first success is worth confirming once: it tells the operator the
    /// task they just configured actually works.
    #[test]
    fn the_first_ever_success_is_announced() {
        assert!(should_notify(None, MaintenanceStatus::Success));
    }

    /// The noise being suppressed: a task that keeps succeeding.
    #[test]
    fn a_stable_success_is_silent() {
        assert!(!should_notify(
            Some(MaintenanceStatus::Success),
            MaintenanceStatus::Success
        ));
    }

    #[test]
    fn crossing_into_failure_is_announced() {
        assert!(should_notify(
            Some(MaintenanceStatus::Success),
            MaintenanceStatus::Failed
        ));
        assert!(should_notify(
            Some(MaintenanceStatus::Success),
            MaintenanceStatus::TimedOut
        ));
    }

    /// The asymmetry that matters: a failure that persists is announced on every
    /// run. Silencing it would leave a broken backup invisible for a week.
    #[test]
    fn a_persistent_failure_is_announced_again() {
        assert!(should_notify(
            Some(MaintenanceStatus::Failed),
            MaintenanceStatus::Failed
        ));
        assert!(should_notify(
            Some(MaintenanceStatus::TimedOut),
            MaintenanceStatus::TimedOut
        ));
        assert!(should_notify(
            Some(MaintenanceStatus::Failed),
            MaintenanceStatus::TimedOut
        ));
    }

    #[test]
    fn recovery_is_announced_once() {
        assert!(should_notify(
            Some(MaintenanceStatus::Failed),
            MaintenanceStatus::Success
        ));
        assert!(should_notify(
            Some(MaintenanceStatus::TimedOut),
            MaintenanceStatus::Success
        ));
        assert!(should_notify(
            Some(MaintenanceStatus::Skipped),
            MaintenanceStatus::Success
        ));
    }

    /// A skip is a decision the operator must learn about, but repeating it
    /// every cycle would punish a host that legitimately has no Docker.
    #[test]
    fn a_skip_is_announced_once_and_then_stays_quiet() {
        assert!(
            should_notify(None, MaintenanceStatus::Skipped),
            "the first skip must be reported"
        );
        assert!(
            should_notify(Some(MaintenanceStatus::Success), MaintenanceStatus::Skipped),
            "skipping a task that used to work is a change worth reporting"
        );
        assert!(
            !should_notify(Some(MaintenanceStatus::Skipped), MaintenanceStatus::Skipped),
            "a persistent skip is not news"
        );
    }

    /// Every status must have a defined answer: this asserts the rule is total
    /// rather than relying on a match arm falling through.
    #[test]
    fn the_rule_is_defined_for_every_status() {
        let statuses = [
            MaintenanceStatus::Success,
            MaintenanceStatus::Failed,
            MaintenanceStatus::TimedOut,
            MaintenanceStatus::Skipped,
        ];

        for previous in statuses {
            for current in statuses {
                let announced = should_notify(Some(previous), current);

                // Silence is only ever allowed for a stable success or a
                // persistent skip; anything else must reach the operator.
                let silence_allowed = current == previous
                    && matches!(
                        current,
                        MaintenanceStatus::Success | MaintenanceStatus::Skipped
                    );

                assert_eq!(
                    !announced, silence_allowed,
                    "unexpected policy for {previous:?} -> {current:?}"
                );
            }
        }
    }
}
