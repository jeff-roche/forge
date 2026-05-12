//! F-591 roster discovery IPC tests.
//!
//! Covers `list_skills`, `list_mcp_servers`, `list_agents`, `list_providers`
//! end-to-end through Tauri's `test::get_ipc_response`. Each command:
//!
//! 1. Authz: dashboard label is allowed; an arbitrary unrelated label is rejected.
//! 2. Loads from the canonical source (skill_loader / agent loader / .mcp.json /
//!    hardcoded provider list).
//! 3. Honors `RosterScope` filtering — `SessionWide` returns everything,
//!    `Provider(id)` narrows to that provider, `Agent(id)` narrows to entries
//!    bound to that agent (today: empty for skills/agents/MCP, since those
//!    surface as `SessionWide`).
//!
//! Tests use the dashboard window label and a workspaces-toml registry seeded
//! by `BridgeState::with_test_user_config_and_workspaces`, mirroring the
//! pattern in `ipc_settings.rs`.

#![cfg(feature = "webview-test")]

use forge_core::workspaces::{write_workspaces, WorkspaceEntry};
use forge_shell::bridge::SessionConnections;
use forge_shell::ipc::{build_invoke_handler, BridgeState};
use serde_json::{json, Value};
use std::fs;
use tauri::test::{mock_builder, mock_context, noop_assets, INVOKE_KEY};
use tauri::Manager;
use tempfile::TempDir;

/// Build a dashboard-caller mock app: workspaces registry seeded with the
/// supplied paths so `resolve_workspace_root_for_command` accepts them.
async fn make_app_with_registry(
    workspace_paths: &[&std::path::Path],
) -> (tauri::App<tauri::test::MockRuntime>, TempDir, TempDir) {
    let registry_dir = TempDir::new().unwrap();
    let user_cfg_dir = TempDir::new().unwrap();
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
        user_cfg_dir.path().to_path_buf(),
        toml_path,
    ));
    (app, registry_dir, user_cfg_dir)
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
    label: &str,
) -> tauri::WebviewWindow<tauri::test::MockRuntime> {
    tauri::WebviewWindowBuilder::new(app, label, tauri::WebviewUrl::App("index.html".into()))
        .build()
        .expect("mock session window")
}

fn invoke_ok(
    window: &tauri::WebviewWindow<tauri::test::MockRuntime>,
    cmd: &str,
    payload: Value,
) -> Value {
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
    payload: Value,
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
        Err(Value::String(s)) => s,
        Err(other) => other.to_string(),
    }
}

fn write_skill(workspace: &std::path::Path, id: &str, name: &str) {
    let dir = workspace.join(".agent-skills").join(id);
    fs::create_dir_all(&dir).expect("create skill dir");
    let body = format!("---\nname: {name}\n---\n\nbody for {id}\n");
    fs::write(dir.join("SKILL.md"), body).expect("write SKILL.md");
}

fn write_agent(workspace: &std::path::Path, name: &str) {
    let dir = workspace.join(".agents");
    fs::create_dir_all(&dir).expect("create .agents");
    let body = format!("---\nname: {name}\n---\n\nbody for {name}\n");
    fs::write(dir.join(format!("{name}.md")), body).expect("write agent .md");
}

fn write_mcp_config(workspace: &std::path::Path, server_name: &str) {
    let body = format!(
        r#"{{
  "mcpServers": {{
    "{server_name}": {{
      "command": "/usr/bin/true",
      "args": []
    }}
  }}
}}
"#,
    );
    fs::write(workspace.join(".mcp.json"), body).expect("write .mcp.json");
}

fn session_wide() -> Value {
    json!({ "type": "SessionWide" })
}

fn agent_scope(id: &str) -> Value {
    json!({ "type": "Agent", "id": id })
}

fn provider_scope(id: &str) -> Value {
    json!({ "type": "Provider", "id": id })
}

