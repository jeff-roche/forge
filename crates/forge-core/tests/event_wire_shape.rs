//! Golden wire-shape test for `forge_core::Event`.
//!
//! Pins the JSON serialization of each variant the TS adapter
//! (`web/packages/app/src/ipc/events.ts`) depends on. The same JSON literals
//! appear in `web/packages/app/src/ipc/events.test.ts` as adapter test inputs,
//! so if this file is the source-of-truth for the wire contract.
//!
//! When this test fails, the wire shape changed — update both sides together.
//! When you add a new `Event` variant, add a golden case here before writing
//! adapter code in TS. This satisfies the F-037 DoD requirement that adapter
//! tests be "seeded from real `forged` output (via bridge_e2e.rs or a
//! comparable harness)": the harness is this file — every shape the adapter
//! consumes round-trips through real `serde_json::to_value` on a real
//! `forge_core::Event`, not a hand-crafted object literal.
//!
//! Canonical field values are chosen to be deterministic (no timestamps
//! derived from `Utc::now()`, no random IDs). IDs deserialize from bare JSON
//! strings because `MessageId(String)` / `ToolCallId(String)` have private
//! fields; `serde_json::from_value` is the only stable constructor.
//!
//! F-362: the `variant_label` exhaustive match at the bottom of this file
//! guarantees every `Event` variant has at least one pin in this module —
//! adding a new variant without a pin is a compile error, not a runtime
//! regression that surfaces in the UI.

use chrono::{DateTime, Utc};
use forge_core::{
    AgentId, AgentInstanceId, ApprovalPreview, ApprovalScope, ApprovalSource, CompactTrigger,
    EndReason, Event, McpStateEvent, MessageId, ProviderId, RosterScope, ServerState,
    SessionPersistence, StepId, StepKind, StepOutcome, TokenUsage, ToolCallId,
};
use serde_json::{json, Value};

fn fixed_time() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-04-18T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn msg_id(s: &str) -> MessageId {
    serde_json::from_value(Value::String(s.to_string())).unwrap()
}

fn tool_call_id(s: &str) -> ToolCallId {
    serde_json::from_value(Value::String(s.to_string())).unwrap()
}

fn assert_wire_eq(event: Event, expected: Value) {
    let actual = serde_json::to_value(&event).expect("Event serializes");
    assert_eq!(
        actual, expected,
        "wire shape drifted — update TS adapter golden fixtures too"
    );
}

#[test]
fn user_message_wire_shape() {
    assert_wire_eq(
        Event::UserMessage {
            id: msg_id("mid-1"),
            at: fixed_time(),
            text: "hello".into(),
            context: vec![],
            branch_parent: None,
        },
        json!({
            "type": "user_message",
            "id": "mid-1",
            "at": "2026-04-18T10:00:00Z",
            "text": "hello",
            "context": [],
            "branch_parent": null,
        }),
    );
}

#[test]
fn assistant_delta_wire_shape() {
    assert_wire_eq(
        Event::AssistantDelta {
            id: msg_id("mid-3"),
            at: fixed_time(),
            delta: "partial ".into(),
        },
        json!({
            "type": "assistant_delta",
            "id": "mid-3",
            "at": "2026-04-18T10:00:00Z",
            "delta": "partial ",
        }),
    );
}

#[test]
fn assistant_message_finalised_wire_shape() {
    use forge_core::ProviderId;
    let provider: ProviderId = serde_json::from_value(Value::String("mock".into())).unwrap();
    assert_wire_eq(
        Event::AssistantMessage {
            id: msg_id("mid-2"),
            provider,
            model: "mock-1".into(),
            at: fixed_time(),
            stream_finalised: true,
            text: "hi there".into(),
            branch_parent: None,
            branch_variant_index: 0,
        },
        json!({
            "type": "assistant_message",
            "id": "mid-2",
            "provider": "mock",
            "model": "mock-1",
            "at": "2026-04-18T10:00:00Z",
            "stream_finalised": true,
            "text": "hi there",
            "branch_parent": null,
            "branch_variant_index": 0,
        }),
    );
}

