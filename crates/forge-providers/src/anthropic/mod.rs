//! Anthropic Messages API provider — SSE-streamed responses against
//! `https://api.anthropic.com/v1/messages` (or any compatible base URL).
//!
//! Authentication uses the `x-api-key` header (NOT `Authorization: Bearer`)
//! and pins the API version via `anthropic-version: 2023-06-01`.
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
use crate::{ChatChunk, ChatRequest, Provider};
use bytes::Bytes;
use forge_core::Result;
use futures::stream::{BoxStream, StreamExt};

pub mod translate;

/// Anthropic Messages API version pinned by the `anthropic-version` header.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Anthropic-version header for direct API calls. Vertex AI uses a
/// different version string — see [`VERTEX_ANTHROPIC_VERSION`].
pub const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

/// Authentication mode for [`AnthropicProvider`]. Selects between direct
/// Anthropic API calls (`ApiKey`) and Google Vertex AI (`Vertex`). The
/// Vertex variant retargets the base URL and replaces `x-api-key` with
/// `Authorization: Bearer <gcloud-access-token>`.
#[derive(Clone, Debug)]
pub enum AuthMode {
    /// Direct Anthropic API: `x-api-key: <api_key>` against
    /// `https://api.anthropic.com/v1/messages`.
    ApiKey { api_key: String },
    /// Google Vertex AI: gcloud Application Default Credentials supply
    /// an access token via shelling out to `gcloud auth
    /// application-default print-access-token`. The user runs
    /// `gcloud auth application-default login` once outside Forge to set
    /// this up.
    Vertex {
        /// GCP project hosting the Anthropic publisher.
        project: String,
        /// Vertex region (e.g. `us-central1`).
        region: String,
    },
}

pub struct AnthropicProvider {
    base_url: String,
    auth: AuthMode,
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
        Self::with_auth(
            base_url,
            AuthMode::ApiKey {
                api_key: api_key.into(),
            },
            model,
            max_tokens,
        )
    }

    /// Construct an `AnthropicProvider` with an explicit [`AuthMode`].
    /// The Vertex variant ignores `base_url` and derives its URL from
    /// `project` + `region`; the ApiKey variant uses `base_url` directly.
    pub fn with_auth(
        base_url: impl Into<String>,
        auth: AuthMode,
        model: impl Into<String>,
        max_tokens: u32,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            auth,
            model: model.into(),
            max_tokens,
            stream_client: http_util::build_stream_client(
                &HttpClientConfig::DEFAULT,
                reqwest::redirect::Policy::none(),
            ),
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

    /// Build the request URL for the configured auth mode. Direct API
    /// posts to `<base_url>/v1/messages`; Vertex posts to the
    /// region-scoped publisher endpoint.
    pub fn request_url(&self) -> String {
        match &self.auth {
            AuthMode::ApiKey { .. } => {
                format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
            }
            AuthMode::Vertex { project, region } => format!(
                "https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/anthropic/models/{model}:streamRawPredict",
                model = self.model,
            ),
        }
    }
}

/// Acquire a Vertex AI access token by shelling out to
/// `gcloud auth application-default print-access-token`. The user must
/// have run `gcloud auth application-default login` once outside Forge
/// to populate ADC. Tokens are valid ~1h; callers should refresh on
/// auth failures rather than caching aggressively.
///
/// Returns the raw access token string on stdout. Wrapped errors carry
/// the gcloud stderr verbatim so the user sees actionable messages
/// (e.g. "Reauthentication failed", "command not found").
pub fn fetch_vertex_access_token() -> anyhow::Result<String> {
    let output = std::process::Command::new("gcloud")
        .args(["auth", "application-default", "print-access-token"])
        .output()
        .map_err(|e| anyhow::anyhow!("gcloud not available: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!("gcloud print-access-token failed: {stderr}");
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        anyhow::bail!("gcloud print-access-token returned empty token");
    }
    Ok(token)
}

impl Provider for AnthropicProvider {
    /// F-682: `#[instrument]` enables ops-level latency attribution for the
    /// per-turn chat path. `skip_all` keeps the request body (and any
    /// large transcript fragment inside it) out of the span fields;
    /// `provider = "anthropic"` and `model` are the only attributes
    /// surfaced on the span. The exporter, when wired, sees one span
    /// per `chat` call covering the full request → first byte → stream
    /// duration.
    #[tracing::instrument(
        name = "provider.chat",
        skip_all,
        fields(provider = "anthropic", model = %self.model),
    )]
    fn chat(
        &self,
        req: ChatRequest,
    ) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send {
        let url = self.request_url();
        let body_result = translate::serialize_request(
            &req,
            &self.model,
            self.max_tokens,
            req.parallel_tool_calls_allowed,
        );
        let client = self.stream_client.clone();
        let cfg = self.stream_cfg;
        let auth = self.auth.clone();

        async move {
            let body = body_result
                .map_err(|e| anyhow::anyhow!("anthropic chat body serialization failed: {e}"))?;
            // Build auth-mode-specific request headers. Vertex shells
            // out to gcloud for an ADC access token; failures here
            // surface as the verbatim gcloud stderr so the user can act
            // on them.
            let mut builder = client.post(&url);
            match &auth {
                AuthMode::ApiKey { api_key } => {
                    builder = builder
                        .header("x-api-key", api_key.as_str())
                        .header("anthropic-version", ANTHROPIC_VERSION);
                }
                AuthMode::Vertex { .. } => {
                    let token = tokio::task::spawn_blocking(fetch_vertex_access_token)
                        .await
                        .map_err(|e| anyhow::anyhow!("vertex token join failed: {e}"))?
                        .map_err(|e| anyhow::anyhow!("vertex auth: {e}"))?;
                    builder = builder
                        .header(
                            reqwest::header::AUTHORIZATION,
                            format!("Bearer {token}"),
                        )
                        .header("anthropic-version", VERTEX_ANTHROPIC_VERSION);
                }
            }
            let resp = builder
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
        match &p.auth {
            AuthMode::ApiKey { api_key } => assert_eq!(api_key, "sk-test"),
            AuthMode::Vertex { .. } => panic!("expected ApiKey auth"),
        }
        assert_eq!(p.model, "claude-3-5-sonnet");
        assert_eq!(p.max_tokens, 4096);
    }

    #[test]
    fn anthropic_version_pinned() {
        assert_eq!(ANTHROPIC_VERSION, "2023-06-01");
        assert_eq!(VERTEX_ANTHROPIC_VERSION, "vertex-2023-10-16");
    }

    #[test]
    fn api_key_request_url_uses_base() {
        let p = AnthropicProvider::new("https://api.anthropic.com", "k", "claude-3", 4096);
        assert_eq!(p.request_url(), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn api_key_request_url_trims_trailing_slash() {
        let p = AnthropicProvider::new("https://api.anthropic.com/", "k", "claude-3", 4096);
        assert_eq!(p.request_url(), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn vertex_request_url_targets_region_publisher() {
        let p = AnthropicProvider::with_auth(
            // base_url is ignored in Vertex mode.
            "https://example.com",
            AuthMode::Vertex {
                project: "my-proj".to_string(),
                region: "us-central1".to_string(),
            },
            "claude-3-5-sonnet@20241022",
            4096,
        );
        assert_eq!(
            p.request_url(),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1/publishers/anthropic/models/claude-3-5-sonnet@20241022:streamRawPredict"
        );
    }
}
