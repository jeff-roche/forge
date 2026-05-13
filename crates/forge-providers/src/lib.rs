use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_core::Result;
use futures::stream::BoxStream;
// `parking_lot::Mutex` — guards are not poisoned on a panicking holder, so a
// panicking test that holds the mutex does not cascade into every subsequent
// `.lock()` in the same process. The `std::sync::Mutex` here used to require
// `.lock().unwrap()` at every call site, turning one panic into N.
use parking_lot::Mutex;
use secrecy::SecretString;
use serde::Deserialize;

pub mod anthropic;
// F-679: shared HTTP / SSE helpers used by Anthropic and the OpenAI family.
// Crate-private — the only consumers are sibling provider modules.
pub(crate) mod http_util;
// F-743: Ollama — keyless, NDJSON-streamed `/api/chat`.
pub mod ollama;
pub mod openai;
// F-593: static price-table parser + cost calculator. The committed
// `data/prices.toml` is `include_str!`-embedded so every binary that links
// `forge-providers` (daemon, shell, CLI) shares the same lookup.
pub mod pricing;
// F-586: hot-swappable [`Provider`] dispatcher used by the session daemon
// when the dashboard's `set_active_provider` IPC command emits a
// `ProviderChanged` event for the next turn.
pub mod runtime;
pub mod sse;

pub use runtime::{RuntimeProvider, SwappableProvider};

// ── Chat domain types ─────────────────────────────────────────────────────────

/// A single request to a chat [`Provider`]: an optional system prompt plus
/// the conversation history the provider should respond to.
///
/// Sampling parameters (temperature, max-tokens, stop sequences) are not
/// modelled here — providers either default them or read them from
/// provider-specific configuration. Callers that need provider-specific
/// knobs should wrap or extend this type at the provider boundary rather
/// than overload it with optional fields that no provider honours.
#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    /// Optional system prompt. Providers handle it in role-specific ways:
    /// e.g. Anthropic hoists it to a top-level `system` field. `None` means
    /// "no system prompt".
    ///
    /// F-566: held as `Arc<str>` so per-iteration `req.clone()` on the
    /// orchestrator hot loop is a refcount bump rather than a deep copy
    /// of the (potentially 256 KiB) AGENTS.md prefix. Construction sites
    /// wrap with `Arc::from(s)` once; readers go through `as_deref()`
    /// (returns `Option<&str>`) which keeps every existing call-site
    /// shape unchanged.
    pub system: Option<Arc<str>>,
    /// Conversation history in chronological order. Typically alternates
    /// [`ChatRole::User`] and [`ChatRole::Assistant`], though providers
    /// vary in how strictly they enforce that.
    pub messages: Vec<ChatMessage>,
    /// F-583: whether the provider is permitted to request multiple tool
    /// calls in a single turn. Defaults to `false`; F-583 only plumbs the
    /// flag through, F-599 will drive behavior.
    pub parallel_tool_calls_allowed: bool,
}

/// One message in a chat conversation, tagged with the role that produced
/// it and carrying one or more content blocks.
///
/// Splitting content into a `Vec<ChatBlock>` (rather than a flat string)
/// lets a single message interleave text, tool calls, and tool results
/// — which matches how modern provider APIs frame multi-part turns.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// Who produced this message.
    pub role: ChatRole,
    /// The message body, as an ordered sequence of content blocks.
    /// See [`ChatBlock`] for the variants (text, tool calls, tool results).
    pub content: Vec<ChatBlock>,
}

/// The role a [`ChatMessage`] was produced by.
///
/// Only `User` and `Assistant` are modelled. System prompts live on
/// [`ChatRequest::system`], not as a role. Tool interactions are not a
/// distinct role either — they ride inside an assistant or user message
/// as [`ChatBlock::ToolCall`] / [`ChatBlock::ToolResult`] content blocks.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatRole {
    /// A message authored by the human (or upstream caller acting on the
    /// human's behalf). Carries the user's prompt and any tool results
    /// being fed back into the conversation.
    User,
    /// A message authored by the model. Carries the model's textual reply
    /// and any tool calls it has decided to invoke.
    Assistant,
}

