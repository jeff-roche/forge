//! Runtime adapter that materializes a [`WindowSpec`] on a live Tauri app.
//!
//! Not unit-tested — exercising this path requires a live webview runtime
//! (`WebviewWindowBuilder::build()` talks to wry/WebKitGTK). CI
//! compile-verifies via `cargo check --features webview`; the manual smoke
//! test is `cargo run -p forge-shell`.

use anyhow::{Context, Result};
use tauri::{
    image::Image, AppHandle, Manager, RunEvent, Runtime, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};

use crate::window_spec::WindowSpec;

/// Raw bytes of the canonical forge mark, embedded at compile time so the
/// dev binary doesn't need the bundle's icon directory present at runtime.
const FORGE_ICON_PNG: &[u8] = include_bytes!("../icons/icon.png");

/// Opens and manages Forge windows on a live `AppHandle`.
pub struct WindowManager<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> WindowManager<R> {
    pub fn new(app: AppHandle<R>) -> Self {
        Self { app }
    }

    /// Opens the Dashboard window, or focuses it if already open.
    pub fn open_dashboard(&self) -> Result<WebviewWindow<R>> {
        self.open(WindowSpec::dashboard())
    }

    /// Opens a blank Session window for `id`.
    ///
    /// Scaffold only — F-024 wires the session route and content. No IPC to
    /// `forge-session` yet (F-020).
    pub fn open_session(&self, id: &str) -> Result<WebviewWindow<R>> {
        self.open(WindowSpec::session(id))
    }

    fn open(&self, spec: WindowSpec) -> Result<WebviewWindow<R>> {
        if let Some(existing) = self.app.get_webview_window(&spec.label) {
            existing
                .set_focus()
                .with_context(|| format!("focus existing window `{}`", spec.label))?;
            return Ok(existing);
        }

        let mut builder =
            WebviewWindowBuilder::new(&self.app, &spec.label, WebviewUrl::App(spec.url.into()))
                .title(&spec.title)
                .inner_size(spec.width, spec.height)
                .min_inner_size(spec.min_width, spec.min_height)
                .resizable(spec.resizable)
                .decorations(spec.decorations);
        if spec.center {
            builder = builder.center();
        }
        // `tauri.conf.json`'s `bundle.icon` is only consumed by the bundler.
        // In dev (`cargo tauri dev`) and any unbundled runtime, the window
        // chrome / taskbar / dock fall back to the platform default unless
        // the icon is set explicitly on the builder.
        let icon =
            Image::from_bytes(FORGE_ICON_PNG).context("decode embedded forge window icon")?;
        builder = builder
            .icon(icon)
            .with_context(|| format!("set window icon for `{}`", spec.label))?;
        builder
            .build()
            .with_context(|| format!("build window `{}`", spec.label))
    }
}

