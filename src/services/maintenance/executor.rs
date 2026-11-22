//! Maintenance execution primitives.
//!
//! The scheduler, the task kinds and the reporting pipeline all decide *what*
//! to run and *what to say about it*. This module owns the single seam through
//! which a process actually starts, and it is deliberately the only one.
//!
//! Everything above it talks to [`Executor`], which is why the test suite never
//! spawns a process: `SPEC_MAINTENANCE.md` requires that no test executes a real
//! command, and a trait boundary is the only way to keep that promise while
//! still testing the policy that surrounds execution.

use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

/// Largest slice of a child's output kept in memory.
///
/// A `docker prune` on a busy host can emit megabytes. Capturing without a bound
/// turns a maintenance task into an out-of-memory risk, so the executor keeps
/// only the tail.
pub const MAX_CAPTURE_BYTES: usize = 8 * 1024;

/// Largest slice of output promoted into a log line or a notification.
///
/// Smaller than [`MAX_CAPTURE_BYTES`] on purpose: one bound protects the
/// process, the other protects the operator's attention.
pub const MAX_DETAIL_CHARS: usize = 1500;

/// One budget protects the process, the other protects the operator's
/// attention. If someone raises the detail bound past the capture bound, the
/// notification layer would promise more than the executor can deliver, so the
/// relationship is enforced at compile time rather than trusted.
const _: () = assert!(MAX_DETAIL_CHARS < MAX_CAPTURE_BYTES);

/// A command to execute.
///
/// Built through [`ExecutionRequest::new`] and its builder methods, never by
/// struct literal, so the timeout cannot be forgotten and the environment is
/// carried in one place. There is no way to express a shell: the program and
/// each argument are separate values and are never joined into a string.
#[derive(Clone, PartialEq, Eq)]
pub struct ExecutionRequest {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    timeout: Duration,
    grace: Duration,
    cwd: Option<PathBuf>,
}

impl ExecutionRequest {
    /// Create a request for `program` with separate `args`.
    ///
    /// `timeout` is mandatory by construction: an operation without a deadline
    /// is an operation nobody decided to allow.
    pub fn new(program: impl Into<String>, args: Vec<String>, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            args,
            env: Vec::new(),
            timeout,
            grace: Duration::from_secs(5),
            cwd: None,
        }
    }

    /// Add an environment variable for the child process.
    ///
    /// Secrets travel here rather than in the arguments, which any local user
    /// can read with `ps`.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Override the window between signalling the process group and killing it.
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// Run the command in a specific working directory.
    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn grace(&self) -> Duration {
        self.grace
    }

    pub fn cwd(&self) -> Option<&PathBuf> {
        self.cwd.as_ref()
    }
}

/// Debug output lists environment *keys* and never values.
///
/// A request routinely carries a database password. Deriving `Debug` would put
/// it one `tracing` call away from the journal, so the implementation is manual
/// and the omission is intentional.
impl fmt::Debug for ExecutionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env_keys: Vec<&str> = self.env.iter().map(|(key, _)| key.as_str()).collect();

        f.debug_struct("ExecutionRequest")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env_keys", &env_keys)
            .field("timeout", &self.timeout)
            .field("grace", &self.grace)
            .field("cwd", &self.cwd)
            .finish()
    }
}

/// What a finished execution produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionOutput {
    /// `None` when the process was killed before it could report one.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// True when the deadline expired and the process was terminated.
    pub timed_out: bool,
}

/// A failure to start or collect a process.
///
/// Distinct from a non-zero exit code: a binary that is missing is a
/// configuration problem, while a command that ran and failed is a result. The
/// reporting layer maps them to different statuses for that reason.
#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("failed to spawn {program}: {reason}")]
    Spawn { program: String, reason: String },

    #[error("failed to collect the process: {reason}")]
    Collect { reason: String },
}

