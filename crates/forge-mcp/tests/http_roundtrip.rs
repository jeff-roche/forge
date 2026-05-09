//! F-129 integration test: drive the HTTP transport against a
//! `wiremock`-backed fake MCP server and verify
//!
//! * a POST round-trip returns a JSON-RPC response via `recv()`,
//! * an SSE GET response delivers a `data:` frame as an `HttpEvent::Message`,
//! * spec headers are propagated on both the POST and the SSE GET.
//!
//! F-361: also verifies symmetric terminal-event behaviour. When the SSE
//! reader's reconnect loop saturates (sustained failure), the transport
//! must emit [`HttpEvent::Closed`] so the manager can flip the server to
//! `Degraded` within milliseconds — matching the stdio contract — rather
//! than waiting up to 30s for the next health-check tick.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_mcp::manager::LifecycleTuning;
use forge_mcp::transport::http::MAX_SSE_FRAME_BYTES;
use forge_mcp::transport::{Http, HttpEvent};
use forge_mcp::{McpManager, McpServerSpec, ServerKind, ServerState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn http_spec(url: &str, auth: &str) -> McpServerSpec {
    let mut headers = BTreeMap::new();
    headers.insert("Authorization".to_string(), auth.to_string());
    McpServerSpec {
        kind: ServerKind::Http {
            url: url.to_string(),
            headers,
        },
    }
}

/// In-test HTTP server purpose-built for [`post_roundtrip_and_sse_notification`].
///
/// F-561 follow-up: wiremock writes its mocked body in one shot then
/// closes the socket, which forces the SSE reader into its reconnect
/// ladder for the rest of the test. Under CI load that ladder can trip
/// `CONSECUTIVE_RECONNECT_FAILURE_THRESHOLD` (3 errors inside ~700ms
/// backoff) before all three signals have surfaced, and the transport
/// emits a terminal `HttpEvent::Closed` mid-test. See PR #562 for the
/// initial drain-loop hardening that didn't fully eliminate the flake.
///
/// This fixture sidesteps the entire failure mode by holding the SSE
/// connection open for the lifetime of the test: it writes the two
/// notification frames immediately and then parks on a shutdown signal,
/// so the reader stays inside `bytes_stream().next().await` and never
/// reconnects. POST is handled by the same listener — there is no
/// wiremock dependency for this test.
struct InTestHttpServer {
    addr: std::net::SocketAddr,
    shutdown: Arc<Notify>,
    handle: tokio::task::JoinHandle<()>,
}

impl InTestHttpServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let shutdown = Arc::new(Notify::new());
        let server_shutdown = Arc::clone(&shutdown);

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = server_shutdown.notified() => return,
                    accept = listener.accept() => {
                        let Ok((mut sock, _)) = accept else { return };
                        let conn_shutdown = Arc::clone(&server_shutdown);
                        tokio::spawn(async move {
                            handle_connection(&mut sock, conn_shutdown).await;
                        });
                    }
                }
            }
        });

        Self {
            addr,
            shutdown,
            handle,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }
}

impl Drop for InTestHttpServer {
    fn drop(&mut self) {
        self.shutdown.notify_waiters();
        self.handle.abort();
    }
}

