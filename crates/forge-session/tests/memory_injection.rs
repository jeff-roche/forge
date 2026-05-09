#![allow(deprecated)] // F-652: tests/benches still drive the deprecated bare read_frame helpers.
//! F-601: integration tests for cross-session memory injection.
//!
//! These tests cover the seam that `serve_with_session` uses — they
//! exercise the public `forge_agents` API (`MemoryStore` + frontmatter +
//! `assemble_system_prompt`) that the server composes once per session,
//! plus a daemon-level smoke test that drives `serve_with_session` with
//! the typed `active_agent` parameter (replacing the prior
//! `FORGE_ACTIVE_AGENT` env-var indirection — see
//! `crates/forge-session/src/server.rs::tests::concurrent_sessions_with_different_active_agents_get_their_own_memory`
//! for the regression test that pins the multi-session correctness
//! invariant).
//!
//! End-to-end coverage of system-prompt assembly is exercised by the
//! pure `assemble_system_prompt` tests below and the orchestrator-level
//! AGENTS.md test, which is the precedent for per-turn system-prompt
//! assertions.

use std::sync::Arc;

use forge_agents::{
    assemble_system_prompt, Memory, MemoryFrontmatter, MemoryStore, WriteMode,
    MEMORY_ENVELOPE_CLOSE, MEMORY_HEADING,
};
use forge_ipc::{ClientInfo, Hello, IpcMessage, PROTO_VERSION};
use forge_providers::MockProvider;
use forge_session::{server::serve_with_session, session::Session};
use tempfile::tempdir;
use tokio::net::UnixStream;

#[test]
fn memory_body_injects_after_agents_md_under_memory_heading() {
    let agents_md_prefix = "\n\n---\nAGENTS.md (workspace):\nworkspace rules";
    let memory_body = "remember: ship Phase 3 by Friday";

    let assembled = assemble_system_prompt(Some(agents_md_prefix), Some(memory_body))
        .expect("both inputs present must yield Some");

    let agents_idx = assembled
        .find("AGENTS.md (workspace):")
        .expect("AGENTS.md label must appear");
    let mem_idx = assembled
        .find("## Memory")
        .expect("Memory heading must appear");
    assert!(
        agents_idx < mem_idx,
        "AGENTS.md must precede Memory heading; got: {assembled}"
    );
    assert!(
        assembled.contains(memory_body),
        "memory body must appear in the prompt; got: {assembled}"
    );
    // F-650: the envelope close tag is the final structural element.
    assert!(
        assembled.ends_with(MEMORY_ENVELOPE_CLOSE),
        "envelope close must be last; got: {assembled}"
    );
    assert!(
        assembled.contains(MEMORY_HEADING.trim()),
        "MEMORY_HEADING must be present"
    );
}

#[test]
fn memory_alone_still_uses_memory_heading() {
    let memory_body = "no agents.md, just memory";
    let assembled = assemble_system_prompt(None, Some(memory_body)).unwrap();
    assert!(assembled.contains("## Memory"));
    assert!(assembled.contains(memory_body));
    assert!(assembled.ends_with(MEMORY_ENVELOPE_CLOSE));
}

#[test]
fn no_memory_no_agents_md_yields_none() {
    assert!(assemble_system_prompt(None, None).is_none());
}

#[test]
fn write_then_read_round_trips_for_a_named_agent() {
    let dir = tempdir().unwrap();
    let store = MemoryStore::new(dir.path());
    let result = store
        .write("scribe", "phase 3 plan", WriteMode::Append)
        .unwrap();
    assert_eq!(result.frontmatter.version, 1);

    // A second session of the same agent observes the prior write.
    let reread = store.load("scribe").unwrap().unwrap();
    assert_eq!(reread.body, "phase 3 plan");
    assert_eq!(reread.frontmatter.version, 1);
}

#[test]
fn missing_file_does_not_crash_load_path() {
    let dir = tempdir().unwrap();
    let store = MemoryStore::new(dir.path());
    assert!(store.load("never-written").unwrap().is_none());
}

#[test]
fn explicit_save_with_chosen_metadata_round_trips() {
    let dir = tempdir().unwrap();
    let store = MemoryStore::new(dir.path());
    let memory = Memory {
        frontmatter: MemoryFrontmatter {
            updated_at: chrono::Utc::now(),
            version: 42,
        },
        body: "frozen body".to_string(),
    };
    store.save("scribe", &memory).unwrap();

    let reread = store.load("scribe").unwrap().unwrap();
    assert_eq!(reread.frontmatter.version, 42);
    assert_eq!(reread.body, "frozen body");
}

/// F-601: daemon-level smoke test that `serve_with_session` accepts the
/// typed `active_agent` parameter and a connection completes the
/// handshake without crashing.
///
/// The new typed parameter replaces the prior `FORGE_ACTIVE_AGENT`
/// env-var indirection. This test pins that the parameter is plumbed
/// through the public surface — no env var is set or read by this test.
/// The deeper "two sessions in the same process get their own memory
/// body" invariant is covered by the unit test in `server.rs`.
#[tokio::test]
async fn serve_with_session_accepts_typed_active_agent_parameter() {
    let dir = tempdir().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("memory.sock");
    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(MockProvider::with_default_path());

    let server_sock = sock_path.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            session,
            provider,
            false,
            false,
            None,
            None,
            None, // F-587: keyless test wiring
            // F-601: typed `active_agent` exercised here. Memory is off
            // because no agent runtime is loaded under this tempdir
            // workspace — the rules in `resolve_session_memory` quietly
            // disable memory and the session continues normally.
            Some("scribe".to_string()),
        )
        .await
        .unwrap();
    });

    // Connect with a small retry loop; the server task races our connect.
    let mut stream = {
        let mut last_err = None;
        let mut connected = None;
        for _ in 0..20 {
            match UnixStream::connect(&sock_path).await {
                Ok(s) => {
                    connected = Some(s);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        }
        connected.unwrap_or_else(|| {
            panic!(
                "server did not start in time: {}",
                last_err.expect("at least one connect error")
            )
        })
    };

    let hello = IpcMessage::Hello(Hello {
        proto: PROTO_VERSION,
        client: ClientInfo {
            kind: "shell".into(),
            pid: std::process::id(),
            user: "test-user".into(),
        },
        schema_version: forge_ipc::SCHEMA_VERSION,
    });
    forge_ipc::write_frame(&mut stream, &hello).await.unwrap();

    let response = forge_ipc::read_frame(&mut stream).await.unwrap();
    let IpcMessage::HelloAck(ack) = response else {
        panic!("expected HelloAck, got {response:?}");
    };
    assert_eq!(ack.schema_version, 1);
    assert!(!ack.session_id.is_empty());
}
