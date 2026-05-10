//! Integration tests for `OllamaProvider` using a mocked `reqwest` server.

use std::time::Duration;

use forge_providers::ollama::{ClientConfig, OllamaProvider, StreamConfig};
use forge_providers::{
    ChatBlock, ChatChunk, ChatMessage, ChatRequest, ChatRole, Provider, StreamErrorKind,
};
use futures::StreamExt;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ndjson_body(lines: &[&str]) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

fn user_msg(text: &str) -> ChatMessage {
    ChatMessage {
        role: ChatRole::User,
        content: vec![ChatBlock::Text(text.into())],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_streams_text_tool_call_and_done() {
    let server = MockServer::start().await;

    let body = ndjson_body(&[
        r#"{"message":{"role":"assistant","content":"Hel"},"done":false}"#,
        r#"{"message":{"role":"assistant","content":"lo"},"done":false}"#,
        r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"fs.read","arguments":{"path":"/x"}}}]},"done":false}"#,
        r#"{"model":"llama3","done":true,"done_reason":"stop"}"#,
    ]);

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(body_partial_json(serde_json::json!({
            "model": "llama3",
            "stream": true,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let provider = OllamaProvider::new(server.uri(), "llama3");

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
            ChatChunk::TextDelta("Hel".into()),
            ChatChunk::TextDelta("lo".into()),
            // Ollama's wire shape carries no tool-call id; the chunk
            // surfaces an empty string so the orchestrator falls back to
            // its own internal id when constructing the round-trip
            // assistant `ChatBlock::ToolCall` / user `ChatBlock::ToolResult`.
            ChatChunk::ToolCall {
                id: String::new(),
                name: "fs.read".into(),
                args: serde_json::json!({"path": "/x"}),
            },
            ChatChunk::Done("stop".into()),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_maps_http_errors() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(500).set_body_string("model not found"))
        .mount(&server)
        .await;

    let provider = OllamaProvider::new(server.uri(), "llama3");
    let req = ChatRequest {
        system: None,
        messages: vec![user_msg("hi")],
        parallel_tool_calls_allowed: false,
    };

    let err = match provider.chat(req).await {
        Ok(_) => panic!("HTTP 500 must map to an error"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(msg.contains("500"), "error should mention status: {msg}");
    assert!(
        msg.contains("model not found"),
        "error should include body: {msg}"
    );
}

/// A peer that refuses to emit a newline must not grow the decoder buffer
/// without bound; the stream must terminate with a typed error.
#[tokio::test(flavor = "multi_thread")]
async fn chat_oversized_line_yields_typed_error_and_terminates() {
    let server = MockServer::start().await;

    // 10 MiB of non-newline bytes (matches the issue reproduction); the
    // decoder must not accumulate past its cap, which in this test is 64 KiB.
    let oversized: String = "A".repeat(10 * 1024 * 1024);

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_string(oversized))
        .mount(&server)
        .await;

    // Sub-default line cap keeps the test fast without changing default semantics.
    let cfg = StreamConfig {
        max_line_bytes: 64 * 1024,
        idle_timeout: Duration::from_secs(5),
        wall_clock_timeout: Duration::from_secs(30),
    };
    let provider = OllamaProvider::with_config(server.uri(), "llama3", cfg);
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
    // Stream is closed after the error.
    assert!(
        stream.next().await.is_none(),
        "stream must terminate after a fatal error"
    );
}

/// A peer that opens a response, emits one line, then stalls forever must be
/// cut by the per-chunk idle timer, not block the session indefinitely.
#[tokio::test(flavor = "multi_thread")]
async fn chat_idle_timeout_yields_typed_error_and_terminates() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    // Bind a raw TCP server we fully control (wiremock can't stall mid-stream).
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Accept one connection, send headers + one NDJSON line, then hold the
    // socket open without writing anything further.
    let server_task = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();

        // Drain enough of the request to let the client's send() complete.
        let mut buf = [0u8; 4096];
        let mut total = Vec::new();
        loop {
            let n = match tokio::time::timeout(
                Duration::from_millis(500),
                tokio::io::AsyncReadExt::read(&mut sock, &mut buf),
            )
            .await
            {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => n,
                Ok(Err(_)) => break,
            };
            total.extend_from_slice(&buf[..n]);
            if total.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }

        let headers = b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n";
        sock.write_all(headers).await.unwrap();

        // First (and only) NDJSON chunk: one short line followed by \n.
        let ndjson = br#"{"message":{"role":"assistant","content":"hi"},"done":false}"#;
        let chunk_header = format!("{:x}\r\n", ndjson.len() + 1);
        sock.write_all(chunk_header.as_bytes()).await.unwrap();
        sock.write_all(ndjson).await.unwrap();
        sock.write_all(b"\n\r\n").await.unwrap();
        sock.flush().await.unwrap();

        // Hold the socket open indefinitely — never send another chunk.
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(sock);
    });

    let cfg = StreamConfig {
        max_line_bytes: 1 << 20,
        idle_timeout: Duration::from_millis(150),
        wall_clock_timeout: Duration::from_secs(30),
    };
    let provider = OllamaProvider::with_config(format!("http://{addr}"), "llama3", cfg);
    let req = ChatRequest {
        system: None,
        messages: vec![user_msg("hi")],
        parallel_tool_calls_allowed: false,
    };

    // The chat() call itself must not hang after headers arrive.
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

    // First chunk is the real text delta; last must be the typed idle-timeout error.
    assert!(
        matches!(&chunks[0], ChatChunk::TextDelta(s) if s == "hi"),
        "first chunk should be the real delta, got: {chunks:?}"
    );
    let last = chunks.last().unwrap();
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

/// A peer that accepts the TCP connect but never writes a single byte of the
/// HTTP response must be cut by the client-level `read_timeout`. This is the
/// layer *below* F-045's decoder idle timer — the decoder is never reached,
/// because headers never arrive. `provider.chat().await` must return `Err`
/// within `read_timeout + small_margin`, not hang.
#[tokio::test(flavor = "multi_thread")]
async fn chat_half_open_connect_times_out_within_read_timeout() {
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Accept one connection and hold the socket open without ever writing a
    // byte (no status line, no headers). Drop only after the client has
    // already timed out.
    let server_task = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
        drop(sock);
    });

    let read_timeout = Duration::from_millis(200);
    let client_cfg = ClientConfig {
        connect_timeout: Duration::from_secs(2),
        read_timeout,
        total_timeout: Duration::from_secs(5),
        tcp_keepalive: Duration::from_secs(30),
    };
    let provider = OllamaProvider::with_config_full(
        format!("http://{addr}"),
        "llama3",
        client_cfg,
        StreamConfig::DEFAULT,
    );
    let req = ChatRequest {
        system: None,
        messages: vec![user_msg("hi")],
        parallel_tool_calls_allowed: false,
    };

    let start = std::time::Instant::now();
    let result = tokio::time::timeout(
        read_timeout + Duration::from_millis(800),
        provider.chat(req),
    )
    .await
    .expect("chat() must return within read_timeout + margin, not hang");
    let elapsed = start.elapsed();

    assert!(result.is_err(), "half-open connect must surface as Err");
    assert!(
        elapsed >= read_timeout,
        "chat() returned in {elapsed:?}, before read_timeout ({read_timeout:?}) — \
         the read_timeout did not fire; another code path produced the error"
    );
    assert!(
        elapsed < read_timeout + Duration::from_millis(800),
        "chat() took {elapsed:?}, exceeds read_timeout ({read_timeout:?}) + margin"
    );

    server_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn list_models_returns_tag_names() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [
                {"name": "llama3"},
                {"name": "mistral"},
            ]
        })))
        .mount(&server)
        .await;

    let provider = OllamaProvider::new(server.uri(), "llama3");
    let models = provider.list_models().await.expect("list_models succeeds");
    assert_eq!(models, vec!["llama3", "mistral"]);
}

