//! Server-Sent Events (SSE) decoding adapter.
//!
//! Decodes a byte stream of SSE-framed messages into a stream of typed
//! `(event, data)` pairs per the WHATWG SSE spec. The adapter:
//!
//! - Splits frames on `\n` or `\r\n` (both are spec-legal).
//! - Captures the most-recent `event:` field as the dispatch name; OpenAI
//!   omits it (empty string), Anthropic uses named events like
//!   `content_block_delta`.
//! - Concatenates multiple `data:` lines within a single event with `\n`.
//! - Ignores lines starting with `:` (SSE comments).
//! - Dispatches the buffered event on a blank line.
//!
//! ## Error model
//!
//! The adapter yields `Result<SseEvent, SseError>` so each error variant
//! carries its own type — `LineTooLong`, `IdleTimeout`, `WallClockTimeout`,
//! `Transport`. The provider-layer caller maps these onto
//! [`crate::ChatChunk::Error`] with the matching [`crate::StreamErrorKind`].
//! Keeping the typed `SseError` here means the SSE adapter is reusable from
//! any caller (Anthropic, OpenAI, future providers) without coupling to
//! `ChatChunk`.
//!
//! Bound defaults mirror Ollama's NDJSON decoder so the two transports share
//! a single DoS-resistance posture (1 MiB per line, 30 s idle, 600 s wall
//! clock).

use bytes::{Bytes, BytesMut};
use futures::stream::{BoxStream, StreamExt};
use std::time::Duration;
use tokio_util::codec::FramedRead;
use tokio_util::io::StreamReader;

/// Per-line SSE byte cap (1 MiB). Matches Ollama's NDJSON cap so a hostile
/// peer cannot exhaust memory by streaming a single newline-less line.
pub const DEFAULT_MAX_LINE_BYTES: usize = 1 << 20;
/// Wall-clock gap between consecutive SSE lines. Matches Ollama's idle cap.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Wall-clock cap on the entire SSE stream. Matches Ollama's wall-clock cap.
pub const DEFAULT_WALL_CLOCK_TIMEOUT: Duration = Duration::from_secs(600);

/// Bounds that make the SSE decoder DoS-resistant against a hostile peer.
/// Mirrors `crate::ollama::StreamConfig` — the two configs are deliberately
/// separate types because their callers wire different defaults at different
/// layers, but the defaults themselves are identical.
#[derive(Debug, Clone, Copy)]
pub struct StreamConfig {
    pub max_line_bytes: usize,
    pub idle_timeout: Duration,
    pub wall_clock_timeout: Duration,
}

impl StreamConfig {
    pub const DEFAULT: StreamConfig = StreamConfig {
        max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        idle_timeout: DEFAULT_IDLE_TIMEOUT,
        wall_clock_timeout: DEFAULT_WALL_CLOCK_TIMEOUT,
    };
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One dispatched SSE message.
///
/// `event` is empty when the upstream omitted an `event:` field (typical of
/// OpenAI's chat completions stream). `data` is the raw payload bytes; if
/// the upstream split it across multiple `data:` lines they are joined here
/// with `\n` per the SSE spec.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: String,
    pub data: Bytes,
}

/// Terminal SSE adapter failure. Mirrors `StreamErrorKind` one-for-one so
/// the provider-layer caller can map directly without losing information.
#[derive(Debug)]
pub enum SseError {
    /// One SSE line exceeded `StreamConfig::max_line_bytes`.
    LineTooLong,
    /// No bytes received within `StreamConfig::idle_timeout`.
    IdleTimeout,
    /// Stream exceeded `StreamConfig::wall_clock_timeout`.
    WallClockTimeout,
    /// Transport-level error from the underlying byte stream.
    Transport(String),
}

impl std::fmt::Display for SseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SseError::LineTooLong => write!(f, "sse line exceeded max bytes"),
            SseError::IdleTimeout => write!(f, "sse idle timeout"),
            SseError::WallClockTimeout => write!(f, "sse wall-clock timeout"),
            // Defense-in-depth: redact again on render even though the
            // construction sites already redact. Any future code path that
            // builds a `Transport` from non-redacted text still gets scrubbed
            // before it reaches a caller, the webview, or the event log.
            SseError::Transport(msg) => write!(f, "sse transport: {}", redact(msg)),
        }
    }
}

impl std::error::Error for SseError {}

