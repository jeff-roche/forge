use std::collections::HashMap;
use std::io::ErrorKind;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use forge_core::{
    apply_superseded,
    meta::{write_meta, SessionMeta},
    read_since, SessionId, SessionPersistence, SessionState, WorkspaceId,
};
use forge_ipc::{HelloAck, IpcEvent, IpcMessage, PROTO_VERSION, SCHEMA_VERSION};
use forge_providers::{MockProvider, Provider};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{broadcast, Mutex};

use crate::archive::archive_or_purge;
use crate::orchestrator::{
    run_turn, ApprovalDecision, CredentialContext, Orchestrator, PendingApprovals,
};
use crate::sandbox::ChildRegistry;
use crate::session::Session;
use crate::tools::AgentRuntime;
use crate::usage_flush::flush_session_usage_to_user_dir;
use forge_core::{ApprovalScope, MessageId};

/// F-593: Stable per-workspace id used to tag rows in the usage aggregate
/// (`<config>/forge/usage/<YYYY-MM>.json`). The canonical string form of the
/// workspace path is the simplest stable identity available — it survives
/// daemon restart, doesn't depend on a registry lookup, and is exactly what
/// the dashboard already shows. When no workspace is bound, a synthetic
/// `"<no-workspace>"` id is used so ephemeral runs still aggregate cleanly.
fn derive_workspace_id(workspace_path: Option<&Path>) -> WorkspaceId {
    match workspace_path {
        Some(p) => WorkspaceId::from_string(p.to_string_lossy().into_owned()),
        None => WorkspaceId::from_string("<no-workspace>".to_string()),
    }
}

/// F-354: Maximum time the daemon will wait for a connecting peer to send
/// the `Hello` frame and, once HelloAck is written, the `Subscribe` frame.
/// A silent peer past this deadline is dropped so connection tasks cannot
/// be pinned indefinitely (CWE-400 / CWE-770).
///
/// Override via `FORGE_IPC_HANDSHAKE_DEADLINE_MS` (tests use a small value
/// to keep the regression suite fast).
const HANDSHAKE_DEADLINE_DEFAULT: Duration = Duration::from_secs(10);

/// F-354: Post-handshake idle timeout on the client → daemon command
/// reader. Closes the inter-frame starvation gap: once the handshake
/// finishes, a peer that never sends another command is dropped after
/// this window so a stalled client cannot hold a runtime task forever.
///
/// Override via `FORGE_IPC_IDLE_TIMEOUT_MS`.
const IDLE_TIMEOUT_DEFAULT: Duration = Duration::from_secs(300);

fn handshake_deadline() -> Duration {
    std::env::var("FORGE_IPC_HANDSHAKE_DEADLINE_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(HANDSHAKE_DEADLINE_DEFAULT)
}

fn idle_timeout() -> Duration {
    std::env::var("FORGE_IPC_IDLE_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(IDLE_TIMEOUT_DEFAULT)
}

/// F-140: build the per-session `AgentRuntime` or surface a soft failure.
///
/// Returns `None` when the agent-def load fails or no defs resolve; the
/// session stays usable but `agent.spawn` returns the existing "agent
/// runtime not configured" shape. The root-instance spawn itself cannot
/// fail today (orchestrator only rejects `Isolation::Trusted` under
/// `AgentScope::User`; the synthesized root uses `Isolation::Process`),
/// so we unwrap the spawn error into an `eprintln` rather than
/// propagating.
async fn build_agent_runtime(workspace_path: Option<&Path>) -> Option<AgentRuntime> {
    let user_home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    // Workspace-anchored load when we have one; fall back to user-only so
    // embedder-less sessions (tests, ephemeral CLI runs) still get agent
    // defs the user has authored.
    let defs = match workspace_path {
        Some(ws) => forge_agents::load_agents(ws, &user_home),
        None => forge_agents::load_user_agents(&user_home),
    };
    let defs = match defs {
        Ok(d) => d,
        Err(e) => {
            // F-371: emission-only structured log; no subscriber is
            // installed in the daemon binary, so a consumer (forge-ide or
            // an operator piping through `tracing-subscriber`) attaches
            // upstream.
            tracing::warn!(
                target: "forge_session::server",
                error = %e,
                "agent runtime: skipping (load_agents failed)",
            );
            return None;
        }
    };

    let orchestrator = Arc::new(forge_agents::Orchestrator::new());
    // Synthesize a root `AgentDef` that represents "the session itself" —
    // the parent of every top-level `agent.spawn`. The name is a stable
    // internal marker so Agent Monitor consumers can recognise the root;
    // `Isolation::Process` is chosen so the orchestrator's User-scope
    // guard accepts the spawn.
    let root_def = forge_agents::AgentDef {
        name: "session".to_string(),
        description: Some("session root".to_string()),
        body: String::new(),
        allowed_paths: vec![],
        isolation: forge_agents::Isolation::Process,
        memory_enabled: false,
    };
    let root_instance = match orchestrator
        .spawn(root_def, forge_agents::SpawnContext::user())
        .await
    {
        Ok(inst) => inst,
        Err(e) => {
            tracing::warn!(
                target: "forge_session::server",
                error = %e,
                "agent runtime: session-root spawn failed",
            );
            return None;
        }
    };

    Some(AgentRuntime {
        orchestrator,
        agent_defs: Arc::new(defs),
        parent_instance_id: root_instance.id,
    })
}

/// F-601 / F-602: compute the effective `memory_enabled` flag for an agent.
///
/// Selection rules (in order of precedence):
///
/// 1. The user's `[memory.enabled.<agent>]` setting takes precedence when
///    present (F-602). The Dashboard Memory toggle writes this scalar.
/// 2. Otherwise the agent def's frontmatter `memory_enabled` flag governs
///    (F-601 default).
///
/// Pure / settings-input is the merged view from
/// [`forge_core::settings::load_merged_in`]. The caller resolves it once at
/// session start so per-turn cost stays unchanged.
pub(crate) fn effective_memory_enabled(def_enabled: bool, settings_override: Option<bool>) -> bool {
    settings_override.unwrap_or(def_enabled)
}

/// F-601: resolve the active agent's cross-session memory.
///
/// Returns `(memory_body, memory_binding)`:
///
/// - `memory_body`: the markdown body to append under `## Memory` after
///   `AGENTS.md`, or `None` if memory is disabled / unavailable.
/// - `memory_binding`: the [`MemoryWriteBinding`] used by the dispatcher
///   cache to register `memory.write`, or `None` if memory is disabled.
///
/// Selection rules:
///
/// 1. The active agent name is the `active_agent` parameter — passed in
///    by the caller (typically the Tauri shell, which knows which agent
///    each window is targeting). `None` or an empty/whitespace string →
///    memory off (returns `(None, None)`); the session behaves exactly
///    as before F-601.
/// 2. If the named agent is not found among loaded defs, memory stays off
///    and we log at WARN.
/// 3. F-602: the user's `[memory.enabled.<agent>]` setting overrides the
///    def-level frontmatter flag when present. Absent → fall back to the
///    def's `memory_enabled`.
/// 4. Otherwise, build a [`MemoryStore`] anchored at the platform config
///    dir and try to load the existing file. A missing file yields an
///    empty body but the binding is still produced so `memory.write` can
///    create the file on the agent's first write.
///
/// Reads are best-effort: a corrupted memory file logs a warning inside
/// [`MemoryStore::load`] and surfaces as `None` body — the session stays
/// up.
async fn resolve_session_memory(
    runtime: Option<&AgentRuntime>,
    active_agent: Option<&str>,
    settings_override: Option<bool>,
) -> (
    Option<String>,
    Option<crate::dispatcher_cache::MemoryWriteBinding>,
) {
    let Some(rt) = runtime else {
        return (None, None);
    };
    let Some(active_agent) = active_agent.map(str::trim).filter(|s| !s.is_empty()) else {
        return (None, None);
    };
    let Some(def) = rt.agent_defs.iter().find(|a| a.name == active_agent) else {
        tracing::warn!(
            target: "forge_session::server",
            requested = %active_agent,
            "active_agent names an agent that is not loaded; memory disabled",
        );
        return (None, None);
    };
    if !effective_memory_enabled(def.memory_enabled, settings_override) {
        return (None, None);
    }
    let Some(store) = forge_agents::MemoryStore::from_home() else {
        tracing::warn!(
            target: "forge_session::server",
            "could not resolve user config directory for cross-session memory; disabling",
        );
        return (None, None);
    };
    resolve_session_memory_with(active_agent, std::sync::Arc::new(store)).await
}

/// Pure helper extracted from [`resolve_session_memory`] so the
/// active-agent → memory binding can be unit-tested without touching the
/// platform config dir. The caller is responsible for skip / opt-in
/// gating; this helper assumes the caller has already validated
/// `active_agent` against a loaded def with `memory_enabled: true`.
///
/// Splitting the helper this way is what makes the F-601 regression test
/// for two concurrent sessions in the same process tractable: the test
/// spins up two `MemoryStore`s (one per agent) and asserts that each
/// `active_agent` parameter resolves to its own body. The bug this
/// guards against — the original `FORGE_ACTIVE_AGENT` env-var read —
/// would have shared state across the two calls.
async fn resolve_session_memory_with(
    active_agent: &str,
    store: std::sync::Arc<forge_agents::MemoryStore>,
) -> (
    Option<String>,
    Option<crate::dispatcher_cache::MemoryWriteBinding>,
) {
    let body = match store.load(active_agent) {
        Ok(Some(memory)) => Some(memory.body),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                target: "forge_session::server",
                error = %e,
                "memory load failed; injection skipped, write tool stays available",
            );
            None
        }
    };
    let binding = crate::dispatcher_cache::MemoryWriteBinding {
        store,
        agent_id: active_agent.to_string(),
    };
    (body, Some(binding))
}

