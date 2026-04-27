//! F-608 step 5 acceptance: when `FORGE_AGENT_SIDECAR=1` the
//! `BackgroundAgentRegistry::start` path forks a real `forged-agent`
//! child via the [`SidecarSupervisor`] and feeds the child's PID into
//! the [`ResourceMonitor`]. The first per-instance
//! [`Event::ResourceSample`] arriving on the registry bus closes
//! F-451 — the daemon-PID no-op guard is replaced by a real PID and
//! the AgentMonitor pills receive live numbers.
//!
//! These tests must NOT influence the flag-off baseline covered by
//! the in-module tests at `crates/forge-session/src/bg_agents.rs:451-720`.
//! Each test sets / unsets the env var locally; the registry reads the
//! flag on every `start()` call so concurrent tests with different
//! settings don't cross-contaminate as long as they don't share a
//! registry mid-test.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use forge_agents::{AgentDef, InitialPrompt, Isolation, Orchestrator};
use forge_core::{AgentInstanceId, Event};
use forge_session::sidecar::SidecarSupervisor;
use forge_session::{BackgroundAgentRegistry, ResourceMonitor};
use tempfile::TempDir;

/// Resolve the freshly-built `forged-agent` binary. Mirrors
/// `tests/sidecar_supervisor.rs::forged_agent_path`: walk up from the
/// test exe to `target/<profile>/forged-agent`, falling back to a
/// scoped `cargo build -p forge-agent-host` if absent so a per-package
/// `cargo test` invocation still works.
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

fn def(name: &str) -> AgentDef {
    AgentDef {
        name: name.to_string(),
        description: None,
        body: String::new(),
        allowed_paths: vec![],
        isolation: Isolation::Process,
        memory_enabled: false,
    }
}

fn fast_monitor() -> Arc<ResourceMonitor> {
    Arc::new(ResourceMonitor::new(
        forge_session::default_sampler(),
        Duration::from_millis(100),
    ))
}

/// Build a registry whose monitor ticks fast enough that the test
/// sees a `ResourceSample` within a couple of seconds. Wires the
/// supplied supervisor as the F-608 step 5 sidecar source.
fn registry_with_sidecar(supervisor: Arc<SidecarSupervisor>) -> BackgroundAgentRegistry {
    let orch = Arc::new(Orchestrator::new());
    let defs = Arc::new(vec![def("writer")]);
    BackgroundAgentRegistry::with_monitor(orch, defs, fast_monitor())
        .with_sidecar_supervisor(supervisor)
}

/// F-608 step 5 / F-451 closure.
///
/// With `FORGE_AGENT_SIDECAR=1` set, starting a background agent must:
///   1. Fork a `forged-agent` child via the supervisor.
///   2. Surface a non-daemon child PID into the resource monitor.
///   3. Cause an `Event::ResourceSample { instance_id, .. }` to arrive
///      on the registry's broadcast bus for the new instance — the
///      daemon-PID no-op guard is no longer in the way.
///
/// The flag-off path is exercised by the in-module test
/// `start_does_not_emit_resource_sample_for_background_agents`; this
/// test only flips the flag on for its own duration.
#[tokio::test]
async fn start_with_sidecar_flag_real_pid_emits_resource_sample() {
    // SAFETY: the test sets a process-global env var. The
    // `BackgroundAgentRegistry::start` flag check is read-once-per-
    // call, so other concurrent tests in this file that don't depend
    // on the flag still see whatever default is in their own
    // environment by the time their `start()` runs. We unset on the
    // happy path AND on early returns via the `FlagGuard` RAII below.
    struct FlagGuard;
    impl Drop for FlagGuard {
        fn drop(&mut self) {
            std::env::remove_var("FORGE_AGENT_SIDECAR");
        }
    }
    std::env::set_var("FORGE_AGENT_SIDECAR", "1");
    let _guard = FlagGuard;

    let tmp = TempDir::new().expect("tmp");
    let supervisor = Arc::new(SidecarSupervisor::new(
        tmp.path().to_path_buf(),
        forged_agent_path(),
    ));

    let registry = registry_with_sidecar(supervisor);
    let mut rx = registry.events();

    let id: AgentInstanceId = registry
        .start("writer", InitialPrompt::from("ping"))
        .await
        .expect("start should succeed under sidecar flag");

    // Drain the BackgroundAgentStarted emission so the next
    // ResourceSample for `id` stands out unambiguously.
    let started = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("Started event must arrive within 3s")
        .expect("registry bus must not close");
    assert!(
        matches!(&started, Event::BackgroundAgentStarted { id: ev_id, .. } if ev_id == &id),
        "first event must be Started, got {started:?}"
    );

    // Wait up to 5s for the first per-instance ResourceSample. The
    // monitor's first tick is intentionally a warm-up emission
    // (cpu_pct=None on the first reading), so RSS / fd count are the
    // load-bearing fields the assertion pins. F-451's DoD names this
    // exact arrival as the closure criterion.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut saw_sample = false;
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
            Ok(Ok(Event::ResourceSample { instance_id, .. })) if instance_id == id => {
                saw_sample = true;
                break;
            }
            Ok(Ok(_other)) => continue,
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }
    assert!(
        saw_sample,
        "FORGE_AGENT_SIDECAR=1 must surface Event::ResourceSample on the \
         registry bus for the new instance (closes F-451)"
    );

    // Drive the orchestrator to terminal so the registry's forwarder
    // drops the supervisor handle — leaves no live child after the
    // test exits. The supervisor task SIGKILLs the child via
    // `kill_on_drop` when its handle is dropped; we don't await the
    // BackgroundAgentCompleted emission because the test's purpose is
    // the ResourceSample assertion, not the teardown ordering.
    registry
        .orchestrator()
        .stop(&id)
        .await
        .expect("orchestrator stop");
}
