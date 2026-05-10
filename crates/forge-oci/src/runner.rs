//! Indirection over `tokio::process::Command` so the [`PodmanRuntime`]
//! can be unit-tested without an actual `podman` binary on the host.
//!
//! Production wiring uses [`TokioCommandRunner`]; tests use
//! `RecordingRunner` (or any custom `CommandRunner`) to capture argv arrays
//! and stub responses. The mock toolkit (`RecordingRunner`, `StubResponse`,
//! `RecordedCalls`, `RecordedCall`) is gated behind the `test-support`
//! feature flag so downstream crates only see it in their dev builds.
//!
//! [`PodmanRuntime`]: crate::PodmanRuntime

use async_trait::async_trait;
#[cfg(any(test, feature = "test-support"))]
use std::sync::{Arc, Mutex};
use tokio::process::Command;

/// What a [`CommandRunner`] returns: stdout/stderr/exit code.
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    /// Process exit code if one was produced.
    pub exit_code: Option<i32>,
    /// Captured stdout bytes.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes.
    pub stderr: Vec<u8>,
}

impl CommandOutcome {
    /// Convenience: was the exit code zero?
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Trait the runtime calls into to invoke external binaries. Decouples
/// argv-shaping logic (the part we want to test) from process spawning
/// (the part the OS owns).
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run `program` with the given `args`, capturing stdout/stderr.
    /// Implementations must NOT pass these through a shell. Argv only.
    async fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CommandOutcome>;
}

/// Production runner: shells out via `tokio::process::Command`.
#[derive(Debug, Default, Clone)]
pub struct TokioCommandRunner;

#[async_trait]
impl CommandRunner for TokioCommandRunner {
    async fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CommandOutcome> {
        let output = Command::new(program).args(args).output().await?;
        Ok(CommandOutcome {
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

// ── Test-only mock toolkit ─────────────────────────────────────────────
//
// Gated on `cfg(any(test, feature = "test-support"))` so the published
// API surface of `forge-oci` (default features) carries only the
// production runner. Downstream crates that drive
// `PodmanRuntime::with_runner` from their own tests opt in via
// `forge-oci = { ..., features = ["test-support"] }`.

/// One pre-canned response for a [`RecordingRunner`].
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone)]
pub struct StubResponse {
    /// Optional argv-prefix the call must match. `None` matches anything.
    pub matches_args: Option<Vec<String>>,
    /// Outcome to return.
    pub outcome: CommandOutcome,
}

#[cfg(any(test, feature = "test-support"))]
impl StubResponse {
    /// Always-successful stdout response, regardless of argv.
    pub fn ok_stdout(stdout: impl Into<Vec<u8>>) -> Self {
        Self {
            matches_args: None,
            outcome: CommandOutcome {
                exit_code: Some(0),
                stdout: stdout.into(),
                stderr: Vec::new(),
            },
        }
    }

    /// Failing response (exit 1 with stderr).
    pub fn err(stderr: impl Into<Vec<u8>>) -> Self {
        Self {
            matches_args: None,
            outcome: CommandOutcome {
                exit_code: Some(1),
                stdout: Vec::new(),
                stderr: stderr.into(),
            },
        }
    }
}

/// One recorded invocation: the program name and the argv it was called with.
#[cfg(any(test, feature = "test-support"))]
pub type RecordedCall = (String, Vec<String>);

/// Shared, mutex-guarded log of every call a [`RecordingRunner`] has seen.
#[cfg(any(test, feature = "test-support"))]
pub type RecordedCalls = Arc<Mutex<Vec<RecordedCall>>>;

/// Test-only runner that records every invocation and returns canned
/// responses in FIFO order.
///
/// Behaviour:
/// - Each `run` call pops the front of the configured response queue.
/// - If the queue is empty, returns a successful empty outcome.
/// - Every call (program, argv) is recorded for later assertion.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default, Clone)]
pub struct RecordingRunner {
    responses: Arc<Mutex<std::collections::VecDeque<StubResponse>>>,
    /// Recorded calls. Cloneable handle so tests can read after the runner is
    /// moved into the runtime under test.
    pub calls: RecordedCalls,
}

#[cfg(any(test, feature = "test-support"))]
impl RecordingRunner {
    /// Empty runner — every call returns an empty success outcome.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a response to be returned by the next call.
    pub fn push(&self, response: StubResponse) {
        self.responses.lock().unwrap().push_back(response);
    }

    /// Snapshot of recorded calls, in invocation order.
    pub fn recorded_calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl CommandRunner for RecordingRunner {
    async fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CommandOutcome> {
        self.calls.lock().unwrap().push((
            program.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
        ));
        let next = self.responses.lock().unwrap().pop_front();
        Ok(next.map(|r| r.outcome).unwrap_or(CommandOutcome {
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }))
    }
}
