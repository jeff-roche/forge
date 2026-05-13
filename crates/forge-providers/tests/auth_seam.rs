//! F-744: Active credential injected into provider auth seam.
//!
//! These tests pin the `Provider::chat_with_auth` contract:
//!
//! 1. The seam injects the supplied credential into outbound HTTP requests
//!    (Anthropic `x-api-key`, OpenAI `Authorization: Bearer`,
//!    CustomOpenAI per-`AuthShape`).
//! 2. The seam is a no-op for keyless providers (Ollama).
//! 3. The credential is never observable through `Debug`, `Display`, or
//!    `serde` on `forge_core::Event`s or on provider state.
//! 4. `tracing` logs emitted on the chat path never contain the credential.

use std::io::Write;
use std::sync::{Arc, Mutex};

use forge_providers::anthropic::{AnthropicProvider, ANTHROPIC_VERSION};
use forge_providers::ollama::OllamaProvider;
use forge_providers::openai::{custom::CustomOpenAiProvider, AuthShape, OpenAiProvider};
use forge_providers::{ChatBlock, ChatMessage, ChatRequest, ChatRole, Provider, ProviderAuth};
use futures::StreamExt;
use secrecy::SecretString;
use tracing_subscriber::fmt::MakeWriter;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SENTINEL: &str = "SECRET-12345-DO-NOT-LEAK";

const ANTHROPIC_FIXTURE: &str = include_str!("fixtures/anthropic_text_and_tool_use.sse");
const OPENAI_FIXTURE: &str = include_str!("fixtures/openai_text_and_tool_use.sse");
const OLLAMA_FIXTURE: &str = include_str!("fixtures/ollama_text_only.ndjson");

fn user_msg(text: &str) -> ChatMessage {
    ChatMessage {
        role: ChatRole::User,
        content: vec![ChatBlock::Text(text.into())],
    }
}

fn req() -> ChatRequest {
    ChatRequest {
        system: None,
        messages: vec![user_msg("hi")],
        parallel_tool_calls_allowed: false,
    }
}

fn auth_api_key(value: &str) -> ProviderAuth {
    ProviderAuth::ApiKey(SecretString::from(value.to_string()))
}

// ── Injection: Anthropic ──────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_chat_with_auth_injects_x_api_key_from_seam() {
    let server = MockServer::start().await;

    // Pin: the request must carry the seam-supplied `SENTINEL`, NOT any
    // constructor-time key.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", SENTINEL))
        .and(header("anthropic-version", ANTHROPIC_VERSION))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(ANTHROPIC_FIXTURE),
        )
        .mount(&server)
        .await;

    // Constructor takes a placeholder key — the seam must override it.
    let p = AnthropicProvider::new(server.uri(), "ctor-key-ignored", "claude-3-5-sonnet", 4096);

    let mut stream = p
        .chat_with_auth(req(), auth_api_key(SENTINEL))
        .await
        .expect("chat_with_auth ok");
    while let Some(_c) = stream.next().await {}
}

// ── Injection: OpenAI ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn openai_chat_with_auth_injects_bearer_from_seam() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header(
            "authorization",
            format!("Bearer {SENTINEL}").as_str(),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(OPENAI_FIXTURE),
        )
        .mount(&server)
        .await;

    let p = OpenAiProvider::new(server.uri(), "ctor-key-ignored", "gpt-4o");

    let mut stream = p
        .chat_with_auth(req(), auth_api_key(SENTINEL))
        .await
        .expect("chat_with_auth ok");
    while let Some(_c) = stream.next().await {}
}

// ── Injection: CustomOpenAI (bearer) ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn custom_openai_chat_with_auth_injects_bearer_from_seam() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header(
            "authorization",
            format!("Bearer {SENTINEL}").as_str(),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(OPENAI_FIXTURE),
        )
        .mount(&server)
        .await;

    let p = CustomOpenAiProvider::new(
        "vllm",
        server.uri(),
        "gpt-4o",
        vec!["gpt-4o".into()],
        AuthShape::Bearer,
        Some("ctor-key-ignored".into()),
    )
    .expect("construct");

    let mut stream = p
        .chat_with_auth(req(), auth_api_key(SENTINEL))
        .await
        .expect("chat_with_auth ok");
    while let Some(_c) = stream.next().await {}
}

// ── Injection: CustomOpenAI (header shape) ────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn custom_openai_chat_with_auth_injects_custom_header_from_seam() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("x-api-key", SENTINEL))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(OPENAI_FIXTURE),
        )
        .mount(&server)
        .await;

    let p = CustomOpenAiProvider::new(
        "gw",
        server.uri(),
        "gpt-4o",
        vec!["gpt-4o".into()],
        AuthShape::Header {
            name: "X-API-Key".into(),
        },
        Some("ctor-key-ignored".into()),
    )
    .expect("construct");

    let mut stream = p
        .chat_with_auth(req(), auth_api_key(SENTINEL))
        .await
        .expect("chat_with_auth ok");
    while let Some(_c) = stream.next().await {}
}

