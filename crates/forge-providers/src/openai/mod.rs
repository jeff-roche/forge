//! OpenAI Chat Completions API provider — SSE-streamed responses against
//! `https://api.openai.com/v1/chat/completions` (or any compatible base URL).
//!
//! Authentication uses the `Authorization: Bearer <api-key>` header. Unlike
//! Anthropic, OpenAI does not pin a wire-version header — the `/v1/` path
//! prefix is the only versioning surface.
//!
//! # Streaming bounds
//!
//! The HTTP-layer client and the SSE decoder enforce a hardening posture:
//! per-line byte cap, inter-event idle timeout, and overall wall-clock budget.
//! Any of these terminates the stream with a typed [`ChatChunk::Error`] —
//! the SSE adapter ([`crate::sse`]) yields a typed [`crate::sse::SseError`]
//! that this module maps onto [`crate::StreamErrorKind`] one-for-one.

use crate::http_util::{self, HttpClientConfig};
use crate::sse::{self, SseError, SseEvent};
use crate::{ChatChunk, ChatRequest, Provider, ProviderAuth, StreamErrorKind};
use bytes::Bytes;
use forge_core::Result;
use futures::stream::{self, BoxStream, StreamExt};
use secrecy::{ExposeSecret, SecretString};

pub mod custom;
pub mod translate;

pub use custom::{AuthShape, CustomOpenAiProvider};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com";

pub struct OpenAiProvider {
    base_url: String,
    /// F-753: hold the constructor-time API key in a `SecretString` so the
    /// plaintext bytes are zeroized on drop, bounding the lifetime of the
    /// key in memory to the provider itself. Mirrors
    /// [`crate::anthropic::AnthropicProvider`]'s zeroization posture; the
    /// F-744 per-turn seam already supplies its credential as a
    /// `SecretString`, so this closes the asymmetry on the fallback path.
    api_key: SecretString,
    model: String,
    /// Optional `max_tokens` cap — OpenAI omits the field when `None`,
    /// letting the server default apply.
    max_tokens: Option<u32>,
    stream_client: reqwest::Client,
    stream_cfg: sse::StreamConfig,
}

/// Hand-rolled `Debug` so the constructor-time API key never surfaces in
/// trace / log output. Mirrors [`crate::anthropic::AnthropicProvider`] and
/// [`crate::openai::custom::CustomOpenAiProvider`].
impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("api_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl OpenAiProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: SecretString::from(api_key.into()),
            model: model.into(),
            max_tokens: None,
            stream_client: http_util::build_stream_client(
                &HttpClientConfig::DEFAULT,
                reqwest::redirect::Policy::none(),
            ),
            stream_cfg: sse::StreamConfig::DEFAULT,
        }
    }

    /// Set an explicit `max_tokens` cap. Builder-style.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Override the SSE decoder bounds. Builder-style; primarily a test
    /// affordance for fast idle-timeout / line-cap regression tests.
    #[doc(hidden)]
    pub fn with_config(mut self, stream_cfg: sse::StreamConfig) -> Self {
        self.stream_cfg = stream_cfg;
        self
    }
}

