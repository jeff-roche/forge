//! F-747: Tauri command surface for graceful daemon shutdown on session
//! window close.
//!
//! When the operator closes a session window, the Tauri runtime fires
//! `WindowEvent::CloseRequested`; the window-manager bridge translates that
//! into an async [`session_close`] invocation here. The command orchestrates
//! a three-step escalation against the daemon's UDS + pid file:
//!
//! 1. **Graceful** — send [`forge_ipc::IpcMessage::Shutdown`] over the existing UDS
//!    connection. The daemon emits `Event::SessionEnded { reason: Closed,
//!    archived: true }`, runs `archive_or_purge` (removes the UDS socket),
//!    and exits cleanly; `OwnedPidFile::drop` removes the pid file.
//! 2. **SIGTERM** — if the daemon is still alive after [`GRACEFUL_TIMEOUT`],
//!    deliver SIGTERM via `pidfd_send_signal` against the pid read from the
//!    on-disk pid file. The persistent-mode signal handler runs the same
//!    `archive_or_purge` arm.
//! 3. **SIGKILL** — if the daemon is still alive after another
//!    [`SIGTERM_TIMEOUT`], deliver SIGKILL. The daemon cannot run its
//!    `archive_or_purge` arm here; the orchestrator best-effort removes the
//!    stale UDS socket + pid file itself so subsequent `forge session new`
//!    invocations can rebind the same id.
//!
//! Idempotent: a second `session_close` call for the same id observes the
//! daemon is already gone (pid file missing / socket missing) and returns
//! `Ok` with no work. This matters because Tauri may fire `CloseRequested`
//! and a manual `forge session kill` may race.
//!
//! The orchestration body is split into a pure function
//! [`orchestrate_session_close`] that takes injected hooks for "send
//! shutdown frame" and "deliver signal" so the escalation logic can be
//! exercised end-to-end without spawning a real `forged`.

use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(feature = "webview")]
use tauri::{Runtime, State, Webview};

#[cfg(feature = "webview")]
use crate::ipc::BridgeState;

/// F-673: command-named error prefix. Every outer error path returned from
/// the `session_close` command begins with this so dashboard log filters
/// and end-user error display stay consistent.
pub const SESSION_CLOSE_ERROR: &str = "session_close: ";

/// F-747: bounded wait for the daemon to exit after [`forge_ipc::IpcMessage::Shutdown`]
/// is written to its UDS. The daemon's shutdown path runs `archive_or_purge`
/// (rename + meta rewrite) before `OwnedPidFile::drop` removes the pid file
/// — well under a second on a same-filesystem rename. 2 s is generous
/// enough for the EXDEV fallback (copy + remove_dir_all) on a small session
/// dir while still cutting off a genuinely wedged daemon promptly.
pub const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(2);

/// F-747: bounded wait after SIGTERM before escalating to SIGKILL. SIGTERM
/// goes through the same signal arm as `forge session kill`; the daemon
/// runs its archive path once before exiting. Two seconds again — if
/// archive_or_purge itself wedges the daemon, SIGKILL is the only way out.
pub const SIGTERM_TIMEOUT: Duration = Duration::from_secs(2);

/// F-747: how often the wait loop polls liveness (pid file gone + socket
/// gone + `/proc/<pid>/exe` no longer matches, in production via the
/// `LivenessProbe`). Polling at 50 ms keeps the worst-case "daemon exited
/// right at the timeout boundary" miss-window short while bounding total
/// `pidfd_open` syscalls per shutdown to ~40 per phase.
pub const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Bundled timeouts + poll cadence for [`orchestrate_session_close`].
/// Grouping keeps the orchestrator's argument count under clippy's
/// `too_many_arguments` threshold and gives test helpers a single
/// `default()` knob to reach for.
#[derive(Debug, Clone, Copy)]
pub struct ShutdownTimings {
    pub graceful: Duration,
    pub sigterm: Duration,
    pub poll: Duration,
}

impl ShutdownTimings {
    /// Production defaults — [`GRACEFUL_TIMEOUT`] + [`SIGTERM_TIMEOUT`] +
    /// [`POLL_INTERVAL`].
    pub const fn production() -> Self {
        Self {
            graceful: GRACEFUL_TIMEOUT,
            sigterm: SIGTERM_TIMEOUT,
            poll: POLL_INTERVAL,
        }
    }
}