/// F-648 — strip credential-shaped tokens from error text before it surfaces
/// to a caller, the webview, the event log, or an exported transcript.
///
/// Threat model: lower-level reqwest / std::io errors include the request
/// URL and (occasionally) echoed request headers in their `Display` output.
/// On a custom OpenAI endpoint that authenticates via `?api_key=` query
/// param, that URL carries the API key. This function scrubs known
/// credential shapes before the text becomes the `String` payload of
/// [`SseError::Transport`].
///
/// Patterns redacted:
/// - `Bearer <token>` (case-insensitive)
/// - `Authorization: <value>` / `X-Api-Key: <value>` / `Api-Key: <value>`
/// - `?<credential-name>=<value>` and `&<credential-name>=<value>` query
///   params, where `credential-name` is one of `api_key`, `apikey`, `key`,
///   `access_token`, `token`, `auth`
/// - Userinfo in URLs: `scheme://user:password@host`
/// - Standalone provider-shaped tokens (`sk-ant-...`, `sk-proj-...`, `sk-...`)
///   as a final defense-in-depth pass
///
/// Non-secret diagnostic text (`connection reset by peer (os error 104)`,
/// timeouts, etc.) passes through unchanged.
pub(crate) fn redact(input: &str) -> String {
    // Hand-rolled scanner — avoids pulling a regex crate into the workspace
    // for a problem with a small, fixed pattern set. Each pass scans
    // case-insensitively for a credential keyword, then replaces the
    // following value (token, header value, query value) with `<redacted>`.
    let mut out = input.to_string();
    out = redact_keyword_value(&out, "bearer ", KeywordSep::Whitespace, TokenChars::Bearer);
    out = redact_keyword_value(
        &out,
        "authorization",
        KeywordSep::ColonOrEquals,
        TokenChars::Header,
    );
    out = redact_keyword_value(
        &out,
        "x-api-key",
        KeywordSep::ColonOrEquals,
        TokenChars::Header,
    );
    out = redact_keyword_value(
        &out,
        "x-api_key",
        KeywordSep::ColonOrEquals,
        TokenChars::Header,
    );
    out = redact_keyword_value(
        &out,
        "api-key",
        KeywordSep::ColonOrEquals,
        TokenChars::Header,
    );
    out = redact_keyword_value(
        &out,
        "api_key",
        KeywordSep::ColonOrEquals,
        TokenChars::Header,
    );
    out = redact_query_param(&out, "api_key");
    out = redact_query_param(&out, "api-key");
    out = redact_query_param(&out, "apikey");
    out = redact_query_param(&out, "access_token");
    out = redact_query_param(&out, "access-token");
    out = redact_query_param(&out, "token");
    out = redact_query_param(&out, "key");
    out = redact_query_param(&out, "auth");
    out = redact_url_userinfo(&out);
    out = redact_provider_tokens(&out);
    out
}

#[derive(Clone, Copy)]
enum KeywordSep {
    /// Keyword followed directly by whitespace then the value (`Bearer xxx`).
    Whitespace,
    /// Keyword followed by `:` or `=` (optional whitespace), then the value.
    ColonOrEquals,
}

#[derive(Clone, Copy)]
enum TokenChars {
    /// Token characters acceptable in an HTTP `Bearer` value (RFC 6750
    /// b64token: alnum + `-._~+/=`).
    Bearer,
    /// Header value runs to next whitespace.
    Header,
}

impl TokenChars {
    fn is_value_char(self, c: char) -> bool {
        match self {
            TokenChars::Bearer => {
                c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~' | '+' | '/' | '=')
            }
            // Header values run to whitespace. Strip a trailing `,` or `;`
            // so common log delimiters don't get pulled into the redaction.
            TokenChars::Header => !c.is_whitespace() && !matches!(c, ',' | ';'),
        }
    }
}

