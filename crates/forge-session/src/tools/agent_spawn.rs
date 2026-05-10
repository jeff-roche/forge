//! `agent.spawn` tool (F-134): a parent agent spawns a sub-agent by name.
//!
//! The dispatcher resolves `agent_name` against the agent defs loaded for the
//! session, hands the **child's** `AgentDef` to `forge_agents::Orchestrator::spawn`
//! (so the child runs under its own `isolation`, not the parent's), then emits
//! `Event::SubAgentSpawned` against the session's event log.
//!
//! Isolation invariant: the orchestrator is given `child_def` verbatim — the
//! tool never synthesises isolation from the parent. A parent `Process` agent
//! that spawns a `Container` child must end up with the child registered at
//! `Isolation::Container`. See the integration test
//! `agent_spawn_child_runs_under_child_isolation_not_parent` which exercises
//! this under the user scope.
//!
//! Prompt forwarding (F-137 mandate): the `prompt` arg is threaded onto
//! `SpawnContext::with_prompt` so the child's registered `AgentInstance.initial_prompt`
//! carries the parent's seed user message verbatim. Before F-137 the arg was
//! validated but dropped (`let _prompt = …`); that made F-134 scaffolding-
//! only. The follow-up that wires an actual step-executor reads the stored
//! prompt to materialise the child's first user turn.
//!
//! The event wire shape matches `forge_core::Event::SubAgentSpawned
//! { parent, child, from_msg, model?, tool_count? }`. The optional
//! `model`/`tool_count` chip fields were added in F-448 Phase 3 with
//! `skip_serializing_if = "Option::is_none"` so absent values roundtrip as
//! the Phase-2 wire shape. The tool emits `None` for both today — `AgentDef`
//! carries no model field and the dispatcher's tool surface is session-
//! scoped rather than per-child; the pipe is wired so a follow-up can
//! populate them without a wire change.

use std::sync::Arc;

use super::{get_required_str, Tool, ToolCtx};
use forge_agents::{validate_agent_name, AgentDef, AgentScope, SpawnContext};
use forge_core::{ApprovalPreview, Event};

pub struct AgentSpawnTool;

impl AgentSpawnTool {
    pub const NAME: &'static str = "agent.spawn";
}

