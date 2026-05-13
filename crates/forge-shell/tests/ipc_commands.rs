//! Tauri `ipc` command surface tests for F-020 and F-052.
//!
//! F-020: `build_invoke_handler()` and the five `#[tauri::command]` handlers
//! compile and register together; `session_hello` round-trips against a real
//! daemon via `tauri::test::get_ipc_response`.
//!
//! F-052 (H11 / T7): the production `session_hello` command does **not**
//! accept an arbitrary `socketPath` — any value passed by a webview caller is
//! ignored. The round-trip test uses a `webview-test`-gated
//! [`BridgeState::with_test_socket_override`] constructor instead of a
//! public parameter; the regression test spins up a rogue listener and
//! asserts it never receives a connection even when its path is injected
//! via the `socketPath` JSON field.

#![cfg(feature = "webview-test")]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use forge_providers::MockProvider;
use forge_session::server::serve_with_session;
use forge_session::session::Session;
use forge_shell::bridge::SessionConnections;
use forge_shell::ipc::{build_invoke_handler, BridgeState};
use tauri::test::{mock_builder, mock_context, noop_assets, INVOKE_KEY};
use tauri::Manager;
use tempfile::TempDir;
use tokio::net::UnixListener;

fn make_app() -> tauri::App<tauri::test::MockRuntime> {
    mock_builder()
        .invoke_handler(build_invoke_handler())
        .build(mock_context(noop_assets()))
        .expect("build mock Tauri app")
}

#[test]
fn invoke_handler_builds_without_error() {
    let app = make_app();
    // Attach fresh bridge state so commands have something to resolve against.
    app.manage(BridgeState::new(SessionConnections::new()));
    // Nothing else — this test just proves `build_invoke_handler()` and the
    // five `#[tauri::command]` handlers compile and register together.
    drop(app);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_hello_command_round_trips_via_tauri_invoke() {
    let sock_dir = TempDir::new().unwrap();
    let sock = sock_dir.path().join("tauri-hello.sock");

    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());
    let provider = Arc::new(MockProvider::with_default_path());
    let server_sock = sock.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            session,
            provider,
            true,
            false,
            None,
            Some("tauri-hello".to_string()),
            None, // F-587: keyless test wiring
            None, // F-601: no active agent — memory off in this test
            None,
            None, // F-752
        )
        .await
        .unwrap();
    });
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let app = make_app();
    // F-052: production `session_hello` no longer accepts a `socketPath`
    // parameter — the test daemon's path is wired through a test-only
    // constructor on `BridgeState` gated behind the `webview-test` feature.
    app.manage(BridgeState::with_test_socket_override(
        SessionConnections::new(),
        sock.clone(),
    ));

    // F-051: window label must match `session-{session_id}` for the
    // session_hello authz check to pass.
    let window = tauri::WebviewWindowBuilder::new(
        &app,
        "session-tauri-hello",
        tauri::WebviewUrl::App("index.html".into()),
    )
    .build()
    .expect("mock window");

    let payload = serde_json::json!({ "sessionId": "tauri-hello" });
    let res = tauri::test::get_ipc_response(
        &window,
        tauri::webview::InvokeRequest {
            cmd: "session_hello".into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: "http://tauri.localhost".parse().unwrap(),
            body: tauri::ipc::InvokeBody::Json(payload),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    );

    let value = res.expect("session_hello returns HelloAck JSON");
    let obj = value.deserialize::<serde_json::Value>().unwrap();
    assert_eq!(obj["session_id"], "tauri-hello");
    assert_eq!(obj["schema_version"], 1);
}