// ---------------------------------------------------------------------------
// list_skills
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn list_skills_session_wide_returns_workspace_skills() {
    let workspace = TempDir::new().unwrap();
    let canonical = std::fs::canonicalize(workspace.path()).expect("canonicalize");
    write_skill(&canonical, "planner", "Planner");
    write_skill(&canonical, "reviewer", "Reviewer");

    let (app, _reg, _cfg) = make_app_with_registry(&[&canonical]).await;
    let window = make_dashboard_window(&app);

    let result = invoke_ok(
        &window,
        "list_skills",
        json!({
            "workspaceRoot": canonical,
            "scope": session_wide(),
        }),
    );
    let arr = result.as_array().expect("array");
    let ids: Vec<&str> = arr
        .iter()
        .map(|v| v["entry"]["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"planner"), "missing planner: got {ids:?}");
    assert!(ids.contains(&"reviewer"), "missing reviewer: got {ids:?}");
    for entry in arr {
        assert_eq!(entry["entry"]["type"], "Skill");
        assert_eq!(entry["scope"]["type"], "SessionWide");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn list_skills_agent_filter_returns_empty_for_session_wide_skills() {
    // F-591 today: every disk-loaded skill surfaces as SessionWide. An agent-
    // scoped query must therefore return nothing — the per-agent binding
    // arrives in a later task.
    let workspace = TempDir::new().unwrap();
    let canonical = std::fs::canonicalize(workspace.path()).expect("canonicalize");
    write_skill(&canonical, "planner", "Planner");

    let (app, _reg, _cfg) = make_app_with_registry(&[&canonical]).await;
    let window = make_dashboard_window(&app);

    let result = invoke_ok(
        &window,
        "list_skills",
        json!({
            "workspaceRoot": canonical,
            "scope": agent_scope("any-agent"),
        }),
    );
    assert_eq!(
        result.as_array().unwrap().len(),
        0,
        "agent-scoped query must exclude session-wide skills: {result}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_skills_rejects_unknown_window_label() {
    let workspace = TempDir::new().unwrap();
    let canonical = std::fs::canonicalize(workspace.path()).expect("canonicalize");
    let (app, _reg, _cfg) = make_app_with_registry(&[&canonical]).await;
    let window = make_session_window(&app, "intruder-window");

    let err = invoke_err(
        &window,
        "list_skills",
        json!({
            "workspaceRoot": canonical,
            "scope": session_wide(),
        }),
    );
    assert!(
        err.contains("window label"),
        "expected authz rejection, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// list_mcp_servers (F-591 roster command — distinct from session_list_mcp_servers)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn list_mcp_servers_session_wide_returns_workspace_servers() {
    let workspace = TempDir::new().unwrap();
    let canonical = std::fs::canonicalize(workspace.path()).expect("canonicalize");
    write_mcp_config(&canonical, "github");

    let (app, _reg, _cfg) = make_app_with_registry(&[&canonical]).await;
    let window = make_dashboard_window(&app);

    let result = invoke_ok(
        &window,
        "list_mcp_servers",
        json!({
            "workspaceRoot": canonical,
            "scope": session_wide(),
        }),
    );
    let arr = result.as_array().unwrap();
    assert!(
        arr.iter()
            .any(|e| e["entry"]["type"] == "Mcp" && e["entry"]["id"] == "github"),
        "missing github server in {result}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_mcp_servers_returns_empty_when_no_config() {
    let workspace = TempDir::new().unwrap();
    let canonical = std::fs::canonicalize(workspace.path()).expect("canonicalize");
    let (app, _reg, _cfg) = make_app_with_registry(&[&canonical]).await;
    let window = make_dashboard_window(&app);

    let result = invoke_ok(
        &window,
        "list_mcp_servers",
        json!({
            "workspaceRoot": canonical,
            "scope": session_wide(),
        }),
    );
    assert_eq!(result.as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// list_agents
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn list_agents_session_wide_returns_workspace_agents() {
    let workspace = TempDir::new().unwrap();
    let canonical = std::fs::canonicalize(workspace.path()).expect("canonicalize");
    write_agent(&canonical, "planner");
    write_agent(&canonical, "reviewer");

    let (app, _reg, _cfg) = make_app_with_registry(&[&canonical]).await;
    let window = make_dashboard_window(&app);

    let result = invoke_ok(
        &window,
        "list_agents",
        json!({
            "workspaceRoot": canonical,
            "scope": session_wide(),
        }),
    );
    let arr = result.as_array().unwrap();
    let names: Vec<&str> = arr
        .iter()
        .map(|v| v["entry"]["id"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"planner"), "missing planner: {names:?}");
    assert!(names.contains(&"reviewer"), "missing reviewer: {names:?}");
    for entry in arr {
        assert_eq!(entry["entry"]["type"], "Agent");
        assert_eq!(entry["entry"]["background"], false);
        assert_eq!(entry["scope"]["type"], "SessionWide");
    }
}

// ---------------------------------------------------------------------------
// list_providers
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn list_providers_session_wide_returns_built_ins() {
    let workspace = TempDir::new().unwrap();
    let canonical = std::fs::canonicalize(workspace.path()).expect("canonicalize");
    let (app, _reg, _cfg) = make_app_with_registry(&[&canonical]).await;
    let window = make_dashboard_window(&app);

    let result = invoke_ok(
        &window,
        "list_providers",
        json!({
            "workspaceRoot": canonical,
            "scope": session_wide(),
        }),
    );
    let arr = result.as_array().unwrap();
    let ids: Vec<&str> = arr
        .iter()
        .map(|v| v["entry"]["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"anthropic"), "expected anthropic in {ids:?}");
    assert!(ids.contains(&"openai"), "expected openai in {ids:?}");
    assert!(ids.contains(&"ollama"), "expected ollama in {ids:?}");
    for entry in arr {
        assert_eq!(entry["entry"]["type"], "Provider");
        assert_eq!(entry["scope"]["type"], "Provider");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn list_providers_provider_scope_narrows_to_one() {
    let workspace = TempDir::new().unwrap();
    let canonical = std::fs::canonicalize(workspace.path()).expect("canonicalize");
    let (app, _reg, _cfg) = make_app_with_registry(&[&canonical]).await;
    let window = make_dashboard_window(&app);

    let result = invoke_ok(
        &window,
        "list_providers",
        json!({
            "workspaceRoot": canonical,
            "scope": provider_scope("anthropic"),
        }),
    );
    let arr = result.as_array().unwrap();
    assert_eq!(arr.len(), 1, "expected one provider, got {result}");
    assert_eq!(arr[0]["entry"]["id"], "anthropic");
    assert_eq!(arr[0]["scope"]["id"], "anthropic");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_providers_agent_scope_returns_empty() {
    // No provider is bound to an agent today — agent-filtered query is empty.
    let workspace = TempDir::new().unwrap();
    let canonical = std::fs::canonicalize(workspace.path()).expect("canonicalize");
    let (app, _reg, _cfg) = make_app_with_registry(&[&canonical]).await;
    let window = make_dashboard_window(&app);

    let result = invoke_ok(
        &window,
        "list_providers",
        json!({
            "workspaceRoot": canonical,
            "scope": agent_scope("planner"),
        }),
    );
    assert_eq!(result.as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_providers_rejects_unknown_window_label() {
    let workspace = TempDir::new().unwrap();
    let canonical = std::fs::canonicalize(workspace.path()).expect("canonicalize");
    let (app, _reg, _cfg) = make_app_with_registry(&[&canonical]).await;
    let window = make_session_window(&app, "intruder-window");

    let err = invoke_err(
        &window,
        "list_providers",
        json!({
            "workspaceRoot": canonical,
            "scope": session_wide(),
        }),
    );
    assert!(
        err.contains("window label"),
        "expected authz rejection, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_providers_surfaces_custom_openai_entries_from_settings() {
    // F-585: custom OpenAI-compat entries declared in
    // `[providers.custom_openai.<name>]` should appear in the roster as
    // `custom_openai:<name>`.
    let workspace = TempDir::new().unwrap();
    let canonical = std::fs::canonicalize(workspace.path()).expect("canonicalize");
    let forge_dir = canonical.join(".forge");
    fs::create_dir_all(&forge_dir).unwrap();
    fs::write(
        forge_dir.join("settings.toml"),
        r#"
[providers.custom_openai.together]
base_url = "https://api.together.xyz"
model = "meta-llama/Llama-3-70b-chat-hf"
api_key = "tok"
"#,
    )
    .unwrap();

    let (app, _reg, _cfg) = make_app_with_registry(&[&canonical]).await;
    let window = make_dashboard_window(&app);

    let result = invoke_ok(
        &window,
        "list_providers",
        json!({
            "workspaceRoot": canonical,
            "scope": session_wide(),
        }),
    );
    let arr = result.as_array().unwrap();
    let custom = arr
        .iter()
        .find(|v| v["entry"]["id"] == "custom_openai:together")
        .unwrap_or_else(|| panic!("missing custom_openai:together entry: {result}"));
    assert_eq!(
        custom["entry"]["model"], "meta-llama/Llama-3-70b-chat-hf",
        "expected default model surfaced",
    );
}
