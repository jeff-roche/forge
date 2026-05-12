use anyhow::Result;
use forge_core::Event;
use forge_providers::MockProvider;
use forge_session::{
    log_bridge,
    pid_file::OwnedPidFile,
    provider_spec::{parse_provider_spec, ProviderKind},
    server::{event_log_path, serve_with_session},
    session::Session,
    socket_path::resolve_socket_path,
};
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let auto_approve = args.iter().any(|a| a == "--auto-approve-unsafe");
    let ephemeral = args.iter().any(|a| a == "--ephemeral");
    let provider_spec = parse_flag(&args, "--provider").or_else(|| {
        std::env::var("FORGE_PROVIDER")
            .ok()
            .filter(|s| !s.is_empty())
    });

    // Allow the CLI to pre-assign the session ID and socket path so it can
    // print the path before forged starts and can track it for `session kill`.
    let session_id = std::env::var("FORGE_SESSION_ID")
        .unwrap_or_else(|_| forge_core::SessionId::new().to_string());
    // F-044 (H8): `resolve_socket_path` now refuses to return a path when
    // `XDG_RUNTIME_DIR` is unset rather than falling back to `/tmp/forge-0`.
    // Tests always set `FORGE_SOCKET_PATH` explicitly, so this resolver runs
    // only on the production path where systemd provides `XDG_RUNTIME_DIR`.
    let socket_path = match std::env::var("FORGE_SOCKET_PATH") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => resolve_socket_path(&session_id)?,
    };
    // Normalize FORGE_WORKSPACE to an absolute path so HelloAck.workspace is
    // portable for clients (which may have a different CWD than the daemon).
    // std::path::absolute does not require the path to exist, unlike canonicalize.
    let workspace = std::env::var("FORGE_WORKSPACE")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .map(|p| std::path::absolute(&p).unwrap_or(p));
    // F-371: daemon startup banners stay on stderr rather than going through
    // `tracing::info!`. The `forged` binary intentionally installs no
    // subscriber (emission-only crate per the scope contract), so the only
    // way these lines reach an operator today is direct stderr. The
    // `eprintln_audit` integration test excludes `main.rs` for this reason;
    // see the comment on `is_bin_main` there.
    eprintln!("forged: listening on {}", socket_path.display());

    // F-049: persistent-mode forged owns the pid-file lifecycle. Created
    // with O_EXCL so a leftover file from a prior crash is not clobbered;
    // removed on drop (SIGTERM, SIGINT, or any exit path) so stale pid
    // files don't outlive the process. Ephemeral mode has no external
    // `session_kill` caller and so does not need a pid file.
    // Held in a binding that must outlive `serve_with_session`.
    let _pid_guard = if !ephemeral {
        std::env::var("FORGE_PID_FILE")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|p| OwnedPidFile::create(PathBuf::from(p)))
            .transpose()?
    } else {
        None
    };

    // Install the tracing → event-channel bridge before creating the
    // session. The bridge captures warn-or-error tracing events from
    // anywhere in this process and parks them on an mpsc channel; the
    // forwarder task spawned below drains it into `Session::emit` so
    // the shell sees them as `Event::LogLine` and routes them to the
    // webview console.
    let log_bridge_rx = log_bridge::install();

    let log_path = event_log_path(&session_id, workspace.as_deref());
    let session = Arc::new(Session::create(log_path).await?);

    if let Some(mut rx) = log_bridge_rx {
        let session_for_logs = Arc::clone(&session);
        tokio::spawn(async move {
            while let Some(record) = rx.recv().await {
                if let Err(err) = session_for_logs
                    .emit(Event::LogLine {
                        at: record.at,
                        level: record.level,
                        target: record.target,
                        message: record.message,
                    })
                    .await
                {
                    // Don't recurse through `tracing` — that would feed
                    // every emit error back through the bridge. Operator
                    // stderr is the only place a degraded bridge can
                    // report itself.
                    eprintln!("forged: log_bridge forwarder dropping record: {err}");
                    break;
                }
            }
        });
    }

    // F-601: the daemon binary still accepts `FORGE_ACTIVE_AGENT` as the
    // way an operator (or the Tauri shell launcher) names the agent this
    // daemon process is bound to. Read once here at startup and hand off
    // as a typed argument — `serve_with_session` no longer touches the
    // environment for this concern, which is the fix for the persistent-
    // mode multi-connection bug where two browser windows could share the
    // first window's captured env value.
    let active_agent = std::env::var("FORGE_ACTIVE_AGENT")
        .ok()
        .filter(|s| !s.trim().is_empty());

    // Provider selection (F-038):
    //   1. explicit `--provider <spec>` flag, OR `FORGE_PROVIDER` env, OR
    //   2. Mock when `FORGE_MOCK_SEQUENCE_FILE` is set, OR
    //   3. Mock from default path (legacy fallback for ad-hoc runs).
    // The Provider trait uses `impl Future` (not object-safe), so we cannot
    // box and dispatch — instead, match here and call `serve_with_session`
    // with the concrete provider type from each branch.
    match provider_spec {
        Some(spec) => match parse_provider_spec(&spec)? {
            ProviderKind::Mock => {
                let provider = build_mock_provider().await?;
                serve_with_session(
                    &socket_path,
                    session,
                    provider,
                    auto_approve,
                    ephemeral,
                    workspace,
                    Some(session_id),
                    // F-587: MockProvider is keyless; no credential pull.
                    None,
                    // F-601: typed active-agent — `None` here keeps memory off.
                    active_agent,
                )
                .await
            }
        },
        None => {
            let provider = build_mock_provider().await?;
            serve_with_session(
                &socket_path,
                session,
                provider,
                auto_approve,
                ephemeral,
                workspace,
                Some(session_id),
                // F-587: MockProvider is keyless.
                None,
                // F-601: typed active-agent — `None` here keeps memory off.
                active_agent,
            )
            .await
        }
    }
}

/// Parse `--flag value` from a flat argv. Returns None if the flag isn't
/// present or if it has no following value. Mirrors the shape of the other
/// argv-walks in this file rather than introducing clap mid-task.
fn parse_flag(args: &[String], flag: &str) -> Option<String> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1).cloned()
}

/// FORGE_MOCK_SEQUENCE_FILE points to a JSON array of NDJSON scripts; each
/// element is consumed in order. Falls back to `with_default_path()` when
/// no file is configured.
async fn build_mock_provider() -> Result<Arc<MockProvider>> {
    if let Ok(seq_file) = std::env::var("FORGE_MOCK_SEQUENCE_FILE") {
        let content = tokio::fs::read_to_string(&seq_file).await?;
        let scripts: Vec<String> = serde_json::from_str(&content)?;
        Ok(Arc::new(MockProvider::from_responses(scripts)?))
    } else {
        Ok(Arc::new(MockProvider::with_default_path()))
    }
}
