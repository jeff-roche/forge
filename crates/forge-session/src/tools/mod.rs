//! Tool dispatch: name → handler routing for orchestrator tool calls.

use crate::byte_budget::ByteBudget;
use crate::sandbox::ChildRegistry;
use crate::session::Session;
use forge_agents::{AgentDef, Orchestrator as AgentOrchestrator};
use forge_core::ids::AgentInstanceId;
use forge_core::{ApprovalPreview, MessageId};
use std::sync::Arc;

pub mod agent_spawn;
pub mod args;
pub mod fs_edit;
pub mod fs_read;
pub mod fs_write;
pub mod mcp;
pub mod memory_write;
pub mod shell_exec;

pub use agent_spawn::AgentSpawnTool;
pub use args::{get_optional_str, get_optional_u64, get_required_str};
pub use fs_edit::FsEditTool;
pub use fs_read::FsReadTool;
pub use fs_write::FsWriteTool;
pub use mcp::McpTool;
pub use memory_write::MemoryWriteTool;
pub use shell_exec::ShellExecTool;

/// F-599: `Clone` so the orchestrator can hand each call in a parallel
/// read-only batch its own owned `ToolCtx` for the spawned `JoinSet` task.
/// Every field is already cheap to clone (vec/optional Arc/optional
/// Clone-able registry); no field carries non-Clone state.
#[derive(Default, Clone)]
pub struct ToolCtx {
    pub allowed_paths: Vec<String>,
    /// Workspace root for tools that spawn subprocesses (e.g. `shell.exec`).
    pub workspace_root: Option<std::path::PathBuf>,
    /// Registry of live sandboxed children — populated for tools that spawn
    /// processes so session shutdown can kill them.
    pub child_registry: Option<ChildRegistry>,
    /// F-077: per-session aggregate byte budget. When `Some`, the
    /// dispatcher refuses further calls once the budget is exhausted
    /// and charges each successful call by the bytes its result
    /// occupies. `None` means no aggregate enforcement — preserves the
    /// pre-F-077 behaviour for tests and any embedding that opts out.
    pub byte_budget: Option<Arc<ByteBudget>>,
    /// F-134: per-turn agent runtime plumbing for the `agent.spawn` tool.
    pub agent_ctx: Option<AgentSpawnCtx>,
    /// F-132: shared MCP manager for `McpTool` adapters. Threaded through
    /// the context so the orchestrator can register one `McpTool` per
    /// advertised tool at turn start without the adapter needing to
    /// re-lookup the manager on every invoke. `None` when the session
    /// runs without MCP — adapters are simply not registered.
    pub mcp: Option<std::sync::Arc<forge_mcp::McpManager>>,
}

/// F-134: context threaded to the `agent.spawn` tool so it can resolve a
/// child `AgentDef` by name, call `Orchestrator::spawn(child_def, ctx)` with
/// the child's own isolation, and emit `Event::SubAgentSpawned` against the
/// session event log.
///
/// Every field is required for the tool to succeed; the outer
/// `ToolCtx.agent_ctx: Option<_>` lets embedders (tests, `rerun_replace`)
/// omit the plumbing entirely — the tool then returns a typed "not
/// configured" error rather than panicking.
#[derive(Clone)]
pub struct AgentSpawnCtx {
    /// The agent definitions the tool can resolve `agent_name` against.
    /// Loaded once per turn via `forge_agents::load_agents`.
    pub agent_defs: Arc<Vec<AgentDef>>,
    /// Runtime orchestrator that actually registers the child instance
    /// and emits lifecycle events. Shared across the turn so F-140's
    /// monitor can subscribe once and see every spawn.
    pub orchestrator: Arc<AgentOrchestrator>,
    /// Session whose event log the tool writes `SubAgentSpawned` into.
    pub session: Arc<Session>,
    /// Parent agent instance id — the "parent" field in the emitted event.
    pub parent_instance_id: AgentInstanceId,
    /// The `msg_id` of the assistant turn that issued the tool call —
    /// the `from_msg` field in the emitted event (matches the existing
    /// `forge_core::Event::SubAgentSpawned` wire shape).
    pub current_msg_id: MessageId,
}