/// Read the request line + headers (terminated by `\r\n\r\n`), inspect
/// the method, and serve a canned response. POST returns the JSON-RPC
/// response and closes; GET writes the SSE headers + two `data:` frames
/// and then parks on the shutdown signal so the reader never sees the
/// connection close mid-test.
async fn handle_connection(sock: &mut tokio::net::TcpStream, shutdown: Arc<Notify>) {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    let header_end = loop {
        match sock.read(&mut tmp).await {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break p + 4;
                }
                if buf.len() > 16 * 1024 {
                    return;
                }
            }
            Err(_) => return,
        }
    };
    let head = std::str::from_utf8(&buf[..header_end]).unwrap_or("");
    let is_post = head.starts_with("POST ");
    let is_get = head.starts_with("GET ");

    // Preserve the original test's spec-header propagation coverage:
    // every request must carry the spec-supplied `Authorization: Bearer
    // token`. POST additionally has to declare JSON content-type; GET
    // has to declare it accepts SSE. A header miss aborts the
    // connection so the transport observes a hard error and the test
    // panics with a clear message instead of silently passing.
    let lower = head.to_ascii_lowercase();
    if !lower.contains("authorization: bearer token") {
        return;
    }
    if is_post && !lower.contains("content-type: application/json") {
        return;
    }
    if is_get && !lower.contains("accept: text/event-stream") {
        return;
    }

    if is_post {
        // Drain the request body if Content-Length was advertised so the
        // peer doesn't see a write-side reset before reading the response.
        let content_length = head
            .lines()
            .find_map(|l| {
                l.strip_prefix("Content-Length: ")
                    .or_else(|| l.strip_prefix("content-length: "))
            })
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let already = buf.len() - header_end;
        if content_length > already {
            let mut remaining = content_length - already;
            while remaining > 0 {
                let n = match sock.read(&mut tmp).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                remaining = remaining.saturating_sub(n);
            }
        }

        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.write_all(body).await;
        let _ = sock.shutdown().await;
        return;
    }

    if is_get {
        // Write SSE headers with `Transfer-Encoding: chunked` and stream
        // both notification frames immediately so the reader observes
        // `tools/list_changed` and `ping` regardless of how the runtime
        // schedules the POST round-trip relative to the reader task.
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n";
        if sock.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        for frame in [
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n\n",
        ] {
            let chunk = format!("{:x}\r\n{}\r\n", frame.len(), frame);
            if sock.write_all(chunk.as_bytes()).await.is_err() {
                return;
            }
        }
        // Park until the test drops the server, keeping the SSE
        // connection open so the reader never enters the reconnect
        // ladder. This is the load-bearing change: wiremock's
        // close-after-body behaviour was the root cause of the F-561
        // post-fix residual flake.
        shutdown.notified().await;
        let _ = sock.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_roundtrip_and_sse_notification() {
    let server = InTestHttpServer::start().await;

    let mut t = Http::connect(&http_spec(&server.url(), "Bearer token"))
        .await
        .expect("connect");

    t.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    }))
    .await
    .expect("POST send");

    // Collect messages by id/method rather than count. Order between the
    // POST response and the SSE frames is racy (the GET runs on a reader
    // task from connect time). The fixture holds the SSE connection open
    // for the test's lifetime, so the reader does not reconnect and we
    // do not see duplicate SSE frames — but the drain loop still
    // tolerates them and surfaces detailed diagnostics on failure so a
    // future regression points at the right culprit.
    let mut got_post_response = false;
    let mut got_tools_changed = false;
    let mut got_ping = false;
    let mut observed: Vec<String> = Vec::new();

    let deadline = Instant::now() + Duration::from_secs(15);
    while !(got_post_response && got_tools_changed && got_ping) {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            panic!("deadline reached before all three signals; observed={observed:?}");
        }
        let recv_result = tokio::time::timeout(remaining, t.recv()).await;
        let event = match recv_result {
            Err(_) => panic!("recv timeout; observed={observed:?}"),
            Ok(None) => panic!(
                "channel closed before all signals surfaced; observed={observed:?} \
                 (post_response={got_post_response}, tools_changed={got_tools_changed}, \
                 ping={got_ping})"
            ),
            Ok(Some(ev)) => ev,
        };
        match event {
            HttpEvent::Closed(reason) => panic!(
                "transport surfaced Closed({reason}) mid-test; observed={observed:?} \
                 (post_response={got_post_response}, tools_changed={got_tools_changed}, \
                 ping={got_ping})"
            ),
            HttpEvent::Malformed { bytes_discarded } => {
                panic!("transport surfaced Malformed({bytes_discarded}); observed={observed:?}")
            }
            HttpEvent::Message(v) => {
                if v.get("id") == Some(&serde_json::json!(1)) {
                    assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
                    got_post_response = true;
                    observed.push("Message(post_response)".into());
                } else if v.get("method")
                    == Some(&serde_json::json!("notifications/tools/list_changed"))
                {
                    got_tools_changed = true;
                    observed.push("Message(tools/list_changed)".into());
                } else if v.get("method") == Some(&serde_json::json!("ping")) {
                    got_ping = true;
                    observed.push("Message(ping)".into());
                } else {
                    panic!("unexpected message {v}; observed={observed:?}");
                }
            }
        }
    }

    assert!(got_post_response, "POST response must surface on recv");
    assert!(got_tools_changed, "first SSE notification must surface");
    assert!(got_ping, "second SSE notification must surface");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_2xx_post_surfaces_as_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    // GET is required for the reader task to not spam backoff logs; stub
    // a trivial SSE stream so it terminates cleanly.
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(""),
        )
        .mount(&server)
        .await;

    let t = Http::connect(&http_spec(&server.uri(), "Bearer token"))
        .await
        .expect("connect");

    let err = t
        .send(serde_json::json!({"jsonrpc":"2.0","id":1}))
        .await
        .expect_err("500 must surface");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("500"),
        "error should mention the HTTP status: {msg}"
    );
}

