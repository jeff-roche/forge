//! Shared session-spawn helper used by the CLI's `forge session new` path
//! (`main.rs`) and the Tauri `session_start` IPC (F-725).
//!
//! The function below is the only place that knows how to:
//! 1. allocate a fresh [`forge_core::SessionId`],
//! 2. compose the socket + pid paths,
//! 3. locate the `forged` binary,
//! 4. exec it with the right env vars and `--agent` / `--provider` flags,
//! 5. detach the child, and
//! 6. wait for the socket to appear before returning.
//!
//! Both callers pass owned `String` / `Option<String>` values — the helper
//! has no opinions on how the caller acquired them (clap, Tauri arg
//! deserialization, etc).

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::socket;

/// Result of a successful spawn: the freshly-allocated session id and the
/// UDS path the daemon is now listening on.
#[derive(Debug, Clone)]
pub struct SpawnedSession {
    pub session_id: String,
    pub socket_path: PathBuf,
}

/// Spawn a `forged` session and return its handle once the UDS appears.
///
/// `workspace` is the absolute directory `forged` will operate under. Pass
/// the current working directory when the caller has none.
///
/// `agent` defaults to [`forge_agents::FORGE_DEFAULT_AGENT_NAME`] when `None`.
/// `provider` is forwarded verbatim to `forged` via `--provider <spec>` when
/// present and otherwise omitted so the daemon picks its own default (env
/// var, then mock).
pub async fn spawn_forged_session(
    workspace: &Path,
    agent: Option<&str>,
    provider: Option<&str>,
) -> Result<SpawnedSession> {
    spawn_forged_session_with_id(workspace, agent, provider, None).await
}

/// F-748: spawn a `forged` daemon for a caller-supplied session id, or
/// allocate a fresh one when `existing_id` is `None`. Used by the
/// `session_restart` IPC to re-spawn a daemon for the SAME session id
/// after a crash so the existing event log on disk is reused via
/// `forge_session::session::Session::resume` (the daemon's main loop
/// picks the resume branch when the log file already exists).
///
/// Stale pid / socket artifacts under the resolved paths (the prior
/// daemon's leftovers) are best-effort unlinked before the spawn so the
/// new daemon's `OwnedPidFile::create` (O_EXCL) does not collide with a
/// pid file whose owner is already dead. The session window's `session_close`
/// path normally cleans these up, but a SIGKILL'd daemon never ran its
/// archive arm — F-748's restart is the secondary cleanup point.
pub async fn spawn_forged_session_with_id(
    workspace: &Path,
    agent: Option<&str>,
    provider: Option<&str>,
    existing_id: Option<&str>,
) -> Result<SpawnedSession> {
    let session_id = match existing_id {
        Some(id) => id.to_string(),
        None => forge_core::SessionId::new().to_string(),
    };
    let sock = socket::socket_path(&session_id)?;
    let pid_file = socket::pid_path(&session_id)?;

    // F-748: clear stale artifacts when reusing an id. A graceful shutdown
    // would have removed both via the daemon's drop guards; a crashed /
    // SIGKILL'd daemon leaves them behind. Best-effort — missing files
    // are the normal case for first-spawn (existing_id == None).
    if existing_id.is_some() {
        let _ = tokio::fs::remove_file(&sock).await;
        let _ = tokio::fs::remove_file(&pid_file).await;
    }

    if let Some(parent) = sock.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let forged = find_forged_binary()?;
    let mut cmd = std::process::Command::new(&forged);
    cmd.env("FORGE_SESSION_ID", &session_id)
        .env("FORGE_SOCKET_PATH", sock.to_str().unwrap_or(""))
        .env("FORGE_WORKSPACE", workspace.to_str().unwrap_or(""))
        .env("FORGE_PID_FILE", pid_file.to_str().unwrap_or(""));

    let agent_name = agent.unwrap_or(forge_agents::FORGE_DEFAULT_AGENT_NAME);
    cmd.arg("--agent").arg(agent_name);
    // The daemon currently reads the active agent from the environment;
    // forward it here so per-agent memory injection actually fires when
    // the caller selected an agent (the `--agent` CLI flag is parsed for
    // future compatibility but not yet consumed by `forged`).
    cmd.env("FORGE_ACTIVE_AGENT", agent_name);
    if let Some(spec) = provider {
        cmd.arg("--provider").arg(spec);
    }

    let child = cmd.spawn()?;
    // `forged` runs detached; explicitly leak the handle so dropping does
    // not kill it. Mirrors the CLI's existing behaviour in `main.rs`.
    std::mem::forget(child);

    wait_for_socket(&sock).await?;

    Ok(SpawnedSession {
        session_id,
        socket_path: sock,
    })
}

/// Locate the `forged` binary alongside the calling executable, falling
/// back to `PATH`. Identical policy to the CLI's pre-extraction helper.
///
/// Honours the `FORGE_FORGED_BIN` env var as a test-only override (when
/// `current_exe` is a test harness binary in `target/debug/deps/`, the
/// sibling lookup misses; integration tests set this var to the absolute
/// path of the `forged` binary they pre-built).
fn find_forged_binary() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("FORGE_FORGED_BIN") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("forged");
            if candidate.exists() {
                return Ok(candidate);
            }
            // F-748: tests live in `target/<profile>/deps/<test>-<hash>`
            // while `forged` sits at `target/<profile>/forged`. Try one
            // level up so the integration test harness can find it
            // without setting `FORGE_FORGED_BIN`.
            if let Some(parent) = dir.parent() {
                let up = parent.join("forged");
                if up.exists() {
                    return Ok(up);
                }
            }
        }
    }
    Ok(PathBuf::from("forged"))
}

/// Poll for the UDS to appear (50 ms × 100 = 5 s) before declaring the
/// spawn timed out. Matches the historical CLI behaviour.
async fn wait_for_socket(path: &Path) -> Result<()> {
    for _ in 0..100 {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("timed out waiting for socket at {}", path.display())
}
