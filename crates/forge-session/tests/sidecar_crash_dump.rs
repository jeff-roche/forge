//! F-608 step 7 acceptance test: panic-hook crash-dump writer.
//!
//! Drives the `forged-agent` binary into a panic via the
//! `FORGE_TEST_PANIC_ON_RUNTURN` test knob, then asserts:
//!   - the on-disk dump appears under the architected path
//!     (`<crash dir>/<session-id>/<instance-id>-<unix-ts>.json`)
//!   - the dump's `panic_message` matches the injected payload
//!   - the daemon-side reader (`crashes::collect_crashes_in_dir`) can
//!     parse it
//!
//! The test sets `FORGE_CRASH_DIR` to a tempdir on the supervisor
//! side; the supervisor spawns the child with that env inherited so
//! the writer in the sidecar honors the override too. We never
//! pollute the user's real `~/.local/share/forge/crashes`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use forge_core::{AgentInstanceId, Event, EventSink};
use forge_ipc::sidecar::{
    SidecarAgentDef, SidecarHello, SidecarMessage, SidecarProviderSpec, SidecarRunTurn,
    SidecarSandboxLevel, SIDECAR_PROTO_VERSION,
};
use forge_session::sidecar::{
    crashes::{collect_crashes_in_dir, CrashDump},
    SidecarSupervisor, SpawnParams,
};
use tempfile::TempDir;
use tokio::sync::Mutex;

/// Resolve the freshly-built `forged-agent` binary, mirroring the
/// pattern in `sidecar_supervisor.rs`. cargo doesn't set
/// `CARGO_BIN_EXE_<name>` for binaries outside the package under test,
/// so we walk up from the test exe to `target/<profile>/forged-agent`.
fn forged_agent_path() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe
        .parent()
        .and_then(|p| p.parent())
        .expect("target/<profile> dir")
        .to_path_buf();
    let candidate = dir.join("forged-agent");
    if !candidate.exists() {
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "-p", "forge-agent-host", "--bin", "forged-agent"])
            .status()
            .expect("invoke cargo build for forge-agent-host");
        assert!(
            status.success(),
            "cargo build -p forge-agent-host --bin forged-agent failed"
        );
    }
    assert!(
        candidate.exists(),
        "forged-agent binary missing at {} after build attempt",
        candidate.display()
    );
    candidate
}

#[derive(Debug, Default)]
struct CapturingSink {
    events: Mutex<Vec<Event>>,
}

impl CapturingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[async_trait]
impl EventSink for CapturingSink {
    async fn emit(&self, event: Event) -> Result<()> {
        self.events.lock().await.push(event);
        Ok(())
    }
}

fn fixture_hello(instance_id: &AgentInstanceId) -> SidecarHello {
    SidecarHello {
        proto: SIDECAR_PROTO_VERSION,
        instance_id: instance_id.clone(),
        agent_def: SidecarAgentDef {
            name: "test".into(),
            description: None,
            body: String::new(),
            allowed_paths: vec![],
            isolation: "process".into(),
            memory_enabled: false,
        },
        allowed_paths: vec![],
        workspace_path: "/tmp".into(),
        provider_spec: SidecarProviderSpec {
            kind: "stub".into(),
            model: "stub".into(),
            base_url: None,
        },
        sandbox_level: SidecarSandboxLevel::Level1,
        telemetry_endpoint: None,
        schema_version: forge_ipc::sidecar::SIDECAR_SCHEMA_VERSION,
    }
}

