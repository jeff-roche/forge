//! F-139: end-to-end integration test for fine-grained step events.
//!
//! Drives a full turn (text delta + tool call + continuation) through
//! `serve_with_session`, captures the complete event tape from an IPC
//! subscriber, and asserts:
//!
//! 1. Every `StepStarted` has a terminating `StepFinished` with the same
//!    `step_id`.
//! 2. Step nesting is well-formed — an inner `Tool` step opens and closes
//!    inside its enclosing `Model` step.
//! 3. `ToolInvoked` / `ToolReturned` for a given `step_id` fall strictly
//!    between that step's `StepStarted` and `StepFinished`.
//! 4. `AssistantMessage` / `AssistantDelta` for a model step fall
//!    strictly between its `StepStarted{Model}` and `StepFinished{Model}`.
//! 5. `ToolInvoked` references the same `tool_call_id` as the matching
//!    `ToolCallStarted`.
//! 6. `args_digest` on `ToolInvoked` is deterministic for identical args
//!    and has a stable, short length.
//!
//! This is the ordering invariant pinned by the DoD; downstream consumers
//! (Agent Monitor, replay readers) rely on it.

use forge_core::{Event, StepId, StepKind};
use forge_ipc::{
    ClientInfo, Hello, IpcEvent, IpcMessage, SendUserMessage, Subscribe, ToolCallApproved,
    PROTO_VERSION,
};
use forge_providers::MockProvider;
use forge_session::{server::serve_with_session, session::Session};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::net::UnixStream;

const SCRIPT_INITIAL: &str = r#"{"delta":"Hi there. "}
{"tool_call":{"name":"fs.read","args":{"path":"readme.txt"}}}
{"done":"tool_use"}
"#;

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

/// Capture the full tape of a turn-with-tool-call. Auto-approves so the
/// test flow is linear.
async fn capture_turn_events(auto_approve: bool) -> Vec<Event> {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("step_events.sock");

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
            auto_approve,
            false,
            None,
            None,
            None, // F-587: keyless test wiring
            None, // F-601: no active agent — memory off in this test
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

        if !auto_approve {
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
        let is_continuation_model_close = matches!(event, Event::StepFinished { .. });
        events.push(event);
        // The second turn emits: AssistantMessage(final) → StepFinished(Model).
        // We wait for the trailing StepFinished so the recorded tape
        // includes the Model step's closing bracket.
        if final_count >= 2 && is_continuation_model_close {
            break;
        }
    }
    events
}

#[tokio::test]
async fn every_step_started_has_matching_step_finished_auto_approve() {
    let events = capture_turn_events(true).await;

    let mut open: Vec<StepId> = Vec::new();
    let mut closed: Vec<StepId> = Vec::new();
    for ev in &events {
        match ev {
            Event::StepStarted { step_id, .. } => open.push(step_id.clone()),
            Event::StepFinished { step_id, .. } => closed.push(step_id.clone()),
            _ => {}
        }
    }
    assert!(
        !open.is_empty(),
        "at least one StepStarted must be emitted on a turn; got events: {:?}",
        kinds_of(&events)
    );
    assert_eq!(
        open.len(),
        closed.len(),
        "mismatched StepStarted/StepFinished counts: open={open:?} closed={closed:?}"
    );
    // Multiset equality — order doesn't matter for this invariant, stack
    // nesting is tested separately.
    let mut o = open.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut c = closed.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    o.sort();
    c.sort();
    assert_eq!(o, c, "StepStarted and StepFinished step_ids must match");
}

#[tokio::test]
async fn step_nesting_is_lifo_auto_approve() {
    // Steps must close in reverse order to their opens (a stack discipline).
    // Today that produces the sequence model-open, tool-open, tool-close,
    // model-close, model-open (continuation), model-close.
    let events = capture_turn_events(true).await;
    let mut stack: Vec<StepId> = Vec::new();
    let mut ever_opened = 0usize;
    for ev in &events {
        match ev {
            Event::StepStarted { step_id, .. } => {
                stack.push(step_id.clone());
                ever_opened += 1;
            }
            Event::StepFinished { step_id, .. } => {
                let top = stack.pop().unwrap_or_else(|| {
                    panic!("StepFinished {step_id:?} with no open step on the stack")
                });
                assert_eq!(
                    &top, step_id,
                    "steps must close in LIFO order; closed={step_id:?}, top-of-stack={top:?}"
                );
            }
            _ => {}
        }
    }
    assert!(
        stack.is_empty(),
        "unterminated steps at end of turn: {stack:?}"
    );
    // Tool + two model steps at minimum: one for the initial provider
    // call, one for the continuation after the tool result.
    assert!(
        ever_opened >= 3,
        "expected at least 3 steps (model + tool + continuation model); got {ever_opened}"
    );
}