/// F-602: load the merged settings and look up the user's per-agent
/// `memory.enabled.<agent>` override. Returns `None` when no workspace is
/// known, when the active-agent param is empty, or when the setting is
/// absent for that agent. Best-effort: a settings-load IO error logs at
/// WARN and yields `None` — the session falls back to the def's
/// frontmatter flag.
async fn load_memory_setting_override(
    workspace: Option<&Path>,
    active_agent: Option<&str>,
) -> Option<bool> {
    let workspace = workspace?;
    let agent = active_agent.map(str::trim).filter(|s| !s.is_empty())?;
    let user_dir = dirs::config_dir();
    match forge_core::settings::load_merged_in(user_dir.as_deref(), workspace).await {
        Ok(settings) => settings.memory.enabled.get(agent).copied(),
        Err(e) => {
            tracing::warn!(
                target: "forge_session::server",
                error = %e,
                "failed to load merged settings for memory override; falling back to def-level flag",
            );
            None
        }
    }
}

/// Static name for an `IpcMessage` discriminant — used for diagnostic logging
/// when the dispatch loop encounters a frame that is structurally valid but
/// not legal at the post-handshake stage of a session connection (F-074).
/// Exhaustive over the variant set; adding a new variant is a compile error.
fn ipc_message_kind(msg: &IpcMessage) -> &'static str {
    match msg {
        IpcMessage::Hello(_) => "Hello",
        IpcMessage::HelloAck(_) => "HelloAck",
        IpcMessage::Subscribe(_) => "Subscribe",
        IpcMessage::Event(_) => "Event",
        IpcMessage::SendUserMessage(_) => "SendUserMessage",
        IpcMessage::ToolCallApproved(_) => "ToolCallApproved",
        IpcMessage::ToolCallRejected(_) => "ToolCallRejected",
        IpcMessage::RerunMessage(_) => "RerunMessage",
        IpcMessage::SelectBranch(_) => "SelectBranch",
        IpcMessage::DeleteBranch(_) => "DeleteBranch",
        IpcMessage::ListMcpServers(_) => "ListMcpServers",
        IpcMessage::McpServersList(_) => "McpServersList",
        IpcMessage::ToggleMcpServer(_) => "ToggleMcpServer",
        IpcMessage::McpToggleResult(_) => "McpToggleResult",
        IpcMessage::ImportMcpConfig(_) => "ImportMcpConfig",
        IpcMessage::McpImportResult(_) => "McpImportResult",
        IpcMessage::CompactTranscript(_) => "CompactTranscript",
    }
}

/// Compute the session's `allowed_paths` glob list from its workspace root.
///
/// Returns `[format!("{}/**", canonical_ws)]` when `workspace` points at an
/// existing directory that canonicalizes successfully. Returns `vec![]`
/// otherwise (no workspace, or canonicalization failure). An empty list causes
/// every `fs.*` tool call to be rejected by
/// `forge_fs::validate_against_globs`, which is the intended fail-closed
/// behaviour when the session has no workspace to scope filesystem access to.
pub(crate) fn compute_allowed_paths(workspace: Option<&Path>) -> Vec<String> {
    let Some(ws) = workspace else {
        return Vec::new();
    };
    match std::fs::canonicalize(ws) {
        Ok(canonical) => vec![format!("{}/**", canonical.display())],
        Err(_) => Vec::new(),
    }
}

/// Resolves the events.jsonl path for a daemon session.
///
/// When `workspace` is provided, the log lives under
/// `<workspace>/.forge/sessions/<session_id>/events.jsonl`, which causes
/// `forge_core::workspace::ensure_gitignore` to bootstrap the workspace's
/// `.forge/.gitignore` on first session. Otherwise falls back to
/// `<temp_dir>/forge-session-<session_id>/events.jsonl` for tests and ad-hoc runs.
pub fn event_log_path(session_id: &str, workspace: Option<&Path>) -> PathBuf {
    match workspace {
        Some(ws) => ws
            .join(".forge")
            .join("sessions")
            .join(session_id)
            .join("events.jsonl"),
        None => std::env::temp_dir()
            .join(format!("forge-session-{session_id}"))
            .join("events.jsonl"),
    }
}

/// F-132: construct an `McpManager` from the workspace and user `.mcp.json`
/// files, start every configured server, and return the `Arc<_>` for
/// `run_turn` to register tools against.
///
/// Failures at any stage (missing config, malformed JSON, server refuses
/// `initialize`) are non-fatal: the session keeps running without the
/// failed servers. Logging surfaces each failure reason so an operator
/// can diagnose without killing the daemon. `None` is returned only when
/// the config produced zero servers — either because both files were
/// absent or every entry failed to parse — so the caller can skip MCP
/// registration entirely without creating a manager-shaped no-op.
///
/// The user-scope home directory is resolved via `dirs::home_dir`; a
/// missing `$HOME` (extremely unusual — CI without `HOME` set) skips the
/// user-scope file and loads workspace only. That's the fail-open path:
/// we'd rather run with fewer servers than fail the whole session.
async fn load_mcp_manager(workspace_path: Option<&Path>) -> Option<Arc<forge_mcp::McpManager>> {
    let user_dir = dirs::home_dir();
    let merged = match workspace_path {
        Some(ws) => match (user_dir.as_deref(), forge_mcp::config::load_workspace(ws)) {
            (Some(home), ws_result) => match (forge_mcp::config::load_user_from(home), ws_result) {
                (Ok(mut merged), Ok(ws_specs)) => {
                    for (name, spec) in ws_specs {
                        merged.insert(name, spec);
                    }
                    merged
                }
                (Err(e), _) | (_, Err(e)) => {
                    tracing::warn!(
                        target: "forge_session::mcp",
                        error = %format!("{e:#}"),
                        "failed to load .mcp.json; continuing without MCP",
                    );
                    return None;
                }
            },
            (None, Ok(ws_specs)) => ws_specs,
            (None, Err(e)) => {
                tracing::warn!(
                    target: "forge_session::mcp",
                    error = %format!("{e:#}"),
                    "failed to load workspace .mcp.json",
                );
                return None;
            }
        },
        None => match user_dir.as_deref() {
            Some(home) => match forge_mcp::config::load_user_from(home) {
                Ok(specs) => specs,
                Err(e) => {
                    tracing::warn!(
                        target: "forge_session::mcp",
                        error = %format!("{e:#}"),
                        "failed to load user .mcp.json",
                    );
                    return None;
                }
            },
            None => return None,
        },
    };

    if merged.is_empty() {
        return None;
    }

    let mgr = Arc::new(forge_mcp::McpManager::new(merged));
    // Start every configured server. `start()` itself never fails for a
    // valid config; the driver task surfaces spawn/handshake errors via
    // the state stream and retries per the restart policy. Not awaiting
    // the handshake here keeps session startup snappy — the first turn
    // observes whichever servers became `Healthy` in time.
    let names: Vec<String> = mgr.list().await.into_iter().map(|s| s.name).collect();
    for name in names {
        if let Err(e) = mgr.start(&name).await {
            tracing::warn!(
                target: "forge_session::mcp",
                server_id = %name,
                error = %format!("{e:#}"),
                "mcp server start failed",
            );
        }
    }
    Some(mgr)
}

