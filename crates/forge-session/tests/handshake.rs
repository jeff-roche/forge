#![allow(deprecated)] // F-652: tests/benches still drive the deprecated bare read_frame helpers.
use forge_ipc::{ClientInfo, Hello, IpcMessage, PROTO_VERSION, SCHEMA_VERSION};
use forge_providers::MockProvider;
use forge_session::server::{serve, serve_with_session};
use forge_session::session::Session;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

// F-678: bidirectional schema-version warn tests use the shared capture
// subscriber to assert the daemon-side `warn!` actually fires.
mod common;

/// F-354: The deadline tests mutate process-wide env vars
/// (`FORGE_IPC_HANDSHAKE_DEADLINE_MS` / `FORGE_IPC_IDLE_TIMEOUT_MS`) that
/// the daemon reads when a connection is handled. Cargo runs integration
/// tests in the same process on multiple threads by default, so without
/// serialization one test's value can leak into another's server task.
/// This mutex serializes every test that touches those env vars.
fn deadline_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| StdMutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

async fn connect_with_retry(path: &PathBuf) -> UnixStream {
    for _ in 0..20 {
        match UnixStream::connect(path).await {
            Ok(s) => return s,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
    UnixStream::connect(path)
        .await
        .expect("server did not start in time")
}

#[tokio::test]
async fn handshake_succeeds_with_valid_proto() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.sock");

    let server_path = path.clone();
    tokio::spawn(async move {
        serve(&server_path, false, false).await.unwrap();
    });

    let mut stream = connect_with_retry(&path).await;

    let hello = IpcMessage::Hello(Hello {
        proto: PROTO_VERSION,
        client: ClientInfo {
            kind: "shell".into(),
            pid: std::process::id(),
            user: "test-user".into(),
        },
        schema_version: forge_ipc::SCHEMA_VERSION,
    });
    forge_ipc::write_frame(&mut stream, &hello).await.unwrap();

    let response = forge_ipc::read_frame(&mut stream).await.unwrap();
    let IpcMessage::HelloAck(ack) = response else {
        panic!("expected HelloAck, got {:?}", response);
    };
    assert_eq!(ack.schema_version, 1);
    assert!(!ack.session_id.is_empty());
}

#[tokio::test]
async fn unknown_proto_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test2.sock");

    let server_path = path.clone();
    tokio::spawn(async move {
        serve(&server_path, false, false).await.unwrap();
    });

    let mut stream = connect_with_retry(&path).await;

    let hello = IpcMessage::Hello(Hello {
        proto: 999,
        client: ClientInfo {
            kind: "shell".into(),
            pid: std::process::id(),
            user: "test-user".into(),
        },
        schema_version: forge_ipc::SCHEMA_VERSION,
    });
    forge_ipc::write_frame(&mut stream, &hello).await.unwrap();

    let result: anyhow::Result<IpcMessage> = forge_ipc::read_frame(&mut stream).await;
    assert!(
        result.is_err(),
        "expected connection to be closed for unknown proto"
    );
}

async fn perform_handshake(stream: &mut UnixStream) -> forge_ipc::HelloAck {
    let hello = IpcMessage::Hello(Hello {
        proto: PROTO_VERSION,
        client: ClientInfo {
            kind: "shell".into(),
            pid: std::process::id(),
            user: "test-user".into(),
        },
        schema_version: forge_ipc::SCHEMA_VERSION,
    });
    forge_ipc::write_frame(stream, &hello).await.unwrap();
    let response = forge_ipc::read_frame(stream).await.unwrap();
    let IpcMessage::HelloAck(ack) = response else {
        panic!("expected HelloAck, got {:?}", response);
    };
    ack
}

/// Regression test for F-035: HelloAck.session_id must remain stable across
/// successive handshakes against the same daemon instead of being regenerated
/// per connection.
#[tokio::test]
async fn session_id_stable_across_handshakes() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("stable.sock");

    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(MockProvider::with_default_path());
    let server_sock = sock_path.clone();
    let pinned_id = "fixed-session-id".to_string();
    let server_id = pinned_id.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            session,
            provider,
            false,
            false,
            None,
            Some(server_id),
            None, // F-587: keyless test wiring
            None, // F-601: no active agent — memory off in this test
            None,
        )
        .await
        .unwrap();
    });

    let mut first = connect_with_retry(&sock_path).await;
    let ack1 = perform_handshake(&mut first).await;
    drop(first);

    let mut second = connect_with_retry(&sock_path).await;
    let ack2 = perform_handshake(&mut second).await;

    assert_eq!(ack1.session_id, pinned_id);
    assert_eq!(ack2.session_id, pinned_id);
}

