//! Dashboard Tauri commands and their pure helpers.
//!
//! See `docs/architecture/persistence.md` §7.1, §7.5 and
//! `docs/architecture/window-hierarchy.md` §3.1.
//!
//! The core logic (`SessionSummary`, `collect_sessions`, `Pinger`, `UdsPinger`)
//! is compiled unconditionally so it can be unit-tested under
//! `--no-default-features`. The `#[tauri::command]` wrappers and their
//! `invoke_handler` registration live behind the `webview` feature.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use forge_core::meta::read_meta;
use forge_core::workspaces::read_workspaces;
use forge_core::SessionPersistence;
use serde::Serialize;

/// Wire shape consumed by the Dashboard's sessions panel.
///
/// `state` is a lowercase wire-string (`"active" | "archived" | "stopped"`)
/// and is intentionally decoupled from `forge_core::SessionState` — the UI
/// surfaces a "stopped" state for active sessions whose UDS socket fails to
/// respond, which has no equivalent in the core enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub subject: String,
    pub state: String,
    pub persistence: String,
    /// ISO-8601 UTC timestamp.
    pub created_at: String,
    /// ISO-8601 UTC timestamp.
    pub last_event_at: String,
}

/// Liveness probe for an active session's UDS socket. Injected so
/// `collect_sessions` can be unit-tested without a live server.
#[async_trait]
pub trait Pinger: Send + Sync {
    async fn ping(&self, socket: &Path) -> bool;
}

/// Scan every workspace listed in `workspaces_toml` for sessions and return
/// their summaries. A missing registry yields an empty list.
pub async fn collect_sessions(
    workspaces_toml: &Path,
    pinger: &dyn Pinger,
) -> Result<Vec<SessionSummary>> {
    if !workspaces_toml.exists() {
        return Ok(Vec::new());
    }

    let workspaces = read_workspaces(workspaces_toml).await?;
    let mut summaries = Vec::new();

    for workspace in &workspaces {
        let sessions_root = workspace.path.join(".forge").join("sessions");
        scan_active(&sessions_root, pinger, &mut summaries).await?;
        scan_archived(&sessions_root.join("archived"), &mut summaries).await?;
    }

    Ok(summaries)
}

/// Iterate direct children of `sessions_root` (skipping `archived/`), treat
/// each as an active session dir, and push its summary.
async fn scan_active(
    sessions_root: &Path,
    pinger: &dyn Pinger,
    out: &mut Vec<SessionSummary>,
) -> Result<()> {
    let Some(entries) = read_dir_opt(sessions_root).await? else {
        return Ok(());
    };
    for entry in entries {
        if entry.file_name() == std::ffi::OsStr::new("archived") {
            continue;
        }
        if !entry.is_dir {
            continue;
        }
        if let Some(summary) = summarize_active(&entry.path, pinger).await? {
            out.push(summary);
        }
    }
    Ok(())
}

/// Iterate direct children of `archived_root`, treat each as an archived
/// session dir, and push its summary.
async fn scan_archived(archived_root: &Path, out: &mut Vec<SessionSummary>) -> Result<()> {
    let Some(entries) = read_dir_opt(archived_root).await? else {
        return Ok(());
    };
    for entry in entries {
        if !entry.is_dir {
            continue;
        }
        if let Some(summary) = summarize_archived(&entry.path).await? {
            out.push(summary);
        }
    }
    Ok(())
}

struct DirEntry {
    path: PathBuf,
    is_dir: bool,
}

impl DirEntry {
    fn file_name(&self) -> &std::ffi::OsStr {
        self.path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new(""))
    }
}

async fn read_dir_opt(dir: &Path) -> Result<Option<Vec<DirEntry>>> {
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    while let Some(entry) = rd.next_entry().await? {
        let ft = entry.file_type().await?;
        out.push(DirEntry {
            path: entry.path(),
            is_dir: ft.is_dir(),
        });
    }
    Ok(Some(out))
}

async fn summarize_active(
    session_dir: &Path,
    pinger: &dyn Pinger,
) -> Result<Option<SessionSummary>> {
    let Some(meta) = load_meta(session_dir).await? else {
        return Ok(None);
    };
    let alive = pinger.ping(&meta.socket_path).await;
    let state = if alive { "active" } else { "stopped" };
    Ok(Some(make_summary(state, &meta, session_dir).await))
}