/// Find each case-insensitive occurrence of `keyword` and redact the value
/// that follows it (delimiter rules per `sep`, value chars per `value`).
fn redact_keyword_value(input: &str, keyword: &str, sep: KeywordSep, value: TokenChars) -> String {
    let lower = input.to_ascii_lowercase();
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;

    while cursor < input.len() {
        let Some(rel) = lower[cursor..].find(keyword) else {
            out.push_str(&input[cursor..]);
            break;
        };
        let kw_start = cursor + rel;
        let kw_end = kw_start + keyword.len();

        // Reject mid-word matches so `authorization` doesn't fire inside
        // `unauthorization-foo`. Allow start-of-string or non-alnum prefix.
        let prev_ok = kw_start == 0
            || !input.as_bytes()[kw_start - 1].is_ascii_alphanumeric()
                && input.as_bytes()[kw_start - 1] != b'_';
        if !prev_ok {
            out.push_str(&input[cursor..kw_end]);
            cursor = kw_end;
            continue;
        }

        // Walk the separator.
        let mut value_start = kw_end;
        match sep {
            KeywordSep::Whitespace => {
                if value_start >= bytes.len() || !bytes[value_start].is_ascii_whitespace() {
                    out.push_str(&input[cursor..kw_end]);
                    cursor = kw_end;
                    continue;
                }
                while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
                    value_start += 1;
                }
            }
            KeywordSep::ColonOrEquals => {
                while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
                    value_start += 1;
                }
                if value_start >= bytes.len()
                    || (bytes[value_start] != b':' && bytes[value_start] != b'=')
                {
                    out.push_str(&input[cursor..kw_end]);
                    cursor = kw_end;
                    continue;
                }
                value_start += 1;
                while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
                    value_start += 1;
                }
            }
        }

        // Walk the value.
        let mut value_end = value_start;
        while value_end < input.len() {
            // Index char-by-char to honour UTF-8 boundaries.
            let c = input[value_end..].chars().next().unwrap();
            if !value.is_value_char(c) {
                break;
            }
            value_end += c.len_utf8();
        }

        if value_end == value_start {
            // Keyword present but no value after the separator — pass through.
            out.push_str(&input[cursor..value_end]);
            cursor = value_end;
            continue;
        }

        out.push_str(&input[cursor..value_start]);
        out.push_str("<redacted>");
        cursor = value_end;
    }

    out
}

/// Redact `?<name>=<value>` and `&<name>=<value>` query parameters.
fn redact_query_param(input: &str, name: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;

    while cursor < input.len() {
        // Match either `?name=` or `&name=` case-insensitively.
        let needle_q = format!("?{name}=");
        let needle_a = format!("&{name}=");
        let q = lower[cursor..].find(&needle_q);
        let a = lower[cursor..].find(&needle_a);
        let Some((rel, prefix_len)) = (match (q, a) {
            (Some(q), Some(a)) if q < a => Some((q, needle_q.len())),
            (Some(_), Some(a)) => Some((a, needle_a.len())),
            (Some(q), None) => Some((q, needle_q.len())),
            (None, Some(a)) => Some((a, needle_a.len())),
            (None, None) => None,
        }) else {
            out.push_str(&input[cursor..]);
            break;
        };
        let value_start = cursor + rel + prefix_len;
        let bytes = input.as_bytes();
        let mut value_end = value_start;
        while value_end < bytes.len() {
            let b = bytes[value_end];
            if b == b'&' || b.is_ascii_whitespace() || b == b'#' {
                break;
            }
            value_end += 1;
        }
        if value_end == value_start {
            out.push_str(&input[cursor..value_end]);
            cursor = value_end;
            continue;
        }
        out.push_str(&input[cursor..value_start]);
        out.push_str("<redacted>");
        cursor = value_end;
    }

    out
}

/// Redact userinfo in URL form: `scheme://user:password@host`. The userinfo
/// pair (everything between `://` and the first `@`) is replaced with the
/// fixed marker. We only fire when both `://` and `@` are present and the
/// span between them is short enough to plausibly be userinfo (< 256 chars,
/// no whitespace, no `/`).
fn redact_url_userinfo(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while cursor < input.len() {
        let Some(rel) = input[cursor..].find("://") else {
            out.push_str(&input[cursor..]);
            break;
        };
        let scheme_end = cursor + rel + 3;
        out.push_str(&input[cursor..scheme_end]);
        // Look for `@` before the next `/`, whitespace, or 256 chars.
        let bytes = input.as_bytes();
        let mut probe = scheme_end;
        let mut at_pos: Option<usize> = None;
        while probe < bytes.len() && probe - scheme_end < 256 {
            let b = bytes[probe];
            if b == b'@' {
                at_pos = Some(probe);
                break;
            }
            if b == b'/' || b.is_ascii_whitespace() {
                break;
            }
            probe += 1;
        }
        if let Some(at) = at_pos {
            // Only redact if a `:` exists in the userinfo span — otherwise
            // it's just a username without a credential. (We could redact
            // anyway; bare-username URLs are uncommon enough that leaving
            // them alone preserves diagnostic value.)
            if input[scheme_end..at].contains(':') {
                out.push_str("<redacted>@");
                cursor = at + 1;
                continue;
            }
        }
        cursor = scheme_end;
    }
    out
}

