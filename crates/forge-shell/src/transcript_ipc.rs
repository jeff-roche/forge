//! F-607: Tauri command surface for transcript export.
//!
//! Single command — [`export_transcript`] — that returns the raw
//! `<workspace>/.forge/sessions/<session_id>/events.jsonl` bytes for a
//! session. Wraps the existing on-disk event log so the AgentMonitor
//! Inspector (F-449 §9.3 "Export transcript") can pull the transcript
//! through IPC instead of reaching into the filesystem from the webview.
//!
//! # Authorization
//!
//! Callable from either the dashboard window or the matching
//! `session-{id}` window. Other session windows are rejected — a
//! session-A webview has no business reading session-B's transcript.
//!
//! # Path safety
//!
//! `session_id` is validated against the canonical
//! [`forge_core::SessionId`] wire shape (16 lowercase hex chars). Any
//! non-conforming input is rejected before the path is built, which
//! eliminates path-traversal (`../`) and out-of-band characters from the
//! `events.jsonl` lookup. The path itself is composed via
//! [`forge_session::server::event_log_path`], the same resolver the
//! daemon uses, so shell and daemon stay in lock-step on session layout.
//!
//! # Size cap
//!
//! Transcripts are buffered into a `Vec<u8>` and capped at 50 MiB
//! ([`MAX_TRANSCRIPT_BYTES`]). A long-running session can exceed that
//! cap — in which case the command errors instead of allocating
//! unbounded memory or truncating silently. The webview can fall back
//! to reading the file directly through `read_file` (which is sandboxed
//! to the workspace) if it ever needs to stream a larger transcript.
//!
//! # Missing file
//!
//! A session that has not yet written any events (or one whose log was
//! pruned) returns an empty byte vector with a `tracing::warn`. The
//! Inspector can render "no events" instead of surfacing an error to
//! the user.

use std::path::PathBuf;

#[cfg(feature = "webview")]
use tauri::{Runtime, State, Webview};

#[cfg(feature = "webview")]
use crate::ipc::{require_size, BridgeState, LABEL_MISMATCH_ERROR, MAX_WORKSPACE_ROOT_BYTES};

/// Maximum transcript size returned by [`export_transcript`]. Set at 50 MiB
/// — well above any realistic session log (events are typically a few KiB
/// each; a chatty multi-day session lands in the low single-digit MiB) but
/// low enough to bound the Tauri-side allocation a compromised session
/// window can trigger.
pub const MAX_TRANSCRIPT_BYTES: u64 = 50 * 1024 * 1024;

/// Stable error tag returned when the on-disk transcript exceeds
/// [`MAX_TRANSCRIPT_BYTES`]. Tests pin this prefix; preserve it when
/// evolving the message.
pub const TRANSCRIPT_TOO_LARGE_ERROR: &str = "transcript exceeds export cap";

/// Stable error tag returned when reading the on-disk transcript fails for
/// any reason other than "not found".
pub const TRANSCRIPT_READ_ERROR: &str = "failed to read transcript";

/// Pure helper. Reads `path` into a `Vec<u8>`, returning:
///
/// - `Ok(Vec::new())` when the file does not exist (newly-created session
///   that has not yet written its first event).
/// - `Err(_)` when the file's size exceeds `cap_bytes`.
/// - `Err(_)` when any other I/O error occurs.
///
/// Extracted for unit testing without spinning up Tauri.
pub fn read_transcript_bytes(path: &std::path::Path, cap_bytes: u64) -> Result<Vec<u8>, String> {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                target: "forge_shell::transcript_ipc",
                path = %path.display(),
                "transcript file missing; returning empty bytes"
            );
            return Ok(Vec::new());
        }
        Err(e) => {
            return Err(format!("{TRANSCRIPT_READ_ERROR}: {e}"));
        }
    };

    if metadata.len() > cap_bytes {
        return Err(format!(
            "{TRANSCRIPT_TOO_LARGE_ERROR}: {} bytes exceeds {} byte cap",
            metadata.len(),
            cap_bytes
        ));
    }

    std::fs::read(path).map_err(|e| format!("{TRANSCRIPT_READ_ERROR}: {e}"))
}