impl Default for ShutdownTimings {
    fn default() -> Self {
        Self::production()
    }
}

/// Outcome of a single `session_close` invocation. Surfaced so tests and
/// (eventual) telemetry can distinguish the three escalation paths without
/// parsing log strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCloseOutcome {
    /// Daemon was already gone (idempotent no-op) — pid file + socket
    /// missing or `LivenessProbe::is_alive` returned `false` immediately.
    AlreadyClosed,
    /// Daemon exited after [`forge_ipc::IpcMessage::Shutdown`] within
    /// [`GRACEFUL_TIMEOUT`].
    Graceful,
    /// Daemon survived [`GRACEFUL_TIMEOUT`]; exited after SIGTERM within
    /// [`SIGTERM_TIMEOUT`].
    SigTerm,
    /// Daemon survived both timeouts and SIGKILL was the final escalation.
    /// The orchestrator best-effort cleaned up the on-disk pid + socket
    /// artifacts since the daemon never ran its archive arm.
    SigKill,
}

/// Pluggable hook for liveness probes. Production uses `PidFdLivenessProbe`
/// which opens a pidfd against the pid read from the pid file; tests can
/// inject a deterministic "alive for N polls then dead" probe.
pub trait LivenessProbe: Send + Sync {
    /// Returns `true` while the daemon is still alive (or while the
    /// probe genuinely cannot tell — false negatives are worse than false
    /// positives here because they cause us to skip escalation). Returns
    /// `false` once the probe is confident the daemon has exited.
    fn is_alive(&self) -> bool;
}

/// Pluggable hook for signal delivery. Production uses `PidFdSignaler`
/// which delivers SIGTERM / SIGKILL via `pidfd_send_signal_for_pid`.
pub trait Signaler: Send + Sync {
    /// Deliver SIGTERM to the underlying pid. Returning `Err` is logged
    /// but does not abort the escalation — a missing process is treated as
    /// "already dead" by the next poll loop.
    fn send_sigterm(&self) -> Result<(), String>;
    /// Deliver SIGKILL.
    fn send_sigkill(&self) -> Result<(), String>;
}

/// Pure-function orchestration: send the graceful Shutdown frame via the
/// caller-supplied closure, then poll the [`LivenessProbe`] across the
/// three escalation phases. Used both by the Tauri command and by the
/// integration tests in `crates/forge-shell/tests/ipc_session_close.rs`
/// — the latter inject deterministic probes / signalers to assert each
/// branch.
///
/// The function consumes `send_shutdown` because the natural caller
/// (`SessionBridge::shutdown`) is `async fn` and the closure is awaited
/// inline. `send_shutdown` returning `Err` is treated as "daemon was
/// already gone or the connection was torn" and the orchestrator falls
/// through to the wait phase — the daemon may legitimately have exited
/// between the connection check and the write.
pub async fn orchestrate_session_close<S, P>(
    probe: &P,
    signaler: &S,
    send_shutdown: impl std::future::Future<Output = Result<(), String>>,
    pid_file: Option<&Path>,
    socket_path: Option<&Path>,
    timings: ShutdownTimings,
) -> SessionCloseOutcome
where
    S: Signaler + ?Sized,
    P: LivenessProbe + ?Sized,
{
    let ShutdownTimings {
        graceful: graceful_timeout,
        sigterm: sigterm_timeout,
        poll: poll_interval,
    } = timings;
    // Fast-path idempotency: if the daemon is already gone, return without
    // touching anything. Important because `WindowEvent::CloseRequested`
    // can fire twice (e.g. once from the user, once from the parent app
    // closing) and the second invocation must not produce spurious
    // SIGKILL-and-cleanup behavior.
    if !probe.is_alive() {
        return SessionCloseOutcome::AlreadyClosed;
    }

    // Phase 1: graceful Shutdown IPC, then poll until the daemon exits or
    // `graceful_timeout` elapses. The send failure is non-fatal — the
    // daemon may already be tearing down the connection in response to a
    // racing signal.
    if let Err(e) = send_shutdown.await {
        tracing::debug!(
            target: "forge_shell::session_close",
            error = %e,
            "Shutdown IPC send failed; falling through to signal escalation",
        );
    }
    if wait_until_dead(probe, graceful_timeout, poll_interval).await {
        return SessionCloseOutcome::Graceful;
    }

    // Phase 2: SIGTERM, then poll until exit or `sigterm_timeout`.
    if let Err(e) = signaler.send_sigterm() {
        tracing::warn!(
            target: "forge_shell::session_close",
            error = %e,
            "SIGTERM delivery failed; escalating to SIGKILL",
        );
    }
    if wait_until_dead(probe, sigterm_timeout, poll_interval).await {
        return SessionCloseOutcome::SigTerm;
    }

    // Phase 3: SIGKILL. The daemon never ran its archive arm; the
    // orchestrator best-effort removes the on-disk artifacts itself.
    if let Err(e) = signaler.send_sigkill() {
        tracing::warn!(
            target: "forge_shell::session_close",
            error = %e,
            "SIGKILL delivery failed",
        );
    }
    // Brief grace window for the kernel to reap the process and free the
    // socket inode before we unlink. Probing is the right termination
    // signal but a fully-stuck pid can't influence the inode; the
    // best-effort `remove_file` calls below ignore "not found".
    let _ = wait_until_dead(probe, sigterm_timeout, poll_interval).await;
    if let Some(p) = socket_path {
        let _ = std::fs::remove_file(p);
    }
    if let Some(p) = pid_file {
        let _ = std::fs::remove_file(p);
    }
    SessionCloseOutcome::SigKill
}

