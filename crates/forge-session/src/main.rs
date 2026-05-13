use anyhow::Result;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use forge_core::credentials::KeyringStore;
use forge_core::credentials::{Credentials, EnvFallbackStore, LayeredStore};
use forge_core::Event;
use forge_providers::anthropic::{AnthropicProvider, DEFAULT_MAX_TOKENS};
use forge_providers::ollama::OllamaProvider;
use forge_providers::openai::OpenAiProvider;
use forge_providers::MockProvider;
use forge_session::orchestrator::{CredentialContext, ProviderTag};
use forge_session::{
    log_bridge,
    pid_file::OwnedPidFile,
    provider_spec::{resolve_provider_kind, ProviderKind},
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
    let provider_spec = parse_flag(&args, "--provider")
        .or_else(|| {
            std::env::var("FORGE_PROVIDER")
                .ok()
                .filter(|s| !s.is_empty())
        })
        // F-743: integration tests across this crate spawn `forged` with
        // `FORGE_MOCK_SEQUENCE_FILE` pointing at a scripted NDJSON and
        // previously relied on the implicit Mock fallback. That env var is
        // never set outside test contexts, so treating its presence as an
        // explicit `mock` opt-in preserves the existing test surface
        // without re-introducing a production-path Mock fallback. (The
        // sequence file is still consumed by `build_mock_provider` below.)
        .or_else(|| {
            std::env::var("FORGE_MOCK_SEQUENCE_FILE")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|_| "mock".to_string())
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
    // F-748: a daemon spawned for an existing `session_id` (crash-restart
    // re-spawn) must NOT truncate the durable event log. `Session::resume`
    // preserves the file and seeds `seq` from the persisted event count
    // when the log already exists; otherwise it falls through to a fresh
    // `EventLog::create`. First-spawn callers go through the create branch
    // unchanged.
    let session = Arc::new(Session::resume(log_path).await?);

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

    // Test-isolation seam for the user-scope `~/.mcp.json` loader (mirrors
    // the F-743 shell-side `resolve_user_home_dir` test override). Spawned
    // `forged` subprocesses in integration tests set `FORGE_USER_HOME_FOR_TEST`
    // to a tempdir so `load_mcp_manager` does not see the developer's real
    // `~/.mcp.json` (CI passes because `$HOME` is clean; local runs leak).
    //
    // Gated on `debug_assertions`: disabled in the default release profile;
    // a profiling build with `[profile.release] debug-assertions = true`
    // would still honour it. The `FOR_TEST` naming convention is the real
    // safety contract — never set this env var in production.
    let user_home_override = if cfg!(debug_assertions) {
        std::env::var("FORGE_USER_HOME_FOR_TEST")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    } else {
        None
    };

    // Provider selection (F-038, F-743):
    //   1. explicit `--provider <spec>` flag, OR `FORGE_PROVIDER` env.
    //
    // F-743: there is no production fallback to `MockProvider` — if neither
    // flag nor env is set, the resolver returns
    // `provider_spec_required: ...` and the daemon refuses to start. Tests
    // that need Mock pass `--provider mock` or `FORGE_PROVIDER=mock`
    // explicitly.
    //
    // The Provider trait uses `impl Future` (not object-safe), so we
    // cannot box and dispatch — instead, match here and call
    // `serve_with_session` with the concrete provider type from each
    // branch.
    match resolve_provider_kind(provider_spec.as_deref())? {
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
                // Test isolation: `Some(tempdir)` when
                // `FORGE_USER_HOME_FOR_TEST` is set in a build with
                // `debug_assertions = true`; `None` otherwise, in which case
                // `load_mcp_manager` falls back to `dirs::home_dir()`.
                user_home_override,
                // F-752: MockProvider keeps the legacy `"mock"` tag — the
                // synthetic provider has no real model identifier and the
                // existing test fixtures pin against this value.
                Some(ProviderTag::new("mock", "mock")),
            )
            .await
        }
        ProviderKind::Ollama { base_url, model } => {
            // F-752: capture the resolved model string before the provider
            // constructor consumes it so the tag carries the same value the
            // provider was built with.
            let provider_tag = Some(ProviderTag::new("ollama", model.clone()));
            let provider = Arc::new(OllamaProvider::new(base_url, model));
            serve_with_session(
                &socket_path,
                session,
                provider,
                auto_approve,
                ephemeral,
                workspace,
                Some(session_id),
                // F-743: Ollama is keyless; no credential pull.
                None,
                active_agent,
                user_home_override,
                provider_tag,
            )
            .await
        }
        // F-745: Anthropic direct API. Constructor `api_key` is the empty
        // string — the F-744 seam injects the real key per-turn from the
        // credential store. The orchestrator pulls under `provider_id =
        // "anthropic"`; with no keyring entry the layered store falls through
        // to `ANTHROPIC_API_KEY` env. F-746 will add early-fail credential
        // validation; until then a missing key surfaces as an upstream 401
        // mapped to `ChatChunk::Error` by the provider.
        ProviderKind::Anthropic { base_url, model } => {
            let provider_tag = Some(ProviderTag::new("anthropic", model.clone()));
            let provider = Arc::new(AnthropicProvider::new(
                base_url,
                String::new(),
                model,
                DEFAULT_MAX_TOKENS,
            ));
            let credentials = build_credential_context("anthropic");
            serve_with_session(
                &socket_path,
                session,
                provider,
                auto_approve,
                ephemeral,
                workspace,
                Some(session_id),
                credentials,
                active_agent,
                user_home_override,
                provider_tag,
            )
            .await
        }
        // F-745: OpenAI direct API. Same constructor-vs-seam posture as
        // Anthropic; provider id is `"openai"`, env fallback is
        // `OPENAI_API_KEY`.
        ProviderKind::OpenAi { base_url, model } => {
            let provider_tag = Some(ProviderTag::new("openai", model.clone()));
            let provider = Arc::new(OpenAiProvider::new(base_url, String::new(), model));
            let credentials = build_credential_context("openai");
            serve_with_session(
                &socket_path,
                session,
                provider,
                auto_approve,
                ephemeral,
                workspace,
                Some(session_id),
                credentials,
                active_agent,
                user_home_override,
                provider_tag,
            )
            .await
        }
    }
}

/// F-745: build a [`CredentialContext`] for a keyed provider.
///
/// Production wiring per the credentials module docs:
/// `LayeredStore::new(KeyringStore, EnvFallbackStore::default())` — keyring
/// primary, env fallback. On targets without a platform keyring (none
/// today; the cfg gates all three desktop OSes), fall back to the
/// env-only store.
fn build_credential_context(provider_id: &'static str) -> Option<CredentialContext> {
    // `EnvFallbackStore::default()` reads `ANTHROPIC_API_KEY` for the
    // `anthropic` provider and `OPENAI_API_KEY` for `openai` when the
    // keyring has no entry — the canonical vendor env vars.
    let store: Arc<dyn Credentials> = {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            Arc::new(LayeredStore::new(
                KeyringStore::new(),
                EnvFallbackStore::default(),
            ))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            Arc::new(EnvFallbackStore::default())
        }
    };
    Some(CredentialContext {
        store,
        provider_id: provider_id.to_string(),
        sidecar_push: None,
    })
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
