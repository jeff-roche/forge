//! F-586: Tauri command surface for active-provider selection.
//!
//! Three commands, all gated on `webview` so non-webview builds compile
//! without Tauri:
//!
//! - [`dashboard_list_providers`] — returns the four built-in provider
//!   entries (Ollama, Anthropic, OpenAI, Custom OpenAI) plus any
//!   user-configured `[providers.custom_openai.<name>]` entries, each
//!   enriched with a credential-presence flag pulled from the shell's
//!   `Credentials` store. Named with the `dashboard_` prefix to
//!   disambiguate from F-591's roster catalog `list_providers` (Tauri's
//!   `generate_handler!` rejects two commands with the same wire name).
//! - [`get_active_provider`] — reads the persisted active id from
//!   `[providers.active]` of the merged settings.
//! - [`set_active_provider`] — validates the id matches a known provider,
//!   writes through the same `apply_setting_update` path the generic
//!   `set_setting` uses, then emits `provider:changed` Tauri event app-wide
//!   so any open session window's bridge can broadcast a `ProviderChanged`
//!   into its session log for the orchestrator's next turn.
//!
//! # Authorization
//!
//! Provider commands are dashboard-scoped — only the `dashboard` window
//! label may invoke them. Same model as `credentials_ipc`.

#[cfg(feature = "webview")]
use std::sync::Arc;

#[cfg(feature = "webview")]
use forge_core::{
    settings::{apply_setting_update, save_user_settings_raw_in},
    Credentials, Event,
};
use serde::{Deserialize, Serialize};
#[cfg(feature = "webview")]
use tauri::{AppHandle, Emitter, Runtime, State, Webview};
#[cfg(feature = "webview")]
use tokio::sync::Mutex;
#[allow(unused_imports)]
use tracing;

#[cfg(feature = "webview")]
use crate::credentials_ipc::CredentialsState;

/// Process-wide guard that serializes `set_active_provider`'s
/// read-modify-write of the user-tier settings file. The dashboard's
/// double-tap UX (rapid card clicks) and any future programmatic caller
/// could otherwise race two readers, leaving the second writer's TOML to
/// silently overwrite the first. The guard is held only across the
/// `read → apply_setting_update → save_user_settings_raw_in` triple, so
/// the worst-case latency is one disk write — well below human reaction
/// time.
///
/// Scoped to F-586 today; if a future task wants to serialize *every*
/// settings write across crates, lift this into `forge_core::settings`
/// and have `set_setting` (the generic command) acquire it too.
#[cfg(feature = "webview")]
fn settings_write_guard() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}

/// Built-in provider slugs. Stable ids the dashboard, settings file, and
/// keyring all key on. Adding a new built-in: extend the
/// `BUILTIN_PROVIDERS` table below and add a matching credential-required
/// hint.
pub const PROVIDER_OLLAMA: &str = "ollama";
pub const PROVIDER_ANTHROPIC: &str = "anthropic";
pub const PROVIDER_OPENAI: &str = "openai";
pub const PROVIDER_CUSTOM_OPENAI: &str = "custom_openai";

/// Prefix used for user-configured CustomOpenAI entries:
/// `custom_openai:<name>` where `<name>` is the user-chosen key under
/// `[providers.custom_openai.<name>]`. The colon is the separator the
/// dashboard tokenises on.
pub const CUSTOM_OPENAI_PREFIX: &str = "custom_openai:";

/// Per-built-in metadata used to render the dashboard cards.
struct BuiltinDescriptor {
    id: &'static str,
    display_name: &'static str,
    credential_required: bool,
    /// When `true`, the dashboard skips the "available models" enrichment
    /// step — the user supplies the model identifier per entry.
    user_supplied_model: bool,
}

const BUILTIN_PROVIDERS: &[BuiltinDescriptor] = &[
    BuiltinDescriptor {
        id: PROVIDER_OLLAMA,
        display_name: "Ollama",
        credential_required: false,
        user_supplied_model: false,
    },
    BuiltinDescriptor {
        id: PROVIDER_ANTHROPIC,
        display_name: "Anthropic",
        credential_required: true,
        user_supplied_model: true,
    },
    BuiltinDescriptor {
        id: PROVIDER_OPENAI,
        display_name: "OpenAI",
        credential_required: true,
        user_supplied_model: true,
    },
    BuiltinDescriptor {
        id: PROVIDER_CUSTOM_OPENAI,
        display_name: "Custom OpenAI-compat",
        credential_required: true,
        user_supplied_model: true,
    },
];

