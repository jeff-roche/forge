//! HTTP JSON-RPC transport for a single MCP server reachable over
//! `https://` (or `http://`). Two wire channels share one handle:
//!
//! 1. **POST `{url}`** — outbound JSON-RPC requests. [`Http::send`] sets
//!    `Content-Type: application/json`, merges the spec's custom headers
//!    (for auth tokens), and waits for the JSON response body which it
//!    then forwards into the same event channel the SSE reader uses.
//! 2. **GET `{url}`** — server-sent events. The background reader parses
//!    `data:` frames into JSON-RPC notifications and pushes them as
//!    [`HttpEvent::Message`]. Disconnects are non-fatal: the reader
//!    reconnects with exponential backoff so the manager (F-130) sees a
//!    steady stream until it explicitly drops the transport.
//!
//! Timeouts: per the F-071 reqwest-timeout hardening, the POST client has
//! a 30s total timeout plus a `connect_timeout`. The SSE GET uses the same
//! `connect_timeout` but no total timeout — a long-lived stream must not
//! be killed by a wall-clock cap. The SSE reader relies on reconnection
//! to recover from dead sockets.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use forge_core::process::truncate;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::ssrf;
use crate::{McpServerSpec, ServerKind};

/// Total timeout for the POST round-trip, measured from send to the full
/// response body. Matches the DoD's "configured headers + 30s timeout".
const POST_TIMEOUT: Duration = Duration::from_secs(30);

/// Connect timeout applied to both clients — a slow TCP/TLS handshake
/// should fail fast rather than stall the entire transport.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Initial reconnect delay for the SSE reader. Kept short so a transient
/// blip doesn't lose notifications for long.
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(100);

/// Upper bound on the reconnect delay; we stop doubling once we hit this.
/// 30s is long enough to avoid hammering a down server while still being
/// responsive when the server comes back.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

/// Consecutive reconnect failures tolerated before the SSE reader gives
/// up and emits a terminal [`HttpEvent::Closed`]. Counts only errors —
/// a clean stream end that is followed by a successful reconnect resets
/// the counter. Picked to bound crash-to-Degraded latency to sub-second
/// wall time (100ms + 200ms + 400ms backoff ladder ≈ 700ms) while
/// tolerating a transient blip. See F-361.
const CONSECUTIVE_RECONNECT_FAILURE_THRESHOLD: usize = 3;

/// Channel depth for outbound [`HttpEvent`]s. Matches stdio's capacity so
/// the two transports back-pressure consumers identically.
const EVENT_CHANNEL_CAPACITY: usize = 128;