/// F-140: session-scoped agent runtime handles that live longer than a
/// single turn.
///
/// `run_turn` / `rerun_message` take this as an `Option<AgentRuntime>` and
/// synthesize a per-turn [`AgentSpawnCtx`] by combining the runtime with
/// the turn's `Session` + `current_msg_id`. The split keeps the long-lived
/// handles (orchestrator, loaded defs, parent instance id) under the
/// caller's control while the per-turn fields stay local to each turn.
///
/// Producers (F-140 `serve_with_session`):
///   1. build an `Arc<forge_agents::Orchestrator>` once per session,
///   2. `Orchestrator::spawn` a "session root" `AgentInstance` whose id
///      becomes `parent_instance_id` — every top-level `agent.spawn`
///      attributes back to this root, and every `StepStarted.instance_id`
///      the turn loop emits carries the same id so the Agent Monitor can
///      group a session's trace against a stable parent.
///   3. load agent defs via `forge_agents::load_agents`,
///   4. pass the resulting `AgentRuntime` to `run_turn` / `rerun_message`.
///
/// Consumers that don't need spawning (tests, no-op embedders) pass
/// `None`; the tool still registers on the dispatcher and returns the
/// existing "agent runtime not configured" error shape when invoked.
#[derive(Clone)]
pub struct AgentRuntime {
    /// Shared orchestrator that registers child instances and emits
    /// lifecycle events. Kept alive across all turns in a session so the
    /// Agent Monitor sees a coherent registry.
    pub orchestrator: Arc<AgentOrchestrator>,
    /// Merged workspace + user agent defs, loaded once at session start.
    pub agent_defs: Arc<Vec<AgentDef>>,
    /// The session's root `AgentInstance` id. Every turn's
    /// `StepStarted.instance_id` and every spawned sub-agent's `parent`
    /// attributes to this id.
    pub parent_instance_id: AgentInstanceId,
    /// F-663: the **active agent**'s `AgentDef.allowed_paths` — the glob
    /// list the agent declared as its scope. When non-empty, the
    /// orchestrator narrows `ToolCtx.allowed_paths` to this list before
    /// dispatching tool calls, so an agent cannot reach outside its
    /// declared scope even when the session's workspace-derived globs
    /// would permit it. Empty preserves the pre-F-663 fallback (use the
    /// session scope) for back-compat with agents that didn't declare.
    pub def_allowed_paths: Vec<String>,
}

#[cfg(test)]
mod effective_allowed_paths_tests {
    use super::effective_allowed_paths;

    #[test]
    fn empty_agent_falls_back_to_session() {
        let session = vec!["/ws/**".to_string()];
        assert_eq!(effective_allowed_paths(&session, &[]), session);
    }

    #[test]
    fn non_empty_agent_overrides_session() {
        let session = vec!["/ws/**".to_string()];
        let agent = vec!["/ws/scoped/**".to_string()];
        assert_eq!(effective_allowed_paths(&session, &agent), agent);
    }

    #[test]
    fn empty_session_and_empty_agent_yield_empty() {
        // forge-fs treats an empty list as deny-all — the helper preserves
        // that contract on both inputs.
        assert!(effective_allowed_paths(&[], &[]).is_empty());
    }

    #[test]
    fn agent_can_be_broader_than_session_by_design() {
        // The override is intentional — an agent author can opt into a
        // wider declared scope than the session glob, with the
        // understanding that the kernel-level workspace_root still bounds
        // shell.exec via its own check. This test pins the contract.
        let session = vec!["/ws/sub/**".to_string()];
        let agent = vec!["/ws/**".to_string()];
        assert_eq!(effective_allowed_paths(&session, &agent), agent);
    }
}

/// F-663: compute the effective `allowed_paths` for a tool dispatch.
///
/// When the active agent declared a scope (`agent` non-empty), the agent's
/// own list **overrides** the session-derived list — the agent cannot
/// reach outside what it explicitly opted into. When the agent declared
/// nothing, the session scope is preserved verbatim (back-compat).
///
/// Override (rather than intersection) matches the existing
/// `bg_agents::build_sidecar_hello` contract, which already hands
/// `def.allowed_paths` straight to the sidecar without merging in the
/// session's globs. Keeping both paths semantically aligned avoids a
/// scope-mismatch between in-process and sidecar execution.
pub fn effective_allowed_paths(session: &[String], agent: &[String]) -> Vec<String> {
    if agent.is_empty() {
        session.to_vec()
    } else {
        agent.to_vec()
    }
}

