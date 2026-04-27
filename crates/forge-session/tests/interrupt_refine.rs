//! F-604: end-to-end coverage for the orchestrator interrupt + refine
//! primitive.
//!
//! These tests drive a real `serve_with_session` daemon over a UDS pair, the
//! same shape as `tests/orchestrator.rs` and `tests/pause_resume.rs`. The
//! interrupt contract under test:
//!
//! * `InterruptSession` IPC frame flips an in-memory request flag and waits
//!   for the orchestrator to publish a capture.
//! * The orchestrator polls the flag at every chunk boundary in
//!   `run_request_loop`'s stream loop and breaks out at the next clean
//!   boundary; the captured `assistant_text` reflects every
//!   `AssistantDelta` that already went on the wire.
//! * `Event::SessionInterrupted` fires once per real interrupt with the
//!   partial text and the step / message anchors.
//! * The daemon replies with `IpcMessage::RefineHandoff` carrying the same
//!   payload.
//! * Interrupt with no in-flight assistant turn is a no-op: the daemon
//!   replies with an empty handoff (`partial_text: ""`) and emits no
//!   `SessionInterrupted` event.
//! * The session stays alive after interrupt; the next `SendUserMessage`
//!   continues normally.

use forge_core::Event;
use forge_ipc::{
    ClientInfo, Hello, InterruptSession, IpcEvent, IpcMessage, SendUserMessage, Subscribe,
    PROTO_VERSION,
};
use forge_providers::MockProvider;
use forge_session::{server::serve_with_session, session::Session};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::{timeout, Duration};

/// The mock provider's "deltas-only" stream — many small text deltas
/// followed by a `Done`. Long enough to give the test a reasonable
/// window between the first delta and the stream's close in which to
/// fire the interrupt request before the loop naturally exits.
const SCRIPT_LONG_DELTAS: &str = r#"{"delta":"chunk one "}
{"delta":"chunk two "}
{"delta":"chunk three "}
{"delta":"chunk four "}
{"delta":"chunk five "}
{"delta":"chunk six "}
{"delta":"chunk seven "}
{"delta":"chunk eight "}
{"delta":"chunk nine "}
{"delta":"chunk ten"}
{"done":"end_turn"}
"#;

/// Short single-pass script for the post-interrupt continuation test.
const SCRIPT_SHORT: &str = r#"{"delta":"second turn body"}
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
            true, // auto_approve — these tests never sit in the approval gate.
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

