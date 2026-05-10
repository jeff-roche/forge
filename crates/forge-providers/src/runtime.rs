//! F-586: hot-swappable [`Provider`] dispatcher.
//!
//! The [`Provider`] trait uses `impl Future` (not object-safe), so
//! `Arc<dyn Provider>` is not viable. To allow the dashboard's
//! `set_active_provider` IPC command to swap the active provider for the
//! next turn without restarting the session, we wrap the four built-ins
//! plus the test mock in a runtime enum and provide a hot-swap holder
//! via [`SwappableProvider`].
//!
//! The dispatch overhead is one `match` arm per `chat` call — negligible
//! compared to the network round-trip every variant performs.
//!
//! Construction site: `forge-session::serve_with_session` swaps its
//! `Arc<P>` parameter for an `Arc<SwappableProvider>` so a `ProviderChanged`
//! event delivered through the per-session swap channel takes effect on
//! the next `run_turn` invocation. The current turn finishes on the old
//! inner provider — switching mid-stream is out of scope (matches
//! F-586's DoD).

use std::sync::Arc;

use forge_core::Result;
use futures::stream::BoxStream;
use parking_lot::RwLock;

use crate::anthropic::AnthropicProvider;
use crate::ollama::OllamaProvider;
use crate::openai::custom::CustomOpenAiProvider;
use crate::openai::OpenAiProvider;
#[cfg(any(test, feature = "testing"))]
use crate::MockProvider;
use crate::{ChatChunk, ChatRequest, Provider};

/// Tagged-union of every concrete [`Provider`] implementation. Each chat
/// call dispatches via `match` to the appropriate inner — no dynamic
/// dispatch is needed because the trait is not object-safe today.
///
/// Each variant holds an [`Arc`] so the [`SwappableProvider`] can release
/// its lock guard *after* cloning the `Arc` for the active variant. The
/// concrete `chat()` futures borrow `&self` per the trait signature, so a
/// `parking_lot::RwLockReadGuard` cannot survive the network round-trip
/// (the guard isn't `Send`); the `Arc::clone` workaround keeps the
/// captured reference alive on a refcounted handle that doesn't depend on
/// the lock.
///
/// `Mock` is gated behind `#[cfg(any(test, feature = "testing"))]` so a
/// production binary cannot construct it. Crates that need to drive the
/// swap path from an integration test must enable
/// `forge-providers/testing` in their dev-dependencies. Without the gate,
/// any caller across the workspace could `swap()` to a Mock variant and
/// silently route real prompts to a scripted source.
#[derive(Clone)]
pub enum RuntimeProvider {
    Ollama(Arc<OllamaProvider>),
    Anthropic(Arc<AnthropicProvider>),
    OpenAi(Arc<OpenAiProvider>),
    CustomOpenAi(Arc<CustomOpenAiProvider>),
    #[cfg(any(test, feature = "testing"))]
    Mock(Arc<MockProvider>),
}

impl RuntimeProvider {
    /// Stable slug identifying which variant is currently active. Same
    /// shape the dashboard's `[providers.active]` setting and
    /// `Credentials::has_credential` use.
    pub fn id(&self) -> &str {
        match self {
            RuntimeProvider::Ollama(_) => "ollama",
            RuntimeProvider::Anthropic(_) => "anthropic",
            RuntimeProvider::OpenAi(_) => "openai",
            RuntimeProvider::CustomOpenAi(_) => "custom_openai",
            #[cfg(any(test, feature = "testing"))]
            RuntimeProvider::Mock(_) => "mock",
        }
    }
}

impl Provider for RuntimeProvider {
    fn chat(
        &self,
        req: ChatRequest,
    ) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send {
        // Clone the inner Arc so the future owns the inner state; this is
        // a refcount bump, not a deep copy. Each arm boxes a different
        // concrete future type into a uniform `BoxFuture` for return.
        let cloned = self.clone();
        async move {
            let fut: futures::future::BoxFuture<'static, _> = match cloned {
                RuntimeProvider::Ollama(p) => Box::pin(async move { p.chat(req).await }),
                RuntimeProvider::Anthropic(p) => Box::pin(async move { p.chat(req).await }),
                RuntimeProvider::OpenAi(p) => Box::pin(async move { p.chat(req).await }),
                RuntimeProvider::CustomOpenAi(p) => Box::pin(async move { p.chat(req).await }),
                #[cfg(any(test, feature = "testing"))]
                RuntimeProvider::Mock(p) => Box::pin(async move { p.chat(req).await }),
            };
            fut.await
        }
    }
}

/// Hot-swappable [`Provider`] holder. Stores its inner [`RuntimeProvider`]
/// behind an `Arc<RwLock<_>>` so the orchestrator can replace it in place
/// from a `ProviderChanged` listener without breaking the
/// `Arc<P: Provider>` shape `serve_with_session` expects.
///
/// Read-mostly: writers are only the swap path (one writer per
/// `ProviderChanged` event), readers are every `run_turn` invocation.
/// `parking_lot::RwLock` is used over `tokio::sync::RwLock` because the
/// critical section is brief and synchronous (a `match` arm); a turn-bound
/// async wait would only add latency.
pub struct SwappableProvider {
    inner: Arc<RwLock<RuntimeProvider>>,
}

