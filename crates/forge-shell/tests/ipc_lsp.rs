//! F-123 IPC integration tests for the `lsp_*` Tauri commands.
//!
//! Exercises the full invoke → `forge-lsp::Server` spawn → message-event
//! emit path via `tauri::test::get_ipc_response`. Mirrors the F-125
//! `ipc_terminal.rs` shape:
//!
//! - authz: dashboard is rejected.
//! - spawn binds the server to the calling webview's label.
//! - cross-session send/stop is rejected with label-mismatch.
//! - a round-trip via the stub LSP fixture proves the message event reaches
//!   the owner webview (and only the owner).
//!
//! F-353: `lsp_start` no longer accepts a `binary_path`. The webview names
//! a server id only; the shell resolves the binary through the managed
//! `forge_lsp::Bootstrap` + `Registry`. Tests override the managed
//! bootstrap with a tempdir-rooted one whose registry lists every id each
//! test uses, seeding the fixture binary at the cache-root path the
//! production resolution would land on (`<cache_root>/<id>/<binary_name>`).

#![cfg(feature = "webview-test")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_lsp::{Bootstrap, Checksum, Registry, ServerId, ServerSpec};
use forge_shell::ipc::{build_invoke_handler, manage_lsp, LspBootstrapState};
use serde_json::Value;
use tauri::test::{get_ipc_response, mock_builder, mock_context, noop_assets, INVOKE_KEY};
use tauri::{Listener, Manager};

const LABEL_MISMATCH: &str = "forbidden: window label mismatch";

/// Path to the in-tree `forge-lsp-mock-stdio` fixture binary.
fn mock_stdio_path() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    // target/debug/deps/ipc_lsp-HASH → target/debug/
    let mut dir = exe
        .parent()
        .and_then(|p| p.parent())
        .expect("target/debug dir")
        .to_path_buf();
    dir.push("forge-lsp-mock-stdio");
    if cfg!(windows) {
        dir.set_extension("exe");
    }
    assert!(
        dir.exists(),
        "fixture must be built; run `cargo build -p forge-lsp --bin forge-lsp-mock-stdio`. \
         missing: {}",
        dir.display()
    );
    dir
}

/// `NoopDownloader` proves the production path doesn't hit the network
/// during these tests — `Server::from_registry` does not download, it only
/// resolves a cached path.
struct NoopDownloader;
#[async_trait::async_trait]
impl forge_lsp::Downloader for NoopDownloader {
    async fn fetch_into(
        &self,
        _url: &str,
        _sink: &mut (dyn tokio::io::AsyncWrite + Send + Unpin),
    ) -> Result<[u8; 32], Box<dyn std::error::Error + Send + Sync>> {
        unreachable!("lsp_start must not hit the network in tests");
    }
}