/// Process-wide shared `reqwest::Client`. F-569: building a fresh
/// `reqwest::Client` per server allocates an independent connection pool,
/// DNS cache, TLS root store, and cipher suite list — hundreds of KB per
/// server. A 10-server `.mcp.json` paid that cost ten times. The shared
/// handle gets cloned (cheap `Arc` bump) into every [`Http`]; per-call
/// timeouts on the [`reqwest::RequestBuilder`] continue to work because
/// they override the client default. `connect_timeout` is identical
/// across all MCP servers so the shared default is correct.
///
/// F-645: built on top of `forge_core::http::secure_client_builder()` so
/// every connect runs through the DNS-rebinding-safe resolver from F-644
/// (resolved IPs are filtered against the `url_safety` policy before
/// reqwest opens a socket). We also pin `Policy::none()` for redirects:
/// MCP JSON-RPC has no legitimate use for 3xx hops, and following them
/// would let a hostile endpoint pivot the request to an internal target
/// (e.g. IMDS at 169.254.169.254) without re-running `ssrf::check_url`
/// against the redirect target. With redirects disabled the 302 surfaces
/// as a non-2xx response in [`Http::send`] and the request fails closed.
fn shared_client() -> &'static reqwest::Client {
    static SHARED: OnceLock<reqwest::Client> = OnceLock::new();
    SHARED.get_or_init(|| {
        forge_core::http::secure_client_builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            // The only documented failure path is "no TLS backend compiled
            // in" — we always link rustls (see Cargo.toml). Falling back to
            // `Client::new()` keeps the panic-free invariant of `connect`
            // even if a future build flag drops rustls; reqwest's default
            // is functionally equivalent for the MCP transport.
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Maximum bytes the SSE reader will buffer across network chunks waiting
/// for the next event boundary (`\n\n` or `\r\n\r\n`). Closes F-347: the
/// prior reader accumulated chunks into `buf: Vec<u8>` with no cap, so a
/// hostile or MITM'd `text/event-stream` response that never emits a
/// boundary grew the buffer without bound until OOM. 4 MiB mirrors the
/// stdio transport's `MAX_STDIO_FRAME_BYTES` — large enough for realistic
/// MCP notifications and small enough to keep the worst-case resident set
/// bounded. Over-cap accumulators surface as [`HttpEvent::Malformed`] and
/// force a backoff + reconnect via the sse-reader-loop's error path.
///
/// F-661: also reused as the cap for POST response bodies in [`Http::send`]
/// so a hostile MCP server cannot exhaust daemon memory by replying with a
/// multi-GiB JSON-RPC response. Same threat shape as F-347, same parity
/// limit — JSON-RPC responses don't legitimately exceed an SSE frame.
pub const MAX_SSE_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Events emitted on [`Http::recv`].
///
/// Terminal-event parity with stdio (F-361): after a sustained run of
/// reconnect failures the reader emits exactly one [`HttpEvent::Closed`]
/// and then exits. The manager (F-130) treats that as the cue to flip
/// the server to `Degraded` and drop the transport — without it a dead
/// remote only surfaced on the 30s health-check tick.
///
/// Unlike stdio, the channel does not guarantee `recv()` returns `None`
/// after `Closed`: [`Http::send`] keeps a sender clone alive for POST
/// response forwarding. Consumers should treat `Closed` itself as the
/// terminal signal and drop the transport.
#[derive(Debug)]
pub enum HttpEvent {
    /// A JSON-RPC message: either a response to a prior POST or an SSE
    /// notification. Dispatch is the manager's job.
    Message(serde_json::Value),
    /// The SSE reader accumulated more than [`MAX_SSE_FRAME_BYTES`]
    /// without observing an event boundary and discarded the in-flight
    /// buffer. F-347: closes the DoS surface a hostile or MITM'd
    /// `text/event-stream` exposes by streaming `data:` bytes that never
    /// terminate with `\n\n`. The reader forces a backoff + reconnect
    /// after emitting this event so a misbehaving session does not burn
    /// the sustained-failure budget on a single bad frame.
    Malformed {
        /// How many bytes had been accumulated before the reader hit the
        /// ceiling and discarded. Always `>=` [`MAX_SSE_FRAME_BYTES`].
        bytes_discarded: usize,
    },
    /// The SSE reader exhausted its sustained-failure budget (see
    /// `CONSECUTIVE_RECONNECT_FAILURE_THRESHOLD`) and has exited. The
    /// string is a short human-readable reason suitable for surfacing
    /// as the manager's `Degraded { reason }`.
    Closed(String),
}

/// An active HTTP JSON-RPC connection to one MCP server.
///
/// Construct with [`Http::connect`]. Holds a clone of the process-wide
/// shared `reqwest::Client` (F-569: one connection pool, DNS cache, and
/// TLS root store across every MCP server), an SSE reader task subscribed
/// to the server's GET endpoint, and an `mpsc` receiver that muxes POST
/// responses and SSE notifications into a single stream.
pub struct Http {
    client: reqwest::Client,
    url: String,
    /// Spec-derived custom headers (auth tokens, tenant ids, …). F-569:
    /// behind an `Arc` so spawning the SSE reader and per-call sends
    /// don't deep-copy the `HeaderMap` on every use — a `HeaderMap`
    /// `Clone` is one allocation per name + value, which adds up at
    /// tool-dispatch rate against many servers.
    headers: Arc<HeaderMap>,
    tx: mpsc::Sender<HttpEvent>,
    rx: mpsc::Receiver<HttpEvent>,
    /// SSE reader task. Explicitly aborted in [`Drop`] so the reconnect
    /// loop cannot keep running against a dead server after the handle
    /// has been dropped — it would otherwise sit in the `sleep(delay)`
    /// branch of the backoff loop indefinitely.
    reader: JoinHandle<()>,
}

impl Drop for Http {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Route through `redacted` (F-348) so `{:?}`-style logging — e.g. a
        // `tracing` field carrying the handle — cannot leak a query-string
        // token or `user:pass@` userinfo.
        f.debug_struct("Http")
            .field("url", &redacted(&self.url))
            .finish_non_exhaustive()
    }
}

impl Http {
    /// Connect to the HTTP MCP server described by `spec`. Clones the
    /// process-wide shared `reqwest::Client` handle (cheap `Arc` bump —
    /// see F-569), materialises the custom headers behind an `Arc`, and
    /// spawns the SSE reader. Errors if `spec` describes a stdio server,
    /// if any custom header is malformed, or if the URL fails the SSRF
    /// guard (see [`ssrf::check_url`]).
    pub async fn connect(spec: &McpServerSpec) -> Result<Self> {
        let (url, raw_headers) = match &spec.kind {
            ServerKind::Http { url, headers } => (url.clone(), headers),
            ServerKind::Stdio { .. } => {
                return Err(anyhow!(
                    "http transport cannot connect to a stdio MCP server"
                ));
            }
        };

        // Validate the URL before opening any network connection. This blocks
        // loopback (except in dev builds), link-local, IMDS, and RFC-1918
        // private ranges to prevent SSRF attacks via a crafted .mcp.json.
        ssrf::check_url(&url)?;

        let headers = Arc::new(build_header_map(raw_headers)?);
        let client = shared_client().clone();

        let (tx, rx) = mpsc::channel::<HttpEvent>(EVENT_CHANNEL_CAPACITY);

        let reader_client = client.clone();
        let reader_url = url.clone();
        let reader_headers = Arc::clone(&headers);
        let reader_tx = tx.clone();
        let reader = tokio::spawn(async move {
            sse_reader_loop(reader_client, reader_url, reader_headers, reader_tx).await;
        });

        Ok(Self {
            client,
            url,
            headers,
            tx,
            rx,
            reader,
        })
    }

    /// POST one JSON-RPC request to the server. On a `2xx` response we
    /// parse the JSON body and push it into the event channel as
    /// [`HttpEvent::Message`] so callers see POST responses and SSE
    /// notifications on the same `recv()` surface — this matches the
    /// stdio transport's contract and keeps the manager (F-130) simple.
    ///
    /// Network errors (timeout, connection reset, DNS failure, non-2xx
    /// status) are returned as `Err`. Per the DoD these are recoverable:
    /// the manager is expected to decide whether to retry the request,
    /// restart the whole session, or surface a user-visible failure.
    pub async fn send(&self, message: serde_json::Value) -> Result<()> {
        // F-569: hand the spec headers in by reference rather than
        // `self.headers.clone()` — `apply_headers` extends the
        // `RequestBuilder`'s own header map in place, avoiding the
        // per-call `HeaderMap` allocation that otherwise fires on every
        // tool call + 30s health check ping.
        let resp = apply_headers(self.client.post(&self.url), &self.headers)
            .header(CONTENT_TYPE, "application/json")
            .timeout(POST_TIMEOUT)
            .json(&message)
            .send()
            .await
            .with_context(|| format!("POST {} failed", redacted(&self.url)))?;

        let status = resp.status();
        if !status.is_success() {
            let body = read_body_capped(resp, MAX_SSE_FRAME_BYTES)
                .await
                .unwrap_or_default();
            return Err(anyhow!(
                "POST {} returned HTTP {}: {}",
                redacted(&self.url),
                status,
                truncate(&String::from_utf8_lossy(&body), 512),
            ));
        }

        // MCP HTTP responses are JSON-RPC objects; empty 2xx bodies (e.g.
        // 202 Accepted for a fire-and-forget notification) are legal and
        // we silently skip them — there's nothing to forward.
        //
        // F-661: stream the body through a counting accumulator that
        // aborts on cap overflow. A hostile MCP server could otherwise
        // answer one JSON-RPC POST with a multi-GiB body (or an unbounded
        // chunked stream) and exhaust daemon memory. The cap mirrors
        // `MAX_SSE_FRAME_BYTES` for parity with the SSE GET path.
        let body = read_body_capped(resp, MAX_SSE_FRAME_BYTES)
            .await
            .with_context(|| format!("reading POST {} response body", redacted(&self.url)))?;
        if body.is_empty() {
            return Ok(());
        }

        let value: serde_json::Value = serde_json::from_slice(&body).with_context(|| {
            format!(
                "parsing POST {} response as JSON: {}",
                redacted(&self.url),
                truncate(&String::from_utf8_lossy(&body), 512),
            )
        })?;

        // Best-effort forward; if the consumer has dropped we log and
        // move on — the POST itself succeeded, which is what the caller
        // is asking about.
        if self.tx.send(HttpEvent::Message(value)).await.is_err() {
            tracing::debug!(
                target: "forge_mcp::transport::http",
                "consumer dropped before POST response could be forwarded",
            );
        }

        Ok(())
    }

    /// Receive the next inbound event, or `None` only if the event
    /// channel has been closed (which only happens if the handle itself
    /// is being torn down — see note on [`HttpEvent`]).
    pub async fn recv(&mut self) -> Option<HttpEvent> {
        self.rx.recv().await
    }
}

/// Return a log-safe rendering of `url`: scheme + host + path only. Query
/// strings, fragments, and `user:pass@` userinfo are stripped so signed
/// one-time URLs (`?X-Amz-Signature=...`), personal-proxy tokens
/// (`?token=...`), and HTTP basic-auth credentials cannot bleed into
/// tracing sinks or the `Degraded { reason }` broadcast (F-348).
///
/// Every user-facing emission of `url` in this module — error contexts,
/// `tracing` fields, and any text that ends up in `HttpEvent::Closed` —
/// must route through this helper. The real URL stays inside reqwest,
/// where the query string and userinfo are actually needed for the
/// request.
fn redacted(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            u.set_query(None);
            u.set_fragment(None);
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.into()
        }
        Err(_) => "<invalid-url>".to_string(),
    }
}