/// A malicious Ollama substitute that returns a multi-megabyte 200 OK body
/// must not OOM the dashboard refresh. `list_models()` enforces a size cap
/// before `serde_json::from_slice` and surfaces the overflow as a clean error.
#[tokio::test(flavor = "multi_thread")]
async fn list_models_rejects_oversized_body() {
    let server = MockServer::start().await;

    // 10 MiB of body (matches the DoD reproduction). Default cap is 1 MiB
    // (see `DEFAULT_MAX_BODY_BYTES`), so this must error out — not OOM and
    // not hang. Content is a large opaque string; the size check fires
    // before deserialization, so the payload's JSON validity is irrelevant.
    let oversized = "A".repeat(10 * 1024 * 1024);

    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_string(oversized))
        .mount(&server)
        .await;

    let provider = OllamaProvider::new(server.uri(), "llama3");

    let err = match provider.list_models().await {
        Ok(_) => panic!("oversized body must map to an error"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    // Must surface as an explicit size-cap error (not a generic JSON decode
    // failure). The message should name the cap so operators can diagnose.
    assert!(
        msg.contains("cap") || msg.contains("too large"),
        "error should describe the size cap, got: {msg}"
    );
    assert!(
        msg.contains("1048576") || msg.contains("1 MiB") || msg.contains("1MiB"),
        "error should name the cap value, got: {msg}"
    );
}