/// DoD: interrupt mid-stream captures the partial text.
///
/// Strategy: the mock provider script emits many small deltas. We
/// drain events until at least one `AssistantDelta` lands (so we know
/// the orchestrator is mid-stream), then fire `InterruptSession`. The
/// daemon's response (`RefineHandoff`) and the `SessionInterrupted`
/// event must both carry the captured partial. The captured text must
/// be a non-strict prefix of the script's full body (some deltas may
/// land before the interrupt observes the flag, depending on
/// scheduling).
#[tokio::test]
async fn interrupt_mid_stream_captures_partial_text() {
    let dir = TempDir::new().unwrap();
    let (sock_path, _session) = spawn_daemon(&dir, vec![SCRIPT_LONG_DELTAS.into()]).await;

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

    // Drain until the first AssistantDelta — proves the orchestrator
    // is mid-stream so the interrupt can land at a chunk boundary
    // rather than racing the stream's natural `Done`.
    let mut saw_first_delta = false;
    while !saw_first_delta {
        let frame = timeout(Duration::from_secs(5), forge_ipc::read_frame(&mut reader))
            .await
            .expect("daemon stalled before first AssistantDelta")
            .unwrap();
        if matches!(extract_event(&frame), Some(Event::AssistantDelta { .. })) {
            saw_first_delta = true;
        }
    }

    // Fire the interrupt. The daemon replies on the same connection
    // with `IpcMessage::RefineHandoff`. We need to drain
    // `IpcMessage::Event(...)` frames in the meantime — the daemon
    // multiplexes events and the response on one socket, like the
    // MCP responses do.
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::InterruptSession(InterruptSession::default()),
    )
    .await
    .unwrap();

    let mut interrupted_event: Option<Event> = None;
    let mut handoff: Option<forge_ipc::RefineHandoff> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let Ok(frame) = timeout(remaining, forge_ipc::read_frame(&mut reader)).await else {
            break;
        };
        match frame.unwrap() {
            IpcMessage::Event(IpcEvent { event, .. }) => {
                if matches!(event, Event::SessionInterrupted { .. }) {
                    interrupted_event = Some(event);
                }
            }
            IpcMessage::RefineHandoff(h) => {
                handoff = Some(h);
            }
            _ => {}
        }
        if interrupted_event.is_some() && handoff.is_some() {
            break;
        }
    }

    let event = interrupted_event.expect("SessionInterrupted event never landed");
    let handoff = handoff.expect("RefineHandoff response never arrived");

    let (event_partial, event_step, event_msg) = match event {
        Event::SessionInterrupted {
            partial_text,
            captured_at_step_id,
            captured_at_msg_id,
            ..
        } => (
            partial_text.to_string(),
            captured_at_step_id.to_string(),
            captured_at_msg_id.to_string(),
        ),
        other => panic!("expected SessionInterrupted, got {other:?}"),
    };

    // The captured partial must be non-empty (we waited for at least
    // one delta) and must be a prefix of the full scripted body.
    assert!(
        !event_partial.is_empty(),
        "captured partial_text must be non-empty after at least one delta"
    );
    let full_body = "chunk one chunk two chunk three chunk four chunk five \
                     chunk six chunk seven chunk eight chunk nine chunk ten";
    assert!(
        full_body.starts_with(&event_partial),
        "captured partial_text must be a prefix of the full script body; \
         partial={event_partial:?}, full={full_body:?}",
    );

    // The IPC handoff and the event payload must agree exactly.
    assert_eq!(handoff.partial_text, event_partial);
    assert_eq!(handoff.captured_at_step_id, event_step);
    assert_eq!(handoff.captured_at_msg_id, event_msg);

    // Anchors must be non-empty (a real turn was in flight).
    assert!(
        !handoff.captured_at_step_id.is_empty(),
        "captured_at_step_id must be populated"
    );
    assert!(
        !handoff.captured_at_msg_id.is_empty(),
        "captured_at_msg_id must be populated"
    );
}

/// DoD: interrupt with no in-flight assistant turn is a no-op.
///
/// Strategy: never send a `SendUserMessage` — fire `InterruptSession`
/// directly after the handshake. The daemon's IPC handler bounds its
/// wait on the orchestrator's capture publication; with no
/// orchestrator turn to observe the flag, the deadline expires and
/// the handler replies with the empty handoff shape. No
/// `SessionInterrupted` event must fire.
#[tokio::test]
async fn interrupt_with_no_in_flight_turn_is_a_no_op() {
    let dir = TempDir::new().unwrap();
    let (sock_path, session) = spawn_daemon(&dir, vec![SCRIPT_SHORT.into()]).await;

    let mut stream = connect_with_retry(&sock_path).await;
    do_handshake(&mut stream).await;
    forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();

    let (mut reader, mut writer) = stream.into_split();

    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::InterruptSession(InterruptSession::default()),
    )
    .await
    .unwrap();

    // The handler awaits a capture publication for up to 5s; we expect
    // the deadline path on the daemon side and the empty handoff
    // response. Cap the test deadline at 10s so the deadline path has
    // headroom.
    let mut handoff: Option<forge_ipc::RefineHandoff> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut events_seen: Vec<Event> = Vec::new();
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let Ok(frame) = timeout(remaining, forge_ipc::read_frame(&mut reader)).await else {
            break;
        };
        match frame.unwrap() {
            IpcMessage::Event(IpcEvent { event, .. }) => events_seen.push(event),
            IpcMessage::RefineHandoff(h) => {
                handoff = Some(h);
                break;
            }
            _ => {}
        }
    }

    let h = handoff.expect("daemon must reply with RefineHandoff even on no-op interrupt");
    assert_eq!(
        h.partial_text, "",
        "no-op handoff must carry empty partial_text"
    );
    assert_eq!(
        h.captured_at_step_id, "",
        "no-op handoff must carry empty step anchor"
    );
    assert_eq!(
        h.captured_at_msg_id, "",
        "no-op handoff must carry empty message anchor"
    );

    assert!(
        !events_seen
            .iter()
            .any(|e| matches!(e, Event::SessionInterrupted { .. })),
        "no SessionInterrupted event must fire on a no-op interrupt; saw {events_seen:?}",
    );

    // Critical: the no-op path cleared the request flag so a subsequent
    // turn isn't short-circuited by a stale request.
    assert!(
        !session.is_interrupt_requested(),
        "no-op interrupt must clear the request flag"
    );
}