/// Redact provider-shaped standalone tokens (`sk-ant-...`, `sk-proj-...`,
/// `sk-...`) anywhere in the input. Defense-in-depth in case a credential
/// appears outside any of the keyword/header/query-param contexts above.
fn redact_provider_tokens(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        // Anchor on a non-alnum boundary (or string start) followed by `sk-`.
        let prev_boundary = cursor == 0
            || (!bytes[cursor - 1].is_ascii_alphanumeric() && bytes[cursor - 1] != b'_');
        if prev_boundary && cursor + 3 <= bytes.len() && &bytes[cursor..cursor + 3] == b"sk-" {
            // Find the end of the token — alnum + `-_.`.
            let mut end = cursor + 3;
            while end < bytes.len() {
                let b = bytes[end];
                if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' {
                    end += 1;
                } else {
                    break;
                }
            }
            // Require at least 6 characters of body to avoid false positives
            // on short identifiers like `sk-test`.
            if end - cursor >= 9 {
                out.push_str("<redacted>");
                cursor = end;
                continue;
            }
        }
        out.push(bytes[cursor] as char);
        cursor += 1;
    }
    out
}

/// Decode a byte stream of SSE-framed messages into typed events.
///
/// Terminal failures (line cap exceeded, idle window elapsed, wall-clock
/// budget elapsed, transport error) yield a single `Err(SseError::*)` and
/// close the stream. The caller is responsible for translating these onto
/// its own terminal error shape.
pub fn decode_sse_stream<S, E>(
    byte_stream: S,
    cfg: StreamConfig,
) -> BoxStream<'static, Result<SseEvent, SseError>>
where
    S: futures::Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    // F-648: scrub credentials from upstream-error text before it enters
    // the io::Error chain. The reqwest error for a custom OpenAI endpoint
    // that authenticates via `?api_key=` query param includes the URL with
    // the key in its Display output; if we let it through the SSE adapter
    // would surface that key in `SseError::Transport`.
    let pinned =
        Box::pin(byte_stream.map(|r| r.map_err(|e| std::io::Error::other(redact(&e.to_string())))));
    let reader = StreamReader::new(pinned);
    let framed = FramedRead::new(reader, SseLineCodec::new(cfg.max_line_bytes));

    let deadline = tokio::time::Instant::now() + cfg.wall_clock_timeout;
    let state = DecoderState {
        framed,
        cfg,
        deadline,
        terminated: false,
        event: String::new(),
        data: BytesMut::new(),
        data_started: false,
        has_field: false,
    };

    let stream = futures::stream::unfold(state, |mut s| async move {
        if s.terminated {
            return None;
        }

        loop {
            let now = tokio::time::Instant::now();
            if now >= s.deadline {
                s.terminated = true;
                return Some((Err(SseError::WallClockTimeout), s));
            }

            let idle = s.cfg.idle_timeout.min(s.deadline - now);
            match tokio::time::timeout(idle, s.framed.next()).await {
                Err(_) => {
                    s.terminated = true;
                    let err = if tokio::time::Instant::now() >= s.deadline {
                        SseError::WallClockTimeout
                    } else {
                        SseError::IdleTimeout
                    };
                    return Some((Err(err), s));
                }
                Ok(None) => return None,
                Ok(Some(Err(e))) => {
                    s.terminated = true;
                    let err = match e {
                        SseLineError::MaxLineLengthExceeded => SseError::LineTooLong,
                        // Already redacted at the byte-stream boundary
                        // above, but re-apply at the SseError construction
                        // site as belt-and-suspenders against a future caller
                        // that constructs `SseLineError::Io` from a non-redacted
                        // source (e.g. a different transport adapter).
                        SseLineError::Io(io) => SseError::Transport(redact(&io.to_string())),
                    };
                    return Some((Err(err), s));
                }
                Ok(Some(Ok(line))) => {
                    if let Some(event) = handle_line(&mut s, &line) {
                        return Some((Ok(event), s));
                    }
                }
            }
        }
    });

    Box::pin(stream.fuse())
}

struct DecoderState<R> {
    framed: FramedRead<R, SseLineCodec>,
    cfg: StreamConfig,
    deadline: tokio::time::Instant,
    terminated: bool,
    event: String,
    data: BytesMut,
    data_started: bool,
    has_field: bool,
}

