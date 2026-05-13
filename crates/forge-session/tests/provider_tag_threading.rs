//! F-752 regression: the `provider` + `model` fields on every
//! `AssistantMessage` event a turn emits must reflect the active provider
//! tag the orchestrator was wired with — not the legacy synthetic `"mock"`
//! pair the loop hardcoded pre-F-752.
//!
//! Pinning this end-to-end via `run_turn` (rather than poking
//! `run_request_loop` directly) is deliberate: it exercises the same wiring
//! path that `serve_with_session` / the daemon use in production, so a
//! future refactor that drops the tag inside `run_turn` (e.g. forgetting to
//! thread it into a new auto-compact branch) fails this test.
//!
//! Three assertions:
//!   1. A live turn driven by a non-mock tag emits `AssistantMessage`
//!      events tagged with that exact provider id + model.
//!   2. The default (no tag wired) preserves the legacy `"mock"` model
//!      shape so existing tests / downstream consumers keep observing the
//!      pre-F-752 placeholder when no tag is supplied.
//!   3. Auto-compaction's synthetic summary `AssistantMessage` uses the
//!      same tag as the live turn (the comment in `orchestrator.rs`
//!      promises this co-movement; F-752 makes it tested).

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use forge_core::{ids::MessageId, CompactTrigger, Event};
use forge_providers::MockProvider;
use forge_session::byte_budget::ByteBudget;
use forge_session::orchestrator::{run_turn, PendingApprovals, ProviderTag};
use forge_session::session::Session;
use tempfile::TempDir;
use tokio::sync::Mutex;

#[tokio::test]
async fn assistant_message_carries_active_provider_tag_when_wired() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());

    // The orchestrator emits an AssistantMessage at stream open AND a
    // finalised one at end_turn — both must carry the wired tag.
    let script = "{\"delta\":\"hello\"}\n{\"done\":\"end_turn\"}\n".to_string();
    let provider = Arc::new(MockProvider::from_responses(vec![script]).expect("construct mock"));

    let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
    let mut rx = session.event_tx.subscribe();

    let tag = ProviderTag::new("anthropic", "claude-3-5-sonnet-latest");
    let expected_provider = tag.provider_id.clone();
    let expected_model = tag.model.clone();

    run_turn(
        Arc::clone(&session),
        session.as_ref(),
        Arc::clone(&provider),
        "hi".to_string(),
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
        None, // credentials
        Some(tag),
    )
    .await
    .expect("turn completes");

    // Drain emissions; every AssistantMessage must carry the configured
    // provider+model pair.
    let mut assistant_msgs = 0;
    while let Ok((_, ev)) = rx.try_recv() {
        if let Event::AssistantMessage {
            provider, model, ..
        } = ev
        {
            assert_eq!(
                provider, expected_provider,
                "AssistantMessage.provider must reflect the wired ProviderTag",
            );
            assert_eq!(
                model, expected_model,
                "AssistantMessage.model must reflect the wired ProviderTag, \
                 not the legacy `\"mock\"` placeholder",
            );
            assistant_msgs += 1;
        }
    }
    assert!(
        assistant_msgs >= 1,
        "turn must emit at least one AssistantMessage",
    );
}

#[tokio::test]
async fn assistant_message_falls_back_to_mock_tag_when_unwired() {
    // Negative-side gate: an embedder that doesn't wire the tag must still
    // see the legacy `"mock"` model on AssistantMessage events. This pins
    // the backward-compat contract for the long tail of test fixtures and
    // downstream subscribers that key on the placeholder.
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());

    let script = "{\"delta\":\"hi\"}\n{\"done\":\"end_turn\"}\n".to_string();
    let provider = Arc::new(MockProvider::from_responses(vec![script]).expect("construct mock"));

    let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
    let mut rx = session.event_tx.subscribe();

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
        None,
        None, // F-752: no tag wired — falls back to the legacy mock pair.
    )
    .await
    .expect("turn completes");

    let mut saw_mock = false;
    while let Ok((_, ev)) = rx.try_recv() {
        if let Event::AssistantMessage { model, .. } = ev {
            assert_eq!(model, "mock", "fallback must emit the legacy `\"mock\"`");
            saw_mock = true;
        }
    }
    assert!(
        saw_mock,
        "fallback path must still emit an AssistantMessage"
    );
}