#[test]
fn assistant_message_stream_open_wire_shape() {
    use forge_core::ProviderId;
    let provider: ProviderId = serde_json::from_value(Value::String("mock".into())).unwrap();
    // The orchestrator emits this at stream-start (text empty, stream_finalised:false).
    // The adapter MUST return null for it so the first AssistantDelta is what creates
    // the streaming turn.
    assert_wire_eq(
        Event::AssistantMessage {
            id: msg_id("mid-open"),
            provider,
            model: "mock-1".into(),
            at: fixed_time(),
            stream_finalised: false,
            // F-112: Arc<str> via From<&str>; wire-shape-equivalent to
            // the previous empty `String`.
            text: "".into(),
            branch_parent: None,
            branch_variant_index: 0,
        },
        json!({
            "type": "assistant_message",
            "id": "mid-open",
            "provider": "mock",
            "model": "mock-1",
            "at": "2026-04-18T10:00:00Z",
            "stream_finalised": false,
            "text": "",
            "branch_parent": null,
            "branch_variant_index": 0,
        }),
    );
}

#[test]
fn tool_call_started_wire_shape() {
    assert_wire_eq(
        Event::ToolCallStarted {
            id: tool_call_id("tc-1"),
            msg: msg_id("mid-3"),
            tool: "fs.read".into(),
            args: json!({ "path": "readable.txt" }),
            at: fixed_time(),
            parallel_group: None,
        },
        json!({
            "type": "tool_call_started",
            "id": "tc-1",
            "msg": "mid-3",
            "tool": "fs.read",
            "args": { "path": "readable.txt" },
            "at": "2026-04-18T10:00:00Z",
            "parallel_group": null,
        }),
    );
}

#[test]
fn tool_call_started_with_parallel_group_wire_shape() {
    assert_wire_eq(
        Event::ToolCallStarted {
            id: tool_call_id("tc-2"),
            msg: msg_id("mid-3"),
            tool: "fs.read".into(),
            args: json!({ "path": "a.txt" }),
            at: fixed_time(),
            parallel_group: Some(7),
        },
        json!({
            "type": "tool_call_started",
            "id": "tc-2",
            "msg": "mid-3",
            "tool": "fs.read",
            "args": { "path": "a.txt" },
            "at": "2026-04-18T10:00:00Z",
            "parallel_group": 7,
        }),
    );
}

#[test]
fn tool_call_approval_requested_wire_shape() {
    assert_wire_eq(
        Event::ToolCallApprovalRequested {
            id: tool_call_id("tc-9"),
            preview: ApprovalPreview {
                description: "Edit file /src/foo.ts".into(),
            },
        },
        json!({
            "type": "tool_call_approval_requested",
            "id": "tc-9",
            "preview": { "description": "Edit file /src/foo.ts" },
        }),
    );
}

#[test]
fn tool_call_rejected_wire_shape() {
    assert_wire_eq(
        Event::ToolCallRejected {
            id: tool_call_id("tc-rej-1"),
            reason: Some("user denied".into()),
        },
        json!({
            "type": "tool_call_rejected",
            "id": "tc-rej-1",
            "reason": "user denied",
        }),
    );
}

#[test]
fn tool_call_rejected_without_reason_wire_shape() {
    assert_wire_eq(
        Event::ToolCallRejected {
            id: tool_call_id("tc-rej-2"),
            reason: None,
        },
        json!({
            "type": "tool_call_rejected",
            "id": "tc-rej-2",
            "reason": null,
        }),
    );
}

#[test]
fn branch_selected_wire_shape() {
    // F-144 event consumed by the UI via `fromRustEvent` (F-145).
    assert_wire_eq(
        Event::BranchSelected {
            parent: msg_id("root-1"),
            selected: msg_id("variant-2"),
        },
        json!({
            "type": "branch_selected",
            "parent": "root-1",
            "selected": "variant-2",
        }),
    );
}

#[test]
fn branch_deleted_wire_shape() {
    // F-145 tombstone marker consumed by the UI via `fromRustEvent` and by
    // `apply_superseded` on replay.
    assert_wire_eq(
        Event::BranchDeleted {
            parent: msg_id("root-1"),
            variant_index: 2,
        },
        json!({
            "type": "branch_deleted",
            "parent": "root-1",
            "variant_index": 2,
        }),
    );
}

#[test]
fn tool_call_completed_wire_shape() {
    assert_wire_eq(
        Event::ToolCallCompleted {
            id: tool_call_id("tc-7"),
            result: json!({ "ok": true, "bytes": 42 }),
            duration_ms: 12,
            at: fixed_time(),
        },
        json!({
            "type": "tool_call_completed",
            "id": "tc-7",
            "result": { "ok": true, "bytes": 42 },
            "duration_ms": 12,
            "at": "2026-04-18T10:00:00Z",
        }),
    );
}

