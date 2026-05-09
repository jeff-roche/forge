//! Generic OpenAI-compatible provider for self-hosted servers (vLLM,
//! LiteLLM, Together, Anyscale, etc.) that speak OpenAI's
//! `/v1/chat/completions` wire shape but expose it under a user-supplied
//! base URL with a configurable auth shape.
//!
//! ## Reuse model
//!
//! [`CustomOpenAiProvider`] reuses the request-translation
//! ([`super::translate`]) and SSE-decode pipeline of [`super::OpenAiProvider`]
//! verbatim — every byte on the wire matches what `OpenAiProvider` would
//! send for the same `ChatRequest`. The only knobs that differ:
//!
//! - **Base URL.** Already configurable on `OpenAiProvider`. Validated here
//!   at construction time against [`forge_core::url_safety::check_url`]
//!   (the F-346 SSRF guard) so a misconfigured `base_url = "http://10.0.0.1"`
//!   is refused before any network roundtrip.
//! - **Auth shape.** [`AuthShape`] selects how the API key is presented:
//!   - `Bearer` → `Authorization: Bearer <key>` (vanilla OpenAI shape)
//!   - `Header { name }` → `<name>: <key>` (proxies that use e.g. `X-API-Key`)
//!   - `None` → no auth header at all (private-network gateways, public mocks)
//! - **Model list.** A `Vec<String>` of model identifiers the user has
//!   declared the endpoint can serve. The provider does not validate
//!   `req.model` against this list at chat() time — that gating is the
//!   responsibility of upstream routing — but the list is exposed via
//!   [`CustomOpenAiProvider::model_list`] so settings UIs and the agent
//!   roster can render it.
//!
//! ## SSRF guard
//!
//! Construction goes through [`CustomOpenAiProvider::new`], which calls
//! `forge_core::url_safety::check_url(&base_url)` before any other state
//! is written. The guard accepts public HTTPS hosts and loopback (127/8,
//! ::1, `localhost`) HTTP hosts in debug builds, and rejects everything
//! else (RFC-1918, link-local/IMDS, IPv6 unique-local, non-http schemes).

use std::sync::Arc;

use forge_core::settings::{AuthShapeSettings, CustomOpenAiEntry};
use forge_core::Result;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::http_util::{DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT, DEFAULT_TCP_KEEPALIVE};
use crate::sse;
use crate::{ChatChunk, ChatRequest, Provider};

use super::{chat_request, translate};

/// Cap on redirect hops the streaming client will follow. The redirect
/// policy *also* re-validates each hop's URL via
/// [`forge_core::url_safety::check_url`] (rejecting non-http(s) schemes and
/// private/link-local/IMDS IP literals), so the cap is strictly a
/// belt-and-suspenders bound on chain length — the per-hop check is the
/// primary SSRF guard.
const MAX_REDIRECT_HOPS: usize = 5;

/// How the API key is presented on outbound requests.
///
/// Serialized as a tagged enum so settings TOML can declare e.g.
/// `auth = { shape = "header", name = "X-API-Key" }`. The `bearer` and
/// `none` variants are unit-shaped on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum AuthShape {
    /// `Authorization: Bearer <key>` — the OpenAI default.
    Bearer,
    /// `<name>: <key>` — for proxies/gateways that expect a custom header
    /// (e.g. `X-API-Key`).
    Header { name: String },
    /// No auth header sent. Used for endpoints inside a trusted boundary
    /// (private network, local mock) that need no key at all.
    None,
}

/// Generic OpenAI-compatible chat provider.
///
/// Reuses the OpenAI request translation and SSE accumulator from
/// [`super::translate`] verbatim; only the base URL and auth-header shape
/// differ from the vanilla [`super::OpenAiProvider`].
///
/// `Debug` is hand-rolled so the api_key never appears in log output: the
/// formatter prints a fixed `"<redacted>"` placeholder when the key is
/// `Some`. Mirrors the redaction posture of `forge-mcp` URL logging.
pub struct CustomOpenAiProvider {
    /// Stable identifier (e.g. `"vllm-local"`, `"together"`) — exposed to
    /// the settings UI and surfaced in error messages so a multi-entry
    /// configuration can disambiguate failures.
    name: String,
    base_url: String,
    api_key: Option<Arc<str>>,
    auth_shape: AuthShape,
    model: String,
    /// Model identifiers the user has declared this endpoint serves.
    /// Exposed for UI / roster rendering; not enforced at chat() time.
    model_list: Vec<String>,
    /// Optional `max_tokens` cap. `None` omits the field on the wire.
    max_tokens: Option<u32>,
    stream_client: reqwest::Client,
    stream_cfg: sse::StreamConfig,
}

