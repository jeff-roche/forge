//! F-597: Tauri command surface for the Dashboard's container lifecycle UI.
//!
//! Five commands, all gated behind the `dashboard` window label:
//!
//! - [`detect_container_runtime`] — runs F-595's `PodmanRuntime::detect`
//!   and folds the [`forge_oci::OciError`] variants the F-596 fallback
//!   path classifies as "unavailable" into a [`RuntimeStatus`] enum the
//!   webview can pattern-match on for the first-run banner.
//! - [`list_active_containers`] — snapshot of every Level-2 sandbox
//!   container the shell currently knows about. The registry is in-memory
//!   only; sessions register on `Level2Session::create` success and
//!   unregister on teardown.
//! - [`stop_container`] — request graceful stop of a known container.
//! - [`remove_container`] — force-remove a container (also drops it from
//!   the registry). The runtime's `remove(-f)` accepts both running and
//!   stopped containers.
//! - [`container_logs`] — fetch recent stdout+stderr lines via
//!   [`forge_oci::ContainerLogs::logs`]. Supports `since` (RFC-3339) and
//!   `tail` for incremental polling without re-pulling the full transcript.
//!
//! # Authorization
//!
//! Container commands are dashboard-scoped — only the `dashboard` window
//! label may invoke them. Same model as `credentials_ipc` and
//! `providers_ipc`.
//!
//! # Registry
//!
//! [`ContainerRegistryState`] is a `RwLock<HashMap<container_id, ContainerInfo>>`
//! attached as Tauri-managed state. The contract is:
//!
//! 1. When [`forge_session::sandbox::level2::Level2Session::create`]
//!    succeeds, the session registers `(container_id, image, started_at,
//!    session_id)` via [`ContainerRegistryState::register`].
//! 2. When the session tears down (or the panic-safety net fires), the
//!    container is unregistered.
//! 3. When the Dashboard invokes `stop_container` / `remove_container`,
//!    the entry is left marked as `stopped: true` after `stop` (so the
//!    user sees "stopped" in the list) and removed after `remove`. The
//!    Dashboard refresh then drops the row.
//!
//! Sessions and the Dashboard share the same `RwLock`, so list / register
//! never block each other beyond the lock-hold.
//!
//! # Session → registry wiring
//!
//! F-597 ships the registry surface, the dashboard commands, and the UI.
//! The session-side write path — calling
//! [`ContainerRegistryState::register`] from the orchestrator at
//! `Level2Session::create` time — depends on forge-session gaining a
//! handle to this state. Until that lands, the registry is empty and the
//! dashboard's [`list_active_containers`] returns `[]`. The UI degrades
//! gracefully: an empty-state hint is rendered and the runtime banner
//! still reflects the [`detect_container_runtime`] probe.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use forge_oci::{
    ContainerHandle, ContainerLogs, ContainerRuntime, LogLine, OciError, PodmanRuntime,
};
use serde::{Deserialize, Serialize};
#[cfg(feature = "webview")]
use tauri::{AppHandle, Emitter, Manager, Runtime, State, Webview};
use tokio::sync::RwLock;
#[allow(unused_imports)]
use tracing;

/// Per-field byte cap on the inbound `container_id` argument. Podman
/// container IDs are 64 hex chars; 256 bytes is a generous cap that
/// still rejects a hostile renderer driving megabyte calls.
pub const MAX_CONTAINER_ID_BYTES: usize = 256;

/// Per-field byte cap on the inbound `since` argument (RFC-3339).
pub const MAX_SINCE_BYTES: usize = 64;

/// Maximum number of log lines a single `container_logs` invocation
/// returns. Bounds the IPC payload so a long-running container can't
/// freeze the UI on the first poll. The Dashboard polls incrementally
/// with `since` to keep the live feed up to date.
pub const MAX_LOG_TAIL: usize = 1_000;

/// Tauri event name carrying a "container list changed" notification to
/// the dashboard. The list view subscribes and re-fetches when it fires.
pub const CONTAINERS_CHANGED_EVENT: &str = "containers:list_changed";