async fn summarize_archived(session_dir: &Path) -> Result<Option<SessionSummary>> {
    let Some(meta) = load_meta(session_dir).await? else {
        return Ok(None);
    };
    Ok(Some(make_summary("archived", &meta, session_dir).await))
}

async fn load_meta(session_dir: &Path) -> Result<Option<forge_core::meta::SessionMeta>> {
    let meta_path = session_dir.join("meta.toml");
    if !meta_path.exists() {
        return Ok(None);
    }
    Ok(Some(read_meta(&meta_path).await?))
}

async fn make_summary(
    wire_state: &str,
    meta: &forge_core::meta::SessionMeta,
    session_dir: &Path,
) -> SessionSummary {
    let last_event_at = last_event_at(session_dir, meta.started_at).await;
    SessionSummary {
        id: meta.id.to_string(),
        subject: meta.name.clone(),
        state: wire_state.to_string(),
        persistence: match meta.persistence {
            SessionPersistence::Persist => "persist".to_string(),
            SessionPersistence::Ephemeral => "ephemeral".to_string(),
        },
        created_at: meta.started_at.to_rfc3339(),
        last_event_at: last_event_at.to_rfc3339(),
    }
}

/// Last event wall-clock, taken from `events.jsonl` mtime. Falls back to
/// `started_at` if the log is absent or mtime lookup fails.
async fn last_event_at(session_dir: &Path, fallback: DateTime<Utc>) -> DateTime<Utc> {
    let log = session_dir.join("events.jsonl");
    let Ok(metadata) = tokio::fs::metadata(&log).await else {
        return fallback;
    };
    metadata
        .modified()
        .ok()
        .map(DateTime::<Utc>::from)
        .unwrap_or(fallback)
}

/// Production `Pinger` that talks real UDS. Connects with a 250ms cap and
/// completes a `Hello`→`HelloAck` round-trip within a 500ms total budget,
/// so a stalled dashboard ping never blocks the UI.
pub struct UdsPinger;

const PING_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);
const PING_TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

#[async_trait]
impl Pinger for UdsPinger {
    async fn ping(&self, socket: &Path) -> bool {
        use forge_ipc::{read_frame, write_frame, ClientInfo, Hello, IpcMessage, PROTO_VERSION};
        use tokio::net::UnixStream;

        let connect = tokio::time::timeout(PING_CONNECT_TIMEOUT, UnixStream::connect(socket));
        let mut stream = match connect.await {
            Ok(Ok(s)) => s,
            _ => return false,
        };

        let handshake = async {
            write_frame(
                &mut stream,
                &IpcMessage::Hello(Hello {
                    proto: PROTO_VERSION,
                    client: ClientInfo {
                        kind: "forge-shell".into(),
                        pid: std::process::id(),
                        user: whoami(),
                    },
                }),
            )
            .await
            .ok()?;
            match read_frame(&mut stream).await.ok()? {
                IpcMessage::HelloAck(_) => Some(()),
                _ => None,
            }
        };

        matches!(
            tokio::time::timeout(PING_TOTAL_TIMEOUT, handshake).await,
            Ok(Some(()))
        )
    }
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "unknown".to_string())
}

/// Resolve the path to the user's global `workspaces.toml`
/// (`~/.config/forge/workspaces.toml`).
pub fn default_workspaces_toml() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("~/.config"));
    base.join("forge").join("workspaces.toml")
}

/// F-063 (M11 / T5): structured error returned when `open_session` is
/// called with an id that does not match the canonical `SessionId` wire
/// shape. Surfaced as a plain `String` so it matches the existing
/// `Err(String)` wire shape of every `#[tauri::command]`.
///
/// Only consumed by the `webview`-gated `open_session`; the
/// `cfg_attr(..., allow(dead_code))` keeps `--no-default-features` builds
/// (which still compile the validator helper for its unit tests) clean.
#[cfg_attr(not(feature = "webview"), allow(dead_code))]
pub(crate) const INVALID_SESSION_ID_ERROR: &str = "invalid session id";

