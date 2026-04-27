//! Persistent user + workspace settings (F-151).
//!
//! Shape and lifecycle mirror [`crate::approvals`]:
//! - Two tiers: **workspace** (`<root>/.forge/settings.toml`) and **user**
//!   (`{config_dir}/forge/settings.toml`).
//! - Atomic writes via `<path>.tmp` + rename on the same filesystem.
//! - Missing files return defaults — first-run is the common case.
//!
//! Unlike `ApprovalConfig` (a flat `Vec<ApprovalEntry>`), settings are a
//! **structured object** with nested sections. This has two consequences the
//! IPC layer relies on:
//!
//! 1. **`#[serde(default)]` everywhere.** Every field, and every nested
//!    struct, carries `#[serde(default)]` so a settings file containing only
//!    `[notifications]` still deserializes — the missing `[windows]` section
//!    falls back to defaults. Without this, adding a new section in a later
//!    release would break every older settings.toml in the wild.
//!
//! 2. **Deep field-level merge for workspace-overrides-user.** Workspace does
//!    *not* wholesale replace user (that would delete user prefs the moment a
//!    repo gained a `.forge/settings.toml`). Instead [`load_merged_in`] parses
//!    each tier's file as `toml::Value`, overlays workspace keys onto user
//!    keys at every depth, then deserializes the merged tree into
//!    `AppSettings`. The net effect: `workspace.notifications.bg_agents = "os"`
//!    overrides only that single scalar, leaving `user.windows.session_mode`
//!    intact. See [`apply_setting_update`] + [`save_workspace_settings_raw`]
//!    for the mirror-image invariant on the write path — `set_setting` must
//!    mutate the raw TOML tree, not re-serialize `AppSettings`, or the
//!    merge's "absent-means-absent" semantic silently collapses.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::config_file::{load_toml_or_default, save_raw_atomic, save_toml_atomic};
use crate::Result;

/// Notification delivery mode for background-agent events (F-138 consumer).
/// Serialized as snake_case string so TOML files read naturally
/// (`bg_agents = "toast"`) and ts-rs emits a string literal union.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub enum NotificationMode {
    #[default]
    Toast,
    Os,
    Both,
    Silent,
}

/// Session-window layout preference (consumer: `docs/ui-specs/shell.md`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub enum SessionMode {
    #[default]
    Single,
    Split,
}

/// `[notifications]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct NotificationsSettings {
    #[serde(default)]
    pub bg_agents: NotificationMode,
}

/// `[windows]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct WindowsSettings {
    #[serde(default)]
    pub session_mode: SessionMode,
}

/// One `[providers.custom_openai.<name>]` entry: connection details for a
/// generic OpenAI-compatible self-hosted server (vLLM, LiteLLM, Together,
/// Anyscale, etc.). Each entry round-trips into a
/// `forge_providers::openai::CustomOpenAiProvider` at runtime.
///
/// Field validation (notably the SSRF guard on `base_url`) is deferred to
/// the construction site in `forge-providers`; this struct is purely the
/// on-disk wire shape.
///
/// `Debug` is hand-rolled (NOT derived) so the `api_key` field can never
/// reach a tracing span, panic message, or test failure as plaintext. The
/// formatter prints `Some("<redacted>")` when a key is configured and
/// `None` otherwise — sufficient for diagnostics without leaking the key.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct CustomOpenAiEntry {
    /// User-supplied OpenAI-compatible base URL (e.g.
    /// `https://api.together.xyz` or `http://127.0.0.1:8000`).
    pub base_url: String,
    /// Default model identifier this entry targets when no per-request
    /// override is supplied.
    #[serde(default)]
    pub model: String,
    /// Model identifiers the user has declared this endpoint serves.
    #[serde(default)]
    pub model_list: Vec<String>,
    /// How the API key is presented on the wire. Defaults to `bearer` so a
    /// minimal `[providers.custom_openai.<name>]` block with `base_url`,
    /// `model`, and `api_key` works out-of-the-box for Together/Anyscale-style
    /// vendors that follow the OpenAI shape exactly.
    #[serde(default = "default_auth_shape")]
    pub auth: AuthShapeSettings,
    /// API key — `None` is permitted only for `auth = { shape = "none" }`
    /// (private-network gateways, public mocks). The runtime construction
    /// site enforces the cross-field invariant.
    #[serde(default)]
    pub api_key: Option<String>,
}

impl std::fmt::Debug for CustomOpenAiEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomOpenAiEntry")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("model_list", &self.model_list)
            .field("auth", &self.auth)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// On-disk auth-shape selector. Mirrors
/// `forge_providers::openai::AuthShape` one-for-one but lives here so
/// forge-core does not depend on forge-providers (the dependency direction
/// must go forge-providers → forge-core, not the reverse).
///
/// Wire shape:
/// - `auth = { shape = "bearer" }`
/// - `auth = { shape = "header", name = "X-API-Key" }`
/// - `auth = { shape = "none" }`
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum AuthShapeSettings {
    #[default]
    Bearer,
    Header {
        name: String,
    },
    None,
}

fn default_auth_shape() -> AuthShapeSettings {
    AuthShapeSettings::default()
}

/// `[providers.custom_openai]` table — a map of `name → entry`. Each name
/// is a user-chosen identifier (e.g. `"vllm-local"`, `"together"`) used to
/// disambiguate multiple entries in error messages and the settings UI.
///
/// Backwards-compatible: settings files without `[providers]` continue to
/// load (the map is empty by default), and adding entries does not break
/// older readers because the parent struct is `#[serde(default)]`-friendly
/// and has no `deny_unknown_fields`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct ProvidersSettings {
    /// F-586: id of the active provider (e.g. `"ollama"`, `"anthropic"`,
    /// `"openai"`, `"custom_openai:vllm-local"`). `None` means "no
    /// preference" — the orchestrator falls through to whatever the daemon
    /// was started with (Phase-1 default: Ollama keyless).
    ///
    /// Stored as `Option<String>` rather than the random-hex `ProviderId`
    /// id type from `ids.rs`: provider selection is keyed by stable, human-
    /// readable slugs the user picks in the dashboard, not opaque ids.
    /// Both the IPC command surface and the credential store key on this
    /// same string shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// One entry per user-named OpenAI-compatible server.
    #[serde(default)]
    pub custom_openai: BTreeMap<String, CustomOpenAiEntry>,
}

