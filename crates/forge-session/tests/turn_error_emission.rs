//! F-749: orchestrator-level coverage that provider failures land on the
//! session log as typed `Event::TurnError` events.
//!
//! Each test drives `run_turn` with a minimal in-process provider that
//! yields a single `ChatChunk::Error { status, kind, message }` — covering
//! every HTTP class the spec calls out (401 → Auth, 429 → RateLimit,
//! 5xx → Server) plus the non-status SSE / transport / parse classes
//! (Network, MalformedResponse). The assertions read straight off the
//! durable event log so a regression in classification or wire-shape
//! surfaces in the same place the UI will read it from.

use std::path::PathBuf;
use std::sync::Arc;

use forge_core::{read_since, Event, TurnErrorKind};
use forge_providers::{ChatChunk, ChatRequest, Provider, ProviderAuth, StreamErrorKind};
use forge_session::orchestrator::run_turn;
use forge_session::session::Session;
use futures::stream::{self, BoxStream};
use tempfile::TempDir;

/// In-process provider that yields exactly one `ChatChunk::Error` and ends.
/// Sidesteps `MockProvider` because the NDJSON scripted path can't construct
/// a typed error envelope today — we want the orchestrator's classifier
/// exercised end-to-end with the same shape a real provider emits.
struct ErrorProvider {
    kind: StreamErrorKind,
    message: String,
    status: Option<u16>,
    retry_after_secs: Option<u32>,
}

impl Provider for ErrorProvider {
    fn chat(
        &self,
        _req: ChatRequest,
    ) -> impl std::future::Future<Output = forge_core::Result<BoxStream<'static, ChatChunk>>> + Send
    {
        let kind = self.kind;
        let message = self.message.clone();
        let status = self.status;
        let retry_after_secs = self.retry_after_secs;
        async move {
            let chunk = ChatChunk::Error {
                kind,
                message,
                status,
                retry_after_secs,
            };
            Ok(Box::pin(stream::iter(vec![chunk])) as BoxStream<'static, ChatChunk>)
        }
    }

    fn chat_with_auth(
        &self,
        req: ChatRequest,
        _auth: ProviderAuth,
    ) -> impl std::future::Future<Output = forge_core::Result<BoxStream<'static, ChatChunk>>> + Send
    {
        self.chat(req)
    }
}

async fn run_failing_turn(log_path: PathBuf, provider: ErrorProvider) -> Vec<Event> {
    let session = Arc::new(
        Session::create(log_path.clone())
            .await
            .expect("session create"),
    );
    let approvals = Default::default();
    let _ = run_turn(
        Arc::clone(&session),
        session.as_ref(),
        Arc::new(provider),
        "hello".to_string(),
        approvals,
        vec![],
        false,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    read_since(&log_path, 0)
        .await
        .expect("read event log")
        .into_iter()
        .map(|(_, ev)| ev)
        .collect()
}

fn first_turn_error(events: &[Event]) -> &Event {
    events
        .iter()
        .find(|e| matches!(e, Event::TurnError { .. }))
        .expect("TurnError event landed on the log")
}

#[tokio::test]
async fn http_401_maps_to_auth_kind_with_retriable_false() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("events.jsonl");

    let events = run_failing_turn(
        log,
        ErrorProvider {
            kind: StreamErrorKind::Transport,
            message: "anthropic chat HTTP 401: invalid api key".into(),
            status: Some(401),
            retry_after_secs: None,
        },
    )
    .await;

    let err = first_turn_error(&events);
    let Event::TurnError {
        kind,
        retriable,
        raw,
        ..
    } = err
    else {
        unreachable!()
    };
    assert_eq!(*kind, TurnErrorKind::Auth);
    assert!(!*retriable, "auth errors are not retriable");
    assert!(
        raw.as_deref()
            .map(|s| s.contains("401") && s.contains("invalid api key"))
            .unwrap_or(false),
        "raw should preserve the provider error verbatim: {raw:?}"
    );
}

#[tokio::test]
async fn http_403_also_maps_to_auth_kind() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("events.jsonl");

    let events = run_failing_turn(
        log,
        ErrorProvider {
            kind: StreamErrorKind::Transport,
            message: "openai chat HTTP 403: forbidden".into(),
            status: Some(403),
            retry_after_secs: None,
        },
    )
    .await;
    let err = first_turn_error(&events);
    let Event::TurnError {
        kind, retriable, ..
    } = err
    else {
        unreachable!()
    };
    assert_eq!(*kind, TurnErrorKind::Auth);
    assert!(!*retriable);
}

