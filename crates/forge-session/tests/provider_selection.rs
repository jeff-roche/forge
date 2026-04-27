//! Integration tests for `forged --provider <spec>` dispatch (F-038).
//!
//! Two scenarios:
//!
//! 1. `ollama_dispatch_surfaces_connection_error_at_unreachable_port` — proves
//!    the daemon actually constructs an OllamaProvider when `--provider` is
//!    passed (vs. silently falling back to MockProvider). We point it at an
//!    unreachable URL and assert a `SessionEnded { reason: Error(..) }` whose
//!    text mentions ollama. Runs in CI; no real Ollama needed.
//!
//! 2. `ollama_round_trip_against_local_qwen` — `#[ignore]`-gated. Requires a
//!    local Ollama at 127.0.0.1:11434 with `qwen2.5:0.5b` pulled. Sends a
//!    real chat and asserts at least one `AssistantDelta` arrives. Run with
//!    `cargo test --test provider_selection -- --ignored`.

use forge_core::{EndReason, Event};
use forge_ipc::{
    ClientInfo, Hello, IpcEvent, IpcMessage, SendUserMessage, Subscribe, PROTO_VERSION,
};
use std::process::Stdio;
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::UnixStream;

const FORGED: &str = env!("CARGO_BIN_EXE_forged");

async fn connect_with_retry(path: &std::path::Path) -> UnixStream {
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(path).await {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    UnixStream::connect(path)
        .await
        .expect("forged did not create socket in time")
}

fn extract_event(msg: &IpcMessage) -> Option<Event> {
    // F-112: IpcEvent.event is typed.
    if let IpcMessage::Event(IpcEvent { event, .. }) = msg {
        Some(event.clone())
    } else {
        None
    }
}

async fn handshake(stream: &mut UnixStream) {
    forge_ipc::write_frame(
        stream,
        &IpcMessage::Hello(Hello {
            proto: PROTO_VERSION,
            client: ClientInfo {
                kind: "test".into(),
                pid: std::process::id(),
                user: "tester".into(),
            },
        }),
    )
    .await
    .unwrap();
    let _ack: IpcMessage = forge_ipc::read_frame(stream).await.unwrap();
    forge_ipc::write_frame(stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();
}

#[tokio::test]
async fn ollama_dispatch_surfaces_connection_error_at_unreachable_port() {
    // Proves --provider ollama:<model> actually constructs an OllamaProvider:
    // pointed at port 1 (always refused on Linux), the daemon must surface a
    // connection error rather than scripted MockProvider output.

    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let session_id = "ollama-dispatch-test";
    let sock_path = dir.path().join("session.sock");

    let mut child = tokio::process::Command::new(FORGED)
        .arg("--ephemeral")
        .arg("--provider")
        .arg("ollama:nonexistent-model")
        .env("FORGE_SESSION_ID", session_id)
        .env("FORGE_SOCKET_PATH", &sock_path)
        .env("FORGE_WORKSPACE", &workspace)
        .env("OLLAMA_BASE_URL", "http://127.0.0.1:1")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn forged");

    let mut stream = connect_with_retry(&sock_path).await;
    handshake(&mut stream).await;
    forge_ipc::write_frame(
        &mut stream,
        &IpcMessage::SendUserMessage(SendUserMessage {
            text: "hello".to_string(),
        }),
    )
    .await
    .unwrap();

    let mut saw_error = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let frame = tokio::time::timeout_at(deadline, forge_ipc::read_frame(&mut stream)).await;
        match frame {
            Ok(Ok(msg)) => {
                if let Some(Event::SessionEnded { reason, .. }) = extract_event(&msg) {
                    if let EndReason::Error(text) = reason {
                        // OllamaProvider prefixes every error with "ollama
                        // chat request failed:". A MockProvider regression
                        // could not produce that prefix — so this assertion
                        // distinguishes "OllamaProvider was actually built"
                        // from "any session error happened".
                        assert!(
                            text.to_lowercase().contains("ollama"),
                            "SessionEnded error must come from OllamaProvider; got {text}"
                        );
                        saw_error = true;
                    }
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        saw_error,
        "expected SessionEnded with ollama-related Error reason"
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

#[tokio::test]
#[ignore = "requires local Ollama at 127.0.0.1:11434 with qwen2.5:0.5b pulled"]
async fn ollama_round_trip_against_local_qwen() {
    // Probe Ollama for the expected model; if missing, panic with a clear
    // message rather than silently passing.
    let probe = reqwest::get("http://127.0.0.1:11434/api/tags")
        .await
        .expect("Ollama not reachable at 127.0.0.1:11434 — start it with `ollama serve`");
    let body: serde_json::Value = probe.json().await.expect("invalid /api/tags response");
    let has_model = body
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .any(|m| m.get("name").and_then(|n| n.as_str()) == Some("qwen2.5:0.5b"))
        })
        .unwrap_or(false);
    assert!(
        has_model,
        "qwen2.5:0.5b not pulled — run `ollama pull qwen2.5:0.5b`"
    );

    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let session_id = "ollama-roundtrip-test";
    let sock_path = dir.path().join("session.sock");

    let mut child = tokio::process::Command::new(FORGED)
        .arg("--ephemeral")
        .arg("--provider")
        .arg("ollama:qwen2.5:0.5b")
        .env("FORGE_SESSION_ID", session_id)
        .env("FORGE_SOCKET_PATH", &sock_path)
        .env("FORGE_WORKSPACE", &workspace)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn forged");

    let mut stream = connect_with_retry(&sock_path).await;
    handshake(&mut stream).await;
    forge_ipc::write_frame(
        &mut stream,
        &IpcMessage::SendUserMessage(SendUserMessage {
            text: "Reply with a single word: hi".to_string(),
        }),
    )
    .await
    .unwrap();

    let mut saw_delta = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let frame = tokio::time::timeout_at(deadline, forge_ipc::read_frame(&mut stream)).await;
        match frame {
            Ok(Ok(msg)) => match extract_event(&msg) {
                Some(Event::AssistantDelta { delta, .. }) if !delta.is_empty() => {
                    saw_delta = true;
                }
                Some(Event::SessionEnded { .. }) => break,
                _ => {}
            },
            _ => break,
        }
    }
    assert!(
        saw_delta,
        "expected at least one AssistantDelta from Ollama"
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}