#[derive(Debug, Clone)]
pub enum ChatBlock {
    Text(String),
    ToolCall {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    ToolResult {
        id: String,
        result: serde_json::Value,
    },
}

// ── Stream chunk ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum ChatChunk {
    TextDelta(String),
    /// A complete tool call emitted by the provider.
    ///
    /// `id` is the provider-assigned identifier (Anthropic `toolu_…`,
    /// OpenAI `call_…`) needed to round-trip a `ChatBlock::ToolResult`
    /// back to that provider — the assistant message that initiated the
    /// tool use carries the id, and the user-role tool result references
    /// it. Providers that do not surface an id on the wire (`MockProvider`)
    /// emit an empty string; the orchestrator falls back to its own
    /// internal id in that case.
    ToolCall {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    Done(String),
    /// Terminal, structured stream failure. The chunk stream closes after
    /// yielding this variant — callers should treat the current turn as aborted.
    ///
    /// F-749: `status` carries the HTTP response status code when the failure
    /// originated from an HTTP-level non-2xx response (auth, rate-limit, 5xx,
    /// etc.). `None` indicates the failure happened at the transport / SSE
    /// adapter / parse layer rather than as an HTTP status. Surfaced through
    /// the orchestrator so the UI's `TurnErrorKind` classifier can distinguish
    /// `Auth` (401) / `RateLimit` (429) / `Server` (5xx) from non-status
    /// failures without parsing the human-readable message text.
    Error {
        kind: StreamErrorKind,
        message: String,
        status: Option<u16>,
    },
}

/// Why a provider stream terminated abnormally.
///
/// # Asymmetry with [`crate::sse::SseError`]
///
/// `StreamErrorKind` is `Copy` and carries no payload, while the
/// upstream [`crate::sse::SseError::Transport`] variant *does* carry
/// a `String` description of the underlying transport failure
/// (reqwest error, redirect refusal, malformed framing, etc.). The
/// asymmetry is intentional: the descriptive transport string is
/// surfaced separately on the [`ChatChunk::Error`] envelope via its
/// sibling `message` field, so the kind→envelope mapping can stay
/// allocation-free (`Copy` of an enum tag, no `Clone` of a heap
/// string).
///
/// At the crate-internal boundary (`http_util::map_sse_error`), every
/// `SseError` collapses to a `StreamErrorKind`: variants that already
/// describe themselves (`LineTooLong`, `IdleTimeout`, `WallClockTimeout`)
/// map one-to-one; `SseError::Transport(string)` drops the string here
/// — the caller is expected to copy it into the surrounding
/// `ChatChunk::Error.message` so the wire shape is consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamErrorKind {
    /// A single NDJSON line exceeded the per-line byte cap.
    LineTooLong,
    /// No bytes received within the inter-chunk idle window.
    IdleTimeout,
    /// The overall stream exceeded its wall-clock budget.
    WallClockTimeout,
    /// Transport-level error from the underlying reader. The
    /// descriptive message lives on the sibling
    /// [`ChatChunk::Error::message`] field — see the type-level
    /// asymmetry note above.
    Transport,
}

// ── Provider auth seam (F-744) ────────────────────────────────────────────────

/// Per-request authentication value handed to [`Provider::chat_with_auth`].
///
/// F-744: the orchestrator pulls the active credential from the keyring at
/// the start of every turn (see `forge_session::orchestrator::CredentialContext`)
/// and threads it through this enum into the provider's outbound HTTP
/// request, without storing the secret on the provider struct.
///
/// The variants describe *how* the credential is presented on the wire — not
/// the storage mechanism. `ApiKey` covers both `Authorization: Bearer …`
/// (OpenAI shape) and `x-api-key: …` (Anthropic shape); the provider impl
/// decides which header to emit based on its own protocol. `Vertex` is
/// distinct because the underlying token comes from gcloud ADC rather than
/// the keyring, and the wire shape (`Authorization: Bearer <gcloud-token>`
/// + an alternate `anthropic-version`) differs from the direct API.
///
/// `None` is the no-op variant: the provider falls back to whatever
/// credential it was constructed with (if any), and keyless providers
/// (Ollama) ignore the value entirely.
///
/// # Leak posture
///
/// - The inner secret is held in [`secrecy::SecretString`], whose `Debug`
///   impl renders `"[REDACTED ...]"` and whose backing bytes are zeroized
///   on drop.
/// - `ProviderAuth` deliberately implements `Debug` (delegating to the
///   variant tag + redacted secret) but **not** `Display` or `Serialize`.
///   The plaintext bytes leave the enum only through a provider-private
///   call site that builds an HTTP header value and drops it with the
///   request future.
#[derive(Clone)]
pub enum ProviderAuth {
    /// API key / bearer token. The provider chooses the wire header
    /// (`Authorization: Bearer …` or `x-api-key: …`) based on its own
    /// protocol.
    ApiKey(SecretString),
    /// Google Vertex AI access token obtained out-of-band (via
    /// `gcloud auth application-default print-access-token`). Distinct
    /// from `ApiKey` because the wire shape on Anthropic-on-Vertex
    /// differs (different version pin, `Authorization: Bearer` header).
    Vertex(SecretString),
    /// No per-request credential supplied. Providers fall back to their
    /// constructor-time credential (Anthropic / OpenAI today) or send
    /// nothing (Ollama). `None` is also the variant tests use when
    /// exercising the legacy `chat()` path through `chat_with_auth`.
    None,
}