#[async_trait::async_trait]
impl Tool for AgentSpawnTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn approval_preview(&self, args: &serde_json::Value) -> ApprovalPreview {
        let name = super::get_optional_str(args, "agent_name").unwrap_or("");
        ApprovalPreview {
            description: format!("Spawn sub-agent '{name}'"),
        }
    }

    async fn invoke(&self, args: &serde_json::Value, ctx: &ToolCtx) -> serde_json::Value {
        let agent_name = match get_required_str(args, Self::NAME, "agent_name") {
            Ok(s) => s.to_owned(),
            Err(e) => return serde_json::json!({ "error": e.to_string() }),
        };
        // Length cap + charset gate. `forge_agents::validate_agent_name` is
        // the canonical rule used at parse time for `.agents/*.md` stems —
        // reusing it here keeps the IPC entry point and the on-disk loader
        // in lockstep (64-byte cap, ASCII alphanumeric + `_`/`-`, no path
        // separators, no leading `.`, no whitespace). A name that the
        // loader would reject must also be rejected at spawn time so a
        // forged provider call cannot drive a hostile resolve against
        // `resolve_def`.
        if let Err(e) = validate_agent_name(&agent_name) {
            return serde_json::json!({
                "error": format!("tool.{}: invalid agent_name: {}", Self::NAME, e),
            });
        }
        // F-137 follow-up to F-134: forward `prompt` through `SpawnContext`
        // onto the registered child instance so a step-executor can seed the
        // child's first user turn with the parent's input. Pre-F-137 the arg
        // was validated but dropped, which turned F-134 into scaffolding-only
        // plumbing. Wrapped as `Arc<str>` at the boundary — matches the
        // hot-path contract on `forge_core::Event::UserMessage.text` so the
        // downstream executor can reuse the same allocation when materialising
        // the first turn.
        let prompt: Arc<str> = match get_required_str(args, Self::NAME, "prompt") {
            Ok(s) => Arc::from(s),
            Err(e) => return serde_json::json!({ "error": e.to_string() }),
        };
        // `context` is the third DoD argument (`AgentContext`). It is
        // accepted as an opaque JSON envelope for now — the concrete type
        // lands in the follow-up that introduces `AgentContext` in
        // `forge-agents`. Tolerating any shape keeps F-134 forward-
        // compatible without committing to a type the runtime cannot yet
        // interpret. If present, it must at least be an object or null so
        // obviously-malformed calls (`"context": 42`) fail loudly rather
        // than silently.
        if let Some(v) = args.get("context") {
            if !(v.is_object() || v.is_null()) {
                return serde_json::json!({
                    "error": format!(
                        "tool.{}: 'context' must be a JSON object or null, got {}",
                        Self::NAME,
                        type_of(v),
                    )
                });
            }
        }

        let Some(agent_ctx) = ctx.agent_ctx.as_ref() else {
            return serde_json::json!({
                "error": format!(
                    "tool.{}: agent runtime not configured for this session",
                    Self::NAME,
                )
            });
        };

        let child_def = match resolve_def(&agent_ctx.agent_defs, &agent_name) {
            Some(def) => def.clone(),
            None => {
                return serde_json::json!({
                    "error": format!(
                        "tool.{}: unknown agent '{}'",
                        Self::NAME,
                        agent_name,
                    )
                });
            }
        };

        // Critical invariant: hand the CHILD's def to the orchestrator.
        // Never synthesise isolation from the parent — that would be a
        // privilege-escalation regression. User scope is enforced here so
        // a sub-agent cannot request `Trusted` via this tool even if
        // someone later allows it in `AgentDef`. `with_prompt` forwards the
        // parent-supplied seed to the child's registered instance (F-137
        // additional mandate).
        let spawn_ctx = SpawnContext {
            scope: AgentScope::User,
            initial_prompt: Some(prompt),
        };

        let instance = match agent_ctx.orchestrator.spawn(child_def, spawn_ctx).await {
            Ok(inst) => inst,
            Err(e) => {
                return serde_json::json!({
                    "error": format!("tool.{}: spawn failed: {}", Self::NAME, e),
                });
            }
        };

        // Emit the session-level event so replay / UI banners pick up the
        // spawn. F-448 Phase 3 added optional `model` + `tool_count` to the
        // variant; see module docs for why both are `None` today.
        if let Err(e) = agent_ctx
            .session
            .emit(Event::SubAgentSpawned {
                parent: agent_ctx.parent_instance_id.clone(),
                child: instance.id.clone(),
                from_msg: agent_ctx.current_msg_id.clone(),
                // F-448 Phase 3: `model` and `tool_count` are wired through
                // the event variant but `AgentDef` does not carry a model
                // today and the dispatcher's tool surface is session-wide,
                // not per-child. Emit `None` until a follow-up teaches
                // `AgentDef` a `model` field and scopes the tool registry
                // per instance. The webview hides absent chips cleanly.
                model: None,
                tool_count: None,
            })
            .await
        {
            // The orchestrator already registered the child and emitted a
            // lifecycle event on its own broadcast stream; the session
            // event emit is a best-effort durability signal. Surface the
            // failure in the tool result so the provider sees it, but do
            // not unregister the child — that would leave the orchestrator
            // state inconsistent with its already-emitted `Spawned`.
            return serde_json::json!({
                "error": format!(
                    "tool.{}: spawned {} but SubAgentSpawned emit failed: {}",
                    Self::NAME,
                    instance.id,
                    e,
                ),
                "child_instance_id": instance.id.to_string(),
            });
        }

        serde_json::json!({
            "ok": true,
            "child_instance_id": instance.id.to_string(),
            "agent_name": agent_name,
        })
    }
}

fn resolve_def<'a>(defs: &'a [AgentDef], name: &str) -> Option<&'a AgentDef> {
    defs.iter().find(|d| d.name == name)
}