/// The single seam through which maintenance starts a process.
///
/// Implementations own the platform mechanics: spawning the child as its own
/// process group, enforcing the deadline, escalating from a polite signal to a
/// forced one, and bounding captured output. Tests implement this trait with a
/// script, so no test executes a real command.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Run the request to completion, to its deadline, or to failure.
    ///
    /// A deadline that expires is a [`ExecutionOutput`] with `timed_out` set, not
    /// an error: the task ran and has an outcome. An error means the process
    /// could not be started or collected at all.
    async fn run(&self, request: &ExecutionRequest) -> Result<ExecutionOutput, ExecutionError>;
}

/// Keep at most `max_chars` characters from the end of `text`.
///
/// Counted in characters rather than bytes so a log line containing accented
/// text or an emoji cannot be cut in the middle of a code point.
pub fn tail_chars(text: &str, max_chars: usize) -> &str {
    let total = text.chars().count();
    if total <= max_chars {
        return text;
    }

    // Find the byte offset of the first character to keep. Indexing by byte
    // arithmetic would land inside a multi-byte code point and panic, so the
    // boundary comes from the string itself.
    text.char_indices()
        .nth(total - max_chars)
        .map(|(index, _)| &text[index..])
        .unwrap_or(text)
}

/// Keep at most `max_bytes` bytes from the end of `buffer`, as lossy text.
///
/// Two bounds apply for two reasons: the byte window keeps memory bounded while
/// streaming, and the character trim keeps the result from starting on half a
/// code point. Invalid UTF-8 becomes a replacement character rather than an
/// error, because a log line that cannot be decoded is still worth reporting.
pub fn bounded_text(buffer: &[u8], max_bytes: usize) -> String {
    let start = buffer.len().saturating_sub(max_bytes);
    let lossy = String::from_utf8_lossy(&buffer[start..]);

    // Starting the window inside a multi-byte sequence leaves a replacement
    // character at the front. It is an artefact of where reading began, not part
    // of the log, so it is dropped rather than shown to the operator.
    let trimmed = lossy.trim_start_matches('\u{FFFD}');

    tail_chars(trimmed, max_bytes).to_string()
}

/// Read a child pipe to its end, keeping only the tail.
///
/// The buffer is trimmed while streaming so a task that emits megabytes cannot
/// grow it without limit; the window is twice the budget so the tail is always
/// complete once reading stops.
async fn read_bounded<R>(reader: Option<R>, max_bytes: usize) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return String::new();
    };

    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);

                if buffer.len() > max_bytes.saturating_mul(2) {
                    let excess = buffer.len() - max_bytes;
                    buffer.drain(..excess);
                }
            }
            // A pipe that cannot be read any further ends the capture; the
            // process outcome is what the caller acts on.
            // A retried read is not a broken pipe; only real errors end the capture.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    bounded_text(&buffer, max_bytes)
}

/// Collect both capture tasks, but never for longer than `deadline`.
///
/// `read_bounded` stops only at EOF, and a descendant that inherited the pipe
/// write end keeps it open after the direct child is gone. Awaiting the readers
/// without a bound therefore lets a stray grandchild decide when `run` returns,
/// which is exactly the deadline the caller asked for and did not get.
async fn drain_capture(
    mut stdout_task: tokio::task::JoinHandle<String>,
    mut stderr_task: tokio::task::JoinHandle<String>,
    deadline: Duration,
) -> (String, String) {
    let read = async {
        let stdout = (&mut stdout_task).await.unwrap_or_default();
        let stderr = (&mut stderr_task).await.unwrap_or_default();
        (stdout, stderr)
    };
    let outcome = tokio::time::timeout(deadline, read).await;
    match outcome {
        Ok(captured) => captured,
        Err(_) => {
            // The pipe is still held open somewhere we no longer control. Stop
            // waiting and stop the readers rather than leaking them.
            stdout_task.abort();
            stderr_task.abort();
            (String::new(), String::new())
        }
    }
}

/// Signal an entire process group.
///
/// A negative pid means "the group led by this pid", which is exactly the group
/// `process_group(0)` established for the child. Signalling the child alone
/// would leave anything it forked running.
#[cfg(unix)]
fn signal_group(pid: u32, signal: libc::c_int) {
    // SAFETY: `kill(2)` has no memory-safety preconditions. The call is ignored
    // on failure because the process may already be gone, which is not an error
    // here: the caller reaps it next.
    unsafe {
        libc::kill(-(pid as libc::pid_t), signal);
    }
}

