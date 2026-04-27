//! F-380: Event wire-shape conventions.
//!
//! Pins the cross-cutting rules documented in
//! `docs/architecture/event-conventions.md`:
//!
//! 1. Default timestamp field name is `at`. New event variants MUST use
//!    `at: DateTime<Utc>`. Two pinned exceptions remain — `StepStarted.started_at`
//!    and `ResourceSample.sampled_at` — because the AgentMonitor webview
//!    pins on those specialized names; see the conventions doc.
//! 2. Internally-tagged enums use `tag = "type"` — `Event` and
//!    `ServerState` share one discriminator name. `StepOutcome` keeps
//!    `tag = "status"` as a pinned exception (AgentMonitor webview reads
//!    the field directly).
//! 3. `McpStateEvent.at` rides as RFC3339 `DateTime<Utc>`, not the
//!    pre-F-380 `SystemTime` `{secs, nanos}` pair.
//!
//! If any assertion here fails, update every TS adapter and generated
//! binding at the same time.

use chrono::{DateTime, Utc};
use forge_core::{
    AgentInstanceId, Event, McpStateEvent, ServerState, StepId, StepKind, StepOutcome,
};
use serde_json::{json, Value};

fn fixed_time() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-04-22T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn step_id(s: &str) -> StepId {
    serde_json::from_value(Value::String(s.to_string())).unwrap()
}

fn instance_id(s: &str) -> AgentInstanceId {
    serde_json::from_value(Value::String(s.to_string())).unwrap()
}

#[test]
fn step_started_retains_started_at_pinned_exception() {
    // Pinned exception to rule #1 — documented in
    // `docs/architecture/event-conventions.md`. The field stays `started_at`
    // so the AgentMonitor webview doesn't churn; new event variants MUST
    // still default to `at`.
    let ev = Event::StepStarted {
        step_id: step_id("s1"),
        instance_id: None,
        kind: StepKind::Model,
        started_at: fixed_time(),
        index: 1,
        total: None,
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(
        v["started_at"],
        Value::String("2026-04-22T00:00:00Z".into()),
        "StepStarted.started_at must survive as the pinned exception, got: {v}"
    );
    assert!(
        v.get("at").is_none(),
        "StepStarted must not emit `at` alongside `started_at` — pick one; got: {v}"
    );
}

#[test]
fn resource_sample_retains_sampled_at_pinned_exception() {
    // Pinned exception to rule #1 — see the conventions doc.
    let ev = Event::ResourceSample {
        instance_id: instance_id("inst-1"),
        cpu_pct: None,
        rss_bytes: None,
        fd_count: None,
        sampled_at: fixed_time(),
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(
        v["sampled_at"],
        Value::String("2026-04-22T00:00:00Z".into()),
        "ResourceSample.sampled_at must survive as the pinned exception, got: {v}"
    );
    assert!(
        v.get("at").is_none(),
        "ResourceSample must not emit `at` alongside `sampled_at`; got: {v}"
    );
}

#[test]
fn server_state_uses_type_discriminator() {
    // ServerState is an internally-tagged enum; the tag name must be `type`
    // to align with Event / StepOutcome.
    let healthy = serde_json::to_value(ServerState::Healthy).unwrap();
    assert_eq!(
        healthy,
        json!({ "type": "healthy" }),
        "ServerState tag must be `type`, got: {healthy}"
    );
    let degraded = serde_json::to_value(ServerState::Degraded {
        reason: "slow".into(),
    })
    .unwrap();
    assert_eq!(
        degraded,
        json!({ "type": "degraded", "reason": "slow" }),
        "ServerState tag must be `type` on data-carrying arms, got: {degraded}"
    );
}

#[test]
fn step_outcome_retains_status_pinned_exception() {
    // Pinned exception to rule #2: `StepOutcome` keeps `tag = "status"`
    // because the AgentMonitor webview's `outcomeOf` helper in
    // `web/packages/app/src/routes/AgentMonitor.tsx` reads `.status`
    // directly. Documented in `docs/architecture/event-conventions.md`.
    let ok = serde_json::to_value(StepOutcome::Ok).unwrap();
    assert_eq!(
        ok,
        json!({ "status": "ok" }),
        "StepOutcome.status must survive as the pinned exception, got: {ok}"
    );
    let err = serde_json::to_value(StepOutcome::Error {
        reason: "boom".into(),
    })
    .unwrap();
    assert_eq!(
        err,
        json!({ "status": "error", "reason": "boom" }),
        "StepOutcome.status must survive on data-carrying arms, got: {err}"
    );
}

#[test]
fn mcp_state_event_at_rides_as_rfc3339_datetime() {
    // McpStateEvent.at is a DateTime<Utc>, not SystemTime. Wire shape must
    // be a plain RFC3339 string, not a `{secs_since_epoch, nanos_since_epoch}`
    // object.
    let ev = McpStateEvent {
        server: "ripgrep".into(),
        state: ServerState::Healthy,
        at: fixed_time(),
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(
        v["at"],
        Value::String("2026-04-22T00:00:00Z".into()),
        "McpStateEvent.at must be an RFC3339 string, got: {v}"
    );
    assert!(
        v.get("ts").is_none(),
        "McpStateEvent must not emit the legacy `ts` key; got: {v}"
    );
    // And the ServerState inside rides as `{ "type": "healthy" }` now.
    assert_eq!(v["state"], json!({ "type": "healthy" }));
}

#[test]
fn event_mcp_state_wire_shape_matches_convention() {
    // Full end-to-end wire pin for the flattened `Event::McpState` envelope:
    // outer `type` discriminator, inner `state.type` discriminator, timestamp
    // field named `at`.
    let ev = Event::McpState(McpStateEvent {
        server: "ripgrep".into(),
        state: ServerState::Degraded {
            reason: "slow".into(),
        },
        at: fixed_time(),
    });
    assert_eq!(
        serde_json::to_value(&ev).unwrap(),
        json!({
            "type": "mcp_state",
            "server": "ripgrep",
            "state": { "type": "degraded", "reason": "slow" },
            "at": "2026-04-22T00:00:00Z",
        })
    );
}
