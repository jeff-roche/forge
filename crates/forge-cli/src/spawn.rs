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
/// `agent` defaults to `orchestrator` when `None`. `provider` is forwarded
/// verbatim to `forged` via `--provider <spec>` when present and otherwise
/// omitted so the daemon picks its own default (env var, then mock).
pub async fn spawn_forged_session(
    workspace: &Path,
    agent: Option<&str>,
    provider: Option<&str>,
) -> Result<SpawnedSession> {
    let session_id = forge_core::SessionId::new();
    let sock = socket::socket_path(&session_id.to_string())?;
    let pid_file = socket::pid_path(&session_id.to_string())?;

    if let Some(parent) = sock.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let forged = find_forged_binary()?;
    let mut cmd = std::process::Command::new(&forged);
    cmd.env("FORGE_SESSION_ID", session_id.to_string())
        .env("FORGE_SOCKET_PATH", sock.to_str().unwrap_or(""))
        .env("FORGE_WORKSPACE", workspace.to_str().unwrap_or(""))
        .env("FORGE_PID_FILE", pid_file.to_str().unwrap_or(""));

    let agent_name = agent.unwrap_or("orchestrator");
    cmd.arg("--agent").arg(agent_name);
    if let Some(spec) = provider {
        cmd.arg("--provider").arg(spec);
    }

    let child = cmd.spawn()?;
    // `forged` runs detached; explicitly leak the handle so dropping does
    // not kill it. Mirrors the CLI's existing behaviour in `main.rs`.
    std::mem::forget(child);

    wait_for_socket(&sock).await?;

    Ok(SpawnedSession {
        session_id: session_id.to_string(),
        socket_path: sock,
    })
}

/// Locate the `forged` binary alongside the calling executable, falling
/// back to `PATH`. Identical policy to the CLI's pre-extraction helper.
fn find_forged_binary() -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("forged");
            if candidate.exists() {
                return Ok(candidate);
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