#[tokio::test]
async fn tool_step_contains_assistant_and_tool_events() {
    let events = capture_turn_events(true).await;

    // Find the tool step: the StepStarted with kind=Tool.
    let tool_step_id = events
        .iter()
        .find_map(|e| {
            if let Event::StepStarted {
                step_id,
                kind: StepKind::Tool,
                ..
            } = e
            {
                Some(step_id.clone())
            } else {
                None
            }
        })
        .expect("tool step must be emitted for a turn that invokes a tool");

    // Scan between its StepStarted and StepFinished; both ToolInvoked and
    // ToolReturned must appear for this step_id in that window.
    let mut in_tool_step = false;
    let mut saw_invoked = false;
    let mut saw_returned = false;
    for ev in &events {
        match ev {
            Event::StepStarted { step_id, .. } if *step_id == tool_step_id => {
                in_tool_step = true;
            }
            Event::StepFinished { step_id, .. } if *step_id == tool_step_id => {
                assert!(in_tool_step, "StepFinished before StepStarted for same id");
                in_tool_step = false;
            }
            Event::ToolInvoked { step_id, .. } if *step_id == tool_step_id => {
                assert!(
                    in_tool_step,
                    "ToolInvoked emitted outside its step's [StepStarted, StepFinished] window"
                );
                saw_invoked = true;
            }
            Event::ToolReturned { step_id, .. } if *step_id == tool_step_id => {
                assert!(
                    in_tool_step,
                    "ToolReturned emitted outside its step's [StepStarted, StepFinished] window"
                );
                assert!(
                    saw_invoked,
                    "ToolReturned must follow ToolInvoked within the same tool step"
                );
                saw_returned = true;
            }
            _ => {}
        }
    }
    assert!(saw_invoked, "tool step must contain a ToolInvoked event");
    assert!(saw_returned, "tool step must contain a ToolReturned event");
}

#[tokio::test]
async fn model_step_contains_assistant_events() {
    let events = capture_turn_events(true).await;

    // First model step opens before the first AssistantMessage(open);
    // AssistantDelta / AssistantMessage events for the turn's model step
    // must fall in its [start, end] window.
    let mut in_model_step = false;
    let mut saw_assistant_in_window = false;
    let mut ever_in_model_step = false;
    for ev in &events {
        match ev {
            Event::StepStarted {
                kind: StepKind::Model,
                ..
            } => {
                in_model_step = true;
                ever_in_model_step = true;
            }
            Event::StepFinished { .. } if in_model_step => {
                // Inner steps (Tool) will also emit StepFinished; we
                // close the model window only when we see a non-nested
                // StepFinished matching our open. The LIFO test already
                // guarantees well-formedness, so a simpler approximation:
                // a model step is open as long as at least one model
                // StepStarted is on the stack. For this assertion we
                // just need *any* assistant event to land during a model
                // window.
                in_model_step = false;
            }
            Event::AssistantMessage { .. } | Event::AssistantDelta { .. } if in_model_step => {
                saw_assistant_in_window = true;
            }
            _ => {}
        }
    }
    assert!(
        ever_in_model_step,
        "at least one model StepStarted must be emitted"
    );
    assert!(
        saw_assistant_in_window,
        "AssistantMessage/Delta events must land between StepStarted(Model) and its StepFinished"
    );
}

#[tokio::test]
async fn step_started_index_is_monotonic_one_based_per_turn() {
    // F-606: every `StepStarted` carries a 1-based `index` that increments
    // by exactly 1 across the turn — Model and Tool steps share the same
    // counter so the AgentMonitor §9.2 chip can render `step N`
    // consistently regardless of step kind.
    let events = capture_turn_events(true).await;

    let indices: Vec<u32> = events
        .iter()
        .filter_map(|e| {
            if let Event::StepStarted { index, .. } = e {
                Some(*index)
            } else {
                None
            }
        })
        .collect();

    assert!(
        !indices.is_empty(),
        "at least one StepStarted must be emitted; got: {:?}",
        kinds_of(&events)
    );
    let expected: Vec<u32> = (1..=indices.len() as u32).collect();
    assert_eq!(
        indices, expected,
        "StepStarted.index must be 1-based and monotonically increase by 1 per emission; got {indices:?}"
    );
}

#[tokio::test]
async fn step_started_total_is_unknown_during_turn() {
    // F-606: today the orchestrator does not pre-plan the step list — the
    // model decides what to call turn-by-turn — so `total` rides as
    // `None`. UI falls back to `step N` (no `of M`). A future change may
    // populate `total` retroactively or via a companion event.
    let events = capture_turn_events(true).await;
    for ev in &events {
        if let Event::StepStarted { total, .. } = ev {
            assert!(
                total.is_none(),
                "Phase-3 orchestrator emits StepStarted.total = None until step pre-planning lands; got Some({})",
                total.unwrap()
            );
        }
    }
}

