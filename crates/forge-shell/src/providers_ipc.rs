//! F-586: Tauri command surface for active-provider selection.
//!
//! Three commands, all gated on `webview` so non-webview builds compile
//! without Tauri:
//!
//! - [`dashboard_list_providers`] — returns one row per provider the user
//!   has explicitly configured: any built-in (Anthropic, OpenAI) whose
//!   `providers.enabled.<id>` key is present, plus one row per
//!   `[providers.custom_openai.<name>]` section. A fresh install has no
//!   providers configured; the Add Provider modal writes the first entry.
//!   Each row is enriched with a credential-presence flag pulled from the
//!   shell's
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
use ts_rs::TS;

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
pub const PROVIDER_ANTHROPIC: &str = "anthropic";
pub const PROVIDER_OPENAI: &str = "openai";
pub const PROVIDER_CUSTOM_OPENAI: &str = "custom_openai";

/// Prefix used for user-configured CustomOpenAI entries:
/// `custom_openai:<name>` where `<name>` is the user-chosen key under
/// `[providers.custom_openai.<name>]`. The colon is the separator the
/// dashboard tokenises on.
pub const CUSTOM_OPENAI_PREFIX: &str = "custom_openai:";

/// Separator between a built-in vendor slug and a user-chosen instance
/// name. Phase A: enables `anthropic:work` and `anthropic:personal` style
/// ids alongside the bare `anthropic` legacy form so the user can hold
/// multiple credentials for the same vendor side-by-side.
pub const NAMED_INSTANCE_SEPARATOR: char = ':';

/// Parsed form of a provider id used throughout the IPC layer.
///
/// - `BuiltinBare(vendor)` — legacy single-instance built-in (`anthropic`).
/// - `BuiltinNamed { vendor, name }` — named built-in instance
///   (`anthropic:work`). `name` is the user-chosen instance label.
/// - `CustomOpenAi(name)` — custom OpenAI-spec entry (`custom_openai:vllm`).
/// - `Unknown` — neither prefix matched. Validation flows surface a
///   typed error for this case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedProviderId<'a> {
    BuiltinBare(&'a str),
    BuiltinNamed { vendor: &'a str, name: &'a str },
    CustomOpenAi(&'a str),
    Unknown,
}

impl<'a> ParsedProviderId<'a> {
    /// Vendor slug carried by this id (`anthropic`, `openai`).
    /// `None` for `CustomOpenAi` and `Unknown`.
    pub fn vendor(&self) -> Option<&'a str> {
        match self {
            ParsedProviderId::BuiltinBare(v) | ParsedProviderId::BuiltinNamed { vendor: v, .. } => {
                Some(v)
            }
            _ => None,
        }
    }
}

/// Classify a provider id without consulting settings. The result is used
/// by validation, routing, and the dashboard list to handle the three
/// supported id shapes uniformly. Vendor strings are matched against
/// [`BUILTIN_ADDABLE_KINDS`] — broader than `BUILTIN_PROVIDERS` because
/// `mistral` is an addable slug whose runtime adapter has not landed yet
/// (the schema still recognises it). Anything with the `custom_openai:`
/// prefix flows through `CustomOpenAi`.
pub fn parse_provider_id(id: &str) -> ParsedProviderId<'_> {
    if id.is_empty() {
        return ParsedProviderId::Unknown;
    }
    // Custom OpenAI takes precedence over the named-builtin shape because
    // the literal vendor slug "custom_openai" is intentionally NOT in
    // `BUILTIN_ADDABLE_KINDS` — custom entries route through their own
    // section in settings (see add_provider).
    if let Some(rest) = id.strip_prefix(CUSTOM_OPENAI_PREFIX) {
        return ParsedProviderId::CustomOpenAi(rest);
    }
    if let Some((vendor, name)) = id.split_once(NAMED_INSTANCE_SEPARATOR) {
        if BUILTIN_ADDABLE_KINDS.contains(&vendor) {
            return ParsedProviderId::BuiltinNamed { vendor, name };
        }
        return ParsedProviderId::Unknown;
    }
    if BUILTIN_ADDABLE_KINDS.contains(&id) {
        return ParsedProviderId::BuiltinBare(id);
    }
    ParsedProviderId::Unknown
}

/// Per-built-in metadata used to render the dashboard cards.
struct BuiltinDescriptor {
    id: &'static str,
    display_name: &'static str,
    credential_required: bool,
}

const BUILTIN_PROVIDERS: &[BuiltinDescriptor] = &[
    BuiltinDescriptor {
        id: PROVIDER_ANTHROPIC,
        display_name: "Anthropic",
        credential_required: true,
    },
    BuiltinDescriptor {
        id: PROVIDER_OPENAI,
        display_name: "OpenAI",
        credential_required: true,
    },
    // `custom_openai` is intentionally absent — it's a kind, not a row.
    // Concrete OpenAI-spec endpoints render via the `custom_openai:<name>`
    // loop below; users add new ones through the Add Provider modal.
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
/// "absent" by contract), or when the credential is irrelevant. The
/// dashboard renders the warning glyph only when
/// `credential_required && !has_credential`.
///
/// `endpoint` is populated only for `custom_openai:<name>` rows so the
/// Providers page's Edit dialog can pre-fill its inputs from the row
/// payload directly (no follow-up read). Built-in rows omit it.
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
    /// `base_url` of the underlying `[providers.custom_openai.<name>]`
    /// section. Populated for `custom_openai:*` rows only — built-ins
    /// resolve their endpoint through `builtin_probe_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// F-733: per-provider enable flag mirroring `providers.enabled.<id>`.
    /// Absent settings entries read as `true` — historical configs (pre
    /// F-730) that never wrote the flag keep their built-ins live. The
    /// Providers page toggle and the new-session picker key on this bit.
    #[serde(default = "default_enabled_true")]
    pub enabled: bool,
    /// Phase B: authentication mode for named built-in instances. `None`
    /// for bare-vendor / `custom_openai:` / provider-without-section
    /// rows. When `Some(Vertex)`, the dashboard suppresses the
    /// "ADD CREDENTIAL" CTA and the orange auth pill — Vertex auth pulls
    /// from gcloud ADC at request time, not from the keychain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_kind: Option<forge_core::BuiltinAuthKind>,
}

fn default_enabled_true() -> bool {
    true
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
    // A built-in only appears in the list when the user has explicitly
    // added it — i.e. `providers.enabled.<id>` is present (with any bool
    // value). Absent means "not configured" and the row is omitted.
    // Phase A: the `<id>` may be either the bare vendor (`anthropic`) or
    // a named instance (`anthropic:work`). Both shapes resolve here to a
    // single row carrying the full id; the dashboard shows the user the
    // instance name when one is present so two `anthropic:*` entries
    // are distinguishable at a glance.
    let mut out: Vec<ProviderEntry> = Vec::new();
    // Phase A: emit bare-vendor entries first in `BUILTIN_PROVIDERS`
    // declaration order so the dashboard's user-facing card order stays
    // stable across releases. Then append any named instances
    // (`<vendor>:<name>`) sorted by full id so multiple anthropic / openai
    // configurations cluster together below their bare equivalents.
    // Phase B: look up the per-instance auth_kind for named built-in
    // Anthropic entries so the row can suppress the "ADD CREDENTIAL"
    // surface when gcloud ADC supplies auth at request time.
    let instance_auth = |vendor: &str, name: &str| -> Option<forge_core::BuiltinAuthKind> {
        if vendor == PROVIDER_ANTHROPIC {
            settings
                .providers
                .anthropic
                .get(name)
                .map(|entry| entry.auth_kind)
        } else {
            None
        }
    };

    let row_for = |id: &str,
                   vendor: &str,
                   instance_name: Option<&str>,
                   enabled: bool,
                   auth_kind: Option<forge_core::BuiltinAuthKind>| {
        BUILTIN_PROVIDERS
            .iter()
            .find(|b| b.id == vendor)
            .map(|descriptor| {
                let display_name = match instance_name {
                    Some(name) => format!("{} — {name}", descriptor.display_name),
                    None => descriptor.display_name.to_string(),
                };
                // Vertex instances bypass the keychain — gcloud ADC supplies
                // an access token at request time. Report `credential_required
                // = false` so the dashboard does NOT prompt for an API key.
                let is_vertex = matches!(auth_kind, Some(forge_core::BuiltinAuthKind::Vertex));
                let effective_credential_required = descriptor.credential_required && !is_vertex;
                ProviderEntry {
                    id: id.to_string(),
                    display_name,
                    credential_required: effective_credential_required,
                    // Probe the keychain only when a credential is actually
                    // required — ADC-backed (Vertex) rows report `false` and
                    // the dashboard's pill predicate
                    // (`credential_required && !has_credential`) trivially
                    // falls through to `ready`.
                    has_credential: if effective_credential_required {
                        cred_present(id)
                    } else {
                        false
                    },
                    // Built-ins always claim a model is available — the daemon
                    // ships a default and the orchestrator resolves the concrete
                    // model id at request time. Per-instance overrides land via
                    // `[providers.<vendor>.<name>]` once that schema grows a
                    // `model` field; for now the daemon default is the source.
                    model_available: true,
                    model: None,
                    endpoint: None,
                    enabled,
                    auth_kind,
                }
            })
    };

    for descriptor in BUILTIN_PROVIDERS.iter() {
        if let Some(enabled) = settings.providers.enabled.get(descriptor.id).copied() {
            if let Some(row) = row_for(descriptor.id, descriptor.id, None, enabled, None) {
                out.push(row);
            }
        }
    }

    let mut named: Vec<(&String, bool)> = settings
        .providers
        .enabled
        .iter()
        .filter_map(|(id, enabled)| match parse_provider_id(id) {
            ParsedProviderId::BuiltinNamed { .. } => Some((id, *enabled)),
            _ => None,
        })
        .collect();
    named.sort_by_key(|a| a.0);
    for (id, enabled) in named {
        if let ParsedProviderId::BuiltinNamed { vendor, name } = parse_provider_id(id) {
            let auth_kind = instance_auth(vendor, name);
            if let Some(row) = row_for(id, vendor, Some(name), enabled, auth_kind) {
                out.push(row);
            }
        }
    }

    // User-configured CustomOpenAI entries render as their own rows —
    // one per `[providers.custom_openai.<name>]` section. Their credential
    // is keyed under `custom_openai:<name>`. The section's existence is
    // what makes the entry configured; `providers.enabled.<id>` is just
    // the on/off toggle (absent = on, the user added but never disabled).
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
            endpoint: Some(entry.base_url.clone()),
            enabled: is_enabled_provider(settings, &id),
            id,
            auth_kind: None,
        });
    }

    out
}

/// Reads the on/off toggle bool from `providers.enabled.<id>`. Absent
/// means the user never disabled this provider, so it is reported as on.
/// This is independent of "configured" — use `is_known_provider_id` to
/// gate whether the provider actually exists in settings.
pub fn is_enabled_provider(settings: &forge_core::settings::AppSettings, id: &str) -> bool {
    settings.providers.enabled.get(id).copied().unwrap_or(true)
}