impl CustomOpenAiProvider {
    /// Construct a new provider entry.
    ///
    /// `base_url` is validated against the F-346 SSRF guard
    /// ([`forge_core::url_safety::check_url`]) — HTTPS is always allowed,
    /// HTTP only for loopback (127.0.0.1, ::1, localhost) and only in debug
    /// builds. RFC-1918 / link-local / IMDS addresses are rejected.
    ///
    /// `api_key` is `Option<String>`: `AuthShape::None` does not require one,
    /// and `Bearer` / `Header` require one (an absent key with a non-`None`
    /// shape returns an error so misconfiguration surfaces at construction
    /// rather than at the first chat() call).
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        model_list: Vec<String>,
        auth_shape: AuthShape,
        api_key: Option<String>,
    ) -> Result<Self> {
        let name = name.into();
        let base_url = base_url.into();
        let model = model.into();

        forge_core::url_safety::check_url(&base_url).map_err(|e| {
            anyhow::anyhow!("custom_openai provider {name:?}: invalid base_url {base_url:?}: {e}")
        })?;

        // Validate the header name at construction time so a configuration
        // like `auth = { shape = "header", name = "X-API Key" }` (with an
        // invalid HTTP token character — the space) is rejected up-front
        // with a diagnostic that names the offending value, rather than
        // silently being skipped at chat() time and surfacing as an opaque
        // 401/403 from the upstream server.
        if let AuthShape::Header { name: hdr } = &auth_shape {
            if hdr.parse::<reqwest::header::HeaderName>().is_err() {
                return Err(anyhow::anyhow!(
                    "custom_openai provider {name:?}: auth.name {hdr:?} is not a valid HTTP header name"
                )
                .into());
            }
        }

        match (&auth_shape, &api_key) {
            (AuthShape::None, _) => {}
            (_, Some(_)) => {}
            (AuthShape::Bearer, None) => {
                return Err(anyhow::anyhow!(
                    "custom_openai provider {name:?}: auth_shape=bearer requires an api_key"
                )
                .into());
            }
            (AuthShape::Header { name: hdr }, None) => {
                return Err(anyhow::anyhow!(
                    "custom_openai provider {name:?}: auth_shape=header(name={hdr:?}) requires an api_key"
                )
                .into());
            }
        }

        Ok(Self {
            name,
            base_url,
            api_key: api_key.map(Arc::from),
            auth_shape,
            model,
            model_list,
            max_tokens: None,
            stream_client: build_secure_custom_client(),
            stream_cfg: sse::StreamConfig::DEFAULT,
        })
    }

    /// Stable provider entry name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Configured base URL (already validated at construction time).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Model identifiers the user declared this endpoint serves.
    pub fn model_list(&self) -> &[String] {
        &self.model_list
    }

    /// Auth shape selected for this entry.
    pub fn auth_shape(&self) -> &AuthShape {
        &self.auth_shape
    }

    /// Set an explicit `max_tokens` cap. Builder-style; mirrors
    /// [`super::OpenAiProvider::with_max_tokens`].
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

    /// Construct from a settings-layer [`CustomOpenAiEntry`] — the on-disk
    /// shape parsed out of `[providers.custom_openai.<name>]`. Validation
    /// errors propagate with the entry name embedded so a multi-entry
    /// configuration can disambiguate failures.
    pub fn from_settings(name: impl Into<String>, entry: &CustomOpenAiEntry) -> Result<Self> {
        let name = name.into();
        let auth = match &entry.auth {
            AuthShapeSettings::Bearer => AuthShape::Bearer,
            AuthShapeSettings::Header { name: hdr } => AuthShape::Header { name: hdr.clone() },
            AuthShapeSettings::None => AuthShape::None,
        };
        if entry.model.is_empty() {
            return Err(anyhow::anyhow!(
                "custom_openai provider {name:?}: `model` must be a non-empty string"
            )
            .into());
        }
        Self::new(
            name,
            entry.base_url.clone(),
            entry.model.clone(),
            entry.model_list.clone(),
            auth,
            entry.api_key.clone(),
        )
    }

    /// Build the auth-header list for one outbound chat request.
    ///
    /// Both the `AuthShape::Header { name }` value and the
    /// `AuthShape::Bearer | Header → api_key` requirement are validated at
    /// construction time, so the only legal arms here are the two
    /// authenticated shapes (with a guaranteed-valid header name and key)
    /// or `None`. The `expect` is a structural assertion against the
    /// `new()` invariants — it cannot fire on a `Self` produced through
    /// the public API.
    fn auth_headers(&self) -> Vec<(reqwest::header::HeaderName, String)> {
        match (&self.auth_shape, &self.api_key) {
            (AuthShape::Bearer, Some(key)) => {
                vec![(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))]
            }
            (AuthShape::Header { name }, Some(key)) => {
                let hname: reqwest::header::HeaderName = name
                    .parse()
                    .expect("auth.name validated at construction time");
                vec![(hname, key.to_string())]
            }
            _ => Vec::new(),
        }
    }
}

