//! Integration tests for `AnthropicProvider` using a mocked HTTP server and
//! a recorded SSE fixture.

use std::time::Duration;

use bytes::Bytes;
use forge_providers::anthropic::{parse_anthropic_events, AnthropicProvider, ANTHROPIC_VERSION};
use forge_providers::sse::{decode_sse_stream, StreamConfig};
use forge_providers::{
    ChatBlock, ChatChunk, ChatMessage, ChatRequest, ChatRole, Provider, StreamErrorKind,
};
use futures::stream::{self, StreamExt};
use std::convert::Infallible;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEXT_AND_TOOL_USE_FIXTURE: &str = include_str!("fixtures/anthropic_text_and_tool_use.sse");

fn user_msg(text: &str) -> ChatMessage {
    ChatMessage {
        role: ChatRole::User,
        content: vec![ChatBlock::Text(text.into())],
    }
}

fn fixture_to_byte_stream(
    body: &'static str,
) -> impl futures::Stream<Item = Result<Bytes, Infallible>> {
    stream::iter(vec![Ok(Bytes::from_static(body.as_bytes()))])
}

#[tokio::test]
async fn parse_events_yields_text_and_tool_call_and_done() {
    // Drive the parser directly from the fixture (no HTTP roundtrip) so the
    // event-decode path is exercised in isolation.
    let bytes = fixture_to_byte_stream(TEXT_AND_TOOL_USE_FIXTURE);
    let events = decode_sse_stream(bytes, StreamConfig::DEFAULT);
    let mut chunks_stream = parse_anthropic_events(events);

    let mut chunks = Vec::new();
    while let Some(c) = chunks_stream.next().await {
        chunks.push(c);
    }

    assert_eq!(
        chunks,
        vec![
            ChatChunk::TextDelta("Hello".into()),
            ChatChunk::TextDelta(" world".into()),
            // Regression: the Anthropic-assigned `toolu_…` id captured at
            // `content_block_start` must round-trip end-to-end so the
            // orchestrator can reference it as `tool_use_id` on the
            // follow-up `tool_result`.
            ChatChunk::ToolCall {
                id: "toolu_01".into(),
                name: "get_weather".into(),
                args: serde_json::json!({"city": "sf"}),
            },
            ChatChunk::Done("tool_use".into()),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_round_trip_yields_expected_chunks() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-test"))
        .and(header("anthropic-version", ANTHROPIC_VERSION))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(TEXT_AND_TOOL_USE_FIXTURE),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet", 4096);

    let req = ChatRequest {
        system: Some(std::sync::Arc::from("be helpful")),
        messages: vec![user_msg("hi")],
        parallel_tool_calls_allowed: false,
    };
    let mut stream = provider.chat(req).await.expect("chat call succeeds");

    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk);
    }

    assert_eq!(
        chunks,
        vec![
            ChatChunk::TextDelta("Hello".into()),
            ChatChunk::TextDelta(" world".into()),
            // Regression: id preserved across the full HTTP-mock round-trip.
            ChatChunk::ToolCall {
                id: "toolu_01".into(),
                name: "get_weather".into(),
                args: serde_json::json!({"city": "sf"}),
            },
            ChatChunk::Done("tool_use".into()),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_maps_http_errors() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid api key"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(server.uri(), "bad", "claude-3-5-sonnet", 4096);
    let req = ChatRequest {
        system: None,
        messages: vec![user_msg("hi")],
        parallel_tool_calls_allowed: false,
    };

    // F-749: HTTP non-2xx surfaces as a terminal `ChatChunk::Error` carrying
    // the wire status code rather than collapsing into an opaque `Err`. The
    // orchestrator's classifier reads `status` to route 401 → `Auth`.
    let mut stream = provider.chat(req).await.expect("chat returns Ok stream");
    let chunks: Vec<ChatChunk> = {
        let mut v = Vec::new();
        while let Some(c) = stream.next().await {
            v.push(c);
        }
        v
    };
    assert_eq!(
        chunks.len(),
        1,
        "expected exactly one error chunk: {chunks:?}"
    );
    match &chunks[0] {
        ChatChunk::Error {
            kind,
            message,
            status,
            retry_after_secs,
        } => {
            assert_eq!(*status, Some(401), "status must carry the wire code");
            assert!(matches!(kind, StreamErrorKind::Transport));
            assert!(
                message.contains("401") && message.contains("invalid api key"),
                "message should carry the body: {message}"
            );
            // F-749: 401 isn't a rate-limit, so the parser doesn't run and the
            // field stays `None`. Pinned here so a future refactor that
            // accidentally populates it on non-429 trips this assertion.
            assert_eq!(*retry_after_secs, None);
        }
        other => panic!("expected ChatChunk::Error, got {other:?}"),
    }
}

/// A peer that opens a 200 response with valid SSE prelude, emits one event,
/// then stalls indefinitely must be cut by the per-event idle timer.
#[tokio::test(flavor = "multi_thread")]
async fn chat_idle_timeout_yields_typed_error_and_terminates() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_task = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();

        // Drain the request headers so the client's send() completes.
        let mut buf = [0u8; 4096];
        let mut total = Vec::new();
        loop {
            let n =
                match tokio::time::timeout(Duration::from_millis(500), sock.read(&mut buf)).await {
                    Ok(Ok(0)) | Err(_) => break,
                    Ok(Ok(n)) => n,
                    Ok(Err(_)) => break,
                };
            total.extend_from_slice(&buf[..n]);
            if total.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }

        let headers = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
        sock.write_all(headers).await.unwrap();

        // One real SSE event (text_delta = "hi"), then stall forever.
        let event = b"event: content_block_delta\r\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\r\n\r\n";
        let chunk_header = format!("{:x}\r\n", event.len());
        sock.write_all(chunk_header.as_bytes()).await.unwrap();
        sock.write_all(event).await.unwrap();
        sock.write_all(b"\r\n").await.unwrap();
        sock.flush().await.unwrap();

        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(sock);
    });

    let cfg = StreamConfig {
        max_line_bytes: 1 << 20,
        idle_timeout: Duration::from_millis(150),
        wall_clock_timeout: Duration::from_secs(30),
    };
    let provider = AnthropicProvider::new(
        format!("http://{addr}"),
        "sk-test",
        "claude-3-5-sonnet",
        4096,
    )
    .with_config(cfg);
    let req = ChatRequest {
        system: None,
        messages: vec![user_msg("hi")],
        parallel_tool_calls_allowed: false,
    };

    let stream_fut = provider.chat(req);
    let mut stream = tokio::time::timeout(Duration::from_secs(5), stream_fut)
        .await
        .expect("chat() must not hang after headers arrive")
        .expect("chat call succeeds");

    let collect_fut = async {
        let mut chunks = Vec::new();
        while let Some(chunk) = stream.next().await {
            chunks.push(chunk);
        }
        chunks
    };
    let chunks = tokio::time::timeout(Duration::from_secs(5), collect_fut)
        .await
        .expect("stream must terminate via idle timeout, not hang");

    assert!(
        matches!(&chunks[0], ChatChunk::TextDelta(s) if s == "hi"),
        "first chunk should be the real text delta, got: {chunks:?}"
    );
    let last = chunks.last().expect("at least one chunk (the error)");
    assert!(
        matches!(
            last,
            ChatChunk::Error {
                kind: StreamErrorKind::IdleTimeout,
                ..
            }
        ),
        "expected terminal IdleTimeout error, got: {chunks:?}"
    );

    server_task.abort();
}