impl SwappableProvider {
    /// Construct with an initial provider. The inner can be hot-swapped via
    /// [`Self::swap`].
    pub fn new(initial: RuntimeProvider) -> Self {
        Self {
            inner: Arc::new(RwLock::new(initial)),
        }
    }

    /// Atomically replace the inner provider. The replacement takes effect
    /// on the next [`Provider::chat`] call — any chat-stream already in
    /// flight continues against the previous inner because it captured a
    /// boxed future before the swap.
    pub fn swap(&self, next: RuntimeProvider) {
        *self.inner.write() = next;
    }

    /// Slug of the currently-active provider (matches the dashboard's
    /// `[providers.active]` shape).
    pub fn active_id(&self) -> String {
        self.inner.read().id().to_string()
    }
}

impl Provider for SwappableProvider {
    fn chat(
        &self,
        req: ChatRequest,
    ) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send {
        // Snapshot the active inner `RuntimeProvider` (an `Arc`-cloning
        // operation per variant) under a brief read lock, then drop the
        // guard. The async block then owns the cloned snapshot, which
        // holds Arc-backed state and can outlive any subsequent swap.
        // This is the contract that lets the next turn use the new inner
        // without disturbing the in-flight stream.
        //
        // Why clone the entire `RuntimeProvider` rather than borrow
        // `&self.inner` into the future:
        //
        // 1. `parking_lot::RwLockReadGuard<'_, RuntimeProvider>` is not
        //    `Send`. Holding it across an `.await` would refuse to compile
        //    — the future has to be `Send` to satisfy the trait return
        //    bound (`+ Send`).
        //
        // 2. `RuntimeProvider::Clone` is *cheap*: every variant wraps an
        //    `Arc<ConcreteProvider>`, so the clone is a per-variant
        //    refcount bump, not a deep copy of provider state (HTTP
        //    client, API key bytes, model name).
        //
        // 3. Holding the snapshot decouples the in-flight chat from any
        //    future swap: a `provider:changed` event mid-stream replaces
        //    `self.inner` but the captured `Arc` keeps the previous inner
        //    alive until the stream finishes. Swap-during-stream cannot
        //    yank the network connection out from under an active turn.
        let snapshot: RuntimeProvider = self.inner.read().clone();
        async move { snapshot.chat(req).await }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn mock_with_text(s: &str) -> MockProvider {
        MockProvider::from_responses(vec![format!(
            "{{\"delta\":\"{s}\"}}\n{{\"done\":\"end_turn\"}}\n"
        )])
        .expect("construct mock")
    }

    fn mock_arc(s: &str) -> Arc<MockProvider> {
        Arc::new(mock_with_text(s))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn runtime_provider_dispatches_to_inner_mock() {
        let rp = RuntimeProvider::Mock(mock_arc("hello"));
        let req = ChatRequest::default();
        let mut stream = rp.chat(req).await.expect("chat");
        let first = stream.next().await.expect("first chunk");
        assert_eq!(first, ChatChunk::TextDelta("hello".into()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn swappable_provider_initially_dispatches_to_first_inner() {
        let sp = SwappableProvider::new(RuntimeProvider::Mock(mock_arc("first")));
        assert_eq!(sp.active_id(), "mock");
        let mut stream = sp.chat(ChatRequest::default()).await.expect("chat");
        let first = stream.next().await.expect("first chunk");
        assert_eq!(first, ChatChunk::TextDelta("first".into()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn swappable_provider_uses_new_inner_after_swap() {
        // F-586 DoD #4: after a `ProviderChanged` event the next turn must
        // use the new provider. This test exercises the swap primitive that
        // the orchestrator's listener calls.
        let sp = SwappableProvider::new(RuntimeProvider::Mock(mock_arc("before")));

        // First call against the initial inner.
        {
            let mut stream = sp.chat(ChatRequest::default()).await.expect("chat 1");
            let chunk = stream.next().await.expect("chunk 1");
            assert_eq!(chunk, ChatChunk::TextDelta("before".into()));
        }

        // Swap mid-session — the next chat() must dispatch to the new
        // inner. Construct a fresh mock so the script is single-shot.
        sp.swap(RuntimeProvider::Mock(mock_arc("after")));

        let mut stream = sp.chat(ChatRequest::default()).await.expect("chat 2");
        let chunk = stream.next().await.expect("chunk 2");
        assert_eq!(chunk, ChatChunk::TextDelta("after".into()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn swappable_provider_active_id_reflects_swap() {
        let sp = SwappableProvider::new(RuntimeProvider::Mock(mock_arc("x")));
        assert_eq!(sp.active_id(), "mock");

        // Swap to an Ollama variant — id must update without reaching the
        // network. We can't easily construct a real `OllamaProvider` for
        // unit tests, but the public API is keyless construction, so this
        // is fine.
        sp.swap(RuntimeProvider::Ollama(Arc::new(OllamaProvider::new(
            "http://127.0.0.1:11434",
            "mistral",
        ))));
        assert_eq!(sp.active_id(), "ollama");
    }
}