/// Regression test for F-035: relative FORGE_WORKSPACE inputs must be
/// normalized to absolute paths before being reported in HelloAck.workspace,
/// so clients with a different CWD can still resolve the path.
#[tokio::test]
async fn workspace_reported_as_absolute_path() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("workspace.sock");

    // Mimic main.rs normalization for a relative input.
    let relative = PathBuf::from("relative/workspace");
    let absolute = std::path::absolute(&relative).unwrap();

    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(MockProvider::with_default_path());
    let server_sock = sock_path.clone();
    let server_workspace = absolute.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            session,
            provider,
            false,
            false,
            Some(server_workspace),
            None,
            None, // F-587: keyless test wiring
            None, // F-601: no active agent — memory off in this test
            None,
        )
        .await
        .unwrap();
    });

    let mut stream = connect_with_retry(&sock_path).await;
    let ack = perform_handshake(&mut stream).await;

    let reported = PathBuf::from(&ack.workspace);
    assert!(
        reported.is_absolute(),
        "workspace should be absolute, got {:?}",
        reported
    );
    assert_eq!(reported, absolute);
}

/// F-354: A peer that connects to the UDS and sends zero bytes must be
/// disconnected by the daemon within the handshake deadline. Without the
/// deadline, `read_u32` blocks forever and the connection task hangs.
///
/// Test strategy: set the handshake deadline to 200 ms via the
/// `FORGE_IPC_HANDSHAKE_DEADLINE_MS` env var, connect, send nothing, then
/// wait for the peer to read EOF. A well-behaved daemon drops the
/// connection within ~200 ms; a broken one never does.
// Hold std::sync::Mutex across awaits deliberately — the env var must
// stay unchanged for the whole server connection lifetime, so an async
// Mutex would just shift the same constraint. Clippy's concern (waker
// deadlocks) doesn't apply: the guard is uncontended apart from within
// this test file, and all callers are on the same current-thread runtime.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn handshake_deadline_disconnects_silent_peer() {
    let _guard = deadline_env_lock();
    // Scope env mutation tightly — the value is read once per connection
    // inside handle_connection, so as long as the server task is spawned
    // while the var is set, the deadline takes effect for this connection.
    std::env::set_var("FORGE_IPC_HANDSHAKE_DEADLINE_MS", "200");

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("silent.sock");

    let server_path = path.clone();
    tokio::spawn(async move {
        let _ = serve(&server_path, false, false).await;
    });

    let mut stream = connect_with_retry(&path).await;

    // Wait for the server-side read to hit its deadline and drop the
    // connection. A peer read of zero bytes after EOF is the signal.
    let started = std::time::Instant::now();
    let mut buf = [0u8; 1];
    let read_result =
        tokio::time::timeout(std::time::Duration::from_secs(3), stream.read(&mut buf)).await;
    let elapsed = started.elapsed();

    std::env::remove_var("FORGE_IPC_HANDSHAKE_DEADLINE_MS");

    let n = read_result
        .expect("daemon did not disconnect silent peer within 3s")
        .expect("read should complete with EOF or error");
    assert_eq!(n, 0, "expected EOF (0 bytes), got {n} bytes");
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "disconnect took too long: {elapsed:?}",
    );
}

/// F-354: A peer that completes Hello but never sends Subscribe must be
/// disconnected within the handshake deadline. Verifies the deadline is
/// applied to the second read, not only the first.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn handshake_deadline_disconnects_half_handshaked_peer() {
    let _guard = deadline_env_lock();
    std::env::set_var("FORGE_IPC_HANDSHAKE_DEADLINE_MS", "200");

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("halfway.sock");

    let server_path = path.clone();
    tokio::spawn(async move {
        let _ = serve(&server_path, false, false).await;
    });

    let mut stream = connect_with_retry(&path).await;

    // Complete Hello → HelloAck but stop before sending Subscribe.
    let hello = IpcMessage::Hello(Hello {
        proto: PROTO_VERSION,
        client: ClientInfo {
            kind: "shell".into(),
            pid: std::process::id(),
            user: "test-user".into(),
        },
        schema_version: forge_ipc::SCHEMA_VERSION,
    });
    forge_ipc::write_frame(&mut stream, &hello).await.unwrap();
    let _ack: IpcMessage = forge_ipc::read_frame(&mut stream).await.unwrap();

    // Now stall. The daemon should close us within the Subscribe deadline.
    let started = std::time::Instant::now();
    let mut buf = [0u8; 1];
    let read_result =
        tokio::time::timeout(std::time::Duration::from_secs(3), stream.read(&mut buf)).await;
    let elapsed = started.elapsed();

    std::env::remove_var("FORGE_IPC_HANDSHAKE_DEADLINE_MS");

    let n = read_result
        .expect("daemon did not disconnect half-handshaked peer within 3s")
        .expect("read should complete with EOF or error");
    assert_eq!(n, 0, "expected EOF after Subscribe deadline, got {n} bytes");
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "disconnect took too long: {elapsed:?}",
    );
}

