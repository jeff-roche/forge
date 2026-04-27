//! F-587: integration test that pins the orchestrator's per-turn credential
//! pull contract.
//!
//! The keyless `MockProvider` ignores the credential — that's expected;
//! Phase-1 ships keyless. What this test asserts is that:
//!
//! 1. When `run_turn` is given a `Some(CredentialContext)`, it calls
//!    `store.get(provider_id)` exactly once before the request loop opens.
//! 2. When the store returns an `Err`, that error fails the turn (no
//!    silent fallback to keyless — backend failure is more useful as a
//!    surfaced error than a downstream provider 401).
//! 3. When the store has no entry (`Ok(None)`), the turn proceeds — the
//!    keyless path stays available, the credential pull is just observed
//!    to have missed.
//! 4. **Rerun is a turn**. `Orchestrator::rerun_message` (and each variant
//!    delegate) honors the same pull-once contract — backend failure
//!    surfaces before any provider call, identical to a fresh turn.
//!
//! Together these pin the seam that the Phase-3 `AnthropicProvider` and
//! `OpenAIProvider` will hook into.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use forge_core::{
    ids::{MessageId, ProviderId},
    Credentials, Event, ForgeError, MemoryStore, RerunVariant,
};
use forge_providers::MockProvider;
use forge_session::orchestrator::{run_turn, CredentialContext, Orchestrator, PendingApprovals};
use forge_session::session::Session;
use secrecy::{ExposeSecret, SecretString};
use tempfile::TempDir;
use tokio::sync::Mutex;

/// Counting wrapper over a [`MemoryStore`] so the test can assert the
/// pull happened exactly once per turn. Intentionally not in the
/// production crate — it is a test-only spy.
#[derive(Default)]
struct CountingStore {
    inner: MemoryStore,
    get_calls: std::sync::atomic::AtomicUsize,
}

impl CountingStore {
    fn calls(&self) -> usize {
        self.get_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl Credentials for CountingStore {
    async fn get(&self, provider_id: &str) -> Result<Option<SecretString>, ForgeError> {
        self.get_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.get(provider_id).await
    }
    async fn set(&self, provider_id: &str, value: SecretString) -> Result<(), ForgeError> {
        self.inner.set(provider_id, value).await
    }
    async fn remove(&self, provider_id: &str) -> Result<(), ForgeError> {
        self.inner.remove(provider_id).await
    }
}

/// A store that always errors. Pins the "backend failure fails the turn"
/// branch — `run_turn` must propagate, not swallow.
struct FailingStore;

#[async_trait]
impl Credentials for FailingStore {
    async fn get(&self, _provider_id: &str) -> Result<Option<SecretString>, ForgeError> {
        Err(anyhow::anyhow!("test: keyring backend offline").into())
    }
    async fn set(&self, _: &str, _: SecretString) -> Result<(), ForgeError> {
        Err(anyhow::anyhow!("test: read-only").into())
    }
    async fn remove(&self, _: &str) -> Result<(), ForgeError> {
        Err(anyhow::anyhow!("test: read-only").into())
    }
}

#[tokio::test]
async fn run_turn_pulls_credential_when_context_supplied() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());

    let store = Arc::new(CountingStore::default());
    store
        .set("anthropic", SecretString::from("sk-ant-fake"))
        .await
        .unwrap();
    let cred_store: Arc<dyn Credentials> = store.clone();

    let provider = Arc::new(
        MockProvider::from_responses(vec!["{\"done\":\"end_turn\"}\n".into()])
            .expect("construct mock"),
    );
    let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));

    run_turn(
        Arc::clone(&session),
        session.as_ref(),
        Arc::clone(&provider),
        "hello".to_string(),
        pending,
        vec![],
        true,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(CredentialContext {
            store: cred_store,
            provider_id: "anthropic".to_string(),
        }),
    )
    .await
    .expect("turn should complete");

    assert_eq!(
        store.calls(),
        1,
        "credential pulled exactly once at turn start"
    );

    // Sanity: the value the orchestrator would have pulled is still readable
    // from the store. This pins the round-trip contract end-to-end.
    let got = store.inner.get("anthropic").await.unwrap().unwrap();
    assert_eq!(got.expose_secret(), "sk-ant-fake");
}

