//! `memory.write` tool (F-601): writes the active agent's cross-session
//! memory file at `~/.config/forge/memory/<agent>.md`.
//!
//! The tool is gated on the per-agent memory flag — it is only registered on
//! the dispatcher when the agent owning the session has `memory_enabled:
//! true` in its frontmatter, so an agent that has not opted in cannot
//! discover the tool name and the surface area stays minimal.
//!
//! Tool surface (matches the DoD verbatim):
//!
//! ```text
//! memory.write { content: string, mode: "append" | "replace" }
//! ```
//!
//! On success the tool returns `{ "ok": true, "version": N, "updated_at":
//! "<iso8601>" }` so the agent observes its own write metadata. On any
//! failure the tool returns `{ "error": "<message>" }` rather than
//! propagating an exception — the dispatcher contract is that `invoke`
//! always succeeds and the model decides what to do with the JSON shape.

use std::sync::Arc;

use forge_agents::{MemoryStore, WriteMode, MEMORY_WRITE_CONTENT_CAP};
use forge_core::ApprovalPreview;

use super::{get_required_str, Tool, ToolCtx};

/// `memory.write` registration. The active agent id is captured at
/// registration time so the tool always writes to the same file regardless
/// of how the dispatcher is later threaded through the request loop.
pub struct MemoryWriteTool {
    store: Arc<MemoryStore>,
    agent_id: String,
}

impl MemoryWriteTool {
    /// Wire-name surfaced to the model. Asserted by IPC-level tests.
    pub const NAME: &'static str = "memory.write";

