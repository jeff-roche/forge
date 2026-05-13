#![allow(deprecated)] // F-652: tests/benches still drive the deprecated bare read_frame helpers.
use forge_core::{ApprovalScope, Event};
use forge_ipc::{
    ClientInfo, Hello, IpcEvent, IpcMessage, SendUserMessage, Subscribe, ToolCallApproved,
    PROTO_VERSION,
};
// F-750: explicit-reject integration test below uses the typed
// `forge_ipc::ToolCallRejected` struct (distinct from `Event::ToolCallRejected`
// imported via `forge_core::Event`).
use forge_ipc::ToolCallRejected as IpcToolCallRejected;
use forge_providers::{ChatBlock, MockProvider};
use forge_session::{server::serve_with_session, session::Session};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::net::UnixStream;

// Script 1: provider returns a text delta, then a `fs.read` tool call,
// then Done("tool_use"). `fs.read` is read-only so the orchestrator
// auto-approves it (issue #647) — the recorded event sequence skips
// `ToolCallApprovalRequested` and emits `ToolCallApproved { by: Auto }`.
const SCRIPT_INITIAL: &str = r#"{"delta":"Hi there. "}
{"tool_call":{"name":"fs.read","args":{"path":"readme.txt"}}}
{"done":"tool_use"}
"#;

// Mirror of `SCRIPT_INITIAL` that drives a non-read-only tool (`fs.write`)
// so the approval gate actually fires. Tests that pin behaviour of the
// interactive approval flow (gate firing, scope fidelity, malformed-scope
// rejection, pause-during-approval) use this script — switching `fs.read`
// to read-only would otherwise auto-approve them and bypass the gate
// entirely.
const SCRIPT_INITIAL_NEEDS_APPROVAL: &str = r#"{"delta":"Hi there. "}
{"tool_call":{"name":"fs.write","args":{"path":"readme.txt","content":"x"}}}
{"done":"tool_use"}
"#;

// Script 2: provider receives tool result, returns continuation text, then Done("end_turn")
const SCRIPT_CONTINUATION: &str = r#"{"delta":"Here is the file content."}
{"done":"end_turn"}
"#;

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