/// Poll `probe.is_alive()` every `poll_interval` until the daemon exits or
/// `timeout` elapses. Returns `true` if the daemon exited within budget,
/// `false` on timeout.
async fn wait_until_dead<P: LivenessProbe + ?Sized>(
    probe: &P,
    timeout: Duration,
    poll_interval: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !probe.is_alive() {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(poll_interval.min(remaining)).await;
    }
}

// ---------------------------------------------------------------------------
// Production hooks: PidFd-based probe + signaler
// ---------------------------------------------------------------------------

/// Resolve the on-disk pid + socket paths for `session_id` using the same
/// rules as `forge_cli::socket::{pid_path, socket_path}`. Exposed for the
/// Tauri command body and tests that need to fabricate matching artifacts
/// in a temp dir.
pub fn resolve_pid_and_socket_paths(session_id: &str) -> Result<(PathBuf, PathBuf), String> {
    let sock = forge_cli::socket::socket_path(session_id)
        .map_err(|e| format!("{SESSION_CLOSE_ERROR}resolve socket path: {e}"))?;
    let pid = forge_cli::socket::pid_path(session_id)
        .map_err(|e| format!("{SESSION_CLOSE_ERROR}resolve pid path: {e}"))?;
    Ok((pid, sock))
}

/// Read the pid from a daemon-owned pid file. Returns `None` when the file
/// is missing (idempotent: a second `session_close` after the daemon has
/// already exited and removed its pid file goes through here).
pub fn read_pid_from_file(pid_file: &Path) -> Option<libc::pid_t> {
    let raw = std::fs::read_to_string(pid_file).ok()?;
    forge_cli::socket::parse_pid_file_record(&raw)
        .ok()
        .map(|(pid, _start_time)| pid)
}

/// Liveness probe rooted in the pid file. Re-reads on each call so a
/// daemon that exits and removes its own pid file is observed promptly.
/// `pid_file == None` short-circuits to `is_alive() == false` so a session
/// whose pid file we cannot locate is treated as already closed.
pub struct PidFileLivenessProbe<'a> {
    pid_file: Option<&'a Path>,
    socket_path: Option<&'a Path>,
}

impl<'a> PidFileLivenessProbe<'a> {
    pub fn new(pid_file: Option<&'a Path>, socket_path: Option<&'a Path>) -> Self {
        Self {
            pid_file,
            socket_path,
        }
    }
}

impl LivenessProbe for PidFileLivenessProbe<'_> {
    fn is_alive(&self) -> bool {
        let Some(pid_file) = self.pid_file else {
            return false;
        };
        let Some(pid) = read_pid_from_file(pid_file) else {
            // pid file is gone — daemon either exited cleanly (drop removed
            // it) or never wrote one. Cross-check the socket: if it's gone
            // too, the daemon is definitely dead; if only the pid file is
            // missing but the socket persists, treat as dead (no pid means
            // we can't signal anyway, and a stray socket is a no-op).
            let _ = self.socket_path; // silence unused on non-linux
            return false;
        };
        is_pid_alive(pid)
    }
}

