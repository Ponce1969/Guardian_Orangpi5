//! What has already run, kept across restarts.
//!
//! The scheduler holds its last-run record in memory, which is correct for a
//! process that runs for weeks and wrong for one that restarts: every start
//! forgets, `never_ran` becomes true again, and a daily backup runs once per
//! restart. Seven restarts in a day against a retention of seven quietly replace a
//! week of history with a day of it.
//!
//! Only the wall-clock instant is stored. A monotonic instant is meaningful only
//! within one boot, so it is reconstructed on load rather than written down.
//!
//! Reading is deliberately forgiving and writing is deliberately atomic. A state
//! file is a convenience: losing it costs one extra run, while refusing to start
//! would cost all monitoring, so a missing file is normal and an unreadable one is
//! a warning rather than an error the caller must handle.

use std::io::Write;
use std::path::Path;

use chrono::{NaiveDate, NaiveDateTime, TimeDelta};
use serde::{Deserialize, Serialize};

/// Bumped when the on-disk shape changes in a way an older build cannot read.
const FORMAT_VERSION: u32 = 1;

/// The record of completed runs and daily digests.
#[derive(Debug, Serialize, Deserialize)]
pub struct MaintenanceState {
    #[serde(default = "current_version")]
    version: u32,
    #[serde(default)]
    tasks: Vec<TaskState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    previous_tasks: Vec<TaskState>,
    #[serde(default)]
    last_digest_date: Option<NaiveDate>,
}

/// One task's last completed run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskState {
    pub name: String,
    pub last_run: NaiveDateTime,
}

fn current_version() -> u32 {
    FORMAT_VERSION
}

impl Default for MaintenanceState {
    fn default() -> Self {
        Self {
            version: FORMAT_VERSION,
            tasks: Vec::new(),
            previous_tasks: Vec::new(),
            last_digest_date: None,
        }
    }
}

impl MaintenanceState {
    /// Build a record from finished runs.
    pub fn new(tasks: Vec<TaskState>) -> Self {
        Self {
            version: FORMAT_VERSION,
            tasks,
            previous_tasks: Vec::new(),
            last_digest_date: None,
        }
    }

    /// Attach prior successful runs so catch-up digests survive an early next-day run.
    pub fn with_previous_tasks(mut self, previous_tasks: Vec<TaskState>) -> Self {
        self.previous_tasks = previous_tasks;
        self
    }

    /// The prior successful run recorded for `name` before the latest one, if any.
    pub fn previous_run(&self, name: &str) -> Option<NaiveDateTime> {
        self.previous_tasks
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.last_run)
    }

    /// Attach the most recent local calendar date for which the daily digest ran.
    pub fn with_last_digest_date(mut self, date: Option<NaiveDate>) -> Self {
        self.last_digest_date = date;
        self
    }

    /// Local calendar date of the last delivered daily digest, if any.
    pub fn last_digest_date(&self) -> Option<NaiveDate> {
        self.last_digest_date
    }

    /// How many runs are recorded.
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Whether nothing has been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// The last run recorded for `name`, if any.
    pub fn last_run(&self, name: &str) -> Option<NaiveDateTime> {
        self.tasks
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.last_run)
    }

    /// How long ago `name` last ran, if it has.
    ///
    /// A recorded instant in the future means the wall clock moved backwards. It is
    /// reported as zero rather than a negative duration, so a clock correction
    /// makes a task look freshly run instead of infinitely overdue.
    pub fn elapsed_since(&self, name: &str, now: NaiveDateTime) -> Option<TimeDelta> {
        let last = self.last_run(name)?;
        let elapsed = now.signed_duration_since(last);

        Some(if elapsed < TimeDelta::zero() {
            TimeDelta::zero()
        } else {
            elapsed
        })
    }
}

/// Read the record, treating a missing file as "nothing has run".
pub fn load(path: &Path) -> Result<MaintenanceState, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MaintenanceState::default())
        }
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };

    let state: MaintenanceState = serde_json::from_str(&raw)
        .map_err(|error| format!("{} is not valid state: {error}", path.display()))?;

    // A file from a newer build may carry fields this one would silently drop, and
    // dropping a record causes exactly the extra run this module exists to prevent.
    if state.version > FORMAT_VERSION {
        return Err(format!(
            "{} was written by a newer build (version {} > {FORMAT_VERSION})",
            path.display(),
            state.version
        ));
    }

    Ok(state)
}