fn type_of(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;
    use crate::tools::{AgentSpawnCtx, ToolCtx, ToolDispatcher};
    use forge_agents::{AgentDef, Isolation, Orchestrator as AgentOrchestrator};
    use forge_core::ids::AgentInstanceId;
    use forge_core::MessageId;
    use serde_json::json;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn def(name: &str, isolation: Isolation) -> AgentDef {
        AgentDef {
            name: name.to_string(),
            description: None,
            body: String::new(),
            allowed_paths: vec![],
            isolation,
            memory_enabled: false,
        }
    }

    async fn session_fixture() -> (TempDir, Arc<Session>) {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());
        (dir, session)
    }

    fn ctx_with(agent_ctx: AgentSpawnCtx) -> ToolCtx {
        ToolCtx {
            allowed_paths: vec![],
            workspace_root: None,
            child_registry: None,
            byte_budget: None,
            agent_ctx: Some(agent_ctx),
            mcp: None,
        }
    }

    #[tokio::test]
    async fn missing_agent_name_returns_unified_error_shape() {
        let mut d = ToolDispatcher::new();
        d.register(Box::new(AgentSpawnTool)).unwrap();

        let result = d
            .dispatch(
                "agent.spawn",
                &json!({ "prompt": "hi" }),
                &ToolCtx::default(),
            )
            .await
            .unwrap();

        assert_eq!(
            result["error"].as_str(),
            Some("tool.agent.spawn: missing required parameter 'agent_name'"),
            "result: {result}"
        );
    }

    #[tokio::test]
    async fn missing_prompt_returns_unified_error_shape() {
        let mut d = ToolDispatcher::new();
        d.register(Box::new(AgentSpawnTool)).unwrap();

        let result = d
            .dispatch(
                "agent.spawn",
                &json!({ "agent_name": "child" }),
                &ToolCtx::default(),
            )
            .await
            .unwrap();

        assert_eq!(
            result["error"].as_str(),
            Some("tool.agent.spawn: missing required parameter 'prompt'"),
            "result: {result}"
        );
    }

    #[tokio::test]
    async fn unknown_agent_returns_typed_error() {
        let (_dir, session) = session_fixture().await;

        let agent_ctx = AgentSpawnCtx {
            agent_defs: Arc::new(vec![def("other", Isolation::Process)]),
            orchestrator: Arc::new(AgentOrchestrator::new()),
            session,
            parent_instance_id: AgentInstanceId::new(),
            current_msg_id: MessageId::new(),
        };
        let ctx = ctx_with(agent_ctx);

        let mut d = ToolDispatcher::new();
        d.register(Box::new(AgentSpawnTool)).unwrap();

        let result = d
            .dispatch(
                "agent.spawn",
                &json!({ "agent_name": "missing", "prompt": "hi" }),
                &ctx,
            )
            .await
            .unwrap();

        let err = result["error"].as_str().unwrap_or_default();
        assert!(
            err.contains("unknown agent 'missing'"),
            "expected unknown-agent error, got: {result}"
        );
    }

    #[tokio::test]
    async fn not_configured_ctx_returns_typed_error() {
        // Embedders that omit the agent runtime plumbing (e.g. `rerun_replace`,
        // tests that don't need spawning) must still be able to register the
        // tool without panicking when it's invoked.
        let mut d = ToolDispatcher::new();
        d.register(Box::new(AgentSpawnTool)).unwrap();

        let result = d
            .dispatch(
                "agent.spawn",
                &json!({ "agent_name": "child", "prompt": "hi" }),
                &ToolCtx::default(),
            )
            .await
            .unwrap();

        assert!(
            result["error"]
                .as_str()
                .unwrap_or_default()
                .contains("agent runtime not configured"),
            "result: {result}"
        );
    }

    #[tokio::test]
    async fn child_runs_under_child_isolation_not_parent() {
        // Critical invariant: a Process-isolation parent that spawns a
        // Container-isolation child must end up with the registered instance
        // using the child's isolation. The orchestrator never promotes or
        // demotes based on parent scope.
        let (_dir, session) = session_fixture().await;
        let orchestrator = Arc::new(AgentOrchestrator::new());

        let parent_def = def("parent", Isolation::Process);
        let child_def = def("child", Isolation::Container);

        // Register the parent instance up front so we have a realistic
        // parent_instance_id to pass into the tool.
        let parent_inst = orchestrator
            .spawn(parent_def, SpawnContext::user())
            .await
            .unwrap();
        assert_eq!(parent_inst.def.isolation, Isolation::Process);

        let agent_ctx = AgentSpawnCtx {
            agent_defs: Arc::new(vec![child_def.clone()]),
            orchestrator: Arc::clone(&orchestrator),
            session,
            parent_instance_id: parent_inst.id.clone(),
            current_msg_id: MessageId::new(),
        };
        let ctx = ctx_with(agent_ctx);

        let mut d = ToolDispatcher::new();
        d.register(Box::new(AgentSpawnTool)).unwrap();

        let result = d
            .dispatch(
                "agent.spawn",
                &json!({ "agent_name": "child", "prompt": "go" }),
                &ctx,
            )
            .await
            .unwrap();

        assert_eq!(result["ok"].as_bool(), Some(true), "result: {result}");

        let child_id_s = result["child_instance_id"].as_str().unwrap();
        let child_id = AgentInstanceId::from_string(child_id_s.to_string());
        let child_inst = orchestrator.get(&child_id).await.expect("child registered");

        assert_eq!(
            child_inst.def.isolation,
            Isolation::Container,
            "child isolation must match the child's def, not the parent's"
        );
        assert_ne!(
            child_inst.def.isolation, parent_inst.def.isolation,
            "privilege escalation guard: parent Process must not bleed into child Container"
        );
    }

    #[tokio::test]
    async fn emits_sub_agent_spawned_on_session_event_bus() {
        let (_dir, session) = session_fixture().await;
        let orchestrator = Arc::new(AgentOrchestrator::new());
        let parent_id = AgentInstanceId::new();
        let from_msg = MessageId::new();

        let mut rx = session.event_tx.subscribe();

        let agent_ctx = AgentSpawnCtx {
            agent_defs: Arc::new(vec![def("child", Isolation::Process)]),
            orchestrator: Arc::clone(&orchestrator),
            session: Arc::clone(&session),
            parent_instance_id: parent_id.clone(),
            current_msg_id: from_msg.clone(),
        };
        let ctx = ctx_with(agent_ctx);

        let mut d = ToolDispatcher::new();
        d.register(Box::new(AgentSpawnTool)).unwrap();

        let result = d
            .dispatch(
                "agent.spawn",
                &json!({ "agent_name": "child", "prompt": "go" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(result["ok"].as_bool(), Some(true), "result: {result}");

        // Drain events looking for SubAgentSpawned. Anything else emitted
        // is test noise.
        let mut found = None;
        while let Ok((_seq, event)) = rx.try_recv() {
            if let Event::SubAgentSpawned {
                parent,
                child,
                from_msg: emitted_from,
                ..
            } = event
            {
                found = Some((parent, child, emitted_from));
                break;
            }
        }
        let (p, c, m) = found.expect("SubAgentSpawned must be emitted");
        assert_eq!(p, parent_id, "parent must match ctx.parent_instance_id");
        assert_eq!(m, from_msg, "from_msg must match ctx.current_msg_id");
        // child is the fresh orchestrator-assigned id; just assert it
        // resolves to a registered instance with the child's isolation.
        let inst = orchestrator.get(&c).await.expect("child registered");
        assert_eq!(inst.def.name, "child");
        assert_eq!(inst.def.isolation, Isolation::Process);
    }

    #[tokio::test]
    async fn rejects_trusted_child_under_user_scope() {
        // The tool pins `SpawnContext::user()` regardless of what scope the
        // parent holds. A user-authored agent def with `Isolation::Trusted`
        // (which the parser already rejects at load time, but a test / fuzz
        // input can construct programmatically) must still be rejected at
        // runtime rather than silently promoted.
        let (_dir, session) = session_fixture().await;

        let agent_ctx = AgentSpawnCtx {
            agent_defs: Arc::new(vec![def("evil", Isolation::Trusted)]),
            orchestrator: Arc::new(AgentOrchestrator::new()),
            session,
            parent_instance_id: AgentInstanceId::new(),
            current_msg_id: MessageId::new(),
        };
        let ctx = ctx_with(agent_ctx);

        let mut d = ToolDispatcher::new();
        d.register(Box::new(AgentSpawnTool)).unwrap();

        let result = d
            .dispatch(
                "agent.spawn",
                &json!({ "agent_name": "evil", "prompt": "escalate" }),
                &ctx,
            )
            .await
            .unwrap();

        let err = result["error"].as_str().unwrap_or_default();
        assert!(
            err.contains("spawn failed") && err.to_lowercase().contains("trusted"),
            "expected isolation-violation rejection, got: {result}"
        );
    }

    #[tokio::test]
    async fn approval_preview_shows_agent_name() {
        let tool = AgentSpawnTool;
        let preview = tool.approval_preview(&json!({ "agent_name": "reviewer" }));
        assert!(
            preview.description.contains("reviewer"),
            "preview missing agent name: {}",
            preview.description,
        );
        assert!(
            preview.description.contains("Spawn sub-agent"),
            "preview missing action prefix: {}",
            preview.description,
        );
    }

    #[tokio::test]
    async fn malformed_context_arg_is_rejected() {
        // DoD names `context: AgentContext` as the third argument. The
        // concrete type lands in a follow-up; here we accept any JSON
        // object or null and reject everything else so a forged / buggy
        // provider call fails loud rather than silently dropping the arg.
        let mut d = ToolDispatcher::new();
        d.register(Box::new(AgentSpawnTool)).unwrap();

        let result = d
            .dispatch(
                "agent.spawn",
                &json!({ "agent_name": "child", "prompt": "go", "context": 42 }),
                &ToolCtx::default(),
            )
            .await
            .unwrap();

        assert!(
            result["error"]
                .as_str()
                .unwrap_or_default()
                .contains("'context' must be a JSON object or null"),
            "result: {result}"
        );
    }

    #[tokio::test]
    async fn well_formed_context_arg_is_accepted() {
        // A missing or null or object context is fine; the tool still
        // reaches the "not configured" branch because no agent_ctx was
        // wired in. Asserting that branch fires (rather than the context
        // branch) proves the validator didn't misfire on a valid call.
        let mut d = ToolDispatcher::new();
        d.register(Box::new(AgentSpawnTool)).unwrap();

        for ctx_val in [
            json!({ "agent_name": "c", "prompt": "p" }),
            json!({ "agent_name": "c", "prompt": "p", "context": null }),
            json!({ "agent_name": "c", "prompt": "p", "context": {"k":"v"} }),
        ] {
            let result = d
                .dispatch("agent.spawn", &ctx_val, &ToolCtx::default())
                .await
                .unwrap();
            assert!(
                result["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("agent runtime not configured"),
                "valid context should not trip the validator; result: {result}"
            );
        }
    }

    #[tokio::test]
    async fn duplicate_registration_fails() {
        let mut d = ToolDispatcher::new();
        d.register(Box::new(AgentSpawnTool)).unwrap();
        let err = d.register(Box::new(AgentSpawnTool)).unwrap_err();
        assert!(matches!(
            err,
            crate::tools::ToolError::DuplicateName(ref n) if n == "agent.spawn"
        ));
    }

    #[tokio::test]
    async fn rejects_oversized_agent_name() {
        // `validate_agent_name` caps at 64 bytes. 65 should fail loud rather
        // than reach `resolve_def` and silently miss.
        let oversized = "a".repeat(forge_agents::MAX_AGENT_NAME_BYTES + 1);

        let mut d = ToolDispatcher::new();
        d.register(Box::new(AgentSpawnTool)).unwrap();

        let result = d
            .dispatch(
                "agent.spawn",
                &json!({ "agent_name": oversized, "prompt": "hi" }),
                &ToolCtx::default(),
            )
            .await
            .unwrap();

        let err = result["error"].as_str().unwrap_or_default();
        assert!(
            err.contains("invalid agent_name") && err.contains("too large"),
            "expected size-cap rejection, got: {result}"
        );
    }

    #[tokio::test]
    async fn rejects_agent_name_with_forbidden_chars() {
        // Path separators, '..', leading '.', and arbitrary punctuation must
        // all be rejected at the tool entry point, mirroring the parse-time
        // rule for `.agents/*.md` stems.
        let bad_names = [
            "../escape",
            "child/sub",
            "child\\sub",
            ".hidden",
            "name!",
            "naïve", // non-ASCII
        ];

        let mut d = ToolDispatcher::new();
        d.register(Box::new(AgentSpawnTool)).unwrap();

        for bad in bad_names {
            let result = d
                .dispatch(
                    "agent.spawn",
                    &json!({ "agent_name": bad, "prompt": "hi" }),
                    &ToolCtx::default(),
                )
                .await
                .unwrap();
            let err = result["error"].as_str().unwrap_or_default();
            assert!(
                err.contains("invalid agent_name"),
                "must reject {bad:?}, got: {result}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_agent_name_with_whitespace() {
        let bad_names = [" leading", "trailing ", "mid space", "tab\tname"];

        let mut d = ToolDispatcher::new();
        d.register(Box::new(AgentSpawnTool)).unwrap();

        for bad in bad_names {
            let result = d
                .dispatch(
                    "agent.spawn",
                    &json!({ "agent_name": bad, "prompt": "hi" }),
                    &ToolCtx::default(),
                )
                .await
                .unwrap();
            let err = result["error"].as_str().unwrap_or_default();
            assert!(
                err.contains("invalid agent_name") && err.contains("whitespace"),
                "must reject whitespace in {bad:?}, got: {result}"
            );
        }
    }

    #[tokio::test]
    async fn accepts_valid_agent_name() {
        // A well-formed name must pass the validator and fall through to the
        // "agent runtime not configured" branch (since no ctx is wired in).
        // Asserting that branch fires proves the validator didn't misfire.
        let mut d = ToolDispatcher::new();
        d.register(Box::new(AgentSpawnTool)).unwrap();

        for ok in ["child", "code-reviewer", "agent_42", "Aa-Bb_99"] {
            let result = d
                .dispatch(
                    "agent.spawn",
                    &json!({ "agent_name": ok, "prompt": "hi" }),
                    &ToolCtx::default(),
                )
                .await
                .unwrap();
            assert!(
                result["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("agent runtime not configured"),
                "valid agent_name {ok:?} must not trip the validator; got: {result}"
            );
        }
    }
}