/// Full turn with tool call: verifies correct event sequence end-to-end.
///
/// `fs.read` is read-only (issue #647), so the orchestrator skips the
/// interactive approval gate and emits `ToolCallApproved { by: Auto }`
/// directly. The expected sequence is otherwise identical to the
/// pre-#647 shape.
///
/// Expected sequence:
///   UserMessage
///   AssistantMessage { stream_finalised: false }   ← opened before first chunk
///   AssistantDelta("Hi there. ")
///   ToolCallStarted { tool: "fs.read" }
///   ToolCallApproved { by: Auto }   ← read-only tools skip the gate
///   ToolCallCompleted
///   AssistantMessage { stream_finalised: true }    ← finalised when Done("tool_use") arrives
///   AssistantMessage { stream_finalised: false }   ← continuation turn opens
///   AssistantDelta("Here is the file content.")
///   AssistantMessage { stream_finalised: true }    ← continuation finalised
#[tokio::test]
async fn full_turn_with_tool_call_emits_correct_event_sequence() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("test.sock");
    // Isolate the daemon's user-scope `~/.mcp.json` loader from the
    // developer's real home so a populated host `~/.mcp.json` does not
    // prepend an `Event::McpState` to the captured event tape and break
    // the sequence assertion below.
    let user_home = dir.path().join("user_home");
    std::fs::create_dir_all(&user_home).unwrap();

    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(
        MockProvider::from_responses(vec![
            SCRIPT_INITIAL.to_string(),
            SCRIPT_CONTINUATION.to_string(),
        ])
        .unwrap(),
    );

    let server_session = Arc::clone(&session);
    let server_provider = Arc::clone(&provider);
    let server_sock = sock_path.clone();
    let server_user_home = user_home.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            server_session,
            server_provider,
            false,
            false,
            None,
            None,
            None, // F-587: keyless test wiring
            None, // F-601: no active agent — memory off in this test
            Some(server_user_home),
        )
        .await
        .unwrap();
    });

    let mut stream = connect_with_retry(&sock_path).await;
    do_handshake(&mut stream).await;

    let sub = IpcMessage::Subscribe(Subscribe { since: 0 });
    forge_ipc::write_frame(&mut stream, &sub).await.unwrap();

    let send = IpcMessage::SendUserMessage(SendUserMessage {
        text: "hello".to_string(),
    });
    forge_ipc::write_frame(&mut stream, &send).await.unwrap();

    // Collect events; when we see ToolCallApprovalRequested, send approval back.
    // The full turn produces two AssistantMessage(final) events: one when the
    // initial stream ends with Done("tool_use"), and one when the continuation ends.
    let (mut reader, _writer) = stream.into_split();
    let mut events: Vec<Event> = Vec::new();
    let mut final_count = 0;

    loop {
        let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
        let Some(event) = extract_event(&frame) else {
            continue;
        };

        // No client-side approval needed — `fs.read` is read-only and the
        // orchestrator emits `ToolCallApproved { by: Auto }` on its own.

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
        // Two scripts → two provider calls → two finalised messages.
        if final_count >= 2 {
            break;
        }
    }

    // Assert the event sequence. F-139 step-trace events are filtered
    // out here — they're covered by `tests/step_events.rs` and are
    // orthogonal to the user-visible ordering this test pins.
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
            | Event::ToolReturned { .. } => None,
            _ => Some("Other"),
        })
        .collect();

    assert_eq!(
        kinds,
        vec![
            "UserMessage",
            "AssistantMessage(open)", // opened before first chunk
            "AssistantDelta",
            "ToolCallStarted",
            "ToolCallApproved", // read-only auto-approval (#647) — no prompt
            "ToolCallCompleted",
            "AssistantMessage(final)", // finalised when Done("tool_use") arrives
            "AssistantMessage(open)",  // continuation turn opens
            "AssistantDelta",
            "AssistantMessage(final)", // continuation turn finalised
        ],
        "event sequence mismatch: got {kinds:?}"
    );

    // The single ToolCallApproved must carry `ApprovalSource::Auto` so the
    // event log reflects that no human approved the call.
    use forge_core::ApprovalSource;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::ToolCallApproved {
                by: ApprovalSource::Auto,
                ..
            }
        )),
        "expected ToolCallApproved {{ by: Auto }} for read-only fs.read"
    );

    // Verify text delta content
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
    assert_eq!(deltas, vec!["Hi there. ", "Here is the file content."]);

    // Verify tool call details
    let tool_name = events.iter().find_map(|e| {
        if let Event::ToolCallStarted { tool, .. } = e {
            Some(tool.as_str())
        } else {
            None
        }
    });
    assert_eq!(tool_name, Some("fs.read"));

    // Verify tool result was fed back: the second provider call happened
    // (proven by receiving continuation events after ToolCallCompleted)
    let continuation_delta_idx = events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| matches!(e, Event::AssistantDelta { .. }).then_some(i))
        .nth(1); // second delta
    let tool_completed_idx = events
        .iter()
        .position(|e| matches!(e, Event::ToolCallCompleted { .. }));
    assert!(
        continuation_delta_idx > tool_completed_idx,
        "continuation delta must follow tool completion"
    );
}

/// Verify approval gate fires for non-whitelisted tools (and NOT for the turn to continue
/// without approval being sent — i.e., the orchestrator pauses until client responds).
#[tokio::test]
async fn approval_gate_fires_and_blocks_until_client_approves() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("test2.sock");

    let session = Arc::new(Session::create(log_path).await.unwrap());
    // Single-script provider: tool call only (no continuation needed for this test).
    // Use the non-read-only script so the approval gate actually fires
    // (issue #647 made `fs.read` auto-approve).
    let provider = Arc::new(
        MockProvider::from_responses(vec![SCRIPT_INITIAL_NEEDS_APPROVAL.to_string()]).unwrap(),
    );

    let server_session = Arc::clone(&session);
    let server_provider = Arc::clone(&provider);
    let server_sock = sock_path.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            server_session,
            server_provider,
            false,
            false,
            None,
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
            text: "hi".to_string(),
        }),
    )
    .await
    .unwrap();

    let (mut reader, mut writer) = stream.into_split();

    // Read events until ToolCallApprovalRequested is received
    let mut saw_approval_requested = false;
    let mut tool_call_id = String::new();
    for _ in 0..10 {
        let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
        if let Some(Event::ToolCallApprovalRequested { id, .. }) = extract_event(&frame) {
            saw_approval_requested = true;
            tool_call_id = id.to_string();
            break;
        }
    }
    assert!(
        saw_approval_requested,
        "expected ToolCallApprovalRequested to be emitted"
    );

    // Now send approval
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::ToolCallApproved(ToolCallApproved {
            id: tool_call_id,
            scope: "Once".to_string(),
        }),
    )
    .await
    .unwrap();

    // Verify ToolCallCompleted arrives after approval
    let mut saw_completed = false;
    for _ in 0..10 {
        let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
        if let Some(Event::ToolCallCompleted { .. }) = extract_event(&frame) {
            saw_completed = true;
            break;
        }
    }
    assert!(saw_completed, "expected ToolCallCompleted after approval");
}

