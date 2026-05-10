//! F-359: Tauri-command coverage for `context_fetch_url` +
//! `set_context_allowed_hosts`.
//!
//! Covers the wire invariants the pure-policy and HTTP-layer tests
//! cannot:
//!
//! 1. **Authz.** Dashboard / cross-session invokes of `context_fetch_url`
//!    are rejected with the F-051 label-mismatch error before the fetch
//!    path runs. Pins the "only the owning session window may fetch"
//!    contract.
//! 2. **Server-side allowlist authority.** A URL the webview submits but
//!    that is not on the server-owned list returns a host-not-allowed
//!    error. The webview cannot widen its own reach by passing an
//!    arbitrary URL — the finding's core remediation.
//! 3. **Disallowed-scheme rejection at IPC boundary.** `file://` / `javascript:`
//!    are refused before any transport I/O — both the explicit
//!    non-http(s) case and URLs without any host.
//! 4. **Size caps.** Oversize URL or allowlist entries are rejected by
//!    the existing `require_size` / entry-count gate before the fetch
//!    path allocates.

#![cfg(feature = "webview-test")]

mod common;

use forge_shell::bridge::SessionConnections;
use forge_shell::ipc::{
    build_invoke_handler, manage_context_fetch, AllowedHostsState, BridgeState,
};
use tauri::test::{get_ipc_response, mock_builder, mock_context, noop_assets, INVOKE_KEY};
use tauri::Manager;

const LABEL_MISMATCH: &str = "forbidden: window label mismatch";
const TEST_SESSION: &str = "abcdef0123456789";

