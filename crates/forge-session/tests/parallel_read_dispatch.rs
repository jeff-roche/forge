#![allow(deprecated)] // F-652: tests/benches still drive the deprecated bare read_frame helpers.
//! Issue #647: end-to-end coverage that two real `fs.read` calls in one
//! turn dispatch through the parallel branch, without the orchestrator
//! emitting an interactive approval prompt for either call.
//!
//! The matching unit tests in `crates/forge-session/src/orchestrator.rs`
//! exercise the parallel-dispatch shape with a synthetic `SleepyTool`.
//! This integration test wires the real `FsReadTool` against the public
//! `serve_with_session` daemon so a regression in `FsReadTool::read_only`
//! (or in any of the tool-handling glue between dispatcher registration
//! and the orchestrator's grouping pass) surfaces here even if the
//! synthetic harness keeps passing.

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
    for _ in 0..50 {
        match UnixStream::connect(path).await {
            Ok(s) => return s,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
    UnixStream::connect(path)
        .await
        .expect("daemon did not create socket in time")
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
    if let IpcMessage::Event(IpcEvent { event, .. }) = msg {
        Some(event.clone())
    } else {
        None
    }
}

/// Two consecutive `fs.read` calls in one model turn:
///   * neither call surfaces `ToolCallApprovalRequested` (read-only auto-approve);
///   * both `ToolCallApproved` events carry `by: ApprovalSource::Auto`;
///   * both `ToolCallStarted` events share a single `Some(parallel_group)` id,
///     proving the orchestrator placed them on the parallel-dispatch branch.
#[tokio::test]
async fn two_real_fs_read_calls_dispatch_in_parallel_without_approval() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("parallel_read.sock");

    // Two distinct files inside the workspace so `allowed_paths` derived
    // from the workspace root admits both reads cleanly.
    let mut f_a = NamedTempFile::new_in(dir.path()).unwrap();
    f_a.write_all(b"alpha").unwrap();
    let mut f_b = NamedTempFile::new_in(dir.path()).unwrap();
    f_b.write_all(b"beta").unwrap();
    let path_a = f_a.path().to_str().unwrap().to_string();
    let path_b = f_b.path().to_str().unwrap().to_string();

    // One turn, two consecutive `fs.read` calls, then Done("tool_use") so
    // the orchestrator dispatches the batch and asks the provider for a
    // continuation. The continuation script just ends the turn.
    let initial = format!(
        "{}\n{}\n{}\n",
        serde_json::json!({"tool_call": {"name": "fs.read", "args": {"path": path_a}}}),
        serde_json::json!({"tool_call": {"name": "fs.read", "args": {"path": path_b}}}),
        serde_json::json!({"done": "tool_use"}),
    );
    let cont = format!("{}\n", serde_json::json!({"done": "end_turn"}));

    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(MockProvider::from_responses(vec![initial, cont]).unwrap());

    let server_session = Arc::clone(&session);
    let server_provider = Arc::clone(&provider);
    let server_sock = sock_path.clone();
    let server_workspace = Some(dir.path().to_path_buf());
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            server_session,
            server_provider,
            // auto_approve OFF — if read-only auto-approve regresses, the
            // orchestrator parks on the approval gate and the test times
            // out. We must NOT mask that regression with the global flag.
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
            text: "read both".into(),
        }),
    )
    .await
    .unwrap();

    // Drain until the continuation completes (two AssistantMessage(final)
    // events — one closing the tool turn, one closing the end_turn).
    let mut events: Vec<Event> = Vec::new();
    let mut final_count = 0;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let (mut reader, _writer) = stream.into_split();
    loop {
        let frame = tokio::time::timeout_at(deadline, forge_ipc::read_frame(&mut reader))
            .await
            .expect(
                "timed out — read-only auto-approval regressed and the daemon \
                 is parked on an approval prompt",
            )
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

    // ── Approval-gate behaviour ──────────────────────────────────────────
    let approval_requests = events
        .iter()
        .filter(|e| matches!(e, Event::ToolCallApprovalRequested { .. }))
        .count();
    assert_eq!(
        approval_requests, 0,
        "fs.read must auto-approve under #647 — saw {approval_requests} ToolCallApprovalRequested events"
    );

    let auto_approvals = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::ToolCallApproved {
                    by: ApprovalSource::Auto,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        auto_approvals, 2,
        "expected two ToolCallApproved {{ by: Auto }} (one per fs.read)"
    );

    // ── Parallel-dispatch branch ────────────────────────────────────────
    let started_groups: Vec<Option<u32>> = events
        .iter()
        .filter_map(|e| {
            if let Event::ToolCallStarted { parallel_group, .. } = e {
                Some(*parallel_group)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        started_groups.len(),
        2,
        "expected two ToolCallStarted events (one per fs.read), got {}",
        started_groups.len()
    );
    assert!(
        started_groups.iter().all(|g| g.is_some()),
        "both fs.read calls must carry parallel_group=Some(_) — sequential dispatch \
         (parallel_group=None) means the orchestrator skipped the parallel branch: {started_groups:?}"
    );
    assert_eq!(
        started_groups[0], started_groups[1],
        "both fs.read calls must share the same parallel_group id; got {started_groups:?}"
    );

    // ── Tool results landed ─────────────────────────────────────────────
    let read_payloads: Vec<String> = events
        .iter()
        .filter_map(|e| {
            if let Event::ToolCallCompleted { result, .. } = e {
                result
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        read_payloads.len(),
        2,
        "both fs.read calls must complete with a content payload; got {read_payloads:?}"
    );
    assert!(
        read_payloads.contains(&"alpha".to_string()) && read_payloads.contains(&"beta".to_string()),
        "fs.read results must contain both file contents; got {read_payloads:?}"
    );
}