impl std::fmt::Debug for ProviderAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The wrapped `SecretString` already redacts on `Debug`; the
        // variant tag is non-secret. We render the tag explicitly so the
        // shape stays useful in trace output without ever surfacing the
        // bytes.
        match self {
            ProviderAuth::ApiKey(_) => f.debug_tuple("ApiKey").field(&"[redacted]").finish(),
            ProviderAuth::Vertex(_) => f.debug_tuple("Vertex").field(&"[redacted]").finish(),
            ProviderAuth::None => f.write_str("None"),
        }
    }
}

// ── Provider trait ────────────────────────────────────────────────────────────

/// Streaming chat provider.
///
/// # Why `impl Future`, not `dyn`
///
/// `Provider` returns `impl std::future::Future<Output = ...> + Send`
/// rather than `Pin<Box<dyn Future ...>>`. The `impl Future` shape is
/// monomorphized per concrete provider, so we get zero allocation on
/// the hot per-turn entry path and the compiler's auto-trait inference
/// can confirm `Send` without manual `Box::pin`-ing each variant.
///
/// The cost is that `Provider` is **not object-safe**: `Arc<dyn Provider>`
/// is rejected by the compiler. That trade is deliberate — every
/// caller in the workspace today knows the concrete provider at
/// compile time except the hot-swap path, and the hot-swap path
/// resolves the lack of dynamic dispatch with the
/// [`crate::runtime::RuntimeProvider`] tagged-union enum: one `match`
/// arm per built-in provider, zero virtual call. See
/// [`crate::runtime::RuntimeProvider`] and
/// [`crate::runtime::SwappableProvider`] for the rationale and the
/// per-call dispatch shape.
pub trait Provider: Send + Sync {
    fn chat(
        &self,
        req: ChatRequest,
    ) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send;

    /// F-744: authenticated chat entry point. **Preferred entry point for
    /// credential-aware callers** — `chat()` is retained for compatibility
    /// with callers that don't thread per-turn credentials (tests, the
    /// `SwappableProvider` trampolines, the keyless `MockProvider`); new
    /// code should call `chat_with_auth` so the seam is exercised end to
    /// end. The trait keeps both methods because adding `#[deprecated]`
    /// would noise up every `SwappableProvider::chat` trampoline.
    ///
    /// The orchestrator calls this with the credential pulled from the
    /// keyring for the active provider on every turn. Implementations:
    ///
    /// - **Anthropic / OpenAI / CustomOpenAI**: use the supplied
    ///   [`ProviderAuth::ApiKey`] (or `Vertex`) to build the outbound
    ///   request's auth header, falling back to their constructor-time
    ///   credential when the seam passes `ProviderAuth::None` (preserves
    ///   the legacy `chat()` path for tests and callers that haven't
    ///   wired the seam yet).
    /// - **Ollama**: ignore the credential entirely — Ollama is keyless.
    ///
    /// The default impl simply discards the credential and delegates to
    /// [`Provider::chat`]. New providers added to the workspace get the
    /// keyless no-op for free; provider impls that want per-request auth
    /// override this method.
    fn chat_with_auth(
        &self,
        req: ChatRequest,
        _auth: ProviderAuth,
    ) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send {
        self.chat(req)
    }
}

// ── NDJSON deserialization ────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(untagged)]
enum RawChunk {
    Delta { delta: String },
    ToolCall { tool_call: RawToolCall },
    Done { done: String },
}

#[derive(Deserialize)]
struct RawToolCall {
    /// Optional in the NDJSON mock shape. Real providers (Anthropic,
    /// OpenAI) always carry an id; the mock shape predates the field
    /// and falls back to an empty string so the orchestrator's
    /// internal id is used for round-trip.
    #[serde(default)]
    id: String,
    name: String,
    args: serde_json::Value,
}

fn parse_ndjson(content: &str) -> Result<Vec<ChatChunk>> {
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let raw: RawChunk = serde_json::from_str(line)?;
            Ok(match raw {
                RawChunk::Delta { delta } => ChatChunk::TextDelta(delta),
                RawChunk::ToolCall { tool_call } => ChatChunk::ToolCall {
                    id: tool_call.id,
                    name: tool_call.name,
                    args: tool_call.args,
                },
                RawChunk::Done { done } => ChatChunk::Done(done),
            })
        })
        .collect()
}

// ── MockProvider ──────────────────────────────────────────────────────────────