/// DoD: interrupt + new SendUserMessage continues normally.
///
/// Strategy: drive a mid-stream interrupt against the long-deltas
/// script as in the first test, drain the handoff + event, then send
/// a fresh `SendUserMessage` and verify the second turn streams to
/// completion. The provider replies with the short script for the
/// second turn (the mock provider iterates through the supplied
/// responses one per call).
#[tokio::test]
async fn session_continues_after_interrupt_with_next_user_message() {
    let dir = TempDir::new().unwrap();
    let (sock_path, _session) =
        spawn_daemon(&dir, vec![SCRIPT_LONG_DELTAS.into(), SCRIPT_SHORT.into()]).await;

    let mut stream = connect_with_retry(&sock_path).await;
    do_handshake(&mut stream).await;
    forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();
    forge_ipc::write_frame(
        &mut stream,
        &IpcMessage::SendUserMessage(SendUserMessage {
            text: "first".into(),
        }),
    )
    .await
    .unwrap();

    let (mut reader, mut writer) = stream.into_split();

    // Wait for the first delta of turn 1 so we know the stream is open.
    loop {
        let frame = timeout(Duration::from_secs(5), forge_ipc::read_frame(&mut reader))
            .await
            .expect("turn 1: daemon stalled")
            .unwrap();
        if matches!(extract_event(&frame), Some(Event::AssistantDelta { .. })) {
            break;
        }
    }

    // Interrupt turn 1.
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::InterruptSession(InterruptSession::default()),
    )
    .await
    .unwrap();

    // Drain until both `SessionInterrupted` and the `RefineHandoff`
    // reply have landed.
    let mut saw_interrupted = false;
    let mut saw_handoff = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && !(saw_interrupted && saw_handoff) {
        let remaining = deadline - tokio::time::Instant::now();
        let Ok(frame) = timeout(remaining, forge_ipc::read_frame(&mut reader)).await else {
            break;
        };
        match frame.unwrap() {
            IpcMessage::Event(IpcEvent { event, .. }) => {
                if matches!(event, Event::SessionInterrupted { .. }) {
                    saw_interrupted = true;
                }
            }
            IpcMessage::RefineHandoff(_) => {
                saw_handoff = true;
            }
            _ => {}
        }
    }
    assert!(saw_interrupted, "turn 1 must emit SessionInterrupted");
    assert!(saw_handoff, "turn 1 must reply with RefineHandoff");

    // Send a second user message — the session must accept it and the
    // orchestrator must drive a fresh turn to completion.
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::SendUserMessage(SendUserMessage {
            text: "second".into(),
        }),
    )
    .await
    .unwrap();

    // Drain until we see a non-streaming `AssistantMessage(final)` for
    // the second turn (text == "second turn body" per SCRIPT_SHORT).
    let mut saw_second_final = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let Ok(frame) = timeout(remaining, forge_ipc::read_frame(&mut reader)).await else {
            break;
        };
        if let Some(Event::AssistantMessage {
            stream_finalised: true,
            text,
            ..
        }) = extract_event(&frame.unwrap())
        {
            // The first turn also emitted an `AssistantMessage(final)`
            // with the partial — we want the *second* turn's, which
            // has the SCRIPT_SHORT body.
            if text.as_ref() == "second turn body" {
                saw_second_final = true;
                break;
            }
        }
    }
    assert!(
        saw_second_final,
        "second turn never reached its AssistantMessage(final) — session did not continue"
    );
}

