//! Maintenance subsystem.
//!
//! Split so each file keeps one subject: [`executor`] owns the single seam
//! through which a process starts, and [`outcome`] owns what a finished run
//! means and when it deserves the operator's attention.
//!
//! Nothing in either module knows how to schedule a task; that arrives with the
//! scheduler, and it will consume these types rather than duplicate them.

pub mod backup;
pub mod backup_task;
pub mod executor;
pub mod outcome;
pub mod schedule;

pub use executor::{
    tail_chars, ExecutionError, ExecutionOutput, ExecutionRequest, Executor, MAX_CAPTURE_BYTES,
    MAX_DETAIL_CHARS,
};
pub use outcome::{
    classify_execution, should_notify, summarize, MaintenanceOutcome, MaintenanceStatus,
};
pub use schedule::{daily_is_due, interval_is_due, DailyTime, DueReason};