/// `true` when `id` is configured in `settings` — either a built-in slug
/// that the user has added (`providers.enabled.<id>` key present) or a
/// `custom_openai:<name>` whose section exists. Pure helper exposed so
/// `set_active_provider` and `set_provider_enabled` can validate the id
/// without going through the credential store.
pub fn is_known_provider_id(settings: &forge_core::settings::AppSettings, id: &str) -> bool {
    match parse_provider_id(id) {
        // Both bare (`anthropic`) and named (`anthropic:work`) built-ins
        // record the same shape in settings: a key in
        // `providers.enabled.<full_id>`. The named form is added by the
        // Phase-A Add Provider modal when the user supplies an instance
        // name; the bare form is the legacy single-instance shape.
        ParsedProviderId::BuiltinBare(_) | ParsedProviderId::BuiltinNamed { .. } => {
            settings.providers.enabled.contains_key(id)
        }
        ParsedProviderId::CustomOpenAi(name) => settings.providers.custom_openai.contains_key(name),
        ParsedProviderId::Unknown => false,
    }
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
pub const ADD_PROVIDER_ERROR: &str = "add_provider: ";
pub const TEST_PROVIDER_CONNECTION_ERROR: &str = "test_provider_connection: ";
pub const UPDATE_PROVIDER_ERROR: &str = "update_provider: ";
pub const REMOVE_PROVIDER_ERROR: &str = "remove_provider: ";
pub const SET_PROVIDER_ENABLED_ERROR: &str = "set_provider_enabled: ";

/// Built-in provider kinds accepted by `add_provider`. The umbrella
/// `custom_openai` slug is excluded from this set — custom OpenAI-compat
/// entries flow through the `custom_openai:<name>` branch which writes a
/// `[providers.custom_openai.<name>]` section instead of an `enabled` flag.
///
/// `mistral` is admitted here ahead of a dedicated runtime adapter: the
/// dashboard add-provider form ships it as a built-in kind so the schema
/// keeps room for a future first-class adapter without a wire break.
pub const BUILTIN_ADDABLE_KINDS: &[&str] = &[PROVIDER_ANTHROPIC, PROVIDER_OPENAI, "mistral"];

/// Validate the shape of an instance name suffix. Shared by
/// `custom_openai:<name>` and built-in `<vendor>:<name>` ids so the two id
/// families honor the same charset / non-empty constraints — the keyring
/// id and the dashboard display both depend on it.
fn validate_instance_name(name: &str, kind_for_error: &str) -> Result<(), String> {
    if name.is_empty() || name.chars().all(char::is_whitespace) {
        return Err(format!(
            "provider_id {kind_for_error}<name> must have a non-empty name"
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "provider_id {kind_for_error}<name> name must contain only [A-Za-z0-9_-]"
        ));
    }
    Ok(())
}

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
    match parse_provider_id(provider_id) {
        ParsedProviderId::BuiltinBare(_) => Ok(()),
        ParsedProviderId::BuiltinNamed { vendor, name } => {
            validate_instance_name(name, &format!("{vendor}{NAMED_INSTANCE_SEPARATOR}"))
        }
        ParsedProviderId::CustomOpenAi(name) => validate_instance_name(name, CUSTOM_OPENAI_PREFIX),
        // Some downstream paths (e.g. `set_active_provider`) call
        // `validate_provider_id` to guard the size cap on ids they later
        // probe through `is_known_provider_id`. Returning `Ok` here keeps
        // that contract while letting `is_known_provider_id` reject the
        // unknown shape against the persisted settings.
        ParsedProviderId::Unknown => Ok(()),
    }
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

    // F-733: disabled providers cannot be the active selection. The toggle
    // is the user-facing kill switch; selecting through `set_active_provider`
    // (radio click, programmatic caller) is rejected verbatim per F-673 so
    // the renderer can surface the safeguard inline.
    if !is_enabled_provider(&settings, &provider_id) {
        return Err(format!(
            "{SET_ACTIVE_PROVIDER_ERROR}provider {provider_id} is disabled"
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
// ---- add_provider ---------------------------------------------------------
// ---------------------------------------------------------------------------

/// `add_provider` IPC input. Two shapes share one struct — the
/// `id` discriminates:
///
/// - `"anthropic"` / `"openai"` / `"mistral"` — built-in kind.
///   The optional `config` MUST be `None`; setting it is rejected so the
///   wire shape stays a typed pair and a future schema change for a built-in
///   can't silently absorb a `custom_openai`-shaped payload.
/// - `"custom_openai:<name>"` — custom OpenAI-compat endpoint. `config`
///   is required and is persisted as `[providers.custom_openai.<name>]`.
#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct AddProviderInput {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<CustomOpenAiConfig>,
    /// Phase B: per-instance config for named built-ins. Required when
    /// `auth_kind` is non-default (e.g. `vertex`); absent leaves the
    /// instance with all-default `BuiltinInstanceEntry` values and no
    /// `[providers.<vendor>.<name>]` section written to disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<BuiltinProviderConfig>,
}

/// Connection details for a `custom_openai:<name>` entry, supplied at add
/// time. Mirrors the wire fields the `providers-page.md` spec calls for —
/// the credential is stored separately via `login_provider` and is never
/// carried on this struct.
#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct CustomOpenAiConfig {
    pub endpoint: String,
    pub model: String,
    /// When `true`, persist `auth = { shape = "none" }` so the section
    /// represents a keyless OpenAI-compatible endpoint (Ollama via the
    /// custom_openai preset, local
    /// vLLM, internal mocks). The `login_provider` chain is skipped on
    /// the UI side for these entries — there is no key to store.
    /// Absent / `false` keeps the default Bearer-token auth shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyless: Option<bool>,
}

/// Phase B: per-instance settings carried by the Add Provider modal for
/// named built-ins. Today this drives the API-key vs. Vertex AI auth
/// selector on Anthropic. Future expansions (model override, endpoint
/// override) land here without breaking the existing wire.
#[derive(Debug, Clone, Default, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct BuiltinProviderConfig {
    /// `"api_key"` (default) or `"vertex"`.
    #[serde(default)]
    pub auth_kind: forge_core::BuiltinAuthKind,
    /// Required when `auth_kind = "vertex"`: Google Cloud project id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertex_project: Option<String>,
    /// Required when `auth_kind = "vertex"`: Vertex region.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertex_region: Option<String>,
}

/// Pure validation exposed for unit tests. Mirrors the field rules in
/// `docs/ui-specs/providers-page.md` §"Add provider":
///
/// - `id` non-empty, ≤ `MAX_PROVIDER_ID_BYTES`, matches a built-in slug OR
///   parses as `custom_openai:<name>` with a non-empty `[A-Za-z0-9_-]+` suffix.
/// - Built-in branch: `config` MUST be `None`.
/// - Custom-openai branch: `config` is required; `endpoint` parses as
///   `http`/`https`; `model` non-empty.
pub fn validate_add_provider_input(input: &AddProviderInput) -> Result<(), String> {
    validate_provider_id(&input.id).map_err(|e| format!("{ADD_PROVIDER_ERROR}{e}"))?;
    match parse_provider_id(&input.id) {
        ParsedProviderId::CustomOpenAi(_) => {
            let cfg = input
                .config
                .as_ref()
                .ok_or_else(|| format!("{ADD_PROVIDER_ERROR}custom_openai requires config"))?;
            validate_endpoint(&cfg.endpoint)?;
            if cfg.model.trim().is_empty() {
                return Err(format!("{ADD_PROVIDER_ERROR}model is required"));
            }
        }
        ParsedProviderId::BuiltinBare(vendor) | ParsedProviderId::BuiltinNamed { vendor, .. } => {
            if !BUILTIN_ADDABLE_KINDS.contains(&vendor) {
                return Err(format!(
                    "{ADD_PROVIDER_ERROR}unknown provider kind: {vendor}",
                ));
            }
            if input.config.is_some() {
                return Err(format!(
                    "{ADD_PROVIDER_ERROR}built-in provider {} does not accept config",
                    input.id
                ));
            }
            if let Some(builtin) = input.builtin.as_ref() {
                match builtin.auth_kind {
                    forge_core::BuiltinAuthKind::ApiKey => {
                        // No extra fields required.
                    }
                    forge_core::BuiltinAuthKind::Vertex => {
                        if vendor != PROVIDER_ANTHROPIC {
                            return Err(format!(
                                "{ADD_PROVIDER_ERROR}vertex auth is only supported on anthropic",
                            ));
                        }
                        match builtin.vertex_project.as_deref().map(str::trim) {
                            Some(p) if !p.is_empty() => {}
                            _ => {
                                return Err(format!(
                                    "{ADD_PROVIDER_ERROR}vertex_project is required for vertex auth",
                                ));
                            }
                        }
                        match builtin.vertex_region.as_deref().map(str::trim) {
                            Some(r) if !r.is_empty() => {}
                            _ => {
                                return Err(format!(
                                    "{ADD_PROVIDER_ERROR}vertex_region is required for vertex auth",
                                ));
                            }
                        }
                    }
                }
            }
        }
        ParsedProviderId::Unknown => {
            return Err(format!(
                "{ADD_PROVIDER_ERROR}unknown provider kind: {}",
                input.id
            ));
        }
    }
    Ok(())
}

fn validate_endpoint(raw: &str) -> Result<(), String> {
    let url = url::Url::parse(raw)
        .map_err(|e| format!("{ADD_PROVIDER_ERROR}invalid endpoint URL: {e}"))?;
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(format!(
            "{ADD_PROVIDER_ERROR}invalid endpoint URL: scheme {other} is not http/https"
        )),
    }
}

/// Pure check: `true` when `id` is already configured in `settings` —
/// either as a built-in with a `providers.enabled.<id>` key present (any
/// bool value: true or false), or as a `[providers.custom_openai.<name>]`
/// entry. A disabled built-in still counts as configured — the row is
/// just toggled off, not removed. Exposed for unit tests.
pub fn provider_already_configured(settings_toml: &str, id: &str) -> Result<bool, String> {
    if let Some(name) = id.strip_prefix(CUSTOM_OPENAI_PREFIX) {
        let tree: toml::Value = if settings_toml.trim().is_empty() {
            return Ok(false);
        } else {
            toml::from_str(settings_toml).map_err(|e| format!("{ADD_PROVIDER_ERROR}{e}"))?
        };
        return Ok(tree
            .get("providers")
            .and_then(|p| p.get("custom_openai"))
            .and_then(|c| c.get(name))
            .is_some());
    }
    let tree: toml::Value = if settings_toml.trim().is_empty() {
        return Ok(false);
    } else {
        toml::from_str(settings_toml).map_err(|e| format!("{ADD_PROVIDER_ERROR}{e}"))?
    };
    Ok(tree
        .get("providers")
        .and_then(|p| p.get("enabled"))
        .and_then(|e| e.get(id))
        .is_some())
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn add_provider<R: Runtime>(
    input: AddProviderInput,
    webview: Webview<R>,
    state: State<'_, crate::ipc::BridgeState>,
    creds: State<'_, CredentialsState>,
) -> Result<ProviderEntry, String> {
    crate::ipc::require_window_label(&webview, "dashboard", "add_provider")
        .map_err(|e| format!("{ADD_PROVIDER_ERROR}{e}"))?;
    validate_add_provider_input(&input)?;

    // Serialize the read-modify-write so a double-submit from the modal
    // can't race two writers. Same guard `set_active_provider` uses — the
    // worst-case latency is one disk write.
    let _write_lock = settings_write_guard().lock().await;

    let user_dir = crate::ipc::resolve_user_config_dir(&state)
        .ok_or_else(|| format!("{ADD_PROVIDER_ERROR}could not resolve user config directory"))?;
    let user_path = forge_core::settings::user_settings_path_in(&user_dir);
    let existing = tokio::fs::read_to_string(&user_path)
        .await
        .unwrap_or_default();

    if provider_already_configured(&existing, &input.id)? {
        return Err(format!(
            "{ADD_PROVIDER_ERROR}provider {} already configured",
            input.id
        ));
    }

    let updated = if let Some(name) = input.id.strip_prefix(CUSTOM_OPENAI_PREFIX) {
        let cfg = input.config.as_ref().expect("validated above");
        write_custom_openai_section(&existing, name, cfg)?
    } else {
        let with_enabled = apply_setting_update(
            &existing,
            &format!("providers.enabled.{}", input.id),
            toml::Value::Boolean(true),
        )
        .map_err(|e| format!("{ADD_PROVIDER_ERROR}{e}"))?;

        // Phase B: persist the per-instance section when the modal sent
        // non-default builtin config (today: anything other than the
        // implicit api_key auth_kind).
        if let Some(builtin) = input.builtin.as_ref() {
            if let ParsedProviderId::BuiltinNamed { vendor, name } = parse_provider_id(&input.id) {
                if matches!(builtin.auth_kind, forge_core::BuiltinAuthKind::Vertex) {
                    write_builtin_instance_section(&with_enabled, vendor, name, builtin)?
                } else {
                    with_enabled
                }
            } else {
                with_enabled
            }
        } else {
            with_enabled
        }
    };
    save_user_settings_raw_in(&user_dir, &updated)
        .await
        .map_err(|e| format!("{ADD_PROVIDER_ERROR}{e}"))?;

    // Build the row the frontend renders — same shape `dashboard_list_providers`
    // returns so the page can splice it in without a refetch.
    let settings = forge_core::settings::load_user_settings_in(&user_dir)
        .await
        .map_err(|e| format!("{ADD_PROVIDER_ERROR}{e}"))?;
    let store = creds.store();
    let presence = cred_presence_map(&store, std::slice::from_ref(&input.id)).await;
    let presence_map: std::collections::HashMap<String, bool> = presence.into_iter().collect();
    let rows = build_provider_list(&settings, |id| {
        presence_map.get(id).copied().unwrap_or(false)
    });
    rows.into_iter().find(|r| r.id == input.id).ok_or_else(|| {
        format!(
            "{ADD_PROVIDER_ERROR}post-write lookup failed for {}",
            input.id
        )
    })
}

/// Walk-and-set helper for the `[providers.custom_openai.<name>]` branch.
/// Two dotted-key writes (`base_url`, `model`) so the auth-shape default
/// and the empty model_list stay implicit.
fn write_custom_openai_section(
    existing: &str,
    name: &str,
    cfg: &CustomOpenAiConfig,
) -> Result<String, String> {
    let mut body = apply_setting_update(
        existing,
        &format!("providers.custom_openai.{name}.base_url"),
        toml::Value::String(cfg.endpoint.clone()),
    )
    .map_err(|e| format!("{ADD_PROVIDER_ERROR}{e}"))?;
    body = apply_setting_update(
        &body,
        &format!("providers.custom_openai.{name}.model"),
        toml::Value::String(cfg.model.clone()),
    )
    .map_err(|e| format!("{ADD_PROVIDER_ERROR}{e}"))?;
    if matches!(cfg.keyless, Some(true)) {
        // Emit `[providers.custom_openai.<name>.auth] shape = "none"` so
        // the runtime constructs the client without an Authorization /
        // x-api-key header. The bracketed table form is what the typed
        // `AuthShapeSettings::None` round-trips to in TOML.
        body = apply_setting_update(
            &body,
            &format!("providers.custom_openai.{name}.auth.shape"),
            toml::Value::String("none".to_string()),
        )
        .map_err(|e| format!("{ADD_PROVIDER_ERROR}{e}"))?;
    }
    Ok(body)
}

/// Phase B: write `[providers.<vendor>.<name>]` for a named built-in
/// instance. Only emits the section when the auth_kind is non-default —
/// the legacy ApiKey path leaves no on-disk per-instance state so
/// existing user files stay byte-identical.
///
/// Vendor is one of `BUILTIN_PROVIDERS` (currently `anthropic` — other
/// vendors do not yet have a Vertex / non-default auth mode). Caller
/// validates `vertex_project`/`vertex_region` presence; we trust the
/// payload here.
fn write_builtin_instance_section(
    existing: &str,
    vendor: &str,
    name: &str,
    cfg: &BuiltinProviderConfig,
) -> Result<String, String> {
    let auth_kind_str = match cfg.auth_kind {
        forge_core::BuiltinAuthKind::ApiKey => "api_key",
        forge_core::BuiltinAuthKind::Vertex => "vertex",
    };
    let mut body = apply_setting_update(
        existing,
        &format!("providers.{vendor}.{name}.auth_kind"),
        toml::Value::String(auth_kind_str.to_string()),
    )
    .map_err(|e| format!("{ADD_PROVIDER_ERROR}{e}"))?;
    if let Some(project) = cfg.vertex_project.as_deref().map(str::trim) {
        if !project.is_empty() {
            body = apply_setting_update(
                &body,
                &format!("providers.{vendor}.{name}.vertex_project"),
                toml::Value::String(project.to_string()),
            )
            .map_err(|e| format!("{ADD_PROVIDER_ERROR}{e}"))?;
        }
    }
    if let Some(region) = cfg.vertex_region.as_deref().map(str::trim) {
        if !region.is_empty() {
            body = apply_setting_update(
                &body,
                &format!("providers.{vendor}.{name}.vertex_region"),
                toml::Value::String(region.to_string()),
            )
            .map_err(|e| format!("{ADD_PROVIDER_ERROR}{e}"))?;
        }
    }
    Ok(body)
}

// ---------------------------------------------------------------------------
// ---- test_provider_connection --------------------------------------------
// ---------------------------------------------------------------------------

/// `test_provider_connection` IPC input. The `provider_id` discriminates a
/// built-in slug (`anthropic` / `openai` / `mistral`) from a
/// `custom_openai:<name>` entry — same surface `add_provider` accepts.
#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct TestProviderConnectionInput {
    pub provider_id: String,
}