/// Stop a child that overran its deadline, escalating from polite to forced.
///
/// Returns nothing: a timed-out run reports `exit_code: None`, because the code
/// of a process that was killed is not a result the operator can use.
async fn terminate(child: &mut Child, pid: Option<u32>, grace: Duration) {
    #[cfg(unix)]
    {
        let Some(pid) = pid else {
            let _ = child.kill().await;
            return;
        };

        signal_group(pid, libc::SIGTERM);

        // The grace window decides how long the group gets to leave politely; it
        // never decides whether the group is signalled at all. A direct child that
        // exits on SIGTERM proves nothing about its descendants: one that ignores
        // SIGTERM would keep running while `run` reports a timeout.
        let _ = tokio::time::timeout(grace, child.wait()).await;
        signal_group(pid, libc::SIGKILL);
        let _ = child.wait().await;
    }

    #[cfg(not(unix))]
    {
        // No process group to signal. `kill` plus `kill_on_drop` is the whole
        // story here, and the spec records it as a documented limitation rather
        // than pretending the platforms are equivalent.
        let _ = (pid, grace);
        let _ = child.kill().await;
    }
}

/// Kills the child's process group if it is dropped without being disarmed.
///
/// `kill_on_drop` reaps the **direct child only**, so cancelling a run future — a
/// shutdown arriving while `pg_dump` is working — would leave anything the dump
/// forked still running, holding its connection and its locks. This guard closes
/// that gap: it lives on the run future's stack, so dropping the future kills the
/// whole group.
struct GroupKillGuard(Option<u32>);

impl GroupKillGuard {
    fn new(pid: Option<u32>) -> Self {
        Self(pid)
    }

    /// Called once the child has been reaped through the normal path.
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupKillGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            signal_group(pid, libc::SIGKILL);
        }

        #[cfg(not(unix))]
        {
            let _ = self.0;
        }
    }
}

/// The production [`Executor`]: real processes, real deadlines, real signals.
///
/// No test constructs this type. The suite is written against [`Executor`] with a
/// script, so nothing below is exercised by `cargo test`; the escalation is
/// verified once, by hand, against a real orphan-prone command.
pub struct ProcessExecutor;