/// `[catalog]` section (F-592): per-(kind,id) enable flags for the Skills /
/// MCP / Agents catalog UI.
///
/// Wire shape mirrors the dotted-key path the frontend writes via
/// `set_setting`: `catalog.enabled.<kind>.<id> = <bool>`. Both levels are
/// open maps — adding a new kind (or a new asset id within a kind) requires
/// zero schema churn. Absent entries default to "enabled = true" at the read
/// site.
///
/// `HashMap` (rather than `BTreeMap`) is intentional: order does not matter
/// for catalog flags, and the dotted-key write path inserts arbitrary string
/// keys; we never enumerate them in a stable order on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct CatalogSettings {
    /// `enabled.<kind>.<id> = <bool>`. Outer key is the catalog tab
    /// (`"skills"`, `"mcp"`, `"agents"`); inner key is the asset id.
    #[serde(default)]
    pub enabled: HashMap<String, HashMap<String, bool>>,
}

/// `[dashboard]` section. Per-user UI preferences for the Dashboard
/// window (F-597). Backwards-compatible: settings files without
/// `[dashboard]` continue to deserialize with all defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct DashboardSettings {
    /// F-597: when `true`, the first-run container-runtime banner is
    /// suppressed even if `detect_container_runtime` reports the runtime
    /// is missing or broken. The Dashboard exposes a "Don't show again"
    /// button that flips this flag.
    #[serde(default)]
    pub container_banner_dismissed: bool,
}

/// `[memory]` section (F-602): per-agent enable flags for cross-session
/// memory.
///
/// Wire shape mirrors the dotted-key path the frontend writes via
/// `set_setting`: `memory.enabled.<agent> = <bool>`. The frontend Dashboard
/// surfaces a per-agent toggle that flips this flag; `serve_with_session`
/// consults the merged settings at session start to decide whether memory
/// injection + the `memory.write` tool are active for the active agent.
///
/// Absent entries fall back to the agent's frontmatter `memory_enabled`
/// flag (F-601 semantics) — settings here are *user overrides*, not the
/// authoritative source.
///
/// `HashMap` (rather than `BTreeMap`) parallels `CatalogSettings` —
/// dotted-key writes insert arbitrary string keys and we never need a
/// stable enumeration order on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct MemorySettings {
    /// `enabled.<agent> = <bool>`. Absent agents fall back to the def-level
    /// `memory_enabled` frontmatter flag.
    #[serde(default)]
    pub enabled: HashMap<String, bool>,
}

/// Top-level settings shape persisted to `settings.toml`.
///
/// Intentionally does **not** carry `#[serde(deny_unknown_fields)]` — the
/// schema is open for extension. A future release may add new sections; older
/// builds reading a newer file must silently ignore unknown keys rather than
/// refuse to load. This is the opposite of `ApprovalConfig`, where strict
/// validation protects the approval trust boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct AppSettings {
    #[serde(default)]
    pub notifications: NotificationsSettings,
    #[serde(default)]
    pub windows: WindowsSettings,
    /// Built-in provider connection details (F-585). Optional and
    /// backwards-compatible: settings files without `[providers]` deserialize
    /// to an empty map.
    #[serde(default)]
    pub providers: ProvidersSettings,
    /// Catalog UI enable/disable flags (F-592). Open-shape map; absent =
    /// enabled.
    #[serde(default)]
    pub catalog: CatalogSettings,
    /// Dashboard UI preferences (F-597). Optional and backwards-compatible.
    #[serde(default)]
    pub dashboard: DashboardSettings,
    /// Per-agent cross-session memory toggles (F-602). Absent agents fall
    /// back to the def-level `memory_enabled` frontmatter flag.
    #[serde(default)]
    pub memory: MemorySettings,
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Workspace-scoped settings path: `<root>/.forge/settings.toml`.
pub fn workspace_settings_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".forge").join("settings.toml")
}

/// User-scoped settings path under the platform config dir.
pub fn user_settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|base| base.join("forge").join("settings.toml"))
}

/// Test seam / caller-supplied variant of [`user_settings_path`].
pub fn user_settings_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("forge").join("settings.toml")
}

// ---------------------------------------------------------------------------
// Load
// ---------------------------------------------------------------------------

/// Read the user-scoped settings. Returns defaults if the file is absent.
pub async fn load_user_settings() -> Result<AppSettings> {
    match user_settings_path() {
        Some(p) => load_from_path(&p).await,
        None => Ok(AppSettings::default()),
    }
}

/// Test-friendly variant that reads from `<dir>/forge/settings.toml`.
pub async fn load_user_settings_in(config_dir: &Path) -> Result<AppSettings> {
    load_from_path(&user_settings_path_in(config_dir)).await
}

/// Read the workspace-scoped settings from `<root>/.forge/settings.toml`.
pub async fn load_workspace_settings(workspace_root: &Path) -> Result<AppSettings> {
    load_from_path(&workspace_settings_path(workspace_root)).await
}

async fn load_from_path(path: &Path) -> Result<AppSettings> {
    load_toml_or_default(path).await
}

// ---------------------------------------------------------------------------
// Save (atomic .tmp + rename)
// ---------------------------------------------------------------------------

/// Atomically write the user-scoped settings.
pub async fn save_user_settings(settings: &AppSettings) -> Result<()> {
    let path = user_settings_path()
        .ok_or_else(|| anyhow::anyhow!("could not resolve user config directory"))?;
    save_to_path(&path, settings).await
}

/// Test-friendly variant of [`save_user_settings`].
pub async fn save_user_settings_in(config_dir: &Path, settings: &AppSettings) -> Result<()> {
    save_to_path(&user_settings_path_in(config_dir), settings).await
}

/// Atomically write the workspace-scoped settings.
pub async fn save_workspace_settings(workspace_root: &Path, settings: &AppSettings) -> Result<()> {
    save_to_path(&workspace_settings_path(workspace_root), settings).await
}

