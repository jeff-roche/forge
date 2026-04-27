//! F-608 step 4 acceptance tests for the [`SidecarSupervisor`].
//!
//! Each test drives the real `forged-agent` binary through one path of
//! the lifecycle:
//!
//! - [`spawn_returns_real_pid`] — handshake succeeds and the supervisor
//!   exposes the child's actual PID.
//! - [`crash_within_window_restarts`] — `SIGKILL`ing the current child
//!   is silently recovered with a fresh fork inside the
//!   60 s / 3-retry window.
//! - [`crash_exhausts_retries_emits_failed`] — repeated crashes past
//!   the budget escalate to a `BackgroundAgentCompleted` (failure
//!   path) emission on the supplied [`EventSink`].
//! - [`shutdown_grace_window_then_sigterm`] — a sub-millisecond grace
//!   forces the SIGTERM/SIGKILL fallback even for a cooperative child.
//!
//! The forge-agent-host crate is added as a dev-dependency on
//! forge-session in this PR so cargo builds the sidecar binary before
//! these tests run; the binary is then resolved next to the test exe
//! (`target/<profile>/forged-agent`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use forge_core::{AgentInstanceId, Event, EventSink};
use forge_ipc::sidecar::{
    SidecarAgentDef, SidecarHello, SidecarProviderSpec, SidecarSandboxLevel, SIDECAR_PROTO_VERSION,
};
use forge_session::sidecar::{SidecarSupervisor, SpawnParams};
use tempfile::TempDir;
use tokio::sync::Mutex;

/// Resolve the freshly-built `forged-agent` binary. cargo doesn't set
/// `CARGO_BIN_EXE_<name>` for binaries outside the package under test,
/// so we walk up from the test exe to `target/<profile>/forged-agent`.
/// If the binary is absent (e.g. `cargo test -p forge-session` without
/// a prior workspace build), invoke `cargo build -p forge-agent-host`
/// once to materialize it — keeps the test self-contained.
fn forged_agent_path() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    // target/<profile>/deps/<test>-<HASH> → target/<profile>/
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

/// In-memory [`EventSink`] that records every emit so tests can assert
/// on the supervisor's escalation events.
#[derive(Debug, Default)]
struct CapturingSink {
    events: Mutex<Vec<Event>>,
}

impl CapturingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    async fn snapshot(&self) -> Vec<Event> {
        self.events.lock().await.clone()
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
    }
}

fn supervisor(socket_dir: &std::path::Path) -> SidecarSupervisor {
    SidecarSupervisor::new(socket_dir.to_path_buf(), forged_agent_path())
}

/// Send `signal` to `pid`. Returns the libc rc; tests treat any non-
/// zero return as a fatal test setup error rather than a normal path.
fn signal(pid: u32, sig: i32) -> i32 {
    // SAFETY: kill(2) is async-signal-safe and side-effect-free for
    // the test process. The PID is the supervisor's reported child;
    // misuse would only affect this test.
    unsafe { libc::kill(pid as i32, sig) }
}

/// Wait until `predicate` returns `true` or the deadline elapses.
async fn wait_for<F>(timeout: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    predicate()
}

/// Acceptance: a successful spawn surfaces the child's PID, and the
/// PID corresponds to a live process.
#[tokio::test]
async fn spawn_returns_real_pid() {
    let tmp = TempDir::new().expect("tmp");
    let supervisor = supervisor(tmp.path());
    let id = AgentInstanceId::from_string("inst-pid".into());
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

    let pid = handle.pid();
    assert!(pid > 0, "spawn must report a positive child PID, got {pid}");
    // `kill -0` probes for liveness without delivering a signal.
    assert_eq!(
        signal(pid, 0),
        0,
        "child PID {pid} reported by supervisor must be alive after handshake"
    );

    handle.shutdown().await.expect("shutdown");
    // After shutdown the PID must no longer be alive (kill -0 returns
    // -1 with errno = ESRCH for a dead PID we still own as a parent).
    let dead = wait_for(Duration::from_secs(5), || signal(pid, 0) != 0).await;
    assert!(dead, "child PID {pid} must be reaped after shutdown");
}

/// Acceptance: SIGKILL on the current child triggers a transparent
/// re-fork inside the retry window. The supervisor's reported PID
/// changes to the new child and remains a live process.
#[tokio::test]
async fn crash_within_window_restarts() {
    let tmp = TempDir::new().expect("tmp");
    let supervisor = supervisor(tmp.path());
    let id = AgentInstanceId::from_string("inst-restart".into());
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

    let original_pid = handle.pid();
    assert_eq!(signal(original_pid, 0), 0, "child must be alive pre-crash");

    // SIGKILL the child; the supervisor task should observe the EOF
    // on its read half, classify the non-zero exit as a crash, and
    // re-fork inside the 60 s window.
    assert_eq!(signal(original_pid, libc::SIGKILL), 0, "SIGKILL");

    let restarted = wait_for(Duration::from_secs(15), || {
        let pid = handle.pid();
        pid != 0 && pid != original_pid && signal(pid, 0) == 0
    })
    .await;
    assert!(
        restarted,
        "supervisor must re-fork within 15 s; pid stayed at {original_pid}"
    );

    let new_pid = handle.pid();
    assert_ne!(new_pid, original_pid, "PID must change on restart");

    // Failure-path event must NOT have fired — we are within budget.
    let snap = sink.snapshot().await;
    assert!(
        !snap
            .iter()
            .any(|e| matches!(e, Event::BackgroundAgentCompleted { .. })),
        "in-budget restart must not emit BackgroundAgentCompleted; got {snap:?}"
    );

    // Clean up.
    handle.shutdown().await.expect("shutdown");
}

