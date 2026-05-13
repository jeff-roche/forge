//! F-748: Tauri command surface for restarting a crashed `forged` daemon
//! while preserving its session id and event log.
//!
//! Triggered by the SessionWindow's crash-restart overlay after the bridge
//! detects a daemon UDS pump error (EOF / ECONNRESET / framing error). The
//! command:
//!
//! 1. drops any stale connection state from the bridge registry (the prior
//!    daemon's writer/reader pair, the cached workspace root, the
//!    background pump task),
//! 2. re-spawns `forged` with the SAME `FORGE_SESSION_ID` via
//!    [`forge_cli::spawn::spawn_forged_session_with_id`] so the daemon
//!    resumes the persisted event log under
//!    `<workspace>/.forge/sessions/<id>/events.jsonl` via
//!    [`forge_session::session::Session::resume`] instead of truncating
//!    it,
//! 3. returns once the new daemon's UDS appears so the webview can
//!    immediately call `sessionHello` and `sessionSubscribe { since:
//!    last_seq }` — that re-subscription is what produces the
//!    no-duplicate-events guarantee on the daemon side (history replay
//!    starts after the webview's last observed seq, the daemon never
//!    re-emits events from the persisted log).
//!
//! Workspace registration (the first-spawn responsibility per F-725) is
//! intentionally skipped: the workspace is still registered from the
//! original session start; F-748 only re-spawns the daemon.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[cfg(feature = "webview")]
use tauri::{Runtime, State, Webview};

#[cfg(feature = "webview")]
use crate::ipc::BridgeState;

/// F-673: command-named error prefix.
pub const SESSION_RESTART_ERROR: &str = "session_restart: ";

#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct SessionRestartInput {
    pub session_id: String,
    /// Workspace root the daemon should bind under. The webview resends it
    /// from its cached HelloAck — the shell does not maintain a workspace
    /// cache outside the live bridge connection, which is exactly the state
    /// `session_restart` is rebuilding. An empty value is rejected with a
    /// canonical `SESSION_RESTART_ERROR` prefix.
    pub workspace_root: String,
    /// Optional agent / provider triple identical to `session_start`. The
    /// webview re-supplies whatever it bound the original session with so
    /// the re-spawned daemon picks up the same orchestrator + provider
    /// rather than reverting to defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct SessionRestartOutput {
    pub session_id: String,
}

/// Pure validation of `SessionRestartInput`. Exposed so tests can exercise
/// the rejection paths without standing up a Tauri runtime.
pub fn validate_session_restart_input(input: &SessionRestartInput) -> Result<(), String> {
    if input.session_id.is_empty() {
        return Err(format!("{SESSION_RESTART_ERROR}session_id is empty"));
    }
    if !crate::dashboard_sessions::is_valid_session_id(&input.session_id) {
        return Err(format!("{SESSION_RESTART_ERROR}invalid session_id"));
    }
    if input.workspace_root.is_empty() {
        return Err(format!("{SESSION_RESTART_ERROR}workspace_root is empty"));
    }
    Ok(())
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn session_restart<R: Runtime>(
    input: SessionRestartInput,
    webview: Webview<R>,
    state: State<'_, BridgeState>,
) -> Result<SessionRestartOutput, String> {
    // Authorization: only the matching `session-<id>` window can ask its
    // own daemon to be re-spawned. The dashboard does not own this path —
    // crash-restart is a session-window-local affordance.
    let window_label = format!("session-{}", input.session_id);
    crate::ipc::require_window_label(&webview, &window_label, "session_restart")
        .map_err(|e| format!("{SESSION_RESTART_ERROR}{e}"))?;

    validate_session_restart_input(&input)?;

    run_session_restart(
        &state.bridge,
        &input.session_id,
        &input.workspace_root,
        input.agent.as_deref(),
        input.provider.as_deref(),
    )
    .await
}

/// Production restart body factored out for tests. Drops the stale bridge
/// connection first so a second `session_hello` from the webview after
/// restart does not see "already connected" — the original connection's
/// pump task has already exited (that's the crash signal that brought us
/// here), but the registry entry still holds the dead writer half until
/// we explicitly drop it.
pub async fn run_session_restart(
    bridge: &crate::bridge::SessionBridge,
    session_id: &str,
    workspace_root: &str,
    agent: Option<&str>,
    provider: Option<&str>,
) -> Result<SessionRestartOutput, String> {
    bridge.drop_connection(session_id).await;

    let workspace = std::path::Path::new(workspace_root);
    let spawned = forge_cli::spawn::spawn_forged_session_with_id(
        workspace,
        agent,
        provider,
        Some(session_id),
    )
    .await
    .map_err(|e| format!("{SESSION_RESTART_ERROR}{e}"))?;

    Ok(SessionRestartOutput {
        session_id: spawned.session_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(session_id: &str, workspace: &str) -> SessionRestartInput {
        SessionRestartInput {
            session_id: session_id.to_string(),
            workspace_root: workspace.to_string(),
            agent: None,
            provider: None,
        }
    }

    #[test]
    fn error_prefix_is_command_named() {
        assert_eq!(SESSION_RESTART_ERROR, "session_restart: ");
    }

    #[test]
    fn validate_rejects_empty_session_id() {
        let err = validate_session_restart_input(&input("", "/tmp/ws")).unwrap_err();
        assert!(err.starts_with(SESSION_RESTART_ERROR));
        assert!(err.contains("session_id"));
    }

    #[test]
    fn validate_rejects_empty_workspace_root() {
        // Pick a session id with the canonical shape so the validator
        // reaches the workspace check.
        let err = validate_session_restart_input(&input("deadbeefcafebabe", "")).unwrap_err();
        assert!(err.starts_with(SESSION_RESTART_ERROR));
        assert!(err.contains("workspace_root"));
    }

    #[test]
    fn validate_rejects_malformed_session_id() {
        let err = validate_session_restart_input(&input("../escape", "/tmp/ws")).unwrap_err();
        assert!(err.starts_with(SESSION_RESTART_ERROR));
        assert!(err.contains("invalid"));
    }

    #[test]
    fn validate_accepts_canonical_pair() {
        validate_session_restart_input(&input("deadbeefcafebabe", "/tmp/ws"))
            .expect("canonical input passes");
    }
}