/// DoD: cancel-after-interrupt still terminates cleanly.
///
/// `session_cancel` is currently a Tauri-side stub (no
/// `IpcMessage::CancelTurn` exists yet, per the comment on
/// `session_cancel` in `crates/forge-shell/src/ipc.rs`). The closest
/// honest assertion against the contract is: an interrupted session
/// must still respond to ordinary lifecycle teardown — i.e. closing
/// the connection (the local equivalent of "cancel") does not panic
/// the daemon, and a fresh connection to the same daemon still
/// works. We exercise that here.
#[tokio::test]
async fn cancel_via_disconnect_after_interrupt_terminates_cleanly() {
    let dir = TempDir::new().unwrap();
    let (sock_path, _session) =
        spawn_daemon(&dir, vec![SCRIPT_LONG_DELTAS.into(), SCRIPT_SHORT.into()]).await;

    // First connection: send, interrupt, drop.
    {
        let mut stream = connect_with_retry(&sock_path).await;
        do_handshake(&mut stream).await;
        forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
            .await
            .unwrap();
        forge_ipc::write_frame(
            &mut stream,
            &IpcMessage::SendUserMessage(SendUserMessage {
                text: "first".into(),
            }),
        )
        .await
        .unwrap();

        let (mut reader, mut writer) = stream.into_split();
        loop {
            let frame = timeout(Duration::from_secs(5), forge_ipc::read_frame(&mut reader))
                .await
                .expect("daemon stalled")
                .unwrap();
            if matches!(extract_event(&frame), Some(Event::AssistantDelta { .. })) {
                break;
            }
        }

        forge_ipc::write_frame(
            &mut writer,
            &IpcMessage::InterruptSession(InterruptSession::default()),
        )
        .await
        .unwrap();

        // Wait for the handoff before dropping.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut got_handoff = false;
        while tokio::time::Instant::now() < deadline && !got_handoff {
            let remaining = deadline - tokio::time::Instant::now();
            let Ok(frame) = timeout(remaining, forge_ipc::read_frame(&mut reader)).await else {
                break;
            };
            if matches!(frame.unwrap(), IpcMessage::RefineHandoff(_)) {
                got_handoff = true;
            }
        }
        assert!(got_handoff, "first connection must receive the handoff");
        // Drop the stream — this is the connection-close "cancel"
        // analogue today. The daemon must not panic.
    }

    // Second connection: the daemon must still accept new clients.
    let mut stream = connect_with_retry(&sock_path).await;
    do_handshake(&mut stream).await;
    forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();
    forge_ipc::write_frame(
        &mut stream,
        &IpcMessage::SendUserMessage(SendUserMessage {
            text: "second".into(),
        }),
    )
    .await
    .unwrap();

    let mut saw_second_final = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let Ok(frame) = timeout(remaining, forge_ipc::read_frame(&mut stream)).await else {
            break;
        };
        if let Some(Event::AssistantMessage {
            stream_finalised: true,
            text,
            ..
        }) = extract_event(&frame.unwrap())
        {
            if text.as_ref() == "second turn body" {
                saw_second_final = true;
                break;
            }
        }
    }
    assert!(
        saw_second_final,
        "daemon must accept new clients after a connection-drop following interrupt"
    );
}
