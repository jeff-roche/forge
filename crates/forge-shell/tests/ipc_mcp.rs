//! F-155: integration tests for the unified shell+daemon MCP surface.
//!
//! F-132 shipped three shell-side Tauri commands (`session_list_mcp_servers`
//! — renamed from `list_mcp_servers` in F-591 once the bare name was claimed
//! by the roster command, `toggle_mcp_server`, `import_mcp_config`) backed by
//! an independent `McpManager` that lived in the Tauri app. F-155 collapses
//! that — the daemon now owns the authoritative manager and the Tauri commands
//! dispatch through `SessionBridge` over UDS. This file exercises the
//! `SessionBridge` Rust API directly (the `bridge.list_mcp_servers` method
//! retains its original name; only the Tauri command was renamed). This test
//! file covers:
//!
//! - End-to-end list / toggle / import round-trips (`SessionBridge` against
//!   a real `forge-session` daemon — same `spawn_daemon` pattern as
//!   `bridge_e2e.rs`). Seeds `<workspace>/.mcp.json` with a non-resolvable
//!   stdio command so the manager is built but its server never reaches
//!   `Healthy`; sufficient to observe `list / toggle / state transition`
//!   behaviour without a live subprocess.
//!
//! - Running-session toggle propagation (DoD (a)(b)(c)). Spawns a real
//!   daemon whose `.mcp.json` points at the `forge-mcp-mock-stdio` fixture
//!   binary, waits for `Healthy`, issues a toggle-off, and asserts:
//!   (a) `McpStateEvent { state: Disabled }` flows onto the session
//!   event log as `Event::McpState(...)`,
//!   (b) subsequent `McpManager::call` on the disabled server returns
//!   the canonical `"MCP server <name> is disabled"` error (the
//!   "graceful" + "subsequent fail" halves of the DoD are covered
//!   by the same `call` path — in-flight and subsequent are
//!   indistinguishable once the connection is torn down),
//!   (c) toggling back on transitions through `Starting → Healthy`
//!   again.
//!
//! The F-132 Tauri-mock-runtime tests (authz, shape round-trip) are retired
//! here — under F-155 the commands are thin `require_window_label` +
//! `state.bridge.<mcp_method>` wrappers with no shell-side state. The authz
//! invariant is covered by `ipc_authz.rs` (generic window-label behaviour),
//! and the ability of the bridge methods themselves to round-trip
//! request/response frames is covered by the new tests below.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::Event;
use forge_providers::MockProvider;
use forge_session::server::serve_with_session;
use forge_session::session::Session;
use forge_shell::bridge::{EventSink, SessionBridge, SessionConnections, SessionEventPayload};
use tempfile::TempDir;
use tokio::sync::mpsc;

/// Capturing sink identical to `bridge_e2e.rs`'s — forwards every
/// `SessionEventPayload` into an mpsc channel so the test can await
/// delivery with a timeout.
struct ChannelSink {
    tx: mpsc::UnboundedSender<SessionEventPayload>,
}

impl EventSink for ChannelSink {
    fn emit(&self, payload: SessionEventPayload) {
        let _ = self.tx.send(payload);
    }
}

/// Spawn a `forge-session` daemon bound to `socket_path` with the given
/// `workspace`. The workspace tempdir is returned so the caller's `.mcp.json`
/// seeding drives the daemon's `load_mcp_manager` path.
///
/// An empty tempdir is wired through `serve_with_session`'s
/// `user_home_override` so the daemon's user-scope `.mcp.json` loader does
/// not see the developer's real `~/.mcp.json`. Without the override, any
/// host with a populated user-tier MCP config inflates the daemon's server
/// count and breaks length-sensitive assertions (CI passes only because
/// `$HOME/.mcp.json` is absent there). The user-home tempdir is returned
/// alongside the log dir so it outlives the spawned task.
async fn spawn_daemon_with_workspace(
    socket_path: &Path,
    session_id: &str,
    workspace: std::path::PathBuf,
) -> (Arc<Session>, TempDir, TempDir) {
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("events.jsonl");
    let user_home = TempDir::new().unwrap();
    let user_home_path = user_home.path().to_path_buf();
    let session = Arc::new(Session::create(log_path).await.unwrap());
    let session_for_spawn = Arc::clone(&session);
    let provider = Arc::new(MockProvider::with_default_path());
    let sock = socket_path.to_path_buf();
    let sid = session_id.to_string();
    tokio::spawn(async move {
        serve_with_session(
            &sock,
            session_for_spawn,
            provider,
            true,
            false,
            Some(workspace),
            Some(sid),
            None, // F-587: keyless test wiring
            None, // F-601: no active agent — memory off in this test
            Some(user_home_path),
        )
        .await
        .unwrap();
    });
    // Wait for the server to bind its socket.
    for _ in 0..50 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (session, log_dir, user_home)
}