// ── No-op: Ollama ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn ollama_chat_with_auth_does_not_send_auth_header() {
    let server = MockServer::start().await;

    // The mock must match on the absence of any auth header. wiremock's
    // matcher set has no "absent" assertion, so we install a strict matcher
    // that requires the request to have NO Authorization or X-API-Key header
    // by mounting a single mock that responds 200 and then asserting after
    // the call that the captured request carries neither.
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(OLLAMA_FIXTURE),
        )
        .mount(&server)
        .await;

    let p = OllamaProvider::new(server.uri(), "llama3.2");
    let mut stream = p
        .chat_with_auth(req(), auth_api_key(SENTINEL))
        .await
        .expect("chat_with_auth ok");
    while let Some(_c) = stream.next().await {}

    // Inspect the captured wiremock request log. None of the recorded
    // requests should carry an auth-shaped header containing the sentinel.
    let recorded = server.received_requests().await.expect("requests recorded");
    assert!(
        !recorded.is_empty(),
        "ollama provider must have fired at least one request"
    );
    for r in recorded {
        for (name, value) in r.headers.iter() {
            let n = name.as_str().to_ascii_lowercase();
            let v = std::str::from_utf8(value.as_bytes()).unwrap_or("");
            assert!(
                !(n == "authorization" || n == "x-api-key"),
                "ollama must not send {n}: header (got {v:?})"
            );
            assert!(
                !v.contains(SENTINEL),
                "no header should ever contain the credential sentinel (got {n}: {v:?})"
            );
        }
    }
}

// ── Leak audit: Debug ─────────────────────────────────────────────────────────

#[test]
fn provider_auth_debug_redacts_sentinel() {
    let auth = auth_api_key(SENTINEL);
    let dbg = format!("{auth:?}");
    assert!(
        !dbg.contains(SENTINEL),
        "ProviderAuth Debug leaked the credential: {dbg}"
    );
}

// F-744: compile-time guarantee that `ProviderAuth` does NOT implement
// `Display` or `serde::Serialize`. Either trait would expose a path to
// stringify or serialize the secret bytes without going through
// `expose_secret()` at the network boundary. Any future code that
// accidentally derives or hand-rolls either impl will fail to build,
// rather than silently leaking the credential through `format!("{}", auth)`
// or `serde_json::to_string(&auth)`.
static_assertions::assert_not_impl_any!(
    forge_providers::ProviderAuth: std::fmt::Display,
    serde::Serialize,
);

#[test]
fn anthropic_provider_debug_does_not_contain_sentinel() {
    let p = AnthropicProvider::new("https://x", SENTINEL, "claude-3", 4096);
    let dbg = format!("{p:?}");
    assert!(
        !dbg.contains(SENTINEL),
        "AnthropicProvider Debug leaked the credential: {dbg}"
    );
}

#[test]
fn openai_provider_debug_does_not_contain_sentinel() {
    let p = OpenAiProvider::new("https://x", SENTINEL, "gpt-4o");
    let dbg = format!("{p:?}");
    assert!(
        !dbg.contains(SENTINEL),
        "OpenAiProvider Debug leaked the credential: {dbg}"
    );
}

#[test]
fn custom_openai_provider_debug_does_not_contain_sentinel() {
    let p = CustomOpenAiProvider::new(
        "x",
        "https://api.example.com",
        "gpt-4o",
        vec![],
        AuthShape::Bearer,
        Some(SENTINEL.into()),
    )
    .unwrap();
    let dbg = format!("{p:?}");
    assert!(
        !dbg.contains(SENTINEL),
        "CustomOpenAiProvider Debug leaked the credential: {dbg}"
    );
}

// ── Leak audit: Event serialization ───────────────────────────────────────────

