//! F-603: end-to-end coverage for the orchestrator pause/resume primitive.
//!
//! These tests drive a real `serve_with_session` daemon over a UDS pair, the
//! same shape as `tests/orchestrator.rs`. The pause/resume contract under
//! test:
//!
//! * `PauseSession` / `ResumeSession` IPC frames flip the daemon's
//!   in-memory `Paused` flag.
//! * The orchestrator parks at the between-step checkpoint
//!   (`Session::wait_if_paused`) — never mid-step.
//! * `Event::SessionPaused` / `Event::SessionResumed` fire **once** per
//!   real transition.
//! * Idempotency: pause-while-paused and resume-while-running are no-ops
//!   that emit no event (a `debug!` log fires server-side).
//! * Tool-in-flight semantics: a pause issued during the approval window
//!   takes effect at the next inter-step boundary, not by aborting the
//!   in-flight tool.

use forge_core::Event;
use forge_ipc::{
    ClientInfo, Hello, IpcEvent, IpcMessage, PauseSession, ResumeSession, SendUserMessage,
    Subscribe, ToolCallApproved, PROTO_VERSION,
};
use forge_providers::MockProvider;
use forge_session::{server::serve_with_session, session::Session};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::{timeout, Duration};

// Two-pass script: first pass emits one assistant delta + Done; the
// continuation also emits one delta + Done. The pause checkpoint sits
// between provider passes, so wedging a pause between the two passes is
// the cleanest way to exercise the between-step contract.
const SCRIPT_FIRST: &str = r#"{"delta":"step one"}
{"tool_call":{"name":"fs.read","args":{"path":"a.txt"}}}
{"done":"tool_use"}
"#;

const SCRIPT_SECOND: &str = r#"{"delta":"step two"}
{"done":"end_turn"}
"#;

async fn connect_with_retry(path: &std::path::PathBuf) -> UnixStream {
    for _ in 0..50 {
        match UnixStream::connect(path).await {
            Ok(s) => return s,
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
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
    assert!(matches!(response, IpcMessage::HelloAck(_)));
}

fn extract_event(msg: &IpcMessage) -> Option<Event> {
    if let IpcMessage::Event(IpcEvent { event, .. }) = msg {
        Some(event.clone())
    } else {
        None
    }
}

/// Spawn a `serve_with_session` daemon. Returns the socket path and the
/// shared `Session` so tests can inspect `is_paused()` directly.
async fn spawn_daemon(dir: &TempDir, scripts: Vec<String>) -> (std::path::PathBuf, Arc<Session>) {
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("test.sock");

    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(MockProvider::from_responses(scripts).unwrap());

    let server_session = Arc::clone(&session);
    let server_provider = Arc::clone(&provider);
    let server_sock = sock_path.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            server_session,
            server_provider,
            true, // auto_approve so the test pauses without juggling approvals
            false,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    });

    (sock_path, session)
}

