#![allow(deprecated)] // F-652: tests/benches still drive the deprecated bare read_frame helpers.
//! Integration test: full headless turn via UDS.
//!
//! Spawns `forged`, connects via Unix domain socket, sends a `SendUserMessage`,
//! and asserts the complete event sequence including a tool call auto-approved
//! by the server.

use forge_core::{ApprovalSource, Event};
use forge_ipc::{
    ClientInfo, Hello, IpcEvent, IpcMessage, SendUserMessage, Subscribe, PROTO_VERSION,
};
use std::process::Stdio;
use tempfile::TempDir;
use tokio::net::UnixStream;

const FORGED: &str = env!("CARGO_BIN_EXE_forged");

async fn connect_with_retry(path: &std::path::Path) -> UnixStream {
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(path).await {
            return s;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
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

/// Spawn `forged`, perform a full turn with a tool call, assert the event sequence.
///
/// Expected sequence (with `--auto-approve-unsafe`):
///   UserMessage
///   AssistantMessage(open)
///   AssistantDelta("I will read the file.")
///   ToolCallStarted { tool: "fs.read" }
///   ToolCallApproved { by: Auto }
///   ToolCallCompleted
///   AssistantMessage(final)
///   AssistantMessage(open)    ← continuation turn
///   AssistantDelta("Done reading.")
///   AssistantMessage(final)
#[tokio::test]
async fn full_headless_turn_emits_correct_event_sequence() {
    let dir = TempDir::new().unwrap();

    // File for fs.read to read during the turn
    let readable_file = dir.path().join("readable.txt");
    std::fs::write(&readable_file, "hello world\n").unwrap();

    // Build two NDJSON scripts: initial turn with tool call, then continuation
    let script1 = format!(
        "{}\n{}\n{}",
        serde_json::json!({"delta": "I will read the file."}),
        serde_json::json!({"tool_call": {"name": "fs.read", "args": {"path": readable_file.to_str().unwrap()}}}),
        serde_json::json!({"done": "tool_use"}),
    );
    let script2 = format!(
        "{}\n{}",
        serde_json::json!({"delta": "Done reading."}),
        serde_json::json!({"done": "end_turn"}),
    );

    let mock_path = dir.path().join("mock.json");
    std::fs::write(
        &mock_path,
        serde_json::to_string(&vec![script1, script2]).unwrap(),
    )
    .unwrap();

    let sock_path = dir.path().join("session.sock");

    let mut child = tokio::process::Command::new(FORGED)
        .arg("--auto-approve-unsafe")
        .env("FORGE_SESSION_ID", "headless-test-001")
        .env("FORGE_SOCKET_PATH", &sock_path)
        .env("FORGE_MOCK_SEQUENCE_FILE", &mock_path)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn forged");

    let mut stream = connect_with_retry(&sock_path).await;

    // Handshake
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

    // Subscribe from the beginning
    forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();

    // Send user message to start the turn
    forge_ipc::write_frame(
        &mut stream,
        &IpcMessage::SendUserMessage(SendUserMessage {
            text: "please read the file".to_string(),
        }),
    )
    .await
    .unwrap();

    // Collect events until two AssistantMessage(final) events arrive
    let mut events: Vec<Event> = Vec::new();
    let mut final_count = 0;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);

    loop {
        let frame = tokio::time::timeout_at(deadline, forge_ipc::read_frame(&mut stream))
            .await
            .expect("timed out waiting for events from forged")
            .unwrap();

        let Some(event) = extract_event(&frame) else {
            continue;
        };

        if matches!(
            event,
            Event::AssistantMessage {
                stream_finalised: true,
                ..
            }
        ) {
            final_count += 1;
        }
        events.push(event);
        if final_count >= 2 {
            break;
        }
    }

    let _ = child.kill().await;
    let _ = child.wait().await;

    // --- Assertions ---

    // F-139: step-trace events (`StepStarted`/`StepFinished`/`ToolInvoked`/
    // `ToolReturned`) are emitted in addition to the user-visible message /
    // tool-call sequence this test pins. They're covered by
    // `tests/step_events.rs`; filter them out here so the existing
    // backward-compatibility assertion stays tight on the original
    // sequence.
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::UserMessage { .. } => Some("UserMessage"),
            Event::AssistantMessage {
                stream_finalised: false,
                ..
            } => Some("AssistantMessage(open)"),
            Event::AssistantMessage {
                stream_finalised: true,
                ..
            } => Some("AssistantMessage(final)"),
            Event::AssistantDelta { .. } => Some("AssistantDelta"),
            Event::ToolCallStarted { .. } => Some("ToolCallStarted"),
            Event::ToolCallApprovalRequested { .. } => Some("ToolCallApprovalRequested"),
            Event::ToolCallApproved { .. } => Some("ToolCallApproved"),
            Event::ToolCallCompleted { .. } => Some("ToolCallCompleted"),
            Event::StepStarted { .. }
            | Event::StepFinished { .. }
            | Event::ToolInvoked { .. }
            | Event::ToolReturned { .. }
            | Event::LogLine { .. } => None,
            _ => Some("Other"),
        })
        .collect();

    assert_eq!(
        kinds,
        vec![
            "UserMessage",
            "AssistantMessage(open)",
            "AssistantDelta",
            "ToolCallStarted",
            "ToolCallApproved",
            "ToolCallCompleted",
            "AssistantMessage(final)",
            "AssistantMessage(open)",
            "AssistantDelta",
            "AssistantMessage(final)",
        ],
        "event sequence mismatch: got {kinds:?}"
    );

    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|e| {
            if let Event::AssistantDelta { delta, .. } = e {
                // F-112: `delta: Arc<str>` — `&*delta` is `&str` via Deref.
                Some(&**delta)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        deltas,
        vec!["I will read the file.", "Done reading."],
        "delta text mismatch"
    );

    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ToolCallStarted { tool, .. } if tool == "fs.read")),
        "expected ToolCallStarted for fs.read"
    );

    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::ToolCallApproved {
                by: ApprovalSource::Auto,
                ..
            }
        )),
        "expected ToolCallApproved with by=Auto"
    );
}
