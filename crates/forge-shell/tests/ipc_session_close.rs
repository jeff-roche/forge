//! F-747: end-to-end test for the shell-side graceful daemon shutdown.
//!
//! Spawns a real `forged` daemon with a known pid file + socket path,
//! drives it through the [`SessionBridge`], invokes the orchestration
//! function exactly as the Tauri command would, and asserts that:
//!
//! - the daemon exits with status 0,
//! - the on-disk UDS socket is removed,
//! - the pid file is removed,
//! - the outcome reported is [`SessionCloseOutcome::Graceful`].
//!
//! Escalation paths (SIGTERM, SIGKILL) are covered by the pure-function
//! unit tests in `session_close_ipc.rs` — they inject deterministic
//! probes / signalers so the escalation logic is testable without
//! conjuring a wedged daemon binary in CI.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use forge_shell::bridge::{EventSink, SessionBridge, SessionConnections, SessionEventPayload};
use forge_shell::session_close_ipc::{
    orchestrate_session_close, PidFdSignaler, PidFileLivenessProbe, SessionCloseOutcome,
    ShutdownTimings,
};
use tempfile::TempDir;
use tokio::sync::mpsc;

/// Resolve the `forged` binary path alongside the running test binary. The
/// `forge-shell` package doesn't ship `forged` itself (it lives in
/// `forge-session`), so the cargo-injected `CARGO_BIN_EXE_*` env var is not
/// available here. The CLI's `spawn::find_forged_binary` uses the same
/// `current_exe().parent()` lookup at runtime; mirroring it for the test
/// keeps the daemon discovery rule single-sourced.
fn forged_path() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    // tests run from `target/debug/deps/<test>-<hash>`; forged lives at
    // `target/debug/forged`.
    let mut candidate = exe
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("forged"))
        .expect("test exe parent");
    if !candidate.exists() {
        // Fallback: alongside the test binary itself (cargo may place it
        // differently under nextest or artifact-dep layouts).
        candidate = exe
            .parent()
            .map(|p| p.join("forged"))
            .expect("test exe parent");
    }
    assert!(
        candidate.exists(),
        "forged binary not found alongside test exe; build with `cargo build -p forge-session --bin forged` first ({})",
        candidate.display(),
    );
    candidate
}

struct NoopSink;
impl EventSink for NoopSink {
    fn emit(&self, _payload: SessionEventPayload) {}
}

async fn wait_for_socket(path: &Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("forged did not create socket at {}", path.display());
}

/// F-747 end-to-end: spawn forged, run `orchestrate_session_close` via the
/// real `SessionBridge::shutdown` path + the real `PidFileLivenessProbe`,
/// and assert the daemon exits cleanly and reaps its artifacts.
#[tokio::test]
async fn graceful_shutdown_via_orchestrator_exits_daemon_and_reaps_artifacts() {
    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let session_id = "shell-close-test";
    let sock_path = dir.path().join("session.sock");
    let pid_path = dir.path().join("session.pid");

    let mut child = tokio::process::Command::new(forged_path())
        .env("FORGE_SESSION_ID", session_id)
        .env("FORGE_SOCKET_PATH", &sock_path)
        .env("FORGE_PID_FILE", &pid_path)
        .env("FORGE_WORKSPACE", &workspace)
        // F-743: explicit provider spec required; the close path doesn't
        // drive any turns so Mock is sufficient.
        .env("FORGE_PROVIDER", "mock")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn forged");

    wait_for_socket(&sock_path).await;

    // Stand up a SessionBridge against the daemon's socket. Mirrors
    // production: hello + subscribe so the daemon's writer loop is
    // pumping live events when Shutdown arrives.
    let connections = SessionConnections::new();
    let bridge = SessionBridge::new(connections);
    let _ack = bridge
        .hello(session_id, Some(&sock_path))
        .await
        .expect("hello");
    let (tx, _rx) = mpsc::unbounded_channel::<SessionEventPayload>();
    let sink: Arc<dyn EventSink> = Arc::new(SinkAdapter { tx });
    bridge
        .subscribe(session_id, 0, sink)
        .await
        .expect("subscribe");

    // Read the pid as the production command would. Must exist while
    // forged is running.
    let pid = forge_shell::session_close_ipc::read_pid_from_file(&pid_path)
        .expect("pid file present while forged is alive");

    let probe = PidFileLivenessProbe::new(Some(&pid_path), Some(&sock_path));
    let signaler = PidFdSignaler::open(pid).expect("pidfd_open on live daemon");

    let outcome = orchestrate_session_close(
        &probe,
        &signaler,
        async { bridge.shutdown(session_id).await.map_err(|e| e.to_string()) },
        Some(&pid_path),
        Some(&sock_path),
        // Production timeouts are 2 s each; keep them for the e2e so the
        // archive arm has the same budget the dashboard would give.
        ShutdownTimings::production(),
    )
    .await;

    assert_eq!(
        outcome,
        SessionCloseOutcome::Graceful,
        "expected Graceful outcome — daemon should exit on Shutdown IPC well under 2s",
    );

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("forged did not exit after graceful shutdown")
        .expect("child.wait()");
    assert!(
        status.success(),
        "forged exited non-zero after graceful shutdown: {:?}",
        status.code(),
    );

    assert!(
        !sock_path.exists(),
        "UDS socket must be removed by archive_or_purge: {}",
        sock_path.display(),
    );
    assert!(
        !pid_path.exists(),
        "pid file must be removed on clean exit: {}",
        pid_path.display(),
    );

    bridge.drop_connection(session_id).await;
}