/// Window label that owns container management. Only this label may
/// invoke the five container commands.
pub const CONTAINERS_OWNER_LABEL: &str = "dashboard";

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Result of probing the container runtime. The webview pattern-matches
/// on the variant to decide whether to show the first-run banner and
/// what install hint to surface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeStatus {
    /// Runtime is installed and rootless mode is available. The dashboard
    /// suppresses the banner.
    Available,
    /// Runtime is not on `PATH`.
    Missing { tool: String },
    /// Runtime is installed but the detection probe failed (cgroup
    /// delegation, missing newuidmap, SELinux denial, etc.).
    Broken { tool: String, reason: String },
    /// Runtime is installed but rootless mode is unavailable.
    RootlessUnavailable { tool: String, reason: String },
    /// Probe ran into an unexpected I/O / parse error. Treated as
    /// "unavailable" for fallback purposes; surfaced separately so the
    /// banner copy can say "could not probe" instead of "not installed".
    Unknown { reason: String },
}

impl RuntimeStatus {
    /// `true` when the runtime is unusable for Level-2 sandboxes. The
    /// dashboard renders the first-run banner whenever this is `true`
    /// AND the user hasn't dismissed the banner.
    pub fn is_unavailable(&self) -> bool {
        !matches!(self, RuntimeStatus::Available)
    }
}

/// One row in `list_active_containers`: a Level-2 sandbox container the
/// shell currently knows about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContainerInfo {
    /// Stable session id from `forge-session`. The dashboard joins this
    /// against its session list so each container row can link back to
    /// the owning session.
    pub session_id: String,
    /// Runtime-assigned container id (as returned by `podman create`).
    pub container_id: String,
    /// Human-readable image reference (e.g. `"alpine:3.19"`).
    pub image: String,
    /// RFC-3339 timestamp captured at registration.
    pub started_at: String,
    /// `true` once the user has invoked `stop_container` or the runtime
    /// reports the container as exited. Purely advisory — the dashboard
    /// renders a "stopped" pip on these rows; remove unconditionally
    /// drops the row regardless.
    pub stopped: bool,
}

// ---------------------------------------------------------------------------
// Registry — shared between forge-session and the dashboard
// ---------------------------------------------------------------------------

/// Tauri-managed in-memory registry of active Level-2 sandbox containers.
/// Sessions write; the dashboard reads. The registry is intentionally
/// process-scoped: a Forge restart drops the map (the containers are
/// reaped by `Level2Session::drop`'s panic-safety net or already gone by
/// the time the new process starts).
#[derive(Default)]
pub struct ContainerRegistryState {
    inner: Arc<RwLock<HashMap<String, ContainerInfo>>>,
}

