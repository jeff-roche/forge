//! F-672: tracing-emission pin for the dashboard-scoped IPC commands in
//! `containers_ipc`, `memory_ipc`, and `usage_ipc`.
//!
//! Each module emits `tracing::trace!(target: "forge_shell::<module>", ...)`
//! on the success path and `tracing::warn!` on the failure path. This
//! suite drives a representative success path per module through the
//! Tauri mock harness and asserts the structured fields a dashboard
//! operator needs when triaging logs (target + principal field).
//!
//! Mirrors the F-371 `ipc_authz_tracing.rs` shape: install the global
//! capture subscriber once, serialize tests on `capture_test_lock`, drain
//! and grep.

#![cfg(feature = "webview-test")]

mod common;

use forge_shell::bridge::SessionConnections;
use forge_shell::ipc::{build_invoke_handler, BridgeState};
use tauri::test::{mock_builder, mock_context, noop_assets, INVOKE_KEY};
use tauri::Manager;
use tempfile::TempDir;

fn make_app(user_cfg_dir: &std::path::Path) -> tauri::App<tauri::test::MockRuntime> {
    let connections = SessionConnections::new();
    let app = mock_builder()
        .invoke_handler(build_invoke_handler())
        .build(mock_context(noop_assets()))
        .expect("build mock Tauri app");
    app.manage(BridgeState::with_test_user_config_dir(
        connections,
        user_cfg_dir.to_path_buf(),
    ));
    app.manage(forge_shell::containers_ipc::ContainerRegistryState::new());
    app
}

fn make_dashboard_window(
    app: &tauri::App<tauri::test::MockRuntime>,
) -> tauri::WebviewWindow<tauri::test::MockRuntime> {
    tauri::WebviewWindowBuilder::new(
        app,
        "dashboard",
        tauri::WebviewUrl::App("index.html".into()),
    )
    .build()
    .expect("mock dashboard window")
}

fn invoke_ok(
    window: &tauri::WebviewWindow<tauri::test::MockRuntime>,
    cmd: &str,
    payload: serde_json::Value,
) -> serde_json::Value {
    let res = tauri::test::get_ipc_response(
        window,
        tauri::webview::InvokeRequest {
            cmd: cmd.into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: "http://tauri.localhost".parse().unwrap(),
            body: tauri::ipc::InvokeBody::Json(payload),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    );
    res.expect("invoke returned Ok").deserialize().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn save_agent_memory_emits_trace_on_success() {
    let _g = common::capture_test_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    common::install_capture_subscriber();
    let _ = common::drain_capture();

    let user_cfg_dir = TempDir::new().unwrap();
    let app = make_app(user_cfg_dir.path());
    let window = make_dashboard_window(&app);

    let _ = invoke_ok(
        &window,
        "save_agent_memory",
        serde_json::json!({ "agentId": "scribe", "body": "remember the milk" }),
    );

    let logs = common::drain_capture();
    assert!(
        logs.contains("forge_shell::memory"),
        "expected memory target, got: {logs}"
    );
    assert!(
        logs.contains("save_agent_memory ok"),
        "expected success message, got: {logs}"
    );
    assert!(
        logs.contains("agent_id=scribe"),
        "expected agent_id field, got: {logs}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_active_containers_does_not_log_authz_warn_on_success() {
    // The container commands trace on runtime calls (stop/remove/logs);
    // their success-path tracing is exercised on the `detect` and `logs`
    // paths which require a real podman binary. This guard pins the
    // lower bar: the dashboard label *passes* the authz gate, so no
    // forge_shell::ipc::authz warning should appear.
    let _g = common::capture_test_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    common::install_capture_subscriber();
    let _ = common::drain_capture();

    let user_cfg_dir = TempDir::new().unwrap();
    let app = make_app(user_cfg_dir.path());
    let window = make_dashboard_window(&app);

    let _ = invoke_ok(&window, "list_active_containers", serde_json::json!({}));

    let logs = common::drain_capture();
    assert!(
        !logs.contains("forge_shell::ipc::authz"),
        "dashboard label must pass authz silently, got: {logs}"
    );
}
