//! `fs.write` tool: writes a file via [`forge_fs::write`] and returns
//! `{ ok: true }` or `{ error }`. Preview delegates to
//! [`forge_fs::write_preview`].
//!
//! F-106: `forge_fs::write` is synchronous and can block a tokio worker for
//! ~100–200 ms on a 10 MB write, stalling concurrent-session streaming on
//! a shared worker. The write is wrapped in `tokio::task::spawn_blocking`
//! so the stall is confined to the blocking pool.

use super::{get_optional_str, get_required_str, Tool, ToolCtx};
use forge_core::ApprovalPreview;

pub struct FsWriteTool;

impl FsWriteTool {
    pub const NAME: &'static str = "fs.write";
}

#[async_trait::async_trait]
impl Tool for FsWriteTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn approval_preview(&self, args: &serde_json::Value) -> ApprovalPreview {
        // Preview reflects whatever the client sent so the approval UI shows
        // the literal request; `invoke` performs the required-arg check (F-074).
        let path = get_optional_str(args, "path").unwrap_or("");
        let content = get_optional_str(args, "content").unwrap_or("");
        ApprovalPreview {
            description: forge_fs::write_preview(path, content),
        }
    }

    async fn invoke(&self, args: &serde_json::Value, ctx: &ToolCtx) -> serde_json::Value {
        let path = match get_required_str(args, Self::NAME, "path") {
            Ok(p) => p.to_owned(),
            Err(e) => return serde_json::json!({ "error": e.to_string() }),
        };
        let content = match get_required_str(args, Self::NAME, "content") {
            Ok(c) => c.to_owned(),
            Err(e) => return serde_json::json!({ "error": e.to_string() }),
        };
        let allowed_paths = ctx.allowed_paths.clone();
        // F-106: move the synchronous write off the tokio worker.
        let result = tokio::task::spawn_blocking(move || {
            forge_fs::write(
                &path,
                &content,
                &allowed_paths,
                &forge_fs::Limits::default(),
            )
        })
        .await;
        match result {
            Ok(Ok(())) => serde_json::json!({ "ok": true }),
            Ok(Err(e)) => serde_json::json!({ "error": e.to_string() }),
            Err(join_err) => {
                serde_json::json!({ "error": format!("fs.write blocking task failed: {join_err}") })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! F-750: pin the consent text the web client renders for `fs.write`.
    //! `forge_fs::write_preview` decides the shape — the assertions here
    //! verify both that path + content surface through and that empty args
    //! still produce a non-empty description (avoids a silent "blank
    //! preview" regression when a provider streams an incomplete envelope).
    use super::*;
    use serde_json::json;

    #[test]
    fn approval_preview_shows_path_and_content_summary() {
        // Pin the exact wire shape — same DoD contract as the `fs.read`
        // and `shell.exec` tests in this milestone. `forge_fs::write_preview`
        // owns the format string; locking it here means a change there fails
        // this test loudly before it can drift the web client's consent card.
        let preview = FsWriteTool.approval_preview(&json!({
            "path": "/tmp/bar.txt",
            "content": "hello",
        }));
        assert_eq!(
            preview.description,
            "Write file /tmp/bar.txt (5 bytes)\nhello",
        );
    }

    #[test]
    fn approval_preview_tolerates_missing_args() {
        // Same defensive contract as fs.read: render *something* so the
        // approval card surfaces rather than presenting a blank field.
        let preview = FsWriteTool.approval_preview(&json!({}));
        assert!(
            !preview.description.is_empty(),
            "preview must remain non-empty for missing args",
        );
    }
}