/// F-361: when the SSE GET keeps failing, the reader must eventually give
/// up and surface a terminal `HttpEvent::Closed`. Without this the
/// manager's `pump_exit` channel is dead for HTTP and a crashed remote
/// server only becomes visible on the 30s health-check tick. Here every
/// GET returns 503 so the reader backs off and never recovers — the
/// transport must emit `Closed` and then close the receiver.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_sustained_failure_surfaces_terminal_closed_event() {
    let server = MockServer::start().await;

    // Stub a POST so `Http::connect` has a partner for the outbound path;
    // the test itself only exercises the GET/SSE reader's failure path.
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    // Every GET returns 503 — no SSE stream will ever open. The reader
    // should retry up to the sustained-failure threshold, then emit
    // `HttpEvent::Closed`.
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;

    let mut t = Http::connect(&http_spec(&server.uri(), "Bearer token"))
        .await
        .expect("connect");

    // Budget: reader backoff is initial 100ms, doubling to 30s cap. Three
    // consecutive failures fit well under a couple seconds even with
    // jitter on slow CI runners.
    let ev = tokio::time::timeout(Duration::from_secs(10), t.recv())
        .await
        .expect("timed out waiting for HttpEvent::Closed")
        .expect("channel closed before Closed event surfaced");

    match ev {
        HttpEvent::Closed(reason) => {
            assert!(
                !reason.is_empty(),
                "Closed event must carry a non-empty reason"
            );
            // Dropping the handle aborts the reader and releases the
            // reqwest client; we don't assert channel auto-close here
            // because `Http::send` keeps a live sender clone for POST
            // response forwarding. The manager treats `Closed` itself
            // as the terminal signal and drops the transport.
        }
        HttpEvent::Message(v) => {
            panic!("expected HttpEvent::Closed after sustained failure, got Message({v})")
        }
        HttpEvent::Malformed { bytes_discarded } => {
            panic!("expected HttpEvent::Closed after sustained failure, got Malformed({bytes_discarded})")
        }
    }
}

