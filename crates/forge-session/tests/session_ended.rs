#![allow(deprecated)] // F-652: tests/benches still drive the deprecated bare read_frame helpers.
//! Integration tests: SessionEnded emission for ephemeral sessions.
//!
//! Verifies that forged emits `SessionEnded { reason: Completed }` after a
//! single-turn ephemeral session and that the process exits cleanly.

use forge_core::{EndReason, Event};
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
    // F-112: IpcEvent.event is now typed `Event` — no Value intermediate.
    if let IpcMessage::Event(IpcEvent { event, .. }) = msg {
        Some(event.clone())
    } else {
        None
    }
}

/// Ephemeral session: `SessionEnded { reason: Completed }` must be the last
/// event and forged must exit with code 0.
#[tokio::test]
async fn ephemeral_session_emits_session_ended_with_completed_reason() {
    let dir = TempDir::new().unwrap();

    let script = format!(
        "{}\n{}",
        serde_json::json!({"delta": "Hello."}),
        serde_json::json!({"done": "end_turn"}),
    );
    let mock_path = dir.path().join("mock.json");
    std::fs::write(&mock_path, serde_json::to_string(&vec![script]).unwrap()).unwrap();

    let sock_path = dir.path().join("session.sock");

    let mut child = tokio::process::Command::new(FORGED)
        .arg("--ephemeral")
        .env("FORGE_SESSION_ID", "ephemeral-ended-test")
        .env("FORGE_SOCKET_PATH", &sock_path)
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
            schema_version: forge_ipc::SCHEMA_VERSION,
        }),
    )
    .await
    .unwrap();

    let ack = forge_ipc::read_frame(&mut stream).await.unwrap();
    assert!(
        matches!(ack, IpcMessage::HelloAck(_)),
        "expected HelloAck, got {ack:?}"
    );

    forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();

    forge_ipc::write_frame(
        &mut stream,
        &IpcMessage::SendUserMessage(SendUserMessage {
            text: "say hello".to_string(),
        }),
    )
    .await
    .unwrap();

    // Collect all events until SessionEnded or timeout.
    let mut events: Vec<Event> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);

    loop {
        let frame = tokio::time::timeout_at(deadline, forge_ipc::read_frame(&mut stream))
            .await
            .expect("timed out waiting for SessionEnded from forged");

        match frame {
            Ok(msg) => {
                if let Some(event) = extract_event(&msg) {
                    let is_ended = matches!(event, Event::SessionEnded { .. });
                    events.push(event);
                    if is_ended {
                        break;
                    }
                }
            }
            // Connection closed without SessionEnded — test will fail on assert below.
            Err(_) => break,
        }
    }

    let last = events.last().expect("no events received");
    assert!(
        matches!(
            last,
            Event::SessionEnded {
                reason: EndReason::Completed,
                archived: false,
                ..
            }
        ),
        "last event was not SessionEnded{{Completed}}: {last:?}"
    );

    // forged must exit cleanly after the ephemeral session ends.
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("forged did not exit within 5s after SessionEnded")
        .expect("child.wait() failed");
    assert!(
        status.success(),
        "forged exited with non-zero: {:?}",
        status.code()
    );
}