/// F-747 idempotency: calling `orchestrate_session_close` a second time
/// (after the first run reaped the daemon) must return
/// `AlreadyClosed` — no signal delivery, no failed `kill` syscalls.
#[tokio::test]
async fn second_close_after_daemon_already_gone_is_already_closed() {
    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let session_id = "shell-close-idempotent";
    let sock_path = dir.path().join("session.sock");
    let pid_path = dir.path().join("session.pid");

    let mut child = tokio::process::Command::new(forged_path())
        .env("FORGE_SESSION_ID", session_id)
        .env("FORGE_SOCKET_PATH", &sock_path)
        .env("FORGE_PID_FILE", &pid_path)
        .env("FORGE_WORKSPACE", &workspace)
        .env("FORGE_PROVIDER", "mock")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn forged");

    wait_for_socket(&sock_path).await;

    let connections = SessionConnections::new();
    let bridge = SessionBridge::new(connections);
    let _ack = bridge
        .hello(session_id, Some(&sock_path))
        .await
        .expect("hello");
    bridge
        .subscribe(session_id, 0, Arc::new(NoopSink))
        .await
        .expect("subscribe");

    let pid =
        forge_shell::session_close_ipc::read_pid_from_file(&pid_path).expect("pid file present");
    let probe = PidFileLivenessProbe::new(Some(&pid_path), Some(&sock_path));
    let signaler = PidFdSignaler::open(pid).expect("pidfd_open on live daemon");

    // First call: graceful shutdown.
    let first = orchestrate_session_close(
        &probe,
        &signaler,
        async { bridge.shutdown(session_id).await.map_err(|e| e.to_string()) },
        Some(&pid_path),
        Some(&sock_path),
        ShutdownTimings::production(),
    )
    .await;
    assert_eq!(first, SessionCloseOutcome::Graceful);
    let _ = child.wait().await;
    bridge.drop_connection(session_id).await;

    // Second call: pid file and socket are both gone. The probe reports
    // not-alive immediately and the orchestrator must short-circuit
    // without signaling. We can't construct a real `PidFdSignaler` here
    // (pidfd_open would fail on the dead pid), so plug in a panicking
    // stub: if the orchestrator wrongly tries to escalate, the test
    // fails loudly rather than silently signaling an unrelated process.
    struct PanicOnSignal;
    impl forge_shell::session_close_ipc::Signaler for PanicOnSignal {
        fn send_sigterm(&self) -> Result<(), String> {
            panic!("AlreadyClosed fast path must bypass SIGTERM");
        }
        fn send_sigkill(&self) -> Result<(), String> {
            panic!("AlreadyClosed fast path must bypass SIGKILL");
        }
    }

    let probe2 = PidFileLivenessProbe::new(Some(&pid_path), Some(&sock_path));
    let signaler2 = PanicOnSignal;
    let second = orchestrate_session_close(
        &probe2,
        &signaler2,
        async { Err::<(), String>("connection refused (expected — daemon is gone)".to_string()) },
        Some(&pid_path),
        Some(&sock_path),
        ShutdownTimings::production(),
    )
    .await;
    assert_eq!(
        second,
        SessionCloseOutcome::AlreadyClosed,
        "second close after the daemon is already gone must be a no-op",
    );
}

// Adapter so the bridge subscribe sink can drive a channel for tests that
// want to observe events. The graceful-shutdown test currently doesn't
// inspect the stream — the daemon-side `session_close_ipc` integration
// test in `forge-session` already pins the SessionEnded { reason: Closed }
// emission — so this is a Drop-and-forget sink.
struct SinkAdapter {
    tx: mpsc::UnboundedSender<SessionEventPayload>,
}
impl EventSink for SinkAdapter {
    fn emit(&self, payload: SessionEventPayload) {
        let _ = self.tx.send(payload);
    }
}