/// F-361 regression at the manager layer. A dead HTTP MCP server (503 on
/// both POST and GET) must surface as `Degraded` via the transport's
/// terminal event, not the 30s health-check tick. We set the health
/// interval high enough that any Degraded we observe cannot have come
/// from a health ping — it can only have come from `pump_exit` firing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manager_degrades_http_server_on_sustained_reconnect_failure() {
    let server = MockServer::start().await;

    // Every POST (including `initialize` / `tools/list` handshake) and
    // every GET returns 503. The manager should flip `Degraded` via the
    // SSE reader's terminal `Closed` event or the handshake failure —
    // either path is driven by the transport, not the health tick.
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;

    let mut headers = BTreeMap::new();
    headers.insert("Authorization".into(), "Bearer token".into());
    let spec = McpServerSpec {
        kind: ServerKind::Http {
            url: server.uri(),
            headers,
        },
    };

    let mut cfg = BTreeMap::new();
    cfg.insert("remote".to_string(), spec);

    // Health-check interval is pinned high enough that any Degraded we
    // observe inside the test budget cannot have come from a health
    // ping — it can only have come from the transport's terminal event.
    let tuning = LifecycleTuning {
        health_check_interval: Duration::from_secs(60),
    };
    let mgr = McpManager::with_tuning(cfg, tuning);

    mgr.start("remote").await.expect("start remote");

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let list = mgr.list().await;
        let entry = list
            .iter()
            .find(|s| s.name == "remote")
            .expect("remote entry");
        if matches!(entry.state, ServerState::Degraded { .. }) {
            break;
        }
        if Instant::now() >= deadline {
            panic!(
                "remote server did not reach Degraded; last state = {:?}",
                entry.state
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    mgr.stop("remote").await.expect("stop remote");
}

/// F-348: URL credentials (query-string tokens, `user:pass@` userinfo) must
/// never appear in the error returned by `Http::send`. The MCP server URL is
/// needed inside reqwest but every user-facing emission routes through the
/// `redacted()` helper, which strips query, fragment, and userinfo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_error_redacts_query_string_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(""),
        )
        .mount(&server)
        .await;

    // Append a secret as a query-string token, mirroring signed-URL /
    // personal-dev-proxy patterns called out in the threat model.
    let url_with_token = format!("{}/?access_token=shhh-no-logging", server.uri());
    let t = Http::connect(&http_spec(&url_with_token, "Bearer token"))
        .await
        .expect("connect");

    let err = t
        .send(serde_json::json!({"jsonrpc":"2.0","id":1}))
        .await
        .expect_err("500 must surface");
    let msg = format!("{err:#}");
    assert!(
        !msg.contains("shhh-no-logging"),
        "query-string token must not appear in error: {msg}"
    );
    assert!(
        !msg.contains("access_token"),
        "query key must not appear in error: {msg}"
    );
    assert!(
        msg.contains("500"),
        "error should still name the HTTP status: {msg}"
    );
}

/// F-348: sustained SSE failure emits `HttpEvent::Closed(reason)`. That
/// reason string is broadcast by the manager as `Degraded { reason }`, so
/// it absolutely must not carry URL credentials.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_closed_reason_redacts_query_string_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;

    let url_with_token = format!("{}/?access_token=shhh-no-broadcast", server.uri());
    let mut t = Http::connect(&http_spec(&url_with_token, "Bearer token"))
        .await
        .expect("connect");

    let ev = tokio::time::timeout(Duration::from_secs(10), t.recv())
        .await
        .expect("timed out waiting for HttpEvent::Closed")
        .expect("channel closed before Closed event surfaced");

    match ev {
        HttpEvent::Closed(reason) => {
            assert!(
                !reason.contains("shhh-no-broadcast"),
                "Closed reason must not carry the token: {reason}"
            );
            assert!(
                !reason.contains("access_token"),
                "Closed reason must not carry the query key: {reason}"
            );
        }
        HttpEvent::Message(v) => {
            panic!("expected HttpEvent::Closed after sustained failure, got Message({v})")
        }
        HttpEvent::Malformed { bytes_discarded } => {
            panic!("expected HttpEvent::Closed after sustained failure, got Malformed({bytes_discarded})")
        }
    }
}