#[tokio::test]
async fn auto_compaction_summary_carries_active_provider_tag() {
    // F-752 invariant: the synthetic AssistantMessage emitted by the
    // auto-compaction summary call inherits the same provider+model as
    // the live turn. Without this, replay correlates summaries against a
    // synthetic `ProviderId::new()` and `"mock"` even though a real
    // provider served them.
    use forge_core::Event as Ev;

    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());

    // Seed prior turns so compaction has something to summarise.
    for n in 0..3 {
        session
            .emit(Ev::UserMessage {
                id: MessageId::new(),
                at: Utc::now(),
                text: Arc::from(format!("prior {n}").as_str()),
                context: vec![],
                branch_parent: None,
            })
            .await
            .unwrap();
        session
            .emit(Ev::AssistantMessage {
                id: MessageId::new(),
                provider: forge_core::ids::ProviderId::new(),
                model: "mock".into(),
                at: Utc::now(),
                stream_finalised: true,
                text: Arc::from(format!("answer {n}").as_str()),
                branch_parent: None,
                branch_variant_index: 0,
            })
            .await
            .unwrap();
    }

    // Two scripted provider responses: the privileged summary call, then
    // the actual turn that follows once compaction releases the gate.
    let summary = "{\"delta\":\"summarised\"}\n{\"done\":\"end_turn\"}\n".to_string();
    let live = "{\"delta\":\"answer\"}\n{\"done\":\"end_turn\"}\n".to_string();
    let provider =
        Arc::new(MockProvider::from_responses(vec![summary, live]).expect("construct mock"));

    // Tiny budget pre-charged past 98% so run_turn trips auto-compaction
    // on entry.
    let budget = Arc::new(ByteBudget::new(1_000));
    budget.charge(990);

    let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
    let mut rx = session.event_tx.subscribe();

    let tag = ProviderTag::new("openai", "gpt-4o");
    let expected_provider = tag.provider_id.clone();
    let expected_model = tag.model.clone();

    run_turn(
        Arc::clone(&session),
        session.as_ref(),
        Arc::clone(&provider),
        "fresh".to_string(),
        pending,
        vec![],
        true,
        None,
        None,
        Some(Arc::clone(&budget)),
        None,
        None,
        None,
        None,
        None,
        Some(tag),
    )
    .await
    .expect("turn completes");

    // Walk the emissions; pull out the AssistantMessage that arrives
    // immediately before `ContextCompacted` — that's the synthetic
    // summary. It must carry the wired tag, not the legacy mock pair.
    let mut events = Vec::new();
    while let Ok((_, ev)) = rx.try_recv() {
        events.push(ev);
    }
    let compacted_idx = events
        .iter()
        .position(|ev| matches!(ev, Ev::ContextCompacted { trigger, .. } if *trigger == CompactTrigger::AutoAt98Pct))
        .expect("auto-trigger must emit ContextCompacted");
    // Summary AssistantMessage is the most recent one before ContextCompacted.
    let summary_msg = events[..compacted_idx]
        .iter()
        .rev()
        .find_map(|ev| match ev {
            Ev::AssistantMessage {
                provider, model, ..
            } => Some((provider.clone(), model.clone())),
            _ => None,
        })
        .expect("summary AssistantMessage must precede ContextCompacted");
    assert_eq!(
        summary_msg.0, expected_provider,
        "summary AssistantMessage.provider must match the live-turn tag",
    );
    assert_eq!(
        summary_msg.1, expected_model,
        "summary AssistantMessage.model must match the live-turn tag",
    );
}