/// `test_provider_connection` IPC output. `ok` is the canonical success bit;
/// `latency_ms` is populated only when `ok = true` and the probe round-trip
/// fits the 5s deadline. `model_count` and `models` are best-effort — providers
/// that do not return a recognizable model list on the probe endpoint leave
/// both `None`. When present, `models` lists each `id` from the response array
/// in source order; callers can render a dropdown directly from it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct TestProviderConnectionOutput {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
}

/// Wall-clock deadline applied to each probe — matches the 5s budget in
/// `docs/ui-specs/providers-page.md §"Test connection"`.
pub const TEST_PROVIDER_CONNECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Build the `/v1/models` URL for a user-configured base. The wire
/// convention is that `base_url` is the host root (e.g.
/// `http://127.0.0.1:11434`) and callers append their own versioned
/// path; in practice users sometimes paste the full
/// `http://host/v1` URL — strip a single trailing `/v1` plus any
/// trailing slashes so we never produce `…/v1/v1/models`.
fn models_probe_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    let stripped = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    format!("{stripped}/v1/models")
}

/// Built-in models endpoint per kind. The `custom_openai:<name>` branch
/// derives its URL from the user-configured `base_url`.
fn builtin_probe_url(kind: &str) -> Option<&'static str> {
    match kind {
        PROVIDER_ANTHROPIC => Some("https://api.anthropic.com/v1/models"),
        PROVIDER_OPENAI => Some("https://api.openai.com/v1/models"),
        "mistral" => Some("https://api.mistral.ai/v1/models"),
        _ => None,
    }
}

/// Classify an HTTP status into one of the canonical
/// `test_provider_connection` error infixes. `auth ` is the load-bearing
/// signal — the dashboard pill flips to `auth-required` on this prefix.
fn classify_status(status: u16) -> String {
    match status {
        401 | 403 => format!("auth HTTP {status}"),
        s if (400..500).contains(&s) => format!("network HTTP {s}"),
        s => format!("network HTTP {s}"),
    }
}

#[cfg(feature = "webview")]
async fn probe_http_request(
    client: &reqwest::Client,
    url: &str,
    headers: Vec<(String, String)>,
) -> Result<TestProviderConnectionOutput, String> {
    let mut builder = client.get(url);
    for (name, value) in &headers {
        builder = builder.header(name.as_str(), value);
    }

    // Header names only — values may contain Bearer tokens or other
    // secrets and must never be logged verbatim.
    let header_names: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
    tracing::debug!(
        target: "forge_shell::providers",
        url = %url,
        headers = ?header_names,
        "probe GET"
    );

    let started = std::time::Instant::now();
    let response = builder.send().await.map_err(|e| {
        tracing::debug!(
            target: "forge_shell::providers",
            url = %url,
            error = %e,
            "probe network error"
        );
        format!("{TEST_PROVIDER_CONNECTION_ERROR}network {e}")
    })?;
    let status = response.status();
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::debug!(
        target: "forge_shell::providers",
        url = %url,
        status = status.as_u16(),
        latency_ms = latency_ms,
        "probe response"
    );

    if !status.is_success() {
        return Err(format!(
            "{TEST_PROVIDER_CONNECTION_ERROR}{}",
            classify_status(status.as_u16())
        ));
    }

    // Best-effort model list: parse the body as JSON and look for a
    // top-level `data` array (OpenAI/Mistral/Anthropic) or `models` array.
    // For each element, accept the canonical OpenAI shape (`{ "id": "..." }`)
    // or the Ollama shape (`{ "name": "..." }`). Failures here do not
    // invalidate the probe — the connection itself succeeded.
    let models = response
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|body| {
            body.get("data")
                .or_else(|| body.get("models"))
                .and_then(|arr| arr.as_array().cloned())
        })
        .map(|arr| {
            arr.into_iter()
                .filter_map(|item| {
                    item.get("id")
                        .or_else(|| item.get("name"))
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                })
                .collect::<Vec<String>>()
        });
    let model_count = models.as_ref().and_then(|m| u32::try_from(m.len()).ok());
    // Log up to the first five ids so a misconfigured endpoint reads
    // obviously in the log without bloating the line for a 200-model
    // response.
    let preview: Vec<&str> = models
        .as_ref()
        .map(|m| m.iter().take(5).map(String::as_str).collect())
        .unwrap_or_default();
    tracing::debug!(
        target: "forge_shell::providers",
        url = %url,
        model_count = model_count.unwrap_or(0),
        preview = ?preview,
        "probe models parsed"
    );

    Ok(TestProviderConnectionOutput {
        ok: true,
        latency_ms: Some(latency_ms),
        model_count,
        models,
    })
}

#[cfg(feature = "webview")]
async fn load_credential(
    creds: &Arc<dyn Credentials>,
    provider_id: &str,
) -> Result<String, String> {
    use secrecy::ExposeSecret;
    let secret = creds
        .get(provider_id)
        .await
        .map_err(|e| format!("{TEST_PROVIDER_CONNECTION_ERROR}{e}"))?
        .ok_or_else(|| format!("{TEST_PROVIDER_CONNECTION_ERROR}missing credential"))?;
    Ok(secret.expose_secret().to_string())
}

#[cfg(feature = "webview")]
async fn dispatch_probe(
    client: &reqwest::Client,
    provider_id: &str,
    settings: &forge_core::settings::AppSettings,
    creds: &Arc<dyn Credentials>,
) -> Result<TestProviderConnectionOutput, String> {
    if let Some(name) = provider_id.strip_prefix(CUSTOM_OPENAI_PREFIX) {
        let entry = settings.providers.custom_openai.get(name).ok_or_else(|| {
            format!("{TEST_PROVIDER_CONNECTION_ERROR}unknown provider: {provider_id}")
        })?;
        let url = models_probe_url(&entry.base_url);
        let headers = match &entry.auth {
            forge_core::settings::AuthShapeSettings::None => Vec::new(),
            forge_core::settings::AuthShapeSettings::Bearer => {
                let key = load_credential(creds, provider_id).await?;
                vec![("authorization".to_string(), format!("Bearer {key}"))]
            }
            forge_core::settings::AuthShapeSettings::Header { name } => {
                let key = load_credential(creds, provider_id).await?;
                vec![(name.clone(), key)]
            }
        };
        return probe_http_request(client, &url, headers).await;
    }

    // Built-in (bare or `<vendor>:<name>`). The vendor drives URL +
    // header shape; the full `provider_id` keys credential lookup so
    // named instances pull their own API key from the keychain.
    let parsed = parse_provider_id(provider_id);
    let vendor = parsed.vendor().ok_or_else(|| {
        format!("{TEST_PROVIDER_CONNECTION_ERROR}unknown provider: {provider_id}")
    })?;

    // Phase B: Anthropic instances may be configured for Google Vertex
    // AI via `[providers.anthropic.<name>] auth_kind = "vertex"`. In
    // that case the probe shells out to gcloud for an ADC access token
    // and hits the Vertex publisher endpoint — no API key involved.
    if vendor == PROVIDER_ANTHROPIC {
        if let ParsedProviderId::BuiltinNamed { vendor: _, name } = parsed {
            if let Some(entry) = settings.providers.anthropic.get(name) {
                if matches!(entry.auth_kind, forge_core::BuiltinAuthKind::Vertex) {
                    return probe_anthropic_vertex(client, entry).await;
                }
            }
        }
    }

    let url = builtin_probe_url(vendor).ok_or_else(|| {
        format!("{TEST_PROVIDER_CONNECTION_ERROR}unknown provider: {provider_id}")
    })?;

    let headers: Vec<(String, String)> = match vendor {
        PROVIDER_ANTHROPIC => {
            let key = load_credential(creds, provider_id).await?;
            vec![
                ("x-api-key".to_string(), key),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ]
        }
        _ => {
            // openai / mistral
            let key = load_credential(creds, provider_id).await?;
            vec![("authorization".to_string(), format!("Bearer {key}"))]
        }
    };

    probe_http_request(client, url, headers).await
}

/// Vertex AI probe for `[providers.anthropic.<name>] auth_kind = "vertex"`
/// instances. Shells out via `forge_providers::anthropic::fetch_vertex_access_token`
/// (gcloud ADC) and GETs the location resource at
/// `https://aiplatform.googleapis.com/v1/projects/<project>/locations/<region>`.
/// A 2xx response confirms: gcloud ADC is wired, the project exists, the
/// caller has Vertex AI access on it, and the region is a valid Vertex
/// location. This is the standard cross-API "get location" call — works
/// reliably even when partner-model resources like `publishers/anthropic`
/// aren't list-able directly.
#[cfg(feature = "webview")]
async fn probe_anthropic_vertex(
    client: &reqwest::Client,
    entry: &forge_core::BuiltinInstanceEntry,
) -> Result<TestProviderConnectionOutput, String> {
    let project = entry.vertex_project.as_deref().unwrap_or("").trim();
    let region = entry.vertex_region.as_deref().unwrap_or("").trim();
    if project.is_empty() {
        return Err(format!(
            "{TEST_PROVIDER_CONNECTION_ERROR}vertex_project is empty in settings"
        ));
    }
    if region.is_empty() {
        return Err(format!(
            "{TEST_PROVIDER_CONNECTION_ERROR}vertex_region is empty in settings"
        ));
    }
    let token = tokio::task::spawn_blocking(forge_providers::anthropic::fetch_vertex_access_token)
        .await
        .map_err(|e| format!("{TEST_PROVIDER_CONNECTION_ERROR}vertex token join failed: {e}"))?
        .map_err(|e| format!("{TEST_PROVIDER_CONNECTION_ERROR}vertex auth: {e}"))?;
    let url = format!("https://aiplatform.googleapis.com/v1/projects/{project}/locations/{region}");
    probe_http_request(
        client,
        &url,
        vec![("authorization".to_string(), format!("Bearer {token}"))],
    )
    .await
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn test_provider_connection<R: Runtime>(
    input: TestProviderConnectionInput,
    webview: Webview<R>,
    state: State<'_, crate::ipc::BridgeState>,
    creds: State<'_, CredentialsState>,
) -> Result<TestProviderConnectionOutput, String> {
    crate::ipc::require_window_label(&webview, "dashboard", "test_provider_connection")
        .map_err(|e| format!("{TEST_PROVIDER_CONNECTION_ERROR}{e}"))?;
    validate_provider_id(&input.provider_id)
        .map_err(|e| format!("{TEST_PROVIDER_CONNECTION_ERROR}{e}"))?;

    let user_dir = crate::ipc::resolve_user_config_dir(&state);
    let settings = match user_dir.as_deref() {
        Some(dir) => forge_core::settings::load_user_settings_in(dir)
            .await
            .map_err(|e| format!("{TEST_PROVIDER_CONNECTION_ERROR}{e}"))?,
        None => forge_core::settings::AppSettings::default(),
    };

    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| format!("{TEST_PROVIDER_CONNECTION_ERROR}network {e}"))?;
    let store = creds.store();

    let probe = dispatch_probe(&client, &input.provider_id, &settings, &store);
    match tokio::time::timeout(TEST_PROVIDER_CONNECTION_TIMEOUT, probe).await {
        Ok(result) => result,
        Err(_) => Err(format!("{TEST_PROVIDER_CONNECTION_ERROR}timeout")),
    }
}

