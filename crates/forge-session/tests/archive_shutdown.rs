//! Integration test: orchestrator shutdown wires `archive_or_purge`.
//!
//! Spawns `forged --ephemeral` with an explicit workspace, drives a single
//! turn, waits for `SessionEnded`, and asserts the session directory and
//! socket are removed.
//!
//! Also covers F-039: a persistent (non-ephemeral) `forged` receiving
//! SIGTERM must run the same archive path, moving the live session dir
//! under `sessions/archived/` and rewriting `meta.toml`.

use forge_core::Event;
use forge_ipc::{
    ClientInfo, Hello, IpcEvent, IpcMessage, SendUserMessage, Subscribe, PROTO_VERSION,
};
use std::process::Stdio;
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::UnixStream;

const FORGED: &str = env!("CARGO_BIN_EXE_forged");

async fn connect_with_retry(path: &std::path::Path) -> UnixStream {
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(path).await {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    UnixStream::connect(path)
        .await
        .expect("forged did not create socket in time")
}

fn extract_event(msg: &IpcMessage) -> Option<Event> {
    // F-112: IpcEvent.event is typed.
    if let IpcMessage::Event(IpcEvent { event, .. }) = msg {
        Some(event.clone())
    } else {
        None
    }
}

#[tokio::test]
async fn ephemeral_shutdown_removes_session_dir_and_socket() {
    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let script = format!(
        "{}\n{}",
        serde_json::json!({"delta": "Hello."}),
        serde_json::json!({"done": "end_turn"}),
    );
    let mock_path = dir.path().join("mock.json");
    std::fs::write(&mock_path, serde_json::to_string(&vec![script]).unwrap()).unwrap();

    let session_id = "archive-shutdown-test";
    let sock_path = dir.path().join("session.sock");
    let session_dir = workspace.join(".forge").join("sessions").join(session_id);

    let mut child = tokio::process::Command::new(FORGED)
        .arg("--ephemeral")
        .env("FORGE_SESSION_ID", session_id)
        .env("FORGE_SOCKET_PATH", &sock_path)
        .env("FORGE_WORKSPACE", &workspace)
        .env("FORGE_MOCK_SEQUENCE_FILE", &mock_path)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn forged");

    let mut stream = connect_with_retry(&sock_path).await;

    forge_ipc::write_frame(
        &mut stream,
        &IpcMessage::Hello(Hello {
            proto: PROTO_VERSION,
            client: ClientInfo {
                kind: "test".into(),
                pid: std::process::id(),
                user: "tester".into(),
            },
        }),
    )
    .await
    .unwrap();

    let _ack: IpcMessage = forge_ipc::read_frame(&mut stream).await.unwrap();

    forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();

    forge_ipc::write_frame(
        &mut stream,
        &IpcMessage::SendUserMessage(SendUserMessage {
            text: "hi".to_string(),
        }),
    )
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let frame = tokio::time::timeout_at(deadline, forge_ipc::read_frame(&mut stream))
            .await
            .expect("timed out waiting for SessionEnded");
        match frame {
            Ok(msg) => {
                if let Some(event) = extract_event(&msg) {
                    if matches!(event, Event::SessionEnded { .. }) {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("forged did not exit after SessionEnded")
        .expect("child.wait() failed");
    assert!(
        status.success(),
        "forged exited non-zero: {:?}",
        status.code()
    );

    assert!(
        !session_dir.exists(),
        "ephemeral session dir must be purged on shutdown: {}",
        session_dir.display()
    );
    assert!(
        !sock_path.exists(),
        "socket must be removed on shutdown: {}",
        sock_path.display()
    );
}

#[tokio::test]
async fn persistent_sigterm_archives_session_dir_and_meta() {
    // F-039: a persistent forged on SIGTERM must (a) exit cleanly with code 0,
    // (b) move the live session dir to sessions/archived/<id>/ preserving the
    // events.jsonl schema header, (c) rewrite meta.toml with state="archived",
    // and (d) remove the socket file.
    //
    // NOTE: this test does not exercise the `child_registry.kill_all()` step
    // of the shutdown path — no SendUserMessage is dispatched, so no tool
    // subprocess is registered. That step is verified structurally (the call
    // is one line above the archive call in server.rs) but not behaviorally.
    // Adding behavioral coverage requires routing a real subprocess through
    // run_turn → tool_dispatcher → SandboxedCommand and is left as future work.

    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let session_id = "sigterm-archive-test";
    let sock_path = dir.path().join("session.sock");
    let session_dir = workspace.join(".forge").join("sessions").join(session_id);
    let archived_dir = workspace
        .join(".forge")
        .join("sessions")
        .join("archived")
        .join(session_id);

    let mut child = tokio::process::Command::new(FORGED)
        // No --ephemeral flag: persistent mode.
        .env("FORGE_SESSION_ID", session_id)
        .env("FORGE_SOCKET_PATH", &sock_path)
        .env("FORGE_WORKSPACE", &workspace)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn forged");

    // Hello+Subscribe, then immediately drop the stream — this proves the
    // server is up and serving before we signal it.
    let mut stream = connect_with_retry(&sock_path).await;
    forge_ipc::write_frame(
        &mut stream,
        &IpcMessage::Hello(Hello {
            proto: PROTO_VERSION,
            client: ClientInfo {
                kind: "test".into(),
                pid: std::process::id(),
                user: "tester".into(),
            },
        }),
    )
    .await
    .unwrap();
    let _ack: IpcMessage = forge_ipc::read_frame(&mut stream).await.unwrap();
    forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();
    drop(stream);

    // SIGTERM the daemon.
    let pid = child.id().expect("child pid available") as libc::pid_t;
    // SAFETY: pid is the live child we just spawned; SIGTERM is a valid signal.
    unsafe {
        let rc = libc::kill(pid, libc::SIGTERM);
        assert_eq!(
            rc,
            0,
            "kill(SIGTERM) failed: {}",
            std::io::Error::last_os_error()
        );
    }

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("forged did not exit after SIGTERM")
        .expect("child.wait() failed");
    assert!(
        status.success(),
        "forged exited non-zero after clean SIGTERM: {:?}",
        status.code()
    );

    // Live dir gone.
    assert!(
        !session_dir.exists(),
        "persistent session dir must be moved on archive: {}",
        session_dir.display()
    );

    // Archived events.jsonl exists and starts with the schema header line.
    let archived_log = archived_dir.join("events.jsonl");
    assert!(
        archived_log.exists(),
        "archived events.jsonl missing: {}",
        archived_log.display()
    );
    let log_contents = std::fs::read_to_string(&archived_log).expect("read archived log");
    let first_line = log_contents.lines().next().unwrap_or("");
    assert_eq!(
        first_line, r#"{"schema_version":1}"#,
        "archived events.jsonl must preserve the schema_version header"
    );

    // meta.toml exists and reports state = "Archived".
    let archived_meta = archived_dir.join("meta.toml");
    assert!(
        archived_meta.exists(),
        "archived meta.toml missing: {}",
        archived_meta.display()
    );
    let meta_contents = std::fs::read_to_string(&archived_meta).expect("read archived meta");
    assert!(
        meta_contents.contains(r#"state = "Archived""#),
        "archived meta.toml must have state = \"Archived\"; got:\n{meta_contents}"
    );

    // Socket removed.
    assert!(
        !sock_path.exists(),
        "socket must be removed on shutdown: {}",
        sock_path.display()
    );
}
