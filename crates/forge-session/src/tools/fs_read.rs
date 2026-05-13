//! `fs.read` tool: reads a file via [`forge_fs::read_file`] and returns
//! `{ content, bytes, sha256 }` or `{ error }`.
//!
//! F-106: `forge_fs::read_file` is deliberately synchronous. Running it on
//! a tokio worker blocks the worker for the duration of a potentially
//! large read (10 MB ≈ 50–100 ms), stalling streaming in concurrent
//! sessions that share the worker. The call is wrapped in
//! `tokio::task::spawn_blocking` so only the blocking-pool thread stalls.

use super::{get_required_str, Tool, ToolCtx};
use forge_core::ApprovalPreview;

pub struct FsReadTool;

impl FsReadTool {
    pub const NAME: &'static str = "fs.read";
}

#[async_trait::async_trait]
impl Tool for FsReadTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    /// F-599 follow-up (#647): `fs.read` never mutates external state, so
    /// the orchestrator can auto-approve it (logging `ApprovalSource::Auto`
    /// in place of an interactive prompt) and dispatch consecutive reads in
    /// the parallel branch alongside other read-only tools.
    fn read_only(&self) -> bool {
        true
    }

    fn approval_preview(&self, args: &serde_json::Value) -> ApprovalPreview {
        // Preview shows whatever the client sent (including blank) so the user
        // sees exactly what was requested before approval. Required-argument
        // validation runs in `invoke` only — no point rejecting a malformed
        // call until the user has had a chance to refuse it. (F-074)
        let path = super::get_optional_str(args, "path").unwrap_or("");
        ApprovalPreview {
            description: format!("Read file '{path}'"),
        }
    }

    async fn invoke(&self, args: &serde_json::Value, ctx: &ToolCtx) -> serde_json::Value {
        let path = match get_required_str(args, Self::NAME, "path") {
            Ok(p) => p.to_owned(),
            Err(e) => return serde_json::json!({ "error": e.to_string() }),
        };
        let allowed_paths = ctx.allowed_paths.clone();
        // F-106: move the synchronous read off the tokio worker.
        let result = tokio::task::spawn_blocking(move || {
            forge_fs::read_file(&path, &allowed_paths, &forge_fs::Limits::default())
        })
        .await;
        match result {
            Ok(Ok(r)) => serde_json::json!({
                "content": r.content,
                "bytes": r.bytes,
                "sha256": r.sha256,
            }),
            Ok(Err(e)) => serde_json::json!({ "error": e.to_string() }),
            Err(join_err) => {
                serde_json::json!({ "error": format!("fs.read blocking task failed: {join_err}") })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! F-750: lock the `approval_preview` wire-shape so the front-end
    //! tool-approval card renders identically regardless of which provider
    //! (Ollama / Anthropic / OpenAI) emitted the call. The web client
    //! consumes `description` verbatim — drift here means a regression in
    //! the user-visible consent text.
    use super::*;
    use serde_json::json;

    #[test]
    fn approval_preview_shows_path() {
        let preview = FsReadTool.approval_preview(&json!({ "path": "/tmp/foo.txt" }));
        assert_eq!(preview.description, "Read file '/tmp/foo.txt'");
    }

    #[test]
    fn approval_preview_tolerates_missing_path() {
        // Provider tool-call envelopes occasionally arrive empty (Anthropic
        // streams `input_json_delta`s; a truncated stream can leave args
        // empty). The preview must still render so the user sees the consent
        // prompt rather than a blank card.
        let preview = FsReadTool.approval_preview(&json!({}));
        assert_eq!(preview.description, "Read file ''");
    }
}