impl ContainerRegistryState {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow the inner `Arc<RwLock<...>>` so other crates (notably
    /// `forge-session` when `Level2Session` is created) can register
    /// without going through Tauri state.
    pub fn handle(&self) -> Arc<RwLock<HashMap<String, ContainerInfo>>> {
        Arc::clone(&self.inner)
    }

    /// Insert (or overwrite) an entry for `container_id`.
    pub async fn register(&self, info: ContainerInfo) {
        let mut g = self.inner.write().await;
        g.insert(info.container_id.clone(), info);
    }

    /// Drop the entry for `container_id` if present. Idempotent.
    pub async fn unregister(&self, container_id: &str) {
        let mut g = self.inner.write().await;
        g.remove(container_id);
    }

    /// Mark an entry as stopped without dropping it from the map.
    pub async fn mark_stopped(&self, container_id: &str) {
        let mut g = self.inner.write().await;
        if let Some(entry) = g.get_mut(container_id) {
            entry.stopped = true;
        }
    }

    /// Snapshot all entries, sorted by `started_at` ascending so the
    /// dashboard renders the oldest container first (stable across
    /// refreshes).
    ///
    /// # Invariant: `started_at` is RFC-3339 with `+00:00` offset
    ///
    /// We sort `started_at` lexicographically as a `String`, which is
    /// only safe because every value is produced by
    /// [`make_container_info`], which calls `chrono::DateTime::<Utc>::to_rfc3339()`
    /// — that always emits the `+00:00` form (never `Z`, never a non-UTC
    /// offset). Mixed offset forms would break the lex-sort ordering.
    /// Any future code path that registers a container MUST go through
    /// `make_container_info` (or otherwise emit the same canonical form);
    /// the `make_container_info_emits_rfc3339_timestamp` test pins this.
    pub async fn list(&self) -> Vec<ContainerInfo> {
        let g = self.inner.read().await;
        let mut out: Vec<ContainerInfo> = g.values().cloned().collect();
        out.sort_by(|a, b| a.started_at.cmp(&b.started_at));
        out
    }
}

/// Idempotent state attachment.
#[cfg(feature = "webview")]
pub fn manage_containers<R: Runtime>(app: &AppHandle<R>) {
    if app.try_state::<ContainerRegistryState>().is_none() {
        app.manage(ContainerRegistryState::new());
    }
}

// ---------------------------------------------------------------------------
// Pure validation
// ---------------------------------------------------------------------------

/// F-674: pure validation helper for the optional `since` argument
/// accepted by [`container_logs`]. Mirrors the rule documented in
/// `crate::ipc`: optional-string params route through the canonical
/// `require_size` helper when present, and skip the check when absent.
/// Exposed so unit tests can assert the standardized error marker
/// without standing up a Tauri runtime.
pub fn validate_optional_since(since: Option<&str>) -> Result<(), String> {
    if let Some(s) = since {
        crate::ipc::require_size("since", s, MAX_SINCE_BYTES)?;
    }
    Ok(())
}

/// Validate `container_id`. Pure; exposed for unit tests.
pub fn validate_container_id(container_id: &str) -> Result<(), String> {
    if container_id.is_empty() {
        return Err("container_id is empty".to_string());
    }
    if container_id.len() > MAX_CONTAINER_ID_BYTES {
        return Err(format!(
            "container_id too large: {} bytes exceeds cap of {} bytes",
            container_id.len(),
            MAX_CONTAINER_ID_BYTES
        ));
    }
    // Container IDs from podman are 64-char hex; podman container *names*
    // accept `[a-zA-Z0-9][a-zA-Z0-9_.-]*`. Our allowlist is a deliberate
    // superset of the hex-id form and a subset of the name form (we omit
    // `.` because no live forge codepath produces dotted names and the
    // looser charset would slacken the structured-argv guarantee for no
    // benefit). Any non-matching id is rejected at the IPC boundary —
    // not a security guarantee on its own (argv is structured), just a
    // tight contract that catches accidental mis-routing and shaves an
    // attacker's bug-search surface.
    if !container_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("container_id contains invalid characters".to_string());
    }
    Ok(())
}