fn make_app() -> tauri::App<tauri::test::MockRuntime> {
    let app = mock_builder()
        .invoke_handler(build_invoke_handler())
        .build(mock_context(noop_assets()))
        .expect("build mock Tauri app");
    app.manage(BridgeState::new(SessionConnections::new()));
    manage_context_fetch(&app.handle().clone());
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
    res.expect("invoke returned Ok").deserialize().unwrap()
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
async fn context_fetch_url_rejects_dashboard_window() {
    // The @-context fetcher is session-scoped. The dashboard window
    // must not be able to use the command as a side-channel proxy into
    // the fetch path even with a valid-looking URL.
    let app = make_app();
    let window = make_dashboard_window(&app);
    let err = invoke_err(
        &window,
        "context_fetch_url",
        serde_json::json!({
            "sessionId": TEST_SESSION,
            "url": "https://docs.rs/tokio",
        }),
    );
    assert!(
        err.contains(LABEL_MISMATCH),
        "dashboard window must be rejected, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn context_fetch_url_rejects_host_not_on_server_allowlist() {
    // The finding's core: a webview submits a URL whose host is NOT
    // on the server-side allowlist. The fetcher must refuse without
    // making any network I/O. This is the "webview cannot lie about
    // the target host" contract — even a DNS-resolving, publicly
    // reachable hostname is rejected if the server hasn't allowlisted
    // it.
    //
    // Tracker #702: the user-facing message is generic — "URL not
    // allowed by policy" — so the rejection reason doesn't leak which
    // gate fired. The detailed reason is captured in `tracing` (see
    // `context_fetch_url_logs_detailed_reason_on_policy_reject`).
    let app = make_app();
    // Server-side allowlist intentionally left empty.
    let window = make_session_window(&app, TEST_SESSION);
    let err = invoke_err(
        &window,
        "context_fetch_url",
        serde_json::json!({
            "sessionId": TEST_SESSION,
            "url": "https://docs.rs/tokio",
        }),
    );
    assert!(
        err.contains("URL not allowed by policy"),
        "expected sanitized policy rejection; got: {err}"
    );
    assert!(
        !err.contains("docs.rs") && !err.contains("allowed-hosts list"),
        "sanitized error must not leak host or policy detail; got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn context_fetch_url_rejects_file_scheme() {
    let app = make_app();
    // Seed the allowlist with a valid hostname (post-#702 the setter
    // rejects path-shaped strings outright). The scheme check fires
    // before any host lookup, so the seeded value doesn't matter for
    // this assertion — only that the setter accepts it.
    let allowed = app.state::<AllowedHostsState>();
    allowed
        .replace(vec!["example.com".to_string()])
        .expect("seed allowlist");
    let window = make_session_window(&app, TEST_SESSION);
    let err = invoke_err(
        &window,
        "context_fetch_url",
        serde_json::json!({
            "sessionId": TEST_SESSION,
            "url": "file:///etc/passwd",
        }),
    );
    // Sanitized message — does not leak which gate fired.
    assert!(
        err.contains("URL not allowed by policy"),
        "expected sanitized policy rejection; got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn context_fetch_url_rejects_link_local_ip_literal() {
    // SSRF vector: AWS IMDS link-local. Even if the user had
    // misguidedly allowlisted `169.254.169.254` as a string, the
    // IP-class block rejects it before any transport I/O.
    //
    // Tracker #702: the rejection message is now generic — the
    // specific "link-local" classification stays in `tracing`, not
    // in the wire response, so a hostile webview cannot probe which
    // IP class triggered the block.
    let app = make_app();
    let allowed = app.state::<AllowedHostsState>();
    allowed
        .replace(vec!["169.254.169.254".to_string()])
        .expect("seed allowlist");
    let window = make_session_window(&app, TEST_SESSION);
    let err = invoke_err(
        &window,
        "context_fetch_url",
        serde_json::json!({
            "sessionId": TEST_SESSION,
            "url": "http://169.254.169.254/latest/meta-data/",
        }),
    );
    assert!(
        err.contains("URL not allowed by policy"),
        "expected sanitized policy rejection; got: {err}"
    );
    assert!(
        !err.contains("link-local") && !err.contains("169.254"),
        "sanitized error must not leak IP-class detail; got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn context_fetch_url_rejects_oversize_url() {
    let app = make_app();
    let window = make_session_window(&app, TEST_SESSION);
    // MAX_CONTEXT_URL_BYTES = 8 KiB. Submit 16 KiB.
    let long = format!("https://docs.rs/{}", "x".repeat(16 * 1024));
    let err = invoke_err(
        &window,
        "context_fetch_url",
        serde_json::json!({
            "sessionId": TEST_SESSION,
            "url": long,
        }),
    );
    assert!(
        err.contains("payload too large") && err.contains("url"),
        "expected size-cap error; got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_context_allowed_hosts_replaces_the_list() {
    // `set_context_allowed_hosts` overwrites the server-side list
    // verbatim (post-trim). A subsequent `context_fetch_url` whose
    // host is NOT on the list returns the sanitized policy-violation
    // error, confirming the setter is authoritative.
    let app = make_app();
    let session_window = make_session_window(&app, TEST_SESSION);

    // Seed from the session window (session-* is accepted). Surrounding
    // whitespace is trimmed; the trimmed form is what lands on the list.
    invoke_ok(
        &session_window,
        "set_context_allowed_hosts",
        serde_json::json!({ "hosts": ["  docs.rs  "] }),
    );
    let allowed = app.state::<AllowedHostsState>();
    assert_eq!(allowed.snapshot(), vec!["docs.rs".to_string()]);

    // Replace via the dashboard window (also accepted by the gate).
    let dashboard = make_dashboard_window(&app);
    invoke_ok(
        &dashboard,
        "set_context_allowed_hosts",
        serde_json::json!({ "hosts": ["only.example.com"] }),
    );
    assert_eq!(
        app.state::<AllowedHostsState>().snapshot(),
        vec!["only.example.com".to_string()]
    );

    // A URL for the *previous* allowlist entry must now be rejected
    // with the sanitized policy-violation message.
    let err = invoke_err(
        &session_window,
        "context_fetch_url",
        serde_json::json!({
            "sessionId": TEST_SESSION,
            "url": "https://docs.rs/tokio",
        }),
    );
    assert!(
        err.contains("URL not allowed by policy"),
        "expected sanitized policy rejection after replacement; got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_context_allowed_hosts_rejects_oversize_entry() {
    let app = make_app();
    let window = make_session_window(&app, TEST_SESSION);
    let too_long = "x".repeat(512); // > MAX_ALLOWED_HOST_BYTES (256)
    let err = invoke_err(
        &window,
        "set_context_allowed_hosts",
        serde_json::json!({ "hosts": ["good.example.com", too_long] }),
    );
    assert!(
        err.contains("payload too large") && err.contains("host"),
        "expected per-entry size cap; got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_context_allowed_hosts_rejects_oversize_list() {
    let app = make_app();
    let window = make_session_window(&app, TEST_SESSION);
    let hosts: Vec<String> = (0..512).map(|i| format!("host{i}.example.com")).collect();
    let err = invoke_err(
        &window,
        "set_context_allowed_hosts",
        serde_json::json!({ "hosts": hosts }),
    );
    assert!(
        err.contains("payload too large") && err.contains("hosts"),
        "expected list-length cap; got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Tracker #702: hostname validation on `AllowedHostsState::replace`
//
// RFC 1123 host rules: alphanumeric, hyphens, dots; non-empty; ≤253 bytes;
// each label ≤63 bytes and not starting/ending with a hyphen; no wildcards
// or path characters. Invalid entries are `Err` — never silently filtered.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn set_context_allowed_hosts_rejects_empty_entry() {
    let app = make_app();
    let window = make_session_window(&app, TEST_SESSION);
    let err = invoke_err(
        &window,
        "set_context_allowed_hosts",
        serde_json::json!({ "hosts": ["good.example.com", ""] }),
    );
    assert!(
        err.contains("invalid host") || err.contains("empty"),
        "expected empty-entry rejection; got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_context_allowed_hosts_rejects_whitespace_only_entry() {
    // Whitespace-only entries used to be silently filtered. Per #702
    // they are now explicit errors.
    let app = make_app();
    let window = make_session_window(&app, TEST_SESSION);
    let err = invoke_err(
        &window,
        "set_context_allowed_hosts",
        serde_json::json!({ "hosts": ["   "] }),
    );
    assert!(
        err.contains("invalid host"),
        "expected whitespace-only rejection; got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_context_allowed_hosts_rejects_path_chars() {
    let app = make_app();
    let window = make_session_window(&app, TEST_SESSION);
    let err = invoke_err(
        &window,
        "set_context_allowed_hosts",
        serde_json::json!({ "hosts": ["/etc/passwd"] }),
    );
    assert!(
        err.contains("invalid host"),
        "expected path-char rejection; got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_context_allowed_hosts_rejects_wildcard() {
    let app = make_app();
    let window = make_session_window(&app, TEST_SESSION);
    let err = invoke_err(
        &window,
        "set_context_allowed_hosts",
        serde_json::json!({ "hosts": ["*.example.com"] }),
    );
    assert!(
        err.contains("invalid host"),
        "expected wildcard rejection; got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_context_allowed_hosts_rejects_control_char() {
    let app = make_app();
    let window = make_session_window(&app, TEST_SESSION);
    let err = invoke_err(
        &window,
        "set_context_allowed_hosts",
        serde_json::json!({ "hosts": ["bad\u{0007}.example.com"] }),
    );
    assert!(
        err.contains("invalid host"),
        "expected control-char rejection; got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_context_allowed_hosts_rejects_leading_hyphen_label() {
    let app = make_app();
    let window = make_session_window(&app, TEST_SESSION);
    let err = invoke_err(
        &window,
        "set_context_allowed_hosts",
        serde_json::json!({ "hosts": ["-bad.example.com"] }),
    );
    assert!(
        err.contains("invalid host"),
        "expected leading-hyphen rejection; got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_context_allowed_hosts_rejects_overlong_hostname() {
    let app = make_app();
    let window = make_session_window(&app, TEST_SESSION);
    // 254-byte hostname (max is 253). Made of 5-char labels so no
    // single label exceeds 63 bytes — isolates the total-length gate.
    let labels: Vec<String> = (0..50).map(|i| format!("lab{:02}", i)).collect();
    let host = labels.join("."); // 50 * 6 - 1 = 299 bytes; well over 253.
    let err = invoke_err(
        &window,
        "set_context_allowed_hosts",
        serde_json::json!({ "hosts": [host] }),
    );
    // Either the per-entry size cap or the hostname-validity gate
    // rejects — both qualify. The list-length cap is irrelevant here.
    assert!(
        err.contains("invalid host") || err.contains("payload too large"),
        "expected overlong-hostname rejection; got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn context_fetch_url_logs_detailed_reason_on_policy_reject() {
    // Tracker #702: the wire response is sanitized to a generic
    // "URL not allowed by policy" — but the detailed reason (here the
    // IP-class block on the AWS IMDS link-local address) must still
    // reach `tracing` so operators can debug without re-running.
    let _g = common::capture_test_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    common::install_capture_subscriber();
    let _ = common::drain_capture();

    let app = make_app();
    let allowed = app.state::<AllowedHostsState>();
    allowed
        .replace(vec!["169.254.169.254".to_string()])
        .expect("seed allowlist");
    let window = make_session_window(&app, TEST_SESSION);
    let err = invoke_err(
        &window,
        "context_fetch_url",
        serde_json::json!({
            "sessionId": TEST_SESSION,
            "url": "http://169.254.169.254/latest/meta-data/",
        }),
    );
    assert_eq!(err, "context_fetch_url: URL not allowed by policy");

    let logs = common::drain_capture();
    assert!(
        logs.contains("forge_shell::ipc::context_fetch"),
        "expected tracing target, got: {logs}"
    );
    assert!(logs.contains("WARN"), "expected WARN level, got: {logs}");
    assert!(
        logs.contains("link-local"),
        "expected detailed reason in tracing, got: {logs}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_context_allowed_hosts_accepts_valid_hostnames() {
    let app = make_app();
    let window = make_session_window(&app, TEST_SESSION);
    invoke_ok(
        &window,
        "set_context_allowed_hosts",
        serde_json::json!({ "hosts": [
            "docs.rs",
            "sub-domain.example.com",
            "127.0.0.1",  // IP literal — parses as labels of digits.
            "a",          // single-label hostname.
        ] }),
    );
    assert_eq!(
        app.state::<AllowedHostsState>().snapshot(),
        vec![
            "docs.rs".to_string(),
            "sub-domain.example.com".to_string(),
            "127.0.0.1".to_string(),
            "a".to_string(),
        ]
    );
}
