//! Anthropic Messages API provider — SSE-streamed responses against
//! `https://api.anthropic.com/v1/messages` (or any compatible base URL).
//!
//! Authentication uses the `x-api-key` header (NOT `Authorization: Bearer`)
//! and pins the API version via `anthropic-version: 2023-06-01`.
//!
//! # Streaming bounds
//!
//! The HTTP-layer client and the SSE decoder share Ollama's hardening posture:
//! per-line byte cap, inter-event idle timeout, and overall wall-clock budget.
//! Any of these terminates the stream with a typed [`ChatChunk::Error`] —
//! the SSE adapter ([`crate::sse`]) yields a typed [`crate::sse::SseError`]
//! that this module maps onto [`crate::StreamErrorKind`] one-for-one.

use crate::http_util::{self, HttpClientConfig};
use crate::sse::{self, SseError, SseEvent};
use crate::{ChatChunk, ChatRequest, Provider};
use bytes::Bytes;
use forge_core::Result;
use futures::stream::{BoxStream, StreamExt};

pub mod translate;

/// Anthropic Messages API version pinned by the `anthropic-version` header.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

pub struct AnthropicProvider {
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
    stream_client: reqwest::Client,
    stream_cfg: sse::StreamConfig,
}

impl AnthropicProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        max_tokens: u32,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            max_tokens,
            stream_client: http_util::build_stream_client(&HttpClientConfig::DEFAULT),
            stream_cfg: sse::StreamConfig::DEFAULT,
        }
    }

    /// Override the SSE decoder bounds. Builder-style; primarily a test
    /// affordance for fast idle-timeout / line-cap regression tests.
    #[doc(hidden)]
    pub fn with_config(mut self, stream_cfg: sse::StreamConfig) -> Self {
        self.stream_cfg = stream_cfg;
        self
    }
}

impl Provider for AnthropicProvider {
    fn chat(
        &self,
        req: ChatRequest,
    ) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let body_result = translate::serialize_request(
            &req,
            &self.model,
            self.max_tokens,
            req.parallel_tool_calls_allowed,
        );
        let client = self.stream_client.clone();
        let cfg = self.stream_cfg;
        let api_key = self.api_key.clone();

        async move {
            let body = body_result
                .map_err(|e| anyhow::anyhow!("anthropic chat body serialization failed: {e}"))?;
            let resp = client
                .post(&url)
                .header("x-api-key", api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("anthropic chat request failed: {e}"))?;

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!(
                    "anthropic chat HTTP {status}: {}",
                    http_util::truncate(&body, 500)
                )
                .into());
            }

            Ok(decode_anthropic_stream(resp.bytes_stream(), cfg))
        }
    }
}

/// Decode the raw `bytes` stream into a `ChatChunk` stream by piping through
/// the shared SSE adapter and translating each event payload via
/// [`translate::AnthropicEventAccumulator`].
fn decode_anthropic_stream<S, E>(
    byte_stream: S,
    cfg: sse::StreamConfig,
) -> BoxStream<'static, ChatChunk>
where
    S: futures::Stream<Item = std::result::Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    let sse_stream = sse::decode_sse_stream(byte_stream, cfg);
    parse_anthropic_events(sse_stream)
}

/// Translate a stream of `Result<SseEvent, SseError>` into `ChatChunk`s. Public
/// for the integration-test harness so it can drive the parser from a static
/// fixture without an HTTP roundtrip.
#[doc(hidden)]
pub fn parse_anthropic_events<S>(events: S) -> BoxStream<'static, ChatChunk>
where
    S: futures::Stream<Item = std::result::Result<SseEvent, SseError>> + Send + 'static,
{
    let mut acc = translate::AnthropicEventAccumulator::default();
    let stream = events.flat_map(move |item| {
        let chunks = match item {
            Ok(ev) => acc.consume(&ev),
            Err(e) => vec![ChatChunk::Error {
                kind: http_util::map_sse_error(&e),
                message: e.to_string(),
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
        let p = AnthropicProvider::new("https://example.com", "sk-test", "claude-3-5-sonnet", 4096);
        assert_eq!(p.base_url, "https://example.com");
        assert_eq!(p.api_key, "sk-test");
        assert_eq!(p.model, "claude-3-5-sonnet");
        assert_eq!(p.max_tokens, 4096);
    }

    #[test]
    fn anthropic_version_pinned() {
        assert_eq!(ANTHROPIC_VERSION, "2023-06-01");
    }
}