/// F-354: A peer that completes the full handshake (Hello + Subscribe) but
/// then stalls on the post-handshake command reader must be disconnected
/// within the idle timeout. This closes the inter-frame starvation gap
/// identified in the finding.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn post_handshake_idle_timeout_disconnects_peer() {
    let _guard = deadline_env_lock();
    std::env::set_var("FORGE_IPC_HANDSHAKE_DEADLINE_MS", "2000");
    std::env::set_var("FORGE_IPC_IDLE_TIMEOUT_MS", "300");

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("idle.sock");

    let server_path = path.clone();
    tokio::spawn(async move {
        let _ = serve(&server_path, false, false).await;
    });

    let mut stream = connect_with_retry(&path).await;

    // Full handshake.
    let hello = IpcMessage::Hello(Hello {
        proto: PROTO_VERSION,
        client: ClientInfo {
            kind: "shell".into(),
            pid: std::process::id(),
            user: "test-user".into(),
        },
        schema_version: forge_ipc::SCHEMA_VERSION,
    });
    forge_ipc::write_frame(&mut stream, &hello).await.unwrap();
    let _ack: IpcMessage = forge_ipc::read_frame(&mut stream).await.unwrap();

    let subscribe = IpcMessage::Subscribe(forge_ipc::Subscribe { since: 0 });
    forge_ipc::write_frame(&mut stream, &subscribe)
        .await
        .unwrap();

    // Handshake complete. Now stall on the command reader. The daemon's
    // post-handshake idle timeout should drop us.
    //
    // The idle-timeout watchdog only breaks the server's command-reader
    // task; the outer select loop may keep the write-half alive until a
    // live event arrives. Detecting from the client side: a write to the
    // peer will eventually surface a broken-pipe error after the reader
    // has dropped and the connection is closed.
    //
    // To keep the test deterministic, we simply assert that *some* signal
    // of disconnection arrives within a bounded window by probing with a
    // Ping-style frame and polling for a send failure.
    let started = std::time::Instant::now();
    let mut disconnected = false;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // Write a byte; when the peer is closed on both halves this
        // eventually fails.
        if stream.write_all(b"\x00").await.is_err() {
            disconnected = true;
            break;
        }
        // Try to read — a clean server-side close surfaces as EOF.
        let mut buf = [0u8; 1];
        match tokio::time::timeout(std::time::Duration::from_millis(10), stream.read(&mut buf))
            .await
        {
            Ok(Ok(0)) => {
                disconnected = true;
                break;
            }
            Ok(Err(_)) => {
                disconnected = true;
                break;
            }
            _ => {}
        }
    }
    let elapsed = started.elapsed();

    std::env::remove_var("FORGE_IPC_HANDSHAKE_DEADLINE_MS");
    std::env::remove_var("FORGE_IPC_IDLE_TIMEOUT_MS");

    assert!(
        disconnected,
        "daemon did not enforce post-handshake idle timeout within {:?}",
        elapsed
    );
}

#[tokio::test]
async fn garbage_frame_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test3.sock");

    let server_path = path.clone();
    tokio::spawn(async move {
        serve(&server_path, false, false).await.unwrap();
    });

    let mut stream = connect_with_retry(&path).await;

    // Send raw garbage bytes — not valid length-prefixed JSON
    use tokio::io::AsyncWriteExt;
    stream
        .write_all(b"\xff\xff\xff\xff{garbage}")
        .await
        .unwrap();

    let result: anyhow::Result<IpcMessage> = forge_ipc::read_frame(&mut stream).await;
    assert!(
        result.is_err(),
        "expected connection to be closed for garbage frame"
    );
}

