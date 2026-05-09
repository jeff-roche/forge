#![allow(deprecated)] // F-652: tests/benches still drive the deprecated bare read_frame helpers.
//! F-144: Re-run Fresh variant — truncate to the originating user message,
//! regenerate from there (new root), and supersede the original turn.
//!
//! Scenario:
//!   1. Client drives two user→assistant turns. Turn 1 asks a question and
//!      gets answer A. Turn 2 asks a follow-up and gets answer B.
//!   2. Client fires `RerunMessage { msg_id: B, variant: Fresh }`. Fresh
//!      semantics (CONCEPT.md §10.3): regenerate from turn 2's user message
//!      *alone* — intermediate tool calls and prior turns are discarded.
//!      The mock provider's second response (`SCRIPT_FRESH`) is asserted
//!      to have been dispatched a request containing exactly one user
//!      message with turn 2's text.
//!   3. `MessageSuperseded { old_id: B, new_id: B' }` hides the original B
//!      in replay; B' is a new root (`branch_parent = None`).

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

const SCRIPT_TURN1: &str = r#"{"delta":"answer one"}
{"done":"end_turn"}
"#;

const SCRIPT_TURN2: &str = r#"{"delta":"answer two"}
{"done":"end_turn"}
"#;

const SCRIPT_FRESH: &str = r#"{"delta":"fresh answer"}
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
    assert!(matches!(response, IpcMessage::HelloAck(_)));
}

fn extract_event(msg: &IpcMessage) -> Option<Event> {
    if let IpcMessage::Event(IpcEvent { event, .. }) = msg {
        Some(event.clone())
    } else {
        None
    }
}

async fn drive_turn(
    reader: &mut tokio::net::unix::OwnedReadHalf,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    text: &str,
) -> MessageId {
    forge_ipc::write_frame(
        writer,
        &IpcMessage::SendUserMessage(SendUserMessage { text: text.into() }),
    )
    .await
    .unwrap();
    loop {
        let frame = forge_ipc::read_frame(reader).await.unwrap();
        let Some(event) = extract_event(&frame) else {
            continue;
        };
        if let Event::AssistantMessage {
            id,
            stream_finalised: true,
            ..
        } = event
        {
            return id;
        }
    }
}

#[tokio::test]
async fn rerun_fresh_regenerates_from_user_message_root_and_supersedes_original() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("rerun-fresh.sock");

    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(
        MockProvider::from_responses(vec![
            SCRIPT_TURN1.into(),
            SCRIPT_TURN2.into(),
            SCRIPT_FRESH.into(),
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
        )
        .await
        .unwrap();
    });

    let mut stream = connect_with_retry(&sock_path).await;
    do_handshake(&mut stream).await;
    forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();

    let (mut reader, mut writer) = stream.into_split();
    let _turn1_id = drive_turn(&mut reader, &mut writer, "first question").await;
    let turn2_id = drive_turn(&mut reader, &mut writer, "follow up question").await;

    // ── Fresh rerun on turn 2 ──────────────────────────────────────────
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::RerunMessage(RerunMessage {
            msg_id: turn2_id.to_string(),
            variant: RerunVariant::Fresh,
        }),
    )
    .await
    .unwrap();

    // Drain until we see the supersede marker.
    let fresh_id: MessageId = loop {
        let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
        let Some(event) = extract_event(&frame) else {
            continue;
        };
        if let Event::MessageSuperseded { old_id, new_id } = &event {
            assert_eq!(
                old_id, &turn2_id,
                "Fresh must supersede the targeted turn-2 assistant message"
            );
            break new_id.clone();
        }
    };

    // The regenerated message must be a new root (branch_parent: None,
    // branch_variant_index: 0) — this is the "new root" criterion in the DoD.
    let events = provider.recorded_requests();
    // Three requests dispatched: turn 1, turn 2, fresh-rerun.
    assert_eq!(
        events.len(),
        3,
        "Fresh must dispatch a new provider request (turn1, turn2, fresh)"
    );

    // The fresh request MUST contain exactly one message — the originating
    // user message for turn 2. Prior turns are discarded (this is what
    // distinguishes Fresh from Replace). The assertion on content verifies
    // turn 2's user text, not turn 1's.
    let fresh_req = &events[2];
    assert_eq!(
        fresh_req.messages.len(),
        1,
        "Fresh request must have exactly one message (the originating user turn only)"
    );
    let msg = &fresh_req.messages[0];
    assert!(
        matches!(msg.role, forge_providers::ChatRole::User),
        "Fresh's single message must be a user message"
    );
    let text_ok = msg
        .content
        .iter()
        .any(|b| matches!(b, forge_providers::ChatBlock::Text(t) if t == "follow up question"));
    assert!(
        text_ok,
        "Fresh must regenerate from turn 2's user message verbatim, got: {:?}",
        msg.content
    );

    drop(reader);
    drop(writer);

    // ── Fresh subscriber: original turn-2 assistant is hidden, new one visible ──
    let mut stream2 = connect_with_retry(&sock_path).await;
    do_handshake(&mut stream2).await;
    forge_ipc::write_frame(&mut stream2, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();

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

    let superseded_present = replay.iter().any(|e| {
        matches!(
            e,
            Event::AssistantMessage { id, stream_finalised: true, .. } if *id == turn2_id
        )
    });
    assert!(
        !superseded_present,
        "Fresh must supersede the original turn-2 assistant message on replay"
    );

    let regenerated = replay
        .iter()
        .find(|e| {
            matches!(
                e,
                Event::AssistantMessage { id, stream_finalised: true, .. } if *id == fresh_id
            )
        })
        .expect("regenerated Fresh assistant message must appear in replay");

    if let Event::AssistantMessage {
        branch_parent,
        branch_variant_index,
        ..
    } = regenerated
    {
        assert_eq!(
            *branch_parent, None,
            "Fresh regeneration must be a new root (branch_parent = None)"
        );
        assert_eq!(
            *branch_variant_index, 0,
            "Fresh regeneration must have branch_variant_index = 0 (root position)"
        );
    }
}