/// One row of the `dashboard_list_providers` response — what the dashboard renders
/// per card.
///
/// `model_available` is `Some(true)` when the provider has a configured
/// default model (built-in providers carry one out-of-the-box, custom
/// entries declare it explicitly), `Some(false)` when none is available
/// (e.g. a custom entry with an empty `model` field), and `None` when the
/// presence is not yet probed.
///
/// `has_credential` is `false` when the keyring backend reports no entry
/// for the provider id, when the backend is unavailable (treated as
/// "absent" by contract), or when the credential is irrelevant
/// (Ollama keyless). The dashboard renders the warning glyph only when
/// `credential_required && !has_credential`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderEntry {
    pub id: String,
    pub display_name: String,
    pub credential_required: bool,
    pub has_credential: bool,
    pub model_available: bool,
    /// Optional human-readable model id for the dashboard's secondary line
    /// (e.g. the configured `model` field of a `[providers.custom_openai.X]`
    /// entry). `None` for built-ins without a baked-in model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

// ---------------------------------------------------------------------------
// Pure helpers — exercised by unit tests under `--no-default-features`.
// ---------------------------------------------------------------------------

/// Build the list of [`ProviderEntry`] for the dashboard. Pure: takes the
/// merged settings + the credential probe results as inputs so tests can
/// drive every shape (missing keys, unavailable backend, custom entries,
/// etc.) without a live keyring.
///
/// `cred_present(id)` returns `true` when the credential store reported
/// the entry as present. Backend-failure callers should pass a closure
/// that returns `false` for every id — matching the spec's "if the keyring
/// backend is unavailable, treat as `false`".
pub fn build_provider_list(
    settings: &forge_core::settings::AppSettings,
    cred_present: impl Fn(&str) -> bool,
) -> Vec<ProviderEntry> {
    let mut out: Vec<ProviderEntry> = BUILTIN_PROVIDERS
        .iter()
        .map(|b| ProviderEntry {
            id: b.id.to_string(),
            display_name: b.display_name.to_string(),
            credential_required: b.credential_required,
            has_credential: if b.credential_required {
                cred_present(b.id)
            } else {
                false
            },
            // Built-ins always claim a model is available — the daemon
            // either ships one (Ollama via env / discovery) or the user
            // supplies it via the spec / a custom entry.
            model_available: !b.user_supplied_model,
            model: None,
        })
        .collect();

    // User-configured CustomOpenAI entries are reachable both individually
    // (their credential keyed under `custom_openai:<name>`) and through
    // the umbrella `custom_openai` builtin. Render each as its own card so
    // the user can pick one without going through a sub-picker.
    for (name, entry) in &settings.providers.custom_openai {
        let id = format!("{CUSTOM_OPENAI_PREFIX}{name}");
        let model_available = !entry.model.is_empty();
        out.push(ProviderEntry {
            display_name: format!("{} — {}", PROVIDER_CUSTOM_OPENAI, name),
            credential_required: !matches!(
                entry.auth,
                forge_core::settings::AuthShapeSettings::None
            ),
            has_credential: cred_present(&id),
            model_available,
            model: if model_available {
                Some(entry.model.clone())
            } else {
                None
            },
            id,
        });
    }

    out
}

/// `true` when `id` matches one of the built-in slugs or one of the
/// user-configured `custom_openai:<name>` entries in `settings`. Pure
/// helper exposed so `set_active_provider` can validate the id without
/// going through the credential store.
pub fn is_known_provider_id(settings: &forge_core::settings::AppSettings, id: &str) -> bool {
    if BUILTIN_PROVIDERS.iter().any(|b| b.id == id) {
        return true;
    }
    if let Some(rest) = id.strip_prefix(CUSTOM_OPENAI_PREFIX) {
        return settings.providers.custom_openai.contains_key(rest);
    }
    false
}

// F-675: `MAX_PROVIDER_ID_BYTES` is defined canonically in `crate::ipc` so
// the credentials and providers IPC surfaces share one cap. Re-import here
// rather than redeclaring.
use crate::ipc::MAX_PROVIDER_ID_BYTES;

