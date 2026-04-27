//! F-139: golden JSON snapshot for fine-grained step-trace `Event` variants.
//!
//! Pins the wire shape of the step events the Agent Monitor (F-140) will
//! consume. Sibling-file conventions (see `event_wire_shape.rs`): plain
//! `serde_json::to_value` vs `json!({...})` equality — no `insta`, no
//! hidden snapshot files. When this test fails, the wire shape changed —
//! update the adapter / schema consumers at the same time.
//!
//! Determinism: all timestamps, ids, and digests below are fixed string
//! literals chosen to produce a stable JSON — no `Utc::now()`, no
//! `StepId::new()`.

use chrono::{DateTime, Utc};
use forge_core::{AgentInstanceId, Event, StepId, StepKind, StepOutcome, TokenUsage, ToolCallId};
use serde_json::{json, Value};

fn fixed_time() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-04-20T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn step_id(s: &str) -> StepId {
    serde_json::from_value(Value::String(s.to_string())).unwrap()
}

fn agent_instance_id(s: &str) -> AgentInstanceId {
    serde_json::from_value(Value::String(s.to_string())).unwrap()
}

fn tool_call_id(s: &str) -> ToolCallId {
    serde_json::from_value(Value::String(s.to_string())).unwrap()
}

fn assert_snapshot(event: Event, expected: Value) {
    let actual = serde_json::to_value(&event).expect("Event serializes");
    assert_eq!(
        actual, expected,
        "schema snapshot drifted — update adapter and consumer fixtures together"
    );
    // Round-trip: the serialized form must be accepted by the same enum.
    let roundtrip: Event = serde_json::from_value(actual).expect("Event round-trips");
    assert_eq!(
        roundtrip, event,
        "Event round-trip produced a different value"
    );
}

#[test]
fn step_started_model_top_level_turn_snapshot() {
    // Top-level session turn has no owning AgentInstance until F-140 —
    // `instance_id: None`. Serializes as `null` so the wire shape stays
    // uniform with populated instances.
    assert_snapshot(
        Event::StepStarted {
            step_id: step_id("step-1"),
            instance_id: None,
            kind: StepKind::Model,
            started_at: fixed_time(),
            index: 1,
            total: None,
        },
        json!({
            "type": "step_started",
            "step_id": "step-1",
            "instance_id": null,
            "kind": "model",
            "started_at": "2026-04-20T12:00:00Z",
            "index": 1,
        }),
    );
}

#[test]
fn step_started_tool_under_instance_snapshot() {
    assert_snapshot(
        Event::StepStarted {
            step_id: step_id("step-2"),
            instance_id: Some(agent_instance_id("inst-1")),
            kind: StepKind::Tool,
            started_at: fixed_time(),
            index: 2,
            total: None,
        },
        json!({
            "type": "step_started",
            "step_id": "step-2",
            "instance_id": "inst-1",
            "kind": "tool",
            "started_at": "2026-04-20T12:00:00Z",
            "index": 2,
        }),
    );
}

#[test]
fn step_kind_wire_labels_are_snake_case() {
    // Lock the wire form for every kind so adapter code can switch on
    // string literals without a central remap.
    for (kind, expected) in [
        (StepKind::Plan, "plan"),
        (StepKind::Tool, "tool"),
        (StepKind::Mcp, "mcp"),
        (StepKind::Model, "model"),
        (StepKind::Wait, "wait"),
        (StepKind::Spawn, "spawn"),
    ] {
        let ev = Event::StepStarted {
            step_id: step_id("sx"),
            instance_id: None,
            kind,
            started_at: fixed_time(),
            index: 1,
            total: None,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["kind"], Value::String(expected.to_string()));
    }
}

#[test]
fn step_finished_ok_no_usage_snapshot() {
    assert_snapshot(
        Event::StepFinished {
            step_id: step_id("step-1"),
            outcome: StepOutcome::Ok,
            duration_ms: 12,
            token_usage: None,
        },
        json!({
            "type": "step_finished",
            "step_id": "step-1",
            "outcome": { "status": "ok" },
            "duration_ms": 12,
            "token_usage": null,
        }),
    );
}

#[test]
fn step_finished_error_with_usage_snapshot() {
    assert_snapshot(
        Event::StepFinished {
            step_id: step_id("step-1"),
            outcome: StepOutcome::Error {
                reason: "tool failed".into(),
            },
            duration_ms: 45,
            token_usage: Some(TokenUsage {
                tokens_in: 100,
                tokens_out: 200,
            }),
        },
        json!({
            "type": "step_finished",
            "step_id": "step-1",
            "outcome": { "status": "error", "reason": "tool failed" },
            "duration_ms": 45,
            "token_usage": { "tokens_in": 100, "tokens_out": 200 },
        }),
    );
}

#[test]
fn tool_invoked_snapshot() {
    assert_snapshot(
        Event::ToolInvoked {
            step_id: step_id("step-2"),
            tool_call_id: tool_call_id("tc-1"),
            tool_id: "fs.read".into(),
            args_digest: "abcdef01".into(),
        },
        json!({
            "type": "tool_invoked",
            "step_id": "step-2",
            "tool_call_id": "tc-1",
            "tool_id": "fs.read",
            "args_digest": "abcdef01",
        }),
    );
}

#[test]
fn tool_returned_ok_snapshot() {
    assert_snapshot(
        Event::ToolReturned {
            step_id: step_id("step-2"),
            tool_call_id: tool_call_id("tc-1"),
            ok: true,
            bytes_out: 42,
        },
        json!({
            "type": "tool_returned",
            "step_id": "step-2",
            "tool_call_id": "tc-1",
            "ok": true,
            "bytes_out": 42,
        }),
    );
}

#[test]
fn tool_returned_error_snapshot() {
    assert_snapshot(
        Event::ToolReturned {
            step_id: step_id("step-3"),
            tool_call_id: tool_call_id("tc-err"),
            ok: false,
            bytes_out: 18,
        },
        json!({
            "type": "tool_returned",
            "step_id": "step-3",
            "tool_call_id": "tc-err",
            "ok": false,
            "bytes_out": 18,
        }),
    );
}