/// Tool handler. `invoke` is `async` so filesystem / blocking work can be
/// wrapped in `tokio::task::spawn_blocking` at the async/sync boundary
/// (F-106) without blocking a tokio worker thread while a large file
/// read/write/edit runs for concurrent sessions sharing the worker.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn approval_preview(&self, args: &serde_json::Value) -> ApprovalPreview;
    async fn invoke(&self, args: &serde_json::Value, ctx: &ToolCtx) -> serde_json::Value;
    /// F-132: `true` when calling the tool never mutates external state.
    ///
    /// The orchestrator skips the user-approval prompt for read-only tools and
    /// logs an `ApprovalSource::Auto` entry so the event stream still shows the
    /// tool was admitted. Defaults to `false` so every existing built-in (all
    /// of which may mutate: `fs.write`, `fs.edit`, `shell.exec`, `agent.spawn`)
    /// continues to hit the approval path — this is the safer-by-default
    /// contract mirroring `forge_core::Tool.read_only`. MCP tools override this
    /// with the server-declared `readOnlyHint` annotation.
    fn read_only(&self) -> bool {
        false
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ToolError {
    #[error("tool '{0}' is already registered")]
    DuplicateName(String),
    #[error("unknown tool '{0}'")]
    UnknownTool(String),
    /// A required string argument was absent or the value was not a JSON
    /// string. Empty strings are accepted — see [`args::get_required_str`]
    /// for the rationale. `Display` shape is
    /// `tool.{tool}: missing required parameter '{arg}'` and is asserted by
    /// IPC-level regression tests; treat it as contractual.
    #[error("tool.{tool}: missing required parameter '{arg}'")]
    MissingRequiredArg { tool: String, arg: String },
}

#[derive(Default)]
pub struct ToolDispatcher {
    /// F-599: `Arc<dyn Tool>` rather than `Box<dyn Tool>` so the orchestrator
    /// can clone a handle into a `tokio::task::JoinSet` task and drive a
    /// read-only batch concurrently without forcing the dispatcher to
    /// outlive the spawned futures. `register` keeps its `Box<dyn Tool>`
    /// shape on the public surface — `Arc::from(box)` is a cost-free
    /// rewrap — so every existing call site (including embedders) keeps
    /// compiling with no churn.
    tools: std::collections::HashMap<String, Arc<dyn Tool>>,
}

impl ToolDispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), ToolError> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(ToolError::DuplicateName(name));
        }
        self.tools.insert(name, Arc::from(tool));
        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<&dyn Tool, ToolError> {
        self.tools
            .get(name)
            .map(|t| t.as_ref())
            .ok_or_else(|| ToolError::UnknownTool(name.to_string()))
    }

    /// F-599: hand out an owned [`Arc<dyn Tool>`] so a parallel-batch
    /// dispatcher can move the handle into a `JoinSet` task with a
    /// `'static` future. Returns the same error shape as [`Self::get`]
    /// for an unknown name.
    pub fn get_arc(&self, name: &str) -> Result<Arc<dyn Tool>, ToolError> {
        self.tools
            .get(name)
            .cloned()
            .ok_or_else(|| ToolError::UnknownTool(name.to_string()))
    }

    pub async fn dispatch(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<serde_json::Value, ToolError> {
        let tool = self.get(name)?;

        // F-077 / F-599: route the budget pre-check + invoke + post-charge
        // through the shared helper so the orchestrator's parallel and
        // sequential paths (which call `Tool::invoke` directly on an
        // `Arc<dyn Tool>` they spawn into a `JoinSet`) get the same
        // enforcement without round-tripping the dispatcher.
        Ok(invoke_with_budget(tool, args, ctx).await)
    }
}

/// F-599 budget gate: pre-check the per-session aggregate byte budget,
/// invoke the tool, then post-charge by the bytes the result occupies.
///
/// This is the single source of truth for budget enforcement. Both
/// `ToolDispatcher::dispatch` and the orchestrator's parallel /
/// sequential dispatch paths route through here; bypassing it (calling
/// `tool.invoke` directly) loses the cap and is a regression.
///
/// On exhaustion the tool is **not** invoked — the helper returns a
/// synthetic `{"error": "session byte budget exceeded: …"}` object and
/// the budget counter is unchanged. This matches the pre-F-599 behaviour
/// and the byte-budget module preamble.
pub(crate) async fn invoke_with_budget(
    tool: &dyn Tool,
    args: &serde_json::Value,
    ctx: &ToolCtx,
) -> serde_json::Value {
    if let Some(budget) = ctx.byte_budget.as_ref() {
        if budget.is_exhausted_with_overhead(1) {
            return serde_json::json!({
                "error": format!(
                    "session byte budget exceeded: {}/{} bytes",
                    budget.consumed(),
                    budget.limit(),
                )
            });
        }
    }

    let result = tool.invoke(args, ctx).await;

    if let Some(budget) = ctx.byte_budget.as_ref() {
        budget.charge(result_byte_cost(&result) + crate::byte_budget::PER_TOOL_CALL_OVERHEAD_BYTES);
    }

    result
}

