//! F-586 provider-selection IPC tests.
//!
//! Covers `dashboard_list_providers`, `get_active_provider`, `set_active_provider`
//! end-to-end through Tauri's `test::get_ipc_response`. Mirrors the
//! `ipc_settings.rs` wiring (F-151): user-config-dir override, dashboard
//! caller, mocked credential store via `MemoryStore`.

#![cfg(feature = "webview-test")]

use std::sync::Arc;

use forge_core::workspaces::{write_workspaces, WorkspaceEntry};
use forge_core::{Credentials, MemoryStore};
use forge_shell::bridge::SessionConnections;
use forge_shell::credentials_ipc::manage_credentials_with;
use forge_shell::ipc::{build_invoke_handler, BridgeState};
use secrecy::SecretString;
use tauri::test::{mock_builder, mock_context, noop_assets, INVOKE_KEY};
use tauri::Manager;
use tempfile::TempDir;

/// Build a mock app for dashboard-caller tests: workspaces registry seeded
/// with `workspace_paths`, user-config-dir at `user_cfg_dir`, and a
/// caller-supplied credential store.
async fn make_app(
    workspace_paths: &[&std::path::Path],
    user_cfg_dir: &std::path::Path,
    creds: Arc<dyn Credentials>,
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
    manage_credentials_with(&app.handle().clone(), creds);
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

/// Seed `<user_cfg_dir>/forge/settings.toml` with `[providers.enabled]`
/// entries for the three built-ins. Tests that want a "user already added
/// every built-in" baseline call this — under the "empty by default"
/// model, `dashboard_list_providers` would otherwise return nothing.
async fn seed_user_settings(user_cfg_dir: &std::path::Path, body: &str) {
    let dir = user_cfg_dir.join("forge");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(dir.join("settings.toml"), body)
        .await
        .unwrap();
}

const BUILTINS_ENABLED_TOML: &str = r#"
[providers.enabled]
anthropic = true
openai = true
"#;

// ---------------------------------------------------------------------------
// list_providers
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn list_providers_returns_empty_on_fresh_install() {
    // Under the "empty by default" model, no providers are surfaced until
    // the user adds one through the Add Provider modal.
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    let creds: Arc<dyn Credentials> = Arc::new(MemoryStore::new());

    let (app, _registry) = make_app(&[workspace.path()], user_cfg_dir.path(), creds).await;
    let window = make_dashboard_window(&app);

    let result = invoke_ok(&window, "dashboard_list_providers", serde_json::json!({}));
    let entries = result.as_array().expect("list");
    assert!(entries.is_empty(), "expected no providers, got: {result}");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_providers_returns_added_builtins() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    let creds: Arc<dyn Credentials> = Arc::new(MemoryStore::new());
    seed_user_settings(user_cfg_dir.path(), BUILTINS_ENABLED_TOML).await;

    let (app, _registry) = make_app(&[workspace.path()], user_cfg_dir.path(), creds).await;
    let window = make_dashboard_window(&app);

    let result = invoke_ok(&window, "dashboard_list_providers", serde_json::json!({}));
    let entries = result.as_array().expect("list");
    assert_eq!(entries.len(), 2);
    let ids: Vec<&str> = entries.iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["anthropic", "openai"]);

    // Anthropic is keyed ⇒ credential_required true, has_credential false (empty store).
    assert_eq!(entries[0]["credential_required"], true);
    assert_eq!(entries[0]["has_credential"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_providers_reflects_credential_store_presence() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    let store = Arc::new(MemoryStore::new());
    store
        .set("anthropic", SecretString::from("sk-ant-1"))
        .await
        .unwrap();
    let creds: Arc<dyn Credentials> = store;
    seed_user_settings(user_cfg_dir.path(), BUILTINS_ENABLED_TOML).await;

    let (app, _registry) = make_app(&[workspace.path()], user_cfg_dir.path(), creds).await;
    let window = make_dashboard_window(&app);

    let result = invoke_ok(&window, "dashboard_list_providers", serde_json::json!({}));
    let entries = result.as_array().expect("list");
    let anthropic = entries
        .iter()
        .find(|e| e["id"] == "anthropic")
        .expect("anthropic entry");
    assert_eq!(anthropic["has_credential"], true);
    let openai = entries.iter().find(|e| e["id"] == "openai").unwrap();
    assert_eq!(openai["has_credential"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_providers_appends_user_configured_custom_openai_entries() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    let creds: Arc<dyn Credentials> = Arc::new(MemoryStore::new());

    // Seed user settings with the built-ins enabled and a custom_openai entry.
    seed_user_settings(
        user_cfg_dir.path(),
        r#"
[providers.enabled]
anthropic = true
openai = true

[providers.custom_openai.vllm-local]
base_url = "http://127.0.0.1:8000"
model = "Qwen2"
auth = { shape = "none" }
"#,
    )
    .await;

    let (app, _registry) = make_app(&[workspace.path()], user_cfg_dir.path(), creds).await;
    let window = make_dashboard_window(&app);

    let result = invoke_ok(&window, "dashboard_list_providers", serde_json::json!({}));
    let entries = result.as_array().expect("list");
    assert_eq!(entries.len(), 3);
    let custom = entries.last().unwrap();
    assert_eq!(custom["id"], "custom_openai:vllm-local");
    // `auth = none` ⇒ credential not required.
    assert_eq!(custom["credential_required"], false);
    assert_eq!(custom["model"], "Qwen2");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_providers_rejects_session_callers() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    let creds: Arc<dyn Credentials> = Arc::new(MemoryStore::new());

    let (app, _registry) = make_app(&[workspace.path()], user_cfg_dir.path(), creds).await;
    let window = make_session_window(&app, "abcdef0123456789");

    let err = invoke_err(&window, "dashboard_list_providers", serde_json::json!({}));
    assert!(
        err.contains("forbidden"),
        "expected authz rejection, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// get_active_provider
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn get_active_provider_returns_none_when_unset() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    let creds: Arc<dyn Credentials> = Arc::new(MemoryStore::new());

    let (app, _registry) = make_app(&[workspace.path()], user_cfg_dir.path(), creds).await;
    let window = make_dashboard_window(&app);

    let result = invoke_ok(&window, "get_active_provider", serde_json::json!({}));
    assert!(result.is_null(), "expected null, got: {result}");
}

#[tokio::test(flavor = "multi_thread")]
async fn get_active_provider_returns_persisted_id() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    let creds: Arc<dyn Credentials> = Arc::new(MemoryStore::new());

    // Pre-seed user settings with active = "openai".
    let user_settings_dir = user_cfg_dir.path().join("forge");
    tokio::fs::create_dir_all(&user_settings_dir).await.unwrap();
    let user_settings_path = user_settings_dir.join("settings.toml");
    tokio::fs::write(
        &user_settings_path,
        r#"
[providers]
active = "openai"
"#,
    )
    .await
    .unwrap();

    let (app, _registry) = make_app(&[workspace.path()], user_cfg_dir.path(), creds).await;
    let window = make_dashboard_window(&app);

    let result = invoke_ok(&window, "get_active_provider", serde_json::json!({}));
    assert_eq!(result, serde_json::Value::String("openai".into()));
}

// ---------------------------------------------------------------------------
// set_active_provider
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn set_active_provider_persists_known_builtin() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    let creds: Arc<dyn Credentials> = Arc::new(MemoryStore::new());
    seed_user_settings(user_cfg_dir.path(), BUILTINS_ENABLED_TOML).await;

    let (app, _registry) = make_app(&[workspace.path()], user_cfg_dir.path(), creds).await;
    let window = make_dashboard_window(&app);

    let _: serde_json::Value = invoke_ok(
        &window,
        "set_active_provider",
        serde_json::json!({"providerId": "anthropic"}),
    );

    let got = invoke_ok(&window, "get_active_provider", serde_json::json!({}));
    assert_eq!(got, serde_json::Value::String("anthropic".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn set_active_provider_rejects_unknown_id() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    let creds: Arc<dyn Credentials> = Arc::new(MemoryStore::new());

    let (app, _registry) = make_app(&[workspace.path()], user_cfg_dir.path(), creds).await;
    let window = make_dashboard_window(&app);

    let err = invoke_err(
        &window,
        "set_active_provider",
        serde_json::json!({"providerId": "made-up-provider"}),
    );
    assert!(err.contains("unknown provider"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn set_active_provider_accepts_user_configured_custom_openai() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    let creds: Arc<dyn Credentials> = Arc::new(MemoryStore::new());

    // Pre-seed user settings with a custom_openai entry.
    let user_settings_dir = user_cfg_dir.path().join("forge");
    tokio::fs::create_dir_all(&user_settings_dir).await.unwrap();
    let user_settings_path = user_settings_dir.join("settings.toml");
    tokio::fs::write(
        &user_settings_path,
        r#"
[providers.custom_openai.vllm]
base_url = "http://x"
model = "m"
auth = { shape = "none" }
"#,
    )
    .await
    .unwrap();

    let (app, _registry) = make_app(&[workspace.path()], user_cfg_dir.path(), creds).await;
    let window = make_dashboard_window(&app);

    let _: serde_json::Value = invoke_ok(
        &window,
        "set_active_provider",
        serde_json::json!({"providerId": "custom_openai:vllm"}),
    );

    let got = invoke_ok(&window, "get_active_provider", serde_json::json!({}));
    assert_eq!(got, serde_json::Value::String("custom_openai:vllm".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn set_active_provider_rejects_empty_id() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    let creds: Arc<dyn Credentials> = Arc::new(MemoryStore::new());

    let (app, _registry) = make_app(&[workspace.path()], user_cfg_dir.path(), creds).await;
    let window = make_dashboard_window(&app);

    let err = invoke_err(
        &window,
        "set_active_provider",
        serde_json::json!({"providerId": ""}),
    );
    assert!(err.contains("empty"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn set_active_provider_rejects_session_callers() {
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    let creds: Arc<dyn Credentials> = Arc::new(MemoryStore::new());

    let (app, _registry) = make_app(&[workspace.path()], user_cfg_dir.path(), creds).await;
    let window = make_session_window(&app, "abcdef0123456789");

    let err = invoke_err(
        &window,
        "set_active_provider",
        serde_json::json!({"providerId": "anthropic"}),
    );
    assert!(err.contains("forbidden"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn set_active_provider_then_list_reflects_choice_through_credential_store_unchanged() {
    // After persisting, the list view doesn't change shape — has_credential
    // continues to reflect the keyring state, not the active selection.
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    let store = Arc::new(MemoryStore::new());
    store
        .set("openai", SecretString::from("sk-test"))
        .await
        .unwrap();
    let creds: Arc<dyn Credentials> = store;
    seed_user_settings(user_cfg_dir.path(), BUILTINS_ENABLED_TOML).await;

    let (app, _registry) = make_app(&[workspace.path()], user_cfg_dir.path(), creds).await;
    let window = make_dashboard_window(&app);

    let _: serde_json::Value = invoke_ok(
        &window,
        "set_active_provider",
        serde_json::json!({"providerId": "openai"}),
    );

    let result = invoke_ok(&window, "dashboard_list_providers", serde_json::json!({}));
    let entries = result.as_array().unwrap();
    let openai = entries.iter().find(|e| e["id"] == "openai").unwrap();
    assert_eq!(openai["has_credential"], true);
    let anthropic = entries.iter().find(|e| e["id"] == "anthropic").unwrap();
    assert_eq!(anthropic["has_credential"], false);
}

// ---------------------------------------------------------------------------
// Concurrency guard (F-586 review)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn rapid_set_active_provider_calls_leave_settings_in_a_known_state() {
    // `set_active_provider`'s read-modify-write is now wrapped in a
    // process-wide `tokio::sync::Mutex` (see
    // `providers_ipc::settings_write_guard`). The Tauri MockRuntime is
    // !Send, so we cannot spawn invokes onto separate tokio tasks; we
    // instead drive them sequentially from the same task and assert
    // that every call commits a valid known-id TOML file (no torn write,
    // no lost setting).
    //
    // The serialization contract is unit-pinned by the guard's existence
    // and the per-call read-then-write structure inside
    // `set_active_provider`; this test ensures the surrounding code path
    // doesn't leave the file in a broken state on rapid switches.
    let workspace = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
    let creds: Arc<dyn Credentials> = Arc::new(MemoryStore::new());
    seed_user_settings(user_cfg_dir.path(), BUILTINS_ENABLED_TOML).await;
    let (app, _registry) = make_app(&[workspace.path()], user_cfg_dir.path(), creds).await;
    let window = make_dashboard_window(&app);

    let candidates = ["anthropic", "openai"];
    for id in candidates.iter().cycle().take(20) {
        invoke_ok(
            &window,
            "set_active_provider",
            serde_json::json!({"providerId": *id}),
        );
        // Read-after-write: the value must be one of the three known
        // ids. A torn write would surface as a parse failure inside
        // get_active_provider.
        let active = invoke_ok(&window, "get_active_provider", serde_json::json!({}));
        let active = active.as_str().expect("active provider is a string");
        assert!(
            candidates.contains(&active),
            "expected one of the three contended ids, got {active:?}"
        );
    }
}
