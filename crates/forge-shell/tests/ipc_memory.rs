//! F-602 memory IPC tests.
//!
//! Drives `list_agent_memory`, `read_agent_memory`, `save_agent_memory`,
//! and `clear_agent_memory` end-to-end through Tauri's
//! `test::get_ipc_response`. Mirrors the F-151 settings test harness — the
//! user-config dir is redirected via the `webview-test`-gated
//! `BridgeState::with_test_user_config_dir` so tests never touch the real
//! platform config dir.

#![cfg(feature = "webview-test")]

use forge_core::workspaces::{write_workspaces, WorkspaceEntry};
use forge_shell::bridge::SessionConnections;
use forge_shell::ipc::{build_invoke_handler, BridgeState};
use tauri::test::{mock_builder, mock_context, noop_assets, INVOKE_KEY};
use tauri::Manager;
use tempfile::TempDir;

async fn make_app(
    workspace: &std::path::Path,
    session_id: &str,
    user_cfg_dir: &std::path::Path,
) -> tauri::App<tauri::test::MockRuntime> {
    let connections = SessionConnections::new();
    connections
        .prime_workspace_root_for_test(
            session_id.to_string(),
            std::fs::canonicalize(workspace).expect("canonicalize workspace"),
        )
        .await;
    let app = mock_builder()
        .invoke_handler(build_invoke_handler())
        .build(mock_context(noop_assets()))
        .expect("build mock Tauri app");
    app.manage(BridgeState::with_test_user_config_dir(
        connections,
        user_cfg_dir.to_path_buf(),
    ));
    app
}

