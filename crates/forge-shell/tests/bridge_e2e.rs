//! End-to-end bridge tests: spawn a real `forge-session` daemon, drive it
//! through [`SessionBridge`], and assert the full command + event flow.
//!
//! These tests satisfy the integration requirement in F-020's DoD: the
//! webview → Tauri command → UDS → daemon path is exercised with the real
//! session server. A capturing [`EventSink`] stands in for the Tauri
//! `AppHandle::emit` call; the Tauri command wrappers in [`forge_shell::ipc`]
//! delegate to the same bridge code tested here.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use forge_core::{ApprovalScope, Event};
use forge_providers::MockProvider;
use forge_session::server::serve_with_session;
use forge_session::session::Session;
use forge_shell::bridge::{EventSink, SessionBridge, SessionConnections, SessionEventPayload};
use tempfile::TempDir;
use tokio::sync::mpsc;

/// Test sink that forwards every emitted payload through an mpsc channel so
/// the test can await delivery with a timeout.
struct ChannelSink {
    tx: mpsc::UnboundedSender<SessionEventPayload>,
}

impl EventSink for ChannelSink {
    fn emit(&self, payload: SessionEventPayload) {
        let _ = self.tx.send(payload);
    }
}

async fn spawn_daemon(path: &Path, session_id: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(MockProvider::with_default_path());
    let sock = path.to_path_buf();
    let sid = session_id.to_string();
    tokio::spawn(async move {
        serve_with_session(
            &sock,
            session,
            provider,
            true,
            false,
            None,
            Some(sid),
            None, // F-587: keyless test wiring
            None, // F-601: no active agent — memory off in this test
            None,
        )
        .await
        .unwrap();
    });
    // Wait for the server to bind its socket.
    for _ in 0..50 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    dir
}

#[tokio::test]
async fn hello_handshake_against_real_daemon() {
    let sock_dir = TempDir::new().unwrap();
    let sock = sock_dir.path().join("hello.sock");
    let _daemon = spawn_daemon(&sock, "fixed-id").await;

    let bridge = SessionBridge::new(SessionConnections::new());
    let ack = bridge
        .hello("fixed-id", Some(&sock))
        .await
        .expect("hello succeeds");

    assert_eq!(ack.session_id, "fixed-id");
    assert_eq!(ack.schema_version, 1);
    assert_eq!(bridge.connections().len().await, 1);
    assert!(bridge.connections().contains("fixed-id").await);
}