impl std::fmt::Debug for CustomOpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomOpenAiProvider")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("model_list", &self.model_list)
            .field("auth_shape", &self.auth_shape)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
}

/// Build the `reqwest::Client` used by `CustomOpenAiProvider` for outbound
/// chat completions. Layered defenses against SSRF on a user-supplied
/// `base_url`:
///
/// 1. **Policy-enforcing DNS resolver**
///    ([`forge_core::http::secure_client_builder`]): every resolved
///    `SocketAddr` is filtered through the `url_safety` IPv4/IPv6 range
///    policy. A short-TTL DNS rebind that answers safely on
///    [`crate::openai::custom::CustomOpenAiProvider::new`]'s `check_url`
///    pass and hostilely on the connect that follows is refused at the
///    DNS layer — reqwest never opens the socket.
///
/// 2. **Custom redirect policy**: 3xx responses do not auto-follow blindly.
///    Each hop's URL is re-validated through
///    [`forge_core::url_safety::check_url`] before reqwest dispatches the
///    follow-up request, so a malicious vendor that responds with
///    `302 Location: http://169.254.169.254/...` cannot smuggle a private
///    IP target into the chain. The hop count is capped at
///    [`MAX_REDIRECT_HOPS`] regardless.
///
/// The streaming-HTTP timeouts (connect, read, TCP keepalive) match the
/// shared [`crate::http_util::HttpClientConfig::DEFAULT`] posture used by
/// [`super::OpenAiProvider`]; they are inlined here because
/// [`forge_core::http::secure_client_builder`] returns a fresh
/// `reqwest::ClientBuilder` we must populate ourselves.
fn build_secure_custom_client() -> reqwest::Client {
    let redirect_policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECT_HOPS {
            return attempt.error(format!(
                "custom_openai redirect chain exceeded {MAX_REDIRECT_HOPS} hops"
            ));
        }
        let url = attempt.url().to_string();
        match forge_core::url_safety::check_url(&url) {
            Ok(()) => attempt.follow(),
            Err(e) => attempt.error(format!(
                "custom_openai redirect to {url} refused by SSRF guard: {e}"
            )),
        }
    });

    forge_core::http::secure_client_builder()
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        .read_timeout(DEFAULT_READ_TIMEOUT)
        .tcp_keepalive(Some(DEFAULT_TCP_KEEPALIVE))
        .redirect(redirect_policy)
        .build()
        .expect("reqwest custom-openai client builder — only fails if no TLS backend is available")
}

