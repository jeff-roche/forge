//! F-602: Tauri command surface for the Dashboard Memory section.
//!
//! Four commands, all dashboard-scoped:
//!
//! - [`list_agent_memory`] — enumerate every loaded agent and report its
//!   memory file's path, size, mtime, def-level `memory_enabled` flag, and
//!   the user's settings override (if any).
//! - [`read_agent_memory`] — return the markdown body of one agent's
//!   memory file. Empty string when the file is absent.
//! - [`save_agent_memory`] — write the markdown body verbatim (replace
//!   mode) to the agent's memory file. Increments the F-601 version
//!   counter and bumps `updated_at`.
//! - [`clear_agent_memory`] — wipe the body to empty (replace with `""`).
//!   Idempotent — calling it on an absent file is a no-op success.
//!
//! All four commands operate on
//! `<config_dir>/forge/memory/<agent>.md` via [`forge_agents::MemoryStore`].
//! The same store the F-601 `memory.write` tool uses, so an agent and a
//! human editing the same file see consistent results.
//!
//! # Authorization
//!
//! Every command requires the dashboard window label. A session window
//! has no business editing another agent's memory; routing through the
//! dashboard's Memory section is the intended UX flow.
//!
//! # Size caps
//!
//! - `agent_id`: 64 bytes (matches credential / catalog id caps).
//! - body: 1 MiB. Memory is intended for short summaries; a multi-megabyte
//!   body would balloon every system prompt and is almost certainly a bug
//!   on the caller side.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
#[cfg(feature = "webview")]
use tauri::{Runtime, State, Webview};
use ts_rs::TS;

use forge_agents::{Memory, MemoryFrontmatter, MemoryStore, WriteMode};

#[cfg(feature = "webview")]
use crate::ipc::{require_size, require_window_label_in, BridgeState, MAX_WORKSPACE_ROOT_BYTES};

/// Maximum agent id length. Mirrors the cap on credential / catalog ids.
pub const MAX_AGENT_ID_BYTES: usize = 64;

/// Maximum memory body size on the wire. 1 MiB is well above any sane
/// summary use; a body that big almost certainly indicates a bug or an
/// attempt to abuse the system prompt.
pub const MAX_MEMORY_BODY_BYTES: usize = 1024 * 1024;

/// Wire shape returned by [`list_agent_memory`] — one row per loaded agent.
///
/// `settings_override` is `Some(true|false)` when the merged settings
/// declare `[memory.enabled.<agent>]`, `None` otherwise. The frontend
/// renders the row's effective state by falling through to `def_enabled`
/// when the override is absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct AgentMemoryEntry {
    /// Agent identifier (stem of `.agents/<id>.md`).
    pub agent_id: String,
    /// Absolute path to the memory file (may not yet exist on disk).
    pub path: String,
    /// `Some(bytes)` when the file exists, `None` otherwise.
    #[ts(type = "number | null")]
    pub size_bytes: Option<u64>,
    /// `Some(timestamp)` when the file exists, `None` otherwise.
    /// Wire shape is an RFC 3339 string; ts-rs has no built-in chrono
    /// support so we override the TS surface explicitly.
    #[ts(type = "string | null")]
    pub updated_at: Option<DateTime<Utc>>,
    /// File-level monotonic version (F-601 frontmatter). `None` when the
    /// file is absent.
    #[ts(type = "number | null")]
    pub version: Option<u64>,
    /// The agent def's frontmatter `memory_enabled` flag (F-601 default).
    pub def_enabled: bool,
    /// User's `[memory.enabled.<agent>]` override, if present in settings.
    /// Frontend uses this to render the toggle's "set" vs "inherit" state.
    pub settings_override: Option<bool>,
}

