//! Integration tests for `OpenAiProvider` using a mocked HTTP server and
//! a recorded SSE fixture.

use std::time::Duration;

use bytes::Bytes;
use forge_providers::openai::{parse_openai_events, OpenAiProvider};
use forge_providers::sse::{decode_sse_stream, StreamConfig};
use forge_providers::{
    ChatBlock, ChatChunk, ChatMessage, ChatRequest, ChatRole, Provider, StreamErrorKind,
};
use futures::stream::{self, StreamExt};
use std::convert::Infallible;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEXT_AND_TOOL_USE_FIXTURE: &str = include_str!("fixtures/openai_text_and_tool_use.sse");

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
    let mut chunks_stream = parse_openai_events(events);

    let mut chunks = Vec::new();
    while let Some(c) = chunks_stream.next().await {
        chunks.push(c);
    }

    assert_eq!(
        chunks,
        vec![
            ChatChunk::TextDelta("Hello".into()),
            ChatChunk::TextDelta(" world".into()),
            // Regression: the OpenAI-assigned `call_…` id captured on the
            // first delta for an index must round-trip end-to-end so the
            // orchestrator can reference it as `tool_call_id` on the
            // follow-up `role: "tool"` message.
            ChatChunk::ToolCall {
                id: "call_abc".into(),
                name: "get_weather".into(),
                args: serde_json::json!({"city": "sf"}),
            },
            ChatChunk::Done("tool_calls".into()),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_round_trip_yields_expected_chunks() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .and(header("content-type", "application/json"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(TEXT_AND_TOOL_USE_FIXTURE),
        )
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new(server.uri(), "sk-test", "gpt-4o");

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
                id: "call_abc".into(),
                name: "get_weather".into(),
                args: serde_json::json!({"city": "sf"}),
            },
            ChatChunk::Done("tool_calls".into()),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_maps_http_errors() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid api key"))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new(server.uri(), "bad", "gpt-4o");
    let req = ChatRequest {
        system: None,
        messages: vec![user_msg("hi")],
        parallel_tool_calls_allowed: false,
    };

    // F-749: HTTP non-2xx surfaces as a terminal `ChatChunk::Error` carrying
    // the wire status code rather than collapsing into an opaque `Err`.
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
            // field stays `None`.
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

        // One real OpenAI SSE event (content delta = "hi"), then stall forever.
        let event = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\r\n\r\n";
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
    let provider =
        OpenAiProvider::new(format!("http://{addr}"), "sk-test", "gpt-4o").with_config(cfg);
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
    let body = format!("data: {big}\n\n");

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
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
    let provider = OpenAiProvider::new(server.uri(), "sk-test", "gpt-4o").with_config(cfg);

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

// F-647: OpenAI provider must not follow HTTP redirects. A misconfigured
// proxy or network-layer attacker can inject `302 Location: 169.254.169.254`
// and the default reqwest client would follow blindly to IMDS or another
// internal target. With the SSRF-safe redirect policy in place, the 302
// surfaces to the caller as an error rather than being silently followed.
#[tokio::test(flavor = "multi_thread")]
async fn chat_does_not_follow_redirects() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "https://example.com/elsewhere"),
        )
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new(server.uri(), "sk-test", "gpt-4o");
    let req = ChatRequest {
        system: None,
        messages: vec![user_msg("hi")],
        parallel_tool_calls_allowed: false,
    };
    // F-749: redirect surfaces as a terminal `ChatChunk::Error` (not a
    // followed redirect, not an opaque `Err`).
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
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "45")
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new(server.uri(), "sk-test", "gpt-4o");
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
                Some(45),
                "delta-seconds value must round-trip onto the error chunk"
            );
        }
        other => panic!("expected ChatChunk::Error, got {other:?}"),
    }
}