/// A peer that emits a single SSE line larger than the configured cap must
/// surface a typed `LineTooLong` error and close the stream.
#[tokio::test(flavor = "multi_thread")]
async fn chat_line_too_long_yields_typed_error_and_terminates() {
    let server = MockServer::start().await;

    // 200 KiB of non-newline content as a single oversized data line.
    let big = "a".repeat(200 * 1024);
    let body = format!("event: content_block_delta\ndata: {big}\n\n");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let cfg = StreamConfig {
        max_line_bytes: 64 * 1024,
        idle_timeout: Duration::from_secs(5),
        wall_clock_timeout: Duration::from_secs(30),
    };
    let provider =
        AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet", 4096).with_config(cfg);

    let req = ChatRequest {
        system: None,
        messages: vec![user_msg("hi")],
        parallel_tool_calls_allowed: false,
    };
    let mut stream = provider.chat(req).await.expect("chat call succeeds");

    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk);
    }

    let last = chunks.last().expect("at least one chunk (the error)");
    assert!(
        matches!(
            last,
            ChatChunk::Error {
                kind: StreamErrorKind::LineTooLong,
                ..
            }
        ),
        "expected terminal LineTooLong error, got: {chunks:?}"
    );
    assert!(
        stream.next().await.is_none(),
        "stream must terminate after a fatal error"
    );
}

