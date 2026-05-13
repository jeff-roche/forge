#![allow(deprecated)] // F-652: tests/benches still drive the deprecated bare read_frame helpers.
/// Integration test: Mock provider scripts an fs.read tool call against a real
/// temp file. Verifies ToolCallCompleted.result contains content, bytes, sha256
/// and that `fs.read` auto-approves under the read-only fast path (#647).
use forge_core::{ApprovalSource, Event};
use forge_ipc::{
    ClientInfo, Hello, IpcEvent, IpcMessage, SendUserMessage, Subscribe, PROTO_VERSION,
};
use forge_providers::MockProvider;
use forge_session::{server::serve_with_session, session::Session};
use std::io::Write;
use std::sync::Arc;
use tempfile::{NamedTempFile, TempDir};
use tokio::net::UnixStream;

async fn connect_with_retry(path: &std::path::PathBuf) -> UnixStream {
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

async fn do_handshake(stream: &mut UnixStream) {
    let hello = IpcMessage::Hello(Hello {
        proto: PROTO_VERSION,
        client: ClientInfo {
            kind: "test".into(),
            pid: std::process::id(),
            user: "tester".into(),
        },
        schema_version: forge_ipc::SCHEMA_VERSION,
    });
    forge_ipc::write_frame(stream, &hello).await.unwrap();
    let response = forge_ipc::read_frame(stream).await.unwrap();
    assert!(
        matches!(response, IpcMessage::HelloAck(_)),
        "expected HelloAck"
    );
}

fn extract_event(msg: &IpcMessage) -> Option<Event> {
    // F-112: IpcEvent.event is typed.
    if let IpcMessage::Event(IpcEvent { event, .. }) = msg {
        Some(event.clone())
    } else {
        None
    }
}

/// fs.read tool call through the full orchestrator stack reads a real file
/// and returns content, bytes, sha256 in ToolCallCompleted.result.
#[tokio::test]
async fn fs_read_tool_returns_content_bytes_sha256() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("fs_read_test.sock");

    // Create a real file to read
    let mut temp_file = NamedTempFile::new_in(dir.path()).unwrap();
    let file_content = "hello from fs.read";
    temp_file.write_all(file_content.as_bytes()).unwrap();
    let file_path = temp_file.path().to_str().unwrap().to_string();

    // Build a dynamic script so the tool call references the actual temp file path
    let script = format!(
        "{}\n{}\n",
        serde_json::json!({"tool_call": {"name": "fs.read", "args": {"path": file_path}}}),
        serde_json::json!({"done": "tool_use"}),
    );

    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(MockProvider::from_responses(vec![script]).unwrap());

    let server_session = Arc::clone(&session);
    let server_provider = Arc::clone(&provider);
    let server_sock = sock_path.clone();
    // F-043: `serve_with_session` now derives `allowed_paths` from the
    // workspace root, so the temp file must live inside the workspace it
    // was created in. Previously `allowed_paths` was `vec!["**"]`, which
    // matched every absolute path including the temp file path regardless
    // of workspace. Passing the temp dir as the workspace keeps this test
    // exercising the happy-path read through the orchestrator stack.
    let server_workspace = Some(dir.path().to_path_buf());
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            server_session,
            server_provider,
            false,
            false,
            server_workspace,
            None,
            None, // F-587: keyless test wiring
            None, // F-601: no active agent — memory off in this test
            None,
        )
        .await
        .unwrap();
    });

    let mut stream = connect_with_retry(&sock_path).await;
    do_handshake(&mut stream).await;

    forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();
    forge_ipc::write_frame(
        &mut stream,
        &IpcMessage::SendUserMessage(SendUserMessage {
            text: "read the file".to_string(),
        }),
    )
    .await
    .unwrap();

    let (mut reader, _writer) = stream.into_split();
    let mut tool_completed_result: Option<serde_json::Value> = None;
    let mut auto_approved = false;
    let mut saw_approval_requested = false;

    for _ in 0..20 {
        let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
        let Some(event) = extract_event(&frame) else {
            continue;
        };

        match event {
            // #647: `fs.read` is read-only — the orchestrator must NOT
            // emit an interactive approval prompt. We track and assert on
            // this below.
            Event::ToolCallApprovalRequested { .. } => saw_approval_requested = true,
            Event::ToolCallApproved {
                by: ApprovalSource::Auto,
                ..
            } => auto_approved = true,
            Event::ToolCallCompleted { result, .. } => {
                tool_completed_result = Some(result);
                break;
            }
            _ => {}
        }
    }

    assert!(
        !saw_approval_requested,
        "fs.read is read-only — the orchestrator must skip the interactive approval gate (#647)"
    );
    assert!(
        auto_approved,
        "fs.read must surface a ToolCallApproved {{ by: Auto }} so the event log records why no human approved"
    );
    let result = tool_completed_result.expect("ToolCallCompleted event not received");

    assert_eq!(
        result["content"].as_str().unwrap(),
        file_content,
        "content mismatch"
    );
    assert_eq!(
        result["bytes"].as_u64().unwrap(),
        file_content.len() as u64,
        "bytes mismatch"
    );

    let sha256 = result["sha256"].as_str().unwrap();
    assert_eq!(sha256.len(), 64, "sha256 should be 64 hex chars");
    assert!(
        sha256.chars().all(|c| c.is_ascii_hexdigit()),
        "sha256 should be hex"
    );
}