/// F-155: run an `ImportMcpConfig` request on the daemon.
///
/// Converts a third-party tool's MCP config into Forge's universal
/// `.mcp.json` schema, merges it on top of the existing workspace
/// `.mcp.json` (workspace entries win on name collision), and — when
/// `imp.apply` is `true` — rewrites the file atomically. Returns the
/// `IpcMessage::McpImportResult` frame ready to send back to the client.
///
/// Dry-run mode (`apply: false`) computes the same merged set but skips
/// the file write. `destination_path` is populated in both cases so the
/// UI can show where the apply would land.
async fn handle_import_mcp_config(
    imp: &forge_ipc::ImportMcpConfig,
    workspace_path: Option<&Path>,
) -> IpcMessage {
    let import_source = match forge_mcp::import::ImportSource::from_slug(&imp.source) {
        Some(s) => s,
        None => {
            return IpcMessage::McpImportResult(forge_ipc::McpImportResult {
                source: imp.source.clone(),
                imported: Vec::new(),
                destination_path: String::new(),
                error: Some(format!("unknown import source {:?}", imp.source)),
            });
        }
    };

    let workspace = match workspace_path {
        Some(p) => p.to_path_buf(),
        None => {
            return IpcMessage::McpImportResult(forge_ipc::McpImportResult {
                source: imp.source.clone(),
                imported: Vec::new(),
                destination_path: String::new(),
                error: Some("daemon has no workspace path configured".into()),
            });
        }
    };
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => {
            return IpcMessage::McpImportResult(forge_ipc::McpImportResult {
                source: imp.source.clone(),
                imported: Vec::new(),
                destination_path: String::new(),
                error: Some("could not resolve user home directory".into()),
            });
        }
    };

    let source_path = match import_source.default_path(&workspace, &home) {
        Some(p) => p,
        None => {
            return IpcMessage::McpImportResult(forge_ipc::McpImportResult {
                source: imp.source.clone(),
                imported: Vec::new(),
                destination_path: String::new(),
                error: Some(format!("no default path known for source {:?}", imp.source)),
            });
        }
    };

    let source_body = match tokio::fs::read_to_string(&source_path).await {
        Ok(body) => body,
        Err(e) => {
            return IpcMessage::McpImportResult(forge_ipc::McpImportResult {
                source: imp.source.clone(),
                imported: Vec::new(),
                destination_path: String::new(),
                error: Some(format!("reading {}: {e}", source_path.display())),
            });
        }
    };
    let converted = match import_source.convert(&source_body) {
        Ok(c) => c,
        Err(e) => {
            return IpcMessage::McpImportResult(forge_ipc::McpImportResult {
                source: imp.source.clone(),
                imported: Vec::new(),
                destination_path: String::new(),
                error: Some(format!("converting {}: {e:#}", source_path.display())),
            });
        }
    };

    let destination_path = workspace.join(".mcp.json");
    let imported: Vec<String> = converted.keys().cloned().collect();

    if imp.apply {
        // Merge on top of the existing `.mcp.json` — never clobber
        // user-curated entries. Parse failures are surfaced because the
        // user would otherwise lose visibility into a corrupt config.
        let mut existing = match tokio::fs::read_to_string(&destination_path).await {
            Ok(body) => match parse_existing_mcp_body(&body) {
                Ok(m) => m,
                Err(e) => {
                    return IpcMessage::McpImportResult(forge_ipc::McpImportResult {
                        source: imp.source.clone(),
                        imported: Vec::new(),
                        destination_path: destination_path.display().to_string(),
                        error: Some(e),
                    });
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::collections::BTreeMap::new(),
            Err(e) => {
                return IpcMessage::McpImportResult(forge_ipc::McpImportResult {
                    source: imp.source.clone(),
                    imported: Vec::new(),
                    destination_path: destination_path.display().to_string(),
                    error: Some(format!("reading {}: {e}", destination_path.display())),
                });
            }
        };
        for (name, spec) in converted {
            existing.insert(name, spec);
        }
        let rendered = match forge_mcp::render_universal(&existing) {
            Ok(r) => r,
            Err(e) => {
                return IpcMessage::McpImportResult(forge_ipc::McpImportResult {
                    source: imp.source.clone(),
                    imported: Vec::new(),
                    destination_path: destination_path.display().to_string(),
                    error: Some(format!("rendering merged .mcp.json: {e:#}")),
                });
            }
        };
        if let Some(parent) = destination_path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return IpcMessage::McpImportResult(forge_ipc::McpImportResult {
                    source: imp.source.clone(),
                    imported: Vec::new(),
                    destination_path: destination_path.display().to_string(),
                    error: Some(format!("mkdir {}: {e}", parent.display())),
                });
            }
        }
        if let Err(e) = tokio::fs::write(&destination_path, rendered).await {
            return IpcMessage::McpImportResult(forge_ipc::McpImportResult {
                source: imp.source.clone(),
                imported: Vec::new(),
                destination_path: destination_path.display().to_string(),
                error: Some(format!("writing {}: {e}", destination_path.display())),
            });
        }
        // A future revision may rebuild the `McpManager` here so freshly
        // imported servers reach `Healthy` without a session restart.
        // F-155 intentionally stops short of that — import is rare, a
        // session restart is already the user's typical motion, and the
        // scope here is the lifecycle-unification correctness story.
    }

    IpcMessage::McpImportResult(forge_ipc::McpImportResult {
        source: imp.source.clone(),
        imported,
        destination_path: destination_path.display().to_string(),
        error: None,
    })
}

/// Shared helper for `handle_import_mcp_config`: parse an existing
/// `.mcp.json` body into the same `BTreeMap<String, McpServerSpec>` shape
/// `render_universal` consumes. An empty file is `Ok(empty map)` so the
/// import path can tolerate a bootstrapped placeholder.
fn parse_existing_mcp_body(
    body: &str,
) -> std::result::Result<std::collections::BTreeMap<String, forge_mcp::McpServerSpec>, String> {
    if body.trim().is_empty() {
        return Ok(std::collections::BTreeMap::new());
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Doc {
        #[serde(rename = "mcpServers", default)]
        servers: std::collections::BTreeMap<String, serde_json::Value>,
    }
    let doc: Doc = serde_json::from_str(body).map_err(|e| format!("parsing .mcp.json: {e}"))?;
    let mut out = std::collections::BTreeMap::new();
    for (name, raw) in doc.servers {
        let wrapped = serde_json::json!({ "mcpServers": { &name: raw } });
        let mut parsed = forge_mcp::import::ImportSource::Cursor
            .convert(&wrapped.to_string())
            .map_err(|e| format!("invalid server entry {name:?} in destination: {e:#}"))?;
        if let Some(spec) = parsed.remove(&name) {
            out.insert(name, spec);
        }
    }
    Ok(out)
}

/// Start a session server using the default `MockProvider`.
pub async fn serve(path: &Path, auto_approve: bool, ephemeral: bool) -> Result<()> {
    let log_path = event_log_path(&SessionId::new().to_string(), None);
    let session = Arc::new(Session::create(log_path).await?);
    let provider = Arc::new(MockProvider::with_default_path());
    serve_with_session(
        path,
        session,
        provider,
        auto_approve,
        ephemeral,
        None,
        None,
        None,
        None,
    )
    .await
}

/// Timeout for the post-`EADDRINUSE` liveness probe. Short enough that a
/// genuinely orphaned socket doesn't stall daemon startup, long enough to let
/// a slow local daemon reply. The probe is `connect(2)` only — no handshake —
/// so the common-case round-trip is a few hundred microseconds.
const LIVENESS_PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// Bind a `UnixListener` at `path` without the classic pre-unlink TOCTOU.
///
/// F-056 (T6): the previous `if path.exists() { remove_file } ; bind` sequence
/// let an attacker with write access to the parent directory plant a symlink
/// and watch the daemon blindly unlink it. F-044 (H8) already closed the
/// practical `/tmp/forge-0/` window by refusing the shared-directory fallback
/// and chmod'ing the parent to 0o700, but the TOCTOU remains relevant as
/// defense-in-depth for any future path configuration (e.g. an operator
/// pointing `FORGE_SOCKET_PATH` into a shared location).
///
/// Protocol:
/// 1. Try `bind` first — never pre-unlink.
/// 2. On `EADDRINUSE`, probe with a short `UnixStream::connect`. If the probe
///    succeeds, another live daemon is listening: bail out. **Do not** unlink
///    another daemon's socket out from under it.
/// 3. On probe failure (connection refused or timeout), the entry is likely
///    an orphan. Confirm via `symlink_metadata` — which does **not** follow
///    symlinks — that the entry is a real socket file. Symlinks and regular
///    files are rejected outright: neither is a legitimate orphan, and
///    unlinking either would be the exact exploit path this fix closes.
/// 4. Unlink the confirmed orphan and retry `bind` exactly once.
async fn bind_uds_safely(path: &Path) -> Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            // Address in use — either a live daemon is already serving or a
            // prior daemon crashed leaving an orphan socket behind.
            let probe =
                tokio::time::timeout(LIVENESS_PROBE_TIMEOUT, UnixStream::connect(path)).await;
            if let Ok(Ok(_stream)) = probe {
                anyhow::bail!(
                    "refusing to bind at {}: another daemon is already listening",
                    path.display()
                );
            }

            // Probe failed (refused or timed out). Check the entry type
            // without following symlinks — `metadata()` would follow, which
            // is itself exploitable.
            let meta = tokio::fs::symlink_metadata(path).await.with_context(|| {
                format!(
                    "failed to stat {} while recovering from EADDRINUSE",
                    path.display()
                )
            })?;
            if !meta.file_type().is_socket() {
                anyhow::bail!(
                    "refusing to unlink {}: entry is not a socket (type={:?}). Remove it \
                     manually after verifying it is not attacker-planted.",
                    path.display(),
                    meta.file_type()
                );
            }

            tokio::fs::remove_file(path)
                .await
                .with_context(|| format!("failed to unlink orphan socket at {}", path.display()))?;
            UnixListener::bind(path)
                .with_context(|| format!("retry bind failed at {}", path.display()))
        }
        Err(e) => Err(e).with_context(|| format!("bind failed at {}", path.display())),
    }
}

