//! Shared HTTP / SSE helpers used by the Anthropic and OpenAI providers.
//!
//! Both providers run an identical streaming-HTTP posture (per-line cap,
//! inter-event idle timeout, wall-clock budget) and produce identical
//! [`crate::StreamErrorKind`] envelopes when the SSE adapter aborts. F-583
//! and F-584 shipped near-identical helpers in parallel; this module is the
//! single home those helpers consolidate into.
//!
//! Ollama keeps its own [`crate::ollama::ClientConfig`] and helpers — its
//! public-API surface (used by integration tests) and second `request_client`
//! both diverge from the SSE-streaming providers and were intentionally left
//! untouched by the F-679 refactor.
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
//! - **No-redirect policy** (`reqwest::redirect::Policy::none`) — a
//!   misconfigured proxy or BGP-hijack injecting `302 Location: <internal>`
//!   surfaces to the caller as an HTTP error rather than being silently
//!   followed. The official-vendor APIs (Anthropic, OpenAI) do not redirect
//!   on `/v1/messages` or `/v1/chat/completions`, so disabling redirects
//!   has no behavioural cost.

use std::time::Duration;

use crate::sse::SseError;
use crate::StreamErrorKind;

pub(crate) const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const DEFAULT_TCP_KEEPALIVE: Duration = Duration::from_secs(30);

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
/// posture: per-connect / per-read / TCP keepalive timeouts, the F-644
/// SSRF-safe DNS resolver, and a no-redirect policy (F-647). The official
/// Anthropic and OpenAI endpoints do not return 3xx on the streaming
/// chat paths, so refusing to follow redirects costs nothing in normal
/// operation while closing the redirect-to-internal-IP smuggling vector.
pub(crate) fn build_stream_client(cfg: &HttpClientConfig) -> reqwest::Client {
    forge_core::http::secure_client_builder()
        .connect_timeout(cfg.connect_timeout)
        .read_timeout(cfg.read_timeout)
        .tcp_keepalive(Some(cfg.tcp_keepalive))
        .redirect(reqwest::redirect::Policy::none())
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
        let _ = build_stream_client(&HttpClientConfig::DEFAULT);
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
