//! F-607 transcript export IPC tests.
//!
//! Drives `export_transcript` end-to-end through Tauri's
//! `tauri::test::get_ipc_response`. Mirrors the F-602 memory test harness:
//! the user-config dir + workspaces registry are redirected via the
//! `webview-test`-gated `BridgeState::with_test_*` constructors so tests
//! never touch the real platform paths.

#![cfg(feature = "webview-test")]

use forge_core::workspaces::{write_workspaces, WorkspaceEntry};
use forge_shell::bridge::SessionConnections;
use forge_shell::ipc::{build_invoke_handler, BridgeState};
use tauri::test::{mock_builder, mock_context, noop_assets, INVOKE_KEY};
use tauri::Manager;
use tempfile::TempDir;

const VALID_SID: &str = "deadbeefcafebabe";

/// Build a mock app with the workspaces registry seeded *and* the session
/// connection's cached workspace primed. Lets a single app host both
/// `dashboard` and `session-{id}` window flavors.
async fn make_app(
    workspace: &std::path::Path,
    session_id: &str,
) -> (tauri::App<tauri::test::MockRuntime>, TempDir) {
    let registry_dir = TempDir::new().unwrap();
    let toml_path = registry_dir.path().join("workspaces.toml");
    let canonical = std::fs::canonicalize(workspace).expect("canonicalize workspace");
    let entries = vec![WorkspaceEntry {
        id: forge_core::WorkspaceId::new(),
        path: canonical.clone(),
        name: "ws".into(),
        last_opened: chrono::Utc::now(),
        pinned: false,
    }];
    write_workspaces(&toml_path, &entries)
        .await
        .expect("seed workspaces.toml");

    let connections = SessionConnections::new();
    connections
        .prime_workspace_root_for_test(session_id.to_string(), canonical.clone())
        .await;

    let user_cfg_dir = TempDir::new().unwrap();
    let app = mock_builder()
        .invoke_handler(build_invoke_handler())
        .build(mock_context(noop_assets()))
        .expect("build mock Tauri app");
    app.manage(BridgeState::with_test_user_config_and_workspaces(
        connections,
        user_cfg_dir.path().to_path_buf(),
        toml_path,
    ));
    // Hold user_cfg_dir for the lifetime of the test by leaking it into
    // `forget`. The OS reclaims it at test exit; this just keeps the path
    // alive past the function return without complicating the signature.
    std::mem::forget(user_cfg_dir);
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

/// Seed `<workspace>/.forge/sessions/<sid>/events.jsonl` with `bytes`.
fn seed_transcript(workspace: &std::path::Path, sid: &str, bytes: &[u8]) {
    let dir = workspace.join(".forge").join("sessions").join(sid);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("events.jsonl"), bytes).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn export_transcript_round_trips_session_events() {
    let workspace = TempDir::new().unwrap();
    let payload = b"{\"event\":\"hello\",\"seq\":1}\n{\"event\":\"world\",\"seq\":2}\n".to_vec();
    seed_transcript(workspace.path(), VALID_SID, &payload);

    let (app, _registry) = make_app(workspace.path(), VALID_SID).await;
    let window = make_session_window(&app, VALID_SID);

    let canonical_ws = std::fs::canonicalize(workspace.path()).unwrap();
    let bytes = invoke_ok(
        &window,
        "export_transcript",
        serde_json::json!({
            "sessionId": VALID_SID,
            "workspaceRoot": canonical_ws,
        }),
    );
    let arr = bytes.as_array().expect("response is a byte array");
    let got: Vec<u8> = arr
        .iter()
        .map(|v| v.as_u64().expect("byte") as u8)
        .collect();
    assert_eq!(got, payload, "transcript bytes must round-trip verbatim");
}

#[tokio::test(flavor = "multi_thread")]
async fn export_transcript_round_trips_for_dashboard_caller() {
    // The Inspector's "Export transcript" button is rendered in the
    // dashboard window. Verify the dashboard label is accepted.
    let workspace = TempDir::new().unwrap();
    let payload = b"{\"event\":\"only\"}\n".to_vec();
    seed_transcript(workspace.path(), VALID_SID, &payload);

    let (app, _registry) = make_app(workspace.path(), VALID_SID).await;
    let window = make_dashboard_window(&app);

    let canonical_ws = std::fs::canonicalize(workspace.path()).unwrap();
    let bytes = invoke_ok(
        &window,
        "export_transcript",
        serde_json::json!({
            "sessionId": VALID_SID,
            "workspaceRoot": canonical_ws,
        }),
    );
    let arr = bytes.as_array().unwrap();
    let got: Vec<u8> = arr.iter().map(|v| v.as_u64().unwrap() as u8).collect();
    assert_eq!(got, payload);
}

#[tokio::test(flavor = "multi_thread")]
async fn export_transcript_rejects_invalid_session_id() {
    let workspace = TempDir::new().unwrap();
    let (app, _registry) = make_app(workspace.path(), VALID_SID).await;
    let window = make_dashboard_window(&app);

    let canonical_ws = std::fs::canonicalize(workspace.path()).unwrap();
    for bad in [
        "../etc/passwd",
        "deadbeef cafebabe",
        "DEADBEEFCAFEBABE",
        "deadbeefcafebab",   // 15 chars
        "deadbeefcafebabe0", // 17 chars
        "zzzzzzzzzzzzzzzz",
        "",
    ] {
        let err = invoke_err(
            &window,
            "export_transcript",
            serde_json::json!({
                "sessionId": bad,
                "workspaceRoot": canonical_ws,
            }),
        );
        assert!(
            err.contains("invalid session id"),
            "expected validation rejection for {bad:?}, got {err}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn export_transcript_rejects_session_window_for_other_session() {
    // session-A must not be able to export session-B's transcript.
    let workspace = TempDir::new().unwrap();
    let other_sid = "0123456789abcdef";
    seed_transcript(workspace.path(), other_sid, b"secret\n");

    let (app, _registry) = make_app(workspace.path(), VALID_SID).await;
    // Caller window is `session-deadbeefcafebabe`.
    let window = make_session_window(&app, VALID_SID);

    let canonical_ws = std::fs::canonicalize(workspace.path()).unwrap();
    let err = invoke_err(
        &window,
        "export_transcript",
        serde_json::json!({
            "sessionId": other_sid,
            "workspaceRoot": canonical_ws,
        }),
    );
    assert!(
        err.contains("forbidden") || err.contains("label mismatch"),
        "expected authz rejection, got {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn export_transcript_handles_missing_file_gracefully() {
    // Brand-new session that has not yet written its first event:
    // the directory may not exist yet. Expect an empty byte vec, not an
    // error — the Inspector renders "no events" instead of breaking.
    let workspace = TempDir::new().unwrap();
    let (app, _registry) = make_app(workspace.path(), VALID_SID).await;
    let window = make_dashboard_window(&app);

    let canonical_ws = std::fs::canonicalize(workspace.path()).unwrap();
    let bytes = invoke_ok(
        &window,
        "export_transcript",
        serde_json::json!({
            "sessionId": VALID_SID,
            "workspaceRoot": canonical_ws,
        }),
    );
    let arr = bytes.as_array().expect("array even when empty");
    assert!(arr.is_empty(), "expected empty transcript, got {arr:?}");
}