/// With --auto-approve-unsafe, tool calls proceed without client approval.
/// ToolCallApproved { by: Auto } must be emitted; ToolCallApprovalRequested must not.
///
/// Uses the non-read-only `fs.write` script so this exercises the server-
/// level `--auto-approve-unsafe` flag rather than the per-tool read-only
/// shortcut introduced in #647 (which would also emit `Auto` and mask a
/// regression in the flag itself).
#[tokio::test]
async fn auto_approve_skips_approval_gate_and_emits_auto_approved() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("auto_approve.sock");

    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(
        MockProvider::from_responses(vec![
            SCRIPT_INITIAL_NEEDS_APPROVAL.to_string(),
            SCRIPT_CONTINUATION.to_string(),
        ])
        .unwrap(),
    );

    let server_session = Arc::clone(&session);
    let server_provider = Arc::clone(&provider);
    let server_sock = sock_path.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            server_session,
            server_provider,
            true,
            false,
            None,
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
            text: "hello".to_string(),
        }),
    )
    .await
    .unwrap();

    // Collect events until the turn completes (two finalised AssistantMessages).
    // No client approval is sent — the session must complete without it.
    let mut events: Vec<Event> = Vec::new();
    let mut final_count = 0;
    let (reader, _writer) = stream.into_split();
    // Use a timeout so the test fails fast if the session blocks waiting for approval.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut reader = reader;

    loop {
        let frame = tokio::time::timeout_at(deadline, forge_ipc::read_frame(&mut reader))
            .await
            .expect("timed out — session may be blocked waiting for approval")
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

    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            Event::UserMessage { .. } => "UserMessage",
            Event::AssistantMessage {
                stream_finalised: false,
                ..
            } => "AssistantMessage(open)",
            Event::AssistantMessage {
                stream_finalised: true,
                ..
            } => "AssistantMessage(final)",
            Event::AssistantDelta { .. } => "AssistantDelta",
            Event::ToolCallStarted { .. } => "ToolCallStarted",
            Event::ToolCallApprovalRequested { .. } => "ToolCallApprovalRequested",
            Event::ToolCallApproved { .. } => "ToolCallApproved",
            Event::ToolCallCompleted { .. } => "ToolCallCompleted",
            _ => "Other",
        })
        .collect();

    // ToolCallApprovalRequested must NOT appear; ToolCallApproved must appear.
    assert!(
        !kinds.contains(&"ToolCallApprovalRequested"),
        "auto-approve must not emit ToolCallApprovalRequested; got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"ToolCallApproved"),
        "auto-approve must emit ToolCallApproved; got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"ToolCallCompleted"),
        "auto-approve must emit ToolCallCompleted; got: {kinds:?}"
    );

    // Verify ToolCallApproved carries ApprovalSource::Auto.
    use forge_core::ApprovalSource;
    let auto_approved = events.iter().any(|e| {
        matches!(
            e,
            Event::ToolCallApproved {
                by: ApprovalSource::Auto,
                ..
            }
        )
    });
    assert!(auto_approved, "ToolCallApproved must have by=Auto");
}

