#![allow(deprecated)] // F-652: tests/benches still drive the deprecated bare read_frame helpers.
//! F-144: Re-run Branch variant — both versions co-exist, BranchSelected gates display.
//!
//! Scenario:
//!   1. Client drives a user→assistant turn, capturing the original message id `A`.
//!   2. Client fires `RerunMessage { msg_id: A, variant: Branch }`. The daemon
//!      regenerates and emits a second `AssistantMessage` with
//!      `branch_parent = Some(A)` and `branch_variant_index = 1`. No
//!      `MessageSuperseded` marker — both versions co-exist.
//!   3. Client fires `SelectBranch { parent: A, variant_index: 1 }`; daemon
//!      emits `BranchSelected { parent: A, selected: A_branch }`.
//!   4. A fresh subscriber sees both `AssistantMessage`s in the replay and a
//!      trailing `BranchSelected` event tying them together.
//!
//! The invariant: Branch is the *only* rerun shape where both versions stay
//! visible. Tool-call filtering limitations documented on `apply_superseded`
//! are out of scope for this happy-path coverage.

use forge_core::{ids::MessageId, Event, RerunVariant};
use forge_ipc::{
    ClientInfo, Hello, IpcEvent, IpcMessage, RerunMessage, SelectBranch, SendUserMessage,
    Subscribe, PROTO_VERSION,
};
use forge_providers::MockProvider;
use forge_session::{server::serve_with_session, session::Session};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::net::UnixStream;

const SCRIPT_FIRST: &str = r#"{"delta":"original answer"}
{"done":"end_turn"}
"#;

const SCRIPT_BRANCH: &str = r#"{"delta":"branch answer"}
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
async fn rerun_branch_keeps_both_variants_and_branch_selected_gates_display() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("rerun-branch.sock");

    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(
        MockProvider::from_responses(vec![SCRIPT_FIRST.into(), SCRIPT_BRANCH.into()]).unwrap(),
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

    // ── Turn 1: original assistant response ───────────────────────────
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
            branch_parent,
            branch_variant_index,
            ..
        } = event
        {
            // Sanity: first turn is a root, not a branch.
            assert_eq!(
                branch_parent, None,
                "first assistant message must be a branch root"
            );
            assert_eq!(branch_variant_index, 0);
            break id;
        }
    };

    // ── Branch re-run on the same connection ──────────────────────────
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::RerunMessage(RerunMessage {
            msg_id: original_msg_id.to_string(),
            variant: RerunVariant::Branch,
        }),
    )
    .await
    .unwrap();

    // Drain until we see the finalised Branch variant (distinguished by
    // branch_parent == Some(original)).
    let branch_msg_id: MessageId = loop {
        let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
        let Some(event) = extract_event(&frame) else {
            continue;
        };
        if let Event::AssistantMessage {
            id,
            stream_finalised: true,
            branch_parent: Some(parent),
            branch_variant_index,
            ..
        } = &event
        {
            assert_eq!(
                parent, &original_msg_id,
                "branch variant must point at the original as its root"
            );
            assert_eq!(
                *branch_variant_index, 1,
                "first branch re-run must land at variant_index 1"
            );
            break id.clone();
        }
    };
    assert_ne!(
        branch_msg_id, original_msg_id,
        "variant must have a fresh id"
    );

    // ── select_branch: activate the sibling ───────────────────────────
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::SelectBranch(SelectBranch {
            parent_id: original_msg_id.to_string(),
            variant_index: 1,
        }),
    )
    .await
    .unwrap();

    // Drain until we see the BranchSelected event.
    loop {
        let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
        let Some(event) = extract_event(&frame) else {
            continue;
        };
        if let Event::BranchSelected { parent, selected } = &event {
            assert_eq!(parent, &original_msg_id);
            assert_eq!(
                selected, &branch_msg_id,
                "BranchSelected must resolve variant_index=1 to the branch message id"
            );
            break;
        }
    }

    drop(reader);
    drop(writer);

    // ── Fresh subscriber: both variants MUST replay + BranchSelected ──
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

    let original_present = replay.iter().any(|e| {
        matches!(
            e,
            Event::AssistantMessage { id, stream_finalised: true, .. } if *id == original_msg_id
        )
    });
    assert!(
        original_present,
        "fresh replay must keep the original variant visible — Branch does not supersede"
    );

    let branch_present = replay.iter().any(|e| {
        matches!(
            e,
            Event::AssistantMessage { id, stream_finalised: true, branch_parent: Some(p), .. }
                if *id == branch_msg_id && *p == original_msg_id
        )
    });
    assert!(
        branch_present,
        "fresh replay must include the Branch variant with branch_parent threaded"
    );

    let selected_present = replay.iter().any(|e| {
        matches!(
            e,
            Event::BranchSelected { parent, selected }
                if *parent == original_msg_id && *selected == branch_msg_id
        )
    });
    assert!(
        selected_present,
        "fresh replay must include BranchSelected so the UI knows which variant to display"
    );
}

#[tokio::test]
async fn select_branch_with_variant_zero_resolves_to_parent() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("select-root.sock");

    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(MockProvider::from_responses(vec![SCRIPT_FIRST.into()]).unwrap());

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

    // variant_index=0 resolves to parent itself.
    forge_ipc::write_frame(
        &mut writer,
        &IpcMessage::SelectBranch(SelectBranch {
            parent_id: original_msg_id.to_string(),
            variant_index: 0,
        }),
    )
    .await
    .unwrap();

    loop {
        let frame = forge_ipc::read_frame(&mut reader).await.unwrap();
        let Some(event) = extract_event(&frame) else {
            continue;
        };
        if let Event::BranchSelected { parent, selected } = &event {
            assert_eq!(parent, &original_msg_id);
            assert_eq!(
                selected, &original_msg_id,
                "variant_index=0 must resolve to the root message itself"
            );
            break;
        }
    }
}