/// Acceptance: four crashes inside the retry window escalate to a
/// `BackgroundAgentCompleted` (failure path) emission. The supervisor
/// then exits without forking a fifth child.
#[tokio::test]
async fn crash_exhausts_retries_emits_failed() {
    let tmp = TempDir::new().expect("tmp");
    let supervisor = supervisor(tmp.path());
    let id = AgentInstanceId::from_string("inst-exhaust".into());
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

    // Drive 4 crashes in rapid succession — within the 60 s window —
    // by SIGKILL'ing each new child as soon as the supervisor reports
    // it. The 4th crash exceeds the 3-retry budget and must escalate.
    let mut last_pid = handle.pid();
    for _attempt in 0..4 {
        assert_ne!(last_pid, 0, "supervisor must report a live PID");
        let _ = signal(last_pid, libc::SIGKILL);

        let next = wait_for(Duration::from_secs(15), || {
            let pid = handle.pid();
            pid != 0 && pid != last_pid && signal(pid, 0) == 0
        })
        .await;
        if !next {
            // Either the supervisor escalated already (good) or it
            // failed to re-fork — break and check the sink below.
            break;
        }
        last_pid = handle.pid();
    }

    // Wait for the failure-path emission. The supervisor should emit
    // BackgroundAgentCompleted within a few seconds of the budget-
    // exceeding crash.
    let escalated = wait_for(Duration::from_secs(20), || {
        let snap = futures::executor::block_on(sink.snapshot());
        snap.iter()
            .any(|e| matches!(e, Event::BackgroundAgentCompleted { id: ev, .. } if ev == &id))
    })
    .await;
    assert!(
        escalated,
        "after 4 crashes inside the retry window the supervisor must emit BackgroundAgentCompleted"
    );

    // Final assertion: the failure event references the correct id.
    let snap = sink.snapshot().await;
    let failure = snap
        .iter()
        .find(|e| matches!(e, Event::BackgroundAgentCompleted { id: ev, .. } if ev == &id))
        .expect("failure event present");
    match failure {
        Event::BackgroundAgentCompleted { id: ev_id, .. } => {
            assert_eq!(ev_id, &id, "failure event must reference the spawned id")
        }
        _ => unreachable!(),
    }

    // Cleanup: handle drop triggers a best-effort shutdown signal even
    // though the supervisor task has already exited.
    drop(handle);
}

/// Acceptance: a sub-millisecond shutdown grace forces the
/// SIGTERM/SIGKILL fallback. The child still exits within a bounded
/// window and the handle's `shutdown()` resolves cleanly.
#[tokio::test]
async fn shutdown_grace_window_then_sigterm() {
    let tmp = TempDir::new().expect("tmp");
    let supervisor = supervisor(tmp.path());
    let id = AgentInstanceId::from_string("inst-grace".into());
    let sink = CapturingSink::new();

    let mut handle = supervisor
        .spawn(
            id.clone(),
            SpawnParams {
                hello: fixture_hello(&id),
            },
            sink.clone(),
        )
        .await
        .expect("spawn");

    let pid = handle.pid();
    assert_eq!(signal(pid, 0), 0, "child alive before shutdown");

    // 1 ms grace: short enough that even a cooperative child cannot
    // win the race; the supervisor must escalate to SIGTERM and
    // ultimately SIGKILL within the 500 ms internal window.
    handle
        .shutdown_with_grace(Duration::from_millis(1))
        .await
        .expect("shutdown");

    // Child must be gone.
    let dead = wait_for(Duration::from_secs(5), || signal(pid, 0) != 0).await;
    assert!(
        dead,
        "child PID {pid} must be reaped via SIGTERM/SIGKILL fallback"
    );

    // No retry-exhaustion event should be emitted: this is a clean
    // shutdown, not a crash.
    let snap = sink.snapshot().await;
    assert!(
        !snap
            .iter()
            .any(|e| matches!(e, Event::BackgroundAgentCompleted { .. })),
        "shutdown path must not fire BackgroundAgentCompleted"
    );
    // Sanity: the supervisor likely emitted the stub `Event` frames
    // produced by the child binary's setup. Ignore them — we only
    // care that no failure-path emission slipped through.
    let _ = snap;
}

/// Compile-time confirmation that key types pull in the bound trait
/// objects the supervisor's public API promises.
#[allow(dead_code)]
fn _api_smoke() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SidecarSupervisor>();
}