/// Verify tool result is included in the next ChatRequest to the provider.
/// Proven by: the continuation response arrives (provider was called a second time).
#[tokio::test]
async fn tool_result_fed_back_to_provider_in_continuation() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("test3.sock");

    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(
        MockProvider::from_responses(vec![
            SCRIPT_INITIAL.to_string(),
            SCRIPT_CONTINUATION.to_string(),
        ])
        .unwrap(),
    );

    let server_session = Arc::clone(&session);
    let server_provider = Arc::clone(&provider);
    let server_sock = sock_path.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            server_session,
            server_provider,
            false,
            false,
            None,
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
            text: "hello".to_string(),
        }),
    )
    .await
    .unwrap();

    let (mut reader, mut writer) = stream.into_split();
    let mut events: Vec<Event> = Vec::new();
    let mut final_count = 0;

    loop {
        let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
        let Some(event) = extract_event(&frame) else {
            continue;
        };
        if let Event::ToolCallApprovalRequested { ref id, .. } = event {
            forge_ipc::write_frame(
                &mut writer,
                &IpcMessage::ToolCallApproved(ToolCallApproved {
                    id: id.to_string(),
                    scope: "Once".to_string(),
                }),
            )
            .await
            .unwrap();
        }
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
        // Two scripts → two provider calls → two finalised messages.
        if final_count >= 2 {
            break;
        }
    }

    // The second AssistantDelta ("Here is the file content.") proves the provider
    // was called a second time — i.e., tool result was fed back.
    let continuation_text = events.iter().find_map(|e| {
        if let Event::AssistantDelta { delta, .. } = e {
            if delta.contains("file content") {
                // F-112: `delta: Arc<str>` — `&*delta` is `&str` via Deref.
                Some(&**delta)
            } else {
                None
            }
        } else {
            None
        }
    });
    assert_eq!(
        continuation_text,
        Some("Here is the file content."),
        "continuation response from second provider call not received"
    );

    // Check that the second ChatRequest included a ToolResult block.
    // We verify this indirectly: MockProvider::from_responses() tracks requests;
    // assert the second request contains a ToolResult block.
    let requests = provider.recorded_requests();
    assert_eq!(requests.len(), 2, "provider should have been called twice");
    let second_req = &requests[1];
    let has_tool_result = second_req.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ChatBlock::ToolResult { .. }))
    });
    assert!(
        has_tool_result,
        "second ChatRequest should contain a ToolResult block"
    );
}

/// F-053 regression: the client-supplied ApprovalScope on `ToolCallApproved`
/// must be recorded faithfully in the emitted `Event::ToolCallApproved`,
/// rather than being hard-coded to `Once`.
#[tokio::test]
async fn approval_with_this_tool_scope_is_recorded_faithfully() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("scope_fidelity.sock");

    let session = Arc::new(Session::create(log_path).await.unwrap());
    // Use the non-read-only script — `fs.read` auto-approves under #647 and
    // would never reach the gate this test is asserting on.
    let provider = Arc::new(
        MockProvider::from_responses(vec![SCRIPT_INITIAL_NEEDS_APPROVAL.to_string()]).unwrap(),
    );

    let server_session = Arc::clone(&session);
    let server_provider = Arc::clone(&provider);
    let server_sock = sock_path.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            server_session,
            server_provider,
            false,
            false,
            None,
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
            text: "hi".to_string(),
        }),
    )
    .await
    .unwrap();

    let (mut reader, mut writer) = stream.into_split();

    // Read events, approve with scope="ThisTool" when requested, and capture
    // the emitted ToolCallApproved event so we can inspect its scope.
    let mut approved_scope: Option<ApprovalScope> = None;
    for _ in 0..20 {
        let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
        match extract_event(&frame) {
            Some(Event::ToolCallApprovalRequested { id, .. }) => {
                forge_ipc::write_frame(
                    &mut writer,
                    &IpcMessage::ToolCallApproved(ToolCallApproved {
                        id: id.to_string(),
                        scope: "ThisTool".to_string(),
                    }),
                )
                .await
                .unwrap();
            }
            Some(Event::ToolCallApproved { scope, .. }) => {
                approved_scope = Some(scope);
                break;
            }
            _ => {}
        }
    }

    assert_eq!(
        approved_scope,
        Some(ApprovalScope::ThisTool),
        "client-supplied ApprovalScope::ThisTool must be recorded faithfully"
    );
}