impl Provider for OpenAiProvider {
    /// F-682: see [`crate::anthropic::AnthropicProvider::chat`] for the
    /// full instrumentation rationale; same shape, different provider
    /// label.
    #[tracing::instrument(
        name = "provider.chat",
        skip_all,
        fields(provider = "openai", model = %self.model),
    )]
    fn chat(
        &self,
        req: ChatRequest,
    ) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send {
        // F-744: route the legacy `chat()` path through the auth seam.
        // `ProviderAuth::None` falls back to the constructor-time
        // credential, preserving pre-F-744 behaviour for callers that
        // haven't wired the seam yet.
        self.chat_with_auth(req, ProviderAuth::None)
    }

    #[tracing::instrument(
        name = "provider.chat",
        skip_all,
        fields(provider = "openai", model = %self.model),
    )]
    fn chat_with_auth(
        &self,
        req: ChatRequest,
        auth_in: ProviderAuth,
    ) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send {
        let body_result = translate::serialize_request(&req, &self.model, self.max_tokens);
        // F-744 zeroization contract: hold the per-turn bearer in a
        // `SecretString` (zeroizes on drop) for the lifetime of the
        // request future, NOT a plain `String`. F-753 promotes the
        // constructor-time `api_key` to `SecretString` as well, so the
        // fallback path is a `SecretString::clone` rather than a plaintext
        // `String::clone` — no intermediate plain-`String` allocation on
        // either path. Mirrors the Anthropic provider's `chat_with_auth`.
        //
        // `Vertex` is meaningless against an OpenAI Chat Completions
        // endpoint and falls back to the constructor value.
        let bearer: SecretString = match &auth_in {
            ProviderAuth::ApiKey(s) => s.clone(),
            ProviderAuth::Vertex(_) | ProviderAuth::None => self.api_key.clone(),
        };
        let auth = vec![(
            reqwest::header::AUTHORIZATION,
            AuthHeaderValue::Bearer(bearer),
        )];
        chat_request(
            self.stream_client.clone(),
            self.base_url.clone(),
            auth,
            body_result,
            self.stream_cfg,
        )
    }
}

/// F-744: secret-bearing header value carried into [`chat_request`].
///
/// The plaintext bytes never escape a [`secrecy::SecretString`] except
/// through [`render`] at the `builder.header(...)` boundary; the rendered
/// `String` is dropped with the per-iteration scope of the header loop.
///
/// `Raw` covers `CustomOpenAiProvider`'s `AuthShape::Header { name }` case
/// where the wire value is the raw key (no `Bearer ` prefix).
pub(crate) enum AuthHeaderValue {
    Bearer(SecretString),
    Raw(SecretString),
}

impl AuthHeaderValue {
    /// Materialize the wire-format header-value string. The returned
    /// `String` is intended to feed `reqwest::RequestBuilder::header` and
    /// then drop immediately.
    fn render(&self) -> String {
        match self {
            AuthHeaderValue::Bearer(s) => format!("Bearer {}", s.expose_secret()),
            AuthHeaderValue::Raw(s) => s.expose_secret().to_string(),
        }
    }
}

/// Shared chat-request pipeline used by both [`OpenAiProvider`] and
/// [`CustomOpenAiProvider`]. Lifted here so the two providers share a single
/// implementation of the OpenAI Chat Completions wire protocol — only the
/// auth-header construction differs between them.
///
/// `auth_headers` is an `(HeaderName, AuthHeaderValue)` list rather than a
/// fully-typed `HeaderMap` so call sites stay terse and the
/// custom-provider's `AuthShape::None` variant maps to an empty `vec![]`
/// without wrestling with `HeaderMap::new()`.
///
/// F-744: the value side is [`AuthHeaderValue`] (wrapping a
/// [`secrecy::SecretString`]) rather than a plain `String`, so the
/// plaintext credential bytes are scrubbed on drop even if the request
/// future is cancelled. Plaintext is materialized only inside the
/// per-iteration `value.render()` call below; the rendered string drops
/// at the end of each loop iteration.
pub(crate) fn chat_request(
    client: reqwest::Client,
    base_url: String,
    auth_headers: Vec<(reqwest::header::HeaderName, AuthHeaderValue)>,
    body_result: std::result::Result<Vec<u8>, serde_json::Error>,
    cfg: sse::StreamConfig,
) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send {
    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));

    async move {
        let body = body_result
            .map_err(|e| anyhow::anyhow!("openai chat body serialization failed: {e}"))?;
        let mut builder = client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        for (name, value) in auth_headers {
            builder = builder.header(name, value.render());
        }
        let resp = builder.body(body).send().await.map_err(|e| {
            // Preserve the source chain so a redirect-policy refusal
            // (CustomOpenAI's SSRF guard, F-646) — which reqwest stores
            // on the error's source — surfaces through anyhow's `{:#}`
            // walker rather than being collapsed to the opaque
            // top-level "error following redirect" message.
            anyhow::Error::new(e).context("openai chat request failed")
        })?;

        let status = resp.status();
        if !status.is_success() {
            // F-749: HTTP-level failure surfaces as a terminal `ChatChunk::Error`
            // carrying the wire status code so the orchestrator can route it
            // onto `TurnErrorKind::Auth` / `RateLimit` / `Server` without
            // re-parsing the message text. See the matching branch on
            // `AnthropicProvider::chat_with_auth`.
            let code = status.as_u16();
            // F-749: parse `Retry-After` for 429s. Both vanilla OpenAI and
            // every OpenAI-compatible gateway routed through this helper
            // (CustomOpenAi, Together, Anyscale, vLLM, LiteLLM, …) emit the
            // header per RFC 9110 §10.2.3 — handling here covers all of them
            // in one place rather than duplicating in each provider.
            let retry_after_secs = if code == 429 {
                resp.headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(http_util::parse_retry_after)
            } else {
                None
            };
            let body = resp.text().await.unwrap_or_default();
            let preview = http_util::truncate(&body, 500);
            let message = format!("openai chat HTTP {status}: {preview}");
            return Ok(Box::pin(stream::once(async move {
                ChatChunk::Error {
                    kind: StreamErrorKind::Transport,
                    message,
                    status: Some(code),
                    retry_after_secs,
                }
            })) as BoxStream<'static, ChatChunk>);
        }

        Ok(decode_openai_stream(resp.bytes_stream(), cfg))
    }
}