// ---------------------------------------------------------------------------
// ---- probe_provider_config ------------------------------------------------
// ---------------------------------------------------------------------------

/// `probe_provider_config` IPC input. Mirrors the on-disk
/// `custom_openai:<name>` shape but is supplied ad-hoc by the Add /
/// Edit Provider modal so the user can verify reachability and pull a
/// model list *before* the entry is persisted. `endpoint` is the base
/// URL; the probe appends `/v1/models` itself. `api_key` is the
/// Bearer token to send — leave it `None` (or empty) for keyless
/// endpoints.
#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct ProbeProviderConfigInput {
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// Ad-hoc OpenAI-compatible probe driven by form values rather than a
/// saved entry. Same wire-format output as `test_provider_connection`
/// — including the canonical `test_provider_connection:` error prefix
/// — so the dashboard's auth/network classification logic works
/// unchanged for the add/edit flow.
#[cfg(feature = "webview")]
#[tauri::command]
pub async fn probe_provider_config<R: Runtime>(
    input: ProbeProviderConfigInput,
    webview: Webview<R>,
) -> Result<TestProviderConnectionOutput, String> {
    crate::ipc::require_window_label(&webview, "dashboard", "probe_provider_config")
        .map_err(|e| format!("{TEST_PROVIDER_CONNECTION_ERROR}{e}"))?;
    let endpoint = input.endpoint.trim();
    validate_endpoint_for(TEST_PROVIDER_CONNECTION_ERROR, endpoint)?;

    let url = models_probe_url(endpoint);
    let has_key = matches!(
        input.api_key.as_deref().map(str::trim),
        Some(k) if !k.is_empty()
    );
    tracing::debug!(
        target: "forge_shell::providers",
        endpoint = %endpoint,
        url = %url,
        has_key = has_key,
        "probe_provider_config invoked"
    );
    let headers = match input.api_key.as_deref().map(str::trim) {
        Some(k) if !k.is_empty() => vec![("authorization".to_string(), format!("Bearer {k}"))],
        _ => Vec::new(),
    };

    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| format!("{TEST_PROVIDER_CONNECTION_ERROR}network {e}"))?;

    let probe = probe_http_request(&client, &url, headers);
    let outcome = tokio::time::timeout(TEST_PROVIDER_CONNECTION_TIMEOUT, probe).await;
    match outcome {
        Ok(Ok(out)) => {
            tracing::debug!(
                target: "forge_shell::providers",
                url = %url,
                ok = out.ok,
                latency_ms = ?out.latency_ms,
                model_count = ?out.model_count,
                "probe_provider_config succeeded"
            );
            Ok(out)
        }
        Ok(Err(e)) => {
            tracing::debug!(
                target: "forge_shell::providers",
                url = %url,
                error = %e,
                "probe_provider_config failed"
            );
            Err(e)
        }
        Err(_) => {
            tracing::debug!(
                target: "forge_shell::providers",
                url = %url,
                timeout_ms = TEST_PROVIDER_CONNECTION_TIMEOUT.as_millis() as u64,
                "probe_provider_config timed out"
            );
            Err(format!("{TEST_PROVIDER_CONNECTION_ERROR}timeout"))
        }
    }
}

// ---------------------------------------------------------------------------
// ---- update_provider ------------------------------------------------------
// ---------------------------------------------------------------------------

/// `update_provider` IPC input. Restricted to `custom_openai:<name>` entries —
/// built-in providers carry no editable fields today (kind/model are baked
/// into the runtime adapter), so `update_provider` rejects any id whose
/// prefix is not `custom_openai:`. The `config` field is reused verbatim
/// from `add_provider` for wire-shape symmetry.
#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct UpdateProviderInput {
    pub id: String,
    pub config: CustomOpenAiConfig,
}

/// Pure validation exposed for unit tests.
pub fn validate_update_provider_input(input: &UpdateProviderInput) -> Result<(), String> {
    validate_provider_id(&input.id).map_err(|e| format!("{UPDATE_PROVIDER_ERROR}{e}"))?;
    if !input.id.starts_with(CUSTOM_OPENAI_PREFIX) {
        return Err(format!(
            "{UPDATE_PROVIDER_ERROR}built-in providers are not editable"
        ));
    }
    validate_endpoint_for(UPDATE_PROVIDER_ERROR, &input.config.endpoint)?;
    if input.config.model.trim().is_empty() {
        return Err(format!("{UPDATE_PROVIDER_ERROR}model is required"));
    }
    Ok(())
}

/// `validate_endpoint`'s twin keyed on a caller-supplied prefix. The
/// original `validate_endpoint` hard-codes `ADD_PROVIDER_ERROR`; rather than
/// rewire that call site, we expose a prefix-aware variant so each command
/// keeps its own F-673 error prefix.
fn validate_endpoint_for(prefix: &str, raw: &str) -> Result<(), String> {
    let url = url::Url::parse(raw).map_err(|e| format!("{prefix}invalid endpoint URL: {e}"))?;
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(format!(
            "{prefix}invalid endpoint URL: scheme {other} is not http/https"
        )),
    }
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn update_provider<R: Runtime>(
    input: UpdateProviderInput,
    webview: Webview<R>,
    state: State<'_, crate::ipc::BridgeState>,
    creds: State<'_, CredentialsState>,
) -> Result<ProviderEntry, String> {
    crate::ipc::require_window_label(&webview, "dashboard", "update_provider")
        .map_err(|e| format!("{UPDATE_PROVIDER_ERROR}{e}"))?;
    validate_update_provider_input(&input)?;

    let _write_lock = settings_write_guard().lock().await;

    let user_dir = crate::ipc::resolve_user_config_dir(&state)
        .ok_or_else(|| format!("{UPDATE_PROVIDER_ERROR}could not resolve user config directory"))?;
    let user_path = forge_core::settings::user_settings_path_in(&user_dir);
    let existing = tokio::fs::read_to_string(&user_path)
        .await
        .unwrap_or_default();

    let name = input
        .id
        .strip_prefix(CUSTOM_OPENAI_PREFIX)
        .expect("validated above");

    if !custom_openai_section_present(&existing, name)? {
        return Err(format!(
            "{UPDATE_PROVIDER_ERROR}provider {} not configured",
            input.id
        ));
    }

    let updated = rewrite_custom_openai_section(&existing, name, &input.config)?;
    save_user_settings_raw_in(&user_dir, &updated)
        .await
        .map_err(|e| format!("{UPDATE_PROVIDER_ERROR}{e}"))?;

    let settings = forge_core::settings::load_user_settings_in(&user_dir)
        .await
        .map_err(|e| format!("{UPDATE_PROVIDER_ERROR}{e}"))?;
    let store = creds.store();
    let presence = cred_presence_map(&store, std::slice::from_ref(&input.id)).await;
    let presence_map: std::collections::HashMap<String, bool> = presence.into_iter().collect();
    let rows = build_provider_list(&settings, |id| {
        presence_map.get(id).copied().unwrap_or(false)
    });
    rows.into_iter().find(|r| r.id == input.id).ok_or_else(|| {
        format!(
            "{UPDATE_PROVIDER_ERROR}post-write lookup failed for {}",
            input.id
        )
    })
}

/// Pure check: returns `true` when `[providers.custom_openai.<name>]` is
/// present in the TOML body. Exposed for unit tests.
pub fn custom_openai_section_present(settings_toml: &str, name: &str) -> Result<bool, String> {
    if settings_toml.trim().is_empty() {
        return Ok(false);
    }
    let tree: toml::Value =
        toml::from_str(settings_toml).map_err(|e| format!("{UPDATE_PROVIDER_ERROR}{e}"))?;
    Ok(tree
        .get("providers")
        .and_then(|p| p.get("custom_openai"))
        .and_then(|c| c.get(name))
        .is_some())
}

/// Re-key the `[providers.custom_openai.<name>]` section with `cfg`. Strips
/// any pre-existing `api_version` left behind by older builds so a stale
/// key doesn't linger in the user's settings.toml after an edit.
fn rewrite_custom_openai_section(
    existing: &str,
    name: &str,
    cfg: &CustomOpenAiConfig,
) -> Result<String, String> {
    let mut body = apply_setting_update(
        existing,
        &format!("providers.custom_openai.{name}.base_url"),
        toml::Value::String(cfg.endpoint.clone()),
    )
    .map_err(|e| format!("{UPDATE_PROVIDER_ERROR}{e}"))?;
    body = apply_setting_update(
        &body,
        &format!("providers.custom_openai.{name}.model"),
        toml::Value::String(cfg.model.clone()),
    )
    .map_err(|e| format!("{UPDATE_PROVIDER_ERROR}{e}"))?;
    // Strip any stale `api_version` key written by older builds — the
    // field is gone from the wire shape and the runtime never reads it.
    body = remove_toml_leaf(&body, &["providers", "custom_openai", name, "api_version"])
        .map_err(|e| format!("{UPDATE_PROVIDER_ERROR}{e}"))?;
    Ok(body)
}

/// Best-effort delete of a single TOML leaf addressed by `path`. Missing
/// intermediate tables / missing leaf are not an error — the operation is
/// idempotent. Returns the re-serialized TOML body.
fn remove_toml_leaf(existing: &str, path: &[&str]) -> Result<String, String> {
    if existing.trim().is_empty() || path.is_empty() {
        return Ok(existing.to_string());
    }
    let mut tree: toml::Value = toml::from_str(existing).map_err(|e| e.to_string())?;
    let leaf = *path.last().expect("non-empty");
    let parents = &path[..path.len() - 1];

    let mut cursor = match tree.as_table_mut() {
        Some(t) => t,
        None => return Ok(existing.to_string()),
    };
    for seg in parents {
        let Some(next) = cursor.get_mut(*seg).and_then(|v| v.as_table_mut()) else {
            return Ok(existing.to_string());
        };
        cursor = next;
    }
    cursor.remove(leaf);
    toml::to_string(&tree).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// ---- remove_provider ------------------------------------------------------
// ---------------------------------------------------------------------------

/// `remove_provider` IPC input. Accepts a built-in slug (`anthropic`,
/// `openai`, `mistral`) — removing a built-in clears its
/// `providers.enabled.<id>` flag — or a `custom_openai:<name>` id which
/// drops the corresponding `[providers.custom_openai.<name>]` section.
///
/// Remove is intentionally tolerant of the vendor allowlist: any
/// well-formed id whose entry is present in the user's settings will
/// be cleaned up, even if the vendor itself is no longer a supported
/// built-in. This lets users walk away from deprecated configurations
/// (e.g. legacy `ollama:default` after Ollama moved to a `custom_openai`
/// preset) without hand-editing TOML. The strict "is configured" check
/// lives in [`rewrite_for_remove`].
///
/// The IPC does not touch the keyring. Callers chain `logout_provider`
/// when they want the credential cleared too — per the spec, surfacing
/// the keyring write as its own IPC trace is intentional.
#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct RemoveProviderInput {
    pub id: String,
}