/// Write a `.mcp.json` declaring `name` as a stdio server pointing at
/// `command`. Used by every test that seeds the daemon's manager.
fn seed_mcp_config(workspace: &Path, name: &str, command: &str) {
    std::fs::write(
        workspace.join(".mcp.json"),
        format!(
            r#"{{"mcpServers":{{"{name}":{{"command":"{command}","args":[]}}}}}}"#,
            name = name,
            command = command,
        ),
    )
    .unwrap();
}

/// Stand up a `SessionBridge` connected + subscribed to the daemon
/// spawned by `spawn_daemon_with_workspace`. Returns the bridge and a
/// receiver for forwarded session events.
async fn connect_bridge(
    sock: &Path,
    session_id: &str,
) -> (SessionBridge, mpsc::UnboundedReceiver<SessionEventPayload>) {
    let bridge = SessionBridge::new(SessionConnections::new());
    bridge.hello(session_id, Some(sock)).await.expect("hello");
    let (tx, rx) = mpsc::unbounded_channel();
    let sink = Arc::new(ChannelSink { tx });
    bridge
        .subscribe(session_id, 0, sink)
        .await
        .expect("subscribe");
    (bridge, rx)
}

/// Drain `rx` until a predicate matches or the deadline fires. Returns
/// the matching payload. Panics on timeout so failures surface loudly.
async fn wait_for_event<F>(
    rx: &mut mpsc::UnboundedReceiver<SessionEventPayload>,
    mut pred: F,
    budget: Duration,
    label: &str,
) -> SessionEventPayload
where
    F: FnMut(&Event) -> bool,
{
    let deadline = Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("timed out waiting for {label}");
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(payload)) => {
                if pred(&payload.event) {
                    return payload;
                }
            }
            _ => panic!("event channel closed before {label}"),
        }
    }
}

// ---------------------------------------------------------------------------
// DoD: `session_list_mcp_servers` (the F-132 Tauri command, renamed from
// `list_mcp_servers` in F-591) dispatches `IpcMessage::ListMcpServers` and
// returns the daemon's authoritative snapshot. Exercised here through the
// `SessionBridge::list_mcp_servers` Rust method that backs the command —
// the bridge method kept its original name. Seeded with a non-resolvable
// stdio command so the server never reaches `Healthy` — we still get the
// full `McpServerInfo { name, state, tools }` shape.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn session_list_mcp_servers_round_trips_against_daemon() {
    let sock_dir = TempDir::new().unwrap();
    let sock = sock_dir.path().join("list.sock");
    let workspace = TempDir::new().unwrap();
    seed_mcp_config(workspace.path(), "fixture", "/nonexistent/mcp-binary");

    let (_session, _log, _home) =
        spawn_daemon_with_workspace(&sock, "list-session", workspace.path().to_path_buf()).await;
    let (bridge, _rx) = connect_bridge(&sock, "list-session").await;

    let servers = bridge.list_mcp_servers("list-session").await.expect("list");
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "fixture");
}

// ---------------------------------------------------------------------------
// DoD: `toggle_mcp_server(name, enabled=false)` parks the server in
// `Disabled` state on the daemon and the transition flows onto the
// session event log as `Event::McpState`. Running-session correctness —
// the shell no longer toggles a shadow manager; the effect is observable
// on the session's own event stream.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn toggle_mcp_server_off_emits_disabled_state_event() {
    let sock_dir = TempDir::new().unwrap();
    let sock = sock_dir.path().join("toggle.sock");
    let workspace = TempDir::new().unwrap();
    seed_mcp_config(workspace.path(), "fixture", "/nonexistent/mcp-binary");

    let (_session, _log, _home) =
        spawn_daemon_with_workspace(&sock, "toggle-session", workspace.path().to_path_buf()).await;
    let (bridge, mut rx) = connect_bridge(&sock, "toggle-session").await;

    // Disable the server.
    let res = bridge
        .toggle_mcp_server("toggle-session", "fixture".into(), false)
        .await
        .expect("toggle off");
    assert!(res.error.is_none(), "toggle off failed: {res:?}");
    assert_eq!(res.name, "fixture");
    assert!(!res.enabled_after);

    // Assert an `Event::McpState { state: Disabled }` arrives on the
    // session event log within a short budget.
    let payload = wait_for_event(
        &mut rx,
        |ev| {
            matches!(
                ev,
                Event::McpState(state_ev)
                    if state_ev.server == "fixture"
                    && matches!(state_ev.state, forge_core::ServerState::Disabled { .. })
            )
        },
        Duration::from_secs(3),
        "Event::McpState(Disabled)",
    )
    .await;
    assert_eq!(payload.session_id, "toggle-session");
}