/// F-077: cost a tool result for budget accounting. The intent is to
/// approximate the in-memory footprint the result imposes on the
/// session — we sum the lengths of the *opaque payload* fields each
/// tool returns and ignore framing / metadata. Specifically:
///
/// - `fs.read` charges the `content` byte length (not `bytes`, since
///   `bytes` reflects on-disk size and `content` is the lossy-UTF-8
///   string actually held in memory).
/// - `fs.write` / `fs.edit` carry no payload back — `{"ok": true}` —
///   so the cost is the JSON envelope length as a small token charge.
///   This intentionally lets a write-heavy session run past the
///   budget for far longer than a read-heavy one (writes do not
///   buffer remote-fetched bytes in the daemon).
/// - `shell.exec` charges the lengths of `stdout` and `stderr`.
/// - Errors and unknown shapes fall back to the serialized JSON length;
///   that bounds even a tool that returns a giant unstructured blob.
fn result_byte_cost(result: &serde_json::Value) -> u64 {
    if let Some(obj) = result.as_object() {
        // fs.read shape
        if let Some(content) = obj.get("content").and_then(|v| v.as_str()) {
            return content.len() as u64;
        }
        // shell.exec shape
        if obj.contains_key("stdout") || obj.contains_key("stderr") {
            let so = obj.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
            let se = obj.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
            return so.len() as u64 + se.len() as u64;
        }
    }
    serde_json::to_string(result)
        .map(|s| s.len() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use tempfile::NamedTempFile;

    struct StubTool {
        name: &'static str,
        response: serde_json::Value,
    }

    #[async_trait::async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn approval_preview(&self, _args: &serde_json::Value) -> ApprovalPreview {
            ApprovalPreview {
                description: format!("stub: {}", self.name),
            }
        }
        async fn invoke(&self, _args: &serde_json::Value, _ctx: &ToolCtx) -> serde_json::Value {
            self.response.clone()
        }
    }

    fn empty_ctx() -> ToolCtx {
        ToolCtx::default()
    }

    #[tokio::test]
    async fn register_and_dispatch_returns_tool_result() {
        let mut d = ToolDispatcher::new();
        d.register(Box::new(StubTool {
            name: "noop",
            response: json!({"ok": true}),
        }))
        .unwrap();

        let result = d.dispatch("noop", &json!({}), &empty_ctx()).await.unwrap();
        assert_eq!(result, json!({"ok": true}));
    }

    #[test]
    fn duplicate_registration_returns_error() {
        let mut d = ToolDispatcher::new();
        d.register(Box::new(StubTool {
            name: "noop",
            response: json!({}),
        }))
        .unwrap();

        let err = d
            .register(Box::new(StubTool {
                name: "noop",
                response: json!({}),
            }))
            .unwrap_err();
        assert_eq!(err, ToolError::DuplicateName("noop".to_string()));
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_error() {
        let d = ToolDispatcher::new();
        let err = d
            .dispatch("nope", &json!({}), &empty_ctx())
            .await
            .unwrap_err();
        assert_eq!(err, ToolError::UnknownTool("nope".to_string()));
    }

    // ---- shared `args` helpers (`get_required_str`, `get_optional_str`,
    // `get_optional_u64`) live in `tools::args` with their own unit tests.
    // The per-tool integration tests below assert the unified IPC error
    // shape that those helpers produce on the `invoke` boundary. ----

    // ---- F-074: each tool surfaces the unified missing-arg error from
    // `invoke` rather than coercing missing required args to "" and producing
    // a confusing downstream error from `forge_fs` / sandbox. ----

    #[tokio::test]
    async fn fs_read_invoke_errors_explicitly_on_missing_path() {
        let mut d = ToolDispatcher::new();
        d.register(Box::new(FsReadTool)).unwrap();
        let result = d
            .dispatch("fs.read", &json!({}), &empty_ctx())
            .await
            .unwrap();
        assert_eq!(
            result["error"].as_str(),
            Some("tool.fs.read: missing required parameter 'path'"),
            "result was: {result}"
        );
    }

    #[tokio::test]
    async fn fs_write_invoke_errors_explicitly_on_missing_path() {
        let mut d = ToolDispatcher::new();
        d.register(Box::new(FsWriteTool)).unwrap();
        let result = d
            .dispatch("fs.write", &json!({ "content": "hi" }), &empty_ctx())
            .await
            .unwrap();
        assert_eq!(
            result["error"].as_str(),
            Some("tool.fs.write: missing required parameter 'path'"),
            "result was: {result}"
        );
    }

    #[tokio::test]
    async fn fs_write_invoke_errors_explicitly_on_missing_content() {
        let mut d = ToolDispatcher::new();
        d.register(Box::new(FsWriteTool)).unwrap();
        let result = d
            .dispatch(
                "fs.write",
                &json!({ "path": "/tmp/forge-f074-noop" }),
                &empty_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(
            result["error"].as_str(),
            Some("tool.fs.write: missing required parameter 'content'"),
            "result was: {result}"
        );
    }

    #[tokio::test]
    async fn fs_write_invoke_allows_empty_content_to_truncate() {
        // Empty content must remain a valid request — only missing keys
        // should error. Otherwise the F-074 helper would silently break the
        // legitimate "create empty file" / "truncate" use case.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("empty.txt");
        std::fs::write(&target, "old contents").unwrap();
        let canonical_parent = std::fs::canonicalize(dir.path()).unwrap();
        let allowed = format!("{}/**", canonical_parent.to_str().unwrap());

        let mut d = ToolDispatcher::new();
        d.register(Box::new(FsWriteTool)).unwrap();
        let ctx = ToolCtx {
            allowed_paths: vec![allowed],
            ..ToolCtx::default()
        };
        let result = d
            .dispatch(
                "fs.write",
                &json!({ "path": target.to_str().unwrap(), "content": "" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(result["ok"].as_bool(), Some(true), "result: {result}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
    }

    #[tokio::test]
    async fn fs_edit_invoke_errors_explicitly_on_missing_path() {
        let mut d = ToolDispatcher::new();
        d.register(Box::new(FsEditTool)).unwrap();
        let result = d
            .dispatch("fs.edit", &json!({ "patch": "@@ -1 +1 @@" }), &empty_ctx())
            .await
            .unwrap();
        assert_eq!(
            result["error"].as_str(),
            Some("tool.fs.edit: missing required parameter 'path'"),
            "result was: {result}"
        );
    }

    #[tokio::test]
    async fn fs_edit_invoke_errors_explicitly_on_missing_patch() {
        let mut d = ToolDispatcher::new();
        d.register(Box::new(FsEditTool)).unwrap();
        let result = d
            .dispatch(
                "fs.edit",
                &json!({ "path": "/tmp/forge-f074-noop" }),
                &empty_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(
            result["error"].as_str(),
            Some("tool.fs.edit: missing required parameter 'patch'"),
            "result was: {result}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shell_exec_invoke_errors_explicitly_on_missing_command() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = ToolDispatcher::new();
        d.register(Box::new(ShellExecTool)).unwrap();
        let ctx = ToolCtx {
            allowed_paths: vec![],
            workspace_root: Some(dir.path().to_path_buf()),
            child_registry: Some(crate::sandbox::ChildRegistry::new()),
            byte_budget: None,
            agent_ctx: None,
            mcp: None,
        };
        let result = d.dispatch("shell.exec", &json!({}), &ctx).await.unwrap();
        assert_eq!(
            result["error"].as_str(),
            Some("tool.shell.exec: missing required parameter 'command'"),
            "result was: {result}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shell_exec_invoke_errors_explicitly_on_empty_command() {
        // shell.exec layers an empty-guard on top of `get_required_str`
        // because spawning `""` is meaningless. The error shape stays
        // unified ("tool.X: missing required parameter 'Y'") so the IPC
        // surface is consistent across all four tools.
        let dir = tempfile::tempdir().unwrap();
        let mut d = ToolDispatcher::new();
        d.register(Box::new(ShellExecTool)).unwrap();
        let ctx = ToolCtx {
            allowed_paths: vec![],
            workspace_root: Some(dir.path().to_path_buf()),
            child_registry: Some(crate::sandbox::ChildRegistry::new()),
            byte_budget: None,
            agent_ctx: None,
            mcp: None,
        };
        let result = d
            .dispatch("shell.exec", &json!({ "command": "" }), &ctx)
            .await
            .unwrap();
        assert_eq!(
            result["error"].as_str(),
            Some("tool.shell.exec: missing required parameter 'command'"),
            "result was: {result}"
        );
    }

    #[tokio::test]
    async fn fs_write_dispatch_writes_file_and_previews_diff() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        let canonical_parent = std::fs::canonicalize(dir.path()).unwrap();
        let allowed = format!("{}/**", canonical_parent.to_str().unwrap());

        let mut d = ToolDispatcher::new();
        d.register(Box::new(FsWriteTool)).unwrap();

        let ctx = ToolCtx {
            allowed_paths: vec![allowed],
            ..ToolCtx::default()
        };
        let result = d
            .dispatch(
                "fs.write",
                &json!({"path": target.to_str().unwrap(), "content": "hi"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(result["ok"].as_bool(), Some(true));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi");

        let preview = d
            .get("fs.write")
            .unwrap()
            .approval_preview(&json!({"path": target.to_str().unwrap(), "content": "hi"}));
        assert!(preview.description.contains("Write file"));
    }

    #[tokio::test]
    async fn fs_edit_dispatch_applies_patch_and_previews_diff() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("src.txt");
        std::fs::write(&target, "alpha\nbeta\n").unwrap();
        let canonical_parent = std::fs::canonicalize(dir.path()).unwrap();
        let allowed = format!("{}/**", canonical_parent.to_str().unwrap());

        let patch = similar::TextDiff::from_lines("alpha\nbeta\n", "alpha\nBETA\n")
            .unified_diff()
            .to_string();

        let mut d = ToolDispatcher::new();
        d.register(Box::new(FsEditTool)).unwrap();

        let ctx = ToolCtx {
            allowed_paths: vec![allowed],
            ..ToolCtx::default()
        };
        let result = d
            .dispatch(
                "fs.edit",
                &json!({"path": target.to_str().unwrap(), "patch": patch}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(result["ok"].as_bool(), Some(true));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "alpha\nBETA\n");

        let preview = d
            .get("fs.edit")
            .unwrap()
            .approval_preview(&json!({"path": target.to_str().unwrap(), "patch": patch}));
        assert!(preview.description.contains("Edit file"));
    }

    #[tokio::test]
    async fn fs_read_dispatch_returns_content_bytes_sha256() {
        let mut file = NamedTempFile::new().unwrap();
        let body = "hello dispatcher";
        file.write_all(body.as_bytes()).unwrap();
        let path = file.path().to_str().unwrap().to_string();
        let canonical = std::fs::canonicalize(&path).unwrap();
        let allowed = canonical.to_str().unwrap().to_string();

        let mut d = ToolDispatcher::new();
        d.register(Box::new(FsReadTool)).unwrap();

        let ctx = ToolCtx {
            allowed_paths: vec![allowed],
            ..ToolCtx::default()
        };
        let result = d
            .dispatch("fs.read", &json!({"path": path}), &ctx)
            .await
            .unwrap();

        assert_eq!(result["content"].as_str().unwrap(), body);
        assert_eq!(result["bytes"].as_u64().unwrap(), body.len() as u64);
        assert_eq!(result["sha256"].as_str().unwrap().len(), 64);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shell_exec_dispatch_runs_command_and_captures_stdout() {
        let dir = tempfile::tempdir().unwrap();

        let mut d = ToolDispatcher::new();
        d.register(Box::new(ShellExecTool)).unwrap();

        let ctx = ToolCtx {
            allowed_paths: vec![],
            workspace_root: Some(dir.path().to_path_buf()),
            child_registry: Some(crate::sandbox::ChildRegistry::new()),
            byte_budget: None,
            agent_ctx: None,
            mcp: None,
        };
        let result = d
            .dispatch(
                "shell.exec",
                &json!({"command": "/bin/sh", "args": ["-c", "echo hello-sandbox"], "timeout_ms": 5000}),
                &ctx,
            )
            .await
            .unwrap();

        assert_eq!(result["exit_code"].as_i64(), Some(0), "result: {result}");
        assert_eq!(
            result["stdout"].as_str().unwrap().trim(),
            "hello-sandbox",
            "result: {result}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shell_exec_dispatch_works_inside_outer_tokio_runtime() {
        // Regression: Tool::invoke is now `async` (F-106). Dispatch must
        // await cleanly on a multi-threaded runtime without the pre-F-106
        // `block_in_place + block_on` shim.
        let dir = tempfile::tempdir().unwrap();

        let mut d = ToolDispatcher::new();
        d.register(Box::new(ShellExecTool)).unwrap();

        let ctx = ToolCtx {
            allowed_paths: vec![],
            workspace_root: Some(dir.path().to_path_buf()),
            child_registry: Some(crate::sandbox::ChildRegistry::new()),
            byte_budget: None,
            agent_ctx: None,
            mcp: None,
        };

        let result = d
            .dispatch(
                "shell.exec",
                &json!({"command": "/bin/sh", "args": ["-c", "echo from-async"], "timeout_ms": 5000}),
                &ctx,
            )
            .await
            .unwrap();

        assert_eq!(result["exit_code"].as_i64(), Some(0), "result: {result}");
        assert_eq!(result["stdout"].as_str().unwrap().trim(), "from-async");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shell_exec_timeout_kills_sandboxed_grandchild() {
        // F-047 / H6: after `shell.exec` returns a timeout error, the
        // sandboxed process group (including detached grandchildren) must
        // be dead. Before the fix, `run_linux` called `.into_child()` which
        // detached the `SandboxedChild` from the registry and disabled the
        // Drop-based `killpg`, so the grandchild leaked past both the
        // tool call and daemon shutdown.

        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");

        let mut d = ToolDispatcher::new();
        d.register(Box::new(ShellExecTool)).unwrap();

        let registry = crate::sandbox::ChildRegistry::new();
        let ctx = ToolCtx {
            allowed_paths: vec![],
            workspace_root: Some(dir.path().to_path_buf()),
            child_registry: Some(registry.clone()),
            byte_budget: None,
            agent_ctx: None,
            mcp: None,
        };

        // Background a long sleep, write its pid, detach its stdio from
        // the parent shell so that when the shell exits the wait_fut may
        // complete but the grandchild stays alive. The parent shell needs
        // to keep running long enough for the timeout to fire, so we also
        // sleep in the foreground.
        let script = format!(
            "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > {}; sleep 30",
            pid_file.display()
        );

        let ctx_clone_args = serde_json::json!({
            "command": "/bin/sh",
            "args": ["-c", script],
            "timeout_ms": 200,
        });

        let result = d
            .dispatch("shell.exec", &ctx_clone_args, &ctx)
            .await
            .unwrap();

        assert!(
            result.get("error").is_some(),
            "expected timeout error, got: {result}"
        );

        // Read grandchild pid written by the script.
        let mut grandchild_pid: Option<i32> = None;
        for _ in 0..50 {
            if let Ok(s) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = s.trim().parse::<i32>() {
                    grandchild_pid = Some(pid);
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let grandchild_pid = grandchild_pid.expect("script never wrote grandchild pid");

        // Poll for the grandchild to be reaped — killpg delivery plus shell
        // teardown can take a few ms on loaded CI.
        let mut alive = true;
        for _ in 0..100 {
            let rc = unsafe { libc::kill(grandchild_pid, 0) };
            if rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                alive = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !alive,
            "grandchild {grandchild_pid} survived shell.exec timeout \
             (sandbox escape — pgid was not killed)"
        );

        // Secondary assertion: the registry entry was cleared. If Drop
        // on `SandboxedChild` ran, `registry.remove(pgid)` executed.
        assert!(
            registry.is_empty(),
            "ChildRegistry still tracks pgid after timeout — Drop did not run"
        );
    }

    #[test]
    fn shell_exec_preview_includes_cwd_when_provided() {
        let tool = ShellExecTool;
        let preview = tool.approval_preview(&json!({
            "command": "ls",
            "args": ["."],
            "cwd": "/tmp/somewhere-specific"
        }));
        assert!(
            preview.description.contains("/tmp/somewhere-specific"),
            "cwd missing from preview: {}",
            preview.description
        );
        assert!(
            preview.description.contains("ls"),
            "command missing from preview: {}",
            preview.description
        );
    }

    #[test]
    fn shell_exec_preview_omits_cwd_when_absent() {
        let tool = ShellExecTool;
        let preview = tool.approval_preview(&json!({
            "command": "ls",
            "args": ["."]
        }));
        assert!(
            !preview.description.to_lowercase().contains("cwd"),
            "cwd should not appear when absent: {}",
            preview.description
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shell_exec_dispatch_rejects_cwd_outside_workspace() {
        let dir = tempfile::tempdir().unwrap();

        let mut d = ToolDispatcher::new();
        d.register(Box::new(ShellExecTool)).unwrap();

        let ctx = ToolCtx {
            allowed_paths: vec![],
            workspace_root: Some(dir.path().to_path_buf()),
            child_registry: Some(crate::sandbox::ChildRegistry::new()),
            byte_budget: None,
            agent_ctx: None,
            mcp: None,
        };
        let result = d
            .dispatch(
                "shell.exec",
                &json!({
                    "command": "/bin/sh",
                    "args": ["-c", "echo should-not-run"],
                    "cwd": "/etc",
                    "timeout_ms": 5000
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(
            result.get("error").is_some(),
            "expected error when cwd is outside workspace; got: {result}"
        );
        assert!(
            result.get("exit_code").is_none(),
            "command must not execute when cwd is rejected; got: {result}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shell_exec_dispatch_accepts_cwd_inside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();

        let mut d = ToolDispatcher::new();
        d.register(Box::new(ShellExecTool)).unwrap();

        let ctx = ToolCtx {
            allowed_paths: vec![],
            workspace_root: Some(dir.path().to_path_buf()),
            child_registry: Some(crate::sandbox::ChildRegistry::new()),
            byte_budget: None,
            agent_ctx: None,
            mcp: None,
        };
        let result = d
            .dispatch(
                "shell.exec",
                &json!({
                    "command": "/bin/sh",
                    "args": ["-c", "pwd"],
                    "cwd": sub.to_str().unwrap(),
                    "timeout_ms": 5000
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert_eq!(result["exit_code"].as_i64(), Some(0), "result: {result}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shell_exec_dispatch_rejects_cwd_via_symlink_escape() {
        // Symlink inside the workspace pointing outside must be rejected:
        // canonicalize() resolves the symlink, and the post-canonical
        // prefix check catches the escape.
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("escape");
        std::os::unix::fs::symlink("/etc", &link).unwrap();

        let mut d = ToolDispatcher::new();
        d.register(Box::new(ShellExecTool)).unwrap();

        let ctx = ToolCtx {
            allowed_paths: vec![],
            workspace_root: Some(dir.path().to_path_buf()),
            child_registry: Some(crate::sandbox::ChildRegistry::new()),
            byte_budget: None,
            agent_ctx: None,
            mcp: None,
        };
        let result = d
            .dispatch(
                "shell.exec",
                &json!({
                    "command": "/bin/sh",
                    "args": ["-c", "echo should-not-run"],
                    "cwd": link.to_str().unwrap(),
                    "timeout_ms": 5000
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(
            result.get("error").is_some(),
            "symlink escape must be rejected; got: {result}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shell_exec_clamps_timeout_ms_to_documented_ceiling() {
        // Regression for F-066: a provider may pass an arbitrarily large
        // timeout_ms (up to u64::MAX). Without the clamp, tokio::time::timeout
        // receives a far-future deadline and a shell backgrounding sleeps can
        // hold the tool-call future open indefinitely. With the clamp, the
        // deadline is capped at MAX_TIMEOUT_MS (10 minutes) so the tool call
        // always resolves within the ceiling. We drive a fast-exiting command
        // here so the test runs quickly — the assertion is that the dispatch
        // returns successfully (proving u64::MAX does not overflow or hang)
        // and completes well under the ceiling.
        let dir = tempfile::tempdir().unwrap();

        let mut d = ToolDispatcher::new();
        d.register(Box::new(ShellExecTool)).unwrap();

        let ctx = ToolCtx {
            allowed_paths: vec![],
            workspace_root: Some(dir.path().to_path_buf()),
            child_registry: Some(crate::sandbox::ChildRegistry::new()),
            byte_budget: None,
            agent_ctx: None,
            mcp: None,
        };

        let started = std::time::Instant::now();
        let result = d
            .dispatch(
                "shell.exec",
                &json!({"command": "/bin/true", "timeout_ms": u64::MAX}),
                &ctx,
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();

        assert_eq!(result["exit_code"].as_i64(), Some(0), "result: {result}");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "u64::MAX timeout caused dispatch to hang for {elapsed:?}"
        );
        // Compile-time guard: ceiling exists and stays within the documented
        // 10-minute budget.
        const _: () = assert!(shell_exec::MAX_TIMEOUT_MS <= 10 * 60 * 1000);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shell_exec_dispatch_clears_daemon_env_by_default() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("FORGE_SHELL_EXEC_CANARY", "nope");

        let mut d = ToolDispatcher::new();
        d.register(Box::new(ShellExecTool)).unwrap();

        let ctx = ToolCtx {
            allowed_paths: vec![],
            workspace_root: Some(dir.path().to_path_buf()),
            child_registry: Some(crate::sandbox::ChildRegistry::new()),
            byte_budget: None,
            agent_ctx: None,
            mcp: None,
        };
        let result = d
            .dispatch(
                "shell.exec",
                &json!({"command": "/usr/bin/env", "timeout_ms": 5000}),
                &ctx,
            )
            .await
            .unwrap();
        std::env::remove_var("FORGE_SHELL_EXEC_CANARY");

        let stdout = result["stdout"].as_str().unwrap_or_default();
        assert!(
            !stdout.contains("FORGE_SHELL_EXEC_CANARY"),
            "daemon env leaked into shell.exec:\n{stdout}"
        );
    }
}
