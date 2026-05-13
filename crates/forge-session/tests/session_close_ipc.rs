#![allow(deprecated)] // F-652: tests/benches still drive the deprecated bare read_frame helpers.
//! F-747: integration test for the daemon side of graceful shutdown.
//!
//! Spawns a persistent `forged` daemon, connects, subscribes, sends
//! `IpcMessage::Shutdown`, and asserts:
//!   1. the daemon emits `Event::SessionEnded { reason: Closed, archived: true }`,
//!   2. the daemon exits with status 0,
//!   3. the UDS socket file is removed,
//!   4. the pid file is removed.
//!
//! This is the persistent-mode counterpart to `archive_shutdown.rs`. The
//! window-close → IPC orchestration (timeout + SIGTERM/SIGKILL escalation)
//! is tested separately in `crates/forge-shell/tests/ipc_session_close.rs`
//! against a pure-function harness.

use forge_core::{EndReason, Event};
use forge_ipc::{
    ClientInfo, Hello, IpcEvent, IpcMessage, Shutdown, Subscribe, PROTO_VERSION, SCHEMA_VERSION,
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
    if let IpcMessage::Event(IpcEvent { event, .. }) = msg {
        Some(event.clone())
    } else {
        None
    }
}

/// F-747 happy path: a persistent `forged` that receives `IpcMessage::Shutdown`
/// must (a) emit `SessionEnded { reason: Closed, archived: true }`, (b) exit
/// with status 0, (c) remove the UDS socket, (d) remove the pid file.
#[tokio::test]
async fn shutdown_ipc_emits_session_ended_closed_and_reaps_artifacts() {
    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let session_id = "shutdown-ipc-test";
    let sock_path = dir.path().join("session.sock");
    let pid_path = dir.path().join("session.pid");
    let session_dir = workspace.join(".forge").join("sessions").join(session_id);
    let archived_dir = workspace
        .join(".forge")
        .join("sessions")
        .join("archived")
        .join(session_id);

    let mut child = tokio::process::Command::new(FORGED)
        .env("FORGE_SESSION_ID", session_id)
        .env("FORGE_SOCKET_PATH", &sock_path)
        .env("FORGE_PID_FILE", &pid_path)
        .env("FORGE_WORKSPACE", &workspace)
        // F-743: explicit provider spec required since the production path
        // no longer falls back to MockProvider.
        .env("FORGE_PROVIDER", "mock")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn forged");

    let mut stream = connect_with_retry(&sock_path).await;

    // Pid file must exist while daemon is running.
    assert!(
        pid_path.exists(),
        "pid file should exist while forged is running: {}",
        pid_path.display()
    );

    forge_ipc::write_frame(
        &mut stream,
        &IpcMessage::Hello(Hello {
            proto: PROTO_VERSION,
            client: ClientInfo {
                kind: "test".into(),
                pid: std::process::id(),
                user: "tester".into(),
            },
            schema_version: SCHEMA_VERSION,
        }),
    )
    .await
    .unwrap();

    let _ack: IpcMessage = forge_ipc::read_frame(&mut stream).await.unwrap();

    forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();

    forge_ipc::write_frame(&mut stream, &IpcMessage::Shutdown(Shutdown::default()))
        .await
        .unwrap();

    // Drain frames until we see the SessionEnded.
    let mut saw_closed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let frame = tokio::time::timeout_at(deadline, forge_ipc::read_frame(&mut stream)).await;
        match frame {
            Ok(Ok(msg)) => {
                if let Some(Event::SessionEnded {
                    reason, archived, ..
                }) = extract_event(&msg)
                {
                    assert!(
                        matches!(reason, EndReason::Closed),
                        "expected SessionEnded with reason: Closed, got {reason:?}",
                    );
                    assert!(
                        archived,
                        "persistent-mode shutdown must report archived: true",
                    );
                    saw_closed = true;
                    break;
                }
            }
            Ok(Err(_)) => break, // EOF — daemon closed the connection
            Err(_) => panic!("timed out waiting for SessionEnded after Shutdown IPC"),
        }
    }
    assert!(
        saw_closed,
        "did not observe SessionEnded {{ reason: Closed }}"
    );

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("forged did not exit after Shutdown IPC")
        .expect("child.wait() failed");
    assert!(
        status.success(),
        "forged exited non-zero after Shutdown: {:?}",
        status.code()
    );

    // Live dir gone (moved to archived/).
    assert!(
        !session_dir.exists(),
        "live session dir must be archived on shutdown: {}",
        session_dir.display()
    );

    // Archived events.jsonl exists.
    let archived_log = archived_dir.join("events.jsonl");
    assert!(
        archived_log.exists(),
        "archived events.jsonl missing: {}",
        archived_log.display()
    );

    // Socket removed.
    assert!(
        !sock_path.exists(),
        "UDS socket must be removed after Shutdown: {}",
        sock_path.display()
    );

    // Pid file removed by OwnedPidFile::drop on clean exit.
    assert!(
        !pid_path.exists(),
        "pid file must be removed after Shutdown: {}",
        pid_path.display()
    );
}
