//! F-748: end-to-end test for the crash-restart prompt flow.
//!
//! Spawns a real `forged` daemon, drives a turn so the event log has
//! durable content, SIGKILLs the daemon (no archive arm runs — the prior
//! pid file + socket survive), invokes `run_session_restart` exactly as
//! the Tauri command would, and asserts:
//!
//! - the new daemon binds the same socket path,
//! - re-attaching with `Subscribe { since: last_seq }` produces ZERO
//!   duplicate events from the persisted log,
//! - the events ARE preserved (a fresh `Subscribe { since: 0 }` replays
//!   the pre-crash transcript),
//! - the new daemon's pid differs from the killed one (proves we
//!   re-spawned rather than handing back a stale process handle).

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use forge_core::Event;
use forge_shell::bridge::{EventSink, SessionBridge, SessionConnections, SessionEventPayload};
use forge_shell::session_restart_ipc::run_session_restart;
use tempfile::TempDir;
use tokio::sync::mpsc;

/// Resolve the `forged` binary path alongside the running test binary —
/// same rule as `ipc_session_close.rs`.
fn forged_path() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let mut candidate = exe
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("forged"))
        .expect("test exe parent");
    if !candidate.exists() {
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

struct ChannelSink {
    tx: mpsc::UnboundedSender<SessionEventPayload>,
}
impl EventSink for ChannelSink {
    fn emit(&self, payload: SessionEventPayload) {
        let _ = self.tx.send(payload);
    }
}

async fn wait_for_socket(path: &std::path::Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("forged did not create socket at {}", path.display());
}

#[tokio::test]
async fn session_restart_resumes_event_log_without_duplicates() {
    // The session_id is the hex-shape `is_valid_session_id` accepts and
    // also what the daemon stamps into the on-disk log directory. The
    // event log lives under `<workspace>/.forge/sessions/<id>/events.jsonl`
    // when the daemon is launched with `FORGE_WORKSPACE` set, so resume
    // must observe the same file across the two daemon lifetimes.
    let session_id = "deadbeefcafebabe";

    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    // The restart path's `spawn_forged_session_with_id` resolves the socket
    // + pid paths via the canonical `forge_cli::socket` helpers (which
    // honour `XDG_RUNTIME_DIR` / platform conventions); use those same
    // helpers here so the two daemon lifetimes share a UDS path.
    let sock_path = forge_cli::socket::socket_path(session_id).expect("socket_path");
    let pid_path = forge_cli::socket::pid_path(session_id).expect("pid_path");
    // Clean up any artifact left by a prior test run so the original
    // spawn isn't blocked by an O_EXCL pid-file collision.
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&pid_path);

    // -- Phase 1: spawn the original daemon and emit one event we'll
    // assert is preserved across the restart. The Mock provider's default
    // sequence is empty, so the daemon stays idle; we drive a single
    // `send_message` to land a `UserMessage` event on the log.
    let mut child = tokio::process::Command::new(forged_path())
        .env("FORGE_SESSION_ID", session_id)
        .env("FORGE_SOCKET_PATH", &sock_path)
        .env("FORGE_PID_FILE", &pid_path)
        .env("FORGE_WORKSPACE", &workspace)
        .env("FORGE_PROVIDER", "mock")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn forged #1");
    let first_pid = child.id().expect("child pid");

    wait_for_socket(&sock_path).await;

    let bridge = SessionBridge::new(SessionConnections::new());
    let _ack = bridge
        .hello(session_id, Some(&sock_path))
        .await
        .expect("hello #1");

    let (tx, mut rx) = mpsc::unbounded_channel();
    bridge
        .subscribe(session_id, 0, Arc::new(ChannelSink { tx }))
        .await
        .expect("subscribe #1");

    bridge
        .send_message(session_id, "first turn".to_string())
        .await
        .expect("send_message #1");

    // Drain events until a UserMessage with "first turn" lands. The
    // daemon also emits `SessionStarted` at boot; we want to skip past it
    // to the one we put on the wire, and capture the last_seq the
    // webview would later resume from.
    let mut last_seq: u64;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if tokio::time::Instant::now() >= deadline {
            panic!("did not observe UserMessage(first turn) before deadline");
        }
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(payload)) => {
                last_seq = payload.seq;
                if let Event::UserMessage { ref text, .. } = payload.event {
                    if &**text == "first turn" {
                        break;
                    }
                }
            }
            _ => panic!("event channel closed before UserMessage(first turn)"),
        }
    }
    assert!(last_seq >= 1, "expected at least one event with seq >= 1");

    // -- Phase 2: SIGKILL the daemon. No archive arm runs — pid file +
    // socket are stale on disk; `session_restart` is the cleanup path.
    let killed_pid = first_pid;
    // SAFETY: libc::kill against a child we just spawned; signal 9 is
    // valid.
    let rc = unsafe { libc::kill(killed_pid as libc::pid_t, libc::SIGKILL) };
    assert_eq!(rc, 0, "SIGKILL must deliver");
    let _ = child.wait().await;

    // The on-disk artifacts may linger briefly while the kernel reaps —
    // `spawn_forged_session_with_id` best-effort unlinks them.

    // -- Phase 3: invoke `run_session_restart` (the body the Tauri
    // command calls). It drops the dead bridge connection, spawns a new
    // `forged` with the SAME session id, and returns once the new
    // socket is up.
    let restart_out = run_session_restart(
        &bridge,
        session_id,
        workspace.to_str().unwrap(),
        None,
        Some("mock"),
    )
    .await
    .expect("session_restart");
    assert_eq!(restart_out.session_id, session_id);

    // F-748 review fix (Q5): the new daemon must be a DIFFERENT process
    // than the killed one — proves we actually re-spawned rather than
    // stitching the test back onto a stale handle. Read the post-restart
    // pid file via the canonical parser so a future format change still
    // exercises the assertion.
    let post_restart_raw = std::fs::read_to_string(&pid_path)
        .expect("post-restart pid file must exist after run_session_restart returns");
    let (post_restart_pid, _start_time) =
        forge_cli::socket::parse_pid_file_record(&post_restart_raw)
            .expect("post-restart pid file must parse");
    assert_ne!(
        post_restart_pid as u32, killed_pid,
        "session_restart must spawn a fresh process, not reuse the killed pid",
    );

    // -- Phase 4: re-hello and re-subscribe with `since: last_seq`. The
    // daemon's history-replay path uses `read_since(log_path, since)` —
    // events with seq <= last_seq must NOT replay (no duplicates), and
    // emit calls inside the resumed daemon must continue past last_seq.
    let (tx2, mut rx2) = mpsc::unbounded_channel();
    let _ack2 = bridge
        .hello(session_id, Some(&sock_path))
        .await
        .expect("hello #2");
    bridge
        .subscribe(session_id, last_seq, Arc::new(ChannelSink { tx: tx2 }))
        .await
        .expect("subscribe #2");

    // Drain whatever replay arrives within a bounded window. The exact
    // shape depends on what events the daemon emits at startup, but the
    // invariant we care about is: NO event with seq <= last_seq is
    // delivered.
    let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while tokio::time::Instant::now() < drain_deadline {
        let remaining = drain_deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, rx2.recv()).await {
            Ok(Some(payload)) => {
                assert!(
                    payload.seq > last_seq,
                    "duplicate event after restart: seq {} replayed even though since={}",
                    payload.seq,
                    last_seq,
                );
            }
            _ => break,
        }
    }

    // Independent verification that the log was actually preserved: a
    // brand-new `Subscribe { since: 0 }` against the live socket must
    // replay every event with seq <= last_seq. We can't open a third
    // connection through the same `SessionBridge` (it's keyed per
    // session id), so re-read the persisted log directly.
    let log_path = workspace
        .join(".forge")
        .join("sessions")
        .join(session_id)
        .join("events.jsonl");
    let preserved = forge_core::read_since(&log_path, 0)
        .await
        .expect("read events.jsonl");
    let preserved_first_turn = preserved.iter().any(|(_seq, ev)| {
        matches!(
            ev,
            Event::UserMessage { text, .. } if &**text == "first turn"
        )
    });
    assert!(
        preserved_first_turn,
        "pre-crash UserMessage must survive in the persisted event log",
    );

    // Cleanup: shutdown the resumed daemon politely so the test process
    // doesn't leak.
    let _ = bridge.shutdown(session_id).await;
}
