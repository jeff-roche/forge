//! F-608 step 6: orchestrator-side sidecar credential push.
//!
//! Pins three guarantees the architecture doc §5 calls out by name:
//!
//! 1. When `CredentialContext.sidecar_push` is `Some` AND the credential
//!    pull hits, `run_turn` frames the value as a
//!    [`SidecarMessage::Credentials`] and sends it on the supplied
//!    channel **before** the orchestrator's caller would emit `RunTurn`.
//! 2. When `sidecar_push` is `None` (in-process / `FORGE_AGENT_SIDECAR=0`
//!    path), no frame is sent. Behavior is byte-for-byte identical to
//!    the pre-step-6 keyless path.
//! 3. The credential value never appears in any captured log line at any
//!    level — even when the test injects a `tracing::trace`-grade
//!    subscriber. The architecture doc explicitly forbids leaking the
//!    secret bytes through the supervisor's tracing layer.
//!
//! Together these pin the seam Phase-3 `AnthropicProvider` /
//! `OpenAIProvider` will plug into.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use forge_core::{
    ids::{AgentInstanceId, MessageId, ProviderId},
    Credentials, Event, MemoryStore, RerunVariant,
};
use forge_ipc::sidecar::SidecarMessage;
use forge_providers::MockProvider;
use forge_session::orchestrator::{
    run_turn, CredentialContext, Orchestrator, PendingApprovals, SidecarCredentialPush,
};
use forge_session::session::Session;
use secrecy::SecretString;
use tempfile::TempDir;
use tokio::sync::{mpsc, Mutex as AsyncMutex};

mod common;

/// Test secret value — pinned here so the log-redaction assertion can
/// scan captured output for it. Pick something distinctive enough that a
/// false positive on an unrelated string is implausible.
const TEST_SECRET_VALUE: &str = "sk-ant-step6-leakcanary-XYZZY";

/// Construct a session, mock provider, and the boilerplate scaffolding
/// `run_turn` needs. Centralized so each test stays focused on the seam.
async fn fixtures() -> (Arc<Session>, Arc<MockProvider>, PendingApprovals, TempDir) {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(
        MockProvider::from_responses(vec!["{\"done\":\"end_turn\"}\n".into()])
            .expect("construct mock"),
    );
    let pending: PendingApprovals = Arc::new(AsyncMutex::new(HashMap::new()));
    (session, provider, pending, dir)
}

/// Acceptance: `run_turn` pushes a `Credentials` frame on the supplied
/// channel before opening the request loop, and the frame round-trips to
/// the same secret bytes the keyring stored.
#[tokio::test]
async fn run_turn_pushes_credentials_frame_when_sidecar_hook_set() {
    let (session, provider, pending, _dir) = fixtures().await;

    let store: Arc<dyn Credentials> = {
        let s = Arc::new(MemoryStore::new());
        s.set("anthropic", SecretString::from(TEST_SECRET_VALUE))
            .await
            .unwrap();
        s
    };

    let (tx, mut rx) = mpsc::channel::<SidecarMessage>(8);
    let instance_id = AgentInstanceId::from_string("inst-push-1".into());

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
            store: Arc::clone(&store),
            provider_id: "anthropic".to_string(),
            sidecar_push: Some(SidecarCredentialPush {
                instance_id: instance_id.clone(),
                command_tx: tx,
            }),
        }),
    )
    .await
    .expect("turn should complete");

    // The push happens synchronously inside `pull_active_credential`,
    // so a non-blocking `try_recv` is sufficient.
    let frame = rx
        .try_recv()
        .expect("a Credentials frame must have been pushed");
    match frame {
        SidecarMessage::Credentials(c) => {
            assert_eq!(c.provider_id.to_string(), "anthropic");
            assert_eq!(
                c.secret.expose_bytes(),
                TEST_SECRET_VALUE.as_bytes(),
                "wire frame must carry the exact secret bytes"
            );
        }
        other => panic!("expected Credentials, got {other:?}"),
    }

    // No follow-up frames — the run-turn body is in-process for this
    // test (no `FORGE_AGENT_SIDECAR` registry path), so only the
    // credential push reaches the channel.
    assert!(
        rx.try_recv().is_err(),
        "no further frames should be pushed by an in-process run_turn"
    );
}

/// Acceptance: when the credential pull misses (`Ok(None)`), no frame is
/// sent. Keyless providers rely on this — pushing an empty credential
/// would teach the sidecar a bogus "I have a key" signal for a provider
/// that's actually keyless.
#[tokio::test]
async fn run_turn_does_not_push_when_credential_miss() {
    let (session, provider, pending, _dir) = fixtures().await;

    // Empty store — `get` returns Ok(None).
    let store: Arc<dyn Credentials> = Arc::new(MemoryStore::new());
    let (tx, mut rx) = mpsc::channel::<SidecarMessage>(8);
    let instance_id = AgentInstanceId::from_string("inst-miss".into());

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
            store,
            provider_id: "anthropic".to_string(),
            sidecar_push: Some(SidecarCredentialPush {
                instance_id,
                command_tx: tx,
            }),
        }),
    )
    .await
    .expect("missing credential must remain a clean miss");

    assert!(
        rx.try_recv().is_err(),
        "miss path must not push a Credentials frame"
    );
}