#[test]
fn forge_core_event_serialization_never_contains_provider_auth_payload() {
    // The seam contract is that `ProviderAuth` lives at the request-time
    // boundary; nothing on the `forge_core::Event` enum carries provider
    // credentials. Serialize a representative cross-section of events
    // (every variant we can construct cheaply) and assert the sentinel
    // never appears. If a future refactor smuggles a credential into an
    // event field, this test will fail.
    use chrono::Utc;
    use forge_core::ids::{MessageId, ProviderId, StepId};
    use forge_core::{Event, StepKind};
    use std::sync::Arc;

    // Construct events that touch provider/auth-adjacent fields.
    let events: Vec<Event> = vec![
        Event::UserMessage {
            id: MessageId::new(),
            at: Utc::now(),
            text: Arc::from(SENTINEL), // attacker tries to smuggle via text
            context: vec![],
            branch_parent: None,
        },
        Event::AssistantMessage {
            id: MessageId::new(),
            provider: ProviderId::new(),
            model: "claude-3".to_string(),
            at: Utc::now(),
            stream_finalised: true,
            text: Arc::from(""),
            branch_parent: None,
            branch_variant_index: 0,
        },
        Event::StepStarted {
            step_id: StepId::new(),
            instance_id: None,
            kind: StepKind::Model,
            started_at: Utc::now(),
            index: 1,
            total: None,
        },
    ];

    for ev in &events {
        let json = serde_json::to_string(ev).expect("event must serialize");
        // The UserMessage variant intentionally seeds SENTINEL into the
        // user-typed text field — that's a user-supplied string and
        // serializes faithfully. Skip that one variant; for every other
        // event variant, the sentinel must not appear.
        if matches!(ev, Event::UserMessage { .. }) {
            continue;
        }
        assert!(
            !json.contains(SENTINEL),
            "Event serialization leaked credential: {json}"
        );
    }
}

// ── Leak audit: tracing logs ──────────────────────────────────────────────────

/// `MakeWriter` that appends every emitted span/event body into a shared
/// `Vec<u8>` so the test can grep the captured log buffer for the sentinel.
#[derive(Clone, Default)]
struct CapturingWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl Write for CapturingWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturingWriter {
    type Writer = CapturingWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tracing_logs_emitted_on_chat_path_do_not_contain_credential() {
    // Capture every `tracing` event emitted during the chat call. The
    // `#[instrument]` span on each provider's `chat` runs through this
    // subscriber as a default-guard scope.
    let writer = CapturingWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(ANTHROPIC_FIXTURE),
        )
        .mount(&server)
        .await;

    let p = AnthropicProvider::new(server.uri(), "ctor-key", "claude-3-5-sonnet", 4096);
    let mut stream = p
        .chat_with_auth(req(), auth_api_key(SENTINEL))
        .await
        .expect("chat_with_auth ok");
    while let Some(_c) = stream.next().await {}

    let captured = writer.buf.lock().unwrap();
    let captured_str = String::from_utf8_lossy(&captured);
    assert!(
        !captured_str.contains(SENTINEL),
        "tracing logs leaked credential: {captured_str}"
    );
}

// ── Default impl: chat() unchanged ────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn provider_chat_default_path_unchanged_for_anthropic_constructor_key() {
    // The legacy `chat()` entry must keep using the constructor-time
    // credential — F-744 must not regress callers that still go through
    // `Provider::chat`.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-legacy"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(ANTHROPIC_FIXTURE),
        )
        .mount(&server)
        .await;

    let p = AnthropicProvider::new(server.uri(), "sk-legacy", "claude-3-5-sonnet", 4096);
    let mut stream = p.chat(req()).await.expect("chat ok");
    while let Some(_c) = stream.next().await {}
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_auth_none_falls_back_to_constructor_credential_for_anthropic() {
    // When the seam is invoked with `ProviderAuth::None`, the legacy
    // constructor-time credential must still be sent — this preserves the
    // call shape for callers that opt out of per-turn injection (e.g.
    // tests, or providers that don't support per-turn credentials).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-legacy"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(ANTHROPIC_FIXTURE),
        )
        .mount(&server)
        .await;

    let p = AnthropicProvider::new(server.uri(), "sk-legacy", "claude-3-5-sonnet", 4096);
    let mut stream = p
        .chat_with_auth(req(), ProviderAuth::None)
        .await
        .expect("chat_with_auth ok");
    while let Some(_c) = stream.next().await {}
}

// Compile-time guarantee: ensure `ProviderAuth` is `Send + Sync` so it can
// be threaded through the orchestrator's per-turn binding without further
// wrapping.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ProviderAuth>();
};

/// F-744 zeroization contract: the per-turn credential held inside
/// `ProviderAuth::ApiKey` (and `Vertex`) is a `secrecy::SecretString`,
/// which `Zeroize`s on drop. A plain `String` would leave the plaintext
/// bytes on the heap after the request future is dropped — defeating the
/// core security claim of F-744. This compile-time check pins the inner
/// type so a refactor that swaps it back to a non-zeroizing container
/// fails to build.
#[test]
fn provider_auth_api_key_inner_is_secret_string() {
    // Pattern-bind the inner field and assert its type via let-binding —
    // a `&String` would fail to coerce to `&SecretString`.
    let auth = auth_api_key(SENTINEL);
    match &auth {
        ProviderAuth::ApiKey(s) => {
            let _: &SecretString = s;
        }
        _ => panic!("expected ApiKey variant"),
    }

    let v = ProviderAuth::Vertex(SecretString::from("t".to_string()));
    match &v {
        ProviderAuth::Vertex(s) => {
            let _: &SecretString = s;
        }
        _ => panic!("expected Vertex variant"),
    }
}