/// Seed a tempdir cache root with the mock fixture at
/// `<cache_root>/<id>/<binary_name>` for every id in `ids`, and build a
/// `Bootstrap` whose single-purpose registry names those ids. Returns the
/// owned `TempDir` so the caller can keep the cache alive for the test
/// body.
fn seed_fixture_bootstrap(
    ids: &[&'static str],
    binary_name: &'static str,
) -> (tempfile::TempDir, Arc<Bootstrap>, Vec<&'static str>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = mock_stdio_path();

    for id in ids {
        let dst_dir = tmp.path().join(id);
        std::fs::create_dir_all(&dst_dir).expect("create server dir");
        let dst = dst_dir.join(binary_name);
        std::fs::copy(&src, &dst).expect("copy fixture");
        set_executable(&dst);
    }

    let entries: Vec<ServerSpec> = ids
        .iter()
        .map(|id| ServerSpec {
            id: ServerId(id),
            language_id: "mock",
            binary_name,
            download_url: "http://example.invalid/",
            checksum: Checksum::Pending,
        })
        .collect();
    let leaked: &'static [ServerSpec] = Box::leak(entries.into_boxed_slice());
    let registry = Registry::from_entries(leaked);

    let bootstrap =
        Bootstrap::with_registry(tmp.path().to_path_buf(), Box::new(NoopDownloader), registry);
    (tmp, Arc::new(bootstrap), ids.to_vec())
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}

struct TestApp {
    app: tauri::App<tauri::test::MockRuntime>,
    _cache: tempfile::TempDir,
}

impl TestApp {
    fn handle(&self) -> tauri::AppHandle<tauri::test::MockRuntime> {
        self.app.handle().clone()
    }
}

fn make_app_with_bootstrap(ids: &[&'static str]) -> TestApp {
    let (cache, bootstrap, _ids) = seed_fixture_bootstrap(ids, "mock-stdio");
    let app = mock_builder()
        .invoke_handler(build_invoke_handler())
        .build(mock_context(noop_assets()))
        .expect("build mock Tauri app");
    manage_lsp(&app.handle().clone());
    app.handle()
        .state::<LspBootstrapState>()
        .override_for_tests(bootstrap);
    TestApp { app, _cache: cache }
}

/// Build an app with the managed `LspBootstrap` wired to an empty-registry
/// tempdir — used by tests that assert the IPC rejects arbitrary paths
/// and unknown server ids without ever reaching the fixture.
fn make_app_empty_registry() -> TestApp {
    let tmp = tempfile::tempdir().expect("tempdir");
    let registry = Registry::from_entries(&[]);
    let bootstrap = Arc::new(Bootstrap::with_registry(
        tmp.path().to_path_buf(),
        Box::new(NoopDownloader),
        registry,
    ));
    let app = mock_builder()
        .invoke_handler(build_invoke_handler())
        .build(mock_context(noop_assets()))
        .expect("build mock Tauri app");
    manage_lsp(&app.handle().clone());
    app.handle()
        .state::<LspBootstrapState>()
        .override_for_tests(bootstrap);
    TestApp { app, _cache: tmp }
}

fn make_window(
    app: &tauri::App<tauri::test::MockRuntime>,
    label: &str,
) -> tauri::WebviewWindow<tauri::test::MockRuntime> {
    tauri::WebviewWindowBuilder::new(app, label, tauri::WebviewUrl::App("index.html".into()))
        .build()
        .expect("mock window")
}

fn invoke(
    window: &tauri::WebviewWindow<tauri::test::MockRuntime>,
    cmd: &str,
    payload: serde_json::Value,
) -> Result<tauri::ipc::InvokeResponseBody, String> {
    get_ipc_response(
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
    )
    .map_err(|v| match v {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------

#[test]
fn dashboard_window_cannot_start_an_lsp_server() {
    let app = make_app_with_bootstrap(&["rust-analyzer"]);
    let window = make_window(&app.app, "dashboard");
    let err = invoke(
        &window,
        "lsp_start",
        serde_json::json!({
            "args": {
                "server": "rust-analyzer",
                "args": [],
            }
        }),
    )
    .expect_err("dashboard window must not start an lsp server");

    assert!(
        err.contains(LABEL_MISMATCH),
        "expected label-mismatch error, got: {err}"
    );
    let _ = app.handle();
}

#[test]
fn cross_session_send_is_rejected_with_label_mismatch() {
    // Alice starts an LSP server; Bob tries to send to it. The registry
    // binds the server to alice's label and must reject bob's invoke.
    let app = make_app_with_bootstrap(&["alice-srv"]);
    let alice = make_window(&app.app, "session-alice-lsp");
    let bob = make_window(&app.app, "session-bob-lsp");

    invoke(
        &alice,
        "lsp_start",
        serde_json::json!({
            "args": {
                "server": "alice-srv",
                "args": [],
            }
        }),
    )
    .expect("alice start");

    let err = invoke(
        &bob,
        "lsp_send",
        serde_json::json!({
            "server": "alice-srv",
            "message": {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}
        }),
    )
    .expect_err("bob must not send to alice's server");
    assert!(
        err.contains(LABEL_MISMATCH),
        "expected label-mismatch error for send, got: {err}"
    );

    let err = invoke(&bob, "lsp_stop", serde_json::json!({"server": "alice-srv"}))
        .expect_err("bob must not stop alice's server");
    assert!(
        err.contains(LABEL_MISMATCH),
        "expected label-mismatch error for stop, got: {err}"
    );

    // Clean up via the owner.
    invoke(
        &alice,
        "lsp_stop",
        serde_json::json!({"server": "alice-srv"}),
    )
    .expect("owner stop");
}

// ---------------------------------------------------------------------------
// F-353: arbitrary-binary-exec closure at the IPC boundary
// ---------------------------------------------------------------------------

#[test]
fn lsp_start_rejects_unknown_server_id() {
    // The webview names an id the shell has never heard of. With the raw-
    // PathBuf surface removed, there's no way to coax `Command::new` into
    // accepting the request — it bounces at the registry gate before any
    // path is resolved.
    let app = make_app_empty_registry();
    let window = make_window(&app.app, "session-lsp-unknown");
    let err = invoke(
        &window,
        "lsp_start",
        serde_json::json!({
            "args": {
                "server": "not-in-registry",
                "args": [],
            }
        }),
    )
    .expect_err("unknown server id must reject");
    assert!(
        err.contains("unknown lsp server"),
        "expected unknown-id error, got: {err}"
    );
}

#[test]
fn lsp_start_rejects_arbitrary_binary_path_field() {
    // Pre-F-353, the webview could supply `binary_path: "/usr/bin/ncat"`.
    // Post-fix the field is gone from the wire format: serde rejects the
    // unknown top-level key, or the id lookup fails, depending on how the
    // compromised webview frames the payload. Either way the invoke does
    // not spawn a child.
    let app = make_app_empty_registry();
    let window = make_window(&app.app, "session-lsp-path-inject");

    // Shape 1: keep the `binary_path` key even though the server type no
    // longer names it. Serde's default strategy rejects extra fields
    // because the struct has no `#[serde(deny_unknown_fields)]`-relaxing
    // option — but `serde_json` allows extras by default. So the *real*
    // post-fix guarantee is the registry-gate failure below, not a
    // deserialization failure. This test locks *that* guarantee.
    let err = invoke(
        &window,
        "lsp_start",
        serde_json::json!({
            "args": {
                "server": "arbitrary-binary-srv",
                "binary_path": "/usr/bin/ncat",
                "args": ["-lvp", "4444", "-e", "/bin/sh"],
            }
        }),
    )
    .expect_err("compromised payload must not spawn a child");
    assert!(
        err.contains("unknown lsp server"),
        "expected unknown-id rejection (registry gate), got: {err}"
    );
}

// ---------------------------------------------------------------------------
// DoD: initialize → lsp_message event → shutdown round trip via IPC
// ---------------------------------------------------------------------------

/// Subscription to `lsp_message` events on a webview. Registering the
/// listener **before** triggering the LSP roundtrip closes the race window
/// where an event arrives between `lsp_send` and a later listener attach
/// (the original cause of #816 flakes). The channel is signalled by Tauri's
/// listener callback, so waiters wake immediately on arrival instead of
/// polling on a fixed cadence.
struct LspMessageStream {
    rx: std::sync::mpsc::Receiver<Value>,
}

impl LspMessageStream {
    fn subscribe(window: &tauri::WebviewWindow<tauri::test::MockRuntime>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        // The listener handle is intentionally leaked: the listener lives
        // as long as the window does, and the channel sender drops when the
        // window's event loop tears down.
        let _id = window.listen("lsp_message", move |ev| {
            if let Ok(v) = serde_json::from_str::<Value>(ev.payload()) {
                let _ = tx.send(v);
            }
        });
        LspMessageStream { rx }
    }

    /// Block until at least one message arrives or `timeout` elapses.
    /// Returns every message received within the window so assertions can
    /// inspect the first frame's shape; subsequent frames are drained
    /// non-blockingly so the test doesn't lose them.
    fn collect_until_first(&self, timeout: Duration) -> Vec<Value> {
        let mut out = Vec::new();
        match self.rx.recv_timeout(timeout) {
            Ok(v) => out.push(v),
            Err(_) => return out,
        }
        // Drain anything else that arrived back-to-back without blocking.
        while let Ok(v) = self.rx.try_recv() {
            out.push(v);
        }
        out
    }
}

#[test]
fn initialize_round_trip_emits_lsp_message_on_owner_webview() {
    let app = make_app_with_bootstrap(&["mock-srv"]);
    let window = make_window(&app.app, "session-lsp-round");

    invoke(
        &window,
        "lsp_start",
        serde_json::json!({
            "args": {
                "server": "mock-srv",
                "args": [],
            }
        }),
    )
    .expect("lsp_start");

    // Subscribe BEFORE issuing the request. Pre-#816 the listener was
    // registered after `lsp_send`, opening a race: under loaded runners the
    // mock LSP could write its response before the test's listener attached,
    // so the event was emitted into the void and the assertion below fired.
    // Subscribing first guarantees every frame produced by the supervisor
    // forwarder lands in the channel.
    let stream = LspMessageStream::subscribe(&window);

    // Give the supervisor a moment to spawn the child + install stdin. The
    // transport returns `server not running` until then; retrying until
    // success mirrors the pattern in the forge-lsp integration test.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let attempt = invoke(
            &window,
            "lsp_send",
            serde_json::json!({
                "server": "mock-srv",
                "message": {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {}
                }
            }),
        );
        if attempt.is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            panic!("lsp_send never succeeded: {attempt:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Block on the listener channel — wakes immediately when the supervisor
    // forwarder emits the response. The 5s budget is a safety net for stuck
    // CI runners; the test normally returns within a few milliseconds.
    let messages = stream.collect_until_first(Duration::from_secs(5));
    assert!(
        !messages.is_empty(),
        "expected at least one lsp_message event"
    );

    // Verify shape: { server: "mock-srv", message: { id, result.capabilities } }
    let first = &messages[0];
    assert_eq!(
        first.get("server").and_then(|v| v.as_str()),
        Some("mock-srv"),
        "payload.server must match start arg; got {first}"
    );
    let msg = first.get("message").expect("payload.message present");
    assert_eq!(
        msg.get("id").and_then(|v| v.as_u64()),
        Some(1),
        "response id must match request"
    );
    assert!(
        msg.pointer("/result/capabilities").is_some(),
        "initialize result.capabilities missing: {msg}"
    );

    invoke(
        &window,
        "lsp_stop",
        serde_json::json!({"server": "mock-srv"}),
    )
    .expect("lsp_stop");
}

#[test]
fn duplicate_server_id_is_rejected() {
    let app = make_app_with_bootstrap(&["dup-srv"]);
    let window = make_window(&app.app, "session-lsp-dup");

    invoke(
        &window,
        "lsp_start",
        serde_json::json!({
            "args": {
                "server": "dup-srv",
                "args": [],
            }
        }),
    )
    .expect("first start");

    let err = invoke(
        &window,
        "lsp_start",
        serde_json::json!({
            "args": {
                "server": "dup-srv",
                "args": [],
            }
        }),
    )
    .expect_err("duplicate id must be rejected");
    assert!(
        err.contains("already running"),
        "expected duplicate-id error, got: {err}"
    );

    invoke(
        &window,
        "lsp_stop",
        serde_json::json!({"server": "dup-srv"}),
    )
    .expect("stop");
}

#[test]
fn oversize_message_is_rejected_at_command_layer() {
    // A 1 MiB payload exceeds MAX_LSP_MESSAGE_BYTES (512 KiB). The command
    // must reject before the transport touches stdin.
    let app = make_app_with_bootstrap(&["cap-srv"]);
    let window = make_window(&app.app, "session-lsp-cap");

    invoke(
        &window,
        "lsp_start",
        serde_json::json!({
            "args": {
                "server": "cap-srv",
                "args": [],
            }
        }),
    )
    .expect("start");

    let huge = "A".repeat(1024 * 1024);
    let err = invoke(
        &window,
        "lsp_send",
        serde_json::json!({
            "server": "cap-srv",
            "message": {"blob": huge}
        }),
    )
    .expect_err("oversize must be rejected");
    assert!(
        err.contains("payload too large") && err.contains("message"),
        "expected message size-cap error, got: {err}"
    );

    invoke(
        &window,
        "lsp_stop",
        serde_json::json!({"server": "cap-srv"}),
    )
    .expect("stop");
}

// ---------------------------------------------------------------------------
// F-374: current-state surface — `lsp_list` IPC
//
// Parity with `session_list_mcp_servers` (F-132; renamed from
// `list_mcp_servers` in F-591). A session webview can introspect its own
// live LSP servers in one `invoke` instead of reconstructing state from
// `lsp_message` history. Authz mirrors `lsp_send`: a caller only sees the
// servers its own window label owns.
// ---------------------------------------------------------------------------

#[test]
fn lsp_list_returns_empty_before_any_lsp_start() {
    // No servers started yet — the command must return an empty array, not
    // a label-mismatch error. The UI relies on "empty list" being a
    // legitimate shape so it can render the no-servers state.
    let app = make_app_with_bootstrap(&["rust-analyzer"]);
    let window = make_window(&app.app, "session-lsp-empty");

    let resp = invoke(&window, "lsp_list", serde_json::json!({})).expect("list");
    let body: Value = resp.deserialize().expect("deserialize body");
    assert_eq!(
        body,
        serde_json::json!([]),
        "lsp_list must return an empty array when no servers started: got {body}"
    );
}

#[test]
fn lsp_list_reflects_started_servers_with_id_and_state() {
    // After `lsp_start` the server appears in the snapshot with its id.
    // The state is non-deterministic vs. wall-clock (`Starting` → `Running`
    // as the child spawns), so assert on `id` exactly and on `state.state`
    // being one of the valid variants. The MCP equivalent
    // (`session_list_mcp_servers`) has the same contract.
    let app = make_app_with_bootstrap(&["list-srv"]);
    let window = make_window(&app.app, "session-lsp-listed");

    invoke(
        &window,
        "lsp_start",
        serde_json::json!({
            "args": {
                "server": "list-srv",
                "args": [],
            }
        }),
    )
    .expect("lsp_start");

    // Poll: the state handle is shared with the supervisor task, which
    // flips to `Running` asynchronously. A quick retry loop covers both
    // pre-spawn (`starting`) and post-spawn (`running`) observations.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_expected = false;
    while Instant::now() < deadline {
        let resp = invoke(&window, "lsp_list", serde_json::json!({})).expect("list");
        let body: Value = resp.deserialize().expect("deserialize body");
        let arr = body.as_array().expect("array");
        if arr.len() == 1
            && arr[0].get("id").and_then(|v| v.as_str()) == Some("list-srv")
            && matches!(
                arr[0]
                    .get("state")
                    .and_then(|s| s.get("state"))
                    .and_then(|v| v.as_str()),
                Some("starting" | "running")
            )
        {
            saw_expected = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        saw_expected,
        "lsp_list must surface the started server with a valid id+state"
    );

    invoke(
        &window,
        "lsp_stop",
        serde_json::json!({"server": "list-srv"}),
    )
    .expect("stop");
}

#[test]
fn lsp_list_is_scoped_to_caller_label() {
    // Alice starts a server; Bob's `lsp_list` must not see alice's server.
    // This is the same per-owner shape that `lsp_send` / `lsp_stop`
    // enforce, so one session cannot introspect another session's LSP
    // fleet without a label match.
    let app = make_app_with_bootstrap(&["alice-list-srv"]);
    let alice = make_window(&app.app, "session-alice-list");
    let bob = make_window(&app.app, "session-bob-list");

    invoke(
        &alice,
        "lsp_start",
        serde_json::json!({
            "args": {
                "server": "alice-list-srv",
                "args": [],
            }
        }),
    )
    .expect("alice start");

    let resp = invoke(&bob, "lsp_list", serde_json::json!({})).expect("bob list");
    let body: Value = resp.deserialize().expect("deserialize body");
    assert_eq!(
        body,
        serde_json::json!([]),
        "bob must not see alice's server in his lsp_list"
    );

    invoke(
        &alice,
        "lsp_stop",
        serde_json::json!({"server": "alice-list-srv"}),
    )
    .expect("owner stop");
}

#[test]
fn lsp_list_rejects_dashboard_window() {
    // Dashboard is not a session; `lsp_list` must refuse it with the
    // standard label-mismatch error. Mirrors the authz on
    // `lsp_start` / `lsp_stop` / `lsp_send`.
    let app = make_app_with_bootstrap(&["rust-analyzer"]);
    let window = make_window(&app.app, "dashboard");
    let err = invoke(&window, "lsp_list", serde_json::json!({}))
        .expect_err("dashboard must not call lsp_list");
    assert!(
        err.contains(LABEL_MISMATCH),
        "expected label-mismatch error, got: {err}"
    );
}