fn instance_id(s: &str) -> AgentInstanceId {
    serde_json::from_value(Value::String(s.to_string())).unwrap()
}

#[test]
fn resource_sample_wire_shape_all_fields_present() {
    // F-152: AgentMonitor pills fold on this event. Pinning the wire shape
    // here keeps the TS side (`applyEventToState` in
    // `web/packages/app/src/routes/AgentMonitor.tsx`) honest across refactors.
    assert_wire_eq(
        Event::ResourceSample {
            instance_id: instance_id("inst-1"),
            cpu_pct: Some(12.5),
            rss_bytes: Some(64 * 1024 * 1024),
            fd_count: Some(18),
            sampled_at: fixed_time(),
        },
        json!({
            "type": "resource_sample",
            "instance_id": "inst-1",
            "cpu_pct": 12.5,
            "rss_bytes": 64 * 1024 * 1024,
            "fd_count": 18,
            "sampled_at": "2026-04-18T10:00:00Z",
        }),
    );
}

#[test]
fn resource_sample_wire_shape_missing_fields_are_null() {
    // A best-effort platform probe can leave any subset of fields unpopulated
    // (notably `fd_count` on macOS via libproc). The wire shape must preserve
    // `null` for those — the UI renders `null` as the `—` placeholder and must
    // not see a field disappear from the JSON object between emissions.
    assert_wire_eq(
        Event::ResourceSample {
            instance_id: instance_id("inst-partial"),
            cpu_pct: Some(5.0),
            rss_bytes: None,
            fd_count: None,
            sampled_at: fixed_time(),
        },
        json!({
            "type": "resource_sample",
            "instance_id": "inst-partial",
            "cpu_pct": 5.0,
            "rss_bytes": null,
            "fd_count": null,
            "sampled_at": "2026-04-18T10:00:00Z",
        }),
    );
}

fn agent_id(s: &str) -> AgentId {
    serde_json::from_value(Value::String(s.to_string())).unwrap()
}

fn provider_id(s: &str) -> ProviderId {
    serde_json::from_value(Value::String(s.to_string())).unwrap()
}

fn step_id(s: &str) -> StepId {
    serde_json::from_value(Value::String(s.to_string())).unwrap()
}

// ---------------------------------------------------------------------------
// F-362: wire-shape pins for every remaining production-emitted `Event`
// variant. The Phase-2 landing sweep (F-137, F-138, F-139, F-143, F-152,
// F-155) added 14 variants without goldens — ts-rs doesn't catch serde
// discriminator drift, so the TS adapter silently tolerates null fields
// until a runtime regression surfaces in the UI. Each test below pins one
// variant; the `event_variant_wire_shape_coverage_is_exhaustive` guard at
// the bottom of this file forces any *future* variant to add its pin here
// at compile time.
// ---------------------------------------------------------------------------

#[test]
fn session_started_wire_shape() {
    assert_wire_eq(
        Event::SessionStarted {
            at: fixed_time(),
            workspace: "/work".into(),
            agent: Some(agent_id("agent-1")),
            persistence: SessionPersistence::Persist,
        },
        json!({
            "type": "session_started",
            "at": "2026-04-18T10:00:00Z",
            "workspace": "/work",
            "agent": "agent-1",
            "persistence": "Persist",
        }),
    );
}

#[test]
fn session_started_without_agent_wire_shape() {
    // Workspace-level sessions (no bound agent) must keep `agent: null`
    // on the wire rather than dropping the key — the adapter checks
    // `event.agent === null` to branch to the "no agent" code path.
    assert_wire_eq(
        Event::SessionStarted {
            at: fixed_time(),
            workspace: "/work".into(),
            agent: None,
            persistence: SessionPersistence::Ephemeral,
        },
        json!({
            "type": "session_started",
            "at": "2026-04-18T10:00:00Z",
            "workspace": "/work",
            "agent": null,
            "persistence": "Ephemeral",
        }),
    );
}

#[test]
fn message_superseded_wire_shape() {
    // F-143: marker consumed by `apply_superseded` on replay and by the
    // transcript view to hide the superseded turn.
    assert_wire_eq(
        Event::MessageSuperseded {
            old_id: msg_id("old-1"),
            new_id: msg_id("new-1"),
        },
        json!({
            "type": "message_superseded",
            "old_id": "old-1",
            "new_id": "new-1",
        }),
    );
}