/// F-678: a shell whose `Hello.schema_version` does not match the daemon's
/// `SCHEMA_VERSION` must complete the handshake (warn, do not reject) and
/// the daemon's `warn!` must fire so a version-skewed peer surfaces in
/// operator logs. This is the explicit DoD acceptance test for the
/// daemon↔shell leg.
// The capture-subscriber lock is a `std::sync::Mutex` held across awaits
// to serialize the `drain_capture()` invariant — the existing
// `bg_agents_tracing` tests use the same pattern. Clippy's concern
// (waker deadlocks) doesn't apply: the guard is uncontended apart from
// other capture-reading tests, which serialize through the same lock.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn handshake_with_mismatched_schema_version_warns_and_proceeds() {
    let _test_lock = common::capture_test_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    common::install_capture_subscriber();
    let _ = common::drain_capture(); // clear any prior emissions

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("schema-mismatch.sock");

    let server_path = path.clone();
    tokio::spawn(async move {
        serve(&server_path, false, false).await.unwrap();
    });

    let mut stream = connect_with_retry(&path).await;

    // Stamp a deliberately wrong schema_version. The daemon should warn
    // but still produce a HelloAck.
    let bogus_schema: u32 = SCHEMA_VERSION.wrapping_add(42);
    let hello = IpcMessage::Hello(Hello {
        proto: PROTO_VERSION,
        client: ClientInfo {
            kind: "shell".into(),
            pid: std::process::id(),
            user: "test-user".into(),
        },
        schema_version: bogus_schema,
    });
    forge_ipc::write_frame(&mut stream, &hello).await.unwrap();

    let response = forge_ipc::read_frame(&mut stream).await.unwrap();
    let IpcMessage::HelloAck(ack) = response else {
        panic!("expected HelloAck, got {response:?}");
    };
    assert_eq!(
        ack.schema_version, SCHEMA_VERSION,
        "daemon should still ack with its local SCHEMA_VERSION",
    );

    // Give the daemon a moment to flush the warn through the subscriber.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let logs = common::drain_capture();
    assert!(
        logs.contains("schema_version mismatch"),
        "expected mismatch warn in capture, got: {logs}",
    );
    assert!(
        logs.contains("peer=\"shell\""),
        "expected peer=shell label in capture, got: {logs}",
    );
    assert!(
        logs.contains(&format!("peer_schema_version={bogus_schema}")),
        "expected reported peer schema_version in capture, got: {logs}",
    );
}

/// F-678: a shell that omits `schema_version` entirely (legacy peer that
/// pre-dates the bidirectional check) deserializes via `#[serde(default)]`
/// to `0`, surfaces through the warn path, and the connection still
/// completes. Pins the wire-shape compatibility contract.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn handshake_with_legacy_hello_omitting_schema_version_warns_and_proceeds() {
    let _test_lock = common::capture_test_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    common::install_capture_subscriber();
    let _ = common::drain_capture();

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("legacy-hello.sock");

    let server_path = path.clone();
    tokio::spawn(async move {
        serve(&server_path, false, false).await.unwrap();
    });

    let mut stream = connect_with_retry(&path).await;

    // Hand-build a legacy `Hello` body with no `schema_version` key. We
    // can't use the typed `Hello { ... }` literal because the field is
    // mandatory in Rust; emit raw JSON to mimic an older shell.
    let body = serde_json::json!({
        "t": "Hello",
        "proto": PROTO_VERSION,
        "client": {
            "kind": "shell",
            "pid": std::process::id(),
            "user": "legacy",
        },
    });
    let bytes = serde_json::to_vec(&body).unwrap();
    let len = bytes.len() as u32;
    use tokio::io::AsyncWriteExt;
    stream.write_u32(len).await.unwrap();
    stream.write_all(&bytes).await.unwrap();

    let response = forge_ipc::read_frame(&mut stream).await.unwrap();
    let IpcMessage::HelloAck(ack) = response else {
        panic!("expected HelloAck for legacy peer, got {response:?}");
    };
    assert_eq!(ack.schema_version, SCHEMA_VERSION);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let logs = common::drain_capture();
    assert!(
        logs.contains("schema_version mismatch"),
        "legacy peer should still warn (default 0 vs local): {logs}",
    );
    assert!(
        logs.contains("peer_schema_version=0"),
        "legacy peer reports default 0: {logs}",
    );
}