async fn save_to_path(path: &Path, settings: &AppSettings) -> Result<()> {
    save_toml_atomic(path, settings).await
}

/// Atomically write `body` to the workspace settings file. Exposed so the
/// IPC layer can persist the output of [`apply_setting_update`] without a
/// struct round-trip (see module docs on why that would clobber sibling
/// fields).
pub async fn save_workspace_settings_raw(workspace_root: &Path, body: &str) -> Result<()> {
    save_raw_atomic(&workspace_settings_path(workspace_root), body.as_bytes()).await
}

/// Atomically write `body` to `<config_dir>/forge/settings.toml`.
pub async fn save_user_settings_raw_in(config_dir: &Path, body: &str) -> Result<()> {
    save_raw_atomic(&user_settings_path_in(config_dir), body.as_bytes()).await
}

// ---------------------------------------------------------------------------
// Merge: workspace overrides user at field granularity
// ---------------------------------------------------------------------------
//
// Why "raw TOML tree" and not `AppSettings`-level struct merge:
//
// `#[serde(default)]` on every field means deserialization fills in defaults
// for any section the file does not declare. So `AppSettings` lost the
// distinction between "workspace explicitly set `windows.session_mode = single`"
// and "workspace did not mention windows at all" — both deserialize identically.
// A struct-level merge on these two identical-looking structs would happily
// clobber a user's non-default `windows.session_mode` with the workspace's
// implicit default.
//
// The merge must therefore run on the raw `toml::Value` trees parsed from the
// respective files (or on an empty tree when the file is absent). Only keys
// the workspace file physically contains overlay user keys; missing sections
// fall through unchanged.

/// Load both tiers and return the merged result. Workspace keys override user
/// keys at every depth; sections and fields the workspace file does not
/// declare are preserved from user. Missing files are treated as empty trees.
///
/// This is the entry point the Tauri `get_settings` command uses — it
/// captures the merge semantic in one place so callers cannot accidentally
/// apply struct-level overrides (see module doc for why that would be wrong).
pub async fn load_merged_in(
    user_config_dir: Option<&Path>,
    workspace_root: &Path,
) -> Result<AppSettings> {
    let user_tree = match user_config_dir {
        Some(dir) => load_tree(&user_settings_path_in(dir)).await?,
        None => toml::Value::Table(toml::value::Table::new()),
    };
    let workspace_tree = load_tree(&workspace_settings_path(workspace_root)).await?;
    Ok(merge_trees_into_settings(user_tree, workspace_tree))
}

async fn load_tree(path: &Path) -> Result<toml::Value> {
    match tokio::fs::read_to_string(path).await {
        Ok(contents) => {
            let val: toml::Value = toml::from_str(&contents).map_err(|e| anyhow::anyhow!(e))?;
            Ok(val)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(toml::Value::Table(toml::value::Table::new()))
        }
        Err(e) => Err(e.into()),
    }
}

fn merge_trees_into_settings(mut user: toml::Value, workspace: toml::Value) -> AppSettings {
    merge_value(&mut user, workspace);
    user.try_into().unwrap_or_else(|_| AppSettings::default())
}

/// Recursively overlay `src` onto `dst`. Tables merge key-by-key; everything
/// else is overwritten wholesale. Arrays are overwritten (not concatenated) —
/// the only arrays the current schema could carry would be replacement lists,
/// not additive ones, so "workspace replaces" is the intuitive default.
fn merge_value(dst: &mut toml::Value, src: toml::Value) {
    match (dst, src) {
        (toml::Value::Table(dst_tbl), toml::Value::Table(src_tbl)) => {
            for (k, v) in src_tbl {
                match dst_tbl.get_mut(&k) {
                    Some(existing) => merge_value(existing, v),
                    None => {
                        dst_tbl.insert(k, v);
                    }
                }
            }
        }
        (dst, src) => *dst = src,
    }
}

// ---------------------------------------------------------------------------
// Apply a (key, value) update to a settings tier, preserving sibling fields.
// ---------------------------------------------------------------------------
//
// `set_setting` at the IPC layer MUST load → mutate the raw toml tree →
// validate (by deserializing to `AppSettings`) → save. If it instead
// struct-mutated `AppSettings` and re-saved, every sibling field would be
// re-serialized from its in-memory value — including fields that were absent
// from the file (and therefore default-filled on load). That would silently
// promote defaults into the persisted file and erase the
// absent-means-pick-up-default semantic the merge layer depends on.