// F-673: command-named error prefixes. Every outer error path returned from a
// `*_ipc.rs` command must begin with one of these constants so the dashboard
// log filter and end-user error display stay consistent across modules. See
// the "Error-message prefix style" header comment in `ipc.rs`.
pub const DASHBOARD_LIST_PROVIDERS_ERROR: &str = "dashboard_list_providers: ";
pub const GET_ACTIVE_PROVIDER_ERROR: &str = "get_active_provider: ";
pub const SET_ACTIVE_PROVIDER_ERROR: &str = "set_active_provider: ";

/// Pure validation helper exposed for unit tests.
pub fn validate_provider_id(provider_id: &str) -> Result<(), String> {
    if provider_id.is_empty() {
        return Err("provider_id is empty".to_string());
    }
    if provider_id.len() > MAX_PROVIDER_ID_BYTES {
        return Err(format!(
            "provider_id too large: {} bytes exceeds cap of {} bytes",
            provider_id.len(),
            MAX_PROVIDER_ID_BYTES
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tauri command surface
// ---------------------------------------------------------------------------

#[cfg(feature = "webview")]
async fn cred_presence_map(store: &Arc<dyn Credentials>, ids: &[String]) -> Vec<(String, bool)> {
    use futures::future::join_all;
    let probes = ids.iter().map(|id| async move {
        // F-587 contract: Ok(None) → false, Err(_) → false (treat as absent
        // when the backend is unavailable, per the F-586 spec).
        let present = store.has(id).await.unwrap_or(false);
        (id.clone(), present)
    });
    join_all(probes).await
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn dashboard_list_providers<R: Runtime>(
    webview: Webview<R>,
    state: State<'_, crate::ipc::BridgeState>,
    creds: State<'_, CredentialsState>,
) -> Result<Vec<ProviderEntry>, String> {
    crate::ipc::require_window_label(&webview, "dashboard", "dashboard_list_providers")?;

    // Provider selection is a user-tier setting (no workspace scope today).
    // Read user settings directly without an unused workspace path.
    let user_dir = crate::ipc::resolve_user_config_dir(&state);
    let settings = match user_dir.as_deref() {
        Some(dir) => forge_core::settings::load_user_settings_in(dir)
            .await
            .map_err(|e| format!("{DASHBOARD_LIST_PROVIDERS_ERROR}{e}"))?,
        None => forge_core::settings::AppSettings::default(),
    };

    // Probe credential presence for every id we'll emit. Two passes so the
    // map drives the closure — `build_provider_list` is sync.
    let mut ids: Vec<String> = BUILTIN_PROVIDERS.iter().map(|b| b.id.to_string()).collect();
    for name in settings.providers.custom_openai.keys() {
        ids.push(format!("{CUSTOM_OPENAI_PREFIX}{name}"));
    }
    let store = creds.store();
    let presence = cred_presence_map(&store, &ids).await;
    let presence_map: std::collections::HashMap<String, bool> = presence.into_iter().collect();

    Ok(build_provider_list(&settings, |id| {
        presence_map.get(id).copied().unwrap_or(false)
    }))
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn get_active_provider<R: Runtime>(
    webview: Webview<R>,
    state: State<'_, crate::ipc::BridgeState>,
) -> Result<Option<String>, String> {
    crate::ipc::require_window_label(&webview, "dashboard", "get_active_provider")?;

    let user_dir = crate::ipc::resolve_user_config_dir(&state);
    let settings = match user_dir.as_deref() {
        Some(dir) => forge_core::settings::load_user_settings_in(dir)
            .await
            .map_err(|e| format!("{GET_ACTIVE_PROVIDER_ERROR}{e}"))?,
        None => forge_core::settings::AppSettings::default(),
    };

    Ok(settings.providers.active)
}

/// Tauri event name carrying a [`Event::ProviderChanged`] payload to any
/// listener (session windows, the dashboard's own state). The session
/// window's IPC bridge (when wired) re-emits this onto its session log so
/// the in-daemon orchestrator picks up the change for its next turn.
pub const PROVIDER_CHANGED_EVENT: &str = "provider:changed";

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn set_active_provider<R: Runtime>(
    provider_id: String,
    app: AppHandle<R>,
    webview: Webview<R>,
    state: State<'_, crate::ipc::BridgeState>,
) -> Result<(), String> {
    crate::ipc::require_window_label(&webview, "dashboard", "set_active_provider")?;
    validate_provider_id(&provider_id)?;

    // Serialize the read-modify-write under a process-wide guard so a
    // double-tap of the dashboard cards (or any future programmatic
    // caller) can't lose updates. The lock holds across the triple:
    // read user-tier TOML → apply_setting_update → save raw TOML.
    let _write_lock = settings_write_guard().lock().await;

    let user_dir = crate::ipc::resolve_user_config_dir(&state);
    let settings = match user_dir.as_deref() {
        Some(dir) => forge_core::settings::load_user_settings_in(dir)
            .await
            .map_err(|e| format!("{SET_ACTIVE_PROVIDER_ERROR}{e}"))?,
        None => forge_core::settings::AppSettings::default(),
    };

    if !is_known_provider_id(&settings, &provider_id) {
        return Err(format!(
            "{SET_ACTIVE_PROVIDER_ERROR}unknown provider: {provider_id}"
        ));
    }

    // Persist to user-tier so the choice survives across workspaces. Same
    // semantics as the existing settings-write path: load → mutate raw TOML
    // → validate → save.
    let user_dir = user_dir.ok_or_else(|| {
        format!("{SET_ACTIVE_PROVIDER_ERROR}could not resolve user config directory")
    })?;
    let user_path = forge_core::settings::user_settings_path_in(&user_dir);
    let existing = tokio::fs::read_to_string(&user_path)
        .await
        .unwrap_or_default();
    let updated = apply_setting_update(
        &existing,
        "providers.active",
        toml::Value::String(provider_id.clone()),
    )
    .map_err(|e| format!("{SET_ACTIVE_PROVIDER_ERROR}{e}"))?;
    save_user_settings_raw_in(&user_dir, &updated)
        .await
        .map_err(|e| format!("{SET_ACTIVE_PROVIDER_ERROR}{e}"))?;

    // Workspace tier is left untouched — provider preference is a global
    // user setting in F-586. If a future task wants to scope per-workspace,
    // extend the IPC with a `level: SettingsLevel` argument and route to
    // `save_workspace_settings_raw` like the generic `set_setting` does.

    tracing::trace!(
        target: "forge_shell::providers",
        provider_id = %provider_id,
        "set_active_provider persisted",
    );

    // F-586 DoD #4: emit `ProviderChanged` so the orchestrator picks up
    // the change for its next turn. We dispatch through Tauri's app-wide
    // emitter so every session window's bridge can fan it out onto its
    // session log; the dashboard itself also listens to update its UI
    // optimistically without waiting for a refetch.
    let event = Event::ProviderChanged {
        provider_id: provider_id.clone(),
    };
    if let Err(e) = app.emit(PROVIDER_CHANGED_EVENT, &event) {
        tracing::warn!(
            target: "forge_shell::providers",
            provider_id = %provider_id,
            error = %e,
            "ProviderChanged emit failed",
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::settings::{
        AppSettings, AuthShapeSettings, CustomOpenAiEntry, ProvidersSettings,
    };
    use std::collections::BTreeMap;

    fn empty_settings() -> AppSettings {
        AppSettings::default()
    }

    fn settings_with_custom(name: &str, entry: CustomOpenAiEntry) -> AppSettings {
        let mut entries = BTreeMap::new();
        entries.insert(name.to_string(), entry);
        AppSettings {
            providers: ProvidersSettings {
                custom_openai: entries,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn build_provider_list_emits_four_builtins_in_stable_order() {
        let s = empty_settings();
        let entries = build_provider_list(&s, |_| false);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["ollama", "anthropic", "openai", "custom_openai"],
            "builtin order is the user-facing card order; do not reorder casually"
        );
    }

    #[test]
    fn build_provider_list_marks_ollama_as_keyless() {
        let s = empty_settings();
        let entries = build_provider_list(&s, |_| false);
        let ollama = entries.iter().find(|e| e.id == "ollama").unwrap();
        assert!(!ollama.credential_required);
        assert!(!ollama.has_credential);
        assert!(ollama.model_available);
    }

    #[test]
    fn build_provider_list_marks_anthropic_credential_present_when_store_says_so() {
        let s = empty_settings();
        let entries = build_provider_list(&s, |id| id == "anthropic");
        let anthropic = entries.iter().find(|e| e.id == "anthropic").unwrap();
        assert!(anthropic.credential_required);
        assert!(anthropic.has_credential);
    }

    #[test]
    fn build_provider_list_treats_keyring_failure_as_absent() {
        // Spec: "if the keyring backend is unavailable, treat as `false`".
        let s = empty_settings();
        let entries = build_provider_list(&s, |_| false);
        for e in &entries {
            if e.credential_required {
                assert!(!e.has_credential, "{e:?}");
            }
        }
    }

    #[test]
    fn build_provider_list_appends_custom_openai_entries() {
        let s = settings_with_custom(
            "vllm-local",
            CustomOpenAiEntry {
                base_url: "http://127.0.0.1:8000".into(),
                model: "Qwen2".into(),
                model_list: vec!["Qwen2".into()],
                auth: AuthShapeSettings::None,
                api_key: None,
            },
        );
        let entries = build_provider_list(&s, |_| false);
        assert_eq!(entries.len(), 5);
        let custom = entries.last().unwrap();
        assert_eq!(custom.id, "custom_openai:vllm-local");
        // `auth = none` ⇒ credential not required.
        assert!(!custom.credential_required);
        assert!(custom.model_available);
        assert_eq!(custom.model.as_deref(), Some("Qwen2"));
    }

    #[test]
    fn build_provider_list_marks_custom_openai_with_bearer_as_credential_required() {
        let s = settings_with_custom(
            "together",
            CustomOpenAiEntry {
                base_url: "https://api.together.xyz".into(),
                model: "mixtral".into(),
                model_list: vec![],
                auth: AuthShapeSettings::Bearer,
                api_key: Some("sk-test".into()),
            },
        );
        let entries = build_provider_list(&s, |id| id == "custom_openai:together");
        let custom = entries
            .iter()
            .find(|e| e.id == "custom_openai:together")
            .unwrap();
        assert!(custom.credential_required);
        assert!(custom.has_credential);
    }

    #[test]
    fn build_provider_list_marks_custom_openai_without_model_as_unavailable() {
        let s = settings_with_custom(
            "stub",
            CustomOpenAiEntry {
                base_url: "https://api.example.com".into(),
                model: String::new(),
                model_list: vec![],
                auth: AuthShapeSettings::Bearer,
                api_key: None,
            },
        );
        let entries = build_provider_list(&s, |_| false);
        let custom = entries
            .iter()
            .find(|e| e.id == "custom_openai:stub")
            .unwrap();
        assert!(!custom.model_available);
        assert!(custom.model.is_none());
    }

    #[test]
    fn is_known_provider_id_accepts_builtins() {
        let s = empty_settings();
        for id in &["ollama", "anthropic", "openai", "custom_openai"] {
            assert!(is_known_provider_id(&s, id), "expected `{id}` known");
        }
    }

    #[test]
    fn is_known_provider_id_rejects_unknown_slug() {
        let s = empty_settings();
        assert!(!is_known_provider_id(&s, "gemini"));
        assert!(!is_known_provider_id(&s, ""));
        assert!(!is_known_provider_id(&s, "custom_openai:does-not-exist"));
    }

    #[test]
    fn is_known_provider_id_accepts_configured_custom_openai_entry() {
        let s = settings_with_custom(
            "vllm",
            CustomOpenAiEntry {
                base_url: "http://x".into(),
                model: "m".into(),
                model_list: vec![],
                auth: AuthShapeSettings::None,
                api_key: None,
            },
        );
        assert!(is_known_provider_id(&s, "custom_openai:vllm"));
        assert!(!is_known_provider_id(&s, "custom_openai:other"));
    }

    #[test]
    fn validate_provider_id_rejects_empty() {
        let err = validate_provider_id("").unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn validate_provider_id_rejects_oversize() {
        let huge = "x".repeat(MAX_PROVIDER_ID_BYTES + 1);
        let err = validate_provider_id(&huge).unwrap_err();
        assert!(err.contains("exceeds cap"));
    }

    #[test]
    fn validate_provider_id_accepts_realistic_slugs() {
        validate_provider_id("anthropic").expect("anthropic");
        validate_provider_id("custom_openai:together").expect("custom_openai:together");
    }
}