/// Map an [`OciError`] from `PodmanRuntime::detect` into a [`RuntimeStatus`].
/// Pure; exposed for unit tests so the variant mapping is pinned.
pub fn classify_runtime_status(result: Result<(), OciError>) -> RuntimeStatus {
    match result {
        Ok(()) => RuntimeStatus::Available,
        Err(OciError::RuntimeMissing(tool)) => RuntimeStatus::Missing {
            tool: tool.to_string(),
        },
        Err(OciError::RuntimeBroken { tool, stderr }) => RuntimeStatus::Broken {
            tool: tool.to_string(),
            reason: stderr,
        },
        Err(OciError::RootlessUnavailable { runtime, reason }) => {
            RuntimeStatus::RootlessUnavailable {
                tool: runtime.to_string(),
                reason,
            }
        }
        Err(other) => RuntimeStatus::Unknown {
            reason: other.to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// Tauri command surface
// ---------------------------------------------------------------------------

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn detect_container_runtime<R: Runtime>(
    webview: Webview<R>,
) -> Result<RuntimeStatus, String> {
    crate::ipc::require_window_label(&webview, CONTAINERS_OWNER_LABEL, "detect_container_runtime")?;
    let runtime = PodmanRuntime::new();
    Ok(classify_runtime_status(runtime.detect().await))
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn list_active_containers<R: Runtime>(
    webview: Webview<R>,
    registry: State<'_, ContainerRegistryState>,
) -> Result<Vec<ContainerInfo>, String> {
    crate::ipc::require_window_label(&webview, CONTAINERS_OWNER_LABEL, "list_active_containers")?;
    Ok(registry.list().await)
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn stop_container<R: Runtime>(
    container_id: String,
    app: AppHandle<R>,
    webview: Webview<R>,
    registry: State<'_, ContainerRegistryState>,
) -> Result<(), String> {
    crate::ipc::require_window_label(&webview, CONTAINERS_OWNER_LABEL, "stop_container")?;
    validate_container_id(&container_id)?;

    let runtime = PodmanRuntime::new();
    let handle = ContainerHandle::new(&container_id);
    runtime.stop(&handle).await.map_err(|e| e.to_string())?;
    registry.mark_stopped(&container_id).await;
    let _ = app.emit(CONTAINERS_CHANGED_EVENT, &container_id);
    Ok(())
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn remove_container<R: Runtime>(
    container_id: String,
    app: AppHandle<R>,
    webview: Webview<R>,
    registry: State<'_, ContainerRegistryState>,
) -> Result<(), String> {
    crate::ipc::require_window_label(&webview, CONTAINERS_OWNER_LABEL, "remove_container")?;
    validate_container_id(&container_id)?;

    let runtime = PodmanRuntime::new();
    let handle = ContainerHandle::new(&container_id);
    runtime.remove(&handle).await.map_err(|e| e.to_string())?;
    registry.unregister(&container_id).await;
    let _ = app.emit(CONTAINERS_CHANGED_EVENT, &container_id);
    Ok(())
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn container_logs<R: Runtime>(
    container_id: String,
    since: Option<String>,
    tail: Option<usize>,
    webview: Webview<R>,
) -> Result<Vec<LogLine>, String> {
    crate::ipc::require_window_label(&webview, CONTAINERS_OWNER_LABEL, "container_logs")?;
    validate_container_id(&container_id)?;
    validate_optional_since(since.as_deref())?;
    let tail = tail.map(|n| n.min(MAX_LOG_TAIL));
    let runtime = PodmanRuntime::new();
    let handle = ContainerHandle::new(&container_id);
    runtime
        .logs(&handle, since.as_deref(), tail)
        .await
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Helpers exposed for forge-session integration
// ---------------------------------------------------------------------------

/// Build a [`ContainerInfo`] from the parts every `Level2Session::create`
/// caller already has (image string, container handle, session id). The
/// `started_at` is captured here so callers don't need a chrono dep just
/// to register.
pub fn make_container_info(
    session_id: impl Into<String>,
    container_id: impl Into<String>,
    image: impl Into<String>,
    started_at: DateTime<Utc>,
) -> ContainerInfo {
    ContainerInfo {
        session_id: session_id.into(),
        container_id: container_id.into(),
        image: image.into(),
        started_at: started_at.to_rfc3339(),
        stopped: false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_optional_since_accepts_none() {
        assert!(validate_optional_since(None).is_ok());
    }

    #[test]
    fn validate_optional_since_accepts_within_cap() {
        assert!(validate_optional_since(Some("2025-04-26T10:00:00Z")).is_ok());
    }

    #[test]
    fn validate_optional_since_rejects_oversize_with_canonical_marker() {
        // F-674: the standardized error string from `crate::ipc::require_size`
        // begins with `"payload too large"` and names the offending field.
        // Tests assert against that marker so the UI layer can pattern-match
        // a single shape.
        let huge = "a".repeat(MAX_SINCE_BYTES + 1);
        let err = validate_optional_since(Some(&huge)).unwrap_err();
        assert!(err.contains("payload too large"), "got: {err}");
        assert!(err.contains("since"), "got: {err}");
    }

    #[test]
    fn validate_container_id_rejects_empty() {
        assert!(validate_container_id("").is_err());
    }

    #[test]
    fn validate_container_id_rejects_oversize() {
        let huge = "a".repeat(MAX_CONTAINER_ID_BYTES + 1);
        assert!(validate_container_id(&huge).is_err());
    }

    #[test]
    fn validate_container_id_rejects_special_chars() {
        assert!(validate_container_id("abc;rm -rf /").is_err());
        assert!(validate_container_id("abc/def").is_err());
        assert!(validate_container_id("abc def").is_err());
    }

    #[test]
    fn validate_container_id_accepts_podman_hex_id() {
        let id: String = "a".repeat(64);
        assert!(validate_container_id(&id).is_ok());
    }

    #[test]
    fn validate_container_id_accepts_named_id() {
        assert!(validate_container_id("forge-session-abc").is_ok());
        assert!(validate_container_id("forge_sandbox_1").is_ok());
    }

    #[test]
    fn classify_runtime_status_maps_ok_to_available() {
        assert_eq!(classify_runtime_status(Ok(())), RuntimeStatus::Available);
    }

    #[test]
    fn classify_runtime_status_maps_runtime_missing() {
        let s = classify_runtime_status(Err(OciError::RuntimeMissing("podman")));
        match s {
            RuntimeStatus::Missing { tool } => assert_eq!(tool, "podman"),
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn classify_runtime_status_maps_runtime_broken() {
        let s = classify_runtime_status(Err(OciError::RuntimeBroken {
            tool: "podman",
            stderr: "newuidmap missing".into(),
        }));
        match s {
            RuntimeStatus::Broken { tool, reason } => {
                assert_eq!(tool, "podman");
                assert!(reason.contains("newuidmap"));
            }
            other => panic!("expected Broken, got {other:?}"),
        }
    }

    #[test]
    fn classify_runtime_status_maps_rootless_unavailable() {
        let s = classify_runtime_status(Err(OciError::RootlessUnavailable {
            runtime: "podman",
            reason: "rootless=false".into(),
        }));
        assert!(matches!(s, RuntimeStatus::RootlessUnavailable { .. }));
    }

    #[test]
    fn classify_runtime_status_maps_other_to_unknown() {
        let s = classify_runtime_status(Err(OciError::CommandFailed {
            tool: "podman",
            args: vec!["info".into()],
            exit_code: Some(1),
            stderr: "boom".into(),
        }));
        assert!(matches!(s, RuntimeStatus::Unknown { .. }));
    }

    #[test]
    fn runtime_status_is_unavailable() {
        assert!(!RuntimeStatus::Available.is_unavailable());
        assert!(RuntimeStatus::Missing {
            tool: "podman".into()
        }
        .is_unavailable());
        assert!(RuntimeStatus::Broken {
            tool: "podman".into(),
            reason: "x".into(),
        }
        .is_unavailable());
        assert!(RuntimeStatus::RootlessUnavailable {
            tool: "podman".into(),
            reason: "x".into(),
        }
        .is_unavailable());
        assert!(RuntimeStatus::Unknown { reason: "x".into() }.is_unavailable());
    }

    #[tokio::test]
    async fn registry_register_and_list_are_sorted_by_started_at() {
        let reg = ContainerRegistryState::new();
        reg.register(make_container_info(
            "sess-2",
            "cid-2",
            "alpine:3.19",
            DateTime::parse_from_rfc3339("2025-04-26T10:00:01Z")
                .unwrap()
                .with_timezone(&Utc),
        ))
        .await;
        reg.register(make_container_info(
            "sess-1",
            "cid-1",
            "alpine:3.19",
            DateTime::parse_from_rfc3339("2025-04-26T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        ))
        .await;
        let list = reg.list().await;
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].container_id, "cid-1");
        assert_eq!(list[1].container_id, "cid-2");
    }

    #[tokio::test]
    async fn registry_unregister_drops_entry() {
        let reg = ContainerRegistryState::new();
        reg.register(make_container_info("sess-1", "cid-1", "alpine", Utc::now()))
            .await;
        assert_eq!(reg.list().await.len(), 1);
        reg.unregister("cid-1").await;
        assert_eq!(reg.list().await.len(), 0);
    }

    #[tokio::test]
    async fn registry_mark_stopped_flips_flag_without_removing() {
        let reg = ContainerRegistryState::new();
        reg.register(make_container_info("sess-1", "cid-1", "alpine", Utc::now()))
            .await;
        reg.mark_stopped("cid-1").await;
        let list = reg.list().await;
        assert_eq!(list.len(), 1);
        assert!(list[0].stopped);
    }

    #[test]
    fn make_container_info_emits_rfc3339_timestamp() {
        let ts = DateTime::parse_from_rfc3339("2025-04-26T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let info = make_container_info("sess", "cid", "alpine:3.19", ts);
        assert_eq!(info.started_at, "2025-04-26T10:00:00+00:00");
        assert_eq!(info.image, "alpine:3.19");
        assert!(!info.stopped);
    }

    #[test]
    fn runtime_status_serde_round_trip() {
        let cases = [
            RuntimeStatus::Available,
            RuntimeStatus::Missing {
                tool: "podman".into(),
            },
            RuntimeStatus::Broken {
                tool: "podman".into(),
                reason: "x".into(),
            },
            RuntimeStatus::RootlessUnavailable {
                tool: "podman".into(),
                reason: "rootless=false".into(),
            },
            RuntimeStatus::Unknown { reason: "x".into() },
        ];
        for case in cases {
            let body = serde_json::to_string(&case).unwrap();
            let back: RuntimeStatus = serde_json::from_str(&body).unwrap();
            assert_eq!(back, case);
        }
    }
}