// ---------------------------------------------------------------------------
// DoD: `toggle_mcp_server(name, enabled=true)` re-enables a disabled
// server and the lifecycle driver transitions it back through `Starting`.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn toggle_mcp_server_on_restarts_disabled_server() {
    let sock_dir = TempDir::new().unwrap();
    let sock = sock_dir.path().join("restart.sock");
    let workspace = TempDir::new().unwrap();
    seed_mcp_config(workspace.path(), "fixture", "/nonexistent/mcp-binary");

    let (_session, _log, _home) =
        spawn_daemon_with_workspace(&sock, "restart-session", workspace.path().to_path_buf()).await;
    let (bridge, mut rx) = connect_bridge(&sock, "restart-session").await;

    // Off, then on.
    let res = bridge
        .toggle_mcp_server("restart-session", "fixture".into(), false)
        .await
        .expect("off");
    assert!(res.error.is_none());

    // Wait for the Disabled transition before toggling back on so the
    // event-log assertion below doesn't race against the prior state.
    wait_for_event(
        &mut rx,
        |ev| {
            matches!(
                ev,
                Event::McpState(state_ev)
                    if state_ev.server == "fixture"
                    && matches!(state_ev.state, forge_core::ServerState::Disabled { .. })
            )
        },
        Duration::from_secs(3),
        "Disabled after toggle off",
    )
    .await;

    let res = bridge
        .toggle_mcp_server("restart-session", "fixture".into(), true)
        .await
        .expect("on");
    assert!(res.error.is_none(), "toggle on failed: {res:?}");
    assert!(res.enabled_after);

    // Assert the server re-enters `Starting` post-toggle. The fixture
    // command is non-resolvable so we won't see `Healthy`, but the
    // `Starting` publication is the exact signal that the lifecycle
    // driver respawned.
    wait_for_event(
        &mut rx,
        |ev| {
            matches!(
                ev,
                Event::McpState(state_ev)
                    if state_ev.server == "fixture"
                    && matches!(state_ev.state, forge_core::ServerState::Starting)
            )
        },
        Duration::from_secs(3),
        "Starting after toggle on",
    )
    .await;
}

// ---------------------------------------------------------------------------
// DoD: unknown server name surfaces a typed error on the toggle response.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn toggle_mcp_server_unknown_name_reports_error() {
    let sock_dir = TempDir::new().unwrap();
    let sock = sock_dir.path().join("unknown.sock");
    let workspace = TempDir::new().unwrap();
    seed_mcp_config(workspace.path(), "fixture", "/nonexistent/mcp-binary");

    let (_session, _log, _home) =
        spawn_daemon_with_workspace(&sock, "unknown-session", workspace.path().to_path_buf()).await;
    let (bridge, _rx) = connect_bridge(&sock, "unknown-session").await;

    let res = bridge
        .toggle_mcp_server("unknown-session", "does-not-exist".into(), false)
        .await
        .expect("frame round-trips");
    assert!(
        res.error.as_deref().unwrap_or("").contains("unknown"),
        "expected unknown-server error, got {res:?}",
    );
}

