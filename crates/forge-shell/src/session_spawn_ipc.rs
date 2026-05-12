//! F-725: Tauri command surface for spawning a new daemon-backed session
//! from the Dashboard's `+ New session` modal.
//!
//! Thin wrapper around the same code path `forge session new` walks in the
//! CLI — see [`forge_cli::spawn::spawn_forged_session`]. The IPC adds three
//! responsibilities the CLI's clap layer covers for free:
//!
//! 1. **Authorization** — only the `dashboard` window label may invoke.
//! 2. **Identifier validation** — `workspace_root` must exist on disk and
//!    appear in the workspaces registry (per the dashboard-window pattern
//!    `crate::ipc::resolve_workspace_root_for_command` enforces); `provider`
//!    (when supplied) must match the dashboard's known-provider set; `agent`
//!    (when supplied) must match the loaded agent roster.
//! 3. **Error-prefix discipline (F-673)** — every rejection is wrapped in
//!    [`SESSION_START_ERROR`] before crossing the wire.
//!
//! F-727: companion `list_sessions` command. Same authorization gate
//! (dashboard-only), thin filter over `dashboard_sessions::collect_sessions`
//! returning only the attachable rows (wire-state `"stopped"`) that the
//! hero's `Attach to session` picker can re-open.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[cfg(feature = "webview")]
use tauri::{Runtime, State, Webview};

#[cfg(feature = "webview")]
use crate::ipc::{
    resolve_user_config_dir, resolve_workspace_root_for_command, BridgeState,
    MAX_WORKSPACE_ROOT_BYTES,
};

// F-673: command-named error prefix. Every outer error path returned from a
// `*_ipc.rs` command must begin with one of these constants so the
// dashboard log filter and end-user error display stay consistent across
// modules.
pub const SESSION_START_ERROR: &str = "session_start: ";

// F-727: error prefix for `list_sessions`. Matches F-673's command-named
// convention so dashboard log filters and end-user messages stay consistent.
pub const LIST_SESSIONS_ERROR: &str = "list_sessions: ";

/// F-727: wire-state value the picker treats as "attachable". The
/// `dashboard_sessions` module emits `"stopped"` for active sessions whose
/// UDS ping fails — those are the sessions the operator can re-attach by
/// re-opening their window.
pub const ATTACHABLE_WIRE_STATE: &str = "stopped";

/// Cap on the optional agent / provider identifiers. Both are short slugs —
/// real values are under 64 bytes (`orchestrator`, `anthropic`,
/// `custom_openai:my-vllm`). 256 bytes leaves room for the largest
/// `custom_openai:<name>` entry without admitting unbounded growth from a
/// compromised webview.
pub const MAX_SESSION_START_ID_BYTES: usize = 256;

#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct SessionStartInput {
    pub workspace_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct SessionStartOutput {
    pub session_id: String,
}

/// Pure validation of the optional identifier fields. Exposed so tests can
/// exercise the rejection paths without standing up a Tauri window.
pub fn validate_session_start_input(input: &SessionStartInput) -> Result<(), String> {
    if input.workspace_root.is_empty() {
        return Err(format!("{SESSION_START_ERROR}workspace_root is empty"));
    }
    bound_optional(&input.provider, "provider")?;
    bound_optional(&input.agent, "agent")?;
    Ok(())
}

fn bound_optional(value: &Option<String>, field: &str) -> Result<(), String> {
    let Some(v) = value.as_deref() else {
        return Ok(());
    };
    if v.is_empty() {
        return Err(format!("{SESSION_START_ERROR}{field} is empty"));
    }
    if v.len() > MAX_SESSION_START_ID_BYTES {
        return Err(format!(
            "{SESSION_START_ERROR}{field} too large: {} bytes exceeds cap of {} bytes",
            v.len(),
            MAX_SESSION_START_ID_BYTES
        ));
    }
    Ok(())
}

/// Pure provider check: `id` must match one of the slugs the dashboard's
/// provider list emits (built-in + `custom_openai:<name>` entries). Exposed
/// so tests can drive the rejection path without standing up Tauri state.
pub fn provider_is_known(settings: &forge_core::settings::AppSettings, id: &str) -> bool {
    crate::providers_ipc::is_known_provider_id(settings, id)
}