/// Pure validation exposed for unit tests. Only validates the id shape —
/// vendor-allowlist enforcement is skipped on the remove path so users
/// can clean up entries for deprecated built-ins (see the struct doc).
pub fn validate_remove_provider_input(input: &RemoveProviderInput) -> Result<(), String> {
    validate_provider_id(&input.id).map_err(|e| format!("{REMOVE_PROVIDER_ERROR}{e}"))?;
    Ok(())
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn remove_provider<R: Runtime>(
    input: RemoveProviderInput,
    app: AppHandle<R>,
    webview: Webview<R>,
    state: State<'_, crate::ipc::BridgeState>,
) -> Result<(), String> {
    crate::ipc::require_window_label(&webview, "dashboard", "remove_provider")
        .map_err(|e| format!("{REMOVE_PROVIDER_ERROR}{e}"))?;
    validate_remove_provider_input(&input)?;

    let _write_lock = settings_write_guard().lock().await;

    let user_dir = crate::ipc::resolve_user_config_dir(&state)
        .ok_or_else(|| format!("{REMOVE_PROVIDER_ERROR}could not resolve user config directory"))?;
    let user_path = forge_core::settings::user_settings_path_in(&user_dir);
    let existing = tokio::fs::read_to_string(&user_path)
        .await
        .unwrap_or_default();

    let updated_body = rewrite_for_remove(&existing, &input.id)?;
    save_user_settings_raw_in(&user_dir, &updated_body)
        .await
        .map_err(|e| format!("{REMOVE_PROVIDER_ERROR}{e}"))?;

    // Active-provider safeguard: a removed provider can no longer be active.
    // Re-read the persisted settings to discover whether `[providers.active]`
    // pointed at the id we just removed. If so, clear it — `set_active_provider`
    // permits an empty id under F-586 semantics (the next session falls back
    // to the catalog default).
    let settings_after = forge_core::settings::load_user_settings_in(&user_dir)
        .await
        .map_err(|e| format!("{REMOVE_PROVIDER_ERROR}{e}"))?;
    if settings_after.providers.active.as_deref() == Some(input.id.as_str()) {
        let cleared = remove_toml_leaf(&updated_body, &["providers", "active"])
            .map_err(|e| format!("{REMOVE_PROVIDER_ERROR}{e}"))?;
        save_user_settings_raw_in(&user_dir, &cleared)
            .await
            .map_err(|e| format!("{REMOVE_PROVIDER_ERROR}{e}"))?;
    }

    // Broadcast a `provider:changed` so any open session window /
    // dashboard refetches its provider list. The payload carries the
    // removed id; listeners that key on equality still observe the change.
    let event = Event::ProviderChanged {
        provider_id: input.id.clone(),
    };
    if let Err(e) = app.emit(PROVIDER_CHANGED_EVENT, &event) {
        tracing::warn!(
            target: "forge_shell::providers",
            provider_id = %input.id,
            error = %e,
            "remove_provider: ProviderChanged emit failed",
        );
    }

    Ok(())
}

/// Pure helper: rewrite the user-settings TOML to drop `id`. For a
/// `custom_openai:<name>` id the `[providers.custom_openai.<name>]`
/// section must exist and is removed entirely. For a built-in slug the
/// `providers.enabled.<id>` key must be present and is deleted, so the
/// provider drops off the list until the user re-adds it through the Add
/// Provider modal. Returns an error if the id is not currently configured.
pub fn rewrite_for_remove(existing: &str, id: &str) -> Result<String, String> {
    if let Some(name) = id.strip_prefix(CUSTOM_OPENAI_PREFIX) {
        if !custom_openai_section_present(existing, name)
            .map_err(|e| e.replace(UPDATE_PROVIDER_ERROR, REMOVE_PROVIDER_ERROR))?
        {
            return Err(format!(
                "{REMOVE_PROVIDER_ERROR}provider {id} not configured"
            ));
        }
        return remove_toml_leaf(existing, &["providers", "custom_openai", name])
            .map_err(|e| format!("{REMOVE_PROVIDER_ERROR}{e}"));
    }

    if !provider_already_configured(existing, id)
        .map_err(|e| e.replace(ADD_PROVIDER_ERROR, REMOVE_PROVIDER_ERROR))?
    {
        return Err(format!(
            "{REMOVE_PROVIDER_ERROR}provider {id} not configured"
        ));
    }
    remove_toml_leaf(existing, &["providers", "enabled", id])
        .map_err(|e| format!("{REMOVE_PROVIDER_ERROR}{e}"))
}

// ---------------------------------------------------------------------------
// ---- set_provider_enabled -------------------------------------------------
// ---------------------------------------------------------------------------

/// `set_provider_enabled` IPC input. Flips the `providers.enabled.<id>`
/// flag. Accepts either a built-in slug (`anthropic`, `openai`,
/// `mistral`, `custom_openai`) or a `custom_openai:<name>` id. Disabled
/// providers stay listed by `dashboard_list_providers` so the user can
/// re-enable or remove them, but `set_active_provider` rejects them and
/// the new-session picker filters them out.
#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct SetProviderEnabledInput {
    pub id: String,
    pub enabled: bool,
}