/// Resolve the absolute path to the events.jsonl for `session_id` under
/// `workspace_root`. Mirrors the daemon's
/// [`forge_session::server::event_log_path`] so shell and daemon stay
/// in lock-step on session layout.
pub fn transcript_path(session_id: &str, workspace_root: &std::path::Path) -> PathBuf {
    forge_session::server::event_log_path(session_id, Some(workspace_root))
}

/// F-607: return the raw `events.jsonl` bytes for `session_id`.
///
/// **Authorization.** Callable from the dashboard window (Inspector use
/// case) or the matching `session-{session_id}` window (in-session
/// transcript export). Any other session window is rejected.
///
/// **Path safety.** `session_id` must match the canonical [`SessionId`]
/// wire shape (16 lowercase hex chars). Path traversal is impossible:
/// `../` / NUL / separator bytes fail validation before the path is
/// built.
///
/// **Workspace resolution.** For a session caller the workspace is taken
/// from the server-side cache populated at `session_hello`. For the
/// dashboard the supplied `workspace_root` is validated against the
/// workspaces registry — the same gate every other dashboard-callable
/// IPC command uses.
///
/// [`SessionId`]: forge_core::SessionId
#[cfg(feature = "webview")]
#[tauri::command]
pub async fn export_transcript<R: Runtime>(
    session_id: String,
    workspace_root: String,
    webview: Webview<R>,
    state: State<'_, BridgeState>,
) -> Result<Vec<u8>, String> {
    // Authorization: dashboard window OR exactly the matching session window.
    // We deliberately do not delegate to `require_window_label_in` because
    // that helper's `allow_session_prefix=true` mode lets *any* `session-*`
    // label through — we need to bind the caller to the *specific*
    // `session-{session_id}` it claims to export.
    let caller_label = webview.label();
    let session_label = format!("session-{session_id}");
    if caller_label != "dashboard" && caller_label != session_label {
        tracing::warn!(
            target: "forge_shell::ipc::authz",
            actual = caller_label,
            expected_dashboard = "dashboard",
            expected_session = %session_label,
            command = "export_transcript",
            "window label mismatch"
        );
        return Err(LABEL_MISMATCH_ERROR.to_string());
    }

    require_size("workspace_root", &workspace_root, MAX_WORKSPACE_ROOT_BYTES)?;

    if !crate::dashboard_sessions::is_valid_session_id(&session_id) {
        return Err(crate::dashboard_sessions::INVALID_SESSION_ID_ERROR.to_string());
    }

    // For a session caller `resolve_workspace_root_for_command` ignores the
    // webview-supplied path and reads the cached value primed by
    // `session_hello`. For the dashboard it canonicalizes the supplied
    // path and verifies it lives in the workspaces registry. Either way
    // the value we feed to `transcript_path` is server-trusted.
    let workspace =
        crate::ipc::resolve_workspace_root_for_command(caller_label, &workspace_root, &state)
            .await?;
    let path = transcript_path(&session_id, &workspace);
    read_transcript_bytes(&path, MAX_TRANSCRIPT_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn read_returns_bytes_verbatim_for_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.jsonl");
        let payload = b"{\"event\":\"hello\"}\n{\"event\":\"world\"}\n";
        std::fs::write(&path, payload).unwrap();

        let got = read_transcript_bytes(&path, MAX_TRANSCRIPT_BYTES).unwrap();
        assert_eq!(got, payload.to_vec());
    }

    #[test]
    fn read_returns_empty_when_file_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        let got = read_transcript_bytes(&path, MAX_TRANSCRIPT_BYTES).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn read_errors_when_file_exceeds_cap() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.jsonl");
        let cap: u64 = 1024;
        let mut f = std::fs::File::create(&path).unwrap();
        // Write cap + 1 bytes.
        let chunk = vec![b'x'; (cap as usize) + 1];
        f.write_all(&chunk).unwrap();
        drop(f);

        let err = read_transcript_bytes(&path, cap).expect_err("expected oversize error");
        assert!(
            err.starts_with(TRANSCRIPT_TOO_LARGE_ERROR),
            "got error: {err}"
        );
    }

    #[test]
    fn transcript_path_matches_daemon_layout() {
        let workspace = std::path::Path::new("/tmp/ws");
        let path = transcript_path("deadbeefcafebabe", workspace);
        assert_eq!(
            path,
            std::path::Path::new("/tmp/ws/.forge/sessions/deadbeefcafebabe/events.jsonl")
        );
    }
}