/// F-745 DoD #3: a continuation turn that carries a prior assistant
/// `ChatBlock::ToolCall` plus a user `ChatBlock::ToolResult` must serialize
/// onto the wire as Anthropic's `tool_use` block (inside the assistant
/// message) and a paired user-role `tool_result` block referencing the
/// original `tool_use_id`. Asserts on the actual HTTP request body the
/// mock server received — not the in-memory translate output — so a
/// future refactor that breaks the wire shape surfaces here.
#[tokio::test(flavor = "multi_thread")]
async fn tool_call_and_result_round_trip_through_request_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(TEXT_AND_TOOL_USE_FIXTURE),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet", 4096);
    let req = ChatRequest {
        system: None,
        messages: vec![
            ChatMessage {
                role: ChatRole::User,
                content: vec![ChatBlock::Text("what is the weather in sf".into())],
            },
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![
                    ChatBlock::Text("checking".into()),
                    ChatBlock::ToolCall {
                        id: "toolu_round_trip".into(),
                        name: "get_weather".into(),
                        args: serde_json::json!({"city": "sf"}),
                    },
                ],
            },
            ChatMessage {
                role: ChatRole::User,
                content: vec![ChatBlock::ToolResult {
                    id: "toolu_round_trip".into(),
                    result: serde_json::json!({"temp": 60}),
                }],
            },
        ],
        parallel_tool_calls_allowed: false,
    };
    let mut stream = provider.chat(req).await.expect("chat ok");
    while stream.next().await.is_some() {}

    let received = server.received_requests().await.expect("received");
    assert_eq!(received.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");

    let messages = body["messages"].as_array().expect("messages array");
    // The assistant turn must carry a `tool_use` block referencing the id.
    let assistant_msg = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant message must be present");
    let content = assistant_msg["content"]
        .as_array()
        .expect("assistant content array");
    let tool_use = content
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("tool_use block must be present on the assistant message");
    assert_eq!(tool_use["id"], "toolu_round_trip");
    assert_eq!(tool_use["name"], "get_weather");
    assert_eq!(tool_use["input"], serde_json::json!({"city": "sf"}));

    // The follow-up user-role tool_result must reference the same id.
    // Anthropic carries the result as a string-encoded JSON payload.
    let tool_result_msg = messages
        .iter()
        .find(|m| {
            m["role"] == "user"
                && m["content"]
                    .as_array()
                    .map(|c| c.iter().any(|b| b["type"] == "tool_result"))
                    .unwrap_or(false)
        })
        .expect("user-role tool_result message expected");
    let result_block = tool_result_msg["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_result")
        .unwrap();
    assert_eq!(result_block["tool_use_id"], "toolu_round_trip");
    let payload = result_block["content"].as_str().expect("content string");
    let parsed: serde_json::Value = serde_json::from_str(payload).expect("nested json");
    assert_eq!(parsed, serde_json::json!({"temp": 60}));
}

// F-647: Anthropic provider must not follow HTTP redirects. A misconfigured
// proxy or network-layer attacker can inject `302 Location: 169.254.169.254`
// and the default reqwest client would follow blindly to IMDS or another
// internal target. With the SSRF-safe redirect policy in place, the 302
// surfaces to the caller as an error rather than being silently followed.
#[tokio::test(flavor = "multi_thread")]
async fn chat_does_not_follow_redirects() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "https://example.com/elsewhere"),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet", 4096);
    let req = ChatRequest {
        system: None,
        messages: vec![user_msg("hi")],
        parallel_tool_calls_allowed: false,
    };
    // F-749: redirect surfaces as a terminal `ChatChunk::Error` (not a
    // followed redirect, not an opaque `Err`). The 302 status rides on
    // `ChatChunk::Error.status` so the orchestrator can classify it.
    let mut stream = provider.chat(req).await.expect("chat returns Ok stream");
    let chunks: Vec<ChatChunk> = {
        let mut v = Vec::new();
        while let Some(c) = stream.next().await {
            v.push(c);
        }
        v
    };
    let last = chunks.last().expect("at least one chunk");
    match last {
        ChatChunk::Error {
            message, status, ..
        } => {
            assert_eq!(*status, Some(302), "status must reflect the wire 302");
            assert!(message.contains("302"), "message echoes status: {message}");
        }
        other => panic!("expected ChatChunk::Error, got {other:?}"),
    }
}

/// F-749: a 429 response with a delta-seconds `Retry-After` header must
/// surface that value on `ChatChunk::Error.retry_after_secs` so the
/// orchestrator can thread it onto `TurnErrorKind::RateLimit` and the UI
/// countdown lights up against a real provider value.
#[tokio::test(flavor = "multi_thread")]
async fn chat_429_with_retry_after_delta_seconds_threads_value_onto_chunk() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "30")
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet", 4096);
    let req = ChatRequest {
        system: None,
        messages: vec![user_msg("hi")],
        parallel_tool_calls_allowed: false,
    };
    let mut stream = provider.chat(req).await.expect("chat returns Ok stream");
    let chunk = stream.next().await.expect("at least one chunk");
    match chunk {
        ChatChunk::Error {
            status,
            retry_after_secs,
            ..
        } => {
            assert_eq!(status, Some(429));
            assert_eq!(
                retry_after_secs,
                Some(30),
                "delta-seconds value must round-trip onto the error chunk"
            );
        }
        other => panic!("expected ChatChunk::Error, got {other:?}"),
    }
}