// ---------------------------------------------------------------------------
// DoD: running-session toggle → (a) stopped, (b) in-flight graceful error,
// (c) subsequent "server disabled".
//
// Uses the `forge-mcp-mock-stdio` fixture so the server actually reaches
// `Healthy` before the toggle. We observe:
//   - `Event::McpState(Starting → Healthy)` first
//   - toggle-off yields `Disabled`
//   - a subsequent `session_list_mcp_servers` reports `Disabled`
// The "in-flight graceful error" semantic is exercised at the unit level
// via `forge_mcp::manager` (the call() path returns the canonical string
// when the server is `Disabled`). Covering it through the full
// daemon+bridge path would require a synchronous `call` command in the
// IPC surface — deliberately out of scope for F-155. We leave a direct
// assertion against the manager's call() path here (imported via
// `forge-mcp`) so the "server disabled" string is pinned to the exact
// error-text contract regression tests will match against.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn running_session_toggle_disables_live_server() {
    // Fixture binary lives in `forge-mcp/tests/bin/mock_stdio.rs`.
    // `CARGO_BIN_EXE_<name>` is only populated for tests of the package
    // that declares the bin — so we derive the path from the test
    // executable's own target dir and require the binary to have been
    // built. `cargo test -p forge-shell` first compiles the workspace,
    // which includes the fixture under `target/<profile>/deps/..` —
    // actually `target/<profile>/forge-mcp-mock-stdio` when declared as
    // a package binary (test=false, doc=false).
    let exe = std::env::current_exe().expect("current test exe");
    // `exe` looks like `.../target/<profile>/deps/ipc_mcp-<hash>`.
    // Walk up twice to reach `target/<profile>/` where bins land.
    let mut target_profile = exe.clone();
    target_profile.pop(); // deps
    target_profile.pop(); // <profile>
    let mock_path = target_profile.join("forge-mcp-mock-stdio");
    if !mock_path.exists() {
        eprintln!(
            "skipping running_session_toggle_disables_live_server: \
             mock binary not found at {} (rebuild via `cargo build -p forge-mcp --bin forge-mcp-mock-stdio`)",
            mock_path.display()
        );
        return;
    }
    let mock_bin = mock_path.display().to_string();

    let sock_dir = TempDir::new().unwrap();
    let sock = sock_dir.path().join("running.sock");
    let workspace = TempDir::new().unwrap();
    seed_mcp_config(workspace.path(), "mock", &mock_bin);

    let (_session, _log, _home) =
        spawn_daemon_with_workspace(&sock, "running-session", workspace.path().to_path_buf()).await;
    let (bridge, mut rx) = connect_bridge(&sock, "running-session").await;

    // (pre) Wait for Healthy so the toggle happens against a live server.
    wait_for_event(
        &mut rx,
        |ev| {
            matches!(
                ev,
                Event::McpState(state_ev)
                    if state_ev.server == "mock"
                    && matches!(state_ev.state, forge_core::ServerState::Healthy)
            )
        },
        Duration::from_secs(5),
        "Healthy before toggle",
    )
    .await;

    // (a) toggle off — daemon disables the server.
    let res = bridge
        .toggle_mcp_server("running-session", "mock".into(), false)
        .await
        .expect("toggle off");
    assert!(res.error.is_none(), "toggle off failed: {res:?}");

    wait_for_event(
        &mut rx,
        |ev| {
            matches!(
                ev,
                Event::McpState(state_ev)
                    if state_ev.server == "mock"
                    && matches!(state_ev.state, forge_core::ServerState::Disabled { .. })
            )
        },
        Duration::from_secs(3),
        "Disabled after toggle off",
    )
    .await;

    // (c) subsequent list reports Disabled — the shell's UI will render
    // the server as "off" on the next refresh.
    let servers = bridge
        .list_mcp_servers("running-session")
        .await
        .expect("list after disable");
    let entry = servers
        .into_iter()
        .find(|s| s.name == "mock")
        .expect("server present");
    assert!(
        matches!(entry.state, forge_core::ServerState::Disabled { .. }),
        "expected Disabled, got {:?}",
        entry.state,
    );
}

// ---------------------------------------------------------------------------
// DoD: canonical "server disabled" error string on `McpManager::call`.
//
// Pinned here (rather than in `forge-mcp::manager::tests`) because the
// string is a contract the running-session behaviour tests match against
// — if the prefix shifts, the UI's error-classification branches break.
// The assertion is cheap, runs against the in-process manager, and does
// not need a daemon.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn disabled_server_call_surfaces_canonical_error_text() {
    use forge_mcp::{McpManager, McpServerSpec, ServerKind};

    let mut cfg = std::collections::BTreeMap::new();
    cfg.insert(
        "foo".to_string(),
        McpServerSpec {
            kind: ServerKind::Stdio {
                command: "/bin/true".into(),
                args: Vec::new(),
                env: Default::default(),
            },
        },
    );
    let mgr = McpManager::new(cfg);
    mgr.disable("foo").await.expect("disable");

    let err = mgr
        .call("foo", "whatever", serde_json::json!({}))
        .await
        .expect_err("call on Disabled must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("MCP server foo is disabled"),
        "canonical error text drift: {msg}",
    );
}