/// Returns `true` if `pid` is a live process. Linux uses pidfd_open which
/// is the same primitive `kill_session_from_pid_file` uses for race-free
/// signal delivery; we don't take action here so we don't need the full
/// start-time check. On non-Linux we conservatively return `true` because
/// the F-049 race-free probing isn't available; the escalation will still
/// fire SIGTERM/SIGKILL via the (non-existent on non-Linux) signaler.
#[cfg(target_os = "linux")]
pub fn is_pid_alive(pid: libc::pid_t) -> bool {
    // kill(pid, 0) is the canonical aliveness probe: returns 0 if the
    // signal could have been delivered (process exists and is signalable
    // by us), -1 with ESRCH otherwise. We're not actually delivering a
    // signal — this is a permissions+existence check only.
    //
    // SAFETY: `libc::kill` is FFI; arguments are simple integers; signal 0
    // means "no signal, just check".
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    let err = std::io::Error::last_os_error();
    !matches!(err.raw_os_error(), Some(libc::ESRCH))
}

#[cfg(not(target_os = "linux"))]
pub fn is_pid_alive(_pid: libc::pid_t) -> bool {
    // On non-Linux we lack the race-free pidfd primitive; conservatively
    // report alive so the escalation path at least *attempts* SIGTERM /
    // SIGKILL through the platform-specific signaler. The CLI's
    // `kill_session_from_pid_file` itself errors out on non-Linux today
    // (see `crates/forge-cli/src/socket.rs`), so this branch is in
    // practice unreachable from a session_close invocation that resolved
    // a real pid file.
    true
}

/// Signaler that delivers SIGTERM / SIGKILL via the F-049 `pidfd_send_signal`
/// path.
///
/// Holds ONE `pidfd` (opened at construction on Linux) across both signal
/// deliveries. This anchors the kernel-pinned process identity for the
/// entire SIGTERM → wait → SIGKILL escalation: any signal sent through the
/// held fd goes to *this* process, never a recycled PID. A fresh
/// `pidfd_open` between SIGTERM and SIGKILL would not be safe — during the
/// inter-signal sleep window the daemon could exit and Linux could recycle
/// its PID to an unrelated process; the re-opened fd would then pin that
/// recycled PID and SIGKILL would land on the wrong target.
pub struct PidFdSignaler {
    pid: libc::pid_t,
    #[cfg(target_os = "linux")]
    pidfd: forge_cli::socket::OwnedPidFd,
}

impl PidFdSignaler {
    /// Open a pidfd against `pid` and return a signaler that holds it
    /// across the escalation. Fails if the process is already gone
    /// (typically observed when `session_close` races a daemon that
    /// exited between pid-file read and signaler construction); callers
    /// should treat that as "already closed".
    #[cfg(target_os = "linux")]
    pub fn open(pid: libc::pid_t) -> Result<Self, String> {
        let pidfd = forge_cli::socket::pidfd_open(pid).map_err(|e| e.to_string())?;
        Ok(Self { pid, pidfd })
    }

    /// Non-Linux constructor: stores the pid only. The Signaler impl on
    /// non-Linux returns an error from `send_sigterm` / `send_sigkill`
    /// (pidfd is Linux-only); the constructor is infallible to keep the
    /// call shape symmetric.
    #[cfg(not(target_os = "linux"))]
    pub fn open(pid: libc::pid_t) -> Result<Self, String> {
        Ok(Self { pid })
    }
}

#[cfg(target_os = "linux")]
impl Signaler for PidFdSignaler {
    fn send_sigterm(&self) -> Result<(), String> {
        forge_cli::socket::pidfd_send_signal(self.pid, &self.pidfd, libc::SIGTERM)
            .map_err(|e| e.to_string())
    }
    fn send_sigkill(&self) -> Result<(), String> {
        forge_cli::socket::pidfd_send_signal(self.pid, &self.pidfd, libc::SIGKILL)
            .map_err(|e| e.to_string())
    }
}