/// F-074 regression: when the client sends a `ToolCallApproved` whose
/// `scope` does not deserialize into `ApprovalScope` (e.g. PascalCase typo
/// "Always" or any other unknown variant), the session must NOT silently
/// downgrade the approval to `Once`. Doing so would honour an approval the
/// user did not actually grant. The new behaviour rejects the approval
/// outright; the orchestrator emits `Event::ToolCallRejected` and the tool
/// call is denied — observable to both client and session log.
#[tokio::test]
async fn malformed_approval_scope_rejects_instead_of_silently_downgrading_to_once() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("scope_reject.sock");

    let session = Arc::new(Session::create(log_path).await.unwrap());
    // Use the non-read-only script — `fs.read` auto-approves under #647 so
    // the gate we're stress-testing here would never fire on it.
    let provider = Arc::new(
        MockProvider::from_responses(vec![SCRIPT_INITIAL_NEEDS_APPROVAL.to_string()]).unwrap(),
    );

    let server_session = Arc::clone(&session);
    let server_provider = Arc::clone(&provider);
    let server_sock = sock_path.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            server_session,
            server_provider,
            false,
            false,
            None,
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
            text: "hi".to_string(),
        }),
    )
    .await
    .unwrap();

    let (mut reader, mut writer) = stream.into_split();

    // Read events, send a bogus scope when approval is requested, and
    // assert we observe a ToolCallRejected (NOT a ToolCallApproved {Once}).
    let mut saw_rejected = false;
    let mut saw_approved = false;
    for _ in 0..40 {
        let frame = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            forge_ipc::read_frame(&mut reader),
        )
        .await
        {
            Ok(Ok(f)) => f,
            Ok(Err(_)) | Err(_) => break,
        };
        match extract_event(&frame) {
            Some(Event::ToolCallApprovalRequested { id, .. }) => {
                forge_ipc::write_frame(
                    &mut writer,
                    &IpcMessage::ToolCallApproved(ToolCallApproved {
                        id: id.to_string(),
                        // Deliberately unknown variant — this used to be
                        // silently downgraded to `ApprovalScope::Once`.
                        scope: "Always".to_string(),
                    }),
                )
                .await
                .unwrap();
            }
            Some(Event::ToolCallRejected { .. }) => {
                saw_rejected = true;
                break;
            }
            Some(Event::ToolCallApproved { .. }) => {
                saw_approved = true;
                break;
            }
            _ => {}
        }
    }

    assert!(
        !saw_approved,
        "malformed scope must NOT produce ToolCallApproved (silent downgrade regression)"
    );
    assert!(
        saw_rejected,
        "malformed scope must produce ToolCallRejected"
    );
}

/// F-074 regression: the dispatch loop's previous catch-all
/// `Some(_) => {}` silently dropped unexpected `IpcMessage` variants. The
/// new exhaustive match logs the discriminant and continues; the session
/// must not panic, deadlock, or stop processing subsequent valid frames.
///
/// This test sends a duplicate `Hello` mid-session — structurally valid as
/// an `IpcMessage`, never expected after handshake — then drives a normal
/// `SendUserMessage` turn through the same connection. If the session
/// survives and emits the expected events, the dispatch path correctly
/// handles the unexpected frame.
#[tokio::test]
async fn unexpected_post_handshake_frame_is_logged_not_silently_dropped() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("ipc_unexpected.sock");

    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider =
        Arc::new(MockProvider::from_responses(vec![SCRIPT_INITIAL.to_string()]).unwrap());

    let server_session = Arc::clone(&session);
    let server_provider = Arc::clone(&provider);
    let server_sock = sock_path.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            server_session,
            server_provider,
            true, // auto_approve so we don't need an approval round-trip
            false,
            None,
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

    // Send a duplicate Hello after handshake — this is an unexpected frame.
    forge_ipc::write_frame(
        &mut stream,
        &IpcMessage::Hello(Hello {
            proto: PROTO_VERSION,
            client: ClientInfo {
                kind: "test-replay".into(),
                pid: std::process::id(),
                user: "tester".into(),
            },
            schema_version: forge_ipc::SCHEMA_VERSION,
        }),
    )
    .await
    .unwrap();

    // Then issue a normal turn — the session must still be alive and
    // responsive after the unexpected frame.
    forge_ipc::write_frame(
        &mut stream,
        &IpcMessage::SendUserMessage(SendUserMessage {
            text: "hi".to_string(),
        }),
    )
    .await
    .unwrap();

    let (mut reader, _writer) = stream.into_split();
    let mut saw_user_msg = false;
    for _ in 0..40 {
        let frame = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            forge_ipc::read_frame(&mut reader),
        )
        .await
        {
            Ok(Ok(f)) => f,
            Ok(Err(_)) | Err(_) => break,
        };
        if let Some(Event::UserMessage { .. }) = extract_event(&frame) {
            saw_user_msg = true;
            break;
        }
    }

    assert!(
        saw_user_msg,
        "session must keep processing valid frames after an unexpected one"
    );
}