/// DoD: pause-before-turn → resume → completion.
///
/// Strategy: pause the session BEFORE sending the user message so the
/// orchestrator's first-iteration `wait_if_paused` checkpoint
/// deterministically parks the turn. With auto_approve on, a single
/// `SendUserMessage` would otherwise drive the full two-script
/// completion to the wire faster than the IPC handler can flip the
/// pause flag — using pre-send pause sidesteps that race and pins the
/// "no StepStarted while paused" contract precisely.
///
/// We then verify:
///   * `Event::SessionPaused` arrives.
///   * `Session::is_paused()` is true and remains true while no resume
///     frame has been sent.
///   * NO `StepStarted` lands while paused.
///   * `Event::SessionResumed` arrives on resume.
///   * The full turn completes after resume (final
///     `AssistantMessage(stream_finalised: true)` lands).
#[tokio::test]
async fn pause_before_turn_then_resume_then_complete() {
    let dir = TempDir::new().unwrap();
    let (sock_path, session) =
        spawn_daemon(&dir, vec![SCRIPT_FIRST.into(), SCRIPT_SECOND.into()]).await;

    let mut stream = connect_with_retry(&sock_path).await;
    do_handshake(&mut stream).await;
    forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();

    let (mut reader, mut writer) = stream.into_split();

    // Pause FIRST. No turn is in flight, so this is purely a flag flip
    // on the daemon side; `SessionPaused` is the next event we will
    // observe from the stream.
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::PauseSession(PauseSession::default()),
    )
    .await
    .unwrap();

    let paused_event = timeout(Duration::from_secs(5), async {
        loop {
            let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
            if let Some(event) = extract_event(&frame) {
                return event;
            }
        }
    })
    .await
    .expect("never observed SessionPaused");
    assert!(
        matches!(paused_event, Event::SessionPaused { .. }),
        "expected SessionPaused, got {paused_event:?}",
    );
    assert!(session.is_paused(), "Session::is_paused must be true");

    // Now send the user message. The orchestrator's request loop must
    // park at `Session::wait_if_paused` before emitting the first
    // `StepStarted(Model)`. Confirm by waiting for any frame —
    // none should arrive while paused.
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::SendUserMessage(SendUserMessage {
            text: "hello".into(),
        }),
    )
    .await
    .unwrap();

    // The `UserMessage` event still fires (it's emitted before the
    // request loop opens — see `run_turn`), so we must allow exactly
    // one `UserMessage` here. After that the loop's first iteration
    // hits `wait_if_paused` and parks; no `StepStarted` should land.
    let mut events_while_paused: Vec<Event> = Vec::new();
    let _ = timeout(Duration::from_millis(500), async {
        loop {
            let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
            if let Some(event) = extract_event(&frame) {
                events_while_paused.push(event);
            }
        }
    })
    .await;
    assert!(
        events_while_paused
            .iter()
            .all(|e| matches!(e, Event::UserMessage { .. })),
        "no `StepStarted` (or any non-UserMessage event) may fire while paused; saw {events_while_paused:?}",
    );

    // Resume. Expect `SessionResumed`, then the turn's full event
    // sequence (StepStarted(Model), AssistantMessage(open),
    // AssistantDelta, ..., AssistantMessage(final)).
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::ResumeSession(ResumeSession::default()),
    )
    .await
    .unwrap();

    let mut events_after_resume: Vec<Event> = Vec::new();
    let mut saw_resumed = false;
    let mut saw_step_started = false;
    let mut saw_assistant_final = false;
    let collected = timeout(Duration::from_secs(5), async {
        loop {
            let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
            let Some(event) = extract_event(&frame) else {
                continue;
            };
            match &event {
                Event::SessionResumed { .. } => saw_resumed = true,
                Event::StepStarted { .. } => saw_step_started = true,
                Event::AssistantMessage {
                    stream_finalised: true,
                    ..
                } => {
                    saw_assistant_final = true;
                    events_after_resume.push(event);
                    break;
                }
                _ => {}
            }
            events_after_resume.push(event);
        }
    })
    .await;

    assert!(collected.is_ok(), "daemon stalled after resume");
    assert!(saw_resumed, "expected SessionResumed in post-resume stream");
    assert!(
        saw_step_started,
        "no StepStarted observed after resume — orchestrator did not exit the checkpoint",
    );
    assert!(
        saw_assistant_final,
        "turn never reached an `AssistantMessage(final)` after resume",
    );
    assert!(
        !session.is_paused(),
        "Session::is_paused must be false after resume",
    );
    // SessionResumed must precede the first StepStarted.
    let resumed_idx = events_after_resume
        .iter()
        .position(|e| matches!(e, Event::SessionResumed { .. }))
        .expect("SessionResumed must appear in post-resume slice");
    let first_step_idx = events_after_resume
        .iter()
        .position(|e| matches!(e, Event::StepStarted { .. }))
        .expect("StepStarted must appear after resume");
    assert!(
        resumed_idx < first_step_idx,
        "SessionResumed must precede the first StepStarted after resume",
    );
}

/// DoD: pause-while-paused is a no-op (no second event, no error).
#[tokio::test]
async fn pause_while_paused_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let (sock_path, session) = spawn_daemon(&dir, vec![SCRIPT_SECOND.into()]).await;

    let mut stream = connect_with_retry(&sock_path).await;
    do_handshake(&mut stream).await;
    forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();

    let (mut reader, mut writer) = stream.into_split();

    // Pause without ever sending a user message — the orchestrator has
    // no parked turn to wake, but `try_pause` still flips the flag and
    // emits `SessionPaused` exactly once.
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::PauseSession(PauseSession::default()),
    )
    .await
    .unwrap();

    // Drain the first SessionPaused.
    let first = timeout(Duration::from_secs(5), async {
        loop {
            let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
            if let Some(event) = extract_event(&frame) {
                return event;
            }
        }
    })
    .await
    .expect("never observed first SessionPaused");
    assert!(matches!(first, Event::SessionPaused { .. }));
    assert!(session.is_paused());

    // Pause again — should be silently coalesced (DoD: debug log, no
    // event). We confirm by waiting for a frame that never arrives.
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::PauseSession(PauseSession::default()),
    )
    .await
    .unwrap();
    let stalled = timeout(
        Duration::from_millis(300),
        forge_ipc::read_frame(&mut reader),
    )
    .await;
    assert!(
        stalled.is_err(),
        "redundant PauseSession must not emit a second SessionPaused; got {:?}",
        stalled
            .ok()
            .and_then(|r| r.ok())
            .and_then(|f| extract_event(&f))
    );
    assert!(session.is_paused(), "still paused after redundant pause");
}