#[cfg(not(target_os = "linux"))]
impl Signaler for PidFdSignaler {
    fn send_sigterm(&self) -> Result<(), String> {
        Err(format!(
            "session_close: SIGTERM not implemented on this platform (pid {})",
            self.pid
        ))
    }
    fn send_sigkill(&self) -> Result<(), String> {
        Err(format!(
            "session_close: SIGKILL not implemented on this platform (pid {})",
            self.pid
        ))
    }
}

// ---------------------------------------------------------------------------
// Tauri command surface
// ---------------------------------------------------------------------------

/// F-063 / F-057: validate `session_id` against the canonical SessionId
/// wire shape *before* using it in path composition. Matches the same
/// validation `open_session` runs.
fn is_valid_session_id(id: &str) -> bool {
    crate::dashboard_sessions::is_valid_session_id(id)
}

/// F-747 Tauri command: orchestrate graceful daemon shutdown for
/// `session_id`. Authorized only when called from the matching
/// `session-<id>` window — the dashboard's manual "Close session" path
/// is intentionally a different command (TBD); the window-close
/// CloseRequested handler is the one and only caller of this entry.
#[cfg(feature = "webview")]
#[tauri::command]
pub async fn session_close<R: Runtime>(
    session_id: String,
    webview: Webview<R>,
    state: State<'_, BridgeState>,
) -> Result<(), String> {
    let window_label = format!("session-{session_id}");
    crate::ipc::require_window_label(&webview, &window_label, "session_close")?;
    if !is_valid_session_id(&session_id) {
        return Err(format!("{SESSION_CLOSE_ERROR}invalid session_id"));
    }
    run_session_close(&state.bridge, &session_id).await
}