enum MockSource {
    File(PathBuf),
    Sequence {
        scripts: Arc<Mutex<VecDeque<String>>>,
        log: Arc<Mutex<Vec<ChatRequest>>>,
    },
}

/// Scripted provider for testing.
///
/// Two construction modes:
/// - `new(path)` — reads NDJSON from a file on every call
/// - `from_responses(scripts)` — pops the next script per `chat()` call,
///   and records every received `ChatRequest` (see `recorded_requests()`)
pub struct MockProvider {
    source: MockSource,
}

impl MockProvider {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            source: MockSource::File(path.as_ref().to_path_buf()),
        }
    }

    pub fn with_default_path() -> Self {
        let path = dirs::config_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("forge/mock.json");
        Self::new(path)
    }

    /// Construct from a sequence of NDJSON scripts, one per expected `chat()` call.
    pub fn from_responses(scripts: Vec<String>) -> Result<Self> {
        Ok(Self {
            source: MockSource::Sequence {
                scripts: Arc::new(Mutex::new(scripts.into_iter().collect())),
                log: Arc::new(Mutex::new(Vec::new())),
            },
        })
    }

    /// All `ChatRequest` values received in call order (sequence mode only).
    pub fn recorded_requests(&self) -> Vec<ChatRequest> {
        match &self.source {
            MockSource::Sequence { log, .. } => log.lock().clone(),
            MockSource::File(_) => vec![],
        }
    }

    /// Stream directly from file (legacy; ignores request content).
    pub async fn stream(&self) -> Result<BoxStream<'static, ChatChunk>> {
        match &self.source {
            MockSource::File(path) => {
                let content = tokio::fs::read_to_string(path).await?;
                Ok(Box::pin(futures::stream::iter(parse_ndjson(&content)?)))
            }
            MockSource::Sequence { scripts, .. } => {
                let script = scripts.lock().pop_front().unwrap_or_default();
                Ok(Box::pin(futures::stream::iter(parse_ndjson(&script)?)))
            }
        }
    }
}

impl Provider for MockProvider {
    fn chat(
        &self,
        req: ChatRequest,
    ) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send {
        match &self.source {
            MockSource::File(path) => {
                let path = path.clone();
                futures::future::Either::Left(async move {
                    let content = tokio::fs::read_to_string(&path).await?;
                    Ok(Box::pin(futures::stream::iter(parse_ndjson(&content)?))
                        as BoxStream<'static, ChatChunk>)
                })
            }
            MockSource::Sequence { scripts, log } => {
                log.lock().push(req);
                let script = scripts.lock().pop_front().unwrap_or_default();
                let result: Result<BoxStream<'static, ChatChunk>> =
                    parse_ndjson(&script).map(|c| Box::pin(futures::stream::iter(c)) as _);
                futures::future::Either::Right(async move { result })
            }
        }
    }

    /// F-744: `MockProvider` ignores the seam credential — scripted
    /// responses don't talk to a network. Tests that need to assert on
    /// the value being threaded through the orchestrator should mount a
    /// `wiremock` server against a real provider impl instead; the mock
    /// path is for replay-only scenarios.
    fn chat_with_auth(
        &self,
        req: ChatRequest,
        _auth: ProviderAuth,
    ) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send {
        self.chat(req)
    }
}

#[cfg(test)]
mod mock_provider_concurrency_tests {
    use super::*;

    /// F-080: `MockProvider` previously held its inner state in
    /// `std::sync::Mutex`, which poisons on holder-thread panic and turns
    /// every subsequent `.lock().unwrap()` into a panic chain. With
    /// `parking_lot::Mutex` the surviving threads keep working — this test
    /// would panic under the old implementation.
    #[test]
    fn recorded_requests_survives_panicking_holder_thread() {
        let provider = Arc::new(
            MockProvider::from_responses(vec!["{\"done\":\"stop\"}\n".into()]).expect("construct"),
        );

        // A worker thread panics while it would otherwise have been holding
        // the mutex. With `std::sync::Mutex`, the next `lock()` would
        // observe a `PoisonError`; with `parking_lot` the next caller just
        // takes the lock.
        let panicker = {
            let p = Arc::clone(&provider);
            std::thread::spawn(move || {
                // Force a `MockSource::Sequence` access path equivalent to
                // the production holder, then panic mid-flight.
                let _snapshot = p.recorded_requests();
                panic!("simulated holder-thread failure");
            })
        };
        assert!(panicker.join().is_err(), "worker must have panicked");

        // The main thread can still observe the log.
        let observed = provider.recorded_requests();
        assert!(observed.is_empty(), "no requests have been logged yet");
    }
}