/// Start a session server with an explicit provider.
///
/// `workspace` is reported back to clients via `HelloAck.workspace` (empty when `None`).
/// `session_id` is reported back to clients via `HelloAck.session_id` and identifies
/// this daemon's persistent session; when `None`, a fresh id is generated for the lifetime
/// of this server (so all connections to the same server still see the same value).
#[allow(clippy::too_many_arguments)]
pub async fn serve_with_session<P: Provider + 'static>(
    path: &Path,
    session: Arc<Session>,
    provider: Arc<P>,
    auto_approve: bool,
    ephemeral: bool,
    workspace: Option<PathBuf>,
    session_id: Option<String>,
    // F-587: optional per-turn credential pull. When `Some`, every
    // `run_turn` invocation in this session reads the active provider's
    // credential through this context (see `CredentialContext` for the
    // contract). `None` keeps the keyless `OllamaProvider` / `MockProvider`
    // path identical to pre-F-587 behaviour.
    credentials: Option<CredentialContext>,
    // F-601: the agent the session is bound to for cross-session memory
    // selection. `None` (or empty/whitespace) → memory off and the session
    // behaves exactly as before F-601. The Tauri shell knows which agent
    // each window targets and passes it in here; tests pass `None` unless
    // the test exercises memory injection. Replaces the prior
    // `FORGE_ACTIVE_AGENT` env-var indirection: a typed parameter is the
    // only safe way to give different connections in a persistent-mode
    // daemon distinct active agents.
    active_agent: Option<String>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
        // F-044 (H8): tighten the socket's parent dir to 0o700 regardless of
        // whether we created it or found it pre-existing. `XDG_RUNTIME_DIR`
        // itself is 0o700 per systemd.exec(5), but the `forge/sessions`
        // subdir sits inside and inherits `0o777 & ~umask` (typically 0o755)
        // on first creation. Applying 0o700 explicitly closes the tiny window
        // where the subdir is created loose and means the defense-in-depth
        // survives if a future operator runs forged with `XDG_RUNTIME_DIR`
        // pointed somewhere with a permissive parent. Best-effort: if the
        // chmod fails (e.g. parent owned by another user under FORGE_SOCKET_PATH
        // in a test), the 0o600 mode on the socket itself is the real defense.
        let _ = tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await;
    }
    let listener = bind_uds_safely(path).await?;
    // F-044 (H8): chmod the socket to 0o600 the moment it exists. bind(2)
    // creates the socket file with `0o777 & ~umask`, typically 0o755 —
    // world-connectable, which in Phase 1 means any local user can drive the
    // session. There is a brief post-bind TOCTOU window between `bind` and
    // this chmod; it is a distinct concern from F-056 (which closed the
    // *pre-bind* `remove_file`→`bind` race via `bind_uds_safely`) and remains
    // bounded by the 0o700 parent-dir mode set above. We refuse to proceed if
    // the chmod fails rather than serve on a permissive socket.
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    let workspace_path: Option<PathBuf> = workspace.clone();
    // F-043: Derive `allowed_paths` from the workspace root so `fs.*` tools can
    // only touch files inside the session's workspace. Fail-closed — if no
    // workspace is configured, or canonicalization fails (e.g. the workspace
    // path doesn't exist), the session stays usable but every `fs.*` call is
    // rejected by `forge_fs::validate_against_globs` (empty list matches
    // nothing). Previously this was `vec!["**"]`, which matched every absolute
    // path including `/etc/passwd` and `~/.ssh/id_rsa`.
    let allowed_paths: Arc<Vec<String>> =
        Arc::new(compute_allowed_paths(workspace_path.as_deref()));

    // F-135 / F-352 / F-566: load workspace `AGENTS.md` exactly once at
    // session start **and** pre-build the labeled system-prompt prefix here so
    // `run_turn` never re-formats per turn. Missing file is not an error;
    // unreadable file is logged once and treated as absent so a transient
    // filesystem problem never fails the session.
    //
    // Stored as `Option<Arc<str>>`: each turn clones the `Arc` into
    // `ChatRequest.system` (refcount bump), and per-iteration `req.clone()`
    // inside `run_request_loop` is now refcount-only on the prefix even when
    // the prefix is hundreds of KiB.
    //
    // F-601: when an active agent is selected via the `active_agent`
    // parameter and that agent's def has `memory_enabled: true`, the
    // cross-session memory body is appended after the AGENTS.md prefix
    // under a `## Memory` heading; both the load and the assembly run
    // once here so per-turn cost stays at the existing `Arc::clone`.
    let agents_md_prefix: Option<String> = match workspace_path.as_deref() {
        Some(ws) => match forge_agents::load_agents_md(ws) {
            Ok(Some(content)) => {
                tracing::warn!(
                    path = %ws.join("AGENTS.md").display(),
                    "AGENTS.md injected into session system prompt; \
                     review the file if this workspace is not fully trusted"
                );
                Some(format!("\n\n---\nAGENTS.md (workspace):\n{content}"))
            }
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(
                    target: "forge_session::server",
                    error = %e,
                    "AGENTS.md: skipping injection",
                );
                None
            }
        },
        None => None,
    };

    // F-132 / F-155: build the MCP manager from `.mcp.json` (workspace +
    // user) and start every configured server. Failures at this layer are
    // non-fatal — a missing or malformed config, or a server that refuses
    // to initialize, leaves the session without MCP tools but still
    // functional. Each server's health is tracked per-server inside the
    // manager; `list()` reflects the state so the shell's
    // `list_mcp_servers` command (which now dispatches to this daemon via
    // UDS) can show the failure reason.
    //
    // F-155: this is now the single authoritative `McpManager` for the
    // shell+daemon pair. The shell no longer runs its own manager; every
    // `list_mcp_servers` / `toggle_mcp_server` / `import_mcp_config`
    // Tauri command bounces through this session's UDS so a toggle
    // affects running tool-call dispatch immediately.
    let mcp = load_mcp_manager(workspace_path.as_deref()).await;

    // F-371: build `session_id` before spawning the MCP state forwarder so
    // the forwarder can carry it on every structured log line. Moved up
    // from its original position near `socket_path`; other consumers
    // (`agent_runtime`) still read it below through the `Arc` clone.
    let session_id = Arc::new(session_id.unwrap_or_else(|| SessionId::new().to_string()));

    // F-567: shared epoch the dispatcher cache keys against. Every
    // `McpStateEvent` bumps it so the cached dispatcher is invalidated
    // exactly when a server transition could change the advertised tool
    // set. With no MCP wired up, the epoch stays at zero forever and the
    // cache rebuilds the dispatcher exactly once.
    let mcp_tools_epoch = crate::dispatcher_cache::McpToolsEpoch::new();

    // F-155: fan out `McpManager::state_stream()` onto the session event
    // log. Every `Starting / Healthy / Degraded / Failed / Disabled`
    // transition becomes an `Event::McpState(McpStateEvent)` on the log,
    // which reaches the shell's session event forwarder and the webview
    // via the normal event pipeline. Subscribe once per daemon (not per
    // connection) because the broadcast channel fans out identical events
    // and the session event log is the sole canonical destination.
    //
    // F-567: same forwarder bumps `mcp_tools_epoch` so the next turn
    // rebuilds the dispatcher cache from `mgr.list().await`. Bumping
    // before emitting keeps the invariant simple: if a turn observes the
    // event on the log, the corresponding cache entry is already stale.
    if let Some(mcp_mgr) = mcp.as_ref() {
        let session_for_mcp = Arc::clone(&session);
        let session_id_for_mcp = Arc::clone(&session_id);
        let epoch_for_mcp = mcp_tools_epoch.clone();
        let mut stream = mcp_mgr.state_stream();
        tokio::spawn(async move {
            use futures::StreamExt;
            while let Some(ev) = stream.next().await {
                epoch_for_mcp.bump();
                if let Err(e) = session_for_mcp.emit(forge_core::Event::McpState(ev)).await {
                    tracing::error!(
                        target: "forge_session::mcp",
                        session_id = %session_id_for_mcp,
                        error = %e,
                        "mcp state-event emit failed",
                    );
                }
            }
        });
    }

    // F-140: session-scoped agent runtime so live turns can actually spawn
    // sub-agents via `agent.spawn`.
    //
    // One `forge_agents::Orchestrator` lives for the session's lifetime,
    // shared across every turn so the Agent Monitor subscribes once and
    // sees every spawn. We pre-register a "session root" `AgentInstance`
    // whose id becomes the stable `parent_instance_id` for every
    // top-level spawn and the `StepStarted.instance_id` for every turn
    // loop emission. Agent defs are loaded once from `<workspace>/.agents`
    // + `<home>/.agents` — a failed load downgrades the runtime to
    // `None` so a filesystem blip never kills the whole session, it just
    // reverts to the pre-F-140 "agent runtime not configured" behaviour.
    //
    // F-601: built before the dispatcher cache so the active agent's
    // `memory_enabled` flag can decide whether `memory.write` registers
    // and whether the memory body is appended to the system prompt.
    let agent_runtime: Option<AgentRuntime> = build_agent_runtime(workspace_path.as_deref()).await;

    // F-602: consult the user's `[memory.enabled.<agent>]` settings entry
    // before deciding whether memory is on. The Dashboard's Memory toggle
    // writes this scalar; when present it overrides the def's
    // frontmatter flag. Absent → fall through to the F-601 def-level
    // semantic. Read once at session start so per-turn cost is unchanged.
    let memory_setting_override: Option<bool> =
        load_memory_setting_override(workspace_path.as_deref(), active_agent.as_deref()).await;

    // F-601: resolve the active agent + load its memory once per session.
    //
    // The active agent is selected via the `active_agent` parameter. When
    // set and matching a loaded def, the F-602 settings override (if
    // present) or the def's `memory_enabled` flag controls both memory
    // injection and `memory.write` registration. When `None`, unmatched,
    // or resolved-disabled, memory is off — the session behaves exactly
    // as before F-601.
    let (memory_body, memory_binding) = resolve_session_memory(
        agent_runtime.as_ref(),
        active_agent.as_deref(),
        memory_setting_override,
    )
    .await;

    // F-601: assemble the final per-turn system prompt by appending the
    // optional memory body under a `## Memory` heading after AGENTS.md.
    // Stored as `Arc<str>` so each turn clones the refcount, never the
    // bytes.
    let agents_md: Option<Arc<str>> =
        forge_agents::assemble_system_prompt(agents_md_prefix.as_deref(), memory_body.as_deref())
            .map(Arc::from);

    let agent_runtime = Arc::new(agent_runtime);

    // F-567: the per-session dispatcher cache. Lives across every turn so
    // the builtins register exactly once and MCP adapters rebuild only
    // when `mcp_tools_epoch` advances.
    //
    // F-601: when the active agent has `memory_enabled: true`,
    // `memory.write` registers alongside the other built-ins so the agent
    // can update its own cross-session memory file. Without the binding,
    // the tool stays unregistered.
    let dispatcher_cache = match memory_binding {
        Some(b) => crate::dispatcher_cache::DispatcherCache::with_memory(mcp_tools_epoch, b),
        None => crate::dispatcher_cache::DispatcherCache::new(mcp_tools_epoch),
    };
    let workspace = Arc::new(
        workspace
            .map(|w| w.display().to_string())
            .unwrap_or_default(),
    );
    // `session_id` already constructed above (F-371) so the MCP forwarder
    // could attach it to its structured logs.

    let socket_path = Arc::new(path.to_path_buf());
    // Session-scoped registry of sandboxed child process groups. Killed on
    // session shutdown so tool subprocesses (e.g. `shell.exec`) cannot outlive
    // the daemon.
    let child_registry = ChildRegistry::new();
    // F-077: per-session aggregate byte budget. One `ByteBudget` is shared
    // across every `run_turn` invocation in this session so chained tool
    // calls collectively decrement against the same ceiling. Default is
    // 500 MiB (`ByteBudget::default`); see `docs/dev/security.md`.
    let byte_budget = Arc::new(crate::byte_budget::ByteBudget::default());

    if ephemeral {
        // Accept exactly one connection, serve it to completion, then exit.
        // `handle_connection` performs the session-scoped process-group
        // cleanup in its ephemeral branch.
        let (stream, _) = listener.accept().await?;
        return handle_connection(
            stream,
            session,
            provider,
            auto_approve,
            true,
            workspace,
            session_id,
            socket_path,
            workspace_path,
            allowed_paths,
            child_registry,
            byte_budget,
            agents_md,
            agent_runtime,
            mcp,
            dispatcher_cache,
            credentials.clone(),
        )
        .await;
    }

    // Persistent mode: write the initial meta.toml so `archive_or_purge`'s
    // `update_meta_to_archived` call has something to update on shutdown.
    // F-031's archive feature only updates an existing meta file; without
    // this write, the archived directory ships with no meta.toml at all.
    if let Some(session_dir) = session.log_path.parent() {
        let meta_path = session_dir.join("meta.toml");
        let meta = SessionMeta {
            id: SessionId::from_string((*session_id).clone()),
            // TODO: derive workspace_id from the workspace path so sessions
            // sharing a workspace correlate. Today no production code groups
            // on workspace_id (dashboard reads `workspace` string), so a
            // synthetic per-session id is acceptable as a placeholder.
            workspace_id: WorkspaceId::new(),
            name: format!("session-{}", *session_id),
            agent: None,
            provider_id: None,
            model: None,
            state: SessionState::Active,
            persistence: SessionPersistence::Persist,
            started_at: Utc::now(),
            ended_at: None,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            pid: std::process::id(),
            socket_path: (*socket_path).clone(),
        };
        write_meta(&meta_path, &meta).await?;
    }

    // Persistent mode: race the accept loop against SIGTERM/SIGINT so the
    // daemon can run `archive_or_purge` on its own session dir before exit.
    // Without this, `forge session kill` SIGTERMs an unhandled signal and
    // the live session dir is left behind under `sessions/<id>/`.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    loop {
        tokio::select! {
            accept = listener.accept() => {
                let (stream, _) = accept?;
                let session = Arc::clone(&session);
                let provider = Arc::clone(&provider);
                let workspace = Arc::clone(&workspace);
                let session_id = Arc::clone(&session_id);
                let socket_path = Arc::clone(&socket_path);
                let workspace_path = workspace_path.clone();
                let allowed_paths = Arc::clone(&allowed_paths);
                let child_registry = child_registry.clone();
                let byte_budget = Arc::clone(&byte_budget);
                let agents_md = agents_md.clone();
                let mcp = mcp.clone();
                let agent_runtime = Arc::clone(&agent_runtime);
                let dispatcher_cache = Arc::clone(&dispatcher_cache);
                let credentials_for_conn = credentials.clone();
                tokio::spawn(async move {
                    // F-371: capture session_id for logging *before* the move
                    // into `handle_connection` consumes the Arc.
                    let session_id_for_log = Arc::clone(&session_id);
                    if let Err(e) = handle_connection(
                        stream,
                        session,
                        provider,
                        auto_approve,
                        false,
                        workspace,
                        session_id,
                        socket_path,
                        workspace_path,
                        allowed_paths,
                        child_registry,
                        byte_budget,
                        agents_md,
                        agent_runtime,
                        mcp,
                        dispatcher_cache,
                        credentials_for_conn,
                    )
                    .await
                    {
                        tracing::error!(
                            target: "forge_session::server",
                            session_id = %session_id_for_log,
                            error = %e,
                            "connection error",
                        );
                    }
                });
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }

    // Graceful shutdown for persistent mode: kill any sandboxed children,
    // then archive the session dir + remove the socket. Errors propagate so
    // `forged` exits non-zero on archive failure (so callers like a future
    // `forge session kill --wait` can surface the failure).
    //
    // In-flight `tokio::spawn`'d connection tasks (above) are not joined —
    // they're orphaned and the runtime drops them on process exit. On the
    // normal (same-filesystem) archive path, `rename` preserves open file
    // descriptors so any late EventLog write still lands on the correct
    // inode; meta.toml writes are atomic via temp+rename in
    // `forge_core::meta::write_meta`, so a torn-meta hazard is not possible.
    // The EXDEV fallback in `archive_or_purge` (cross-filesystem copy +
    // remove_dir_all) is theoretically unreachable here because the source
    // and destination both live under `<workspace>/.forge/sessions/`. If
    // shutdown ever needs to await in-flight turns, replace the spawn with
    // a JoinSet drained here with a timeout.
    child_registry.kill_all();
    // F-593: flush the persistent session's usage tally before archiving the
    // session dir. Running before archive_or_purge is intentional — the
    // archive step renames the session dir into the archive root and the
    // event-log path the flush reads from must still be at its live location.
    let workspace_id_for_flush = derive_workspace_id(workspace_path.as_deref());
    if let Err(e) =
        flush_session_usage_to_user_dir(&session.log_path, &workspace_id_for_flush).await
    {
        tracing::warn!(
            target: "forge_session::server",
            session_id = %session_id,
            error = %e,
            "usage flush failed at persistent shutdown",
        );
    }
    if let Some(session_dir) = session.log_path.parent() {
        archive_or_purge(session_dir, SessionPersistence::Persist, &socket_path).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection<P: Provider + 'static>(
    mut stream: UnixStream,
    session: Arc<Session>,
    provider: Arc<P>,
    auto_approve: bool,
    ephemeral: bool,
    workspace: Arc<String>,
    session_id: Arc<String>,
    socket_path: Arc<PathBuf>,
    workspace_path: Option<PathBuf>,
    allowed_paths: Arc<Vec<String>>,
    child_registry: ChildRegistry,
    byte_budget: Arc<crate::byte_budget::ByteBudget>,
    agents_md: Option<Arc<str>>,
    agent_runtime: Arc<Option<AgentRuntime>>,
    mcp: Option<Arc<forge_mcp::McpManager>>,
    // F-567: shared dispatcher cache, refreshed only on MCP-tools-list epoch
    // bumps. Cloning the `Arc` per turn is the steady-state cost.
    dispatcher_cache: Arc<crate::dispatcher_cache::DispatcherCache>,
    // F-587: per-turn credential pull binding. `None` for keyless providers.
    credentials: Option<CredentialContext>,
) -> Result<()> {
    // ── Handshake ──────────────────────────────────────────────────────────────
    // F-354: both handshake reads are subject to a bounded deadline so a
    // silent/stalled peer cannot pin a daemon task indefinitely.
    let deadline = handshake_deadline();
    let msg = forge_ipc::read_frame_with_deadline(&mut stream, deadline).await?;
    let IpcMessage::Hello(hello) = msg else {
        anyhow::bail!("expected Hello, got unexpected message type");
    };
    if hello.proto != PROTO_VERSION {
        anyhow::bail!("unsupported protocol version: {}", hello.proto);
    }

    let ack = IpcMessage::HelloAck(HelloAck {
        session_id: (*session_id).clone(),
        workspace: (*workspace).clone(),
        started_at: chrono::Utc::now().to_rfc3339(),
        event_seq: session.current_seq().await,
        schema_version: SCHEMA_VERSION,
    });
    forge_ipc::write_frame(&mut stream, &ack).await?;

    // ── Subscribe + history replay ─────────────────────────────────────────────
    let msg = forge_ipc::read_frame_with_deadline(&mut stream, deadline).await?;
    let IpcMessage::Subscribe(sub) = msg else {
        anyhow::bail!("expected Subscribe after HelloAck");
    };

    // Subscribe to live broadcast BEFORE reading history to avoid missing events.
    let mut live_rx = session.event_tx.subscribe();

    // F-143: filter the historical replay through `apply_superseded` so a
    // fresh subscriber sees a coherent transcript — superseded assistant
    // messages (and their deltas / tool calls) are hidden, along with the
    // `MessageSuperseded` markers themselves. Live events pumped through
    // `live_rx` below stay unfiltered: late-joining peers see replay from
    // `read_since`, and the orchestrator emits `MessageSuperseded` after
    // the regenerated turn is finalised so live peers that were attached
    // throughout receive both the original and the new message — their UI
    // is free to interpret the marker as it chooses.
    let history = read_since(&session.log_path, sub.since).await?;
    let history = apply_superseded(history);
    let mut last_sent = sub.since;

    // Split stream so we can read and write concurrently.
    let (mut reader, mut writer) = stream.into_split();

    for (seq, event) in history {
        // F-112: `IpcEvent.event` is now typed — no `serde_json::to_value`
        // intermediate. serde walks `Event` directly to bytes.
        let frame = IpcMessage::Event(IpcEvent { seq, event });
        forge_ipc::write_frame(&mut writer, &frame).await?;
        last_sent = seq;
    }

    // ── Bidirectional loop ─────────────────────────────────────────────────────
    // Pending tool call approvals shared between this loop and spawned turn tasks.
    let pending_approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));

    // Channel for commands arriving from the client reader.
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<IpcMessage>(32);

    // Spawn a task that forwards client frames onto the command channel.
    // F-354: apply a per-frame idle timeout so a peer that completes the
    // handshake and then stalls cannot hold the connection task forever.
    // A timeout causes the reader task to exit; the outer `select!` loop
    // sees `cmd_rx` close and tears the connection down.
    let idle = idle_timeout();
    tokio::spawn(async move {
        while let Ok(msg) = forge_ipc::read_frame_with_deadline(&mut reader, idle).await {
            if cmd_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            // Live events → forward to client.
            result = live_rx.recv() => {
                match result {
                    Ok((seq, event)) if seq > last_sent => {
                        let is_session_ended = matches!(event, forge_core::Event::SessionEnded { .. });
                        // F-112: typed path — no Value intermediate.
                        let frame = IpcMessage::Event(IpcEvent { seq, event });
                        forge_ipc::write_frame(&mut writer, &frame).await?;
                        last_sent = seq;
                        if is_session_ended {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            target: "forge_session::server",
                            session_id = %session_id,
                            dropped = n,
                            "subscriber dropped events; closing connection",
                        );
                        break;
                    }
                }
            }

            // Commands from client → dispatch.
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(IpcMessage::SendUserMessage(m)) => {
                        let session = Arc::clone(&session);
                        let provider = Arc::clone(&provider);
                        let approvals = Arc::clone(&pending_approvals);
                        let workspace_path = workspace_path.clone();
                        let allowed_paths = Arc::clone(&allowed_paths);
                        let child_registry = child_registry.clone();
                        let byte_budget = Arc::clone(&byte_budget);
                        let agents_md = agents_md.clone();
                        let mcp = mcp.clone();
                        let agent_runtime = (*agent_runtime).clone();
                        let dispatcher_cache = Arc::clone(&dispatcher_cache);
                        let session_id_for_turn = Arc::clone(&session_id);
                        // F-587: clone the per-session credential binding into
                        // the turn task. `Clone` on `Arc<dyn Credentials>` +
                        // `String` is a refcount bump; the actual store is
                        // shared across every turn in the session.
                        let credential_ctx_for_turn = credentials.clone();
                        // F-593: capture the session log path + workspace path
                        // *before* the move so the post-`SessionEnded` flush
                        // can read its own log without re-borrowing `session`.
                        let log_path_for_flush = session.log_path.clone();
                        let workspace_path_for_flush = workspace_path.clone();
                        tokio::spawn(async move {
                            let result = run_turn(
                                Arc::clone(&session),
                                provider,
                                m.text,
                                approvals,
                                (*allowed_paths).clone(),
                                auto_approve,
                                workspace_path,
                                Some(child_registry),
                                Some(byte_budget),
                                agents_md,
                                agent_runtime,
                                mcp,
                                Some(dispatcher_cache),
                                // F-587: per-turn credential pull. Wired
                                // when the daemon is constructed with a
                                // provider that needs auth (Anthropic /
                                // OpenAI in Phase 3); `None` for keyless
                                // providers (current `OllamaProvider` /
                                // `MockProvider`) so the orchestrator
                                // skips the pull entirely. The keyring
                                // store + active provider id flow in
                                // through `credential_ctx_for_turn`
                                // below, captured at session start.
                                credential_ctx_for_turn.clone(),
                            ).await;
                            if let Err(e) = &result {
                                tracing::warn!(
                                    target: "forge_session::server",
                                    session_id = %session_id_for_turn,
                                    error = %e,
                                    "turn error",
                                );
                            }
                            if ephemeral {
                                let reason = match result {
                                    Ok(()) => forge_core::EndReason::Completed,
                                    Err(e) => forge_core::EndReason::Error(e.to_string()),
                                };
                                if let Err(e) = session.emit(forge_core::Event::SessionEnded {
                                    at: chrono::Utc::now(),
                                    reason,
                                    archived: false,
                                }).await {
                                    tracing::error!(
                                        target: "forge_session::server",
                                        session_id = %session_id_for_turn,
                                        error = %e,
                                        "failed to emit SessionEnded",
                                    );
                                }
                                // F-593: flush per-session usage tally into the
                                // monthly aggregate. Best-effort — a flush
                                // failure must not surface to the client (the
                                // session is already ending) but is logged so
                                // operators see persistent corruption early.
                                let workspace_id_for_flush =
                                    derive_workspace_id(workspace_path_for_flush.as_deref());
                                if let Err(e) = flush_session_usage_to_user_dir(
                                    &log_path_for_flush,
                                    &workspace_id_for_flush,
                                ).await {
                                    tracing::warn!(
                                        target: "forge_session::server",
                                        session_id = %session_id_for_turn,
                                        error = %e,
                                        "usage flush failed at session end",
                                    );
                                }
                            }
                        });
                    }

                    Some(IpcMessage::ToolCallApproved(a)) => {
                        // F-053 / F-074: parse the client-supplied scope (wire
                        // format is still a bare string, per
                        // `forge_ipc::ToolCallApproved`) into the typed
                        // `ApprovalScope`. On parse failure we now reject the
                        // approval rather than silently downgrading to `Once`:
                        // a user who granted "Always" must not have it
                        // demoted without notice (audit M7-class finding).
                        // The client receives the rejection via the same
                        // approval channel it would for an explicit deny,
                        // and downstream `ToolCallRejected` event surfaces
                        // the outcome to the session log.
                        let parsed = serde_json::from_value::<ApprovalScope>(
                            serde_json::Value::String(a.scope.clone()),
                        );
                        let mut map = pending_approvals.lock().await;
                        if let Some(tx) = map.remove(&a.id) {
                            let decision = match parsed {
                                Ok(scope) => ApprovalDecision::Approved(scope),
                                Err(_) => {
                                    tracing::warn!(
                                        target: "forge_session::server",
                                        session_id = %session_id,
                                        scope = ?a.scope,
                                        "ToolCallApproved: unknown scope, rejecting approval",
                                    );
                                    ApprovalDecision::Rejected
                                }
                            };
                            let _ = tx.send(decision);
                        }
                    }

                    Some(IpcMessage::ToolCallRejected(r)) => {
                        let mut map = pending_approvals.lock().await;
                        if let Some(tx) = map.remove(&r.id) {
                            let _ = tx.send(ApprovalDecision::Rejected);
                        }
                    }

                    Some(IpcMessage::RerunMessage(r)) => {
                        // F-143 / F-144: dispatch rerun through the
                        // `Orchestrator`. All three variants (Replace,
                        // Branch, Fresh) are wired. Spawn off the event
                        // loop so concurrent tool approvals still flow
                        // through the same connection while regeneration
                        // streams.
                        let session = Arc::clone(&session);
                        let provider = Arc::clone(&provider);
                        let approvals = Arc::clone(&pending_approvals);
                        let workspace_path = workspace_path.clone();
                        let allowed_paths = Arc::clone(&allowed_paths);
                        let child_registry = child_registry.clone();
                        let byte_budget = Arc::clone(&byte_budget);
                        let agent_runtime = (*agent_runtime).clone();
                        // F-587: rerun is a turn — clone the per-session
                        // credential binding into the rerun task, identical
                        // to the `SendUserMessage` branch above.
                        let credential_ctx_for_rerun = credentials.clone();
                        // MessageId wraps an `Arc<str>` so any string is
                        // structurally a valid id (the log lookup later
                        // surfaces "not found" if the client fabricated
                        // one). No pre-validation here.
                        let target = MessageId::from_string(r.msg_id.clone());
                        let variant = r.variant;
                        let session_id_for_rerun = Arc::clone(&session_id);
                        tokio::spawn(async move {
                            let orch = Orchestrator::new();
                            if let Err(e) = orch
                                .rerun_message(
                                    session,
                                    provider,
                                    target,
                                    variant,
                                    approvals,
                                    (*allowed_paths).clone(),
                                    auto_approve,
                                    workspace_path,
                                    Some(child_registry),
                                    Some(byte_budget),
                                    agent_runtime,
                                    credential_ctx_for_rerun,
                                )
                                .await
                            {
                                tracing::warn!(
                                    target: "forge_session::server",
                                    session_id = %session_id_for_rerun,
                                    error = %e,
                                    "rerun error",
                                );
                            }
                        });
                    }

                    Some(IpcMessage::SelectBranch(s)) => {
                        // F-144: activate a specific branch variant. Spawned
                        // off the event loop like rerun so the connection
                        // stays responsive if the log scan is slow on a
                        // large session.
                        let session = Arc::clone(&session);
                        let parent = MessageId::from_string(s.parent_id.clone());
                        let variant_index = s.variant_index;
                        let session_id_for_select = Arc::clone(&session_id);
                        tokio::spawn(async move {
                            let orch = Orchestrator::new();
                            if let Err(e) =
                                orch.select_branch(session, parent, variant_index).await
                            {
                                tracing::warn!(
                                    target: "forge_session::server",
                                    session_id = %session_id_for_select,
                                    error = %e,
                                    "select_branch error",
                                );
                            }
                        });
                    }

                    Some(IpcMessage::DeleteBranch(d)) => {
                        // F-145: tombstone a branch variant. Same spawn pattern
                        // as select_branch — daemon resolves and emits
                        // `BranchDeleted` asynchronously; the client observes
                        // the effect through the event stream.
                        let session = Arc::clone(&session);
                        let parent = MessageId::from_string(d.parent_id.clone());
                        let variant_index = d.variant_index;
                        let session_id_for_delete = Arc::clone(&session_id);
                        tokio::spawn(async move {
                            let orch = Orchestrator::new();
                            if let Err(e) =
                                orch.delete_branch(session, parent, variant_index).await
                            {
                                tracing::warn!(
                                    target: "forge_session::server",
                                    session_id = %session_id_for_delete,
                                    error = %e,
                                    "delete_branch error",
                                );
                            }
                        });
                    }

                    Some(IpcMessage::CompactTranscript(_)) => {
                        // F-598: manual transcript compaction. Spawned off
                        // the event loop like the rerun/branch commands so
                        // a slow summary call does not block other client
                        // frames. The compaction emits ContextCompacted
                        // through the event stream — there's no direct
                        // response frame.
                        //
                        // Provider id + model stamped onto the summary
                        // message match the synthetic pair `run_request_loop`
                        // uses for live turns (`ProviderId::new()` /
                        // "mock"). When real providers thread their own
                        // ids through the orchestrator, this site moves
                        // with them.
                        let session = Arc::clone(&session);
                        let provider = Arc::clone(&provider);
                        let session_id_for_compact = Arc::clone(&session_id);
                        tokio::spawn(async move {
                            let pinned = std::collections::HashSet::new();
                            if let Err(e) = crate::compaction::compact(
                                session,
                                provider,
                                forge_core::ids::ProviderId::new(),
                                "mock".to_string(),
                                crate::compaction::DEFAULT_COMPACT_FRACTION,
                                &pinned,
                                forge_core::CompactTrigger::UserRequested,
                            )
                            .await
                            {
                                tracing::warn!(
                                    target: "forge_session::server",
                                    session_id = %session_id_for_compact,
                                    error = %e,
                                    "compact_transcript error",
                                );
                            }
                        });
                    }

                    Some(IpcMessage::ListMcpServers(_)) => {
                        // F-155: reply with the daemon's authoritative
                        // snapshot. Empty list when no manager is loaded
                        // (no `.mcp.json`) — distinguishable from an error
                        // because the frame shape succeeds.
                        let servers = match mcp.as_ref() {
                            Some(mgr) => mgr.list().await,
                            None => Vec::new(),
                        };
                        let frame = IpcMessage::McpServersList(
                            forge_ipc::McpServersList { servers },
                        );
                        if let Err(e) = forge_ipc::write_frame(&mut writer, &frame).await {
                            tracing::error!(
                                target: "forge_session::mcp",
                                session_id = %session_id,
                                error = %e,
                                "McpServersList write failed",
                            );
                            break;
                        }
                    }

                    Some(IpcMessage::ToggleMcpServer(t)) => {
                        // F-155: run the toggle against the daemon's single
                        // authoritative manager so running tool calls feel
                        // the effect. The client sends the *target* state
                        // (`enabled: true | false`); the daemon maps that
                        // onto `enable`/`disable`. `McpStateEvent`s emitted
                        // by the lifecycle transition flow separately through
                        // the session event log (via the forwarder installed
                        // at daemon start).
                        let (enabled_after, error) = match mcp.as_ref() {
                            Some(mgr) => {
                                let res = if t.enabled {
                                    mgr.enable(&t.name).await
                                } else {
                                    mgr.disable(&t.name).await
                                };
                                match res {
                                    Ok(()) => (t.enabled, None),
                                    // Report pre-toggle state on error —
                                    // look it up from the current list so
                                    // the UI can reconcile without a
                                    // follow-up `ListMcpServers`. Unknown
                                    // name short-circuits with `false`.
                                    Err(e) => {
                                        let msg = format!("{e:#}");
                                        let current = mgr.list().await.into_iter()
                                            .find(|s| s.name == t.name)
                                            .map(|s| !matches!(
                                                s.state,
                                                forge_mcp::ServerState::Disabled { .. }
                                                | forge_mcp::ServerState::Failed { .. }
                                            ))
                                            .unwrap_or(false);
                                        (current, Some(msg))
                                    }
                                }
                            }
                            None => (
                                false,
                                Some(
                                    "daemon has no MCP manager (no .mcp.json loaded)"
                                        .to_string(),
                                ),
                            ),
                        };
                        let frame = IpcMessage::McpToggleResult(
                            forge_ipc::McpToggleResult {
                                name: t.name,
                                enabled_after,
                                error,
                            },
                        );
                        if let Err(e) = forge_ipc::write_frame(&mut writer, &frame).await {
                            tracing::error!(
                                target: "forge_session::mcp",
                                session_id = %session_id,
                                error = %e,
                                "McpToggleResult write failed",
                            );
                            break;
                        }
                    }

                    Some(IpcMessage::ImportMcpConfig(imp)) => {
                        // F-155: delegate the import to `forge_mcp::import`
                        // and, on `apply: true`, rewrite
                        // `<workspace>/.mcp.json` atomically. A dry-run
                        // (`apply: false`) returns the would-be-imported
                        // names without touching the file — the webview
                        // confirms the diff before applying.
                        let frame = handle_import_mcp_config(
                            &imp,
                            workspace_path.as_deref(),
                        )
                        .await;
                        if let Err(e) = forge_ipc::write_frame(&mut writer, &frame).await {
                            tracing::error!(
                                target: "forge_session::mcp",
                                session_id = %session_id,
                                error = %e,
                                "McpImportResult write failed",
                            );
                            break;
                        }
                    }

                    // F-074: exhaustive match over the remaining
                    // `IpcMessage` variants — adding a new variant must be a
                    // compile error here so an IPC handler is added
                    // deliberately rather than silently swallowed at the
                    // trust boundary. `Hello` / `HelloAck` are handshake
                    // frames already consumed before the read loop;
                    // `Subscribe` is reserved for future multi-subscriber
                    // sessions; `Event` is server→client only and would
                    // indicate a malformed/forged client frame. F-155
                    // daemon→client response variants (`McpServersList`,
                    // `McpToggleResult`, `McpImportResult`) are never
                    // received from a client either.
                    Some(other @ (IpcMessage::Hello(_)
                        | IpcMessage::HelloAck(_)
                        | IpcMessage::Subscribe(_)
                        | IpcMessage::Event(_)
                        | IpcMessage::McpServersList(_)
                        | IpcMessage::McpToggleResult(_)
                        | IpcMessage::McpImportResult(_))) => {
                        tracing::warn!(
                            target: "forge_session::server",
                            session_id = %session_id,
                            frame = ipc_message_kind(&other),
                            "ignoring unexpected client frame",
                        );
                    }
                    None => break,
                }
            }
        }
    }

    // In ephemeral mode the daemon exits after this connection, so we kill
    // any still-live sandboxed process groups here. For long-running
    // sessions, per-connection cleanup is handled by `SandboxedChild::drop`
    // (which both killpg's and deregisters from the shared registry);
    // the final `kill_all` runs when the daemon itself shuts down.
    if ephemeral {
        child_registry.kill_all();

        if let Some(session_dir) = session.log_path.parent() {
            if let Err(e) =
                archive_or_purge(session_dir, SessionPersistence::Ephemeral, &socket_path).await
            {
                tracing::error!(
                    target: "forge_session::server",
                    session_id = %session_id,
                    error = %e,
                    "archive_or_purge failed",
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_log_path_uses_workspace_when_set() {
        let ws = Path::new("/tmp/ws");
        let p = event_log_path("abc123", Some(ws));
        assert_eq!(p, Path::new("/tmp/ws/.forge/sessions/abc123/events.jsonl"));
    }

    #[test]
    fn event_log_path_falls_back_to_temp_when_unset() {
        let p = event_log_path("abc123", None);
        let expected = std::env::temp_dir()
            .join("forge-session-abc123")
            .join("events.jsonl");
        assert_eq!(p, expected);
    }

    #[test]
    fn compute_allowed_paths_returns_empty_when_workspace_absent() {
        assert!(compute_allowed_paths(None).is_empty());
    }

    #[test]
    fn compute_allowed_paths_returns_workspace_glob_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let expected = format!("{}/**", canonical.display());
        assert_eq!(compute_allowed_paths(Some(dir.path())), vec![expected]);
    }

    #[test]
    fn compute_allowed_paths_returns_empty_when_workspace_does_not_exist() {
        // Fail-closed: a non-existent path can't be canonicalized, so the
        // session rejects every `fs.*` call rather than guessing at semantics.
        let missing = Path::new("/nonexistent/forge-f043-test-path");
        assert!(compute_allowed_paths(Some(missing)).is_empty());
    }

    // F-043 regression: derived `allowed_paths` must not permit reads outside
    // the session workspace. These tests exercise the *composition* of
    // `compute_allowed_paths` with `forge_fs::read_file` — a helper returning
    // `vec![]` is only safe if the enforcement layer it feeds actually denies
    // on empty. Likewise a workspace of `/tmp/ws` must not match `/etc/passwd`.
    //
    // `forge_fs::read_file` returns `Result<_, FsError>` (F-061); we assert
    // on the `Display` rendering of the typed `NotAllowed` variant, which
    // still formats as "…not allowed by allowed_paths".
    #[test]
    fn fs_read_etc_passwd_denied_when_no_workspace() {
        let allowed = compute_allowed_paths(None);
        let err = forge_fs::read_file("/etc/passwd", &allowed, &forge_fs::Limits::default())
            .expect_err("reading /etc/passwd without a workspace must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not allowed by allowed_paths"),
            "expected path-denied error, got: {msg}"
        );
    }

    #[test]
    fn fs_read_etc_passwd_denied_when_workspace_is_tmp_ws() {
        // Use a real temp directory so canonicalization succeeds and we get a
        // non-empty allow-list. `/etc/passwd` must still be outside it.
        let dir = tempfile::tempdir().unwrap();
        let allowed = compute_allowed_paths(Some(dir.path()));
        assert!(
            !allowed.is_empty(),
            "workspace present → allow-list should not be empty"
        );
        let err = forge_fs::read_file("/etc/passwd", &allowed, &forge_fs::Limits::default())
            .expect_err("reading /etc/passwd with a scoped workspace must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not allowed by allowed_paths"),
            "expected path-denied error, got: {msg}"
        );
    }

    /// F-601 regression: two concurrent sessions in the same process can
    /// target different active agents (typed param) and each must receive
    /// its own memory body.
    ///
    /// This is the test that would have caught the original
    /// `FORGE_ACTIVE_AGENT` env-var bug: a single mutable process-global
    /// captured at session start meant a second connection (or a second
    /// `serve_with_session` invocation in the same process) silently
    /// observed the first active agent's memory. With the typed
    /// parameter, no shared state — calling `resolve_session_memory_with`
    /// concurrently with two distinct `active_agent` strings keyed
    /// against the same store must surface the per-agent body each time.
    #[tokio::test]
    async fn concurrent_sessions_with_different_active_agents_get_their_own_memory() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(forge_agents::MemoryStore::new(dir.path()));

        // Seed two distinct memory bodies for two distinct agent ids.
        store
            .write("alpha", "alpha body", forge_agents::WriteMode::Append)
            .unwrap();
        store
            .write("beta", "beta body", forge_agents::WriteMode::Append)
            .unwrap();

        // Resolve concurrently — the typed parameter is the only thing
        // that distinguishes the two calls. With the prior env-var
        // implementation, both calls would have observed whichever value
        // `std::env::var("FORGE_ACTIVE_AGENT")` happened to return.
        let (a_result, b_result) = tokio::join!(
            resolve_session_memory_with("alpha", store.clone()),
            resolve_session_memory_with("beta", store.clone()),
        );

        let (a_body, a_binding) = a_result;
        let (b_body, b_binding) = b_result;

        assert_eq!(a_body.as_deref(), Some("alpha body"));
        assert_eq!(b_body.as_deref(), Some("beta body"));
        assert_eq!(
            a_binding.expect("alpha binding").agent_id,
            "alpha",
            "binding agent_id must reflect the typed parameter, not shared state"
        );
        assert_eq!(
            b_binding.expect("beta binding").agent_id,
            "beta",
            "binding agent_id must reflect the typed parameter, not shared state"
        );
    }

    // -------------------------------------------------------------------
    // F-602: settings override the def-level memory_enabled flag
    // -------------------------------------------------------------------

    #[test]
    fn effective_memory_enabled_falls_through_to_def_when_override_absent() {
        assert!(effective_memory_enabled(true, None));
        assert!(!effective_memory_enabled(false, None));
    }

    #[test]
    fn effective_memory_enabled_settings_true_overrides_def_false() {
        // F-602: opting INTO memory via settings even when the def did not
        // declare `memory: true` in frontmatter.
        assert!(effective_memory_enabled(false, Some(true)));
    }

    #[test]
    fn effective_memory_enabled_settings_false_overrides_def_true() {
        // F-602: opting OUT of memory via settings even when the def has
        // `memory: true`. The user toggle must be able to disable a
        // memory-on-by-default agent.
        assert!(!effective_memory_enabled(true, Some(false)));
    }

    #[tokio::test]
    async fn load_memory_setting_override_returns_none_when_workspace_missing() {
        let result = load_memory_setting_override(None, Some("scribe")).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn load_memory_setting_override_returns_none_when_active_agent_blank() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_memory_setting_override(Some(dir.path()), None)
            .await
            .is_none());
        assert!(load_memory_setting_override(Some(dir.path()), Some(""))
            .await
            .is_none());
        assert!(load_memory_setting_override(Some(dir.path()), Some("   "))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn load_memory_setting_override_returns_none_when_no_settings_file() {
        // No `.forge/settings.toml` — should fall through to None so the
        // session uses the def-level flag.
        let dir = tempfile::tempdir().unwrap();
        let result = load_memory_setting_override(Some(dir.path()), Some("scribe")).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn load_memory_setting_override_reads_workspace_setting() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join(".forge"))
            .await
            .unwrap();
        tokio::fs::write(
            dir.path().join(".forge").join("settings.toml"),
            "[memory.enabled]\nscribe = true\n",
        )
        .await
        .unwrap();
        let result = load_memory_setting_override(Some(dir.path()), Some("scribe")).await;
        assert_eq!(result, Some(true));
    }

    #[tokio::test]
    async fn load_memory_setting_override_distinguishes_per_agent() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join(".forge"))
            .await
            .unwrap();
        tokio::fs::write(
            dir.path().join(".forge").join("settings.toml"),
            "[memory.enabled]\nscribe = true\nresearcher = false\n",
        )
        .await
        .unwrap();
        assert_eq!(
            load_memory_setting_override(Some(dir.path()), Some("scribe")).await,
            Some(true)
        );
        assert_eq!(
            load_memory_setting_override(Some(dir.path()), Some("researcher")).await,
            Some(false)
        );
        assert_eq!(
            load_memory_setting_override(Some(dir.path()), Some("unknown")).await,
            None,
            "unknown agents fall through to the def-level flag"
        );
    }
}
