//! Integration tests for `OllamaProvider` (F-743).
//!
//! Ollama's `/api/chat` returns NDJSON: one JSON object per line, with
//! a final line carrying `"done": true` and a `done_reason`. Reference:
//! https://github.com/ollama/ollama/blob/main/docs/api.md#generate-a-chat-completion

use forge_providers::ollama::OllamaProvider;
use forge_providers::{ChatBlock, ChatChunk, ChatMessage, ChatRequest, ChatRole, Provider};
use futures::stream::StreamExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEXT_ONLY_FIXTURE: &str = include_str!("fixtures/ollama_text_only.ndjson");
const TOOL_CALL_FIXTURE: &str = include_str!("fixtures/ollama_with_tool_call.ndjson");

fn user_msg(text: &str) -> ChatMessage {
    ChatMessage {
        role: ChatRole::User,
        content: vec![ChatBlock::Text(text.into())],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_yields_text_deltas_and_done() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(TEXT_ONLY_FIXTURE),
        )
        .mount(&server)
        .await;

    let provider = OllamaProvider::new(server.uri(), "llama3.2");
    let req = ChatRequest {
        system: None,
        messages: vec![user_msg("hi")],
        parallel_tool_calls_allowed: false,
    };

    let mut stream = provider.chat(req).await.expect("chat call succeeds");
    let mut chunks = Vec::new();
    while let Some(c) = stream.next().await {
        chunks.push(c);
    }

    assert_eq!(
        chunks,
        vec![
            ChatChunk::TextDelta("Hello".into()),
            ChatChunk::TextDelta(" world".into()),
            ChatChunk::Done("stop".into()),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_yields_tool_call_chunk_with_generated_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(TOOL_CALL_FIXTURE),
        )
        .mount(&server)
        .await;

    let provider = OllamaProvider::new(server.uri(), "llama3.2");
    let mut stream = provider
        .chat(ChatRequest {
            system: None,
            messages: vec![user_msg("read hello.txt")],
            parallel_tool_calls_allowed: false,
        })
        .await
        .expect("chat ok");

    let mut chunks = Vec::new();
    while let Some(c) = stream.next().await {
        chunks.push(c);
    }

    // Expect: ToolCall(generated id, fs.read, {path: "hello.txt"}), Done("tool_use")
    assert_eq!(chunks.len(), 2, "got {chunks:?}");
    match &chunks[0] {
        ChatChunk::ToolCall { id, name, args } => {
            assert!(!id.is_empty(), "Ollama tool-call id must be auto-generated");
            assert_eq!(name, "fs.read");
            assert_eq!(args, &serde_json::json!({"path": "hello.txt"}));
        }
        other => panic!("expected ToolCall first, got {other:?}"),
    }
    assert_eq!(chunks[1], ChatChunk::Done("tool_use".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_result_round_trips_into_next_request_body() {
    // F-743 DoD #4: tool results from `forge-session::tools::*` must be
    // serialized back into the next Ollama request so the model can see
    // them on the continuation turn.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(TEXT_ONLY_FIXTURE),
        )
        .mount(&server)
        .await;

    let provider = OllamaProvider::new(server.uri(), "llama3.2");

    // Build a continuation request: prior user message → prior assistant
    // tool call → user-role tool result feeding the model the contents.
    let req = ChatRequest {
        system: None,
        messages: vec![
            ChatMessage {
                role: ChatRole::User,
                content: vec![ChatBlock::Text("read hello.txt".into())],
            },
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![ChatBlock::ToolCall {
                    id: "tc-1".into(),
                    name: "fs.read".into(),
                    args: serde_json::json!({"path": "hello.txt"}),
                }],
            },
            ChatMessage {
                role: ChatRole::User,
                content: vec![ChatBlock::ToolResult {
                    id: "tc-1".into(),
                    result: serde_json::json!({"content": "hello world"}),
                }],
            },
        ],
        parallel_tool_calls_allowed: false,
    };
    let mut stream = provider.chat(req).await.expect("chat ok");
    while stream.next().await.is_some() {}

    // Inspect the request body the mock server actually received.
    let received = server.received_requests().await.expect("received requests");
    assert_eq!(received.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");

    assert_eq!(body["model"], "llama3.2");
    assert_eq!(body["stream"], true);
    let messages = body["messages"].as_array().expect("messages array");
    // The tool result must surface as a `tool` role message.
    let tool_msg = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("tool role message expected for round-trip");
    let content = tool_msg["content"].as_str().expect("content string");
    let parsed: serde_json::Value = serde_json::from_str(content).expect("nested json");
    assert_eq!(parsed, serde_json::json!({"content": "hello world"}));
}

#[tokio::test(flavor = "multi_thread")]
async fn assistant_tool_call_round_trips_as_structured_tool_calls() {
    // Regression for the PR-896 review finding: a continuation turn that
    // carries a prior assistant `ChatBlock::ToolCall` must serialize as
    // Ollama's structured `tool_calls` array on the assistant message,
    // not as a plaintext `[tool_call:...]` marker the model can't parse.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(TEXT_ONLY_FIXTURE),
        )
        .mount(&server)
        .await;

    let provider = OllamaProvider::new(server.uri(), "llama3.2");
    let req = ChatRequest {
        system: None,
        messages: vec![
            ChatMessage {
                role: ChatRole::User,
                content: vec![ChatBlock::Text("read hello.txt".into())],
            },
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![ChatBlock::ToolCall {
                    id: "tc-1".into(),
                    name: "fs.read".into(),
                    args: serde_json::json!({"path": "hello.txt"}),
                }],
            },
        ],
        parallel_tool_calls_allowed: false,
    };
    let mut stream = provider.chat(req).await.expect("chat ok");
    while stream.next().await.is_some() {}

    let received = server.received_requests().await.expect("received requests");
    assert_eq!(received.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");
    let messages = body["messages"].as_array().expect("messages array");

    let assistant_msg = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant message expected for round-trip");
    let tcs = assistant_msg["tool_calls"]
        .as_array()
        .expect("assistant message must carry a structured tool_calls array");
    assert_eq!(tcs.len(), 1, "expected exactly one tool_call");
    assert_eq!(tcs[0]["function"]["name"], "fs.read");
    assert_eq!(
        tcs[0]["function"]["arguments"],
        serde_json::json!({"path": "hello.txt"})
    );

    // Defensive: the legacy plaintext marker must not appear anywhere.
    let body_str = serde_json::to_string(&body).unwrap();
    assert!(
        !body_str.contains("[tool_call:"),
        "legacy plaintext tool_call marker leaked into request body: {body_str}"
    );
}

/// Manual smoke test against a real local Ollama instance.
///
/// Skipped by default. To run:
///
/// ```bash
/// ollama serve  # already running
/// FORGE_OLLAMA_SMOKE_MODEL=gemma4:e4b \
///   cargo test -p forge-providers --test ollama \
///   -- --ignored chat_against_local_ollama
/// ```
///
/// Verifies the full request → stream → typed chunks path against a
/// real Ollama server. CI never runs this — it depends on the
/// developer's local environment and is documented as evidence for
/// F-743 DoD #5 ("smoke test against local Ollama").
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn chat_against_local_ollama() {
    let base_url = std::env::var("FORGE_OLLAMA_SMOKE_URL")
        .unwrap_or_else(|_| "http://localhost:11434".to_string());
    let model = std::env::var("FORGE_OLLAMA_SMOKE_MODEL")
        .expect("set FORGE_OLLAMA_SMOKE_MODEL to a model installed locally (e.g. llama3.2)");

    let provider = OllamaProvider::new(&base_url, &model);
    let req = ChatRequest {
        system: Some(std::sync::Arc::from(
            "You are a terse assistant. Reply with one short sentence.",
        )),
        messages: vec![user_msg("Say hello in five words or fewer.")],
        parallel_tool_calls_allowed: false,
    };

    let mut stream = provider.chat(req).await.expect("chat call succeeds");
    let mut accumulated = String::new();
    let mut saw_done = false;
    while let Some(chunk) = stream.next().await {
        match chunk {
            ChatChunk::TextDelta(delta) => accumulated.push_str(&delta),
            ChatChunk::Done(_) => saw_done = true,
            ChatChunk::ToolCall { .. } => {
                // unexpected for this prompt — model shouldn't request a tool
            }
            ChatChunk::Error {
                kind,
                message,
                status,
            } => {
                panic!("ollama returned error: kind={kind:?}, status={status:?}, message={message}")
            }
        }
    }
    assert!(saw_done, "expected a final Done chunk");
    assert!(
        !accumulated.trim().is_empty(),
        "expected non-empty assistant text"
    );
    eprintln!("ollama replied: {accumulated}");
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_maps_http_error_to_typed_chunk() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .mount(&server)
        .await;

    let provider = OllamaProvider::new(server.uri(), "llama3.2");
    let mut stream = provider
        .chat(ChatRequest {
            system: None,
            messages: vec![user_msg("hi")],
            parallel_tool_calls_allowed: false,
        })
        .await
        .expect("chat ok (error surfaces as chunk)");

    let first = stream.next().await.expect("first chunk");
    match first {
        ChatChunk::Error {
            kind,
            message,
            status,
        } => {
            assert!(matches!(kind, forge_providers::StreamErrorKind::Transport));
            assert_eq!(status, Some(500), "status must carry the wire code");
            assert!(
                message.contains("500"),
                "message should mention status: {message}"
            );
        }
        other => panic!("expected Error chunk, got {other:?}"),
    }
}