/// Validate the inbound `agent_id` against the size cap and an allowlist
/// of safe stem characters. Pure helper exposed for unit tests.
///
/// Agent ids land on disk as `<root>/<agent>.md`. A blocklist (rejecting
/// only `/`, `\`, `..`) accidentally accepts surprising shapes:
///   - `"."`     → file becomes `<root>/.md`
///   - `".x"`    → dot-prefix files are hard to find
///   - `"con"`/`"nul"`/`"aux"` → reserved names on Windows
///   - whitespace, control bytes, unicode tricks, etc.
///
/// We mirror the catalog/credential id shape used elsewhere in the repo
/// (e.g. [`forge_core::skill::SkillId`]) and require the id be a non-empty
/// ASCII alphanumeric / `-` / `_` stem.
pub fn validate_agent_id(agent_id: &str) -> Result<(), String> {
    if agent_id.is_empty() {
        return Err("agent_id must not be empty".to_string());
    }
    if agent_id.len() > MAX_AGENT_ID_BYTES {
        return Err(format!(
            "agent_id too large: {} bytes exceeds cap of {} bytes",
            agent_id.len(),
            MAX_AGENT_ID_BYTES
        ));
    }
    if !agent_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("agent_id must contain only [A-Za-z0-9_-]".to_string());
    }
    Ok(())
}

/// Validate the inbound body against the byte cap. The body is otherwise
/// free-form Markdown — bytes round-trip verbatim.
pub fn validate_memory_body(body: &str) -> Result<(), String> {
    if body.len() > MAX_MEMORY_BODY_BYTES {
        return Err(format!(
            "memory body too large: {} bytes exceeds cap of {} bytes",
            body.len(),
            MAX_MEMORY_BODY_BYTES
        ));
    }
    Ok(())
}

/// Build the per-agent listing rows. Pure helper extracted so tests can
/// drive every shape (file present / absent, override present / absent)
/// without spinning up Tauri.
pub fn build_agent_memory_entries(
    store: &MemoryStore,
    defs: &[forge_agents::AgentDef],
    settings_overrides: &std::collections::HashMap<String, bool>,
) -> Vec<AgentMemoryEntry> {
    let mut out: Vec<AgentMemoryEntry> = defs
        .iter()
        .map(|def| {
            let path = store.path_for(&def.name);
            let (size_bytes, updated_at, version) = match std::fs::metadata(&path) {
                Ok(meta) => {
                    let size = meta.len();
                    // Pull `updated_at` + `version` from the YAML
                    // frontmatter so the UI shows the F-601 monotonic
                    // version, not the filesystem mtime — those advance
                    // independently when an external edit lands.
                    let memory = store.load(&def.name).ok().flatten();
                    let (ts, ver) = match memory.as_ref() {
                        Some(Memory {
                            frontmatter:
                                MemoryFrontmatter {
                                    updated_at,
                                    version,
                                },
                            ..
                        }) => (Some(*updated_at), Some(*version)),
                        None => (None, None),
                    };
                    (Some(size), ts, ver)
                }
                Err(_) => (None, None, None),
            };
            AgentMemoryEntry {
                agent_id: def.name.clone(),
                path: path.display().to_string(),
                size_bytes,
                updated_at,
                version,
                def_enabled: def.memory_enabled,
                settings_override: settings_overrides.get(&def.name).copied(),
            }
        })
        .collect();
    // Stable sort for deterministic UI rendering / test assertions.
    out.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    out
}