/// Process one decoded SSE line. Returns `Some(event)` when a blank line
/// dispatches an accumulated event; `None` when the line is buffered
/// (field, comment, or empty without a pending event).
fn handle_line<R>(state: &mut DecoderState<R>, line: &Bytes) -> Option<SseEvent> {
    let line = strip_cr(line);

    if line.is_empty() {
        // Blank line — dispatch only if we have at least one field.
        if !state.has_field {
            return None;
        }
        let event = SseEvent {
            event: std::mem::take(&mut state.event),
            data: state.data.split().freeze(),
        };
        state.data_started = false;
        state.has_field = false;
        return Some(event);
    }

    // Comment line.
    if line[0] == b':' {
        return None;
    }

    // Field parsing per SSE spec: split on the first `:`. The portion after
    // an optional single space is the value. A line with no `:` treats the
    // whole line as the field name with an empty value.
    let (field, value) = match line.iter().position(|b| *b == b':') {
        Some(idx) => {
            let f = &line[..idx];
            let mut v = &line[idx + 1..];
            if let Some((b' ', rest)) = v.split_first() {
                v = rest;
            }
            (f, v)
        }
        None => (line, &b""[..]),
    };

    state.has_field = true;

    match field {
        b"event" => {
            // Last `event:` wins. Non-UTF-8 collapses to empty (the spec
            // allows replacement, but every real provider sends ASCII names).
            state.event = std::str::from_utf8(value).unwrap_or("").to_string();
        }
        b"data" => {
            if state.data_started {
                state.data.extend_from_slice(b"\n");
            }
            state.data.extend_from_slice(value);
            state.data_started = true;
        }
        _ => {
            // `id:` / `retry:` / unknown fields — buffered (count as a
            // field for dispatch) but otherwise ignored. SSE consumers in
            // this codebase don't use them.
        }
    }

    None
}

fn strip_cr(line: &Bytes) -> &[u8] {
    let bytes = line.as_ref();
    match bytes.last() {
        Some(b'\r') => &bytes[..bytes.len() - 1],
        _ => bytes,
    }
}

// ── SseLineCodec ──────────────────────────────────────────────────────────────
//
// Lifted from `ollama::BytesLineCodec` because the line-framing requirements
// are identical (\n-delimited, byte-cap-bounded, yields `Bytes` slices over
// the codec's buffer). The two crates intentionally do NOT share a codec —
// pulling Ollama's into a shared module would be churn outside this task's
// DoD. Diverging behavior between the two would be caught by the unit
// tests in each module.

#[derive(Debug)]
struct SseLineCodec {
    max_line_bytes: usize,
    next_index: usize,
    discarding: bool,
}

#[derive(Debug)]
enum SseLineError {
    MaxLineLengthExceeded,
    Io(std::io::Error),
}

impl From<std::io::Error> for SseLineError {
    fn from(e: std::io::Error) -> Self {
        SseLineError::Io(e)
    }
}

impl std::fmt::Display for SseLineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SseLineError::MaxLineLengthExceeded => write!(f, "max line length exceeded"),
            SseLineError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SseLineError {}

impl SseLineCodec {
    fn new(max_line_bytes: usize) -> Self {
        Self {
            max_line_bytes,
            next_index: 0,
            discarding: false,
        }
    }
}

impl tokio_util::codec::Decoder for SseLineCodec {
    type Item = Bytes;
    type Error = SseLineError;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Bytes>, Self::Error> {
        use bytes::Buf;
        if self.discarding {
            if let Some(nl_offset) = buf.iter().position(|b| *b == b'\n') {
                buf.advance(nl_offset + 1);
                self.discarding = false;
                self.next_index = 0;
            } else {
                let len = buf.len();
                buf.advance(len);
                return Ok(None);
            }
        }

        let read_to = std::cmp::min(self.max_line_bytes.saturating_add(1), buf.len());
        let newline = buf[self.next_index..read_to]
            .iter()
            .position(|b| *b == b'\n');

        match newline {
            Some(offset) => {
                let nl_index = self.next_index + offset;
                let mut line = buf.split_to(nl_index + 1);
                line.truncate(line.len() - 1);
                self.next_index = 0;
                Ok(Some(line.freeze()))
            }
            None if buf.len() > self.max_line_bytes => {
                self.discarding = true;
                self.next_index = 0;
                Err(SseLineError::MaxLineLengthExceeded)
            }
            None => {
                self.next_index = buf.len();
                Ok(None)
            }
        }
    }

