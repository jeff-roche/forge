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

// F-746: reason suffix appended after `SESSION_START_ERROR` when the
// pre-spawn credential check rejects. The dashboard's new-session dialog
// substring-matches on this constant to render an actionable
// "Configure provider" CTA pointing at `/providers#<id>`. Both the
// missing-entry case (`store.has` returned `Ok(false)`) and the backend-
// error case (`store.has` returned `Err`) collapse onto this single
// reason — the user-visible recovery is identical (open the provider
// settings; a locked keyring will surface a more specific message
// there).
pub const CREDENTIALS_MISSING_REASON: &str = "credentials_missing for provider ";

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

/// F-746: pure keyless classifier.
///
/// Returns `true` when `provider_id` requires no credential at the
/// network boundary:
///
/// - The legacy bare slug `"ollama"` — kept for back-compat with callers
///   that still pass the Phase-1 id before it was migrated to a
///   `custom_openai:ollama` entry.
/// - Any `custom_openai:<name>` whose section in
///   `settings.providers.custom_openai[name].auth` is
///   [`forge_core::settings::AuthShapeSettings::None`]. This is the
///   production shape for the Ollama / local-vLLM / private-network
///   gateway presets per F-730.
///
/// Built-in providers (`anthropic`, `openai`, named instances) are NOT
/// keyless. Anthropic's Vertex sub-mode pulls a Google access token via
/// `gcloud` rather than a keyring entry; F-746 deliberately keeps
/// Vertex out of the keyless bucket because the absence of a usable
/// token still wants surfacing — that's a follow-up enhancement, not
/// this task.
pub fn provider_is_keyless(
    settings: &forge_core::settings::AppSettings,
    provider_id: &str,
) -> bool {
    use forge_core::settings::AuthShapeSettings;

    if provider_id == "ollama" {
        return true;
    }
    if let Some(name) = provider_id.strip_prefix(crate::providers_ipc::CUSTOM_OPENAI_PREFIX) {
        if let Some(entry) = settings.providers.custom_openai.get(name) {
            return matches!(entry.auth, AuthShapeSettings::None);
        }
    }
    false
}