/// F-052 regression: a webview that injects an attacker-controlled
/// `socketPath` into the `session_hello` invoke payload must not cause the
/// shell to connect to that path. We bind a rogue `UnixListener` at a
/// tempdir path, invoke `session_hello` with `socketPath` set to that path,
/// and assert the rogue listener never accepts a connection. The command
/// should instead attempt the default path (which has no daemon) and fail.
#[tokio::test(flavor = "multi_thread")]
async fn session_hello_ignores_attacker_supplied_socket_path() {
    let rogue_dir = TempDir::new().unwrap();
    let rogue_sock = rogue_dir.path().join("attacker.sock");
    let rogue_listener = UnixListener::bind(&rogue_sock).expect("bind rogue UDS");

    let accept_count = Arc::new(AtomicU32::new(0));
    let accept_count_bg = Arc::clone(&accept_count);
    let accept_task = tokio::spawn(async move {
        // If the shell ever connects here, record it. The test must see 0.
        if let Ok((_stream, _addr)) = rogue_listener.accept().await {
            accept_count_bg.fetch_add(1, Ordering::SeqCst);
        }
    });

    let app = make_app();
    // Production path: no test override → default_socket_path will be used.
    app.manage(BridgeState::new(SessionConnections::new()));

    let window = tauri::WebviewWindowBuilder::new(
        &app,
        "session-attacker",
        tauri::WebviewUrl::App("index.html".into()),
    )
    .build()
    .expect("mock window");

    let payload = serde_json::json!({
        "sessionId": "attacker",
        // Injected by a webview caller. With F-052 this field is unknown
        // to the command and silently ignored by serde.
        "socketPath": rogue_sock.to_string_lossy(),
    });
    let res = tauri::test::get_ipc_response(
        &window,
        tauri::webview::InvokeRequest {
            cmd: "session_hello".into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: "http://tauri.localhost".parse().unwrap(),
            body: tauri::ipc::InvokeBody::Json(payload),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    );

    // No daemon at the default path → the call fails. The security-meaningful
    // assertion is that the rogue listener never accepted.
    assert!(
        res.is_err(),
        "session_hello must not succeed when no daemon is listening at the default path"
    );

    // Give the accept task a brief window to (not) fire.
    tokio::time::sleep(Duration::from_millis(100)).await;
    accept_task.abort();
    let _ = accept_task.await;

    assert_eq!(
        accept_count.load(Ordering::SeqCst),
        0,
        "rogue listener must not receive any connection from session_hello"
    );
}

// F-068 / L4 (T7): size caps on untyped-string fields in session commands.
//
// These tests assert that oversize payloads are rejected by the *command*
// layer (before the bridge is consulted), not by the `forge_ipc` 4 MiB wire
// cap. The discriminator: with an empty `SessionConnections` registry, the
// bridge layer would respond with "no active connection for session …".
// If the size check fires first, we instead see our size-cap error marker.
//
// The at-cap tests invert the assertion: a payload sized exactly at the cap
// must pass the size check (still error later with "no active connection"),
// proving the boundary is inclusive and the size check alone is not swallowing
// every call.

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
    .expect("mock window")
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

#[test]
fn session_send_message_rejects_text_above_cap_at_command_layer() {
    // 1 MiB is well under `forge_ipc`'s 4 MiB wire cap, so any rejection must
    // originate from the command layer's size check, not the wire.
    let app = make_app();
    app.manage(BridgeState::new(SessionConnections::new()));
    let window = make_session_window(&app, "oversize");

    let err = invoke_err(
        &window,
        "session_send_message",
        serde_json::json!({
            "sessionId": "oversize",
            "text": "A".repeat(1024 * 1024),
        }),
    );

    // The load-bearing assertion: rejection must be the size cap, not the
    // bridge's "no active connection" — the latter would prove the size
    // check was absent.
    assert!(
        err.contains("payload too large") && err.contains("text"),
        "expected size-cap error mentioning text, got: {err}"
    );
    assert!(
        !err.contains("no active connection"),
        "size check must fire before the bridge, got: {err}"
    );
}

#[test]
fn session_send_message_accepts_text_exactly_at_cap() {
    // 128 KiB — the inclusive boundary. The size check must let this through
    // (the call will still fail, but at the bridge with "no active connection",
    // proving the size gate didn't swallow it).
    let app = make_app();
    app.manage(BridgeState::new(SessionConnections::new()));
    let window = make_session_window(&app, "at-cap");

    let err = invoke_err(
        &window,
        "session_send_message",
        serde_json::json!({
            "sessionId": "at-cap",
            "text": "A".repeat(128 * 1024),
        }),
    );

    assert!(
        err.contains("no active connection"),
        "128 KiB text must pass size check and reach the bridge, got: {err}"
    );
    assert!(
        !err.contains("payload too large"),
        "128 KiB must not be rejected by the size cap, got: {err}"
    );
}

#[test]
fn session_approve_tool_rejects_tool_call_id_above_cap_at_command_layer() {
    let app = make_app();
    app.manage(BridgeState::new(SessionConnections::new()));
    let window = make_session_window(&app, "tcid");

    let err = invoke_err(
        &window,
        "session_approve_tool",
        serde_json::json!({
            "sessionId": "tcid",
            "toolCallId": "x".repeat(65),
            "scope": "ThisTool",
        }),
    );

    assert!(
        err.contains("payload too large") && err.contains("tool_call_id"),
        "expected size-cap error mentioning tool_call_id, got: {err}"
    );
    assert!(
        !err.contains("no active connection"),
        "size check must fire before the bridge, got: {err}"
    );
}

// F-069 / L5 (T7) removed the byte-cap test for `scope` above — `scope` is
// now typed as `forge_core::ApprovalScope`, so any oversize/non-variant input
// is rejected by serde at the Tauri arg-deserialization layer. The
// positive/negative coverage lives in `session_approve_tool_rejects_garbage_scope_at_command_layer`
// and `session_approve_tool_accepts_valid_scope_variants` below.

#[test]
fn session_reject_tool_rejects_tool_call_id_above_cap_at_command_layer() {
    let app = make_app();
    app.manage(BridgeState::new(SessionConnections::new()));
    let window = make_session_window(&app, "rej");

    let err = invoke_err(
        &window,
        "session_reject_tool",
        serde_json::json!({
            "sessionId": "rej",
            "toolCallId": "x".repeat(65),
            "reason": null,
        }),
    );

    assert!(
        err.contains("payload too large") && err.contains("tool_call_id"),
        "expected size-cap error mentioning tool_call_id, got: {err}"
    );
    assert!(
        !err.contains("no active connection"),
        "size check must fire before the bridge, got: {err}"
    );
}

#[test]
fn session_reject_tool_rejects_reason_above_cap_at_command_layer() {
    let app = make_app();
    app.manage(BridgeState::new(SessionConnections::new()));
    let window = make_session_window(&app, "rej-reason");

    let err = invoke_err(
        &window,
        "session_reject_tool",
        serde_json::json!({
            "sessionId": "rej-reason",
            "toolCallId": "tc-1",
            "reason": "x".repeat(1025),
        }),
    );

    assert!(
        err.contains("payload too large") && err.contains("reason"),
        "expected size-cap error mentioning reason, got: {err}"
    );
    assert!(
        !err.contains("no active connection"),
        "size check must fire before the bridge, got: {err}"
    );
}

#[test]
fn session_reject_tool_accepts_absent_reason() {
    // `reason` is `Option<String>` — None must not trigger the size check.
    let app = make_app();
    app.manage(BridgeState::new(SessionConnections::new()));
    let window = make_session_window(&app, "rej-none");

    let err = invoke_err(
        &window,
        "session_reject_tool",
        serde_json::json!({
            "sessionId": "rej-none",
            "toolCallId": "tc-1",
            "reason": null,
        }),
    );

    assert!(
        err.contains("no active connection"),
        "None reason must skip size check and reach the bridge, got: {err}"
    );
    assert!(
        !err.contains("payload too large"),
        "None reason must not be rejected by the size cap, got: {err}"
    );
}

// F-069 / L5 (T7): typed-enum validation on `scope`.
//
// `scope` is declared as `forge_core::ApprovalScope` on the Tauri command, so
// Tauri's arg-deserialization layer rejects any non-variant string before the
// command body runs — earlier than `require_window_label`, earlier than the
// F-068 byte caps, earlier than the bridge. A garbage string ("not-a-scope")
// cannot reach the bridge, which defends against type-confusion widening of
// approvals by a compromised webview.

#[test]
fn session_approve_tool_rejects_garbage_scope_at_command_layer() {
    let app = make_app();
    app.manage(BridgeState::new(SessionConnections::new()));
    let window = make_session_window(&app, "garbage-scope");

    let err = invoke_err(
        &window,
        "session_approve_tool",
        serde_json::json!({
            "sessionId": "garbage-scope",
            "toolCallId": "tc-1",
            "scope": "garbage",
        }),
    );

    // The load-bearing assertions: rejection must be serde-level (typed-enum
    // validation fires before the bridge is consulted). If the call reached
    // the bridge, we would see "no active connection" instead.
    assert!(
        !err.contains("no active connection"),
        "typed-enum validation must fire before the bridge, got: {err}"
    );
    assert!(
        err.contains("scope"),
        "error must surface the offending field `scope`, got: {err}"
    );
}

#[test]
fn session_approve_tool_accepts_valid_scope_variants() {
    // Every TS-side ApprovalScope variant must round-trip through
    // Tauri arg-deserialization and reach the bridge (which will then error
    // with "no active connection" — the security-relevant evidence that the
    // typed enum accepted the value).
    let app = make_app();
    app.manage(BridgeState::new(SessionConnections::new()));
    let window = make_session_window(&app, "valid-scope");

    for variant in ["Once", "ThisFile", "ThisPattern", "ThisTool"] {
        let err = invoke_err(
            &window,
            "session_approve_tool",
            serde_json::json!({
                "sessionId": "valid-scope",
                "toolCallId": "tc-1",
                "scope": variant,
            }),
        );
        assert!(
            err.contains("no active connection"),
            "variant {variant:?} must pass enum validation and reach the bridge, got: {err}"
        );
    }
}

// F-391: `session_cancel` is the Tauri command wired to the Composer's Stop
// button and Esc handler. The daemon-side cancel transport is a separate
// follow-up — this task establishes the command surface and its window-
// ownership check so the frontend has a stable target to invoke.

#[test]
fn session_cancel_is_registered_and_succeeds_for_the_owning_window() {
    let app = make_app();
    app.manage(BridgeState::new(SessionConnections::new()));
    let window = make_session_window(&app, "cancel-ok");

    let res = tauri::test::get_ipc_response(
        &window,
        tauri::webview::InvokeRequest {
            cmd: "session_cancel".into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: "http://tauri.localhost".parse().unwrap(),
            body: tauri::ipc::InvokeBody::Json(serde_json::json!({ "sessionId": "cancel-ok" })),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    );
    res.expect("session_cancel must succeed for the matching window label");
}

#[test]
fn session_cancel_rejects_cross_window_callers() {
    let app = make_app();
    app.manage(BridgeState::new(SessionConnections::new()));
    // Window labelled for *another* session — the authz check must block it.
    let window = make_session_window(&app, "owned-by-a");

    let err = invoke_err(
        &window,
        "session_cancel",
        serde_json::json!({ "sessionId": "victim-b" }),
    );
    assert!(
        err.contains("window label mismatch"),
        "cross-window session_cancel must be rejected, got: {err}"
    );
}