    fn decode_eof(&mut self, buf: &mut BytesMut) -> Result<Option<Bytes>, Self::Error> {
        match self.decode(buf)? {
            Some(line) => Ok(Some(line)),
            None => {
                if buf.is_empty() || self.discarding {
                    self.discarding = false;
                    self.next_index = 0;
                    Ok(None)
                } else {
                    let line = buf.split_to(buf.len()).freeze();
                    self.next_index = 0;
                    Ok(Some(line))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, StreamExt};
    use std::convert::Infallible;

    fn bytes_stream(
        chunks: Vec<&'static [u8]>,
    ) -> impl futures::Stream<Item = Result<Bytes, Infallible>> {
        stream::iter(chunks.into_iter().map(|c| Ok(Bytes::from_static(c))))
    }

    async fn collect_events(input: &'static [u8]) -> Vec<Result<SseEvent, SseError>> {
        let s = bytes_stream(vec![input]);
        decode_sse_stream(s, StreamConfig::DEFAULT).collect().await
    }

    #[tokio::test]
    async fn single_event_with_one_data_line() {
        let out = collect_events(b"event: foo\ndata: hello\n\n").await;
        assert_eq!(out.len(), 1, "expected exactly one event, got {out:?}");
        let ev = out[0].as_ref().expect("event ok");
        assert_eq!(ev.event, "foo");
        assert_eq!(ev.data.as_ref(), b"hello");
    }

    #[tokio::test]
    async fn multiple_data_lines_concatenate_with_newline() {
        let out = collect_events(b"event: chunk\ndata: line1\ndata: line2\ndata: line3\n\n").await;
        assert_eq!(out.len(), 1);
        let ev = out[0].as_ref().expect("event ok");
        assert_eq!(ev.event, "chunk");
        assert_eq!(ev.data.as_ref(), b"line1\nline2\nline3");
    }

    #[tokio::test]
    async fn omitted_event_field_yields_empty_string() {
        let out = collect_events(b"data: payload\n\n").await;
        assert_eq!(out.len(), 1);
        let ev = out[0].as_ref().expect("event ok");
        assert_eq!(ev.event, "");
        assert_eq!(ev.data.as_ref(), b"payload");
    }

    #[tokio::test]
    async fn comment_lines_are_ignored() {
        let out = collect_events(b": this is a comment\ndata: real\n: another comment\n\n").await;
        assert_eq!(out.len(), 1, "comments must not dispatch events");
        let ev = out[0].as_ref().expect("event ok");
        assert_eq!(ev.event, "");
        assert_eq!(ev.data.as_ref(), b"real");
    }

    #[tokio::test]
    async fn crlf_line_endings_work() {
        let out = collect_events(b"event: crlf\r\ndata: hello\r\n\r\n").await;
        assert_eq!(
            out.len(),
            1,
            "CRLF framing must produce one event, got {out:?}"
        );
        let ev = out[0].as_ref().expect("event ok");
        assert_eq!(ev.event, "crlf");
        assert_eq!(ev.data.as_ref(), b"hello");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn idle_timeout_yields_terminal_idle_error() {
        // Yield one valid event, then never produce more bytes.
        let (tx, rx) = futures::channel::mpsc::unbounded::<Result<Bytes, Infallible>>();
        tx.unbounded_send(Ok(Bytes::from_static(b"data: hi\n\n")))
            .unwrap();
        // Hold tx open forever — no further bytes arrive.
        let _hold_open = tx;

        let cfg = StreamConfig {
            max_line_bytes: 1024,
            idle_timeout: Duration::from_millis(80),
            wall_clock_timeout: Duration::from_secs(30),
        };
        let mut out = decode_sse_stream(rx, cfg);

        let first = tokio::time::timeout(Duration::from_secs(1), out.next())
            .await
            .expect("first event must arrive promptly")
            .expect("at least one event");
        let ev = first.expect("first item is the valid event");
        assert_eq!(ev.data.as_ref(), b"hi");

        let second = tokio::time::timeout(Duration::from_secs(2), out.next())
            .await
            .expect("must terminate via idle timeout, not hang")
            .expect("must yield terminal error");
        assert!(
            matches!(second, Err(SseError::IdleTimeout)),
            "expected IdleTimeout, got {second:?}"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), out.next())
                .await
                .expect("stream must close")
                .is_none(),
            "stream must close after terminal error"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wall_clock_timeout_yields_terminal_error() {
        // A drip-feeder that keeps the idle timer happy but exceeds the
        // wall-clock budget. Send an empty heartbeat every 30 ms so the
        // 80 ms idle window never trips, but the 200 ms wall-clock will.
        let (tx, rx) = futures::channel::mpsc::unbounded::<Result<Bytes, Infallible>>();
        tokio::spawn(async move {
            for _ in 0..50 {
                if tx
                    .unbounded_send(Ok(Bytes::from_static(b": heartbeat\n")))
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        });

        let cfg = StreamConfig {
            max_line_bytes: 1024,
            idle_timeout: Duration::from_millis(80),
            wall_clock_timeout: Duration::from_millis(200),
        };
        let mut out = decode_sse_stream(rx, cfg);

        let result = tokio::time::timeout(Duration::from_secs(2), out.next())
            .await
            .expect("must terminate via wall-clock, not hang")
            .expect("must yield terminal error");
        assert!(
            matches!(result, Err(SseError::WallClockTimeout)),
            "expected WallClockTimeout, got {result:?}"
        );
        assert!(
            out.next().await.is_none(),
            "stream must close after terminal error"
        );
    }

    #[tokio::test]
    async fn transport_error_yields_terminal_transport_error() {
        #[derive(Debug)]
        struct BadIo;
        impl std::fmt::Display for BadIo {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "boom")
            }
        }
        impl std::error::Error for BadIo {}

        let stream = stream::iter(vec![
            Ok::<Bytes, BadIo>(Bytes::from_static(b"data: ok\n\n")),
            Err(BadIo),
        ]);

        let mut out = decode_sse_stream(stream, StreamConfig::DEFAULT);
        let first = out.next().await.expect("first event").expect("ok event");
        assert_eq!(first.data.as_ref(), b"ok");

        let second = out.next().await.expect("transport error");
        match second {
            Err(SseError::Transport(msg)) => {
                assert!(
                    msg.contains("boom"),
                    "transport error must surface source: {msg}"
                );
            }
            other => panic!("expected Transport error, got {other:?}"),
        }
        assert!(
            out.next().await.is_none(),
            "stream must close after terminal error"
        );
    }

    #[tokio::test]
    async fn line_exceeding_cap_yields_terminal_line_too_long() {
        // 200 bytes of `a` followed by `\n` — cap of 100 must terminate.
        let big: Vec<u8> = std::iter::repeat_n(b'a', 200)
            .chain(std::iter::once(b'\n'))
            .collect();
        let stream = stream::iter(vec![Ok::<_, Infallible>(Bytes::from(big))]);
        let cfg = StreamConfig {
            max_line_bytes: 100,
            ..StreamConfig::DEFAULT
        };
        let mut out = decode_sse_stream(stream, cfg);
        let first = out.next().await.expect("must yield terminal error");
        assert!(
            matches!(first, Err(SseError::LineTooLong)),
            "expected LineTooLong, got {first:?}"
        );
        // Stream must be closed after the terminal error.
        assert!(
            out.next().await.is_none(),
            "stream must close after terminal error"
        );
    }

    // ── Credential redaction (F-648) ──────────────────────────────────────
    //
    // `SseError::Transport` previously carried free-form `std::io::Error`
    // text. On a TLS handshake / connection-reset error against a custom
    // OpenAI endpoint that authenticates via `?api_key=` query param, the
    // reqwest error's `Display` includes the request URL — so the API key
    // landed in the error message, then in `ChatChunk::Error`, the webview,
    // the event log, and the exported transcript.
    //
    // The fix is a `redact()` pass at the construction sites (and the
    // `Display` impl as a defense-in-depth final filter). Each test below
    // asserts a specific credential shape is scrubbed before the text
    // reaches a caller.

    #[test]
    fn redact_strips_bearer_token() {
        let input = "error sending request to https://api.example.com/v1: Authorization: Bearer sk-proj-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789";
        let out = redact(input);
        assert!(
            !out.contains("sk-proj-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789"),
            "bearer token must be redacted, got: {out}"
        );
        assert!(
            !out.to_lowercase().contains("bearer sk-"),
            "bearer prefix + key must be scrubbed, got: {out}"
        );
    }

    #[test]
    fn redact_strips_authorization_header_value() {
        let input = "header authorization: sk-ant-api03-secretvalue123 caused 401";
        let out = redact(input);
        assert!(
            !out.contains("sk-ant-api03-secretvalue123"),
            "authorization header value must be redacted, got: {out}"
        );
    }

    #[test]
    fn redact_strips_x_api_key_header() {
        let input = "request header x-api-key: my-secret-key-xyz failed";
        let out = redact(input);
        assert!(
            !out.contains("my-secret-key-xyz"),
            "x-api-key header value must be redacted, got: {out}"
        );
    }

    #[test]
    fn redact_strips_api_key_query_param() {
        let input =
            "request to https://example.com/v1/chat?api_key=sk-leak-12345&model=gpt-4 failed";
        let out = redact(input);
        assert!(
            !out.contains("sk-leak-12345"),
            "api_key query param must be redacted, got: {out}"
        );
    }

    #[test]
    fn redact_strips_apikey_query_param_no_underscore() {
        let input = "https://host/path?apikey=topsecret reset by peer";
        let out = redact(input);
        assert!(!out.contains("topsecret"), "apikey value redacted: {out}");
    }

    #[test]
    fn redact_strips_access_token_query_param() {
        let input = "https://host/?access_token=abc123def456 timed out";
        let out = redact(input);
        assert!(
            !out.contains("abc123def456"),
            "access_token redacted: {out}"
        );
    }

    #[test]
    fn redact_strips_url_userinfo() {
        let input = "connection refused for https://user:hunter2@host.example.com:443/v1";
        let out = redact(input);
        assert!(
            !out.contains("hunter2"),
            "url userinfo password must be redacted, got: {out}"
        );
        assert!(
            !out.contains("user:hunter2"),
            "url userinfo pair must be redacted, got: {out}"
        );
    }

    #[test]
    fn redact_strips_anthropic_sk_ant_token() {
        let input = "io error: tls handshake failed for token sk-ant-api03-AbCdEfGhIjKlMnOp";
        let out = redact(input);
        assert!(
            !out.contains("sk-ant-api03-AbCdEfGhIjKlMnOp"),
            "anthropic-shaped token must be redacted, got: {out}"
        );
    }

    #[test]
    fn redact_preserves_non_secret_diagnostics() {
        let input = "connection reset by peer (os error 104)";
        let out = redact(input);
        assert_eq!(
            out, input,
            "non-secret diagnostic text must pass through unchanged"
        );
    }

    #[test]
    fn sse_error_transport_display_redacts_secrets() {
        // Defense-in-depth: even if an upstream wrapping path forgets to
        // redact at the construction site, the Display impl scrubs.
        let leaky = SseError::Transport(
            "io error to https://api.example.com/v1?api_key=sk-LEAK-9999".to_string(),
        );
        let rendered = leaky.to_string();
        assert!(
            !rendered.contains("sk-LEAK-9999"),
            "Display must redact secrets as a final safety net, got: {rendered}"
        );
    }

    #[tokio::test]
    async fn transport_error_text_does_not_leak_api_key() {
        // DoD checkbox: regression test that injects an error with
        // credential-shaped content and asserts the surfaced error string
        // contains no secret. Drives the full SSE pipeline so we catch a
        // leak from any of the construction sites.
        #[derive(Debug)]
        struct LeakyIo;
        impl std::fmt::Display for LeakyIo {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(
                    f,
                    "io error sending request to https://api.example.com/v1?api_key=sk-leak-9876543210 caused by tls handshake failure"
                )
            }
        }
        impl std::error::Error for LeakyIo {}

        let stream = stream::iter(vec![
            Ok::<Bytes, LeakyIo>(Bytes::from_static(b"data: ok\n\n")),
            Err(LeakyIo),
        ]);

        let mut out = decode_sse_stream(stream, StreamConfig::DEFAULT);
        let _first = out.next().await.expect("first event").expect("ok event");

        let second = out.next().await.expect("transport error");
        let surfaced = match second {
            Err(ref e) => {
                let display = format!("{e}");
                let debug = format!("{e:?}");
                format!("{display} || {debug}")
            }
            Ok(_) => panic!("expected error, got ok"),
        };
        assert!(
            !surfaced.contains("sk-leak-9876543210"),
            "API key must not leak through SseError, got: {surfaced}"
        );
    }

    #[tokio::test]
    async fn multiple_events_yield_in_order() {
        let input = b"event: first\ndata: 1\n\nevent: second\ndata: 2\n\nevent: third\ndata: 3\n\n";
        let out = collect_events(input).await;
        assert_eq!(out.len(), 3, "expected three events, got {out:?}");
        let names: Vec<&str> = out
            .iter()
            .map(|r| r.as_ref().unwrap().event.as_str())
            .collect();
        assert_eq!(names, vec!["first", "second", "third"]);
        let datas: Vec<&[u8]> = out
            .iter()
            .map(|r| r.as_ref().unwrap().data.as_ref())
            .collect();
        assert_eq!(datas, vec![&b"1"[..], &b"2"[..], &b"3"[..]]);
    }
}