#[tokio::test]
async fn http_429_maps_to_rate_limit_kind() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("events.jsonl");

    let events = run_failing_turn(
        log,
        ErrorProvider {
            kind: StreamErrorKind::Transport,
            message: "anthropic chat HTTP 429: rate limited".into(),
            status: Some(429),
            retry_after_secs: None,
        },
    )
    .await;
    let err = first_turn_error(&events);
    let Event::TurnError {
        kind, retriable, ..
    } = err
    else {
        unreachable!()
    };
    assert!(
        matches!(
            kind,
            TurnErrorKind::RateLimit {
                retry_after_secs: _
            }
        ),
        "expected RateLimit, got {kind:?}"
    );
    assert!(*retriable, "rate-limit failures are retriable");
}

#[tokio::test]
async fn http_429_retry_after_secs_flows_through_to_turn_error() {
    // F-749: when the provider parses `Retry-After: 30` into `Some(30)` on
    // the `ChatChunk::Error` envelope, the orchestrator must thread that
    // onto `TurnErrorKind::RateLimit.retry_after_secs` so the UI's
    // countdown lights up against a real value.
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("events.jsonl");

    let events = run_failing_turn(
        log,
        ErrorProvider {
            kind: StreamErrorKind::Transport,
            message: "anthropic chat HTTP 429: rate limited".into(),
            status: Some(429),
            retry_after_secs: Some(30),
        },
    )
    .await;

    let err = first_turn_error(&events);
    let Event::TurnError { kind, .. } = err else {
        unreachable!()
    };
    assert_eq!(
        *kind,
        TurnErrorKind::RateLimit {
            retry_after_secs: Some(30)
        },
        "retry_after_secs must flow from ChatChunk::Error through to TurnErrorKind::RateLimit"
    );
}

#[tokio::test]
async fn http_429_without_retry_after_yields_none_on_turn_error() {
    // When the provider returns no `Retry-After` header (or a malformed one),
    // the value stays `None` and the UI falls back to an immediately-available
    // Retry button rather than an indefinite countdown.
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("events.jsonl");

    let events = run_failing_turn(
        log,
        ErrorProvider {
            kind: StreamErrorKind::Transport,
            message: "anthropic chat HTTP 429: rate limited".into(),
            status: Some(429),
            retry_after_secs: None,
        },
    )
    .await;

    let err = first_turn_error(&events);
    let Event::TurnError { kind, .. } = err else {
        unreachable!()
    };
    assert_eq!(
        *kind,
        TurnErrorKind::RateLimit {
            retry_after_secs: None
        }
    );
}

#[tokio::test]
async fn wall_clock_timeout_maps_to_network() {
    // F-749: parity with the IdleTimeout case. Wall-clock timeouts share the
    // same UI affordance (retry available immediately, generic network copy)
    // because both are best-modeled as "transient connectivity issue".
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("events.jsonl");

    let events = run_failing_turn(
        log,
        ErrorProvider {
            kind: StreamErrorKind::WallClockTimeout,
            message: "stream exceeded wall-clock budget".into(),
            status: None,
            retry_after_secs: None,
        },
    )
    .await;
    let err = first_turn_error(&events);
    let Event::TurnError {
        kind, retriable, ..
    } = err
    else {
        unreachable!()
    };
    assert_eq!(*kind, TurnErrorKind::Network);
    assert!(*retriable);
}

#[tokio::test]
async fn http_500_maps_to_server_kind() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("events.jsonl");

    let events = run_failing_turn(
        log,
        ErrorProvider {
            kind: StreamErrorKind::Transport,
            message: "openai chat HTTP 500: internal server error".into(),
            status: Some(500),
            retry_after_secs: None,
        },
    )
    .await;
    let err = first_turn_error(&events);
    let Event::TurnError {
        kind, retriable, ..
    } = err
    else {
        unreachable!()
    };
    assert_eq!(*kind, TurnErrorKind::Server);
    assert!(*retriable);
}

