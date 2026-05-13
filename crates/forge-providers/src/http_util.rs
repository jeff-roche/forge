//! Shared HTTP / SSE helpers used by the Anthropic and OpenAI providers.
//!
//! Both providers run an identical streaming-HTTP posture (per-line cap,
//! inter-event idle timeout, wall-clock budget) and produce identical
//! [`crate::StreamErrorKind`] envelopes when the SSE adapter aborts. F-583
//! and F-584 shipped near-identical helpers in parallel; this module is the
//! single home those helpers consolidate into.
//!
//! ## SSRF posture (F-647)
//!
//! [`build_stream_client`] is the single construction site for the
//! `reqwest::Client` used by `AnthropicProvider`, `OpenAiProvider`, and
//! `CustomOpenAiProvider`. It wires in two SSRF defences:
//!
//! - **DNS policy resolver** ([`forge_core::http::secure_client_builder`])
//!   — every resolved `SocketAddr` is re-validated against `url_safety`,
//!   so a TTL=0 DNS rebinding answer of `169.254.169.254` cannot reach the
//!   socket layer even if `check_url` originally accepted the hostname.
//! - **Redirect policy** (caller-supplied) — Anthropic/OpenAI pass
//!   `reqwest::redirect::Policy::none()` because the official endpoints do
//!   not redirect on `/v1/messages` or `/v1/chat/completions`, so a
//!   misconfigured proxy or BGP-hijack injecting `302 Location: <internal>`
//!   surfaces as an HTTP error rather than being silently followed.
//!   `CustomOpenAiProvider` passes a per-hop validator that re-runs each
//!   3xx target URL through `forge_core::url_safety::check_url` and caps
//!   the chain length, since user-supplied OpenAI-compatible gateways may
//!   legitimately redirect.

use std::time::{Duration, SystemTime};

use crate::sse::SseError;
use crate::StreamErrorKind;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_TCP_KEEPALIVE: Duration = Duration::from_secs(30);

/// HTTP-layer timeouts applied at `reqwest::ClientBuilder` construction for
/// SSE-streaming providers (Anthropic, OpenAI, OpenAI-compatible custom).
#[derive(Debug, Clone, Copy)]
pub(crate) struct HttpClientConfig {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub tcp_keepalive: Duration,
}

impl HttpClientConfig {
    pub const DEFAULT: HttpClientConfig = HttpClientConfig {
        connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        read_timeout: DEFAULT_READ_TIMEOUT,
        tcp_keepalive: DEFAULT_TCP_KEEPALIVE,
    };
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Build a streaming `reqwest::Client` with the SSE provider hardening
/// posture: per-connect / per-read / TCP keepalive timeouts and the F-644
/// SSRF-safe DNS resolver. The redirect policy is caller-supplied so the
/// official-vendor providers can lock to `Policy::none()` while the
/// custom OpenAI provider can install a per-hop SSRF-validating policy
/// for user-supplied gateways that may legitimately redirect.
pub(crate) fn build_stream_client(
    cfg: &HttpClientConfig,
    redirect: reqwest::redirect::Policy,
) -> reqwest::Client {
    forge_core::http::secure_client_builder()
        .connect_timeout(cfg.connect_timeout)
        .read_timeout(cfg.read_timeout)
        .tcp_keepalive(Some(cfg.tcp_keepalive))
        .redirect(redirect)
        .build()
        .expect("reqwest stream client builder — only fails if no TLS backend is available")
}

/// Map a transport-layer SSE error onto the public terminal-stream classifier
/// surfaced through [`crate::ChatChunk::Error`].
pub(crate) fn map_sse_error(e: &SseError) -> StreamErrorKind {
    match e {
        SseError::LineTooLong => StreamErrorKind::LineTooLong,
        SseError::IdleTimeout => StreamErrorKind::IdleTimeout,
        SseError::WallClockTimeout => StreamErrorKind::WallClockTimeout,
        SseError::Transport(_) => StreamErrorKind::Transport,
    }
}

/// F-749: parse a `Retry-After` HTTP response header value into seconds-from-now.
///
/// RFC 9110 §10.2.3 allows two forms:
///
/// - **delta-seconds** — a non-negative integer count of seconds (`"120"`).
/// - **HTTP-date** — an absolute timestamp (`"Wed, 21 Oct 2026 07:28:00 GMT"`).
///
/// The delta-seconds branch is tried first because it's the common case for
/// 429 replies from Anthropic / OpenAI / Ollama. The HTTP-date branch falls
/// back to [`httpdate::parse_http_date`] and converts the resulting
/// [`SystemTime`] into a delta against `now`.
///
/// Returns `None` for missing headers, non-ASCII / non-UTF-8 byte sequences,
/// negative deltas, deltas past `u32::MAX`, or HTTP-dates that are in the
/// past. The caller is expected to be defensive: a malformed value collapses
/// to "we don't know how long to wait", not an error.
pub(crate) fn parse_retry_after(value: &reqwest::header::HeaderValue) -> Option<u32> {
    let s = value.to_str().ok()?.trim();
    if let Ok(secs) = s.parse::<u32>() {
        return Some(secs);
    }
    let when = httpdate::parse_http_date(s).ok()?;
    let now = SystemTime::now();
    let dur = when.duration_since(now).ok()?;
    u32::try_from(dur.as_secs()).ok()
}

/// UTF-8-safe prefix truncation for HTTP error-body logging. Walks character
/// boundaries so a multi-byte codepoint is never split (a naive `&s[..max]`
/// would panic on input like `"héllo"` when `max == 2`).
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let cut = s
            .char_indices()
            .take_while(|(i, _)| *i < max)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!("{}…", &s[..cut])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_client_config_default_matches_constants() {
        let cfg = HttpClientConfig::default();
        assert_eq!(cfg.connect_timeout, DEFAULT_CONNECT_TIMEOUT);
        assert_eq!(cfg.read_timeout, DEFAULT_READ_TIMEOUT);
        assert_eq!(cfg.tcp_keepalive, DEFAULT_TCP_KEEPALIVE);
    }