/// Acceptance: when `sidecar_push` is `None`, the frame channel is never
/// touched (there isn't one to touch). Pins the in-process /
/// flag-unset path is byte-for-byte unchanged from pre-step-6.
#[tokio::test]
async fn run_turn_does_not_push_without_sidecar_hook() {
    let (session, provider, pending, _dir) = fixtures().await;

    let store: Arc<dyn Credentials> = {
        let s = Arc::new(MemoryStore::new());
        s.set("anthropic", SecretString::from(TEST_SECRET_VALUE))
            .await
            .unwrap();
        s
    };

    // No channel → `sidecar_push: None`.
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
            store,
            provider_id: "anthropic".to_string(),
            sidecar_push: None,
        }),
    )
    .await
    .expect("turn should complete");
    // Reaching this line at all is the assertion: the in-process path
    // didn't touch a channel that wasn't there.
}

/// Acceptance (security DoD): the credential value `TEST_SECRET_VALUE`
/// must never appear in captured tracing output, **at any level** —
/// even with a `trace`-grade filter that catches the
/// `forge_session::orchestrator::credentials::pushed credential` line
/// and the sidecar's stash emission.
///
/// Drives the full daemon-side push path with a `sidecar_push` hook
/// wired to a real channel, then scans the captured byte buffer for the
/// distinctive secret value. Architecture doc §5 forbids any
/// observation channel that exposes the bytes.
///
/// Uses the existing `tests/common/mod.rs` capture-subscriber pattern
/// (mirrors `bg_agents_tracing.rs` and `forge-shell`'s test harness):
/// install once globally, serialize tests via `capture_test_lock` so
/// `drain_capture` reflects only this test's emissions.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn secret_value_never_appears_in_logs() {
    let _serial = common::capture_test_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    common::install_capture_subscriber();
    let _drain_initial = common::drain_capture();

    let (session, provider, pending, _dir) = fixtures().await;

    let store: Arc<dyn Credentials> = {
        let s = Arc::new(MemoryStore::new());
        s.set("anthropic", SecretString::from(TEST_SECRET_VALUE))
            .await
            .unwrap();
        s
    };

    let (tx, mut rx) = mpsc::channel::<SidecarMessage>(8);
    let instance_id = AgentInstanceId::from_string("inst-leak-test".into());

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
            store,
            provider_id: "anthropic".to_string(),
            sidecar_push: Some(SidecarCredentialPush {
                instance_id,
                command_tx: tx,
            }),
        }),
    )
    .await
    .expect("turn should complete");

    // Drain the channel so any `Debug`-leaking close-down log
    // emission would also be captured.
    let _ = rx.try_recv();
    drop(rx);

    let captured_text = common::drain_capture();

    assert!(
        !captured_text.contains(TEST_SECRET_VALUE),
        "credential value leaked into captured logs:\n{captured_text}"
    );

    // Belt-and-braces: the architecture-doc-mandated trace line MUST
    // appear, since its presence proves the push code path actually
    // ran (i.e. the negative assertion above isn't vacuously satisfied
    // by a no-op control flow).
    assert!(
        captured_text.contains("pushed credential"),
        "expected the architecture-doc-mandated trace emission; \
         absence implies the push path didn't run:\n{captured_text}"
    );
    assert!(
        captured_text.contains("inst-leak-test"),
        "instance_id should appear in trace output:\n{captured_text}"
    );
    assert!(
        captured_text.contains("anthropic"),
        "non-secret provider_id should appear in logs:\n{captured_text}"
    );
}

/// Acceptance: the rerun paths honor the same push contract as the
/// fresh-turn entry, identical to the existing
/// `rerun_*_pulls_credential_when_context_supplied` tests in
/// `credentials_pull.rs`. Without this the rerun path would
/// regenerate a target message against a sidecar that hasn't been
/// handed the active credential.
#[tokio::test]
async fn rerun_replace_pushes_credentials_frame() {
    let (session, provider, _pending, _dir) = fixtures().await;

    // Seed one user → assistant turn so the rerun has a target.
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

    let store: Arc<dyn Credentials> = {
        let s = Arc::new(MemoryStore::new());
        s.set("anthropic", SecretString::from(TEST_SECRET_VALUE))
            .await
            .unwrap();
        s
    };

    let (tx, mut rx) = mpsc::channel::<SidecarMessage>(8);
    let instance_id = AgentInstanceId::from_string("inst-rerun".into());

    Orchestrator::new()
        .rerun_message(
            Arc::clone(&session),
            Arc::clone(&provider),
            assistant_id,
            RerunVariant::Replace,
            Arc::new(AsyncMutex::new(HashMap::new())),
            vec![],
            true,
            None,
            None,
            None,
            None,
            Some(CredentialContext {
                store,
                provider_id: "anthropic".to_string(),
                sidecar_push: Some(SidecarCredentialPush {
                    instance_id,
                    command_tx: tx,
                }),
            }),
        )
        .await
        .expect("rerun should complete");

    let frame = rx
        .try_recv()
        .expect("rerun must push a Credentials frame at entry");
    match frame {
        SidecarMessage::Credentials(c) => {
            assert_eq!(c.provider_id.to_string(), "anthropic");
            assert_eq!(c.secret.expose_bytes(), TEST_SECRET_VALUE.as_bytes());
        }
        other => panic!("expected Credentials, got {other:?}"),
    }
}