/// Apply `(dotted_key, value)` to the raw TOML contents of a settings file,
/// returning the updated TOML text. Creates missing parent tables. Does not
/// touch the filesystem; the caller handles atomic-write.
///
/// The value is validated by deserializing the updated tree back into
/// [`AppSettings`] — unknown keys are allowed (forward-compat), but a type
/// mismatch (e.g. `bg_agents = 42`) is rejected with a structured error.
pub fn apply_setting_update(
    existing_toml: &str,
    dotted_key: &str,
    value: toml::Value,
) -> Result<String> {
    let mut tree: toml::Value = if existing_toml.trim().is_empty() {
        toml::Value::Table(toml::value::Table::new())
    } else {
        toml::from_str(existing_toml).map_err(|e| anyhow::anyhow!(e))?
    };

    let segments: Vec<&str> = dotted_key.split('.').collect();
    if segments.is_empty() || segments.iter().any(|s| s.is_empty()) {
        return Err(anyhow::anyhow!("invalid setting key: {dotted_key:?}").into());
    }

    // Navigate / create the nested tables, then set the leaf.
    {
        let mut cursor = tree
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("settings root must be a table"))?;
        for seg in &segments[..segments.len() - 1] {
            let entry = cursor
                .entry(seg.to_string())
                .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
            if !entry.is_table() {
                // A scalar is in the way (e.g. someone hand-edited a section
                // name to a string). Refuse rather than silently clobber it.
                return Err(
                    anyhow::anyhow!("cannot set {dotted_key}: {seg} is not a table").into(),
                );
            }
            cursor = entry.as_table_mut().expect("just asserted table");
        }
        let leaf = segments[segments.len() - 1].to_string();
        cursor.insert(leaf, value);
    }

    // Validate by deserializing into AppSettings. This rejects type errors at
    // the IPC boundary rather than letting a malformed file land on disk.
    let _validated: AppSettings = tree
        .clone()
        .try_into()
        .map_err(|e| anyhow::anyhow!("invalid setting value: {e}"))?;

    toml::to_string(&tree)
        .map_err(|e| anyhow::anyhow!(e))
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // TOML round-trip + #[serde(default)] behavior
    // -----------------------------------------------------------------------

    #[test]
    fn toml_roundtrip_preserves_all_fields() {
        let cfg = AppSettings {
            notifications: NotificationsSettings {
                bg_agents: NotificationMode::Both,
            },
            windows: WindowsSettings {
                session_mode: SessionMode::Split,
            },
            providers: ProvidersSettings::default(),
            catalog: CatalogSettings::default(),
            dashboard: DashboardSettings::default(),
            memory: MemorySettings::default(),
        };
        let body = toml::to_string(&cfg).unwrap();
        let decoded: AppSettings = toml::from_str(&body).unwrap();
        assert_eq!(cfg, decoded);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // Empty file → all defaults.
        let empty: AppSettings = toml::from_str("").unwrap();
        assert_eq!(empty, AppSettings::default());

        // Partial file: only notifications section set.
        let partial: AppSettings = toml::from_str(
            r#"
[notifications]
bg_agents = "os"
"#,
        )
        .unwrap();
        assert_eq!(partial.notifications.bg_agents, NotificationMode::Os);
        assert_eq!(partial.windows.session_mode, SessionMode::Single);

        // Partial file: only a sub-field inside a section set.
        let partial: AppSettings = toml::from_str(
            r#"
[windows]
session_mode = "split"
"#,
        )
        .unwrap();
        assert_eq!(partial.windows.session_mode, SessionMode::Split);
        assert_eq!(partial.notifications.bg_agents, NotificationMode::default());
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compat() {
        // Schema is open; a newer release's keys must not break older readers.
        let body = r#"
[notifications]
bg_agents = "toast"
future_field = "whatever"

[some_new_section]
x = 1
"#;
        let parsed: AppSettings = toml::from_str(body).unwrap();
        assert_eq!(parsed.notifications.bg_agents, NotificationMode::Toast);
    }

    #[test]
    fn notification_mode_serializes_to_snake_case() {
        let body = toml::to_string(&NotificationsSettings {
            bg_agents: NotificationMode::Os,
        })
        .unwrap();
        assert!(
            body.contains(r#"bg_agents = "os""#),
            "expected snake_case literal, got {body:?}"
        );
    }

    #[test]
    fn session_mode_serializes_to_snake_case() {
        let body = toml::to_string(&WindowsSettings {
            session_mode: SessionMode::Split,
        })
        .unwrap();
        assert!(
            body.contains(r#"session_mode = "split""#),
            "expected snake_case literal, got {body:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Defaults
    // -----------------------------------------------------------------------

    #[test]
    fn default_app_settings_matches_spec() {
        let def = AppSettings::default();
        assert_eq!(def.notifications.bg_agents, NotificationMode::Toast);
        assert_eq!(def.windows.session_mode, SessionMode::Single);
    }

    // -----------------------------------------------------------------------
    // Load
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn load_workspace_missing_returns_defaults() {
        let dir = TempDir::new().unwrap();
        let cfg = load_workspace_settings(dir.path()).await.unwrap();
        assert_eq!(cfg, AppSettings::default());
    }

    #[tokio::test]
    async fn load_user_missing_returns_defaults() {
        let dir = TempDir::new().unwrap();
        let cfg = load_user_settings_in(dir.path()).await.unwrap();
        assert_eq!(cfg, AppSettings::default());
    }

    // -----------------------------------------------------------------------
    // Save + round-trip
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn save_then_load_workspace_roundtrips() {
        let dir = TempDir::new().unwrap();
        let cfg = AppSettings {
            notifications: NotificationsSettings {
                bg_agents: NotificationMode::Both,
            },
            windows: WindowsSettings {
                session_mode: SessionMode::Split,
            },
            providers: ProvidersSettings::default(),
            catalog: CatalogSettings::default(),
            dashboard: DashboardSettings::default(),
            memory: MemorySettings::default(),
        };
        save_workspace_settings(dir.path(), &cfg).await.unwrap();
        let loaded = load_workspace_settings(dir.path()).await.unwrap();
        assert_eq!(loaded, cfg);
    }

    #[tokio::test]
    async fn save_then_load_user_roundtrips() {
        let dir = TempDir::new().unwrap();
        let cfg = AppSettings {
            notifications: NotificationsSettings {
                bg_agents: NotificationMode::Silent,
            },
            windows: WindowsSettings::default(),
            providers: ProvidersSettings::default(),
            catalog: CatalogSettings::default(),
            dashboard: DashboardSettings::default(),
            memory: MemorySettings::default(),
        };
        save_user_settings_in(dir.path(), &cfg).await.unwrap();
        let loaded = load_user_settings_in(dir.path()).await.unwrap();
        assert_eq!(loaded, cfg);
    }

    #[tokio::test]
    async fn save_workspace_creates_dot_forge_dir() {
        let dir = TempDir::new().unwrap();
        assert!(!dir.path().join(".forge").exists());
        save_workspace_settings(dir.path(), &AppSettings::default())
            .await
            .unwrap();
        assert!(dir.path().join(".forge").join("settings.toml").exists());
    }

    #[tokio::test]
    async fn save_is_atomic_via_tmp_and_rename() {
        let dir = TempDir::new().unwrap();
        let cfg = AppSettings {
            notifications: NotificationsSettings {
                bg_agents: NotificationMode::Os,
            },
            windows: WindowsSettings::default(),
            providers: ProvidersSettings::default(),
            catalog: CatalogSettings::default(),
            dashboard: DashboardSettings::default(),
            memory: MemorySettings::default(),
        };
        save_workspace_settings(dir.path(), &cfg).await.unwrap();

        let final_path = dir.path().join(".forge").join("settings.toml");
        let tmp_path = dir.path().join(".forge").join("settings.toml.tmp");
        assert!(final_path.exists());
        assert!(!tmp_path.exists(), "tmp must be renamed, not left behind");

        // Overwrite + re-verify residue.
        save_workspace_settings(dir.path(), &AppSettings::default())
            .await
            .unwrap();
        assert!(final_path.exists());
        assert!(!tmp_path.exists());
    }

    // -----------------------------------------------------------------------
    // load_merged_in: workspace overrides user field-by-field
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn workspace_field_overrides_user_field_leaving_others_intact() {
        // User sets both fields to non-default values.
        let user_dir = TempDir::new().unwrap();
        save_user_settings_in(
            user_dir.path(),
            &AppSettings {
                notifications: NotificationsSettings {
                    bg_agents: NotificationMode::Toast,
                },
                windows: WindowsSettings {
                    session_mode: SessionMode::Split,
                },
                providers: ProvidersSettings::default(),
                catalog: CatalogSettings::default(),
                dashboard: DashboardSettings::default(),
                memory: MemorySettings::default(),
            },
        )
        .await
        .unwrap();

        // Workspace overrides ONLY notifications.bg_agents — no [windows]
        // section at all. Write raw TOML so the file physically lacks the
        // windows table, not merely has it set to defaults.
        let workspace_dir = TempDir::new().unwrap();
        tokio::fs::create_dir_all(workspace_dir.path().join(".forge"))
            .await
            .unwrap();
        tokio::fs::write(
            workspace_dir.path().join(".forge").join("settings.toml"),
            "[notifications]\nbg_agents = \"os\"\n",
        )
        .await
        .unwrap();

        let merged = load_merged_in(Some(user_dir.path()), workspace_dir.path())
            .await
            .unwrap();
        assert_eq!(
            merged.notifications.bg_agents,
            NotificationMode::Os,
            "workspace overrides user on declared field"
        );
        assert_eq!(
            merged.windows.session_mode,
            SessionMode::Split,
            "user field survives when workspace file does not declare it"
        );
    }

    #[tokio::test]
    async fn merge_empty_workspace_returns_user_settings() {
        let user_dir = TempDir::new().unwrap();
        let user_cfg = AppSettings {
            notifications: NotificationsSettings {
                bg_agents: NotificationMode::Silent,
            },
            windows: WindowsSettings {
                session_mode: SessionMode::Split,
            },
            providers: ProvidersSettings::default(),
            catalog: CatalogSettings::default(),
            dashboard: DashboardSettings::default(),
            memory: MemorySettings::default(),
        };
        save_user_settings_in(user_dir.path(), &user_cfg)
            .await
            .unwrap();

        // No workspace file on disk.
        let workspace_dir = TempDir::new().unwrap();
        let merged = load_merged_in(Some(user_dir.path()), workspace_dir.path())
            .await
            .unwrap();
        assert_eq!(merged, user_cfg);
    }

    #[tokio::test]
    async fn merge_no_user_dir_and_no_workspace_file_returns_defaults() {
        let workspace_dir = TempDir::new().unwrap();
        let merged = load_merged_in(None, workspace_dir.path()).await.unwrap();
        assert_eq!(merged, AppSettings::default());
    }

    // -----------------------------------------------------------------------
    // apply_setting_update: in-place field updates preserve siblings
    // -----------------------------------------------------------------------

    #[test]
    fn apply_setting_update_preserves_sibling_fields() {
        // Pre-existing file carries BOTH settings at non-default values. We
        // flip only `windows.session_mode` and assert `notifications.bg_agents`
        // survives unchanged in the output TOML text.
        let initial = r#"
[notifications]
bg_agents = "silent"

[windows]
session_mode = "single"
"#;
        let updated = apply_setting_update(
            initial,
            "windows.session_mode",
            toml::Value::String("split".into()),
        )
        .unwrap();
        let reparsed: AppSettings = toml::from_str(&updated).unwrap();
        assert_eq!(reparsed.notifications.bg_agents, NotificationMode::Silent);
        assert_eq!(reparsed.windows.session_mode, SessionMode::Split);
    }

    #[test]
    fn apply_setting_update_creates_missing_parent_tables() {
        let updated = apply_setting_update(
            "",
            "notifications.bg_agents",
            toml::Value::String("both".into()),
        )
        .unwrap();
        let reparsed: AppSettings = toml::from_str(&updated).unwrap();
        assert_eq!(reparsed.notifications.bg_agents, NotificationMode::Both);
    }

    #[test]
    fn apply_setting_update_rejects_type_mismatch() {
        // bg_agents is an enum string; an integer must fail validation.
        let err = apply_setting_update("", "notifications.bg_agents", toml::Value::Integer(42))
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("invalid setting value"),
            "expected validation failure, got {msg}"
        );
    }

    #[test]
    fn apply_setting_update_rejects_empty_segment() {
        let err = apply_setting_update("", "", toml::Value::String("x".into())).unwrap_err();
        assert!(format!("{err}").contains("invalid setting key"));

        let err = apply_setting_update(
            "",
            "notifications..bg_agents",
            toml::Value::String("toast".into()),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("invalid setting key"));
    }

    #[test]
    fn apply_setting_update_then_parse_matches_expected_scalar() {
        // Successive updates compose: set field A, then set field B, then set
        // field A again — end state reflects every write.
        let mut s = String::new();
        s = apply_setting_update(
            &s,
            "notifications.bg_agents",
            toml::Value::String("os".into()),
        )
        .unwrap();
        s = apply_setting_update(
            &s,
            "windows.session_mode",
            toml::Value::String("split".into()),
        )
        .unwrap();
        s = apply_setting_update(
            &s,
            "notifications.bg_agents",
            toml::Value::String("both".into()),
        )
        .unwrap();

        let reparsed: AppSettings = toml::from_str(&s).unwrap();
        assert_eq!(reparsed.notifications.bg_agents, NotificationMode::Both);
        assert_eq!(reparsed.windows.session_mode, SessionMode::Split);
    }

    // -----------------------------------------------------------------------
    // Paths
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_settings_path_is_under_dot_forge() {
        let p = workspace_settings_path(Path::new("/repo"));
        assert_eq!(p, Path::new("/repo/.forge/settings.toml"));
    }

    #[test]
    fn user_settings_path_in_nests_under_forge() {
        let p = user_settings_path_in(Path::new("/xdg"));
        assert_eq!(p, Path::new("/xdg/forge/settings.toml"));
    }

    // -----------------------------------------------------------------------
    // Providers schema (F-585)
    // -----------------------------------------------------------------------

    #[test]
    fn providers_section_absent_yields_empty_map() {
        // Backwards-compat: settings files without a [providers] section
        // continue to load. An older user upgrading to a build that introduces
        // the section must see no behaviour change.
        let body = r#"
[notifications]
bg_agents = "os"
"#;
        let parsed: AppSettings = toml::from_str(body).unwrap();
        assert!(parsed.providers.custom_openai.is_empty());
        assert_eq!(parsed.notifications.bg_agents, NotificationMode::Os);
    }

    #[test]
    fn providers_custom_openai_parses_one_entry_per_name() {
        let body = r#"
[providers.custom_openai.vllm-local]
base_url = "http://127.0.0.1:8000"
model = "Qwen/Qwen2.5-7B-Instruct"
model_list = ["Qwen/Qwen2.5-7B-Instruct"]
auth = { shape = "none" }

[providers.custom_openai.together]
base_url = "https://api.together.xyz"
model = "mixtral"
model_list = ["mixtral", "llama3-70b"]
auth = { shape = "bearer" }
api_key = "sk-test"
"#;
        let parsed: AppSettings = toml::from_str(body).unwrap();
        let entries = &parsed.providers.custom_openai;
        assert_eq!(entries.len(), 2);

        let local = entries.get("vllm-local").expect("vllm-local entry");
        assert_eq!(local.base_url, "http://127.0.0.1:8000");
        assert_eq!(local.auth, AuthShapeSettings::None);
        assert!(local.api_key.is_none());

        let remote = entries.get("together").expect("together entry");
        assert_eq!(remote.base_url, "https://api.together.xyz");
        assert_eq!(remote.auth, AuthShapeSettings::Bearer);
        assert_eq!(remote.api_key.as_deref(), Some("sk-test"));
        assert_eq!(remote.model_list.len(), 2);
    }

    #[test]
    fn providers_custom_openai_parses_header_auth_with_name() {
        let body = r#"
[providers.custom_openai.gateway]
base_url = "https://gw.example.com"
model = "any"
auth = { shape = "header", name = "X-API-Key" }
api_key = "sk-gw"
"#;
        let parsed: AppSettings = toml::from_str(body).unwrap();
        let entry = parsed
            .providers
            .custom_openai
            .get("gateway")
            .expect("entry");
        assert_eq!(
            entry.auth,
            AuthShapeSettings::Header {
                name: "X-API-Key".into()
            }
        );
    }

    #[test]
    fn providers_custom_openai_defaults_auth_to_bearer() {
        // A minimal entry omitting `auth` should default to bearer so the
        // common Together/Anyscale case needs only base_url + model + api_key.
        let body = r#"
[providers.custom_openai.together]
base_url = "https://api.together.xyz"
model = "mixtral"
api_key = "sk-test"
"#;
        let parsed: AppSettings = toml::from_str(body).unwrap();
        let entry = parsed
            .providers
            .custom_openai
            .get("together")
            .expect("entry");
        assert_eq!(entry.auth, AuthShapeSettings::Bearer);
    }

    #[test]
    fn providers_section_round_trips_through_save_load() {
        // End-to-end: write a settings file with a providers entry, read it
        // back through the same loader the IPC layer uses, assert the round
        // trip is lossless.
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            "vllm-local".to_string(),
            CustomOpenAiEntry {
                base_url: "http://127.0.0.1:8000".into(),
                model: "Qwen/Qwen2.5-7B-Instruct".into(),
                model_list: vec!["Qwen/Qwen2.5-7B-Instruct".into()],
                auth: AuthShapeSettings::None,
                api_key: None,
            },
        );
        let cfg = AppSettings {
            notifications: NotificationsSettings::default(),
            windows: WindowsSettings::default(),
            providers: ProvidersSettings {
                custom_openai: entries,
                ..Default::default()
            },
            catalog: CatalogSettings::default(),
            dashboard: DashboardSettings::default(),
            memory: MemorySettings::default(),
        };
        let serialized = toml::to_string(&cfg).unwrap();
        let reparsed: AppSettings = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed, cfg);
    }

    // -----------------------------------------------------------------------
    // Per-variant TOML round-trip guards (F-585 review feedback).
    //
    // The internally-tagged `AuthShapeSettings` enum (`#[serde(tag = "shape")]`)
    // is a known soft spot for the `toml` crate: unit variants (Bearer, None)
    // and struct variants with extra fields (Header { name }) take different
    // ser/deser paths. The single round-trip test above only covered `None`;
    // these three tests pin every variant explicitly so a future serde or
    // toml-rs upgrade that breaks one variant's repr surfaces here, not as a
    // mysterious "settings file disappeared on next launch" report from a
    // user.
    // -----------------------------------------------------------------------

    fn settings_with_auth(auth: AuthShapeSettings, api_key: Option<&str>) -> AppSettings {
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            "entry".to_string(),
            CustomOpenAiEntry {
                base_url: "https://api.example.com".into(),
                model: "any-model".into(),
                model_list: vec!["any-model".into()],
                auth,
                api_key: api_key.map(str::to_string),
            },
        );
        AppSettings {
            notifications: NotificationsSettings::default(),
            windows: WindowsSettings::default(),
            providers: ProvidersSettings {
                custom_openai: entries,
                ..Default::default()
            },
            catalog: CatalogSettings::default(),
            dashboard: DashboardSettings::default(),
            memory: MemorySettings::default(),
        }
    }

    // -----------------------------------------------------------------------
    // Active provider (F-586) — backwards-compat + round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn providers_active_absent_yields_none() {
        // Settings files written before F-586 have no `active` key. They must
        // still load (the field is `#[serde(default)]`) and the merged value
        // is `None` — meaning "no preference, fall through to the daemon's
        // startup default".
        let body = r#"
[providers.custom_openai.together]
base_url = "https://api.together.xyz"
model = "mixtral"
api_key = "sk-test"
"#;
        let parsed: AppSettings = toml::from_str(body).unwrap();
        assert!(parsed.providers.active.is_none());
        assert_eq!(parsed.providers.custom_openai.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Dashboard settings (F-597)
    // -----------------------------------------------------------------------

    #[test]
    fn dashboard_section_absent_yields_default() {
        // Settings files written before F-597 have no `[dashboard]` table.
        // They must still load with `container_banner_dismissed = false`.
        let parsed: AppSettings = toml::from_str("").unwrap();
        assert!(!parsed.dashboard.container_banner_dismissed);
    }

    #[test]
    fn dashboard_container_banner_dismissed_round_trips() {
        let cfg = AppSettings {
            dashboard: DashboardSettings {
                container_banner_dismissed: true,
            },
            ..Default::default()
        };
        let body = toml::to_string(&cfg).unwrap();
        let reparsed: AppSettings = toml::from_str(&body).unwrap();
        assert!(reparsed.dashboard.container_banner_dismissed);
    }

    #[test]
    fn dashboard_container_banner_apply_setting_update() {
        let updated =
            apply_setting_update("", "dashboard.container_banner_dismissed", true.into()).unwrap();
        let parsed: AppSettings = toml::from_str(&updated).unwrap();
        assert!(parsed.dashboard.container_banner_dismissed);
    }

    #[test]
    fn providers_active_round_trips_through_save_load() {
        let cfg = AppSettings {
            providers: ProvidersSettings {
                active: Some("anthropic".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let serialized = toml::to_string(&cfg).unwrap();
        let reparsed: AppSettings = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed, cfg);
        assert_eq!(reparsed.providers.active.as_deref(), Some("anthropic"));
    }

    #[test]
    fn providers_active_skip_serializing_when_none() {
        // None must NOT serialize — keeping the wire shape stable for older
        // readers and never promoting an implicit absent to a present-but-null.
        let cfg = AppSettings::default();
        let s = toml::to_string(&cfg).unwrap();
        assert!(
            !s.contains("active"),
            "default AppSettings must not emit `active`, got:\n{s}"
        );
    }

    #[test]
    fn providers_active_persists_through_apply_setting_update() {
        // Mirror the IPC `set_setting` path: `apply_setting_update` should
        // accept `providers.active = "openai"` and produce TOML that
        // round-trips into the new `active` field.
        let updated =
            apply_setting_update("", "providers.active", toml::Value::String("openai".into()))
                .unwrap();
        let reparsed: AppSettings = toml::from_str(&updated).unwrap();
        assert_eq!(reparsed.providers.active.as_deref(), Some("openai"));
    }

    #[test]
    fn auth_shape_bearer_round_trips_through_toml() {
        let cfg = settings_with_auth(AuthShapeSettings::Bearer, Some("sk-x"));
        let s = toml::to_string(&cfg).expect("serialize bearer");
        let reparsed: AppSettings = toml::from_str(&s).expect("re-parse bearer");
        assert_eq!(reparsed, cfg);
    }

    #[test]
    fn auth_shape_header_round_trips_through_toml() {
        let cfg = settings_with_auth(
            AuthShapeSettings::Header {
                name: "X-API-Key".into(),
            },
            Some("sk-x"),
        );
        let s = toml::to_string(&cfg).expect("serialize header");
        let reparsed: AppSettings = toml::from_str(&s).expect("re-parse header");
        assert_eq!(reparsed, cfg);
    }

    #[test]
    fn auth_shape_none_round_trips_through_toml() {
        let cfg = settings_with_auth(AuthShapeSettings::None, None);
        let s = toml::to_string(&cfg).expect("serialize none");
        let reparsed: AppSettings = toml::from_str(&s).expect("re-parse none");
        assert_eq!(reparsed, cfg);
    }

    // -----------------------------------------------------------------------
    // CustomOpenAiEntry::Debug must redact api_key (F-585 review feedback).
    // -----------------------------------------------------------------------

    #[test]
    fn custom_openai_entry_debug_redacts_api_key() {
        let entry = CustomOpenAiEntry {
            base_url: "https://api.example.com".into(),
            model: "any".into(),
            model_list: vec![],
            auth: AuthShapeSettings::Bearer,
            api_key: Some("sk-VERY-SECRET-DO-NOT-LEAK".into()),
        };
        let dump = format!("{entry:?}");
        assert!(
            !dump.contains("sk-VERY-SECRET-DO-NOT-LEAK"),
            "Debug output must not contain plaintext api_key: {dump}"
        );
        assert!(
            dump.contains("<redacted>"),
            "Debug output must signal redaction: {dump}"
        );
    }

    // -----------------------------------------------------------------------
    // Catalog (F-592) — round-trip + apply_setting_update path
    // -----------------------------------------------------------------------

    #[test]
    fn catalog_section_absent_yields_empty_map() {
        // Backwards-compat: pre-F-592 settings files have no `[catalog]`
        // section; they must continue to load and produce an empty enabled
        // map.
        let body = r#"
[notifications]
bg_agents = "os"
"#;
        let parsed: AppSettings = toml::from_str(body).unwrap();
        assert!(parsed.catalog.enabled.is_empty());
    }

    #[test]
    fn catalog_enable_persists_through_apply_setting_update() {
        // The exact dotted-key path the F-592 frontend writes:
        // `catalog.enabled.<kind>.<id>`. apply_setting_update must accept the
        // boolean leaf, validate it against the schema, and round-trip the
        // value through to the parsed `AppSettings`.
        let updated = apply_setting_update(
            "",
            "catalog.enabled.skills.typescript-review",
            toml::Value::Boolean(false),
        )
        .unwrap();
        let reparsed: AppSettings = toml::from_str(&updated).unwrap();
        let kind_map = reparsed
            .catalog
            .enabled
            .get("skills")
            .expect("skills kind map");
        assert_eq!(kind_map.get("typescript-review"), Some(&false));
    }

    #[test]
    fn catalog_multiple_kinds_and_ids_round_trip() {
        // Layered writes: two kinds, two ids each. Verifies the nested-map
        // shape survives `apply_setting_update`'s composability.
        let mut s = String::new();
        s = apply_setting_update(
            &s,
            "catalog.enabled.skills.alpha",
            toml::Value::Boolean(true),
        )
        .unwrap();
        s = apply_setting_update(
            &s,
            "catalog.enabled.skills.beta",
            toml::Value::Boolean(false),
        )
        .unwrap();
        s = apply_setting_update(&s, "catalog.enabled.mcp.gamma", toml::Value::Boolean(false))
            .unwrap();

        let reparsed: AppSettings = toml::from_str(&s).unwrap();
        let skills = reparsed.catalog.enabled.get("skills").unwrap();
        assert_eq!(skills.get("alpha"), Some(&true));
        assert_eq!(skills.get("beta"), Some(&false));
        let mcp = reparsed.catalog.enabled.get("mcp").unwrap();
        assert_eq!(mcp.get("gamma"), Some(&false));
    }

    #[test]
    fn catalog_rejects_non_boolean_leaf() {
        // The schema declares `bool`; a string must fail validation rather
        // than land on disk.
        let err = apply_setting_update(
            "",
            "catalog.enabled.skills.alpha",
            toml::Value::String("yes".into()),
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("invalid setting value"),
            "expected validation failure, got {err}"
        );
    }

    #[tokio::test]
    async fn catalog_set_then_load_merged_round_trips() {
        // Full round-trip through the same load_merged_in path the IPC layer
        // uses: write workspace TOML carrying a catalog enable flag, then
        // verify it surfaces in the merged AppSettings.
        let user_dir = TempDir::new().unwrap();
        let workspace_dir = TempDir::new().unwrap();

        tokio::fs::create_dir_all(workspace_dir.path().join(".forge"))
            .await
            .unwrap();
        let body = apply_setting_update(
            "",
            "catalog.enabled.agents.background-runner",
            toml::Value::Boolean(false),
        )
        .unwrap();
        tokio::fs::write(
            workspace_dir.path().join(".forge").join("settings.toml"),
            body,
        )
        .await
        .unwrap();

        let merged = load_merged_in(Some(user_dir.path()), workspace_dir.path())
            .await
            .unwrap();
        let agents = merged
            .catalog
            .enabled
            .get("agents")
            .expect("agents kind map");
        assert_eq!(agents.get("background-runner"), Some(&false));
    }

    #[test]
    fn custom_openai_entry_debug_marks_absent_api_key_as_none() {
        // For `auth = none`, the api_key is `None` and Debug should reflect
        // that — `Some("<redacted>")` would falsely imply a configured key.
        let entry = CustomOpenAiEntry {
            base_url: "http://127.0.0.1:8000".into(),
            model: "any".into(),
            model_list: vec![],
            auth: AuthShapeSettings::None,
            api_key: None,
        };
        let dump = format!("{entry:?}");
        assert!(
            dump.contains("api_key: None"),
            "absent key must show as None, got: {dump}"
        );
        assert!(
            !dump.contains("<redacted>"),
            "no redaction marker when key is absent: {dump}"
        );
    }

    // -----------------------------------------------------------------------
    // Memory (F-602) — round-trip + apply_setting_update path
    // -----------------------------------------------------------------------

    #[test]
    fn memory_section_absent_yields_empty_map() {
        // Backwards-compat: pre-F-602 settings files have no `[memory]`
        // section; they must continue to load and produce an empty enabled
        // map.
        let body = r#"
[notifications]
bg_agents = "os"
"#;
        let parsed: AppSettings = toml::from_str(body).unwrap();
        assert!(parsed.memory.enabled.is_empty());
    }

    #[test]
    fn memory_enable_persists_through_apply_setting_update() {
        // The exact dotted-key path the F-602 frontend writes:
        // `memory.enabled.<agent>`. apply_setting_update must accept the
        // boolean leaf, validate it against the schema, and round-trip the
        // value through to the parsed `AppSettings`.
        let updated =
            apply_setting_update("", "memory.enabled.scribe", toml::Value::Boolean(true)).unwrap();
        let reparsed: AppSettings = toml::from_str(&updated).unwrap();
        assert_eq!(reparsed.memory.enabled.get("scribe"), Some(&true));
    }

    #[test]
    fn memory_multiple_agents_round_trip() {
        let mut s = String::new();
        s = apply_setting_update(&s, "memory.enabled.scribe", toml::Value::Boolean(true)).unwrap();
        s = apply_setting_update(&s, "memory.enabled.researcher", toml::Value::Boolean(false))
            .unwrap();
        let reparsed: AppSettings = toml::from_str(&s).unwrap();
        assert_eq!(reparsed.memory.enabled.get("scribe"), Some(&true));
        assert_eq!(reparsed.memory.enabled.get("researcher"), Some(&false));
    }

    #[test]
    fn memory_rejects_non_boolean_leaf() {
        let err = apply_setting_update(
            "",
            "memory.enabled.scribe",
            toml::Value::String("yes".into()),
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("invalid setting value"),
            "expected validation failure, got {err}"
        );
    }

    #[tokio::test]
    async fn memory_set_then_load_merged_round_trips() {
        let user_dir = TempDir::new().unwrap();
        let workspace_dir = TempDir::new().unwrap();

        tokio::fs::create_dir_all(workspace_dir.path().join(".forge"))
            .await
            .unwrap();
        let body =
            apply_setting_update("", "memory.enabled.scribe", toml::Value::Boolean(true)).unwrap();
        tokio::fs::write(
            workspace_dir.path().join(".forge").join("settings.toml"),
            body,
        )
        .await
        .unwrap();

        let merged = load_merged_in(Some(user_dir.path()), workspace_dir.path())
            .await
            .unwrap();
        assert_eq!(merged.memory.enabled.get("scribe"), Some(&true));
    }
}