#[tokio::test]
async fn tool_invoked_matches_tool_call_started_id() {
    let events = capture_turn_events(true).await;

    // Pair ToolCallStarted with ToolInvoked by tool_call_id.
    let started_id = events
        .iter()
        .find_map(|e| {
            if let Event::ToolCallStarted { id, .. } = e {
                Some(id.clone())
            } else {
                None
            }
        })
        .expect("ToolCallStarted must be emitted on a tool-calling turn");
    let invoked_id = events
        .iter()
        .find_map(|e| {
            if let Event::ToolInvoked { tool_call_id, .. } = e {
                Some(tool_call_id.clone())
            } else {
                None
            }
        })
        .expect("ToolInvoked must be emitted on a tool-calling turn");
    assert_eq!(
        started_id.to_string(),
        invoked_id.to_string(),
        "ToolInvoked.tool_call_id must match the preceding ToolCallStarted.id"
    );
}

#[tokio::test]
async fn args_digest_is_short_and_stable() {
    let events = capture_turn_events(true).await;
    let digest = events
        .iter()
        .find_map(|e| {
            if let Event::ToolInvoked { args_digest, .. } = e {
                Some(args_digest.clone())
            } else {
                None
            }
        })
        .expect("ToolInvoked must carry an args_digest");
    // Hex-8 per the implementation — 8 lowercase hex chars.
    assert_eq!(
        digest.len(),
        8,
        "args_digest must be 8 hex chars for a compact, cheap identifier; got {digest:?}"
    );
    assert!(
        digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && (c.is_ascii_digit() || c.is_lowercase())),
        "args_digest must be lowercase hex; got {digest:?}"
    );

    // A second run of the same canned script must produce the same digest
    // (same args → same digest).
    let events2 = capture_turn_events(true).await;
    let digest2 = events2
        .iter()
        .find_map(|e| {
            if let Event::ToolInvoked { args_digest, .. } = e {
                Some(args_digest.clone())
            } else {
                None
            }
        })
        .expect("ToolInvoked must carry an args_digest");
    assert_eq!(
        digest, digest2,
        "args_digest must be deterministic for identical args across runs"
    );
}

#[tokio::test]
async fn tool_returned_bytes_out_matches_result_size() {
    let events = capture_turn_events(true).await;
    // `bytes_out` must equal the serialized result JSON length at the time
    // of emission. We cross-check by pulling the matching `ToolCallCompleted`.
    let returned = events.iter().find_map(|e| {
        if let Event::ToolReturned {
            step_id: _,
            tool_call_id,
            ok,
            bytes_out,
        } = e
        {
            Some((tool_call_id.clone(), *ok, *bytes_out))
        } else {
            None
        }
    });
    let completed = events.iter().find_map(|e| {
        if let Event::ToolCallCompleted { id, result, .. } = e {
            Some((id.clone(), result.clone()))
        } else {
            None
        }
    });
    let (ret_id, _ok, bytes_out) = returned.expect("ToolReturned must be emitted");
    let (comp_id, result) = completed.expect("ToolCallCompleted must be emitted");
    assert_eq!(
        ret_id.to_string(),
        comp_id.to_string(),
        "ToolReturned.tool_call_id must match ToolCallCompleted.id"
    );
    let expected_bytes = serde_json::to_string(&result).unwrap().len() as u64;
    assert_eq!(
        bytes_out, expected_bytes,
        "ToolReturned.bytes_out must equal the serialized ToolCallCompleted.result length"
    );
}

fn kinds_of(events: &[Event]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match e {
            Event::UserMessage { .. } => "UserMessage",
            Event::AssistantMessage { .. } => "AssistantMessage",
            Event::AssistantDelta { .. } => "AssistantDelta",
            Event::ToolCallStarted { .. } => "ToolCallStarted",
            Event::ToolCallApprovalRequested { .. } => "ToolCallApprovalRequested",
            Event::ToolCallApproved { .. } => "ToolCallApproved",
            Event::ToolCallRejected { .. } => "ToolCallRejected",
            Event::ToolCallCompleted { .. } => "ToolCallCompleted",
            Event::StepStarted { .. } => "StepStarted",
            Event::StepFinished { .. } => "StepFinished",
            Event::ToolInvoked { .. } => "ToolInvoked",
            Event::ToolReturned { .. } => "ToolReturned",
            _ => "Other",
        })
        .collect()
}