/// Write the record, replacing the previous one in a single step.
pub fn save(path: &Path, state: &MaintenanceState) -> Result<(), String> {
    let body = serde_json::to_string_pretty(state)
        .map_err(|error| format!("cannot encode maintenance state: {error}"))?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
    }

    // Written beside the target, flushed and synced, and only then renamed: a
    // rename that reaches the disk ahead of the data it names leaves a zero-length
    // record, which reads as corrupt exactly when the record matters most.
    let mut temp_os = path.as_os_str().to_os_string();
    temp_os.push(".tmp");
    let temp = std::path::PathBuf::from(temp_os);

    let mut file = std::fs::File::create(&temp)
        .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
    file.write_all(body.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
    file.sync_all()
        .map_err(|error| format!("cannot flush {}: {error}", temp.display()))?;
    drop(file);

    if std::fs::rename(&temp, path).is_ok() {
        sync_parent(path);
        return Ok(());
    }

    // Windows refuses to rename over an existing file. The removal first is not
    // atomic, which is why the rename above is attempted before it.
    let _ = std::fs::remove_file(path);

    std::fs::rename(&temp, path)
        .map_err(|error| format!("cannot replace {}: {error}", path.display()))?;
    sync_parent(path);
    Ok(())
}

/// Flush the directory entry too, so the rename itself survives a power loss.
#[cfg(unix)]
fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        let dir = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if let Ok(directory) = std::fs::File::open(dir) {
            let _ = directory.sync_all();
        }
    }
}

/// Windows exposes no directory handle to flush; the file sync above is all there is.
#[cfg(not(unix))]
fn sync_parent(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scratch(label: &str) -> std::path::PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);

        let mut path = std::env::temp_dir();
        path.push(format!(
            "guardian-state-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).expect("scratch directory");
        path
    }

    fn moment(hour: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 30)
            .and_then(|date| date.and_hms_opt(hour, 0, 0))
            .expect("valid timestamp")
    }

    #[test]
    fn a_saved_record_reads_back_unchanged() {
        let dir = scratch("roundtrip");
        let path = dir.join("state.json");

        let state = MaintenanceState::new(vec![TaskState {
            name: "nightly".to_string(),
            last_run: moment(3),
        }]);

        save(&path, &state).expect("saving works");

        let loaded = load(&path).expect("loading works");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.last_run("nightly"), Some(moment(3)));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// First start on a fresh host: no file yet, nothing has run, every task is
    /// due. That is the documented behaviour, not a failure.
    #[test]
    fn a_missing_file_means_nothing_has_run() {
        let dir = scratch("missing");

        let state = load(&dir.join("state.json")).expect("a missing file is not an error");

        assert!(state.is_empty());
        assert_eq!(state.last_run("nightly"), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A truncated file must be reported so the caller can warn, and must never be
    /// silently treated as "nothing has run" without anyone knowing.
    #[test]
    fn a_corrupt_file_is_reported() {
        let dir = scratch("corrupt");
        let path = dir.join("state.json");
        std::fs::write(&path, "{ not json").expect("write");

        let error = load(&path).expect_err("corrupt state must be reported");

        assert!(error.contains("not valid state"), "error was: {error}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_record_from_a_newer_build_is_refused() {
        let dir = scratch("newer");
        let path = dir.join("state.json");
        std::fs::write(
            &path,
            r#"{"version": 99, "tasks": [{"name": "x", "last_run": "2026-10-30T03:00:00"}]}"#,
        )
        .expect("write");

        let error = load(&path).expect_err("a newer format must not be guessed at");

        assert!(error.contains("newer build"), "error was: {error}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Saving twice must leave one readable record, not a stale temporary file
    /// beside it, and the second save must win.
    #[test]
    fn a_second_save_replaces_the_first() {
        let dir = scratch("replace");
        let path = dir.join("state.json");

        save(
            &path,
            &MaintenanceState::new(vec![TaskState {
                name: "nightly".to_string(),
                last_run: moment(3),
            }]),
        )
        .expect("first save");

        save(
            &path,
            &MaintenanceState::new(vec![TaskState {
                name: "nightly".to_string(),
                last_run: moment(4),
            }]),
        )
        .expect("second save");

        assert_eq!(
            load(&path).expect("load").last_run("nightly"),
            Some(moment(4))
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A clock that moved backwards must not make a task infinitely overdue.
    #[test]
    fn a_future_timestamp_elapses_as_zero() {
        let state = MaintenanceState::new(vec![TaskState {
            name: "nightly".to_string(),
            last_run: moment(5),
        }]);

        let elapsed = state
            .elapsed_since("nightly", moment(4))
            .expect("the task has run");

        assert_eq!(elapsed, TimeDelta::zero());
    }

    /// The record is published through a temp file: a crash mid-write must leave
    /// the previous record intact rather than a truncated one.
    #[test]
    fn save_publishes_through_a_temp_file_and_leaves_none_behind() {
        let dir = std::env::temp_dir().join(format!("guardian-state-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("state.json");

        save(&path, &MaintenanceState::new(Vec::new())).expect("save works");

        assert!(path.exists(), "the record was not published");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temp file survived the rename"
        );
        let body = std::fs::read_to_string(&path).expect("read back");
        serde_json::from_str::<serde_json::Value>(&body)
            .expect("the published record is valid JSON");

        std::fs::remove_dir_all(&dir).ok();
    }
}