/// Strict gate over the `id` argument to `open_session` (and reused by
/// F-607's `export_transcript`). Matches exactly the output of
/// `forge_core::SessionId::new()` — 16 lowercase hex chars, no separators —
/// and rejects everything else.
///
/// F-063 (M11 / T5): the window label `session-{id}` is consumed by
/// Tauri's capability matcher (`session-*` glob in
/// `capabilities/default.json`). Without validation an id like
/// `../foo` or one containing NUL / whitespace would still match the glob
/// while producing a label with unexpected semantics. Keeping this helper
/// in terms of the canonical `SessionId` wire shape means a future format
/// change has exactly one place to update.
///
/// Under `--no-default-features` the webview command is not compiled, so
/// the helper is only exercised by its own unit tests — the
/// `cfg_attr(..., allow(dead_code))` silences the resulting dead-code
/// warning without hiding real unused-code regressions under
/// `--features webview`.
#[cfg_attr(not(feature = "webview"), allow(dead_code))]
pub(crate) fn is_valid_session_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod session_id_validation_tests {
    use super::*;

    #[test]
    fn canonical_16_hex_lowercase_is_valid() {
        assert!(is_valid_session_id("deadbeefcafebabe"));
        assert!(is_valid_session_id("0123456789abcdef"));
    }

    #[test]
    fn empty_is_invalid() {
        assert!(!is_valid_session_id(""));
    }

    #[test]
    fn shorter_than_16_is_invalid() {
        assert!(!is_valid_session_id("deadbeefcafebab")); // 15
    }

    #[test]
    fn longer_than_16_is_invalid() {
        assert!(!is_valid_session_id("deadbeefcafebabe0")); // 17
    }

    #[test]
    fn uppercase_hex_is_invalid() {
        // SessionId::new() emits lowercase only; be strict.
        assert!(!is_valid_session_id("DEADBEEFCAFEBABE"));
    }

    #[test]
    fn dashes_are_invalid() {
        // SessionId::new() never produces separators; reject to match the
        // authoritative wire shape.
        assert!(!is_valid_session_id("dead-beef-cafe-ba"));
    }

    #[test]
    fn non_hex_chars_are_invalid() {
        assert!(!is_valid_session_id("zzzzzzzzzzzzzzzz"));
        assert!(!is_valid_session_id("deadbeefcafebabg"));
    }

    #[test]
    fn path_traversal_is_invalid() {
        assert!(!is_valid_session_id("../something"));
        assert!(!is_valid_session_id("..cafebabe0000000"));
    }

    #[test]
    fn whitespace_is_invalid() {
        assert!(!is_valid_session_id("deadbeef cafebabe"));
        assert!(!is_valid_session_id(" deadbeefcafebabe"));
        assert!(!is_valid_session_id("deadbeefcafebabe "));
        assert!(!is_valid_session_id("deadbeef\ncafebabe"));
    }

    #[test]
    fn nul_byte_is_invalid() {
        assert!(!is_valid_session_id("deadbeef\0cafebabe"));
    }
}

/// Tauri command: return the sessions panel's data for the Dashboard.
#[cfg(feature = "webview")]
#[tauri::command]
pub async fn session_list<R: tauri::Runtime>(
    webview: tauri::Webview<R>,
) -> Result<Vec<SessionSummary>, String> {
    crate::ipc::require_window_label(&webview, "dashboard", "session_list")?;
    collect_sessions(&default_workspaces_toml(), &UdsPinger)
        .await
        .map_err(|e| e.to_string())
}

/// Tauri command: open (or focus) the Session window for `id`. Delegates to
/// the F-019 `WindowManager`.
///
/// **F-063 (M11 / T5):** `id` is validated against the canonical
/// `SessionId` wire shape *before* the label is built. The capability
/// file's `session-*` glob would otherwise match labels such as
/// `session-../foo` produced from a path-traversal id.
#[cfg(feature = "webview")]
#[tauri::command]
pub async fn open_session<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    webview: tauri::Webview<R>,
    id: String,
) -> Result<(), String> {
    crate::ipc::require_window_label(&webview, "dashboard", "open_session")?;
    if !is_valid_session_id(&id) {
        return Err(INVALID_SESSION_ID_ERROR.to_string());
    }
    crate::window_manager::WindowManager::new(app)
        .open_session(&id)
        .map(|_| ())
        .map_err(|e| e.to_string())
}