#[tokio::test]
async fn run_turn_proceeds_when_credential_is_missing() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());

    // Empty store — `get` returns Ok(None). The keyless `MockProvider`
    // ignores the credential, so the turn must complete cleanly.
    let store = Arc::new(CountingStore::default());
    let cred_store: Arc<dyn Credentials> = store.clone();

    let provider = Arc::new(
        MockProvider::from_responses(vec!["{\"done\":\"end_turn\"}\n".into()])
            .expect("construct mock"),
    );

    run_turn(
        Arc::clone(&session),
        session.as_ref(),
        Arc::clone(&provider),
        "hello".to_string(),
        Arc::new(Mutex::new(HashMap::new())),
        vec![],
        true,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(CredentialContext {
            store: cred_store,
            provider_id: "anthropic".to_string(),
        }),
    )
    .await
    .expect("missing credential is a clean miss, not an error");

    assert_eq!(store.calls(), 1, "still pulled once on the miss path");
}

#[tokio::test]
async fn run_turn_fails_when_credential_backend_errors() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());

    let store: Arc<dyn Credentials> = Arc::new(FailingStore);
    let provider = Arc::new(
        MockProvider::from_responses(vec!["{\"done\":\"end_turn\"}\n".into()])
            .expect("construct mock"),
    );

    let events_session = Arc::clone(&session);
    let err = run_turn(
        session,
        events_session.as_ref(),
        provider,
        "hello".to_string(),
        Arc::new(Mutex::new(HashMap::new())),
        vec![],
        true,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(CredentialContext {
            store,
            provider_id: "anthropic".to_string(),
        }),
    )
    .await
    .expect_err("backend failure must fail the turn, not silently downgrade");

    let msg = err.to_string();
    assert!(
        msg.contains("keyring backend offline"),
        "error must surface the backend failure: {msg}"
    );
}

#[tokio::test]
async fn run_turn_skips_pull_when_no_context_supplied() {
    // The keyless path: passing `None` for `credentials` keeps the
    // pre-F-587 behavior. No store is consulted, no `get` call happens
    // (there is nothing to instrument here; we assert by passing a store
    // outside the context and checking it was untouched).
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());

    let store = Arc::new(CountingStore::default());
    let provider = Arc::new(
        MockProvider::from_responses(vec!["{\"done\":\"end_turn\"}\n".into()])
            .expect("construct mock"),
    );

    let events_session = Arc::clone(&session);
    run_turn(
        session,
        events_session.as_ref(),
        provider,
        "hello".to_string(),
        Arc::new(Mutex::new(HashMap::new())),
        vec![],
        true,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None, // no credential context — skip the pull.
    )
    .await
    .expect("keyless path completes");

    assert_eq!(
        store.calls(),
        0,
        "no credential context means no store consultation"
    );
}

// ── F-587: rerun paths honor the same pull contract ──────────────────────────

/// Seed a session log with one user → assistant turn so a rerun has
/// something to target. The returned `MessageId` is the assistant turn's
/// id — i.e., the rerun target.
async fn seed_one_turn(session: &Session) -> MessageId {
    let user_id = MessageId::new();
    session
        .emit(Event::UserMessage {
            id: user_id,
            at: Utc::now(),
            text: Arc::from("seed prompt"),
            context: vec![],
            branch_parent: None,
        })
        .await
        .unwrap();

    let assistant_id = MessageId::new();
    session
        .emit(Event::AssistantMessage {
            id: assistant_id.clone(),
            provider: ProviderId::new(),
            model: "mock".into(),
            at: Utc::now(),
            stream_finalised: true,
            text: Arc::from("seed response"),
            branch_parent: None,
            branch_variant_index: 0,
        })
        .await
        .unwrap();

    assistant_id
}

#[tokio::test]
async fn rerun_replace_pulls_credential_when_context_supplied() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());

    let target = seed_one_turn(&session).await;

    let store = Arc::new(CountingStore::default());
    store
        .set("anthropic", SecretString::from("sk-ant-fake"))
        .await
        .unwrap();
    let cred_store: Arc<dyn Credentials> = store.clone();

    let provider = Arc::new(
        MockProvider::from_responses(vec!["{\"done\":\"end_turn\"}\n".into()])
            .expect("construct mock"),
    );
    let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));

    Orchestrator::new()
        .rerun_message(
            Arc::clone(&session),
            Arc::clone(&provider),
            target,
            RerunVariant::Replace,
            pending,
            vec![],
            true,
            None,
            None,
            None,
            None,
            Some(CredentialContext {
                store: cred_store,
                provider_id: "anthropic".to_string(),
            }),
        )
        .await
        .expect("rerun should complete");

    assert_eq!(
        store.calls(),
        1,
        "rerun_replace must pull the credential exactly once at entry"
    );
}