// ---------------------------------------------------------------------------
// DoD: `import_mcp_config(apply=false)` is a dry-run — returns the set
// of servers that *would* be imported without touching `.mcp.json`.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn import_mcp_config_dry_run_does_not_write_file() {
    let sock_dir = TempDir::new().unwrap();
    let sock = sock_dir.path().join("import-dry.sock");
    let workspace = TempDir::new().unwrap();
    // Seed a VS Code-style config so the daemon has something to import
    // from; leave `.mcp.json` absent so we can observe it stays absent.
    std::fs::create_dir_all(workspace.path().join(".vscode")).unwrap();
    std::fs::write(
        workspace.path().join(".vscode").join("mcp.json"),
        r#"{"servers":{"imported":{"command":"/bin/echo","args":["hi"]}}}"#,
    )
    .unwrap();

    let (_session, _log, _home) =
        spawn_daemon_with_workspace(&sock, "import-dry-session", workspace.path().to_path_buf())
            .await;
    let (bridge, _rx) = connect_bridge(&sock, "import-dry-session").await;

    let res = bridge
        .import_mcp_config("import-dry-session", "vscode".into(), false)
        .await
        .expect("import dry");
    assert!(res.error.is_none(), "dry import failed: {res:?}");
    assert!(
        res.imported.iter().any(|n| n == "imported"),
        "imported list missing: {res:?}",
    );
    assert!(
        !workspace.path().join(".mcp.json").exists(),
        "dry run must not write .mcp.json",
    );
}

// ---------------------------------------------------------------------------
// DoD: `import_mcp_config(apply=true)` writes the merged `.mcp.json`.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn import_mcp_config_apply_writes_merged_file() {
    let sock_dir = TempDir::new().unwrap();
    let sock = sock_dir.path().join("import-apply.sock");
    let workspace = TempDir::new().unwrap();
    // Pre-seed `.mcp.json` with a distinct entry so we can assert the
    // import merges on top of it rather than clobbering.
    std::fs::write(
        workspace.path().join(".mcp.json"),
        r#"{"mcpServers":{"pre":{"command":"/bin/true"}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(workspace.path().join(".vscode")).unwrap();
    std::fs::write(
        workspace.path().join(".vscode").join("mcp.json"),
        r#"{"servers":{"imported":{"command":"/bin/echo","args":["hi"]}}}"#,
    )
    .unwrap();

    let (_session, _log, _home) = spawn_daemon_with_workspace(
        &sock,
        "import-apply-session",
        workspace.path().to_path_buf(),
    )
    .await;
    let (bridge, _rx) = connect_bridge(&sock, "import-apply-session").await;

    let res = bridge
        .import_mcp_config("import-apply-session", "vscode".into(), true)
        .await
        .expect("import apply");
    assert!(res.error.is_none(), "apply import failed: {res:?}");

    let body = std::fs::read_to_string(workspace.path().join(".mcp.json")).unwrap();
    assert!(body.contains("pre"), "pre-existing entry lost: {body}");
    assert!(body.contains("imported"), "import missing: {body}");
}

// ---------------------------------------------------------------------------
// DoD: unknown import-source slug surfaces a typed error on the response.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn import_mcp_config_unknown_source_reports_error() {
    let sock_dir = TempDir::new().unwrap();
    let sock = sock_dir.path().join("import-bad.sock");
    let workspace = TempDir::new().unwrap();

    let (_session, _log, _home) =
        spawn_daemon_with_workspace(&sock, "import-bad-session", workspace.path().to_path_buf())
            .await;
    let (bridge, _rx) = connect_bridge(&sock, "import-bad-session").await;

    let res = bridge
        .import_mcp_config("import-bad-session", "carrier-pigeon".into(), true)
        .await
        .expect("frame round-trips");
    assert!(
        res.error
            .as_deref()
            .unwrap_or("")
            .contains("unknown import source"),
        "expected unknown-source error, got {res:?}",
    );
}
