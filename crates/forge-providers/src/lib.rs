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
use serde::Deserialize;

pub mod anthropic;
// F-679: shared HTTP / SSE helpers used by Anthropic and the OpenAI family.
// Crate-private — the only consumers are sibling provider modules.
pub(crate) mod http_util;
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
    /// e.g. Anthropic hoists it to a top-level `system` field, Ollama
    /// prepends it to the message stream. `None` means "no system prompt".
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
    ToolCall {
        name: String,
        args: serde_json::Value,
    },
    Done(String),
    /// Terminal, structured stream failure. The chunk stream closes after
    /// yielding this variant — callers should treat the current turn as aborted.
    Error {
        kind: StreamErrorKind,
        message: String,
    },
}

/// Why a provider stream terminated abnormally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamErrorKind {
    /// A single NDJSON line exceeded the per-line byte cap.
    LineTooLong,
    /// No bytes received within the inter-chunk idle window.
    IdleTimeout,
    /// The overall stream exceeded its wall-clock budget.
    WallClockTimeout,
    /// Transport-level error from the underlying reader.
    Transport,
}

// ── Provider trait ────────────────────────────────────────────────────────────

/// Streaming chat provider.
pub trait Provider: Send + Sync {
    fn chat(
        &self,
        req: ChatRequest,
    ) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send;
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