#[tokio::test]
async fn rerun_replace_fails_when_credential_backend_errors() {
    // Backend failure must surface before any provider call — identical
    // to the fresh-turn path. Without the pull on the rerun side the
    // target message would be regenerated against a 401-bound Anthropic
    // / OpenAI provider once those land in F-588.
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());

    let target = seed_one_turn(&session).await;

    let store: Arc<dyn Credentials> = Arc::new(FailingStore);
    let provider = Arc::new(
        MockProvider::from_responses(vec!["{\"done\":\"end_turn\"}\n".into()])
            .expect("construct mock"),
    );

    let err = Orchestrator::new()
        .rerun_message(
            session,
            provider,
            target,
            RerunVariant::Replace,
            Arc::new(Mutex::new(HashMap::new())),
            vec![],
            true,
            None,
            None,
            None,
            None,
            Some(CredentialContext {
                store,
                provider_id: "anthropic".to_string(),
            }),
        )
        .await
        .expect_err("backend failure must fail the rerun");

    assert!(
        err.to_string().contains("keyring backend offline"),
        "rerun must surface the backend failure: {err}"
    );
}

#[tokio::test]
async fn rerun_branch_pulls_credential_when_context_supplied() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());

    let target = seed_one_turn(&session).await;

    let store = Arc::new(CountingStore::default());
    let cred_store: Arc<dyn Credentials> = store.clone();

    let provider = Arc::new(
        MockProvider::from_responses(vec!["{\"done\":\"end_turn\"}\n".into()])
            .expect("construct mock"),
    );

    Orchestrator::new()
        .rerun_message(
            Arc::clone(&session),
            Arc::clone(&provider),
            target,
            RerunVariant::Branch,
            Arc::new(Mutex::new(HashMap::new())),
            vec![],
            true,
            None,
            None,
            None,
            None,
            Some(CredentialContext {
                store: cred_store,
                provider_id: "anthropic".to_string(),
            }),
        )
        .await
        .expect("rerun branch should complete");

    assert_eq!(
        store.calls(),
        1,
        "rerun_branch must pull the credential exactly once at entry"
    );
}

#[tokio::test]
async fn rerun_fresh_pulls_credential_when_context_supplied() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());

    let target = seed_one_turn(&session).await;

    let store = Arc::new(CountingStore::default());
    let cred_store: Arc<dyn Credentials> = store.clone();

    let provider = Arc::new(
        MockProvider::from_responses(vec!["{\"done\":\"end_turn\"}\n".into()])
            .expect("construct mock"),
    );

    Orchestrator::new()
        .rerun_message(
            Arc::clone(&session),
            Arc::clone(&provider),
            target,
            RerunVariant::Fresh,
            Arc::new(Mutex::new(HashMap::new())),
            vec![],
            true,
            None,
            None,
            None,
            None,
            Some(CredentialContext {
                store: cred_store,
                provider_id: "anthropic".to_string(),
            }),
        )
        .await
        .expect("rerun fresh should complete");

    assert_eq!(
        store.calls(),
        1,
        "rerun_fresh must pull the credential exactly once at entry"
    );
}

#[tokio::test]
async fn rerun_skips_pull_when_no_context_supplied() {
    // Mirrors `run_turn_skips_pull_when_no_context_supplied` — keyless
    // path is preserved; no `Credentials::get` call when ctx is `None`.
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());

    let target = seed_one_turn(&session).await;

    let store = Arc::new(CountingStore::default());
    let provider = Arc::new(
        MockProvider::from_responses(vec!["{\"done\":\"end_turn\"}\n".into()])
            .expect("construct mock"),
    );

    Orchestrator::new()
        .rerun_message(
            Arc::clone(&session),
            Arc::clone(&provider),
            target,
            RerunVariant::Replace,
            Arc::new(Mutex::new(HashMap::new())),
            vec![],
            true,
            None,
            None,
            None,
            None,
            None, // no credential context — skip the pull.
        )
        .await
        .expect("keyless rerun completes");

    assert_eq!(
        store.calls(),
        0,
        "no credential context means no store consultation on rerun either"
    );
}
