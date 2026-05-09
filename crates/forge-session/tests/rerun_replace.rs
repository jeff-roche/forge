#![allow(deprecated)] // F-652: tests/benches still drive the deprecated bare read_frame helpers.
//! F-143: Re-run Replace variant — truncate and regenerate.
//!
//! Scenario:
//!   1. Client sends a user message; server produces an assistant response A.
//!   2. Client sends `RerunMessage { msg_id: A, variant: Replace }`.
//!   3. Server regenerates (second provider script). A new assistant message A'
//!      is appended. `MessageSuperseded { old_id: A, new_id: A' }` is emitted.
//!   4. A **fresh** client connection subscribes `since: 0`. The replay must
//!      include only A' — the superseded A (and its deltas) must not surface.
//!
//! The assertion is behavioural: replay of `read_since` is filtered through
//! `forge_core::apply_superseded` so a late-joining subscriber sees a
//! coherent transcript with no stale assistant message.

use forge_core::{ids::MessageId, Event, RerunVariant};
use forge_ipc::{
    ClientInfo, Hello, IpcEvent, IpcMessage, RerunMessage, SendUserMessage, Subscribe,
    PROTO_VERSION,
};
use forge_providers::MockProvider;
use forge_session::{server::serve_with_session, session::Session};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::net::UnixStream;

const SCRIPT_FIRST: &str = r#"{"delta":"original answer"}
{"done":"end_turn"}
"#;

const SCRIPT_RERUN: &str = r#"{"delta":"regenerated answer"}
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
        "expected HelloAck, got {response:?}"
    );
}

fn extract_event(msg: &IpcMessage) -> Option<Event> {
    if let IpcMessage::Event(IpcEvent { event, .. }) = msg {
        Some(event.clone())
    } else {
        None
    }
}

#[tokio::test]
async fn rerun_replace_supersedes_original_message_in_fresh_replay() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("rerun.sock");

    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(
        MockProvider::from_responses(vec![SCRIPT_FIRST.into(), SCRIPT_RERUN.into()]).unwrap(),
    );

    let server_session = Arc::clone(&session);
    let server_provider = Arc::clone(&provider);
    let server_sock = sock_path.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            server_session,
            server_provider,
            true, // auto-approve — irrelevant here (no tool calls)
            false,
            None,
            None,
            None, // F-587: keyless test wiring
            None, // F-601: no active agent — memory off in this test
        )
        .await
        .unwrap();
    });

    // ── First client connection: drive the initial turn, capture A's id ──
    let mut stream = connect_with_retry(&sock_path).await;
    do_handshake(&mut stream).await;
    forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();
    forge_ipc::write_frame(
        &mut stream,
        &IpcMessage::SendUserMessage(SendUserMessage { text: "ask".into() }),
    )
    .await
    .unwrap();

    let (mut reader, mut writer) = stream.into_split();
    let original_msg_id: MessageId = loop {
        let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
        let Some(event) = extract_event(&frame) else {
            continue;
        };
        if let Event::AssistantMessage {
            id,
            stream_finalised: true,
            ..
        } = event
        {
            break id;
        }
    };

    // Now fire the rerun-replace command over the same connection.
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::RerunMessage(RerunMessage {
            msg_id: original_msg_id.to_string(),
            variant: RerunVariant::Replace,
        }),
    )
    .await
    .unwrap();

    // Drain until we see the MessageSuperseded marker for our original id.
    let new_msg_id: MessageId = loop {
        let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
        let Some(event) = extract_event(&frame) else {
            continue;
        };
        if let Event::MessageSuperseded { old_id, new_id } = &event {
            assert_eq!(
                old_id, &original_msg_id,
                "superseded marker must reference the original id"
            );
            break new_id.clone();
        }
    };
    assert_ne!(
        new_msg_id, original_msg_id,
        "regenerated message must get a fresh id"
    );

    // Close the first connection.
    drop(reader);
    drop(writer);

    // ── Second client connection: fresh subscribe + replay ──
    let mut stream2 = connect_with_retry(&sock_path).await;
    do_handshake(&mut stream2).await;
    forge_ipc::write_frame(&mut stream2, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();

    // Read until we've consumed the full replay (heuristic: after a short
    // idle period with no new frames we assume history is drained).
    let mut replay: Vec<Event> = Vec::new();
    let (mut reader2, _writer2) = stream2.into_split();
    while let Ok(Ok(frame)) = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        forge_ipc::read_frame(&mut reader2),
    )
    .await
    {
        if let Some(ev) = extract_event(&frame) {
            replay.push(ev);
        }
    }

    // Assertion 1: the original assistant message must NOT surface.
    let original_assistant_present = replay.iter().any(|e| {
        matches!(
            e,
            Event::AssistantMessage {
                id,
                stream_finalised: true,
                ..
            } if *id == original_msg_id
        )
    });
    assert!(
        !original_assistant_present,
        "fresh replay must hide the superseded AssistantMessage; saw: {:?}",
        replay
            .iter()
            .filter_map(|e| match e {
                Event::AssistantMessage {
                    id,
                    stream_finalised,
                    ..
                } => Some(format!(
                    "AssistantMessage({id}, finalised={stream_finalised})"
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
    );

    // Assertion 2: the regenerated assistant message MUST surface.
    let regenerated_present = replay.iter().any(|e| {
        matches!(
            e,
            Event::AssistantMessage {
                id,
                stream_finalised: true,
                ..
            } if *id == new_msg_id
        )
    });
    assert!(
        regenerated_present,
        "fresh replay must include the regenerated AssistantMessage"
    );

    // Assertion 3: superseded deltas are also hidden — no AssistantDelta
    // with id == original_msg_id should appear in the replay.
    let old_deltas = replay
        .iter()
        .filter(|e| matches!(e, Event::AssistantDelta { id, .. } if *id == original_msg_id))
        .count();
    assert_eq!(
        old_deltas, 0,
        "superseded AssistantDelta events must be filtered out on replay"
    );
}