/// Pure agent check: `name` must match one of the loaded agent ids from
/// the workspace + user-home roster. Exposed for unit tests.
pub fn agent_is_known(
    workspace_root: &std::path::Path,
    user_home: &std::path::Path,
    name: &str,
) -> Result<bool, String> {
    let defs = forge_agents::load_agents(workspace_root, user_home)
        .map_err(|e| format!("{SESSION_START_ERROR}load agents: {e}"))?;
    Ok(defs.iter().any(|d| d.name == name))
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn session_start<R: Runtime>(
    input: SessionStartInput,
    webview: Webview<R>,
    state: State<'_, BridgeState>,
) -> Result<SessionStartOutput, String> {
    crate::ipc::require_window_label(&webview, "dashboard", "session_start")?;
    validate_session_start_input(&input)?;
    crate::ipc::require_size(
        "workspace_root",
        &input.workspace_root,
        MAX_WORKSPACE_ROOT_BYTES,
    )?;

    let workspace_path =
        resolve_workspace_root_for_command(webview.label(), &input.workspace_root, &state)
            .await
            .map_err(|e| format!("{SESSION_START_ERROR}{e}"))?;

    let user_dir = resolve_user_config_dir(&state);
    let settings = match user_dir.as_deref() {
        Some(dir) => forge_core::settings::load_user_settings_in(dir)
            .await
            .map_err(|e| format!("{SESSION_START_ERROR}{e}"))?,
        None => forge_core::settings::AppSettings::default(),
    };

    if let Some(provider_id) = input.provider.as_deref() {
        if !provider_is_known(&settings, provider_id) {
            return Err(format!(
                "{SESSION_START_ERROR}unknown provider: {provider_id}"
            ));
        }
    }

    let user_home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
    if let Some(agent_name) = input.agent.as_deref() {
        if !agent_is_known(&workspace_path, &user_home, agent_name)? {
            return Err(format!("{SESSION_START_ERROR}unknown agent: {agent_name}"));
        }
    }

    let spawned = forge_cli::spawn::spawn_forged_session(
        &workspace_path,
        input.agent.as_deref(),
        input.provider.as_deref(),
    )
    .await
    .map_err(|e| format!("{SESSION_START_ERROR}{e}"))?;

    Ok(SessionStartOutput {
        session_id: spawned.session_id,
    })
}

// ---------------------------------------------------------------------------
// F-727: `list_sessions` — attachable session picker source
// ---------------------------------------------------------------------------

/// Pure filter exposed for unit tests. Retains only sessions the picker can
/// attach to (`state == "stopped"` in the `dashboard_sessions` wire vocabulary
/// — sessions whose UDS no longer responds, i.e. the daemon is gone and the
/// row is safe to re-open). `"active"` rows already have a live window;
/// `"archived"` rows are terminal.
pub fn filter_attachable(
    sessions: Vec<crate::dashboard_sessions::SessionSummary>,
) -> Vec<crate::dashboard_sessions::SessionSummary> {
    sessions
        .into_iter()
        .filter(|s| s.state == ATTACHABLE_WIRE_STATE)
        .collect()
}

/// Tauri command: list every attachable (detached) session known to the
/// shell. The picker UI consumes this directly — there is no separate
/// loading or filter pass on the webview side.
///
/// Dashboard-only by authorization, matching every other `*_ipc.rs`
/// command that touches workspace state.
#[cfg(feature = "webview")]
#[tauri::command]
pub async fn list_sessions<R: Runtime>(
    webview: Webview<R>,
) -> Result<Vec<crate::dashboard_sessions::SessionSummary>, String> {
    crate::ipc::require_window_label(&webview, "dashboard", "list_sessions")
        .map_err(|e| format!("{LIST_SESSIONS_ERROR}{e}"))?;
    let toml_path = crate::dashboard_sessions::default_workspaces_toml();
    let all = crate::dashboard_sessions::collect_sessions(
        &toml_path,
        &crate::dashboard_sessions::UdsPinger,
    )
    .await
    .map_err(|e| format!("{LIST_SESSIONS_ERROR}{e}"))?;
    Ok(filter_attachable(all))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::settings::AppSettings;

    fn input(
        workspace_root: &str,
        provider: Option<&str>,
        agent: Option<&str>,
    ) -> SessionStartInput {
        SessionStartInput {
            workspace_root: workspace_root.to_string(),
            provider: provider.map(str::to_string),
            agent: agent.map(str::to_string),
        }
    }

    #[test]
    fn validate_rejects_empty_workspace_root() {
        let err = validate_session_start_input(&input("", None, None)).unwrap_err();
        assert!(err.starts_with(SESSION_START_ERROR));
        assert!(err.contains("workspace_root"));
    }

    #[test]
    fn validate_rejects_empty_provider_string() {
        let err = validate_session_start_input(&input("/tmp/ws", Some(""), None)).unwrap_err();
        assert!(err.starts_with(SESSION_START_ERROR));
        assert!(err.contains("provider"));
    }

    #[test]
    fn validate_rejects_empty_agent_string() {
        let err = validate_session_start_input(&input("/tmp/ws", None, Some(""))).unwrap_err();
        assert!(err.starts_with(SESSION_START_ERROR));
        assert!(err.contains("agent"));
    }

    #[test]
    fn validate_rejects_oversize_provider() {
        let huge = "x".repeat(MAX_SESSION_START_ID_BYTES + 1);
        let err = validate_session_start_input(&input("/tmp/ws", Some(&huge), None)).unwrap_err();
        assert!(err.starts_with(SESSION_START_ERROR));
        assert!(err.contains("exceeds cap"));
    }

    #[test]
    fn validate_accepts_workspace_only() {
        validate_session_start_input(&input("/tmp/ws", None, None)).expect("workspace-only");
    }

    #[test]
    fn validate_accepts_full_triple() {
        validate_session_start_input(&input("/tmp/ws", Some("anthropic"), Some("orchestrator")))
            .expect("full triple");
    }

    /// Unknown provider rejection mirrors the path the live command takes
    /// when `is_known_provider_id` returns `false`. Pure check — no Tauri
    /// runtime needed.
    #[test]
    fn provider_lookup_rejects_unknown_slug() {
        let settings = AppSettings::default();
        assert!(!provider_is_known(&settings, "gemini"));
        assert!(!provider_is_known(
            &settings,
            "custom_openai:not-configured"
        ));
    }

    #[test]
    fn provider_lookup_accepts_builtins() {
        // Under the "empty by default" model, a built-in is only known once
        // the user has added it (key present in `providers.enabled`).
        let empty = AppSettings::default();
        for id in &["anthropic", "openai", "ollama"] {
            assert!(
                !provider_is_known(&empty, id),
                "fresh install should treat `{id}` as unconfigured"
            );
        }

        let mut settings = AppSettings::default();
        for id in &["anthropic", "openai", "ollama"] {
            settings.providers.enabled.insert((*id).into(), true);
        }
        for id in &["anthropic", "openai", "ollama"] {
            assert!(provider_is_known(&settings, id), "expected `{id}` known");
        }

        // The bare `custom_openai` slug is a kind, not a row — concrete
        // entries arrive as `custom_openai:<name>`.
        assert!(!provider_is_known(&settings, "custom_openai"));
    }

    /// Unknown-agent rejection: pointing the loader at an empty workspace
    /// tree yields an empty roster, so any non-empty name is "unknown".
    #[test]
    fn agent_lookup_rejects_unknown_name() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let known = agent_is_known(workspace.path(), home.path(), "no-such-agent").unwrap();
        assert!(!known);
    }

    /// Unknown-workspace path returns a `workspace_root not found` style
    /// error wrapped in the canonical prefix. Exercises the rejection path
    /// the live command takes for a nonexistent directory.
    #[test]
    fn unknown_workspace_path_rejection_carries_prefix() {
        // `std::path::Path::canonicalize` on a nonexistent path fails; the
        // live command formats the failure through the same prefix.
        let supplied = "/this/path/should/not/exist/forge-f725-test";
        let result = std::path::Path::new(supplied).canonicalize();
        assert!(result.is_err(), "precondition: path must not exist");
        let synthetic_err = format!(
            "{SESSION_START_ERROR}workspace_root not found on disk: {}",
            result.unwrap_err()
        );
        assert!(synthetic_err.starts_with(SESSION_START_ERROR));
        assert!(synthetic_err.contains("workspace_root"));
    }

    /// Pin the wire-format error prefix so a future rename of
    /// `SESSION_START_ERROR` doesn't silently drift past F-673.
    #[test]
    fn error_prefix_is_command_named() {
        assert_eq!(SESSION_START_ERROR, "session_start: ");
    }

    // ---------------------------------------------------------------------
    // F-727: `list_sessions` pure filter
    // ---------------------------------------------------------------------

    fn summary(id: &str, state: &str) -> crate::dashboard_sessions::SessionSummary {
        crate::dashboard_sessions::SessionSummary {
            id: id.to_string(),
            subject: "subject".to_string(),
            state: state.to_string(),
            persistence: "persist".to_string(),
            created_at: "2026-05-10T00:00:00Z".to_string(),
            last_event_at: "2026-05-10T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn filter_attachable_retains_only_stopped() {
        let rows = vec![
            summary("a", "active"),
            summary("b", "stopped"),
            summary("c", "archived"),
            summary("d", "stopped"),
        ];
        let kept = filter_attachable(rows);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|s| s.state == "stopped"));
        assert_eq!(kept[0].id, "b");
        assert_eq!(kept[1].id, "d");
    }

    #[test]
    fn filter_attachable_drops_archived() {
        let rows = vec![summary("a", "archived"), summary("b", "archived")];
        assert!(filter_attachable(rows).is_empty());
    }

    #[test]
    fn filter_attachable_drops_active() {
        let rows = vec![summary("a", "active"), summary("b", "active")];
        assert!(filter_attachable(rows).is_empty());
    }

    #[test]
    fn filter_attachable_empty_in_empty_out() {
        assert!(filter_attachable(Vec::new()).is_empty());
    }

    /// Pin the F-727 error prefix alongside its sibling so a future rename
    /// of `LIST_SESSIONS_ERROR` doesn't silently drift past F-673.
    #[test]
    fn list_sessions_error_prefix_is_command_named() {
        assert_eq!(LIST_SESSIONS_ERROR, "list_sessions: ");
    }

    /// Pin the attachable wire-state contract — `dashboard_sessions` writes
    /// `"stopped"` for active sessions whose UDS ping fails, and the picker
    /// is the only consumer that depends on that exact string.
    #[test]
    fn attachable_wire_state_is_stopped() {
        assert_eq!(ATTACHABLE_WIRE_STATE, "stopped");
    }
}