/// Resolve the path the [`MemoryStore`] would use when anchored at the
/// platform user-config dir, or `None` when `dirs::config_dir()` fails.
pub fn memory_root() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("forge").join("memory"))
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn list_agent_memory<R: Runtime>(
    workspace_root: String,
    webview: Webview<R>,
    state: State<'_, BridgeState>,
) -> Result<Vec<AgentMemoryEntry>, String> {
    require_window_label_in(&webview, &["dashboard"], false, "list_agent_memory")?;
    require_size("workspace_root", &workspace_root, MAX_WORKSPACE_ROOT_BYTES)?;

    let workspace_path =
        crate::ipc::resolve_workspace_root_for_command(webview.label(), &workspace_root, &state)
            .await?;
    let user_home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let defs = forge_agents::load_agents(&workspace_path, &user_home)
        .map_err(|e| format!("load agents: {e}"))?;

    // Use the test-overridable user config dir so integration tests can
    // redirect the memory root the way ipc_settings.rs redirects settings.
    let user_dir = crate::ipc::resolve_user_config_dir(&state);
    let store = match user_dir.as_deref() {
        Some(dir) => MemoryStore::new(dir),
        None => return Err("could not resolve user config directory".to_string()),
    };

    // Settings overlay (F-602): the user's `[memory.enabled.<agent>]` map
    // is loaded from the same merged tier the session daemon reads. Empty
    // when the settings file is absent or carries no `[memory]` section.
    let settings = forge_core::settings::load_merged_in(user_dir.as_deref(), &workspace_path)
        .await
        .map_err(|e| e.to_string())?;
    let overrides = settings.memory.enabled.clone();

    Ok(build_agent_memory_entries(&store, &defs, &overrides))
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn read_agent_memory<R: Runtime>(
    agent_id: String,
    webview: Webview<R>,
    state: State<'_, BridgeState>,
) -> Result<String, String> {
    require_window_label_in(&webview, &["dashboard"], false, "read_agent_memory")?;
    validate_agent_id(&agent_id)?;

    let user_dir = crate::ipc::resolve_user_config_dir(&state);
    let store = match user_dir.as_deref() {
        Some(dir) => MemoryStore::new(dir),
        None => return Err("could not resolve user config directory".to_string()),
    };

    match store.load(&agent_id) {
        Ok(Some(memory)) => Ok(memory.body),
        Ok(None) => Ok(String::new()),
        Err(e) => Err(format!("read_agent_memory: {e}")),
    }
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn save_agent_memory<R: Runtime>(
    agent_id: String,
    body: String,
    webview: Webview<R>,
    state: State<'_, BridgeState>,
) -> Result<AgentMemorySavedDto, String> {
    require_window_label_in(&webview, &["dashboard"], false, "save_agent_memory")?;
    validate_agent_id(&agent_id)?;
    validate_memory_body(&body)?;

    let user_dir = crate::ipc::resolve_user_config_dir(&state);
    let store = match user_dir.as_deref() {
        Some(dir) => MemoryStore::new(dir),
        None => return Err("could not resolve user config directory".to_string()),
    };

    let memory = store
        .write(&agent_id, &body, WriteMode::Replace)
        .map_err(|e| format!("save_agent_memory: {e}"))?;

    Ok(AgentMemorySavedDto {
        version: memory.frontmatter.version,
        updated_at: memory.frontmatter.updated_at,
    })
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn clear_agent_memory<R: Runtime>(
    agent_id: String,
    webview: Webview<R>,
    state: State<'_, BridgeState>,
) -> Result<(), String> {
    require_window_label_in(&webview, &["dashboard"], false, "clear_agent_memory")?;
    validate_agent_id(&agent_id)?;

    let user_dir = crate::ipc::resolve_user_config_dir(&state);
    let store = match user_dir.as_deref() {
        Some(dir) => MemoryStore::new(dir),
        None => return Err("could not resolve user config directory".to_string()),
    };

    // Idempotent: the file may not exist yet (the agent never wrote, the
    // user never edited). `MemoryStore::write` will create it with an
    // empty body — same path the F-601 `memory.write` tool takes when it
    // creates a fresh file.
    store
        .write(&agent_id, "", WriteMode::Replace)
        .map_err(|e| format!("clear_agent_memory: {e}"))?;
    Ok(())
}

/// Wire shape returned by [`save_agent_memory`] — surfaces the new
/// monotonic version + timestamp so the frontend can update its row
/// without a follow-up list refresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct AgentMemorySavedDto {
    /// New monotonic version after the write (F-601 frontmatter).
    #[ts(type = "number")]
    pub version: u64,
    #[ts(type = "string")]
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn def(name: &str, memory_enabled: bool) -> forge_agents::AgentDef {
        forge_agents::AgentDef {
            name: name.into(),
            description: None,
            body: String::new(),
            allowed_paths: vec![],
            isolation: forge_agents::Isolation::Process,
            memory_enabled,
        }
    }

    #[test]
    fn validate_agent_id_rejects_empty() {
        assert!(validate_agent_id("").is_err());
    }

    #[test]
    fn validate_agent_id_rejects_oversize() {
        let huge = "x".repeat(MAX_AGENT_ID_BYTES + 1);
        assert!(validate_agent_id(&huge).is_err());
    }

    #[test]
    fn validate_agent_id_allowlist_matrix() {
        // Reject — path-separator / traversal shapes.
        for bad in ["..", ".", "/", "\\", "../etc/passwd", "a/b", "a\\b"] {
            assert!(
                validate_agent_id(bad).is_err(),
                "expected rejection for {bad:?}",
            );
        }
        // Reject — empty.
        assert!(validate_agent_id("").is_err());
        // Reject — leading dot (hidden file).
        for bad in [".hidden", ".x", ".agent"] {
            assert!(
                validate_agent_id(bad).is_err(),
                "expected rejection for leading-dot {bad:?}",
            );
        }
        // Reject — symbols outside the allowlist.
        for bad in [
            "agent.v2", "a b", "a\tb", "a\nb", "a:b", "a;b", "a$b", "a*b", "a?b", "a@b", "a!b",
            "agent ", " agent", "ünicode", "agent.md",
        ] {
            assert!(
                validate_agent_id(bad).is_err(),
                "expected rejection for {bad:?}",
            );
        }

        // Accept — alphanumerics, dashes, underscores, mixed case.
        for good in [
            "agent",
            "my-agent",
            "my_agent",
            "agent42",
            "Agent-V2",
            "A",
            "ABC_def-123",
        ] {
            assert!(
                validate_agent_id(good).is_ok(),
                "expected acceptance for {good:?}",
            );
        }
    }

    #[test]
    fn validate_memory_body_rejects_oversize() {
        let huge = "a".repeat(MAX_MEMORY_BODY_BYTES + 1);
        assert!(validate_memory_body(&huge).is_err());
    }

    #[test]
    fn validate_memory_body_accepts_realistic() {
        validate_memory_body("# Notes\n- one\n- two").unwrap();
    }

    #[test]
    fn build_entries_for_defs_with_no_files() {
        let dir = TempDir::new().unwrap();
        let store = MemoryStore::new(dir.path());
        let defs = vec![def("alpha", true), def("beta", false)];
        let overrides: HashMap<String, bool> = HashMap::new();
        let entries = build_agent_memory_entries(&store, &defs, &overrides);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].agent_id, "alpha");
        assert!(entries[0].def_enabled);
        assert_eq!(entries[0].size_bytes, None);
        assert_eq!(entries[0].version, None);
        assert_eq!(entries[0].settings_override, None);
        assert_eq!(entries[1].agent_id, "beta");
        assert!(!entries[1].def_enabled);
    }

    #[test]
    fn build_entries_surfaces_size_and_version_when_file_exists() {
        let dir = TempDir::new().unwrap();
        let store = MemoryStore::new(dir.path());
        store
            .write("alpha", "remember the milk", WriteMode::Append)
            .unwrap();
        let defs = vec![def("alpha", true)];
        let overrides: HashMap<String, bool> = HashMap::new();
        let entries = build_agent_memory_entries(&store, &defs, &overrides);

        assert_eq!(entries.len(), 1);
        assert!(entries[0].size_bytes.is_some());
        assert_eq!(entries[0].version, Some(1));
        assert!(entries[0].updated_at.is_some());
    }

    #[test]
    fn build_entries_attaches_settings_override_when_present() {
        let dir = TempDir::new().unwrap();
        let store = MemoryStore::new(dir.path());
        let defs = vec![def("alpha", false)];
        let mut overrides = HashMap::new();
        overrides.insert("alpha".to_string(), true);
        let entries = build_agent_memory_entries(&store, &defs, &overrides);
        assert_eq!(entries[0].settings_override, Some(true));
        // Override does not mutate def_enabled — the row carries both so
        // the UI can show "settings: ON, def: OFF".
        assert!(!entries[0].def_enabled);
    }

    #[test]
    fn build_entries_sorts_alphabetically() {
        let dir = TempDir::new().unwrap();
        let store = MemoryStore::new(dir.path());
        let defs = vec![def("zulu", false), def("alpha", true), def("mike", false)];
        let overrides: HashMap<String, bool> = HashMap::new();
        let entries = build_agent_memory_entries(&store, &defs, &overrides);
        let names: Vec<&str> = entries.iter().map(|e| e.agent_id.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mike", "zulu"]);
    }
}