    /// Build the tool with a shared [`MemoryStore`] and the agent id whose
    /// memory file the tool will write.
    pub fn new(store: Arc<MemoryStore>, agent_id: impl Into<String>) -> Self {
        Self {
            store,
            agent_id: agent_id.into(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for MemoryWriteTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn approval_preview(&self, args: &serde_json::Value) -> ApprovalPreview {
        let mode = super::get_optional_str(args, "mode").unwrap_or("");
        let content = super::get_optional_str(args, "content").unwrap_or("");
        // Cap the preview length so a multi-KB body does not flood the
        // approval UI; the full bytes still go through `invoke`.
        let snippet: String = content.chars().take(120).collect();
        let suffix = if content.len() > snippet.len() {
            "…"
        } else {
            ""
        };
        ApprovalPreview {
            description: format!(
                "Write memory for agent '{}' (mode={mode}): {snippet}{suffix}",
                self.agent_id
            ),
        }
    }

    async fn invoke(&self, args: &serde_json::Value, _ctx: &ToolCtx) -> serde_json::Value {
        let content = match get_required_str(args, Self::NAME, "content") {
            Ok(c) => c.to_owned(),
            Err(e) => return serde_json::json!({ "error": e.to_string() }),
        };
        // F-650: cap the content size at the IPC entrypoint so an oversize
        // payload never reaches `spawn_blocking` / disk. The store enforces
        // the same bound for defence-in-depth — the duplicate guard here
        // shaves a thread hop and keeps the wire error message anchored to
        // the tool name. Distinct shape from path-traversal rejections so
        // callers can distinguish the two without string-matching.
        if content.len() > MEMORY_WRITE_CONTENT_CAP {
            return serde_json::json!({
                "error": format!(
                    "tool.{}: content is {} bytes, which exceeds the {}-byte cap",
                    Self::NAME,
                    content.len(),
                    MEMORY_WRITE_CONTENT_CAP,
                )
            });
        }
        let mode_str = match get_required_str(args, Self::NAME, "mode") {
            Ok(m) => m.to_owned(),
            Err(e) => return serde_json::json!({ "error": e.to_string() }),
        };
        let mode = match WriteMode::parse(&mode_str) {
            Ok(m) => m,
            Err(e) => return serde_json::json!({ "error": e.to_string() }),
        };

        let store = Arc::clone(&self.store);
        let agent_id = self.agent_id.clone();
        // F-106: file IO off the tokio worker; mirrors fs.read / fs.write.
        let result =
            tokio::task::spawn_blocking(move || store.write(&agent_id, &content, mode)).await;
        match result {
            Ok(Ok(memory)) => serde_json::json!({
                "ok": true,
                "version": memory.frontmatter.version,
                "updated_at": memory.frontmatter.updated_at.to_rfc3339(),
            }),
            Ok(Err(e)) => serde_json::json!({ "error": e.to_string() }),
            Err(join_err) => serde_json::json!({
                "error": format!("memory.write blocking task failed: {join_err}")
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_agents::MemoryStore;
    use serde_json::json;
    use tempfile::tempdir;

    fn fresh_tool(dir: &std::path::Path, agent_id: &str) -> MemoryWriteTool {
        let store = Arc::new(MemoryStore::new(dir));
        MemoryWriteTool::new(store, agent_id)
    }

    #[tokio::test]
    async fn append_creates_file_and_returns_version_one() {
        let dir = tempdir().unwrap();
        let tool = fresh_tool(dir.path(), "scribe");
        let ctx = ToolCtx::default();
        let result = tool
            .invoke(&json!({"content": "first note", "mode": "append"}), &ctx)
            .await;
        assert_eq!(result["ok"].as_bool(), Some(true));
        assert_eq!(result["version"].as_u64(), Some(1));
        assert!(result["updated_at"].as_str().is_some());
    }

    #[tokio::test]
    async fn replace_then_append_increments_version() {
        let dir = tempdir().unwrap();
        let tool = fresh_tool(dir.path(), "scribe");
        let ctx = ToolCtx::default();
        let r1 = tool
            .invoke(&json!({"content": "v1", "mode": "replace"}), &ctx)
            .await;
        assert_eq!(r1["version"].as_u64(), Some(1));
        let r2 = tool
            .invoke(&json!({"content": "v2", "mode": "append"}), &ctx)
            .await;
        assert_eq!(r2["version"].as_u64(), Some(2));
    }

    #[tokio::test]
    async fn missing_content_yields_unified_error() {
        let dir = tempdir().unwrap();
        let tool = fresh_tool(dir.path(), "scribe");
        let result = tool
            .invoke(&json!({"mode": "append"}), &ToolCtx::default())
            .await;
        assert_eq!(
            result["error"].as_str(),
            Some("tool.memory.write: missing required parameter 'content'")
        );
    }

    #[tokio::test]
    async fn missing_mode_yields_unified_error() {
        let dir = tempdir().unwrap();
        let tool = fresh_tool(dir.path(), "scribe");
        let result = tool
            .invoke(&json!({"content": "hi"}), &ToolCtx::default())
            .await;
        assert_eq!(
            result["error"].as_str(),
            Some("tool.memory.write: missing required parameter 'mode'")
        );
    }

    #[tokio::test]
    async fn unknown_mode_is_rejected() {
        let dir = tempdir().unwrap();
        let tool = fresh_tool(dir.path(), "scribe");
        let result = tool
            .invoke(
                &json!({"content": "hi", "mode": "clobber"}),
                &ToolCtx::default(),
            )
            .await;
        assert!(
            result["error"].as_str().unwrap().contains("'clobber'"),
            "unexpected error shape: {result}"
        );
    }

    /// F-650: tool-level guard rejects oversize content with a clear,
    /// tool-scoped error string distinct from the path-traversal failure
    /// shape (`invalid agent name ...`). Callers can match on
    /// `"exceeds the"` (or the typed `Error::MemoryContentTooLarge` at
    /// the store layer) to distinguish the two failure modes.
    #[tokio::test]
    async fn oversize_content_is_rejected_at_tool_layer() {
        let dir = tempdir().unwrap();
        let tool = fresh_tool(dir.path(), "scribe");
        let oversize = "z".repeat(MEMORY_WRITE_CONTENT_CAP + 1);
        let result = tool
            .invoke(
                &json!({"content": oversize, "mode": "append"}),
                &ToolCtx::default(),
            )
            .await;
        let err = result["error"].as_str().expect("error string present");
        assert!(
            err.starts_with(&format!("tool.{}: ", MemoryWriteTool::NAME)),
            "error must be tool-scoped; got: {err}",
        );
        assert!(
            err.contains("exceeds") && err.contains(&MEMORY_WRITE_CONTENT_CAP.to_string()),
            "error must mention the cap; got: {err}",
        );
        // The error shape must not look like a path-traversal rejection
        // (which begins with `invalid agent name`).
        assert!(
            !err.contains("invalid agent name"),
            "size-cap error must be distinct from path-traversal error; got: {err}",
        );
    }

    /// F-650 boundary: a payload at exactly the cap goes through.
    #[tokio::test]
    async fn content_at_exact_cap_is_accepted() {
        let dir = tempdir().unwrap();
        let tool = fresh_tool(dir.path(), "scribe");
        let payload = "p".repeat(MEMORY_WRITE_CONTENT_CAP);
        let result = tool
            .invoke(
                &json!({"content": payload, "mode": "replace"}),
                &ToolCtx::default(),
            )
            .await;
        assert_eq!(
            result["ok"].as_bool(),
            Some(true),
            "exactly-at-cap must be accepted; got: {result}",
        );
    }

    /// F-650: an oversize rejection at the tool layer must not touch the
    /// filesystem — the guard runs before `spawn_blocking` so no tmp file
    /// is staged, no rename happens, no memory file is created.
    #[tokio::test]
    async fn oversize_content_rejection_does_not_touch_filesystem() {
        let dir = tempdir().unwrap();
        let tool = fresh_tool(dir.path(), "scribe");
        let oversize = "q".repeat(MEMORY_WRITE_CONTENT_CAP + 1);
        let _ = tool
            .invoke(
                &json!({"content": oversize, "mode": "replace"}),
                &ToolCtx::default(),
            )
            .await;
        let memory_root = dir.path().join("forge").join("memory");
        assert!(
            !memory_root.join("scribe.md").exists(),
            "oversize tool call must not create the memory file",
        );
    }

    #[tokio::test]
    async fn approval_preview_includes_agent_id_and_mode() {
        let dir = tempdir().unwrap();
        let tool = fresh_tool(dir.path(), "scribe");
        let preview = tool.approval_preview(&json!({
            "content": "remember to ship",
            "mode": "append",
        }));
        assert!(preview.description.contains("'scribe'"));
        assert!(preview.description.contains("mode=append"));
        assert!(preview.description.contains("remember to ship"));
    }
}