/// F-347 DoD regression: an SSE response that streams 16 MiB of bytes
/// without emitting an event boundary (`\n\n` or `\r\n\r\n`) must not
/// drive the reader's accumulator past `MAX_SSE_FRAME_BYTES`. The
/// transport must surface `HttpEvent::Malformed { bytes_discarded >= cap }`
/// and the sustained-failure reconnect loop bounds subsequent runs,
/// eventually emitting `HttpEvent::Closed`.
///
/// Fixture: wiremock serves a single 16 MiB response body that is all
/// `data: xxx...` with no trailing `\n\n`. Because wiremock writes the
/// body in one shot before the connection closes, every reconnect gets
/// the same hostile payload and trips the cap again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_sixteen_mib_no_boundary_surfaces_malformed() {
    let server = MockServer::start().await;

    // POST: harmless 200 so the transport can `connect` cleanly.
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    // 16 MiB of `data: ` bytes with no event boundary. Every byte is an
    // ASCII letter so neither `\n\n` nor `\r\n\r\n` ever appears — this
    // is the pathological DoS shape the pre-fix reader accumulated into
    // `buf` without bound.
    let hostile_body: Vec<u8> = {
        let mut v = Vec::with_capacity(16 * 1024 * 1024);
        v.extend_from_slice(b"data: ");
        while v.len() < 16 * 1024 * 1024 {
            v.push(b'A');
        }
        v
    };

    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_bytes(hostile_body),
        )
        .mount(&server)
        .await;

    let mut t = Http::connect(&http_spec(&server.uri(), "Bearer token"))
        .await
        .expect("connect");

    let ev = tokio::time::timeout(Duration::from_secs(30), t.recv())
        .await
        .expect("transport must emit an event within 30s")
        .expect("transport must not close before emitting Malformed");

    match ev {
        HttpEvent::Malformed { bytes_discarded } => {
            assert!(
                bytes_discarded >= MAX_SSE_FRAME_BYTES,
                "bytes_discarded must be >= cap: {bytes_discarded}",
            );
        }
        // Closed landing before Malformed means the DoD contract is unmet.
        HttpEvent::Closed(reason) => {
            panic!("expected Malformed before Closed; got Closed({reason})")
        }
        HttpEvent::Message(v) => {
            panic!("unexpected Message event in over-cap SSE fixture: {v}")
        }
    }
}

/// F-645: SSRF redirect-pivot regression. A hostile MCP endpoint that
/// answers a JSON-RPC POST with `302 Location: http://169.254.169.254/...`
/// must NOT be followed by the shared client. With redirects enabled,
/// reqwest would re-issue the POST against IMDS without re-running
/// `url_safety::check_url`. The transport's redirect policy must treat the
/// 302 as the final response so the request fails and IMDS is never hit.
///
/// We assert two things:
/// 1. `send()` returns `Err` (the 302 surfaces as a non-2xx HTTP status,
///    same as any other unexpected response).
/// 2. The redirect target server records zero hits — proving the redirect
///    was never followed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_does_not_follow_redirect_to_internal_target() {
    // The "internal target" — stands in for IMDS. If the redirect is ever
    // followed, this server records a hit and we fail the test.
    let internal = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/latest/meta-data"))
        .respond_with(ResponseTemplate::new(200).set_body_string("SECRET"))
        .expect(0) // must never be hit
        .mount(&internal)
        .await;

    // The "attacker" MCP endpoint — answers POSTs with a 302 redirect to
    // the internal target. GET (for SSE) returns an empty event stream so
    // the reader task settles cleanly.
    let attacker = MockServer::start().await;
    let redirect_to = format!("{}/latest/meta-data", internal.uri());
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", redirect_to.as_str()))
        .mount(&attacker)
        .await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(""),
        )
        .mount(&attacker)
        .await;

    let t = Http::connect(&http_spec(&attacker.uri(), "Bearer token"))
        .await
        .expect("connect");

    let err = t
        .send(serde_json::json!({"jsonrpc":"2.0","id":1}))
        .await
        .expect_err("302 redirect must not be followed and must surface as an error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("302"),
        "error should mention the 302 status: {msg}",
    );

    // Belt-and-braces: explicitly assert the internal target saw no
    // request. wiremock's `expect(0)` is verified on drop.
    drop(internal);
    drop(attacker);
}