/// Translate the spec's string-keyed header map into a validated
/// [`HeaderMap`]. Invalid names or values are reported with the offending
/// key so misconfigurations surface at connect time rather than on the
/// first request.
fn build_header_map(raw: &BTreeMap<String, String>) -> Result<HeaderMap> {
    let mut out = HeaderMap::with_capacity(raw.len());
    for (k, v) in raw {
        let name = HeaderName::try_from(k.as_str())
            .with_context(|| format!("invalid header name {k:?}"))?;
        let value = HeaderValue::try_from(v.as_str())
            .with_context(|| format!("invalid header value for {k:?}"))?;
        out.insert(name, value);
    }
    Ok(out)
}

/// Merge `headers` into `req`'s outgoing `HeaderMap` without cloning the
/// caller's map. F-569: previously every POST and SSE GET called
/// `req.headers(self.headers.clone())`, which deep-copies the entire
/// `HeaderMap` (one allocation per name + value entry) on every request.
/// Iterating and calling `header()` per entry merges in place, sharing
/// `HeaderName` references and only cloning the small `HeaderValue`s
/// (typically inline-stored, so close to free). Per-call header overrides
/// (set on the returned builder) still work because reqwest applies them
/// after these defaults. Stable-sort preservation of duplicate-name
/// values is unchanged: spec headers come from a `BTreeMap<String,
/// String>` so each name appears at most once.
fn apply_headers(mut req: reqwest::RequestBuilder, headers: &HeaderMap) -> reqwest::RequestBuilder {
    for (name, value) in headers.iter() {
        req = req.header(name, value);
    }
    req
}