/// Decode the raw `bytes` stream into a `ChatChunk` stream by piping through
/// the shared SSE adapter and translating each event payload via
/// [`translate::OpenAiEventAccumulator`].
fn decode_openai_stream<S, E>(
    byte_stream: S,
    cfg: sse::StreamConfig,
) -> BoxStream<'static, ChatChunk>
where
    S: futures::Stream<Item = std::result::Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    let sse_stream = sse::decode_sse_stream(byte_stream, cfg);
    parse_openai_events(sse_stream)
}

/// Translate a stream of `Result<SseEvent, SseError>` into `ChatChunk`s. Public
/// for the integration-test harness so it can drive the parser from a static
/// fixture without an HTTP roundtrip.
#[doc(hidden)]
pub fn parse_openai_events<S>(events: S) -> BoxStream<'static, ChatChunk>
where
    S: futures::Stream<Item = std::result::Result<SseEvent, SseError>> + Send + 'static,
{
    let mut acc = translate::OpenAiEventAccumulator::default();
    let stream = events.flat_map(move |item| {
        let chunks = match item {
            Ok(ev) => acc.consume(&ev),
            Err(e) => vec![ChatChunk::Error {
                kind: http_util::map_sse_error(&e),
                message: e.to_string(),
                status: None,
                retry_after_secs: None,
            }],
        };
        futures::stream::iter(chunks)
    });
    Box::pin(stream.fuse())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_stores_config() {
        let p = OpenAiProvider::new("https://example.com", "sk-test", "gpt-4o");
        assert_eq!(p.base_url, "https://example.com");
        assert_eq!(p.api_key.expose_secret(), "sk-test");
        assert_eq!(p.model, "gpt-4o");
        assert_eq!(p.max_tokens, None);
    }

    #[test]
    fn with_max_tokens_sets_value() {
        let p = OpenAiProvider::new("https://x", "sk", "gpt-4o").with_max_tokens(2048);
        assert_eq!(p.max_tokens, Some(2048));
    }

    /// F-753 zeroization-window parity with `AnthropicProvider`. The
    /// constructor-time API key MUST live in a `SecretString` (zeroizes on
    /// drop) so the plaintext bytes do not survive for the full provider
    /// lifetime. This pins the storage type at compile time: any future
    /// refactor that swaps `SecretString` back for `String` will fail to
    /// type-check here. Mirrors
    /// `crate::anthropic::tests::resolved_auth_owned_uses_secret_string_for_api_key`.
    #[test]
    fn api_key_field_is_secret_string() {
        let p = OpenAiProvider::new("https://x", "sk-test", "gpt-4o");
        let _: &SecretString = &p.api_key;
    }
}
