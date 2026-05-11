//! F-741: Dashboard status-bar git branch IPC.
//!
//! Single command — [`git_branch`] — that resolves the active branch name
//! for a workspace root by shelling out to `git rev-parse --abbrev-ref HEAD`.
//! The status bar surfaces the result in its left-slot breadcrumb so the
//! user can read the workspace's current branch without leaving the chrome.
//!
//! # Authorization
//!
//! `git_branch` is dashboard-scoped — only the `dashboard` window label may
//! invoke. Same model as the rest of the workspace-aware IPC surface
//! (`session_start`, `list_active_containers`, etc.).
//!
//! # Behaviour
//!
//! - On success the command returns `GitBranchOutput { branch: Some(name) }`.
//! - On a detached HEAD (`git rev-parse` prints `HEAD`) the command returns
//!   `GitBranchOutput { branch: None }` so the webview can paint its
//!   established `unknown` fallback token rather than the literal string
//!   `HEAD`, which would otherwise leak into the status bar.
//! - On a non-zero exit from git (most commonly: directory is not a git
//!   working tree), the command returns `Err(String)` with the
//!   [`GIT_BRANCH_ERROR`] prefix so the webview falls back to `unknown`.
//!
//! The webview's fallback is the only place "unknown" is rendered — this
//! module returns either a populated branch, an explicit `None`, or a
//! prefixed error string.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[cfg(feature = "webview")]
use tauri::{Runtime, Webview};

/// F-673: command-named error prefix. Every outer error path returned from
/// this command begins with this constant so the dashboard log filter and
/// end-user error display stay consistent across modules.
pub const GIT_BRANCH_ERROR: &str = "git_branch: ";

/// Inbound payload for [`git_branch`].
#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct GitBranchInput {
    pub workspace_root: String,
}

/// Result of [`git_branch`]. `branch` is `None` when the working tree has a
/// detached HEAD; the webview renders the `unknown` token in that case.
#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct GitBranchOutput {
    pub branch: Option<String>,
}

/// Pure trim + detached-HEAD classifier exposed for unit tests so the
/// command body stays a thin wrapper around [`tokio::process::Command`].
pub fn classify_branch_output(stdout: &str) -> Option<String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() || trimmed == "HEAD" {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn git_branch<R: Runtime>(
    input: GitBranchInput,
    webview: Webview<R>,
) -> Result<GitBranchOutput, String> {
    crate::ipc::require_window_label(&webview, "dashboard", "git_branch")?;
    crate::ipc::require_size(
        "workspace_root",
        &input.workspace_root,
        crate::ipc::MAX_WORKSPACE_ROOT_BYTES,
    )?;
    if input.workspace_root.is_empty() {
        return Err(format!("{GIT_BRANCH_ERROR}workspace_root is empty"));
    }

    let output = tokio::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&input.workspace_root)
        .output()
        .await
        .map_err(|e| format!("{GIT_BRANCH_ERROR}command failed: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{GIT_BRANCH_ERROR}command failed: {}",
            stderr.trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(GitBranchOutput {
        branch: classify_branch_output(&stdout),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_branch_output_trims_newline() {
        assert_eq!(classify_branch_output("main\n"), Some("main".to_string()));
    }

    #[test]
    fn classify_branch_output_treats_head_as_detached() {
        assert_eq!(classify_branch_output("HEAD\n"), None);
        assert_eq!(classify_branch_output("HEAD"), None);
    }

    #[test]
    fn classify_branch_output_treats_empty_as_detached() {
        assert_eq!(classify_branch_output(""), None);
        assert_eq!(classify_branch_output("   \n"), None);
    }

    #[test]
    fn classify_branch_output_preserves_slash_names() {
        assert_eq!(
            classify_branch_output("feature/F-741\n"),
            Some("feature/F-741".to_string())
        );
    }
}
