#![deny(rustdoc::broken_intra_doc_links, rustdoc::private_intra_doc_links)]
//! forge-shell: Tauri 2 host for the Forge Solid app.
//!
//! Modules:
//! - [`window_spec`]: pure declarative window configuration. Unit-tested.
//! - [`window_manager`]: runtime adapter that applies a `WindowSpec` to a live
//!   `tauri::AppHandle`. Compile-verified; no unit tests (requires a live
//!   webview runtime).
//! - [`dashboard_sessions`]: Dashboard sessions list + open Tauri commands
//!   and their pure helpers. The `collect_sessions` helper and `Pinger` trait
//!   are always compiled so they can be exercised by unit tests under
//!   `--no-default-features`; the `#[tauri::command]` wrappers are gated
//!   behind `webview`.
//!
//! `window_manager` is gated behind the `webview` feature (on by default) so
//! that `window_spec` can be unit-tested on hosts without WebKitGTK via
//! `cargo test -p forge-shell --no-default-features`.
//!
//! # Structured tracing (F-371)
//!
//! All `tracing` emissions from this crate — authz rejections in
//! `ipc::require_window_label` / `ipc::require_window_label_in`, Tauri
//! emit-target failures in [`ipc`], and the terminal / LSP forwarders —
//! use the field-name and target schema pinned in
//! [`forge_session::log_fields`]. That module is the authoritative
//! reference for operators writing log filters; do not introduce new
//! ad-hoc field names at emission sites.

pub mod bridge;
pub mod context_fetch;
// F-587: per-provider credential management commands (`login_provider`,
// `logout_provider`, `has_credential`). Pure validators, the `CredentialsState`
// container, and the production wiring (`KeyringStore` + env-var fallback)
// are always compiled — only the `#[tauri::command]` wrappers are gated
// behind `webview` so non-webview unit tests link without Tauri.
pub mod credentials_ipc;
pub mod dashboard_sessions;
// F-597: container lifecycle UI on the Dashboard. Pure helpers
// (`validate_container_id`, `classify_runtime_status`,
// `ContainerRegistryState`) are always compiled so non-webview unit
// tests link without Tauri; the `#[tauri::command]` wrappers are gated
// behind `webview`.
pub mod containers_ipc;
// F-602: Dashboard Memory section commands (`list_agent_memory`,
// `read_agent_memory`, `save_agent_memory`, `clear_agent_memory`). Pure
// validators and the `build_agent_memory_entries` helper are always
// compiled so non-webview tests link without Tauri; the
// `#[tauri::command]` wrappers are gated behind `webview`.
pub mod memory_ipc;
// F-734: Catalog `+ Add MCP server` command (`add_mcp_server`). Pure
// validators (`validate_input`, `merge_entry`, `resolve_mcp_json_path`)
// are always compiled so non-webview unit tests link without Tauri; the
// `#[tauri::command]` wrapper is gated behind `webview`.
pub mod mcp_ipc;
// F-741: Dashboard status-bar git branch IPC. The pure
// `classify_branch_output` helper is always compiled so non-webview unit
// tests can exercise the detached-HEAD classifier without Tauri; the
// `#[tauri::command]` wrapper is gated behind `webview`.
pub mod git_ipc;
// F-586: provider-selection commands (`dashboard_list_providers`,
// `get_active_provider`, `set_active_provider`). Pure helpers
// (`build_provider_list`, `is_known_provider_id`, `validate_provider_id`)
// are always compiled so non-webview tests link without Tauri; the
// `#[tauri::command]` wrappers are gated behind `webview`. The
// `dashboard_` prefix on `list_providers` disambiguates from F-591's
// roster-catalog command of the same short name.
pub mod providers_ipc;
// F-725: dashboard `+ New session` IPC. Pure validators
// (`validate_session_start_input`, `provider_is_known`, `agent_is_known`)
// are always compiled so non-webview unit tests link without Tauri; the
// `#[tauri::command]` wrapper is gated behind `webview`.
pub mod session_spawn_ipc;
// F-747: graceful daemon shutdown on session window close. Pure
// orchestration (`orchestrate_session_close`, `LivenessProbe`, `Signaler`),
// pid-file readers, and timeout constants are always compiled so non-webview
// unit tests + the escalation harness can exercise the three-stage
// graceful → SIGTERM → SIGKILL escalation without Tauri or a live daemon;
// the `#[tauri::command]` wrapper is gated behind `webview`.
pub mod session_close_ipc;
// F-593: `usage_summary` Tauri command, plus the cross-workspace toggle and
// monthly-file walker that backs it. Gated behind `webview` because the
// command itself depends on Tauri types; the helpers it ships are exercised
// in-module under `#[cfg(test)]`.
#[cfg(feature = "webview")]
pub mod usage_ipc;
// F-607: `export_transcript` Tauri command. Pure helpers
// (`read_transcript_bytes`, `transcript_path`) are always compiled so
// non-webview unit tests link without Tauri; the `#[tauri::command]` wrapper
// is gated behind `webview`.
pub mod transcript_ipc;
pub mod window_spec;

#[cfg(feature = "webview")]
pub mod ipc;
#[cfg(feature = "webview")]
pub mod window_manager;

// Debug-only `tracing` → `log` bridge so `tauri-plugin-log`'s Webview
// target receives forge-shell's tracing emissions and the user sees
// backend warns/errors in the browser devtools console.
#[cfg(feature = "webview")]
mod dev_log_bridge;