    #[test]
    fn build_stream_client_succeeds_with_default_config() {
        // Smoke test — the only failure mode is "no TLS backend available",
        // which would already break every other test in the workspace.
        let _ = build_stream_client(
            &HttpClientConfig::DEFAULT,
            reqwest::redirect::Policy::none(),
        );
    }

    #[test]
    fn truncate_short_input_returns_as_is() {
        assert_eq!(truncate("hi", 100), "hi");
    }

    #[test]
    fn truncate_does_not_panic_on_utf8_boundary() {
        // 'é' is two UTF-8 bytes; byte index 2 falls inside the codepoint, so
        // a naive `&s[..2]` slice panics. The helper must produce a valid
        // prefix instead.
        let s = "héllo";
        let _ = truncate(s, 2);
    }

    #[test]
    fn truncate_appends_ellipsis_when_cut() {
        let out = truncate("abcdefgh", 3);
        assert!(out.ends_with('…'));
        assert!(out.starts_with("abc"));
    }

    #[test]
    fn parse_retry_after_delta_seconds() {
        let h = reqwest::header::HeaderValue::from_static("120");
        assert_eq!(parse_retry_after(&h), Some(120));
    }

    #[test]
    fn parse_retry_after_delta_seconds_zero() {
        let h = reqwest::header::HeaderValue::from_static("0");
        assert_eq!(parse_retry_after(&h), Some(0));
    }

    #[test]
    fn parse_retry_after_delta_seconds_with_whitespace() {
        let h = reqwest::header::HeaderValue::from_static("  42  ");
        assert_eq!(parse_retry_after(&h), Some(42));
    }

    #[test]
    fn parse_retry_after_garbage_returns_none() {
        let h = reqwest::header::HeaderValue::from_static("not-a-number-nor-a-date");
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn parse_retry_after_negative_returns_none() {
        let h = reqwest::header::HeaderValue::from_static("-30");
        // `u32::parse` rejects the leading sign and httpdate rejects the
        // shape — defensively `None`.
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn parse_retry_after_past_http_date_returns_none() {
        let h = reqwest::header::HeaderValue::from_static("Wed, 21 Oct 1970 07:28:00 GMT");
        // The date is in the past, so `duration_since(now)` errors out.
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn parse_retry_after_future_http_date_yields_seconds_from_now() {
        // Build a future timestamp by going +60s from now and formatting it.
        let future = SystemTime::now() + Duration::from_secs(60);
        let formatted = httpdate::fmt_http_date(future);
        let h = reqwest::header::HeaderValue::from_str(&formatted).unwrap();
        let secs = parse_retry_after(&h).expect("future http-date must parse");
        // Allow a wide tolerance — system clock and the test's wall clock
        // race by a few ms. The load-bearing claim is that the result is
        // positive and within an order of magnitude of the input.
        assert!(secs <= 60, "secs={secs} should be <= input window");
        assert!(secs >= 55, "secs={secs} should be near 60");
    }

    #[test]
    fn map_sse_error_covers_all_variants() {
        assert_eq!(
            map_sse_error(&SseError::LineTooLong),
            StreamErrorKind::LineTooLong
        );
        assert_eq!(
            map_sse_error(&SseError::IdleTimeout),
            StreamErrorKind::IdleTimeout
        );
        assert_eq!(
            map_sse_error(&SseError::WallClockTimeout),
            StreamErrorKind::WallClockTimeout
        );
        assert_eq!(
            map_sse_error(&SseError::Transport("x".into())),
            StreamErrorKind::Transport
        );
    }
}