#[test]
fn tool_call_approved_wire_shape() {
    // F-138: approval-flow milestone — the adapter folds this into the
    // approval store to unlock the pending tool call.
    assert_wire_eq(
        Event::ToolCallApproved {
            id: tool_call_id("tc-ap-1"),
            by: ApprovalSource::User,
            scope: ApprovalScope::Once,
            at: fixed_time(),
        },
        json!({
            "type": "tool_call_approved",
            "id": "tc-ap-1",
            "by": "User",
            "scope": "Once",
            "at": "2026-04-18T10:00:00Z",
        }),
    );
}

#[test]
fn tool_call_approved_auto_wire_shape() {
    // Auto-approval path (remembered scope) — the UI renders a distinct
    // badge for `by: "Auto"` so the scope variant and source variant both
    // need to survive the wire.
    assert_wire_eq(
        Event::ToolCallApproved {
            id: tool_call_id("tc-ap-2"),
            by: ApprovalSource::Auto,
            scope: ApprovalScope::ThisTool,
            at: fixed_time(),
        },
        json!({
            "type": "tool_call_approved",
            "id": "tc-ap-2",
            "by": "Auto",
            "scope": "ThisTool",
            "at": "2026-04-18T10:00:00Z",
        }),
    );
}

#[test]
fn sub_agent_spawned_wire_shape() {
    // F-137: sub-agent spawn edge drawn in the AgentMonitor graph.
    // F-448 Phase 3: adds optional `model` + `tool_count` — absent here so
    // the wire shape is byte-identical to the Phase-2 emission. A separate
    // assertion below pins the `Some(_)` path.
    assert_wire_eq(
        Event::SubAgentSpawned {
            parent: instance_id("inst-parent"),
            child: instance_id("inst-child"),
            from_msg: msg_id("mid-spawn"),
            model: None,
            tool_count: None,
        },
        json!({
            "type": "sub_agent_spawned",
            "parent": "inst-parent",
            "child": "inst-child",
            "from_msg": "mid-spawn",
        }),
    );
}

#[test]
fn sub_agent_spawned_wire_shape_carries_model_and_tool_count() {
    // F-448 Phase 3: when the orchestrator knows the child's model / tool
    // surface at spawn time, the two optional header-chip fields serialize
    // on the wire. The Phase-2 triple (parent/child/from_msg) is unchanged.
    assert_wire_eq(
        Event::SubAgentSpawned {
            parent: instance_id("inst-parent"),
            child: instance_id("inst-child"),
            from_msg: msg_id("mid-spawn"),
            model: Some("sonnet-4.5".to_string()),
            tool_count: Some(4),
        },
        json!({
            "type": "sub_agent_spawned",
            "parent": "inst-parent",
            "child": "inst-child",
            "from_msg": "mid-spawn",
            "model": "sonnet-4.5",
            "tool_count": 4,
        }),
    );
}

#[test]
fn background_agent_started_wire_shape() {
    // F-137: background-agent lifecycle — paired with
    // `background_agent_completed`. AgentMonitor renders a running row.
    assert_wire_eq(
        Event::BackgroundAgentStarted {
            id: instance_id("bg-1"),
            agent: agent_id("researcher"),
            at: fixed_time(),
        },
        json!({
            "type": "background_agent_started",
            "id": "bg-1",
            "agent": "researcher",
            "at": "2026-04-18T10:00:00Z",
        }),
    );
}

#[test]
fn background_agent_completed_wire_shape() {
    assert_wire_eq(
        Event::BackgroundAgentCompleted {
            id: instance_id("bg-1"),
            at: fixed_time(),
        },
        json!({
            "type": "background_agent_completed",
            "id": "bg-1",
            "at": "2026-04-18T10:00:00Z",
        }),
    );
}

#[test]
fn usage_tick_wire_shape() {
    // F-155: per-provider token / cost accounting — feeds the usage HUD.
    assert_wire_eq(
        Event::UsageTick {
            provider: provider_id("mock"),
            model: "mock-1".into(),
            tokens_in: 128,
            tokens_out: 256,
            cost_usd: 0.0125,
            scope: RosterScope::SessionWide,
        },
        json!({
            "type": "usage_tick",
            "provider": "mock",
            "model": "mock-1",
            "tokens_in": 128,
            "tokens_out": 256,
            "cost_usd": 0.0125,
            "scope": { "type": "SessionWide" },
        }),
    );
}