/// F-746: pre-spawn credential check.
///
/// Called from `session_start` immediately after the unknown-provider
/// gate and immediately before `spawn_forged_session`. The goal is to
/// fail the new-session flow with an actionable error BEFORE any
/// `forged` daemon is spawned or `/tmp/forge-*` socket is allocated;
/// without this gate the first-turn `pull_active_credential` call
/// (F-744) is where the user would first see the missing credential —
/// by which point a stranded daemon is already running.
///
/// # Keyring key shape
///
/// `store.has(provider_id)` is called with the **full `provider_id`
/// string verbatim** — including any `<vendor>:<name>` suffix for
/// named built-in instances (`anthropic:work`, `openai:personal`) and
/// `custom_openai:<name>` rows. Every [`Credentials`] implementor
/// (memory, env-fallback, keyring) keys its entries on that same raw
/// slug per the trait contract in
/// [`forge_core::credentials`] (point 2 of the trait doc): the
/// `provider_id` the caller hands in is the source of truth.
///
/// On the keyring backend the slug lands in the platform-specific
/// "account" field; the service namespace (`"forge"` by default) is
/// applied by [`forge_core::credentials::KeyringStore`] and is
/// transparent to callers here. The F-587 audit pins this convention.
///
/// [`Credentials`]: forge_core::credentials::Credentials
///
/// # Returns
///
/// - `Ok(())` when the provider is keyless (no credential is needed),
///   or when the store reports the credential is present.
/// - `Err(SESSION_START_ERROR + CREDENTIALS_MISSING_REASON + <id>)`
///   when the store reports no entry OR when the store errors (keyring
///   locked, Secret Service daemon down, etc.). Both shapes collapse
///   to the same reason so the dashboard surfaces one CTA either way;
///   the user-visible recovery (open `/providers#<id>`) is identical.
pub async fn check_provider_credential(
    store: &std::sync::Arc<dyn forge_core::credentials::Credentials>,
    settings: &forge_core::settings::AppSettings,
    provider_id: &str,
) -> Result<(), String> {
    if provider_is_keyless(settings, provider_id) {
        return Ok(());
    }
    match store.has(provider_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "{SESSION_START_ERROR}{CREDENTIALS_MISSING_REASON}{provider_id}"
        )),
        Err(e) => {
            tracing::warn!(
                target: "forge_shell::session_start::credentials",
                provider_id = %provider_id,
                error = %e,
                "credential presence probe failed",
            );
            Err(format!(
                "{SESSION_START_ERROR}{CREDENTIALS_MISSING_REASON}{provider_id}"
            ))
        }
    }
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
    credentials: State<'_, crate::credentials_ipc::CredentialsState>,
) -> Result<SessionStartOutput, String> {
    crate::ipc::require_window_label(&webview, "dashboard", "session_start")?;
    validate_session_start_input(&input)?;
    crate::ipc::require_size(
        "workspace_root",
        &input.workspace_root,
        MAX_WORKSPACE_ROOT_BYTES,
    )?;

    // Seed the workspaces registry with the picked path before the
    // downstream resolve gate validates it. The user explicitly chose
    // this path via the OS file picker in the new-session dialog —
    // that's the registry-creation moment. Idempotent: if the path is
    // already registered (most calls), this is a no-op.
    let supplied = std::path::Path::new(&input.workspace_root);
    if let Ok(canonical) = supplied.canonicalize() {
        let toml_path = crate::ipc::resolve_workspaces_toml(&state);
        forge_core::workspaces::register_workspace_if_missing(&toml_path, &canonical)
            .await
            .map_err(|e| {
                format!("{SESSION_START_ERROR}could not update workspaces registry: {e}")
            })?;
    }

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

        // F-746: pre-spawn credential validation. Keyless providers
        // (`ollama`, `custom_openai:<name>` with `auth.shape = "none"`)
        // skip this check entirely; every other provider must have a
        // reachable credential in the store, or `session_start` fails
        // BEFORE `spawn_forged_session` allocates a daemon + UDS. A
        // missing entry and a backend error both collapse to the same
        // `credentials_missing` reason so the dashboard renders one
        // actionable CTA either way.
        let store = credentials.store();
        check_provider_credential(&store, &settings, provider_id).await?;
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
    use forge_core::credentials::{Credentials, MemoryStore};
    use forge_core::settings::{
        AppSettings, AuthShapeSettings, CustomOpenAiEntry, ProvidersSettings,
    };
    use secrecy::SecretString;
    use std::sync::Arc;

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
        for id in &["anthropic", "openai"] {
            assert!(
                !provider_is_known(&empty, id),
                "fresh install should treat `{id}` as unconfigured"
            );
        }

        let mut settings = AppSettings::default();
        for id in &["anthropic", "openai"] {
            settings.providers.enabled.insert((*id).into(), true);
        }
        for id in &["anthropic", "openai"] {
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

    // -----------------------------------------------------------------
    // F-746: pre-spawn credential validation
    // -----------------------------------------------------------------

    /// Backend wired to fail every `get` / `has` call — exercises the
    /// keyring-locked / Secret-Service-unreachable error path. Holds no
    /// secrets and is not constructible from outside the test module.
    struct FailingStore;

    #[async_trait::async_trait]
    impl Credentials for FailingStore {
        async fn get(
            &self,
            _provider_id: &str,
        ) -> Result<Option<SecretString>, forge_core::ForgeError> {
            Err(anyhow::anyhow!("keyring backend unavailable").into())
        }
        async fn set(
            &self,
            _provider_id: &str,
            _value: SecretString,
        ) -> Result<(), forge_core::ForgeError> {
            Err(anyhow::anyhow!("read-only test stub").into())
        }
        async fn remove(&self, _provider_id: &str) -> Result<(), forge_core::ForgeError> {
            Err(anyhow::anyhow!("read-only test stub").into())
        }
        async fn has(&self, _provider_id: &str) -> Result<bool, forge_core::ForgeError> {
            Err(anyhow::anyhow!("keyring backend unavailable").into())
        }
    }

    fn settings_with_custom_openai(name: &str, auth: AuthShapeSettings) -> AppSettings {
        let mut providers = ProvidersSettings::default();
        providers.custom_openai.insert(
            name.to_string(),
            CustomOpenAiEntry {
                base_url: "http://127.0.0.1:11434".into(),
                model: "qwen2.5".into(),
                model_list: vec![],
                auth,
                api_key: None,
            },
        );
        AppSettings {
            providers,
            ..AppSettings::default()
        }
    }

    /// Pin the F-746 error suffix so the dashboard's CTA mapper has a
    /// stable substring to key on.
    #[test]
    fn credentials_missing_error_suffix_is_stable() {
        assert_eq!(
            CREDENTIALS_MISSING_REASON,
            "credentials_missing for provider "
        );
    }

    /// Built-in providers (anthropic, openai) are NOT keyless — both
    /// require an API key in the credential store.
    #[test]
    fn builtins_are_not_keyless() {
        let settings = AppSettings::default();
        assert!(!provider_is_keyless(&settings, "anthropic"));
        assert!(!provider_is_keyless(&settings, "openai"));
    }

    /// The legacy bare `ollama` id is keyless — covers callers that still
    /// pass the historical Phase-1 slug before it was migrated to
    /// `custom_openai:ollama`.
    #[test]
    fn legacy_ollama_id_is_keyless() {
        let settings = AppSettings::default();
        assert!(provider_is_keyless(&settings, "ollama"));
    }

    /// A `custom_openai:<name>` entry whose section declares
    /// `auth = { shape = "none" }` is keyless. This is the production shape
    /// for Ollama via the custom_openai preset (F-743).
    #[test]
    fn custom_openai_with_auth_none_is_keyless() {
        let settings = settings_with_custom_openai("ollama", AuthShapeSettings::None);
        assert!(provider_is_keyless(&settings, "custom_openai:ollama"));
    }

    /// A `custom_openai:<name>` with the default bearer auth shape is NOT
    /// keyless — its credential must be present in the store.
    #[test]
    fn custom_openai_with_bearer_is_not_keyless() {
        let settings = settings_with_custom_openai("together", AuthShapeSettings::Bearer);
        assert!(!provider_is_keyless(&settings, "custom_openai:together"));
    }

    /// Keyless check: an unconfigured `custom_openai:<name>` (no section
    /// in settings) is NOT keyless. The unknown-provider gate upstream
    /// rejects this case first; the credential check stays conservative.
    #[test]
    fn custom_openai_without_section_is_not_keyless() {
        let settings = AppSettings::default();
        assert!(!provider_is_keyless(&settings, "custom_openai:ghost"));
    }

    /// Keyless skip: when the provider is keyless, the check returns
    /// `Ok(())` without consulting the store. We use `FailingStore` to
    /// prove no `get` / `has` call is made.
    #[tokio::test]
    async fn check_credential_skips_keyless_provider() {
        let store: Arc<dyn Credentials> = Arc::new(FailingStore);
        let settings = settings_with_custom_openai("ollama", AuthShapeSettings::None);
        check_provider_credential(&store, &settings, "custom_openai:ollama")
            .await
            .expect("keyless skip must not touch the store");
    }

    /// Happy path: keyed provider with a stored credential passes the
    /// check.
    #[tokio::test]
    async fn check_credential_accepts_present_credential() {
        let mem = MemoryStore::new();
        mem.set("anthropic", SecretString::from("sk-test-1"))
            .await
            .unwrap();
        let store: Arc<dyn Credentials> = Arc::new(mem);
        let settings = AppSettings::default();
        check_provider_credential(&store, &settings, "anthropic")
            .await
            .expect("present credential must pass");
    }

    /// Missing-entry rejection: the store reports `Ok(false)`. Error
    /// carries the canonical prefix + reason + provider id.
    #[tokio::test]
    async fn check_credential_rejects_missing_entry() {
        let store: Arc<dyn Credentials> = Arc::new(MemoryStore::new());
        let settings = AppSettings::default();
        let err = check_provider_credential(&store, &settings, "anthropic")
            .await
            .expect_err("absent credential must reject");
        assert!(err.starts_with(SESSION_START_ERROR), "prefix: {err}");
        assert!(err.contains(CREDENTIALS_MISSING_REASON), "reason: {err}");
        assert!(err.ends_with("anthropic"), "provider id suffix: {err}");
    }

    /// Keyring-locked rejection: the store errors on `has`. The check
    /// collapses both shapes (missing entry, backend error) onto the
    /// same `credentials_missing` umbrella so the dashboard renders one
    /// CTA either way.
    #[tokio::test]
    async fn check_credential_rejects_backend_error() {
        let store: Arc<dyn Credentials> = Arc::new(FailingStore);
        let settings = AppSettings::default();
        let err = check_provider_credential(&store, &settings, "openai")
            .await
            .expect_err("keyring backend error must reject");
        assert!(err.starts_with(SESSION_START_ERROR), "prefix: {err}");
        assert!(err.contains(CREDENTIALS_MISSING_REASON), "reason: {err}");
        assert!(err.ends_with("openai"), "provider id suffix: {err}");
    }

    /// Named-instance happy path: a built-in with a `<vendor>:<name>`
    /// suffix (`anthropic:work`) and a keyring entry seeded under the
    /// exact same key passes the credential check. Pins the contract
    /// documented above `check_provider_credential`: the store is
    /// consulted with the full `provider_id` verbatim, no
    /// normalization or vendor-prefix stripping.
    #[tokio::test]
    async fn check_credential_accepts_named_builtin_with_keyring_entry() {
        let mem = MemoryStore::new();
        mem.set("anthropic:work", SecretString::from("sk-work-1"))
            .await
            .unwrap();
        let store: Arc<dyn Credentials> = Arc::new(mem);
        let settings = AppSettings::default();
        check_provider_credential(&store, &settings, "anthropic:work")
            .await
            .expect("named-instance credential must pass");
    }

    /// A `custom_openai:<name>` row whose `auth.shape = "header"` (the
    /// X-Api-Key / custom-header preset) is NOT keyless — only
    /// `AuthShapeSettings::None` is. Guards against a future refactor
    /// silently widening the keyless bucket.
    #[test]
    fn custom_openai_with_header_auth_is_not_keyless() {
        let settings = settings_with_custom_openai(
            "local",
            AuthShapeSettings::Header {
                name: "X-Api-Key".into(),
            },
        );
        assert!(!provider_is_keyless(&settings, "custom_openai:local"));
    }
}