/// F-749: a 429 with an HTTP-date `Retry-After` is parsed via
/// `httpdate::parse_http_date` and converted into seconds-from-now.
#[tokio::test(flavor = "multi_thread")]
async fn chat_429_with_retry_after_http_date_threads_value_onto_chunk() {
    let server = MockServer::start().await;
    // Build a "60 seconds from now" HTTP-date so the parsed value is
    // approximately 60 (we allow a wide tolerance below for the wall-clock
    // race between mock-server-setup and provider call).
    let future = std::time::SystemTime::now() + Duration::from_secs(60);
    let http_date = httpdate::fmt_http_date(future);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", http_date.as_str())
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet", 4096);
    let req = ChatRequest {
        system: None,
        messages: vec![user_msg("hi")],
        parallel_tool_calls_allowed: false,
    };
    let mut stream = provider.chat(req).await.expect("chat returns Ok stream");
    let chunk = stream.next().await.expect("at least one chunk");
    match chunk {
        ChatChunk::Error {
            status,
            retry_after_secs,
            ..
        } => {
            assert_eq!(status, Some(429));
            let secs = retry_after_secs.expect("http-date Retry-After must parse");
            assert!(
                (40..=60).contains(&secs),
                "expected ~60s, got {secs}s — allow a wide tolerance for wall-clock race"
            );
        }
        other => panic!("expected ChatChunk::Error, got {other:?}"),
    }
}

/// F-749: a malformed `Retry-After` collapses to `None` rather than failing
/// the request. The orchestrator's UI still surfaces the rate-limit error
/// — it just doesn't show a countdown.
#[tokio::test(flavor = "multi_thread")]
async fn chat_429_with_malformed_retry_after_yields_none() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "not-a-number-or-a-date")
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet", 4096);
    let req = ChatRequest {
        system: None,
        messages: vec![user_msg("hi")],
        parallel_tool_calls_allowed: false,
    };
    let mut stream = provider.chat(req).await.expect("chat returns Ok stream");
    let chunk = stream.next().await.expect("at least one chunk");
    match chunk {
        ChatChunk::Error {
            status,
            retry_after_secs,
            ..
        } => {
            assert_eq!(status, Some(429));
            assert_eq!(
                retry_after_secs, None,
                "malformed Retry-After must defensively collapse to None"
            );
        }
        other => panic!("expected ChatChunk::Error, got {other:?}"),
    }
}

/// Manual smoke test against the real Anthropic API.
///
/// Skipped by default. To run:
///
/// ```bash
/// FORGE_ANTHROPIC_SMOKE_API_KEY=sk-ant-... \
///   cargo test -p forge-providers --test anthropic \
///   -- --ignored chat_against_real_anthropic
/// ```
///
/// Verifies the full request → stream → typed chunks path against the
/// production Anthropic endpoint. CI never runs this — it depends on a
/// live API key and is documented as evidence for F-745 DoD #5 ("smoke
/// test against each provider").
///
/// Mirrors the F-743 `chat_against_local_ollama` pattern: env-gated,
/// terse system prompt, asserts a non-empty assistant reply plus a
/// terminal `Done`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn chat_against_real_anthropic() {
    let api_key = std::env::var("FORGE_ANTHROPIC_SMOKE_API_KEY")
        .expect("set FORGE_ANTHROPIC_SMOKE_API_KEY to a valid Anthropic API key");
    let base_url = std::env::var("FORGE_ANTHROPIC_SMOKE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    // `claude-3-5-haiku-latest` is the cheapest current-line Claude model and is
    // sufficient for a five-word reply. Override with FORGE_ANTHROPIC_SMOKE_MODEL.
    let model = std::env::var("FORGE_ANTHROPIC_SMOKE_MODEL")
        .unwrap_or_else(|_| "claude-3-5-haiku-latest".to_string());

    let provider = AnthropicProvider::new(&base_url, &api_key, &model, 256);
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
            ChatChunk::Error { kind, message } => {
                panic!("anthropic returned error: kind={kind:?}, message={message}")
            }
        }
    }
    assert!(saw_done, "expected a final Done chunk");
    assert!(
        !accumulated.trim().is_empty(),
        "expected non-empty assistant text"
    );
    eprintln!("anthropic replied: {accumulated}");
}