#[test]
fn context_compacted_wire_shape() {
    // F-155: auto-compaction event — the transcript view collapses
    // summarized turns under a "compacted N turns" pill anchored on
    // `summary_msg_id`.
    assert_wire_eq(
        Event::ContextCompacted {
            at: fixed_time(),
            summarized_turns: 4,
            summary_msg_id: msg_id("summary-1"),
            trigger: CompactTrigger::AutoAt98Pct,
        },
        json!({
            "type": "context_compacted",
            "at": "2026-04-18T10:00:00Z",
            "summarized_turns": 4,
            "summary_msg_id": "summary-1",
            "trigger": "AutoAt98Pct",
        }),
    );
}

#[test]
fn session_ended_wire_shape() {
    // Completed session — the dashboard shows "ended cleanly".
    assert_wire_eq(
        Event::SessionEnded {
            at: fixed_time(),
            reason: EndReason::Completed,
            archived: true,
        },
        json!({
            "type": "session_ended",
            "at": "2026-04-18T10:00:00Z",
            "reason": "Completed",
            "archived": true,
        }),
    );
}

#[test]
fn session_ended_with_error_wire_shape() {
    // `Error(String)` is the only data-carrying arm of `EndReason`. Serde's
    // default external tagging wraps it as `{ "Error": "<reason>" }` — the
    // adapter matches on that literal key. Locking both arms' shapes.
    assert_wire_eq(
        Event::SessionEnded {
            at: fixed_time(),
            reason: EndReason::Error("provider dropped".into()),
            archived: false,
        },
        json!({
            "type": "session_ended",
            "at": "2026-04-18T10:00:00Z",
            "reason": { "Error": "provider dropped" },
            "archived": false,
        }),
    );
}

#[test]
fn step_started_wire_shape() {
    // F-139: step-trace open. `instance_id: None` is the top-level-turn
    // case (F-140 populates it once `AgentMonitor` is wired through
    // `run_turn`) — locking the `null` shape prevents the field from
    // disappearing between emissions.
    assert_wire_eq(
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
            "started_at": "2026-04-18T10:00:00Z",
            "index": 1,
        }),
    );
}

#[test]
fn step_started_with_instance_id_wire_shape() {
    assert_wire_eq(
        Event::StepStarted {
            step_id: step_id("step-2"),
            instance_id: Some(instance_id("inst-9")),
            kind: StepKind::Tool,
            started_at: fixed_time(),
            index: 2,
            total: None,
        },
        json!({
            "type": "step_started",
            "step_id": "step-2",
            "instance_id": "inst-9",
            "kind": "tool",
            "started_at": "2026-04-18T10:00:00Z",
            "index": 2,
        }),
    );
}

#[test]
fn step_started_with_total_wire_shape() {
    // F-606: when the orchestrator can populate `total`, it serializes
    // alongside `index`. `total: None` is omitted via
    // `#[serde(skip_serializing_if)]` so older clients never see a `null`
    // they didn't previously read.
    assert_wire_eq(
        Event::StepStarted {
            step_id: step_id("step-3"),
            instance_id: None,
            kind: StepKind::Model,
            started_at: fixed_time(),
            index: 3,
            total: Some(7),
        },
        json!({
            "type": "step_started",
            "step_id": "step-3",
            "instance_id": null,
            "kind": "model",
            "started_at": "2026-04-18T10:00:00Z",
            "index": 3,
            "total": 7,
        }),
    );
}

#[test]
fn step_started_legacy_log_replay_defaults_index_zero() {
    // F-606 backwards-compat: event logs written before the index/total
    // fields existed must still deserialize. Missing fields fall back to
    // `index: 0` (the "legacy / unknown" sentinel) and `total: None`.
    let legacy: Event = serde_json::from_value(json!({
        "type": "step_started",
        "step_id": "step-legacy",
        "instance_id": null,
        "kind": "model",
        "started_at": "2026-04-18T10:00:00Z",
    }))
    .expect("legacy step_started shape must round-trip via serde defaults");
    match legacy {
        Event::StepStarted { index, total, .. } => {
            assert_eq!(index, 0, "legacy logs deserialize with index: 0 sentinel");
            assert!(total.is_none(), "legacy logs deserialize with total: None");
        }
        other => panic!("expected StepStarted, got {other:?}"),
    }
}