#[tokio::test]
async fn send_message_forwards_events_end_to_end() {
    let sock_dir = TempDir::new().unwrap();
    let sock = sock_dir.path().join("send.sock");
    let _daemon = spawn_daemon(&sock, "send-session").await;

    let bridge = SessionBridge::new(SessionConnections::new());
    bridge
        .hello("send-session", Some(&sock))
        .await
        .expect("hello");

    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = Arc::new(ChannelSink { tx });
    bridge
        .subscribe("send-session", 0, sink)
        .await
        .expect("subscribe");

    bridge
        .send_message("send-session", "hello world".to_string())
        .await
        .expect("send_message");

    // Drain events until we see a UserMessageAdded whose text matches.
    let mut saw_user_message = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(payload)) => {
                assert_eq!(payload.session_id, "send-session");
                // F-112: `payload.event` is typed `Event` — no `from_value` needed.
                if let Event::UserMessage { text, .. } = payload.event {
                    // `Arc<str>` derefs to `str`; explicit `&*` gives a `&str`
                    // that matches the `&str` literal for `PartialEq`.
                    assert_eq!(&*text, "hello world");
                    saw_user_message = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(saw_user_message, "expected UserMessageAdded event");
}

#[tokio::test]
async fn approve_tool_proxies_to_daemon() {
    // Without a tool call in flight, approve is a no-op on the daemon side
    // (the pending-approvals map has no entry). We assert the frame is
    // written without error — i.e. the bridge routes the command through
    // the same writer it uses for send_message.
    let sock_dir = TempDir::new().unwrap();
    let sock = sock_dir.path().join("approve.sock");
    let _daemon = spawn_daemon(&sock, "approve-session").await;

    let bridge = SessionBridge::new(SessionConnections::new());
    bridge
        .hello("approve-session", Some(&sock))
        .await
        .expect("hello");

    let (tx, _rx) = mpsc::unbounded_channel();
    bridge
        .subscribe("approve-session", 0, Arc::new(ChannelSink { tx }))
        .await
        .expect("subscribe");

    bridge
        .approve_tool("approve-session", "call-123".into(), ApprovalScope::Once)
        .await
        .expect("approve_tool writes frame");

    bridge
        .reject_tool("approve-session", "call-456".into(), Some("nope".into()))
        .await
        .expect("reject_tool writes frame");
}

/// F-109: `subscribe()` must not hold the `SessionConnections` map lock
/// across its `write_frame().await`. If it does, a concurrent command on a
/// *different* session blocks behind that lock even though its own writer
/// is unrelated. This test stalls session A's `write_frame` inside
/// `subscribe()` by externally holding A's writer mutex, then asserts that
/// `send_message("B", …)` still completes within a tight timeout.
///
/// - Without the fix: `subscribe(A)` holds the map lock while awaiting
///   `writer_A.lock()`. `send_message(B)` calls `writer_for(B)`, which
///   tries to acquire the same map lock — deadlocks until A's writer is
///   released. The test times out.
/// - With the fix: `subscribe(A)` releases the map lock before awaiting
///   `writer_A.lock()`. `send_message(B)` grabs the map lock, captures B's
///   writer, writes the frame, and returns. The assertion holds.
///
/// The timeout (500 ms) is two orders of magnitude above the expected
/// send_message latency on a loopback UDS; any flake here almost certainly
/// indicates a genuine regression of the locking discipline.
#[tokio::test]
async fn subscribe_does_not_block_concurrent_send_on_other_session() {
    let sock_dir = TempDir::new().unwrap();
    let sock_a = sock_dir.path().join("a.sock");
    let sock_b = sock_dir.path().join("b.sock");
    let _daemon_a = spawn_daemon(&sock_a, "session-a").await;
    let _daemon_b = spawn_daemon(&sock_b, "session-b").await;

    let bridge = SessionBridge::new(SessionConnections::new());
    bridge
        .hello("session-a", Some(&sock_a))
        .await
        .expect("hello a");
    bridge
        .hello("session-b", Some(&sock_b))
        .await
        .expect("hello b");

    // Externally hold session A's writer mutex. Any subsequent call that
    // tries to lock it will stall until we drop this guard.
    let writer_a = bridge
        .writer_arc_for_testing("session-a")
        .await
        .expect("writer for a");
    let writer_a_guard = writer_a.lock().await;

    // Fire subscribe(A) in a background task. Its `write_frame` will hang
    // waiting for the writer mutex we hold. In the buggy version it would
    // also be holding the `SessionConnections` map lock.
    let bridge_clone = bridge.clone();
    let (tx_a, _rx_a) = mpsc::unbounded_channel();
    let subscribe_task = tokio::spawn(async move {
        bridge_clone
            .subscribe("session-a", 0, Arc::new(ChannelSink { tx: tx_a }))
            .await
    });

    // Yield so the subscribe task actually reaches its stall point before
    // we race send_message against it. A brief sleep is sufficient; the
    // task only needs to pass through its initial lock-acquire/drop.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // This call must NOT block on session A's stall. Under the fix it
    // completes in < 1 ms on loopback; the 500 ms timeout is pure safety
    // margin. Under the bug it blocks until writer_a_guard is dropped.
    let result = tokio::time::timeout(
        Duration::from_millis(500),
        bridge.send_message("session-b", "ping".to_string()),
    )
    .await;

    // Drop our hold on writer A so the subscribe task can finish cleanly.
    drop(writer_a_guard);
    let _ = subscribe_task.await;

    let send_result = result.expect(
        "send_message(B) timed out while subscribe(A) was stalled: the \
         SessionConnections map lock is being held across an .await in \
         subscribe() (F-109 regression)",
    );
    send_result.expect("send_message(B) frame write");
}

#[tokio::test]
async fn hello_twice_for_same_session_is_rejected() {
    let sock_dir = TempDir::new().unwrap();
    let sock = sock_dir.path().join("dup.sock");
    let _daemon = spawn_daemon(&sock, "dup-session").await;

    let bridge = SessionBridge::new(SessionConnections::new());
    bridge
        .hello("dup-session", Some(&sock))
        .await
        .expect("first hello");
    let err = bridge
        .hello("dup-session", Some(&sock))
        .await
        .expect_err("second hello must be rejected");
    assert!(err.to_string().contains("already connected"));
}