impl ProcessExecutor {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ProcessExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for ProcessExecutor {
    async fn run(&self, request: &ExecutionRequest) -> Result<ExecutionOutput, ExecutionError> {
        let mut command = Command::new(request.program());

        command
            .args(request.args())
            .envs(request.env().iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // The child leads its own process group, so one signal reaches everything
        // it forks instead of only the direct child.
        #[cfg(unix)]
        command.process_group(0);

        if let Some(cwd) = request.cwd() {
            command.current_dir(cwd);
        }

        let mut child = command.spawn().map_err(|error| ExecutionError::Spawn {
            program: request.program().to_string(),
            reason: error.to_string(),
        })?;

        let pid = child.id();

        // Armed here so that dropping this future — a cancellation — takes the
        // whole process group with it.
        let mut group_guard = GroupKillGuard::new(pid);

        let stdout_task = tokio::spawn(read_bounded(child.stdout.take(), MAX_CAPTURE_BYTES));
        let stderr_task = tokio::spawn(read_bounded(child.stderr.take(), MAX_CAPTURE_BYTES));

        let (exit_code, timed_out) =
            match tokio::time::timeout(request.timeout(), child.wait()).await {
                Ok(Ok(status)) => (status.code(), false),
                Ok(Err(error)) => {
                    return Err(ExecutionError::Collect {
                        reason: error.to_string(),
                    })
                }
                Err(_) => {
                    terminate(&mut child, pid, request.grace()).await;
                    (None, true)
                }
            };

        // The child is reaped, but a descendant may still hold a pipe write end
        // open; the drain is bounded so that `run` cannot outlive its deadline.
        let (stdout, stderr) = drain_capture(stdout_task, stderr_task, request.grace()).await;

        // The child is reaped; nothing is left for the guard to kill.
        group_guard.disarm();

        Ok(ExecutionOutput {
            exit_code,
            stdout,
            stderr,
            timed_out,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// An [`Executor`] that answers from a script and records the requests.
    ///
    /// This is the shape every later step tests against: policy above the seam,
    /// no process below it.
    #[derive(Default)]
    struct ScriptedExecutor {
        script: Mutex<Vec<Result<ExecutionOutput, ExecutionError>>>,
        requests: AtomicUsize,
    }

    impl ScriptedExecutor {
        fn with(script: Vec<Result<ExecutionOutput, ExecutionError>>) -> Self {
            Self {
                script: Mutex::new(script),
                requests: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Executor for ScriptedExecutor {
        async fn run(
            &self,
            _request: &ExecutionRequest,
        ) -> Result<ExecutionOutput, ExecutionError> {
            self.requests.fetch_add(1, Ordering::SeqCst);

            let mut script = self.script.lock().map_err(|e| ExecutionError::Collect {
                reason: format!("script lock poisoned: {e}"),
            })?;

            if script.is_empty() {
                return Err(ExecutionError::Collect {
                    reason: "script exhausted".to_string(),
                });
            }

            script.remove(0)
        }
    }

    fn output(exit_code: Option<i32>, timed_out: bool) -> ExecutionOutput {
        ExecutionOutput {
            exit_code,
            stdout: String::new(),
            stderr: String::new(),
            timed_out,
        }
    }

    fn request() -> ExecutionRequest {
        ExecutionRequest::new(
            "docker",
            vec!["system".to_string(), "prune".to_string(), "-af".to_string()],
            Duration::from_secs(120),
        )
    }

    #[test]
    fn tail_keeps_short_text_intact() {
        assert_eq!(
            tail_chars("docker prune complete", MAX_DETAIL_CHARS),
            "docker prune complete"
        );
    }

    #[test]
    fn tail_truncates_to_the_requested_characters() {
        assert_eq!(tail_chars("abcdefghij", 4), "ghij");
        assert_eq!(tail_chars("abcdefghij", 10), "abcdefghij");
    }

    /// Counting bytes instead of characters would slice through a multi-byte
    /// code point and panic, taking the daemon's cycle down with it. Accented
    /// text is ordinary in the logs of a Spanish-language host.
    #[test]
    fn tail_handles_multibyte_characters_without_panicking() {
        // Every character is two bytes, so any byte-based cut lands mid-character.
        let accented = "é".repeat(20);

        let tail = tail_chars(&accented, 5);

        assert_eq!(tail, "ééééé");
        assert_eq!(tail.chars().count(), 5);
    }

    /// A realistic stderr line ending with an emoji, cut to a bound small enough
    /// that the byte offset lands inside the four-byte code point.
    #[test]
    fn tail_handles_a_trailing_emoji() {
        let line = "pg_dump: error al conectar 🐘";

        let tail = tail_chars(line, 3);

        assert_eq!(tail.chars().count(), 3);
        assert!(line.ends_with(tail));
    }

    #[test]
    fn tail_of_an_empty_string_is_empty() {
        assert_eq!(tail_chars("", 10), "");
    }

    /// The request must never be able to express a shell command.
    #[test]
    fn request_keeps_the_program_and_arguments_separate() {
        let request = request();

        assert_eq!(request.program(), "docker");
        assert_eq!(request.args(), ["system", "prune", "-af"]);
        assert_eq!(request.timeout(), Duration::from_secs(120));
        assert_eq!(
            request.grace(),
            Duration::from_secs(5),
            "grace defaults to the documented five seconds"
        );
    }

    /// A secret placed in the environment must not reach a log through `Debug`,
    /// which is what every `tracing` call uses.
    #[test]
    fn request_debug_lists_environment_keys_but_never_values() {
        let request = request().with_env("PGPASSWORD", "s3cret-value");

        let rendered = format!("{:?}", request);

        assert!(
            rendered.contains("PGPASSWORD"),
            "the key is useful context and must be visible"
        );
        assert!(
            !rendered.contains("s3cret-value"),
            "the value must never be rendered: {rendered}"
        );
    }

    #[test]
    fn request_carries_the_environment_for_the_child() {
        let request = request().with_env("PGPASSWORD", "s3cret-value");

        assert_eq!(
            request.env(),
            &[("PGPASSWORD".to_string(), "s3cret-value".to_string())]
        );
    }

    /// The seam is only useful if a scripted implementation can stand in for a
    /// real process, which is what keeps every later test free of side effects.
    #[tokio::test]
    async fn a_scripted_executor_satisfies_the_contract() {
        let executor = ScriptedExecutor::with(vec![Ok(output(Some(0), false))]);

        let result = executor.run(&request()).await.expect("scripted success");

        assert_eq!(result.exit_code, Some(0));
        assert!(!result.timed_out);
        assert_eq!(executor.calls(), 1);
    }

    /// A deadline is an outcome, not an error: the task ran and must be
    /// reported, while a spawn failure is a different situation entirely.
    #[tokio::test]
    async fn a_timeout_is_an_outcome_while_a_spawn_failure_is_an_error() {
        let timed_out = ScriptedExecutor::with(vec![Ok(output(None, true))]);
        let failed_to_spawn = ScriptedExecutor::with(vec![Err(ExecutionError::Spawn {
            program: "docker".to_string(),
            reason: "No such file or directory".to_string(),
        })]);

        let outcome = timed_out.run(&request()).await.expect("timeout is Ok");
        let error = failed_to_spawn.run(&request()).await;

        assert!(outcome.timed_out);
        assert_eq!(outcome.exit_code, None);
        assert!(matches!(error, Err(ExecutionError::Spawn { .. })));
    }

    // === bounded_text ===

    #[test]
    fn bounded_text_keeps_short_input_intact() {
        assert_eq!(
            bounded_text(b"docker: prune complete", MAX_CAPTURE_BYTES),
            "docker: prune complete"
        );
    }

    #[test]
    fn bounded_text_keeps_only_the_tail() {
        assert_eq!(bounded_text(b"abcdefghij", 3), "hij");
    }

    #[test]
    fn bounded_text_of_empty_input_is_empty() {
        assert_eq!(bounded_text(b"", 10), "");
    }

    /// The byte window can land inside a multi-byte sequence. The result must
    /// not begin with half a character, which would render as a replacement
    /// glyph in the notification.
    #[test]
    fn bounded_text_does_not_leave_a_broken_character() {
        let text = "ééééé";

        let bounded = bounded_text(text.as_bytes(), 5);

        assert!(
            !bounded.contains('\u{FFFD}'),
            "expected no replacement character, got {bounded}"
        );
        assert!(text.ends_with(&bounded));
    }

    /// Bytes that cannot be decoded are dropped from the front rather than
    /// poisoning the whole line.
    #[test]
    fn bounded_text_discards_undecodable_leading_bytes() {
        let buffer = [0xFF_u8, 0xFE, b'a', b'b'];

        assert_eq!(bounded_text(&buffer, 100), "ab");
    }

    /// A reader that never finishes must not extend the run: the deadline belongs
    /// to the caller, not to whatever still holds the pipe open.
    #[tokio::test]
    async fn capture_drains_within_the_deadline_even_if_a_reader_never_ends() {
        let stdout_task = tokio::spawn(std::future::pending::<String>());
        let stderr_task = tokio::spawn(std::future::pending::<String>());

        let bounded = tokio::time::timeout(
            Duration::from_secs(2),
            drain_capture(stdout_task, stderr_task, Duration::from_millis(50)),
        )
        .await;

        let (stdout, stderr) =
            bounded.expect("drain_capture never returned: the readers are unbounded");
        assert_eq!((stdout.as_str(), stderr.as_str()), ("", ""));
    }
}