#[test]
fn step_finished_ok_wire_shape() {
    // F-139: step-trace close, ok arm. `token_usage: None` is the "provider
    // didn't report usage" case — today's common path for the mock
    // provider. Locking the `null` wire shape.
    assert_wire_eq(
        Event::StepFinished {
            step_id: step_id("step-1"),
            outcome: StepOutcome::Ok,
            duration_ms: 42,
            token_usage: None,
        },
        json!({
            "type": "step_finished",
            "step_id": "step-1",
            "outcome": { "status": "ok" },
            "duration_ms": 42,
            "token_usage": null,
        }),
    );
}

#[test]
fn step_finished_error_with_token_usage_wire_shape() {
    // F-139: step-trace close, error arm with provider-reported usage.
    // `StepOutcome::Error` is internally tagged (`status`) — locking the
    // shape here guards against a rename of the tag or the field.
    assert_wire_eq(
        Event::StepFinished {
            step_id: step_id("step-3"),
            outcome: StepOutcome::Error {
                reason: "provider dropped".into(),
            },
            duration_ms: 17,
            token_usage: Some(TokenUsage {
                tokens_in: 10,
                tokens_out: 20,
            }),
        },
        json!({
            "type": "step_finished",
            "step_id": "step-3",
            "outcome": { "status": "error", "reason": "provider dropped" },
            "duration_ms": 17,
            "token_usage": { "tokens_in": 10, "tokens_out": 20 },
        }),
    );
}

#[test]
fn tool_invoked_wire_shape() {
    // F-139: tool-step boundary, approval → invoke. `args_digest` is a
    // short SHA-256 hex prefix computed upstream — pin asserts it rides as
    // a plain string.
    assert_wire_eq(
        Event::ToolInvoked {
            step_id: step_id("step-4"),
            tool_call_id: tool_call_id("tc-inv"),
            tool_id: "fs.read".into(),
            args_digest: "abc12345".into(),
        },
        json!({
            "type": "tool_invoked",
            "step_id": "step-4",
            "tool_call_id": "tc-inv",
            "tool_id": "fs.read",
            "args_digest": "abc12345",
        }),
    );
}

#[test]
fn tool_returned_wire_shape() {
    // F-139: tool-step boundary, invoke → complete. `bytes_out` rides as a
    // JSON number (u64), `ok` as a JSON bool.
    assert_wire_eq(
        Event::ToolReturned {
            step_id: step_id("step-4"),
            tool_call_id: tool_call_id("tc-inv"),
            ok: true,
            bytes_out: 1024,
        },
        json!({
            "type": "tool_returned",
            "step_id": "step-4",
            "tool_call_id": "tc-inv",
            "ok": true,
            "bytes_out": 1024,
        }),
    );
}

#[test]
fn mcp_state_wire_shape_healthy() {
    // F-155: `McpState(McpStateEvent)` is a newtype variant carrying a
    // struct. Serde's internal tagging flattens the struct fields into the
    // outer object, so the wire shape is the outer `type` discriminator
    // plus every field of `McpStateEvent`.
    //
    // F-380: `at` is a `DateTime<Utc>` (was `ts: SystemTime`) — serializes
    // as an RFC3339 string matching every other event timestamp. The inner
    // `ServerState` tag is `"type"` (was `"state"`).
    assert_wire_eq(
        Event::McpState(McpStateEvent {
            server: "ripgrep".into(),
            state: ServerState::Healthy,
            at: fixed_time(),
        }),
        json!({
            "type": "mcp_state",
            "server": "ripgrep",
            "state": { "type": "healthy" },
            "at": "2026-04-18T10:00:00Z",
        }),
    );
}

#[test]
fn mcp_state_wire_shape_degraded_with_reason() {
    // `ServerState` is internally tagged on `type` (F-380) — data-carrying
    // arms (Degraded/Failed/Disabled) hang their `reason` alongside. Pin
    // both the healthy (no-reason) and degraded (with-reason) shapes so a
    // rename on either side surfaces.
    assert_wire_eq(
        Event::McpState(McpStateEvent {
            server: "shell".into(),
            state: ServerState::Degraded {
                reason: "health check timeout".into(),
            },
            at: fixed_time(),
        }),
        json!({
            "type": "mcp_state",
            "server": "shell",
            "state": { "type": "degraded", "reason": "health check timeout" },
            "at": "2026-04-18T10:00:00Z",
        }),
    );
}