/// Entry point invoked from `main`. Builds the Tauri app, registers the
/// session IPC bridge + dashboard commands, and opens the Dashboard on setup.
pub fn run() -> Result<()> {
    // Debug-only: install the tracing→log bridge so `tracing::*` calls in
    // this process flow to `tauri-plugin-log`'s Webview target and land in
    // the browser devtools console. The bridge subscriber must be set
    // **before** the plugin's `build()` (which registers the global logger)
    // so the first events the plugin would format aren't dropped.
    #[cfg(debug_assertions)]
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        // `try_init` (not `init`) so `cargo test` runs that already
        // installed a subscriber don't panic on a second registration.
        let _ = tracing_subscriber::registry()
            .with(crate::dev_log_bridge::LogBridge)
            .try_init();
    }

    let mut builder = tauri::Builder::default();

    // Debug-only: forward `log::*` records (now fed by the `LogBridge`
    // above) to the webview console + stdout. In release builds we keep
    // the historical "no logger" posture so production users don't see a
    // chatty stdout or a plugin-registered global logger they didn't ask
    // for.
    #[cfg(debug_assertions)]
    {
        use tauri_plugin_log::{Target, TargetKind};
        builder = builder.plugin(
            tauri_plugin_log::Builder::default()
                .level(log::LevelFilter::Debug)
                // Forge's own crates: keep at debug. Third-party noisy
                // crates can be filtered tighter as needed.
                .level_for("hyper", log::LevelFilter::Info)
                .level_for("h2", log::LevelFilter::Info)
                .level_for("rustls", log::LevelFilter::Info)
                .targets([
                    Target::new(TargetKind::Stdout),
                    Target::new(TargetKind::Webview),
                ])
                .build(),
        );
    }

    builder
        // F-138: OS-notification branch of `notifications.bg_agents`.
        // `.plugin(init())` registers `plugin:notification|*` commands that
        // the webview calls via `@tauri-apps/plugin-notification`. The
        // capability file grants the browser-facing permissions; see
        // `crates/forge-shell/capabilities/default.json`.
        .plugin(tauri_plugin_notification::init())
        // F-726 follow-up: native directory picker for the `+ New session`
        // modal's empty-workspace branch. The webview calls `open({ directory:
        // true, ... })` from `@tauri-apps/plugin-dialog`; the capability file
        // grants the matching `dialog:allow-open` permission.
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            crate::dashboard_sessions::session_list,
            crate::dashboard_sessions::open_session,
            crate::ipc::session_hello,
            crate::ipc::session_subscribe,
            crate::ipc::session_send_message,
            crate::ipc::session_approve_tool,
            crate::ipc::session_reject_tool,
            crate::ipc::get_persistent_approvals,
            crate::ipc::save_approval,
            crate::ipc::remove_approval,
            crate::ipc::terminal_spawn,
            crate::ipc::terminal_write,
            crate::ipc::terminal_resize,
            crate::ipc::terminal_kill,
            crate::ipc::read_file,
            crate::ipc::write_file,
            crate::ipc::tree,
            crate::ipc::lsp_start,
            crate::ipc::lsp_stop,
            crate::ipc::lsp_send,
            // F-359: server-side URL context fetch replaces the webview's
            // direct `fetch()` — the allowlist lives on the Rust side so a
            // compromised renderer cannot widen its reach.
            crate::ipc::context_fetch_url,
            crate::ipc::set_context_allowed_hosts,
            // F-137: background-agent lifecycle commands. Registered here
            // alongside F-138's `stop_background_agent` so the shell binary
            // exposes the full quartet in production. Tests reach the same
            // commands through `build_invoke_handler` in `ipc.rs`; until this
            // list is deduplicated against that helper, both call sites must
            // be updated in lockstep.
            crate::ipc::start_background_agent,
            crate::ipc::promote_background_agent,
            crate::ipc::list_background_agents,
            // F-138: stop completes the quartet.
            crate::ipc::stop_background_agent,
            // F-151: persistent settings store. Required by F-138's
            // `notifications.bg_agents` read; without it, `getSettings`
            // fails and the status-bar notification handler falls back to
            // the default `toast` mode.
            crate::ipc::get_settings,
            crate::ipc::set_setting,
            // F-132: live session-MCP commands. F-591 adds the roster-scoped
            // `list_mcp_servers` next door — distinct semantics: the session
            // command introspects the daemon's running manager, the roster
            // command reads on-disk `.mcp.json`.
            crate::ipc::session_list_mcp_servers,
            crate::ipc::toggle_mcp_server,
            crate::ipc::import_mcp_config,
            // F-591: roster discovery commands.
            crate::ipc::list_skills,
            crate::ipc::list_mcp_servers,
            crate::ipc::list_agents,
            crate::ipc::list_providers,
            // F-587: per-provider credential management. The Dashboard's
            // settings panel is the only call site; `require_window_label`
            // enforces the `dashboard` window label inside each command.
            crate::credentials_ipc::login_provider,
            crate::credentials_ipc::logout_provider,
            crate::credentials_ipc::has_credential,
            // F-586: provider selection — dashboard list / get-active / set-active.
            // `dashboard_list_providers` keeps the shorter `list_providers`
            // wire-name free for F-591's roster catalog command.
            crate::providers_ipc::dashboard_list_providers,
            crate::providers_ipc::get_active_provider,
            crate::providers_ipc::set_active_provider,
            // F-730: + Add provider modal on the Providers page. Writes the
            // built-in / custom_openai entry into user-tier settings.
            crate::providers_ipc::add_provider,
            // F-731: per-row Test connection chip on the Providers page.
            // Reads the configured base URL + stored credential and issues a
            // single low-cost probe against the provider's models endpoint
            // under a 5s wall-clock deadline.
            crate::providers_ipc::test_provider_connection,
            // F-731 (ad-hoc): drives the Add/Edit Provider modal's
            // "Test connection" affordance — accepts a transient
            // endpoint + key so the user can verify a target and
            // populate the model picker before saving.
            crate::providers_ipc::probe_provider_config,
            // F-732: Providers page edit / remove commands. `update_provider`
            // rewrites a `[providers.custom_openai.<name>]` section; built-in
            // ids are rejected. `remove_provider` drops the section (or the
            // built-in `providers.enabled.<id>` flag) and clears
            // `[providers.active]` when the removed id was the active one.
            crate::providers_ipc::update_provider,
            crate::providers_ipc::remove_provider,
            // F-733: per-row Enabled toggle on the Providers page. Flips
            // `providers.enabled.<id>` and clears `[providers.active]` when
            // the disabled id was active. Emits `provider:changed`.
            crate::providers_ipc::set_provider_enabled,
            // F-593: backend foundation for the (deferred F-594) usage view —
            // the dashboard queries aggregated UsageTick rollups via this
            // command; cross-workspace flag aggregates across every
            // monthly file under `<config>/forge/usage/`.
            crate::usage_ipc::usage_summary,
            // F-597: container lifecycle UI. Detect probes the runtime;
            // list / stop / remove / logs operate against the in-memory
            // ContainerRegistryState shared with sessions.
            crate::containers_ipc::detect_container_runtime,
            crate::containers_ipc::list_active_containers,
            crate::containers_ipc::stop_container,
            crate::containers_ipc::remove_container,
            crate::containers_ipc::container_logs,
            // F-725: dashboard `+ New session` modal. Spawns a daemon-backed
            // session via the same code path `forge session new` walks.
            crate::session_spawn_ipc::session_start,
            // F-727: dashboard `Attach to session` picker. Returns the
            // attachable (detached) subset of `session_list`.
            crate::session_spawn_ipc::list_sessions,
            // F-734: catalog `+ Add MCP server` modal. Writes a single
            // entry into the workspace or user-scope `.mcp.json` document.
            crate::mcp_ipc::add_mcp_server,
            // F-741: status-bar git branch feed. Dashboard-scoped — the
            // `require_window_label` gate inside the command rejects every
            // window label other than `dashboard`.
            crate::git_ipc::git_branch,
        ])
        .setup(|app| {
            crate::ipc::manage_bridge(&app.handle().clone());
            crate::ipc::manage_terminals(&app.handle().clone());
            crate::ipc::manage_lsp(&app.handle().clone());
            // F-587: production credential store (`KeyringStore` + env-var
            // fallback). Idempotent like the rest of the `manage_*` family.
            crate::credentials_ipc::manage_credentials(&app.handle().clone());
            // F-597: container registry state — sessions register here
            // when their Level-2 sandbox container is created; the
            // dashboard's `<ContainersSection>` reads via
            // `list_active_containers`. Idempotent.
            crate::containers_ipc::manage_containers(&app.handle().clone());
            // F-137 / F-138: background-agent lifecycle state. Each session's
            // `BackgroundAgentRegistry` is lazily populated by
            // `resolve_bg_session` on first invoke; the state container has
            // to be managed up-front so the `State<'_, BgAgentState>` extractor
            // on each command doesn't panic.
            crate::ipc::manage_bg_agents(&app.handle().clone());
            // F-359: the URL context fetch pool + allowlist state.
            crate::ipc::manage_context_fetch(&app.handle().clone());
            // F-155: MCP commands now dispatch over the session UDS
            // bridge; no shell-side manager state to initialise.
            let manager = WindowManager::new(app.handle().clone());
            manager.open_dashboard()?;
            Ok(())
        })
        .build(tauri::generate_context!("tauri.conf.json"))
        .context("tauri runtime build failed")?
        .run(|_app_handle, event| {
            // F-593: app-shutdown defense-in-depth flush. The orchestrator's
            // session-end hook already flushes per-session UsageTicks into
            // `<config>/forge/usage/<YYYY-MM>.json`, but a webview that
            // crashes before its session emits `SessionEnded` would skip
            // the flush. On `RunEvent::Exit` we walk the shell's view of
            // open sessions and call the same flush function for each.
            //
            // The flush is idempotent (sentinel file alongside each log) so
            // it is safe to call here even when the orchestrator hook
            // already ran.
            if matches!(event, RunEvent::Exit) {
                if let Err(e) = flush_known_sessions_on_exit() {
                    tracing::warn!(
                        target: "forge_shell::window_manager",
                        error = %e,
                        "usage-flush-on-exit walk failed",
                    );
                }
            }
        });
    Ok(())
}