/// F-750: explicit client-rejection path through the full IPC turn.
///
/// Validation contract — DoD item *"Reject path tested at least once per
/// provider — confirms `ToolCallRejected` propagates and the turn ends
/// cleanly"*. The malformed-scope test above exercises the *implicit*
/// rejection path (server rewrites a bad scope to `Rejected`); this test
/// exercises the explicit one — a well-formed `IpcMessage::ToolCallRejected`
/// frame from the client.
///
/// Wire contract pinned here so a real-provider rejection (Ollama,
/// Anthropic, OpenAI) all unwind the same way:
///
/// 1. `Event::ToolCallApprovalRequested` is emitted for the non-read-only
///    `fs.write` call.
/// 2. Client sends `IpcMessage::ToolCallRejected`.
/// 3. Server emits exactly one `Event::ToolCallRejected` (no
///    `ToolCallApproved`, no `ToolCallCompleted`, no
///    `ToolInvoked` / `ToolReturned`).
/// 4. The turn ends without a continuation request to the provider.
#[tokio::test]
async fn explicit_client_rejection_emits_tool_call_rejected_and_ends_turn() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("reject.sock");

    let session = Arc::new(Session::create(log_path).await.unwrap());
    // Single-script provider — if rejection is propagated correctly the
    // provider is never invoked a second time for a continuation.
    let provider = Arc::new(
        MockProvider::from_responses(vec![SCRIPT_INITIAL_NEEDS_APPROVAL.to_string()]).unwrap(),
    );

    let server_session = Arc::clone(&session);
    let server_provider = Arc::clone(&provider);
    let server_sock = sock_path.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            server_session,
            server_provider,
            false,
            false,
            None,
            None,
            None,
            None,
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
            text: "hi".to_string(),
        }),
    )
    .await
    .unwrap();

    let (mut reader, mut writer) = stream.into_split();

    // Drive the turn: wait for the approval request, send a typed Reject,
    // then capture every subsequent event so we can assert the unwind shape.
    let mut sent_reject = false;
    let mut saw_rejected = false;
    let mut saw_approved = false;
    let mut saw_completed = false;
    let mut saw_invoked = false;

    for _ in 0..60 {
        let frame = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            forge_ipc::read_frame(&mut reader),
        )
        .await
        {
            Ok(Ok(f)) => f,
            Ok(Err(_)) | Err(_) => break,
        };
        match extract_event(&frame) {
            Some(Event::ToolCallApprovalRequested { id, .. }) if !sent_reject => {
                forge_ipc::write_frame(
                    &mut writer,
                    &IpcMessage::ToolCallRejected(IpcToolCallRejected {
                        id: id.to_string(),
                        reason: Some("user denied".into()),
                    }),
                )
                .await
                .unwrap();
                sent_reject = true;
            }
            Some(Event::ToolCallRejected { .. }) => {
                saw_rejected = true;
                // Keep draining a few frames to confirm nothing else fires.
            }
            Some(Event::ToolCallApproved { .. }) => saw_approved = true,
            Some(Event::ToolCallCompleted { .. }) => saw_completed = true,
            Some(Event::ToolInvoked { .. }) => saw_invoked = true,
            _ => {}
        }
        if saw_rejected {
            // Give the server a short window to flush any (incorrect)
            // post-rejection events before we conclude.
            if let Ok(Ok(extra)) = tokio::time::timeout(
                std::time::Duration::from_millis(150),
                forge_ipc::read_frame(&mut reader),
            )
            .await
            {
                match extract_event(&extra) {
                    Some(Event::ToolCallApproved { .. }) => saw_approved = true,
                    Some(Event::ToolCallCompleted { .. }) => saw_completed = true,
                    Some(Event::ToolInvoked { .. }) => saw_invoked = true,
                    _ => {}
                }
            }
            break;
        }
    }

    assert!(
        sent_reject,
        "test harness must have observed ToolCallApprovalRequested and sent rejection"
    );
    assert!(
        saw_rejected,
        "explicit client rejection must produce Event::ToolCallRejected"
    );
    assert!(
        !saw_approved,
        "rejected tool call must NOT emit ToolCallApproved"
    );
    assert!(
        !saw_invoked,
        "rejected tool call must NOT reach ToolInvoked"
    );
    assert!(
        !saw_completed,
        "rejected tool call must NOT emit ToolCallCompleted"
    );
}