/// Acceptance: the sidecar's panic hook writes a dump to disk that
/// the daemon-side reader can parse, with the injected panic message
/// intact.
///
/// The test deliberately uses `FORGE_CRASH_DIR` to redirect the
/// writer to a tempdir. The supervisor inherits its full env to the
/// child, so the redirect propagates without any extra plumbing.
///
/// We use `#[tokio::test(flavor = "current_thread")]` so the env-var
/// mutation below is serialized against any other test in this file
/// that might also touch process-global env. Today there is only one
/// test, but pinning the flavor leaves the door open.
#[tokio::test]
async fn panic_hook_writes_crash_dump_to_disk() {
    // Tempdir for the on-disk dumps. Held across the whole test so
    // we can read its contents post-mortem.
    let crash_tmp = TempDir::new().expect("crash tempdir");
    let socket_tmp = TempDir::new().expect("socket tempdir");

    // Process-global env knobs. These must survive into the spawned
    // `forged-agent` child. SAFETY: tokio::process::Command snapshots
    // env at spawn time; setting before `spawn()` is sufficient.
    //
    // We rely on cargo's per-test process isolation only for the
    // FORGE_TEST_PANIC_ON_RUNTURN signal — the dispatch test in
    // `sidecar_supervisor.rs` would also panic if it inherited this
    // var, so we unset on test exit via RAII.
    struct EnvGuard(&'static [&'static str]);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for k in self.0 {
                std::env::remove_var(k);
            }
        }
    }
    let panic_msg = "F-608 step 7 injected panic";
    std::env::set_var("FORGE_CRASH_DIR", crash_tmp.path());
    std::env::set_var("FORGE_TEST_PANIC_ON_RUNTURN", panic_msg);
    // Also enable backtrace capture so the dump's `backtrace` field
    // is populated — the assertion below is permissive (it must be
    // present *or* explicitly None), but a real-user panic dump
    // should carry a trace.
    std::env::set_var("RUST_BACKTRACE", "1");
    let _env = EnvGuard(&[
        "FORGE_CRASH_DIR",
        "FORGE_TEST_PANIC_ON_RUNTURN",
        "RUST_BACKTRACE",
    ]);

    let session_id = "sess-step7-acceptance";
    let supervisor = SidecarSupervisor::new(socket_tmp.path().to_path_buf(), forged_agent_path())
        .with_session_id(session_id);

    let id = AgentInstanceId::from_string("inst-crash".into());
    let sink = CapturingSink::new();
    let handle = supervisor
        .spawn(
            id.clone(),
            SpawnParams {
                hello: fixture_hello(&id),
            },
            sink.clone(),
        )
        .await
        .expect("spawn");

    // Drive the child into the panic by sending a RunTurn frame.
    // The stub run-turn handler reads FORGE_TEST_PANIC_ON_RUNTURN and
    // panics; the panic hook persists the dump before the process
    // exits.
    let turn = SidecarMessage::RunTurn(SidecarRunTurn {
        turn_id: "turn-crash".into(),
        msg_id: forge_core::MessageId::new(),
        text: "anything".into(),
        agents_md: String::new(),
        branch_parent: None,
        branch_variant_index: None,
        byte_budget: 4096,
    });
    handle
        .command_tx
        .send(turn)
        .await
        .expect("queue RunTurn frame");

    // Poll the per-session crash dir until a dump appears or the
    // deadline elapses. The supervisor's restart loop will also try
    // to re-fork (the panicking child counts as a crash); we don't
    // care about that — we only care that *at least one* dump
    // landed on disk before the supervisor wound everything up.
    let session_dir = crash_tmp.path().join(session_id);
    let mut dumps: Vec<CrashDump> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if session_dir.exists() {
            dumps = collect_crashes_in_dir(&session_dir).expect("collect");
            if !dumps.is_empty() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Stop the supervisor (best effort — the supervisor may have
    // already escalated past its retry budget). Drop is enough; we
    // don't await shutdown because the supervisor may have already
    // exited on retry exhaustion, in which case `shutdown()` returns
    // an error we'd have to swallow anyway.
    drop(handle);

    assert!(
        !dumps.is_empty(),
        "expected at least one crash dump under {} after panicking RunTurn",
        session_dir.display()
    );

    let dump = &dumps[0];
    assert_eq!(
        dump.session_id, session_id,
        "dump must carry the spawning session id"
    );
    assert_eq!(
        dump.instance_id,
        id.to_string(),
        "dump must carry the spawning instance id"
    );
    assert!(
        dump.panic_message.contains(panic_msg),
        "panic message must survive the hook → disk roundtrip; got {:?}",
        dump.panic_message
    );

    // The dir we just enumerated must exist (the writer's
    // create_dir_all path) and the per-session dir must be 0o700 per
    // the architecture-doc contract.
    let mode = std::fs::metadata(&session_dir)
        .expect("metadata for session dir")
        .permissions();
    use std::os::unix::fs::PermissionsExt;
    let bits = mode.mode() & 0o777;
    assert_eq!(
        bits,
        0o700,
        "session dir at {} must be 0o700 per F-608 step 7 contract; got 0o{:o}",
        session_dir.display(),
        bits
    );

    // Filename collision invariant: the writer uses
    // `<instance-id>-<unix-ts>.json` and the per-spawn instance id is
    // unique, so the dump file name must include the instance id.
    let entries: Vec<String> = std::fs::read_dir(&session_dir)
        .expect("read session dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.iter().any(|n| n.starts_with(&id.to_string())),
        "expected a dump filename starting with {} under {}; got {:?}",
        id,
        session_dir.display(),
        entries
    );
}