/// Read a `reqwest::Response` body into a `Vec<u8>` with a hard size cap.
///
/// F-661: closes the DoS surface where `Http::send` previously called
/// `resp.bytes().await` without an upper bound. A hostile (or misconfigured)
/// MCP HTTP server could answer a JSON-RPC POST with a multi-GiB response
/// body — or an unbounded `Transfer-Encoding: chunked` stream — and exhaust
/// daemon memory. We defend in two layers:
///
/// 1. If `Content-Length` is advertised and exceeds `cap`, fail fast before
///    pulling any body bytes.
/// 2. Otherwise stream chunks via `bytes_stream()` and abort the moment the
///    accumulator passes `cap`. This catches the chunked-without-Content-
///    Length attack shape that the early check can't see.
///
/// The error message names the cap so operators can correlate with the
/// `MAX_SSE_FRAME_BYTES` constant during incident triage.
async fn read_body_capped(resp: reqwest::Response, cap: usize) -> Result<Vec<u8>> {
    if let Some(advertised) = resp.content_length() {
        if advertised > cap as u64 {
            return Err(anyhow!(
                "response body advertised {advertised} bytes, exceeds cap of {cap} bytes"
            ));
        }
    }

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading response body chunk")?;
        if buf.len().saturating_add(chunk.len()) > cap {
            return Err(anyhow!(
                "response body exceeds cap of {cap} bytes; aborting to avoid OOM"
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Drive the SSE reader indefinitely. Each iteration opens a GET and
/// streams frames until the body ends or an error surfaces, then sleeps
/// with exponential backoff before retrying.
///
/// The loop exits when either:
/// * the consumer drops [`Http`] (clean teardown), or
/// * consecutive reconnect failures reach
///   `CONSECUTIVE_RECONNECT_FAILURE_THRESHOLD` — in which case we emit
///   a terminal [`HttpEvent::Closed`] before returning so the manager
///   (F-130) can flip the server to `Degraded` without waiting for the
///   30s health-check tick (F-361).
async fn sse_reader_loop(
    client: reqwest::Client,
    url: String,
    headers: Arc<HeaderMap>,
    tx: mpsc::Sender<HttpEvent>,
) {
    let mut delay = INITIAL_RECONNECT_DELAY;
    let mut consecutive_failures: usize = 0;
    // Compute the redaction once: `url` is immutable for the reader's
    // lifetime, so we avoid re-parsing on every retry / log line.
    let log_url = redacted(&url);

    loop {
        match open_and_read_sse(&client, &url, &log_url, &headers, &tx).await {
            Ok(ReaderExit::ConsumerDropped) => {
                // Nothing left to feed — quit cleanly.
                return;
            }
            Ok(ReaderExit::StreamEnded) => {
                // Clean server disconnect. Reset backoff + failure count
                // so a server that recovers doesn't trip the threshold.
                delay = INITIAL_RECONNECT_DELAY;
                consecutive_failures = 0;
                tracing::debug!(
                    target: "forge_mcp::transport::http",
                    url = %log_url,
                    "SSE stream ended cleanly; reconnecting",
                );
            }
            Err(err) => {
                consecutive_failures += 1;
                let reason = format!("{err:#}");
                tracing::warn!(
                    target: "forge_mcp::transport::http",
                    url = %log_url,
                    error = %err,
                    attempts = consecutive_failures,
                    "SSE read failed; backing off before reconnect",
                );

                if consecutive_failures >= CONSECUTIVE_RECONNECT_FAILURE_THRESHOLD {
                    let detail = format!(
                        "http sse reader gave up after {consecutive_failures} \
                         consecutive reconnect failures: {reason}"
                    );
                    tracing::warn!(
                        target: "forge_mcp::transport::http",
                        url = %log_url,
                        %detail,
                        "SSE reader terminating; surfacing Closed to consumer",
                    );
                    // Best-effort — if the consumer already dropped we
                    // just exit, the receiver will observe a closed
                    // channel the same way.
                    let _ = tx.send(HttpEvent::Closed(detail)).await;
                    return;
                }

                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_RECONNECT_DELAY);
            }
        }
    }
}

enum ReaderExit {
    /// The event channel was closed by the consumer dropping [`Http`].
    ConsumerDropped,
    /// The HTTP response body ended normally; we should reconnect.
    StreamEnded,
}

/// One pass over an SSE connection: open the GET, stream bytes, split
/// into events, forward JSON frames. Returns on clean end or surfaces
/// any error up to the reconnect loop for backoff.
async fn open_and_read_sse(
    client: &reqwest::Client,
    url: &str,
    log_url: &str,
    headers: &HeaderMap,
    tx: &mpsc::Sender<HttpEvent>,
) -> Result<ReaderExit> {
    // F-569: extend the `RequestBuilder`'s header map in place rather
    // than handing it a per-call `headers.clone()`. The SSE GET fires
    // on connect *and* on every reconnect, so this clone showed up on
    // restart-storms in addition to the steady-state cost.
    let resp = apply_headers(client.get(url), headers)
        .header(ACCEPT, "text/event-stream")
        .send()
        .await
        .with_context(|| format!("GET {log_url} for SSE stream"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("GET {log_url} returned HTTP {status}"));
    }

    let mut stream = resp.bytes_stream();
    // Accumulator across network chunks. SSE frames are separated by
    // `\n\n` and a frame can span multiple TCP reads.
    let mut buf: Vec<u8> = Vec::with_capacity(1024);

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading SSE chunk from {log_url}"))?;
        buf.extend_from_slice(&chunk);

        // F-347: guard the accumulator against an unbounded-frame DoS.
        // A hostile (or MITM'd plain-`http://`) server that streams
        // `data: ` bytes without ever emitting an event boundary grew
        // `buf` until OOM in the pre-fix reader. Emit a `Malformed`
        // event, clear the buffer, and bail to the reconnect loop so
        // the transport resyncs on a fresh session instead of burning
        // the sustained-failure budget on a single bad frame.
        if buf.len() > MAX_SSE_FRAME_BYTES && find_event_boundary(&buf).is_none() {
            let bytes_discarded = buf.len();
            tracing::warn!(
                target: "forge_mcp::transport::http",
                url = %log_url,
                bytes_discarded = bytes_discarded,
                cap = MAX_SSE_FRAME_BYTES,
                "SSE frame exceeded cap without event boundary; dropping buffer and reconnecting",
            );
            buf.clear();
            // Best-effort — if the consumer already dropped we just fall
            // through to the Err, which exits the reconnect loop normally.
            let _ = tx.send(HttpEvent::Malformed { bytes_discarded }).await;
            return Err(anyhow!(
                "sse frame exceeded {MAX_SSE_FRAME_BYTES} bytes without boundary"
            ));
        }

        while let Some(end) = find_event_boundary(&buf) {
            // F-569: parse the event in place against the accumulator
            // rather than draining into a fresh `Vec<u8>` per event. On
            // notification-heavy servers this saves one allocation per
            // event and prevents the per-event Vec from holding the
            // accumulator's high-water-mark capacity. We scope the
            // borrow tightly so `buf.drain(..end.frame_end)` below is
            // free to mutate it.
            let event_end = end.frame_end - end.delim_len;
            let payload = parse_event_data(&buf[..event_end]);
            buf.drain(..end.frame_end);

            if let Some(payload) = payload {
                match serde_json::from_str::<serde_json::Value>(&payload) {
                    Ok(value) => {
                        if tx.send(HttpEvent::Message(value)).await.is_err() {
                            return Ok(ReaderExit::ConsumerDropped);
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            target: "forge_mcp::transport::http",
                            error = %err,
                            payload = %truncate(&payload, 512),
                            "dropping malformed SSE JSON frame",
                        );
                    }
                }
            }
        }
    }

    Ok(ReaderExit::StreamEnded)
}

/// Record of where one SSE event ends in the read buffer.
struct EventBoundary {
    /// Index in the buffer one past the end of the frame delimiter — what
    /// you pass to `drain(..frame_end)` to consume the event plus its
    /// separator.
    frame_end: usize,
    /// Width of the delimiter found (2 for `\n\n`, 4 for `\r\n\r\n`). The
    /// caller trims this many bytes off the drained slice before parsing
    /// so the event body itself has no trailing blank line.
    delim_len: usize,
}

/// Scan `buf` for the first SSE event boundary. Per RFC-like SSE, events
/// are separated by a blank line, which may be `\n\n` (Unix servers) or
/// `\r\n\r\n` (wire-spec-compliant). Returns `None` when no full event
/// has arrived yet.
fn find_event_boundary(buf: &[u8]) -> Option<EventBoundary> {
    // Prefer the 4-byte CRLF delimiter when both are present at the same
    // offset, so we don't split a `\r\n\r\n` into two `\n\n` events.
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n");
    let lf = buf.windows(2).position(|w| w == b"\n\n");
    match (crlf, lf) {
        (Some(c), Some(l)) if c <= l => Some(EventBoundary {
            frame_end: c + 4,
            delim_len: 4,
        }),
        (_, Some(l)) => Some(EventBoundary {
            frame_end: l + 2,
            delim_len: 2,
        }),
        (Some(c), None) => Some(EventBoundary {
            frame_end: c + 4,
            delim_len: 4,
        }),
        (None, None) => None,
    }
}

/// Extract the concatenated `data:` payload from one SSE event. Non-data
/// lines (`event:`, `id:`, comments starting with `:`, retry hints) are
/// ignored — JSON-RPC frames only care about `data:`. Multi-line `data`
/// fields are joined with `\n` per the SSE spec.
fn parse_event_data(event_bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(event_bytes).ok()?;
    let mut data = String::new();
    let mut had_data = false;
    for line in text.split('\n') {
        // Tolerate \r-terminated lines emitted by CRLF servers.
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        // `data: foo` or `data:foo` — strip the prefix and an optional
        // single leading space. Ignore other field names entirely.
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if had_data {
                data.push('\n');
            }
            data.push_str(rest);
            had_data = true;
        }
    }
    if had_data {
        Some(data)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_stdio_spec() {
        let spec = McpServerSpec {
            kind: ServerKind::Stdio {
                command: "/bin/true".to_string(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
        };
        // `connect` is async; we only need the validation path, so drive
        // it on a current-thread runtime.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let err = rt
            .block_on(Http::connect(&spec))
            .expect_err("stdio spec must reject");
        assert!(
            format!("{err:#}").contains("stdio"),
            "error should explain transport mismatch: {err:#}"
        );
    }

    #[test]
    fn build_header_map_round_trips_common_headers() {
        let mut raw = BTreeMap::new();
        raw.insert("Authorization".to_string(), "Bearer abc123".to_string());
        raw.insert("X-Tenant".to_string(), "forge".to_string());
        let map = build_header_map(&raw).expect("valid headers");
        assert_eq!(map.get("authorization").unwrap(), "Bearer abc123");
        assert_eq!(map.get("x-tenant").unwrap(), "forge");
    }

    #[test]
    fn build_header_map_rejects_invalid_name() {
        let mut raw = BTreeMap::new();
        raw.insert("bad header".to_string(), "x".to_string());
        let err = build_header_map(&raw).expect_err("space in name must reject");
        assert!(
            format!("{err:#}").contains("bad header"),
            "error should name the bad header: {err:#}"
        );
    }

    #[test]
    fn parse_event_data_joins_multi_line_data() {
        let frame = b"event: note\ndata: {\"a\":1,\ndata: \"b\":2}";
        let payload = parse_event_data(frame).expect("data field");
        assert_eq!(payload, "{\"a\":1,\n\"b\":2}");
    }

    #[test]
    fn parse_event_data_ignores_comments_and_non_data_lines() {
        let frame = b": keepalive\nid: 42\nretry: 5000\ndata: {\"ok\":true}";
        let payload = parse_event_data(frame).expect("data field");
        assert_eq!(payload, "{\"ok\":true}");
    }

    #[test]
    fn parse_event_data_returns_none_without_data_field() {
        let frame = b"event: heartbeat\nid: 7";
        assert!(parse_event_data(frame).is_none());
    }

    #[test]
    fn redacted_strips_query_string_token() {
        let input = "https://mcp.example.com/v1?access_token=shhh";
        let out = redacted(input);
        assert!(!out.contains("shhh"), "token must not appear: {out}");
        assert!(
            !out.contains("access_token"),
            "query key must be gone: {out}"
        );
        assert!(out.contains("mcp.example.com"), "host must survive: {out}");
        assert!(out.contains("/v1"), "path must survive: {out}");
    }

    #[test]
    fn redacted_strips_fragment() {
        let input = "https://mcp.example.com/v1#tok=shhh";
        let out = redacted(input);
        assert!(!out.contains("shhh"), "fragment must be gone: {out}");
        assert!(!out.contains('#'), "fragment delimiter must be gone: {out}");
    }

    #[test]
    fn redacted_strips_userinfo() {
        let input = "https://alice:s3cret@mcp.example.com/v1";
        let out = redacted(input);
        assert!(!out.contains("alice"), "username must be gone: {out}");
        assert!(!out.contains("s3cret"), "password must be gone: {out}");
        assert!(out.contains("mcp.example.com"));
    }

    #[test]
    fn redacted_falls_back_on_invalid_url() {
        let out = redacted("not a url at all");
        assert_eq!(out, "<invalid-url>");
    }

    #[test]
    fn redacted_preserves_clean_url_shape() {
        let input = "https://mcp.example.com/v1";
        let out = redacted(input);
        assert!(out.contains("https://"));
        assert!(out.contains("mcp.example.com"));
        assert!(out.contains("/v1"));
    }

    #[test]
    fn find_event_boundary_handles_lf_and_crlf() {
        let lf = b"data: 1\n\ndata: 2\n\n";
        let b = find_event_boundary(lf).expect("lf boundary");
        assert_eq!(b.frame_end, 9);
        assert_eq!(b.delim_len, 2);

        let crlf = b"data: 1\r\n\r\ndata: 2\r\n\r\n";
        let b = find_event_boundary(crlf).expect("crlf boundary");
        assert_eq!(b.frame_end, 11);
        assert_eq!(b.delim_len, 4);

        assert!(find_event_boundary(b"data: partial\n").is_none());
    }
}