impl Provider for CustomOpenAiProvider {
    fn chat(
        &self,
        req: ChatRequest,
    ) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send {
        let body_result = translate::serialize_request(&req, &self.model, self.max_tokens);
        let auth = self.auth_headers();
        chat_request(
            self.stream_client.clone(),
            self.base_url.clone(),
            auth,
            body_result,
            self.stream_cfg,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_shape_serializes_as_tagged_enum() {
        let bearer: AuthShape = serde_json::from_str(r#"{"shape":"bearer"}"#).unwrap();
        assert_eq!(bearer, AuthShape::Bearer);

        let header: AuthShape =
            serde_json::from_str(r#"{"shape":"header","name":"X-API-Key"}"#).unwrap();
        assert_eq!(
            header,
            AuthShape::Header {
                name: "X-API-Key".into()
            }
        );

        let none: AuthShape = serde_json::from_str(r#"{"shape":"none"}"#).unwrap();
        assert_eq!(none, AuthShape::None);
    }

    #[test]
    fn new_accepts_https_remote() {
        let p = CustomOpenAiProvider::new(
            "together",
            "https://api.together.xyz",
            "mistralai/Mixtral-8x7B-Instruct-v0.1",
            vec!["mistralai/Mixtral-8x7B-Instruct-v0.1".into()],
            AuthShape::Bearer,
            Some("sk-test".into()),
        )
        .expect("https remote must be accepted");
        assert_eq!(p.name(), "together");
        assert_eq!(p.base_url(), "https://api.together.xyz");
        assert_eq!(p.model_list().len(), 1);
    }

    #[test]
    fn new_accepts_http_loopback() {
        // http://127.0.0.1 is allowed in debug builds (where tests run).
        let p = CustomOpenAiProvider::new(
            "vllm-local",
            "http://127.0.0.1:8000",
            "Qwen/Qwen2.5-7B-Instruct",
            vec!["Qwen/Qwen2.5-7B-Instruct".into()],
            AuthShape::None,
            None,
        )
        .expect("http loopback must be accepted in debug builds");
        assert_eq!(p.auth_shape(), &AuthShape::None);
    }

    #[test]
    fn new_rejects_http_remote() {
        let err = CustomOpenAiProvider::new(
            "evil",
            "http://example.com",
            "any",
            vec![],
            AuthShape::Bearer,
            Some("sk".into()),
        )
        .expect_err("plain-http remote must be rejected by the SSRF guard");
        let msg = format!("{err}");
        assert!(
            msg.contains("evil") && msg.contains("http://example.com"),
            "error must name the offending entry and URL: {msg}"
        );
    }

    #[test]
    fn new_rejects_rfc1918_address() {
        let err = CustomOpenAiProvider::new(
            "internal",
            "https://10.0.0.1/v1",
            "any",
            vec![],
            AuthShape::Bearer,
            Some("sk".into()),
        )
        .expect_err("RFC-1918 must be rejected");
        assert!(
            format!("{err}").contains("10.0.0.0/8"),
            "error should name the blocked range"
        );
    }

    #[test]
    fn new_rejects_imds_address() {
        CustomOpenAiProvider::new(
            "imds",
            "https://169.254.169.254/v1",
            "any",
            vec![],
            AuthShape::Bearer,
            Some("sk".into()),
        )
        .expect_err("IMDS link-local must be rejected");
    }

    #[test]
    fn new_requires_api_key_for_bearer() {
        let err = CustomOpenAiProvider::new(
            "no-key",
            "https://api.example.com",
            "any",
            vec![],
            AuthShape::Bearer,
            None,
        )
        .expect_err("Bearer without api_key must error");
        assert!(format!("{err}").contains("requires an api_key"));
    }

    #[test]
    fn new_requires_api_key_for_header_shape() {
        let err = CustomOpenAiProvider::new(
            "no-key",
            "https://api.example.com",
            "any",
            vec![],
            AuthShape::Header {
                name: "X-API-Key".into(),
            },
            None,
        )
        .expect_err("Header shape without api_key must error");
        assert!(format!("{err}").contains("requires an api_key"));
    }

    #[test]
    fn new_rejects_header_name_with_space() {
        // A space is not a valid HTTP token character; the previous
        // implementation silently dropped this header at chat() time and
        // surfaced as an opaque 401/403 from the upstream server. Now
        // construction must fail with a diagnostic that names the
        // offending value.
        let err = CustomOpenAiProvider::new(
            "bad-hdr",
            "https://api.example.com",
            "any",
            vec![],
            AuthShape::Header {
                name: "X-API Key".into(),
            },
            Some("sk".into()),
        )
        .expect_err("header name with space must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("bad-hdr") && msg.contains("X-API Key"),
            "error must name the offending entry and value: {msg}"
        );
        assert!(
            msg.contains("not a valid HTTP header name"),
            "error must explain the failure: {msg}"
        );
    }

    #[test]
    fn new_rejects_header_name_with_control_char() {
        let err = CustomOpenAiProvider::new(
            "bad-hdr",
            "https://api.example.com",
            "any",
            vec![],
            AuthShape::Header {
                name: "X-Api\nKey".into(),
            },
            Some("sk".into()),
        )
        .expect_err("header name with newline must be rejected");
        assert!(format!("{err}").contains("not a valid HTTP header name"));
    }

    #[test]
    fn new_rejects_empty_header_name() {
        let err = CustomOpenAiProvider::new(
            "bad-hdr",
            "https://api.example.com",
            "any",
            vec![],
            AuthShape::Header { name: "".into() },
            Some("sk".into()),
        )
        .expect_err("empty header name must be rejected");
        assert!(format!("{err}").contains("not a valid HTTP header name"));
    }

    #[test]
    fn auth_headers_bearer_shape() {
        let p = CustomOpenAiProvider::new(
            "x",
            "https://api.example.com",
            "any",
            vec![],
            AuthShape::Bearer,
            Some("sk-secret".into()),
        )
        .unwrap();
        let h = p.auth_headers();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0, reqwest::header::AUTHORIZATION);
        assert_eq!(h[0].1, "Bearer sk-secret");
    }

    #[test]
    fn auth_headers_custom_header_shape() {
        let p = CustomOpenAiProvider::new(
            "x",
            "https://api.example.com",
            "any",
            vec![],
            AuthShape::Header {
                name: "X-API-Key".into(),
            },
            Some("sk-secret".into()),
        )
        .unwrap();
        let h = p.auth_headers();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0.as_str(), "x-api-key");
        assert_eq!(h[0].1, "sk-secret");
    }

    #[test]
    fn auth_headers_none_shape_emits_no_headers() {
        let p = CustomOpenAiProvider::new(
            "x",
            "https://api.example.com",
            "any",
            vec![],
            AuthShape::None,
            None,
        )
        .unwrap();
        assert!(p.auth_headers().is_empty());
    }

    #[test]
    fn from_settings_round_trips_bearer_entry() {
        let entry = CustomOpenAiEntry {
            base_url: "https://api.together.xyz".into(),
            model: "mixtral".into(),
            model_list: vec!["mixtral".into()],
            auth: AuthShapeSettings::Bearer,
            api_key: Some("sk-x".into()),
        };
        let p = CustomOpenAiProvider::from_settings("together", &entry).unwrap();
        assert_eq!(p.name(), "together");
        assert_eq!(p.auth_shape(), &AuthShape::Bearer);
    }

    #[test]
    fn from_settings_round_trips_header_entry() {
        let entry = CustomOpenAiEntry {
            base_url: "https://gateway.example.com".into(),
            model: "any".into(),
            model_list: vec![],
            auth: AuthShapeSettings::Header {
                name: "X-API-Key".into(),
            },
            api_key: Some("sk-x".into()),
        };
        let p = CustomOpenAiProvider::from_settings("gw", &entry).unwrap();
        assert_eq!(
            p.auth_shape(),
            &AuthShape::Header {
                name: "X-API-Key".into()
            }
        );
    }

    #[test]
    fn from_settings_rejects_empty_model() {
        let entry = CustomOpenAiEntry {
            base_url: "https://api.together.xyz".into(),
            model: "".into(),
            model_list: vec![],
            auth: AuthShapeSettings::Bearer,
            api_key: Some("sk".into()),
        };
        let err = CustomOpenAiProvider::from_settings("together", &entry).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("together") && msg.contains("model"),
            "error must name entry and offending field: {msg}"
        );
    }

    #[test]
    fn from_settings_propagates_ssrf_failure_with_entry_name() {
        let entry = CustomOpenAiEntry {
            base_url: "http://10.0.0.1".into(),
            model: "any".into(),
            model_list: vec![],
            auth: AuthShapeSettings::Bearer,
            api_key: Some("sk".into()),
        };
        let err = CustomOpenAiProvider::from_settings("bad", &entry).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("bad"),
            "error must name the offending entry: {msg}"
        );
    }
}