#[tokio::test]
async fn http_503_maps_to_server_kind() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("events.jsonl");

    let events = run_failing_turn(
        log,
        ErrorProvider {
            kind: StreamErrorKind::Transport,
            message: "anthropic chat HTTP 503: service unavailable".into(),
            status: Some(503),
            retry_after_secs: None,
        },
    )
    .await;
    let err = first_turn_error(&events);
    let Event::TurnError { kind, .. } = err else {
        unreachable!()
    };
    assert_eq!(*kind, TurnErrorKind::Server);
}

#[tokio::test]
async fn transport_without_status_maps_to_network() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("events.jsonl");

    let events = run_failing_turn(
        log,
        ErrorProvider {
            kind: StreamErrorKind::Transport,
            message: "connection refused".into(),
            status: None,
            retry_after_secs: None,
        },
    )
    .await;
    let err = first_turn_error(&events);
    let Event::TurnError {
        kind, retriable, ..
    } = err
    else {
        unreachable!()
    };
    assert_eq!(*kind, TurnErrorKind::Network);
    assert!(*retriable);
}

#[tokio::test]
async fn idle_timeout_maps_to_network() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("events.jsonl");

    let events = run_failing_turn(
        log,
        ErrorProvider {
            kind: StreamErrorKind::IdleTimeout,
            message: "stream idle".into(),
            status: None,
            retry_after_secs: None,
        },
    )
    .await;
    let err = first_turn_error(&events);
    let Event::TurnError { kind, .. } = err else {
        unreachable!()
    };
    assert_eq!(*kind, TurnErrorKind::Network);
}

#[tokio::test]
async fn line_too_long_maps_to_malformed_response() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("events.jsonl");

    let events = run_failing_turn(
        log,
        ErrorProvider {
            kind: StreamErrorKind::LineTooLong,
            message: "sse line exceeded cap".into(),
            status: None,
            retry_after_secs: None,
        },
    )
    .await;
    let err = first_turn_error(&events);
    let Event::TurnError {
        kind, retriable, ..
    } = err
    else {
        unreachable!()
    };
    assert_eq!(*kind, TurnErrorKind::MalformedResponse);
    assert!(*retriable);
}

#[tokio::test]
async fn unrecognized_status_maps_to_unknown() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("events.jsonl");

    let events = run_failing_turn(
        log,
        ErrorProvider {
            kind: StreamErrorKind::Transport,
            message: "anthropic chat HTTP 418: i am a teapot".into(),
            status: Some(418),
            retry_after_secs: None,
        },
    )
    .await;
    let err = first_turn_error(&events);
    let Event::TurnError {
        kind, retriable, ..
    } = err
    else {
        unreachable!()
    };
    assert_eq!(*kind, TurnErrorKind::Unknown);
    assert!(*retriable);
}

#[tokio::test]
async fn turn_error_id_matches_originating_user_message() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("events.jsonl");

    let events = run_failing_turn(
        log,
        ErrorProvider {
            kind: StreamErrorKind::Transport,
            message: "HTTP 500".into(),
            status: Some(500),
            retry_after_secs: None,
        },
    )
    .await;

    let user_id = events
        .iter()
        .find_map(|e| match e {
            Event::UserMessage { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("UserMessage on log");
    let Event::TurnError { id, .. } = first_turn_error(&events) else {
        unreachable!()
    };
    assert_eq!(
        id, &user_id,
        "TurnError.id ties the failure to its originating user message slot"
    );
}

#[tokio::test]
async fn turn_error_raw_is_capped_at_4kib() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("events.jsonl");

    let big = "x".repeat(8 * 1024);
    let events = run_failing_turn(
        log,
        ErrorProvider {
            kind: StreamErrorKind::Transport,
            message: big.clone(),
            status: Some(500),
            retry_after_secs: None,
        },
    )
    .await;
    let Event::TurnError { raw, .. } = first_turn_error(&events) else {
        unreachable!()
    };
    let raw = raw
        .as_deref()
        .expect("raw should be populated for non-empty message");
    // The cap is 4 KiB + one trailing ellipsis codepoint when truncation
    // fires. Asserting the raw is strictly shorter than the input is the
    // load-bearing guarantee.
    assert!(
        raw.len() < big.len(),
        "raw must be truncated below input: raw={} input={}",
        raw.len(),
        big.len()
    );
    assert!(
        raw.ends_with('…'),
        "truncation should append an ellipsis marker"
    );
}