/// DoD: resume-while-running is a no-op (no second event, no error).
#[tokio::test]
async fn resume_while_running_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let (sock_path, session) = spawn_daemon(&dir, vec![SCRIPT_SECOND.into()]).await;

    let mut stream = connect_with_retry(&sock_path).await;
    do_handshake(&mut stream).await;
    forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();

    let (mut reader, mut writer) = stream.into_split();

    // Resume without ever pausing. The flag stays clear; no event fires.
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::ResumeSession(ResumeSession::default()),
    )
    .await
    .unwrap();

    let stalled = timeout(
        Duration::from_millis(300),
        forge_ipc::read_frame(&mut reader),
    )
    .await;
    assert!(
        stalled.is_err(),
        "ResumeSession on a running session must not emit; got {:?}",
        stalled
            .ok()
            .and_then(|r| r.ok())
            .and_then(|f| extract_event(&f))
    );
    assert!(!session.is_paused(), "must remain not-paused");
}

/// DoD: pause-cancel coexistence — a pause issued while a tool call is
/// awaiting client approval does NOT kill the tool. The orchestrator
/// suspends only AFTER the tool step completes; the user's approval
/// frame still routes correctly through the daemon.
///
/// This is the load-bearing tool-in-flight assertion: no mid-step kill.
#[tokio::test]
async fn pause_during_tool_approval_does_not_kill_in_flight_tool() {
    // Use the two-pass script but DON'T auto-approve so the tool call
    // sits in the approval gate.
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("test.sock");

    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(
        MockProvider::from_responses(vec![SCRIPT_FIRST.into(), SCRIPT_SECOND.into()]).unwrap(),
    );

    let server_session = Arc::clone(&session);
    let server_provider = Arc::clone(&provider);
    let server_sock = sock_path.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            server_session,
            server_provider,
            false, // auto_approve OFF — tool approval blocks
            false,
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
            text: "hello".into(),
        }),
    )
    .await
    .unwrap();

    let (mut reader, mut writer) = stream.into_split();

    // Drain until ToolCallApprovalRequested — the orchestrator is now
    // parked inside the approval oneshot, NOT at the pause checkpoint.
    let approval_id;
    loop {
        let frame = timeout(Duration::from_secs(5), forge_ipc::read_frame(&mut reader))
            .await
            .expect("daemon stalled before approval request")
            .unwrap();
        let Some(event) = extract_event(&frame) else {
            continue;
        };
        if let Event::ToolCallApprovalRequested { id, .. } = event {
            approval_id = id;
            break;
        }
    }

    // Fire PauseSession while the tool is awaiting approval. The
    // approval channel must still flow — the daemon's command loop
    // routes both `PauseSession` and `ToolCallApproved` independently.
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::PauseSession(PauseSession::default()),
    )
    .await
    .unwrap();
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::ToolCallApproved(ToolCallApproved {
            id: approval_id.to_string(),
            scope: "Once".into(),
        }),
    )
    .await
    .unwrap();

    // Expect: SessionPaused fires (immediate), ToolCallApproved /
    // ToolCallCompleted fire (in-flight tool runs to completion), then
    // the orchestrator parks at the checkpoint before opening the
    // continuation step. We DO NOT expect a continuation
    // AssistantMessage(final) until we resume.
    let mut saw_paused = false;
    let mut saw_tool_completed = false;
    let mut saw_first_final = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let Ok(frame) = timeout(remaining, forge_ipc::read_frame(&mut reader)).await else {
            break;
        };
        let frame = frame.unwrap();
        let Some(event) = extract_event(&frame) else {
            continue;
        };
        match event {
            Event::SessionPaused { .. } => saw_paused = true,
            Event::ToolCallCompleted { .. } => saw_tool_completed = true,
            Event::AssistantMessage {
                stream_finalised: true,
                ..
            } => {
                if !saw_first_final {
                    saw_first_final = true;
                } else {
                    panic!(
                        "continuation AssistantMessage(final) fired while paused — \
                         pause checkpoint missed the between-step boundary",
                    );
                }
            }
            _ => {}
        }
        if saw_paused && saw_tool_completed && saw_first_final {
            // The orchestrator drained the in-flight tool step and the
            // first model step's finalisation; it should now be parked
            // at the pause checkpoint. Stop draining and confirm.
            break;
        }
    }
    assert!(saw_paused, "SessionPaused never fired");
    assert!(
        saw_tool_completed,
        "in-flight tool was killed by pause (ToolCallCompleted never landed)",
    );
    assert!(
        saw_first_final,
        "first model step never finalised after the in-flight tool returned",
    );
    assert!(
        session.is_paused(),
        "Session::is_paused must remain true while orchestrator is parked",
    );

    // Resume and confirm the continuation completes.
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::ResumeSession(ResumeSession::default()),
    )
    .await
    .unwrap();

    let mut saw_resumed = false;
    let mut saw_second_final = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let Ok(frame) = timeout(remaining, forge_ipc::read_frame(&mut reader)).await else {
            break;
        };
        let frame = frame.unwrap();
        let Some(event) = extract_event(&frame) else {
            continue;
        };
        match event {
            Event::SessionResumed { .. } => saw_resumed = true,
            Event::AssistantMessage {
                stream_finalised: true,
                ..
            } => {
                saw_second_final = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_resumed, "SessionResumed never fired");
    assert!(
        saw_second_final,
        "continuation never completed after resume"
    );
}