/// F-593: walk the on-disk session-log directories the shell can see and
/// invoke [`forge_session::usage_flush::flush_session_usage_to_user_dir`]
/// for each. We deliberately do NOT consult the live `BridgeState` —
/// `RunEvent::Exit` runs after Tauri has begun teardown, and re-entering a
/// managed `State<_>` from the run-callback is fragile. Instead we rely on
/// the durable session-log layout: `<workspace>/.forge/sessions/<id>/events.jsonl`
/// (see `forge_session::server::event_log_path`). Tempdir sessions
/// (`std::env::temp_dir().join("forge-session-<id>")`) are out of scope —
/// they are ephemeral by definition and the orchestrator hook flushes them
/// at SessionEnded time.
///
/// Workspaces are discovered from `~/.config/forge/workspaces.toml` (the
/// same registry the dashboard reads). Each workspace's `.forge/sessions/`
/// dir is iterated; every session subdir whose `events.jsonl` exists is
/// flushed. Errors on any individual log are logged + swallowed so one
/// corrupt session can't block flush of its peers.
fn flush_known_sessions_on_exit() -> anyhow::Result<()> {
    use forge_session::usage_flush::flush_session_usage_to_user_dir;

    let toml_path = crate::dashboard_sessions::default_workspaces_toml();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("flush-on-exit: tokio runtime")?;

    runtime.block_on(async move {
        let workspaces = forge_core::workspaces::read_workspaces(&toml_path)
            .await
            .unwrap_or_default();

        for ws in workspaces {
            let sessions_dir = ws.path.join(".forge").join("sessions");
            let Ok(mut rd) = tokio::fs::read_dir(&sessions_dir).await else {
                continue;
            };
            let workspace_id =
                forge_core::WorkspaceId::from_string(ws.path.to_string_lossy().into_owned());
            while let Ok(Some(entry)) = rd.next_entry().await {
                let log = entry.path().join("events.jsonl");
                if !log.exists() {
                    continue;
                }
                if let Err(e) = flush_session_usage_to_user_dir(&log, &workspace_id).await {
                    tracing::warn!(
                        target: "forge_shell::window_manager",
                        log = %log.display(),
                        error = %e,
                        "usage flush at exit failed for session log",
                    );
                }
            }
        }
    });
    Ok(())
}