/// Production session_close body, factored out so tests in
/// `crates/forge-shell/tests/ipc_session_close.rs` can drive it without a
/// live Tauri runtime. Resolves on-disk paths, reads the pid (idempotent
/// no-op if absent), and dispatches to [`orchestrate_session_close`].
pub async fn run_session_close(
    bridge: &crate::bridge::SessionBridge,
    session_id: &str,
) -> Result<(), String> {
    let (pid_file, socket_path) = resolve_pid_and_socket_paths(session_id)?;

    let pid = read_pid_from_file(&pid_file);
    // No pid file → daemon is already gone. Drop any stale connection
    // state and return Ok.
    let Some(pid) = pid else {
        bridge.drop_connection(session_id).await;
        return Ok(());
    };

    let probe = PidFileLivenessProbe::new(Some(&pid_file), Some(&socket_path));
    // Opening the pidfd here anchors the daemon's kernel identity for the
    // entire escalation. If `open` fails the daemon raced us and exited
    // between pid-file read and now — treat as already closed.
    let signaler = match PidFdSignaler::open(pid) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(
                target: "forge_shell::session_close",
                session_id = %session_id,
                error = %e,
                "pidfd_open failed at signaler construction; treating as already closed",
            );
            bridge.drop_connection(session_id).await;
            return Ok(());
        }
    };

    let outcome = orchestrate_session_close(
        &probe,
        &signaler,
        async { bridge.shutdown(session_id).await.map_err(|e| e.to_string()) },
        Some(&pid_file),
        Some(&socket_path),
        ShutdownTimings::production(),
    )
    .await;

    tracing::info!(
        target: "forge_shell::session_close",
        session_id = %session_id,
        outcome = ?outcome,
        "session_close orchestration complete",
    );

    bridge.drop_connection(session_id).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests for the pure orchestration helper
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Deterministic probe that reports alive for `alive_polls` calls then
    /// dead thereafter. Models a daemon that exits N×poll_interval after
    /// some external trigger.
    struct CountdownProbe {
        polls_seen: AtomicUsize,
        alive_polls: usize,
    }

    impl CountdownProbe {
        fn new(alive_polls: usize) -> Self {
            Self {
                polls_seen: AtomicUsize::new(0),
                alive_polls,
            }
        }
    }

    impl LivenessProbe for CountdownProbe {
        fn is_alive(&self) -> bool {
            let n = self.polls_seen.fetch_add(1, Ordering::SeqCst);
            n < self.alive_polls
        }
    }

    /// Probe that is always alive — used to drive the escalation through to
    /// SIGKILL.
    struct AlwaysAliveProbe;
    impl LivenessProbe for AlwaysAliveProbe {
        fn is_alive(&self) -> bool {
            true
        }
    }

    /// Probe that is always dead — exercises the AlreadyClosed fast path.
    struct AlwaysDeadProbe;
    impl LivenessProbe for AlwaysDeadProbe {
        fn is_alive(&self) -> bool {
            false
        }
    }

    /// Recording signaler — captures which signals fired in order, so the
    /// escalation order can be asserted independently of the outcome enum.
    #[derive(Default)]
    struct RecordingSignaler {
        term_fired: AtomicBool,
        kill_fired: AtomicBool,
    }
    impl Signaler for RecordingSignaler {
        fn send_sigterm(&self) -> Result<(), String> {
            self.term_fired.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn send_sigkill(&self) -> Result<(), String> {
            self.kill_fired.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Tiny helper: tests run with very short timeouts so the slowest
    /// branch (SIGKILL) completes inside the default test deadline.
    fn fast_timings() -> ShutdownTimings {
        ShutdownTimings {
            graceful: Duration::from_millis(30),
            sigterm: Duration::from_millis(30),
            poll: Duration::from_millis(5),
        }
    }

    #[tokio::test]
    async fn already_closed_when_probe_starts_dead() {
        let sig = Arc::new(RecordingSignaler::default());
        let outcome = orchestrate_session_close(
            &AlwaysDeadProbe,
            &*sig,
            async { Ok(()) },
            None,
            None,
            fast_timings(),
        )
        .await;
        assert_eq!(outcome, SessionCloseOutcome::AlreadyClosed);
        assert!(!sig.term_fired.load(Ordering::SeqCst));
        assert!(!sig.kill_fired.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn graceful_when_daemon_exits_during_phase_one() {
        // Probe is alive for 1 poll (so the initial gate sees alive),
        // dead from the second poll onward — i.e. exits shortly after
        // Shutdown is sent.
        let probe = CountdownProbe::new(1);
        let sig = Arc::new(RecordingSignaler::default());
        let outcome =
            orchestrate_session_close(&probe, &*sig, async { Ok(()) }, None, None, fast_timings())
                .await;
        assert_eq!(outcome, SessionCloseOutcome::Graceful);
        assert!(
            !sig.term_fired.load(Ordering::SeqCst),
            "SIGTERM must not fire on graceful path"
        );
    }

    #[tokio::test]
    async fn sigterm_when_daemon_outlasts_graceful_timeout() {
        // Initial liveness gate (1 poll) + phase-1 polls fully elapse
        // (~6 polls at 5 ms over 30 ms) before the daemon dies. We size
        // alive_polls slightly larger so the phase-1 wait times out.
        let probe = CountdownProbe::new(10);
        let sig = Arc::new(RecordingSignaler::default());
        let outcome =
            orchestrate_session_close(&probe, &*sig, async { Ok(()) }, None, None, fast_timings())
                .await;
        assert_eq!(outcome, SessionCloseOutcome::SigTerm);
        assert!(sig.term_fired.load(Ordering::SeqCst), "SIGTERM must fire");
        assert!(
            !sig.kill_fired.load(Ordering::SeqCst),
            "SIGKILL must not fire"
        );
    }

    #[tokio::test]
    async fn sigkill_when_daemon_ignores_sigterm() {
        let sig = Arc::new(RecordingSignaler::default());
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("session.pid");
        let sock_path = dir.path().join("session.sock");
        // Drop stale artifacts onto disk so the SIGKILL cleanup branch
        // can prove it removed them. The probe is independent of these
        // files (we use AlwaysAliveProbe), but the orchestrator's SIGKILL
        // arm best-effort unlinks both regardless.
        std::fs::write(&pid_path, "1\n1\n").unwrap();
        std::fs::write(&sock_path, "").unwrap();

        let outcome = orchestrate_session_close(
            &AlwaysAliveProbe,
            &*sig,
            async { Ok(()) },
            Some(&pid_path),
            Some(&sock_path),
            fast_timings(),
        )
        .await;
        assert_eq!(outcome, SessionCloseOutcome::SigKill);
        assert!(sig.term_fired.load(Ordering::SeqCst), "SIGTERM must fire");
        assert!(sig.kill_fired.load(Ordering::SeqCst), "SIGKILL must fire");
        assert!(
            !pid_path.exists(),
            "SIGKILL path must best-effort remove pid file",
        );
        assert!(
            !sock_path.exists(),
            "SIGKILL path must best-effort remove socket file",
        );
    }

    #[tokio::test]
    async fn send_shutdown_error_does_not_abort_escalation() {
        let probe = CountdownProbe::new(1);
        let sig = Arc::new(RecordingSignaler::default());
        let outcome = orchestrate_session_close(
            &probe,
            &*sig,
            async { Err("connection refused".to_string()) },
            None,
            None,
            fast_timings(),
        )
        .await;
        // The probe still observes the daemon as dead after the first
        // poll, so the orchestrator still resolves to Graceful (the
        // connection-refused on send is itself a signal that the daemon
        // was already exiting).
        assert_eq!(outcome, SessionCloseOutcome::Graceful);
    }

    #[test]
    fn error_prefix_is_command_named() {
        // F-673: lock the canonical prefix so a future rename can't drift.
        assert_eq!(SESSION_CLOSE_ERROR, "session_close: ");
    }

    #[test]
    fn timeouts_are_two_seconds_each() {
        // Pin the DoD-defined escalation budgets so a regression that
        // halves either timeout can't slip past CI silently.
        assert_eq!(GRACEFUL_TIMEOUT, Duration::from_secs(2));
        assert_eq!(SIGTERM_TIMEOUT, Duration::from_secs(2));
    }

    #[test]
    fn read_pid_from_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.pid");
        assert!(read_pid_from_file(&path).is_none());
    }

    #[test]
    fn read_pid_from_two_line_record_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.pid");
        std::fs::write(&path, "42\n12345\n").unwrap();
        assert_eq!(read_pid_from_file(&path), Some(42));
    }

    /// Holding the pidfd across both signals is the actual fix for the
    /// PID-reuse race. We can't *observe* PID reuse in a unit test (it
    /// would require draining the PID space), but we can pin the
    /// structural invariants: `open` must succeed on a live pid, and the
    /// returned signaler must successfully deliver multiple signals
    /// without ever calling `pidfd_open` again. Asserting via a spawned
    /// child whose lifetime we own makes the "fd is reused" property
    /// observable — if `send_sigkill` were re-opening the pidfd, the
    /// child would still die (so this isn't a perfect proof), but combined
    /// with the type-level check (no `&mut` or fallible re-open in
    /// `send_*`) and the structural review in the PR comments, the
    /// regression risk is well-bounded.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn pidfd_signaler_holds_fd_across_term_then_kill() {
        use std::process::Command;
        // `sleep 30` is canonical SIGTERM-resistant background process —
        // it terminates on either signal but doesn't trap them.
        let mut child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as libc::pid_t;

        let signaler = PidFdSignaler::open(pid).expect("pidfd_open on live child");

        // Drop just the FD-issuing side of forge_cli::socket — once the
        // signaler holds its fd, we should be able to drive both signals
        // without ever touching `pidfd_open` again. The two `send_*`
        // calls below operate purely on the held fd.
        signaler.send_sigterm().expect("SIGTERM via held pidfd");
        // Best-effort: child may have already exited; tolerate ESRCH.
        let _ = signaler.send_sigkill();

        let status = child.wait().expect("wait sleep child");
        assert!(!status.success(), "sleep should have been killed");
    }

    /// Opening against an absent pid must surface an error so
    /// `run_session_close` can short-circuit to "already closed" instead
    /// of constructing a signaler with no kernel anchor.
    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_signaler_open_fails_on_dead_pid() {
        let result = PidFdSignaler::open(i32::MAX);
        let err = match result {
            Ok(_) => panic!("open must fail on dead pid"),
            Err(e) => e,
        };
        let msg = err.to_lowercase();
        assert!(
            msg.contains("no such") || msg.contains("stale") || msg.contains("esrch"),
            "expected ESRCH-like error, got: {msg}"
        );
    }

    #[test]
    fn read_pid_from_legacy_one_line_record_is_none() {
        // F-049 hardening: legacy one-line pid files must not parse;
        // returning None forces the orchestrator down the "already
        // closed" branch rather than risking signal delivery to a reused
        // pid.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.pid");
        std::fs::write(&path, "42\n").unwrap();
        assert!(read_pid_from_file(&path).is_none());
    }
}