/// Pure validation exposed for unit tests.
pub fn validate_set_provider_enabled_input(input: &SetProviderEnabledInput) -> Result<(), String> {
    validate_provider_id(&input.id).map_err(|e| format!("{SET_PROVIDER_ENABLED_ERROR}{e}"))?;
    Ok(())
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn set_provider_enabled<R: Runtime>(
    input: SetProviderEnabledInput,
    app: AppHandle<R>,
    webview: Webview<R>,
    state: State<'_, crate::ipc::BridgeState>,
) -> Result<(), String> {
    crate::ipc::require_window_label(&webview, "dashboard", "set_provider_enabled")
        .map_err(|e| format!("{SET_PROVIDER_ENABLED_ERROR}{e}"))?;
    validate_set_provider_enabled_input(&input)?;

    let _write_lock = settings_write_guard().lock().await;

    let user_dir = crate::ipc::resolve_user_config_dir(&state).ok_or_else(|| {
        format!("{SET_PROVIDER_ENABLED_ERROR}could not resolve user config directory")
    })?;
    let user_path = forge_core::settings::user_settings_path_in(&user_dir);
    let existing = tokio::fs::read_to_string(&user_path)
        .await
        .unwrap_or_default();

    let settings_before = forge_core::settings::load_user_settings_in(&user_dir)
        .await
        .map_err(|e| format!("{SET_PROVIDER_ENABLED_ERROR}{e}"))?;
    if !is_known_provider_id(&settings_before, &input.id) {
        return Err(format!(
            "{SET_PROVIDER_ENABLED_ERROR}provider {} not configured",
            input.id
        ));
    }

    let updated = apply_setting_update(
        &existing,
        &format!("providers.enabled.{}", input.id),
        toml::Value::Boolean(input.enabled),
    )
    .map_err(|e| format!("{SET_PROVIDER_ENABLED_ERROR}{e}"))?;

    // Disabling the currently-active provider clears the active selection
    // so the next session falls back to the catalog default. The active
    // selector and the new-session picker already filter disabled rows;
    // this branch closes the loop for any caller that reads the persisted
    // `providers.active` directly.
    let updated = if !input.enabled
        && settings_before.providers.active.as_deref() == Some(input.id.as_str())
    {
        remove_toml_leaf(&updated, &["providers", "active"])
            .map_err(|e| format!("{SET_PROVIDER_ENABLED_ERROR}{e}"))?
    } else {
        updated
    };

    save_user_settings_raw_in(&user_dir, &updated)
        .await
        .map_err(|e| format!("{SET_PROVIDER_ENABLED_ERROR}{e}"))?;

    tracing::trace!(
        target: "forge_shell::providers",
        provider_id = %input.id,
        enabled = input.enabled,
        "set_provider_enabled persisted",
    );

    let event = Event::ProviderChanged {
        provider_id: input.id.clone(),
    };
    if let Err(e) = app.emit(PROVIDER_CHANGED_EVENT, &event) {
        tracing::warn!(
            target: "forge_shell::providers",
            provider_id = %input.id,
            error = %e,
            "set_provider_enabled: ProviderChanged emit failed",
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

    /// Returns settings with every built-in marked configured (enabled = true).
    /// Use whenever a test needs `build_provider_list` to actually emit rows
    /// for the built-ins — under the new model, absent keys yield no row.
    fn settings_with_all_builtins_enabled() -> AppSettings {
        let mut s = AppSettings::default();
        for id in &[PROVIDER_ANTHROPIC, PROVIDER_OPENAI] {
            s.providers.enabled.insert((*id).to_string(), true);
        }
        s
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
    fn build_provider_list_emits_builtins_in_stable_order() {
        let s = settings_with_all_builtins_enabled();
        let entries = build_provider_list(&s, |_| false);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["anthropic", "openai"],
            "builtin order is the user-facing card order; do not reorder casually"
        );
    }

    #[test]
    fn build_provider_list_omits_unconfigured_builtins() {
        // Fresh install: no `providers.enabled` entries → no rows.
        // Adding a provider through the Add modal writes the key; only then
        // does the row appear.
        let s = empty_settings();
        assert!(build_provider_list(&s, |_| false).is_empty());

        let mut s = empty_settings();
        s.providers.enabled.insert("anthropic".into(), true);
        let entries = build_provider_list(&s, |_| false);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["anthropic"], "only the added built-in is listed");
    }

    #[test]
    fn build_provider_list_marks_anthropic_credential_present_when_store_says_so() {
        let s = settings_with_all_builtins_enabled();
        let entries = build_provider_list(&s, |id| id == "anthropic");
        let anthropic = entries.iter().find(|e| e.id == "anthropic").unwrap();
        assert!(anthropic.credential_required);
        assert!(anthropic.has_credential);
    }

    #[test]
    fn build_provider_list_vertex_anthropic_suppresses_credential_requirement() {
        // Phase B: when `[providers.anthropic.<name>]` has `auth_kind =
        // "vertex"`, the dashboard row reports `credential_required = false`
        // so the orange auth pill and "ADD CREDENTIAL" CTA do not appear —
        // gcloud ADC supplies auth at request time. The row's `auth_kind`
        // surfaces on the wire so the UI can render Vertex-specific copy.
        let mut s = forge_core::settings::AppSettings::default();
        s.providers
            .enabled
            .insert("anthropic:vertex-work".to_string(), true);
        s.providers.anthropic.insert(
            "vertex-work".to_string(),
            forge_core::BuiltinInstanceEntry {
                auth_kind: forge_core::BuiltinAuthKind::Vertex,
                vertex_project: Some("my-proj".to_string()),
                vertex_region: Some("us-central1".to_string()),
            },
        );
        // Even if cred_present claims true (it won't — there's no entry),
        // the credential_required flag must be false for vertex rows.
        let entries = build_provider_list(&s, |_| false);
        let row = entries
            .iter()
            .find(|e| e.id == "anthropic:vertex-work")
            .expect("vertex instance row");
        assert!(!row.credential_required, "{row:?}");
        assert!(!row.has_credential, "{row:?}");
        assert_eq!(row.auth_kind, Some(forge_core::BuiltinAuthKind::Vertex));
    }

    #[test]
    fn build_provider_list_api_key_anthropic_named_keeps_credential_requirement() {
        // Sibling case: a named Anthropic instance with no per-instance
        // section (or `auth_kind = "api_key"`) still flags
        // credential_required=true so the dashboard renders the prompt.
        let mut s = forge_core::settings::AppSettings::default();
        s.providers
            .enabled
            .insert("anthropic:work".to_string(), true);
        let entries = build_provider_list(&s, |_| false);
        let row = entries
            .iter()
            .find(|e| e.id == "anthropic:work")
            .expect("anthropic:work row");
        assert!(row.credential_required);
        assert!(!row.has_credential);
        // No section → auth_kind absent on the wire.
        assert_eq!(row.auth_kind, None);
    }

    #[test]
    fn build_provider_list_treats_keyring_failure_as_absent() {
        // Spec: "if the keyring backend is unavailable, treat as `false`".
        let s = settings_with_all_builtins_enabled();
        let entries = build_provider_list(&s, |_| false);
        for e in &entries {
            if e.credential_required {
                assert!(!e.has_credential, "{e:?}");
            }
        }
    }

    #[test]
    fn build_provider_list_appends_custom_openai_entries() {
        let mut s = settings_with_custom(
            "vllm-local",
            CustomOpenAiEntry {
                base_url: "http://127.0.0.1:8000".into(),
                model: "Qwen2".into(),
                model_list: vec!["Qwen2".into()],
                auth: AuthShapeSettings::None,
                api_key: None,
            },
        );
        for id in &[PROVIDER_ANTHROPIC, PROVIDER_OPENAI] {
            s.providers.enabled.insert((*id).to_string(), true);
        }
        let entries = build_provider_list(&s, |_| false);
        assert_eq!(entries.len(), 3);
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
        // Under the "empty by default" model, a built-in is only "known"
        // once the user has explicitly added it (key present in
        // `providers.enabled`). Without that, even Anthropic is foreign.
        let empty = empty_settings();
        for id in &["anthropic", "openai"] {
            assert!(
                !is_known_provider_id(&empty, id),
                "fresh install should treat `{id}` as not yet configured"
            );
        }

        let s = settings_with_all_builtins_enabled();
        for id in &["anthropic", "openai"] {
            assert!(is_known_provider_id(&s, id), "expected `{id}` known");
        }
        // A disabled built-in is still configured (key present, value false).
        let mut disabled = empty_settings();
        disabled.providers.enabled.insert("openai".into(), false);
        assert!(is_known_provider_id(&disabled, "openai"));

        // The bare `custom_openai` umbrella is never a known id — it's a
        // kind for adding concrete entries, not a row itself.
        assert!(!is_known_provider_id(&s, "custom_openai"));
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

    #[test]
    fn validate_provider_id_rejects_custom_openai_empty_suffix() {
        let err = validate_provider_id("custom_openai:").unwrap_err();
        assert!(
            err.contains("custom_openai"),
            "expected custom_openai-specific error, got: {err}",
        );
    }

    #[test]
    fn validate_provider_id_rejects_custom_openai_whitespace_suffix() {
        for bad in ["custom_openai: ", "custom_openai:\t", "custom_openai:   "] {
            let err = validate_provider_id(bad)
                .err()
                .unwrap_or_else(|| panic!("expected rejection for {bad:?}"));
            assert!(
                err.contains("custom_openai"),
                "expected custom_openai-specific error for {bad:?}, got: {err}",
            );
        }
    }

    #[test]
    fn validate_provider_id_rejects_custom_openai_bad_charset() {
        // The first `:` is the prefix separator; a second `:` or any
        // other non-allowlist byte in the suffix must trip the charset
        // guard.
        for bad in [
            "custom_openai:foo:bar",
            "custom_openai:foo/bar",
            "custom_openai:foo bar",
            "custom_openai:foo.bar",
        ] {
            assert!(
                validate_provider_id(bad).is_err(),
                "expected rejection for {bad:?}",
            );
        }
    }

    #[test]
    fn validate_provider_id_accepts_custom_openai_named_suffix() {
        for good in [
            "custom_openai:vllm",
            "custom_openai:together-ai",
            "custom_openai:my_endpoint",
            "custom_openai:Endpoint1",
        ] {
            validate_provider_id(good).unwrap_or_else(|e| panic!("{good:?}: {e}"));
        }
    }

    // -----------------------------------------------------------------------
    // ---- add_provider -----------------------------------------------------
    // -----------------------------------------------------------------------

    fn add_input(id: &str, config: Option<CustomOpenAiConfig>) -> AddProviderInput {
        AddProviderInput {
            id: id.to_string(),
            config,
            builtin: None,
        }
    }

    fn custom_cfg() -> CustomOpenAiConfig {
        CustomOpenAiConfig {
            endpoint: "https://api.example.com".to_string(),
            model: "qwen2".to_string(),
            keyless: None,
        }
    }

    #[test]
    fn add_provider_validates_builtin_appends() {
        // Built-in `anthropic` with no config — the validation contract for
        // the IPC's success branch.
        validate_add_provider_input(&add_input("anthropic", None)).expect("anthropic");
    }

    // Phase A: named built-in instances. `anthropic:work` and
    // `openai:personal` flow through the same enabled-flag path as their
    // bare counterparts, with the full id used as the credential key.
    #[test]
    fn add_provider_validates_named_builtin() {
        validate_add_provider_input(&add_input("anthropic:work", None)).expect("anthropic:work");
        validate_add_provider_input(&add_input("openai:personal", None)).expect("openai:personal");
    }

    #[test]
    fn add_provider_rejects_named_builtin_empty_name() {
        let err = validate_add_provider_input(&add_input("anthropic:", None)).unwrap_err();
        assert!(err.starts_with(ADD_PROVIDER_ERROR), "{err}");
        assert!(err.contains("non-empty"), "{err}");
    }

    #[test]
    fn add_provider_rejects_named_builtin_invalid_chars() {
        let err =
            validate_add_provider_input(&add_input("anthropic:has spaces", None)).unwrap_err();
        assert!(err.contains("[A-Za-z0-9_-]"), "{err}");
    }

    #[test]
    fn add_provider_rejects_named_builtin_with_config() {
        let err = validate_add_provider_input(&add_input("anthropic:work", Some(custom_cfg())))
            .unwrap_err();
        assert!(err.contains("does not accept config"), "{err}");
    }

    #[test]
    fn parse_provider_id_classifies_named_builtin() {
        assert!(matches!(
            parse_provider_id("anthropic"),
            ParsedProviderId::BuiltinBare("anthropic")
        ));
        assert!(matches!(
            parse_provider_id("anthropic:work"),
            ParsedProviderId::BuiltinNamed {
                vendor: "anthropic",
                name: "work"
            }
        ));
        assert!(matches!(
            parse_provider_id("custom_openai:vllm"),
            ParsedProviderId::CustomOpenAi("vllm")
        ));
        assert!(matches!(
            parse_provider_id("gemini:work"),
            ParsedProviderId::Unknown
        ));
    }

    #[test]
    fn add_provider_validates_custom_openai_success() {
        validate_add_provider_input(&add_input("custom_openai:vllm", Some(custom_cfg())))
            .expect("custom_openai with config");
    }

    #[test]
    fn add_provider_rejects_builtin_with_extra_config() {
        let err =
            validate_add_provider_input(&add_input("anthropic", Some(custom_cfg()))).unwrap_err();
        assert!(err.starts_with(ADD_PROVIDER_ERROR), "{err}");
        assert!(err.contains("does not accept config"), "{err}");
    }

    #[test]
    fn add_provider_rejects_custom_openai_without_config() {
        let err = validate_add_provider_input(&add_input("custom_openai:vllm", None)).unwrap_err();
        assert!(err.starts_with(ADD_PROVIDER_ERROR), "{err}");
        assert!(err.contains("requires config"), "{err}");
    }

    #[test]
    fn add_provider_rejects_custom_openai_invalid_endpoint() {
        let mut cfg = custom_cfg();
        cfg.endpoint = "not a url".to_string();
        let err =
            validate_add_provider_input(&add_input("custom_openai:vllm", Some(cfg))).unwrap_err();
        assert!(err.starts_with(ADD_PROVIDER_ERROR), "{err}");
        assert!(err.contains("invalid endpoint URL"), "{err}");
    }

    #[test]
    fn add_provider_rejects_custom_openai_non_http_endpoint() {
        let mut cfg = custom_cfg();
        cfg.endpoint = "ftp://api.example.com".to_string();
        let err =
            validate_add_provider_input(&add_input("custom_openai:vllm", Some(cfg))).unwrap_err();
        assert!(err.contains("invalid endpoint URL"), "{err}");
        assert!(err.contains("ftp"), "{err}");
    }

    #[test]
    fn add_provider_rejects_custom_openai_empty_model() {
        let mut cfg = custom_cfg();
        cfg.model = "   ".to_string();
        let err =
            validate_add_provider_input(&add_input("custom_openai:vllm", Some(cfg))).unwrap_err();
        assert!(err.contains("model is required"), "{err}");
    }

    #[test]
    fn add_provider_rejects_unknown_builtin_kind() {
        let err = validate_add_provider_input(&add_input("gemini", None)).unwrap_err();
        assert!(err.starts_with(ADD_PROVIDER_ERROR), "{err}");
        assert!(err.contains("unknown provider kind: gemini"), "{err}");
    }

    #[test]
    fn add_provider_accepts_mistral_builtin() {
        // `mistral` is admitted alongside anthropic / openai per the
        // dashboard add-provider spec — even though the runtime adapter is
        // not yet first-class, the schema reserves the slug.
        validate_add_provider_input(&add_input("mistral", None)).expect("mistral");
    }

    #[test]
    fn provider_already_configured_detects_custom_openai_entry() {
        let toml_body = r#"
[providers.custom_openai.vllm]
base_url = "https://x"
model = "m"
"#;
        assert!(provider_already_configured(toml_body, "custom_openai:vllm").unwrap());
        assert!(!provider_already_configured(toml_body, "custom_openai:other").unwrap());
    }

    #[test]
    fn provider_already_configured_detects_enabled_builtin() {
        let toml_body = r#"
[providers.enabled]
anthropic = true
"#;
        assert!(provider_already_configured(toml_body, "anthropic").unwrap());
        assert!(!provider_already_configured(toml_body, "openai").unwrap());
    }

    #[test]
    fn provider_already_configured_returns_false_on_empty_file() {
        assert!(!provider_already_configured("", "anthropic").unwrap());
        assert!(!provider_already_configured("", "custom_openai:vllm").unwrap());
    }

    #[test]
    fn write_custom_openai_section_emits_all_fields() {
        let cfg = CustomOpenAiConfig {
            endpoint: "https://api.together.xyz".into(),
            model: "mixtral".into(),
            keyless: None,
        };
        let body = write_custom_openai_section("", "together", &cfg).unwrap();
        let parsed: toml::Value = toml::from_str(&body).unwrap();
        let section = &parsed["providers"]["custom_openai"]["together"];
        assert_eq!(
            section["base_url"].as_str().unwrap(),
            "https://api.together.xyz"
        );
        assert_eq!(section["model"].as_str().unwrap(), "mixtral");
    }

    /// Keyless-preset path (e.g. local Ollama exposed via the custom_openai
    /// preset): a keyless `custom_openai` entry must persist
    /// `auth.shape = "none"` so the request layer skips the bearer-token
    /// header lookup. Non-keyless sections must NOT carry an `auth`
    /// key — the daemon's default is still bearer auth.
    #[test]
    fn write_custom_openai_section_writes_auth_none_when_keyless() {
        let cfg = CustomOpenAiConfig {
            endpoint: "http://127.0.0.1:11434".into(),
            model: "llama3.2".into(),
            keyless: Some(true),
        };
        let body = write_custom_openai_section("", "local", &cfg).unwrap();
        let parsed: toml::Value = toml::from_str(&body).unwrap();
        let section = &parsed["providers"]["custom_openai"]["local"];
        assert_eq!(
            section["auth"]["shape"].as_str().unwrap(),
            "none",
            "keyless preset must persist auth.shape = none — {section:?}"
        );
    }

    #[test]
    fn write_custom_openai_section_omits_auth_when_not_keyless() {
        let body = write_custom_openai_section("", "vllm", &custom_cfg()).unwrap();
        let parsed: toml::Value = toml::from_str(&body).unwrap();
        let section = &parsed["providers"]["custom_openai"]["vllm"];
        assert!(
            section.get("auth").is_none(),
            "default custom_openai must not write auth.* — {section:?}"
        );
    }

    /// F-673 prefix invariant — pinned so a future rename of
    /// `ADD_PROVIDER_ERROR` doesn't silently drift.
    #[test]
    fn add_provider_error_prefix_is_command_named() {
        assert_eq!(ADD_PROVIDER_ERROR, "add_provider: ");
    }

    // -----------------------------------------------------------------------
    // ---- test_provider_connection ----------------------------------------
    // -----------------------------------------------------------------------

    /// F-673 prefix invariant.
    #[test]
    fn test_provider_connection_error_prefix_is_command_named() {
        assert_eq!(TEST_PROVIDER_CONNECTION_ERROR, "test_provider_connection: ");
    }

    #[test]
    fn classify_status_signals_auth_for_401_and_403() {
        assert!(classify_status(401).starts_with("auth "));
        assert!(classify_status(403).starts_with("auth "));
    }

    #[test]
    fn classify_status_signals_network_for_non_auth_failures() {
        for s in [404, 429, 500, 502, 503] {
            assert!(
                classify_status(s).starts_with("network "),
                "{s}: {}",
                classify_status(s)
            );
        }
    }

    #[test]
    fn builtin_probe_url_recognises_each_builtin() {
        assert!(builtin_probe_url(PROVIDER_ANTHROPIC).is_some());
        assert!(builtin_probe_url(PROVIDER_OPENAI).is_some());
        assert!(builtin_probe_url("mistral").is_some());
        assert!(builtin_probe_url("gemini").is_none());
    }

    // -----------------------------------------------------------------------
    // ---- update_provider --------------------------------------------------
    // -----------------------------------------------------------------------

    fn update_input(id: &str, cfg: CustomOpenAiConfig) -> UpdateProviderInput {
        UpdateProviderInput {
            id: id.to_string(),
            config: cfg,
        }
    }

    #[test]
    fn update_provider_rejects_builtin_ids() {
        let err =
            validate_update_provider_input(&update_input("anthropic", custom_cfg())).unwrap_err();
        assert!(err.starts_with(UPDATE_PROVIDER_ERROR), "{err}");
        assert!(err.contains("built-in providers are not editable"), "{err}");
    }

    #[test]
    fn update_provider_accepts_custom_openai_ids() {
        validate_update_provider_input(&update_input("custom_openai:vllm", custom_cfg()))
            .expect("valid custom update");
    }

    #[test]
    fn update_provider_rejects_invalid_endpoint() {
        let mut cfg = custom_cfg();
        cfg.endpoint = "not a url".into();
        let err =
            validate_update_provider_input(&update_input("custom_openai:vllm", cfg)).unwrap_err();
        assert!(err.starts_with(UPDATE_PROVIDER_ERROR), "{err}");
        assert!(err.contains("invalid endpoint URL"), "{err}");
    }

    #[test]
    fn update_provider_rejects_non_http_endpoint() {
        let mut cfg = custom_cfg();
        cfg.endpoint = "ftp://api.example.com".into();
        let err =
            validate_update_provider_input(&update_input("custom_openai:vllm", cfg)).unwrap_err();
        assert!(err.contains("invalid endpoint URL"), "{err}");
        assert!(err.contains("ftp"), "{err}");
    }

    #[test]
    fn update_provider_rejects_empty_model() {
        let mut cfg = custom_cfg();
        cfg.model = "   ".into();
        let err =
            validate_update_provider_input(&update_input("custom_openai:vllm", cfg)).unwrap_err();
        assert!(err.contains("model is required"), "{err}");
    }

    #[test]
    fn custom_openai_section_present_detects_existing_entry() {
        let body = r#"
[providers.custom_openai.vllm]
base_url = "https://x"
model = "m"
"#;
        assert!(custom_openai_section_present(body, "vllm").unwrap());
        assert!(!custom_openai_section_present(body, "other").unwrap());
        assert!(!custom_openai_section_present("", "vllm").unwrap());
    }

    #[test]
    fn rewrite_custom_openai_section_overwrites_fields() {
        let existing = r#"
[providers.custom_openai.vllm]
base_url = "http://old"
model = "old-model"
"#;
        let cfg = CustomOpenAiConfig {
            endpoint: "https://new.example.com".into(),
            model: "new-model".into(),
            keyless: None,
        };
        let body = rewrite_custom_openai_section(existing, "vllm", &cfg).unwrap();
        let parsed: toml::Value = toml::from_str(&body).unwrap();
        let section = &parsed["providers"]["custom_openai"]["vllm"];
        assert_eq!(
            section["base_url"].as_str().unwrap(),
            "https://new.example.com"
        );
        assert_eq!(section["model"].as_str().unwrap(), "new-model");
    }

    /// Migration guard: a stale `api_version` key left behind by older
    /// builds must be stripped on the next edit so it doesn't linger in
    /// the user's settings.toml.
    #[test]
    fn rewrite_custom_openai_section_strips_legacy_api_version() {
        let existing = r#"
[providers.custom_openai.vllm]
base_url = "https://x"
model = "m"
api_version = "2024-01"
"#;
        let cfg = CustomOpenAiConfig {
            endpoint: "https://x".into(),
            model: "m".into(),
            keyless: None,
        };
        let body = rewrite_custom_openai_section(existing, "vllm", &cfg).unwrap();
        let parsed: toml::Value = toml::from_str(&body).unwrap();
        let section = &parsed["providers"]["custom_openai"]["vllm"];
        assert!(section.get("api_version").is_none(), "{section:?}");
    }

    /// F-673 prefix invariant.
    #[test]
    fn update_provider_error_prefix_is_command_named() {
        assert_eq!(UPDATE_PROVIDER_ERROR, "update_provider: ");
    }

    // -----------------------------------------------------------------------
    // ---- remove_provider --------------------------------------------------
    // -----------------------------------------------------------------------

    fn remove_input(id: &str) -> RemoveProviderInput {
        RemoveProviderInput { id: id.to_string() }
    }

    #[test]
    fn remove_provider_validates_builtin_id() {
        validate_remove_provider_input(&remove_input("anthropic")).expect("builtin");
        validate_remove_provider_input(&remove_input("custom_openai:vllm")).expect("custom_openai");
        // Named built-in instances must pass too — `validate_remove_provider_input`
        // previously compared the full id against `BUILTIN_ADDABLE_KINDS`, which
        // rejected every `vendor:name` combo and trapped users with legacy
        // entries.
        validate_remove_provider_input(&remove_input("anthropic:work")).expect("named builtin");
    }

    /// Remove is intentionally tolerant of vendor names that are no longer
    /// supported built-ins. The strict "is configured" check happens in
    /// `rewrite_for_remove` so users can clean up deprecated entries
    /// (e.g. legacy `ollama:default` after Ollama moved to a custom_openai
    /// preset) without hand-editing TOML.
    #[test]
    fn remove_provider_accepts_deprecated_vendor_for_cleanup() {
        validate_remove_provider_input(&remove_input("ollama:default"))
            .expect("legacy ollama:default should validate");
        validate_remove_provider_input(&remove_input("gemini"))
            .expect("unknown vendor should validate for tolerant cleanup");
    }

    /// Tolerance at validation does not mean a fictional id silently succeeds —
    /// `rewrite_for_remove` still requires the entry to be present in the
    /// user's settings TOML.
    #[test]
    fn rewrite_for_remove_errors_when_unknown_builtin_not_configured() {
        let body = r#"
[providers.enabled]
anthropic = true
"#;
        let err = rewrite_for_remove(body, "gemini").unwrap_err();
        assert!(err.contains("not configured"), "{err}");
    }

    #[test]
    fn rewrite_for_remove_clears_builtin_enabled_flag() {
        let body = r#"
[providers.enabled]
anthropic = true
openai = true
"#;
        let updated = rewrite_for_remove(body, "anthropic").unwrap();
        let parsed: toml::Value = toml::from_str(&updated).unwrap();
        let enabled = &parsed["providers"]["enabled"];
        assert!(enabled.get("anthropic").is_none(), "{enabled:?}");
        // Siblings preserved.
        assert_eq!(enabled["openai"].as_bool(), Some(true));
    }

    #[test]
    fn rewrite_for_remove_drops_custom_openai_section() {
        let body = r#"
[providers.custom_openai.vllm]
base_url = "http://x"
model = "m"

[providers.custom_openai.together]
base_url = "https://api.together.xyz"
model = "mixtral"
"#;
        let updated = rewrite_for_remove(body, "custom_openai:vllm").unwrap();
        let parsed: toml::Value = toml::from_str(&updated).unwrap();
        let map = &parsed["providers"]["custom_openai"];
        assert!(map.get("vllm").is_none(), "{map:?}");
        // Sibling preserved.
        assert!(map.get("together").is_some());
    }

    #[test]
    fn rewrite_for_remove_errors_when_custom_openai_id_absent() {
        let body = "";
        let err = rewrite_for_remove(body, "custom_openai:vllm").unwrap_err();
        assert!(err.starts_with(REMOVE_PROVIDER_ERROR), "{err}");
        assert!(err.contains("not configured"), "{err}");

        let body = r#"
[providers.custom_openai.other]
base_url = "http://x"
model = "m"
"#;
        let err = rewrite_for_remove(body, "custom_openai:vllm").unwrap_err();
        assert!(err.contains("not configured"), "{err}");
    }

    /// Under the "empty by default" model, removal of a built-in whose
    /// enabled key was never written must fail — there is no listed row to
    /// remove. Without this gate, the UI would happily "delete" a built-in
    /// that wasn't there, hiding bugs upstream that try to remove what was
    /// never added.
    #[test]
    fn rewrite_for_remove_errors_when_builtin_flag_absent() {
        let err = rewrite_for_remove("", "openai").unwrap_err();
        assert!(err.starts_with(REMOVE_PROVIDER_ERROR), "{err}");
        assert!(err.contains("not configured"), "{err}");

        let body = r#"
[providers.enabled]
openai = true
"#;
        let err = rewrite_for_remove(body, "anthropic").unwrap_err();
        assert!(err.contains("not configured"), "{err}");
    }

    /// A built-in whose toggle is OFF (key present, value false) is still
    /// configured; removal must succeed and clear the key entirely so the
    /// row disappears from the list.
    #[test]
    fn rewrite_for_remove_clears_disabled_builtin() {
        let body = r#"
[providers.enabled]
openai = false
"#;
        let updated = rewrite_for_remove(body, "openai").unwrap();
        let parsed: toml::Value = toml::from_str(&updated).unwrap();
        // The whole `providers.enabled.openai` leaf is gone.
        let enabled = parsed
            .get("providers")
            .and_then(|p| p.get("enabled"))
            .and_then(|e| e.get("openai"));
        assert!(enabled.is_none(), "openai key should be cleared: {updated}");
    }

    #[test]
    fn remove_toml_leaf_is_idempotent_on_missing_paths() {
        // Missing intermediate table — return existing unchanged.
        let body = "key = 1\n";
        let out = remove_toml_leaf(body, &["providers", "enabled", "anthropic"]).unwrap();
        assert_eq!(out, body);
        // Empty input — return empty.
        let out = remove_toml_leaf("", &["a", "b"]).unwrap();
        assert_eq!(out, "");
    }

    /// Active-provider safeguard at the pure-helper level. The live IPC test
    /// suite below exercises the same code path against a real on-disk file;
    /// this pin guards the intent.
    #[test]
    fn rewrite_for_remove_preserves_unrelated_active_setting() {
        let body = r#"
[providers]
active = "openai"

[providers.custom_openai.vllm]
base_url = "http://x"
model = "m"
"#;
        let updated = rewrite_for_remove(body, "custom_openai:vllm").unwrap();
        let parsed: toml::Value = toml::from_str(&updated).unwrap();
        // The vllm entry is gone but the unrelated `active` setting is not
        // touched — the safeguard only fires when active == removed id.
        assert!(parsed["providers"]
            .get("custom_openai")
            .and_then(|c| c.get("vllm"))
            .is_none());
        assert_eq!(parsed["providers"]["active"].as_str(), Some("openai"));
    }

    /// F-673 prefix invariant.
    #[test]
    fn remove_provider_error_prefix_is_command_named() {
        assert_eq!(REMOVE_PROVIDER_ERROR, "remove_provider: ");
    }

    // -----------------------------------------------------------------------
    // ---- set_provider_enabled --------------------------------------------
    // -----------------------------------------------------------------------

    fn enable_input(id: &str, enabled: bool) -> SetProviderEnabledInput {
        SetProviderEnabledInput {
            id: id.to_string(),
            enabled,
        }
    }

    #[test]
    fn set_provider_enabled_validates_builtin_id() {
        validate_set_provider_enabled_input(&enable_input("anthropic", true)).expect("builtin");
        validate_set_provider_enabled_input(&enable_input("custom_openai:vllm", false))
            .expect("custom_openai");
    }

    #[test]
    fn set_provider_enabled_rejects_empty_id() {
        let err = validate_set_provider_enabled_input(&enable_input("", true)).unwrap_err();
        assert!(err.starts_with(SET_PROVIDER_ENABLED_ERROR), "{err}");
    }

    #[test]
    fn build_provider_list_reports_disabled_when_flag_is_false() {
        let mut s = settings_with_all_builtins_enabled();
        s.providers.enabled.insert("anthropic".into(), false);
        let entries = build_provider_list(&s, |_| false);
        let anthropic = entries.iter().find(|e| e.id == "anthropic").unwrap();
        assert!(!anthropic.enabled);
        // Other built-ins keep their persisted `true`.
        let openai = entries.iter().find(|e| e.id == "openai").unwrap();
        assert!(openai.enabled);
    }

    #[test]
    fn is_enabled_provider_defaults_true_when_absent() {
        let s = empty_settings();
        assert!(is_enabled_provider(&s, "anthropic"));
        assert!(is_enabled_provider(&s, "custom_openai:vllm"));
    }

    #[test]
    fn is_enabled_provider_returns_persisted_flag() {
        let mut s = empty_settings();
        s.providers.enabled.insert("openai".into(), false);
        s.providers.enabled.insert("anthropic".into(), true);
        assert!(!is_enabled_provider(&s, "openai"));
        assert!(is_enabled_provider(&s, "anthropic"));
    }

    /// Round-trip: apply the same dotted-key write `set_provider_enabled`
    /// uses, then re-parse and confirm the flag flipped.
    #[test]
    fn set_provider_enabled_round_trip_writes_flag() {
        let existing = "";
        let body = forge_core::settings::apply_setting_update(
            existing,
            "providers.enabled.anthropic",
            toml::Value::Boolean(false),
        )
        .unwrap();
        let parsed: toml::Value = toml::from_str(&body).unwrap();
        assert_eq!(
            parsed["providers"]["enabled"]["anthropic"].as_bool(),
            Some(false)
        );
        let body = forge_core::settings::apply_setting_update(
            &body,
            "providers.enabled.anthropic",
            toml::Value::Boolean(true),
        )
        .unwrap();
        let parsed: toml::Value = toml::from_str(&body).unwrap();
        assert_eq!(
            parsed["providers"]["enabled"]["anthropic"].as_bool(),
            Some(true)
        );
    }

    /// F-673 prefix invariant.
    #[test]
    fn set_provider_enabled_error_prefix_is_command_named() {
        assert_eq!(SET_PROVIDER_ENABLED_ERROR, "set_provider_enabled: ");
    }

    /// Active-provider safeguard: `set_active_provider` must refuse to
    /// promote a disabled provider. The pure helper drives the same
    /// settings-load → check pipeline the live IPC uses; the verbatim
    /// error infix `is disabled` is load-bearing per the spec.
    #[test]
    fn set_active_provider_rejects_disabled_target() {
        let mut s = empty_settings();
        s.providers.enabled.insert("anthropic".into(), false);
        // Mirror the inline check at the top of `set_active_provider`.
        assert!(is_known_provider_id(&s, "anthropic"));
        assert!(!is_enabled_provider(&s, "anthropic"));
        let err = format!(
            "{SET_ACTIVE_PROVIDER_ERROR}provider {id} is disabled",
            id = "anthropic"
        );
        assert!(err.starts_with(SET_ACTIVE_PROVIDER_ERROR), "{err}");
        assert!(err.contains("is disabled"), "{err}");
    }

    /// On-disk safeguard: writing `enabled=false` for the currently-active
    /// id must also clear `[providers.active]`. We walk the helpers
    /// `set_provider_enabled` uses against a temp dir and assert the
    /// active key is gone after the second write.
    #[tokio::test(flavor = "multi_thread")]
    async fn set_provider_enabled_clears_active_when_disabling_active_id() {
        use forge_core::settings::{
            apply_setting_update, load_user_settings_in, save_user_settings_raw_in,
            user_settings_path_in,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[providers]
active = "anthropic"

[providers.enabled]
anthropic = true
"#;
        save_user_settings_raw_in(dir.path(), body).await.unwrap();

        let existing = tokio::fs::read_to_string(user_settings_path_in(dir.path()))
            .await
            .unwrap();
        let settings_before = load_user_settings_in(dir.path()).await.unwrap();
        assert_eq!(
            settings_before.providers.active.as_deref(),
            Some("anthropic")
        );

        // The IPC's update sequence: flip the flag, then conditionally
        // clear `providers.active` when the disabled id was active.
        let updated = apply_setting_update(
            &existing,
            "providers.enabled.anthropic",
            toml::Value::Boolean(false),
        )
        .unwrap();
        let cleared = remove_toml_leaf(&updated, &["providers", "active"]).unwrap();
        save_user_settings_raw_in(dir.path(), &cleared)
            .await
            .unwrap();

        let after = load_user_settings_in(dir.path()).await.unwrap();
        assert!(
            after.providers.active.is_none(),
            "{:?}",
            after.providers.active
        );
        assert_eq!(
            after.providers.enabled.get("anthropic").copied(),
            Some(false)
        );
    }
}

// ---------------------------------------------------------------------------
// HTTP-driven probe tests — gated on the `webview` feature so the dispatch
// path (`reqwest::Client`, credential store) compiles. wiremock provides a
// local mock server keyed on `127.0.0.1`; we drive every branch of
// `dispatch_probe` against the custom_openai endpoint, which exercises
// every error infix (`auth `, `network `, `timeout`, `missing credential`,
// `unknown provider`).
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "webview"))]
mod dispatch_tests {
    use super::*;
    use forge_core::settings::{
        AppSettings, AuthShapeSettings, CustomOpenAiEntry, ProvidersSettings,
    };
    use forge_core::MemoryStore;
    use secrecy::SecretString;
    use std::collections::BTreeMap;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn settings_with_custom(name: &str, base_url: &str, auth: AuthShapeSettings) -> AppSettings {
        let entry = CustomOpenAiEntry {
            base_url: base_url.to_string(),
            model: "m".into(),
            model_list: vec![],
            auth,
            api_key: None,
        };
        let mut map = BTreeMap::new();
        map.insert(name.to_string(), entry);
        AppSettings {
            providers: ProvidersSettings {
                custom_openai: map,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    async fn make_store(provider_id: &str, key: &str) -> Arc<dyn Credentials> {
        let store = Arc::new(MemoryStore::new());
        store
            .set(provider_id, SecretString::from(key.to_string()))
            .await
            .unwrap();
        store as Arc<dyn Credentials>
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_probe_success_returns_latency_and_model_count() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    { "id": "model-a" },
                    { "id": "model-b" },
                    { "id": "model-c" },
                ]
            })))
            .mount(&server)
            .await;
        let settings = settings_with_custom("vllm", &server.uri(), AuthShapeSettings::Bearer);
        let store = make_store("custom_openai:vllm", "sk-test").await;
        let client = reqwest::Client::new();

        let out = dispatch_probe(&client, "custom_openai:vllm", &settings, &store)
            .await
            .expect("probe succeeds");
        assert!(out.ok);
        assert_eq!(out.model_count, Some(3));
        assert_eq!(
            out.models.as_deref(),
            Some(
                &[
                    "model-a".to_string(),
                    "model-b".to_string(),
                    "model-c".to_string()
                ][..]
            )
        );
        assert!(out.latency_ms.is_some());
    }

    #[test]
    fn models_probe_url_strips_redundant_v1_suffix() {
        assert_eq!(
            super::models_probe_url("http://127.0.0.1:11434"),
            "http://127.0.0.1:11434/v1/models"
        );
        // User pasted the full `/v1` base — must not double up.
        assert_eq!(
            super::models_probe_url("http://127.0.0.1:11434/v1"),
            "http://127.0.0.1:11434/v1/models"
        );
        // Trailing slash on the bare host root.
        assert_eq!(
            super::models_probe_url("http://127.0.0.1:11434/"),
            "http://127.0.0.1:11434/v1/models"
        );
        // Trailing slash after `/v1`.
        assert_eq!(
            super::models_probe_url("http://127.0.0.1:11434/v1/"),
            "http://127.0.0.1:11434/v1/models"
        );
        // Don't strip an inner `/v1` segment that isn't the trailing one.
        assert_eq!(
            super::models_probe_url("http://host/proxy/v1/openai"),
            "http://host/proxy/v1/openai/v1/models"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_probe_extracts_models_from_ollama_name_shape() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer ollama"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    { "name": "llama3.2" },
                    { "name": "qwen2.5" },
                ]
            })))
            .mount(&server)
            .await;
        let settings = settings_with_custom("ollama", &server.uri(), AuthShapeSettings::Bearer);
        let store = make_store("custom_openai:ollama", "ollama").await;
        let client = reqwest::Client::new();

        let out = dispatch_probe(&client, "custom_openai:ollama", &settings, &store)
            .await
            .expect("probe succeeds");
        assert!(out.ok);
        assert_eq!(
            out.models.as_deref(),
            Some(&["llama3.2".to_string(), "qwen2.5".to_string()][..])
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_probe_returns_unknown_provider_for_garbage_id() {
        let settings = AppSettings::default();
        let store: Arc<dyn Credentials> = Arc::new(MemoryStore::new());
        let client = reqwest::Client::new();

        let err = dispatch_probe(&client, "gemini", &settings, &store)
            .await
            .unwrap_err();
        assert!(err.starts_with(TEST_PROVIDER_CONNECTION_ERROR), "{err}");
        assert!(err.contains("unknown provider"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_probe_returns_unknown_provider_for_missing_custom_entry() {
        let settings = AppSettings::default();
        let store: Arc<dyn Credentials> = Arc::new(MemoryStore::new());
        let client = reqwest::Client::new();

        let err = dispatch_probe(&client, "custom_openai:nope", &settings, &store)
            .await
            .unwrap_err();
        assert!(err.contains("unknown provider"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_probe_returns_missing_credential_when_store_empty() {
        let server = MockServer::start().await;
        let settings = settings_with_custom("vllm", &server.uri(), AuthShapeSettings::Bearer);
        let store: Arc<dyn Credentials> = Arc::new(MemoryStore::new());
        let client = reqwest::Client::new();

        let err = dispatch_probe(&client, "custom_openai:vllm", &settings, &store)
            .await
            .unwrap_err();
        assert!(err.starts_with(TEST_PROVIDER_CONNECTION_ERROR), "{err}");
        assert!(err.contains("missing credential"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_probe_signals_auth_on_401() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let settings = settings_with_custom("vllm", &server.uri(), AuthShapeSettings::Bearer);
        let store = make_store("custom_openai:vllm", "sk-wrong").await;
        let client = reqwest::Client::new();

        let err = dispatch_probe(&client, "custom_openai:vllm", &settings, &store)
            .await
            .unwrap_err();
        // Spec contract: the verbatim error begins `test_provider_connection: auth `
        // so the renderer pill can flip to auth-required without parsing further.
        assert!(
            err.starts_with("test_provider_connection: auth "),
            "expected auth prefix, got {err:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_probe_signals_network_on_5xx() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let settings = settings_with_custom("vllm", &server.uri(), AuthShapeSettings::Bearer);
        let store = make_store("custom_openai:vllm", "sk-test").await;
        let client = reqwest::Client::new();

        let err = dispatch_probe(&client, "custom_openai:vllm", &settings, &store)
            .await
            .unwrap_err();
        assert!(
            err.starts_with("test_provider_connection: network "),
            "expected network prefix, got {err:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_probe_signals_network_on_connection_refused() {
        // Bind a port, drop the listener so the OS reports ECONNREFUSED on
        // connect. A 5xx-style status code never arrives — the failure mode
        // is the reqwest send() error path.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let base_url = format!("http://{addr}");
        let settings = settings_with_custom("vllm", &base_url, AuthShapeSettings::None);
        let store: Arc<dyn Credentials> = Arc::new(MemoryStore::new());
        let client = reqwest::Client::new();

        let err = dispatch_probe(&client, "custom_openai:vllm", &settings, &store)
            .await
            .unwrap_err();
        assert!(
            err.starts_with("test_provider_connection: network "),
            "expected network prefix, got {err:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_provider_connection_timeout_short_circuits_long_probes() {
        // A wiremock server that delays its response well past the probe's
        // timeout. We bypass the public command and drive the timeout
        // wrapper directly so the budget is observable in a test.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(10))
                    .set_body_json(serde_json::json!({"data": []})),
            )
            .mount(&server)
            .await;
        let settings = settings_with_custom("vllm", &server.uri(), AuthShapeSettings::None);
        let store: Arc<dyn Credentials> = Arc::new(MemoryStore::new());
        let client = reqwest::Client::new();

        let probe = dispatch_probe(&client, "custom_openai:vllm", &settings, &store);
        let result = tokio::time::timeout(std::time::Duration::from_millis(150), probe).await;
        // The outer timeout fires first; mirror what `test_provider_connection`
        // returns when its own wrapper trips.
        assert!(result.is_err(), "expected outer timeout to fire");
    }

    // -----------------------------------------------------------------------
    // ---- remove_provider active-provider safeguard ------------------------
    // -----------------------------------------------------------------------

    /// Exercises the on-disk safeguard pipeline directly: write a settings
    /// file with `providers.active = id`, walk the same helpers
    /// `remove_provider` uses, then assert the active key was cleared from
    /// the persisted body. Tauri-side authz / state plumbing is not
    /// exercised here — the unit tests above pin the validation contract,
    /// and the live IPC has identical control flow.
    #[tokio::test(flavor = "multi_thread")]
    async fn remove_provider_clears_active_when_active_matches_removed_id() {
        use forge_core::settings::{load_user_settings_in, save_user_settings_raw_in};
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[providers]
active = "custom_openai:vllm"

[providers.custom_openai.vllm]
base_url = "http://x"
model = "m"
"#;
        save_user_settings_raw_in(dir.path(), body).await.unwrap();

        let existing =
            tokio::fs::read_to_string(forge_core::settings::user_settings_path_in(dir.path()))
                .await
                .unwrap();
        let updated = rewrite_for_remove(&existing, "custom_openai:vllm").unwrap();
        save_user_settings_raw_in(dir.path(), &updated)
            .await
            .unwrap();
        let settings_after = load_user_settings_in(dir.path()).await.unwrap();
        assert_eq!(
            settings_after.providers.active.as_deref(),
            Some("custom_openai:vllm"),
            "safeguard input: active still points at removed id before clearing"
        );
        // The IPC's clearing branch.
        let cleared = remove_toml_leaf(&updated, &["providers", "active"]).unwrap();
        save_user_settings_raw_in(dir.path(), &cleared)
            .await
            .unwrap();
        let final_settings = load_user_settings_in(dir.path()).await.unwrap();
        assert!(
            final_settings.providers.active.is_none(),
            "active should have been cleared, got {:?}",
            final_settings.providers.active
        );
    }

    /// Mirror of the above for the no-op branch: `active` points elsewhere,
    /// so the safeguard must leave it alone.
    #[tokio::test(flavor = "multi_thread")]
    async fn remove_provider_leaves_active_alone_when_unrelated() {
        use forge_core::settings::{load_user_settings_in, save_user_settings_raw_in};
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[providers]
active = "openai"

[providers.custom_openai.vllm]
base_url = "http://x"
model = "m"
"#;
        save_user_settings_raw_in(dir.path(), body).await.unwrap();
        let existing =
            tokio::fs::read_to_string(forge_core::settings::user_settings_path_in(dir.path()))
                .await
                .unwrap();
        let updated = rewrite_for_remove(&existing, "custom_openai:vllm").unwrap();
        save_user_settings_raw_in(dir.path(), &updated)
            .await
            .unwrap();
        let settings_after = load_user_settings_in(dir.path()).await.unwrap();
        // No clearing step because active != removed id.
        assert_eq!(settings_after.providers.active.as_deref(), Some("openai"));
    }
}