#[test]
fn provider_changed_wire_shape() {
    // F-586: dashboard-driven active-provider switch event. The wire shape
    // is the snake_case discriminator plus a single `provider_id` string —
    // matching the slug the dashboard selected and the key
    // `Credentials::has_credential` accepts.
    assert_wire_eq(
        Event::ProviderChanged {
            provider_id: "anthropic".into(),
        },
        json!({
            "type": "provider_changed",
            "provider_id": "anthropic",
        }),
    );
}

#[test]
fn session_paused_wire_shape() {
    // F-603: orchestrator entered the paused state. The wire shape is the
    // snake_case discriminator plus the canonical `at` timestamp — no
    // other fields (the daemon resolves the session from the
    // connection, not from the event).
    assert_wire_eq(
        Event::SessionPaused { at: fixed_time() },
        json!({
            "type": "session_paused",
            "at": "2026-04-18T10:00:00Z",
        }),
    );
}

#[test]
fn session_resumed_wire_shape() {
    // F-603: orchestrator left the paused state. Mirror of the
    // `SessionPaused` shape.
    assert_wire_eq(
        Event::SessionResumed { at: fixed_time() },
        json!({
            "type": "session_resumed",
            "at": "2026-04-18T10:00:00Z",
        }),
    );
}

// ---------------------------------------------------------------------------
// Compile-time guard: adding a new `Event` variant must also add its
// wire-shape pin above. This is enforced by an exhaustive `match` here —
// `#[deny(non_exhaustive_omitted_patterns)]` is unstable, so we rely on the
// default "non-exhaustive patterns" compile error emitted by `match` when
// the enum grows. The match arms return a stable string label that maps to
// a test name above; any future variant has no arm, the test file fails to
// compile, and the new variant cannot land without a matching pin.
//
// If you hit `non-exhaustive patterns: &Event::XxxYyy not covered`: add a
// `#[test] fn xxx_yyy_wire_shape()` above and a matching arm below.
// ---------------------------------------------------------------------------

fn variant_label(e: &Event) -> &'static str {
    match e {
        Event::SessionStarted { .. } => "session_started",
        Event::UserMessage { .. } => "user_message",
        Event::AssistantMessage { .. } => "assistant_message",
        Event::AssistantDelta { .. } => "assistant_delta",
        Event::BranchSelected { .. } => "branch_selected",
        Event::BranchDeleted { .. } => "branch_deleted",
        Event::MessageSuperseded { .. } => "message_superseded",
        Event::ToolCallStarted { .. } => "tool_call_started",
        Event::ToolCallApprovalRequested { .. } => "tool_call_approval_requested",
        Event::ToolCallApproved { .. } => "tool_call_approved",
        Event::ToolCallRejected { .. } => "tool_call_rejected",
        Event::ToolCallCompleted { .. } => "tool_call_completed",
        Event::SubAgentSpawned { .. } => "sub_agent_spawned",
        Event::BackgroundAgentStarted { .. } => "background_agent_started",
        Event::BackgroundAgentCompleted { .. } => "background_agent_completed",
        Event::UsageTick { .. } => "usage_tick",
        Event::ContextCompacted { .. } => "context_compacted",
        Event::SessionEnded { .. } => "session_ended",
        Event::StepStarted { .. } => "step_started",
        Event::StepFinished { .. } => "step_finished",
        Event::ToolInvoked { .. } => "tool_invoked",
        Event::ToolReturned { .. } => "tool_returned",
        Event::McpState(_) => "mcp_state",
        Event::ResourceSample { .. } => "resource_sample",
        Event::ProviderChanged { .. } => "provider_changed",
        Event::SessionPaused { .. } => "session_paused",
        Event::SessionResumed { .. } => "session_resumed",
    }
}

#[test]
fn event_variant_wire_shape_coverage_is_exhaustive() {
    // Call `variant_label` on one constructed event so the function is
    // monomorphized and the exhaustive `match` cannot be dead-code-dropped
    // by the compiler. The compile-time guarantee is the non-exhaustive
    // pattern error on the `match` itself — reached at `cargo check`
    // before this test body ever runs.
    let sample = Event::BranchSelected {
        parent: msg_id("root"),
        selected: msg_id("variant"),
    };
    assert_eq!(variant_label(&sample), "branch_selected");
}
