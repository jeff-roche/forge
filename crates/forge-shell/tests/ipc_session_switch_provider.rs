//! Tracker #702: `session_switch_provider` must size-cap `provider_id`.
//!
//! Other provider IPCs (e.g. `set_active_provider`) already validate
//! through `crate::providers_ipc::validate_provider_id`, which rejects
//! empty IDs and anything past `MAX_PROVIDER_ID_BYTES` (128 bytes). The
//! finding noted `session_switch_provider` lacked the same guard; this
//! test pins the new wiring.

#![cfg(feature = "webview-test")]

use forge_shell::bridge::SessionConnections;
use forge_shell::ipc::{build_invoke_handler, BridgeState};
use tauri::test::{get_ipc_response, mock_builder, mock_context, noop_assets, INVOKE_KEY};
use tauri::Manager;

const TEST_SESSION: &str = "abcdef0123456789";

fn make_app() -> tauri::App<tauri::test::MockRuntime> {
    let app = mock_builder()
        .invoke_handler(build_invoke_handler())
        .build(mock_context(noop_assets()))
        .expect("build mock Tauri app");
    app.manage(BridgeState::new(SessionConnections::new()));
    app
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

fn invoke_err(
    window: &tauri::WebviewWindow<tauri::test::MockRuntime>,
    cmd: &str,
    payload: serde_json::Value,
) -> String {
    let res = get_ipc_response(
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

#[tokio::test(flavor = "multi_thread")]
async fn session_switch_provider_rejects_empty_provider_id() {
    let app = make_app();
    let window = make_session_window(&app, TEST_SESSION);
    let err = invoke_err(
        &window,
        "session_switch_provider",
        serde_json::json!({
            "sessionId": TEST_SESSION,
            "providerId": "",
        }),
    );
    assert!(
        err.contains("provider_id is empty"),
        "expected empty-rejection, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn session_switch_provider_rejects_oversize_provider_id() {
    let app = make_app();
    let window = make_session_window(&app, TEST_SESSION);
    // `validate_provider_id` enforces `MAX_PROVIDER_ID_BYTES` (128). Use
    // 256 to clear that cap with comfortable headroom.
    let huge = "x".repeat(256);
    let err = invoke_err(
        &window,
        "session_switch_provider",
        serde_json::json!({
            "sessionId": TEST_SESSION,
            "providerId": huge,
        }),
    );
    assert!(
        err.contains("provider_id too large"),
        "expected size-cap rejection, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn session_switch_provider_passes_validation_for_realistic_id() {
    // Valid `provider_id` clears validation. The downstream bridge call
    // then fails because no UDS connection is registered for the test
    // session — that's the "no active connection" surface, not the
    // size/empty guard. Pinning this distinguishes the validator from
    // the bridge in case either error string drifts.
    let app = make_app();
    let window = make_session_window(&app, TEST_SESSION);
    let err = invoke_err(
        &window,
        "session_switch_provider",
        serde_json::json!({
            "sessionId": TEST_SESSION,
            "providerId": "anthropic",
        }),
    );
    assert!(
        !err.contains("provider_id is empty") && !err.contains("provider_id too large"),
        "validator should accept 'anthropic'; got: {err}"
    );
}