/// Build a mock app with the workspaces registry seeded so dashboard
/// callers can pass `workspaceRoot` validation. Mirrors the
/// `make_app_with_registry` helper from `ipc_settings.rs`.
async fn make_dashboard_app(
    workspace_paths: &[&std::path::Path],
    user_cfg_dir: &std::path::Path,
) -> (tauri::App<tauri::test::MockRuntime>, TempDir) {
    let registry_dir = TempDir::new().unwrap();
    let toml_path = registry_dir.path().join("workspaces.toml");
    let entries: Vec<WorkspaceEntry> = workspace_paths
        .iter()
        .enumerate()
        .map(|(i, p)| WorkspaceEntry {
            id: forge_core::WorkspaceId::new(),
            path: p.to_path_buf(),
            name: format!("ws-{i}"),
            last_opened: chrono::Utc::now(),
            pinned: false,
        })
        .collect();
    write_workspaces(&toml_path, &entries)
        .await
        .expect("seed workspaces.toml");

    let connections = SessionConnections::new();
    let app = mock_builder()
        .invoke_handler(build_invoke_handler())
        .build(mock_context(noop_assets()))
        .expect("build mock Tauri app");
    app.manage(BridgeState::with_test_user_config_and_workspaces(
        connections,
        user_cfg_dir.to_path_buf(),
        toml_path,
    ));
    (app, registry_dir)
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

fn make_session_window(
    app: &tauri::App<tauri::test::MockRuntime>,
    session_id: &str,
) -> tauri::WebviewWindow<tauri::test::MockRuntime> {
    tauri::WebviewWindowBuilder::new(
        app,
        format!("session-{session_id}"),
        tauri::WebviewUrl::App("index.html".into()),
    )
    .build()
    .expect("mock session window")
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

fn invoke_err(
    window: &tauri::WebviewWindow<tauri::test::MockRuntime>,
    cmd: &str,
    payload: serde_json::Value,
) -> String {
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
    match res {
        Ok(ok) => panic!("expected error, got Ok: {ok:?}"),
        Err(serde_json::Value::String(s)) => s,
        Err(other) => other.to_string(),
    }
}

fn seed_agent(workspace: &std::path::Path, name: &str, memory_enabled: bool) {
    let agents_dir = workspace.join(".agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let body = if memory_enabled {
        format!("---\nname: {name}\nmemory: true\n---\nbody for {name}\n")
    } else {
        format!("---\nname: {name}\n---\nbody for {name}\n")
    };
    std::fs::write(agents_dir.join(format!("{name}.md")), body).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn list_agent_memory_returns_one_row_per_loaded_agent() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    seed_agent(workspace.path(), "alpha", true);
    seed_agent(workspace.path(), "beta", false);
    let canonical_ws = std::fs::canonicalize(workspace.path()).unwrap();

    let (app, _registry) = make_dashboard_app(&[&canonical_ws], user_cfg_dir.path()).await;
    let window = make_dashboard_window(&app);

    let entries = invoke_ok(
        &window,
        "list_agent_memory",
        serde_json::json!({ "workspaceRoot": canonical_ws }),
    );
    let arr = entries.as_array().expect("entries array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["agent_id"], "alpha");
    assert_eq!(arr[0]["def_enabled"], true);
    assert_eq!(arr[1]["agent_id"], "beta");
    assert_eq!(arr[1]["def_enabled"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_agent_memory_rejects_session_window() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    seed_agent(workspace.path(), "alpha", true);

    let sid = "abcdef0123456789";
    let app = make_app(workspace.path(), sid, user_cfg_dir.path()).await;
    let window = make_session_window(&app, sid);

    let err = invoke_err(
        &window,
        "list_agent_memory",
        serde_json::json!({ "workspaceRoot": std::fs::canonicalize(workspace.path()).unwrap() }),
    );
    assert!(
        err.contains("forbidden") || err.contains("label mismatch"),
        "expected forbidden error, got {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn save_then_read_agent_memory_round_trips() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    seed_agent(workspace.path(), "scribe", true);

    let sid = "abcdef0123456789";
    let app = make_app(workspace.path(), sid, user_cfg_dir.path()).await;
    let window = make_dashboard_window(&app);

    let saved = invoke_ok(
        &window,
        "save_agent_memory",
        serde_json::json!({ "agentId": "scribe", "body": "remember the milk" }),
    );
    assert_eq!(saved["version"], 1);

    let body = invoke_ok(
        &window,
        "read_agent_memory",
        serde_json::json!({ "agentId": "scribe" }),
    );
    assert_eq!(body, serde_json::Value::String("remember the milk".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn read_agent_memory_returns_empty_when_absent() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();

    let sid = "abcdef0123456789";
    let app = make_app(workspace.path(), sid, user_cfg_dir.path()).await;
    let window = make_dashboard_window(&app);

    let body = invoke_ok(
        &window,
        "read_agent_memory",
        serde_json::json!({ "agentId": "ghost" }),
    );
    assert_eq!(body, serde_json::Value::String(String::new()));
}

#[tokio::test(flavor = "multi_thread")]
async fn clear_agent_memory_wipes_existing_body() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    seed_agent(workspace.path(), "scribe", true);

    let sid = "abcdef0123456789";
    let app = make_app(workspace.path(), sid, user_cfg_dir.path()).await;
    let window = make_dashboard_window(&app);

    let _: serde_json::Value = invoke_ok(
        &window,
        "save_agent_memory",
        serde_json::json!({ "agentId": "scribe", "body": "secrets" }),
    );

    let _: serde_json::Value = invoke_ok(
        &window,
        "clear_agent_memory",
        serde_json::json!({ "agentId": "scribe" }),
    );

    let body = invoke_ok(
        &window,
        "read_agent_memory",
        serde_json::json!({ "agentId": "scribe" }),
    );
    assert_eq!(body, serde_json::Value::String(String::new()));
}

#[tokio::test(flavor = "multi_thread")]
async fn save_agent_memory_rejects_path_separators_in_id() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();

    let sid = "abcdef0123456789";
    let app = make_app(workspace.path(), sid, user_cfg_dir.path()).await;
    let window = make_dashboard_window(&app);

    let err = invoke_err(
        &window,
        "save_agent_memory",
        serde_json::json!({ "agentId": "../etc/passwd", "body": "x" }),
    );
    assert!(err.contains("path separators"), "got {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_agent_memory_surfaces_settings_override() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    seed_agent(workspace.path(), "alpha", false);
    let canonical_ws = std::fs::canonicalize(workspace.path()).unwrap();

    let (app, _registry) = make_dashboard_app(&[&canonical_ws], user_cfg_dir.path()).await;
    let window = make_dashboard_window(&app);

    // Toggle the settings override ON via set_setting.
    let _: serde_json::Value = invoke_ok(
        &window,
        "set_setting",
        serde_json::json!({
            "key": "memory.enabled.alpha",
            "value": true,
            "level": "workspace",
            "workspaceRoot": canonical_ws,
        }),
    );

    let entries = invoke_ok(
        &window,
        "list_agent_memory",
        serde_json::json!({ "workspaceRoot": canonical_ws }),
    );
    let arr = entries.as_array().expect("entries array");
    let alpha = arr.iter().find(|e| e["agent_id"] == "alpha").unwrap();
    assert_eq!(alpha["settings_override"], true);
    // def_enabled stays false; the override is the user's overlay on top.
    assert_eq!(alpha["def_enabled"], false);
}
