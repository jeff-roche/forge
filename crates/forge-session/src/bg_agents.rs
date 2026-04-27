//! Background-agent lifecycle (F-137).
//!
//! Top-level user-initiated agents that run alongside the active chat and
//! surface completion in the Agent Monitor (see `docs/product/ai-ux.md` §10.6).
//! Distinct from sub-agents (F-134 / `agent.spawn`): a sub-agent is spawned
//! *by* an agent as part of orchestration and appears inline in the parent's
//! chat thread; a background agent is spawned *by the user* and lives in the
//! Agent Monitor pane.
//!
//! This module owns the bookkeeping set of background `AgentInstanceId`s and
//! drives two session-event emissions per instance: `BackgroundAgentStarted`
//! on successful spawn, `BackgroundAgentCompleted` when the underlying
//! orchestrator instance enters a terminal state. The registry speaks
//! `forge_core::Event` on a local `broadcast::Sender` so the shell-side IPC
//! bridge can forward them to the webview under the same `session:event`
//! channel the daemon already uses — no new event name, no new protocol.
//!
//! Promotion (moving a background agent into a main chat pane) removes the id
//! from the tracked set. The UI-level pane rebind — stitching the existing
//! transcript into a new chat pane without transcript loss — is a frontend
//! concern landing with the Agent Monitor work; the lifecycle invariant the
//! DoD pins here is observable state, not pane geometry. See the
//! `promote_removes_from_list` test below for the exact assertion.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use forge_agents::{
    AgentDef, AgentInstance, AgentScope, InitialPrompt, Orchestrator, SpawnContext,
};
use forge_core::{ids::AgentId, AgentInstanceId, Event, EventSink};
use forge_ipc::sidecar::{
    SidecarAgentDef, SidecarHello, SidecarProviderSpec, SidecarSandboxLevel, SIDECAR_PROTO_VERSION,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};

use crate::resource_monitor::{default_sampler, ResourceMonitor, DEFAULT_TICK};
use crate::sidecar::{SidecarHandle, SidecarSupervisor, SpawnParams};

/// Env flag that enables the per-instance sidecar fork path. Read on
/// every [`BackgroundAgentRegistry::start`] so an operator can flip the
/// behavior between session boots without recompiling. Any value other
/// than the empty string and `0` enables the sidecar path; the
/// architecture doc §"Open questions / Feature-flag" pins the
/// `FORGE_AGENT_SIDECAR=1` form as the canonical opt-in.
const SIDECAR_ENV_FLAG: &str = "FORGE_AGENT_SIDECAR";

/// Returns true when the `FORGE_AGENT_SIDECAR` env flag is set to a
/// truthy value. Empty / unset / `0` / `false` all disable the path —
/// any other value enables it. Mirrors the shape of the existing
/// `FORGE_FORCE_SHELL_HANDSHAKE` flag the daemon already honors so
/// operators have one mental model for opt-ins.
fn sidecar_flag_enabled() -> bool {
    match std::env::var(SIDECAR_ENV_FLAG) {
        Ok(val) => {
            let v = val.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    }
}

/// Channel capacity for the background-agent event bus. Matches
/// `forge-session`'s `Session::event_tx` capacity (1024). A dropped
/// subscriber is a non-fatal warmup state — the event is still registered
/// in the in-memory set and a subsequent `list_background_agents` call
/// observes the current running set.
const EVENT_BUS_CAPACITY: usize = 1024;

/// Snapshot row returned by [`BackgroundAgentRegistry::list`]. `agent_name`
/// mirrors the `AgentDef.name` the user started; `state` is one of the
/// lifecycle states `Running` / `Completed` / `Failed` so the UI can render
/// the same status pill the Agent Monitor shows for live agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BgAgentSummary {
    pub id: AgentInstanceId,
    pub agent_name: String,
    pub state: BgAgentState,
}

/// Three-way lifecycle tag. Collapses `forge_agents::InstanceState`'s
/// `Failed { reason }` to `Failed` without the reason — the reason already
/// rides the `forge_agents::AgentEvent::Failed` stream the registry listens
/// to, and exposing it here would force the UI to render arbitrary server
/// strings next to each row. A dedicated "why did this fail?" UI can read the
/// event log directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BgAgentState {
    Running,
    Completed,
    Failed,
}

/// Error type for registry operations. Kept local and small; the Tauri command
/// layer renders it through `Display`, matching the `Err(String)` wire shape
/// the other `ipc.rs` commands use.
#[derive(Debug, thiserror::Error)]
pub enum BgAgentError {
    #[error("unknown agent '{0}'")]
    UnknownAgent(String),
    #[error("spawn failed: {0}")]
    Spawn(#[from] forge_agents::Error),
}

/// F-608 step 5 adapter: forwards every event the
/// [`SidecarSupervisor`] emits onto the registry's broadcast bus, and
/// — on the supervisor's retry-exhaustion path —
/// drives [`Orchestrator::fail`] so the matching `AgentEvent::Failed`
/// reaches downstream subscribers (the bg_agents forwarder converts
/// that into `Event::BackgroundAgentCompleted` and clears the
/// `tracked` set / monitor entry).
///
/// Sidecar-side `Event::ResourceSample` rides the same channel — the
/// supervisor frames per-tick samples through this sink, the registry
/// bus carries them, and the shell's `session:event` forwarder
/// delivers them to the webview unchanged. This is what closes F-451.
struct SidecarSinkBridge {
    instance_id: AgentInstanceId,
    events: broadcast::Sender<Event>,
    orchestrator: Arc<Orchestrator>,
}

#[async_trait]
impl EventSink for SidecarSinkBridge {
    async fn emit(&self, event: Event) -> anyhow::Result<()> {
        // Intercept the supervisor's failure-path completion so the
        // orchestrator-side `AgentEvent::Failed` fires. The bg_agents
        // forwarder converts that back into a single user-visible
        // `BackgroundAgentCompleted` — re-emitting here would race the
        // forwarder and double-deliver. Driving `orchestrator.fail`
        // exclusively (and letting the existing forwarder publish the
        // `Completed`) keeps emission count and ordering identical to
        // the in-process path.
        if let Event::BackgroundAgentCompleted { id, .. } = &event {
            if id == &self.instance_id {
                if let Err(e) = self
                    .orchestrator
                    .fail(
                        &self.instance_id,
                        "sidecar retry budget exhausted".to_string(),
                    )
                    .await
                {
                    tracing::warn!(
                        target: "forge_session::bg_agents",
                        instance_id = %self.instance_id,
                        error = %e,
                        "orchestrator.fail failed on sidecar retry exhaustion",
                    );
                }
                return Ok(());
            }
        }
        // Every other event (notably `ResourceSample`) flows straight
        // onto the registry bus. `broadcast::Sender::send` errors only
        // when no subscriber is attached — a benign warmup state.
        let _ = self.events.send(event);
        Ok(())
    }
}

/// Build the daemon-side [`SidecarHello`] handed to the supervisor.
/// Fields the sidecar needs to seed its run-loop state: `agent_def`,
/// `provider_spec`, the workspace path. Step 5 forwards the
/// already-loaded `AgentDef` straight through; step 6 will swap the
/// stub `provider_spec` for the active provider's serializable shape.
fn build_sidecar_hello(instance_id: &AgentInstanceId, def: &AgentDef) -> SidecarHello {
    let isolation = match def.isolation {
        forge_agents::Isolation::Trusted => "trusted",
        forge_agents::Isolation::Process => "process",
        forge_agents::Isolation::Container => "container",
    };
    SidecarHello {
        proto: SIDECAR_PROTO_VERSION,
        instance_id: instance_id.clone(),
        agent_def: SidecarAgentDef {
            name: def.name.clone(),
            description: def.description.clone(),
            body: def.body.clone(),
            allowed_paths: def.allowed_paths.clone(),
            isolation: isolation.to_string(),
            memory_enabled: def.memory_enabled,
        },
        allowed_paths: def.allowed_paths.clone(),
        // Workspace + provider plumbing are deferred to step 6's
        // credential-push wiring — the architecture doc §"Step 6"
        // owns the daemon-side credential / provider plumbing. Step 5
        // ships a placeholder so the sidecar receives a well-formed
        // frame today.
        workspace_path: String::new(),
        provider_spec: SidecarProviderSpec {
            kind: "stub".to_string(),
            model: "stub".to_string(),
            base_url: None,
        },
        sandbox_level: SidecarSandboxLevel::Level1,
        telemetry_endpoint: None,
    }
}

/// Background-agent lifecycle owner.
///
/// One instance per session. Holds a handle to the session's shared
/// [`Orchestrator`] (so `agent.spawn` sub-agents and user-initiated
/// background agents live in the same orchestrator registry), the agent
/// defs the `start` call resolves `agent_name` against, and a local
/// broadcast channel the caller subscribes to for the two
/// `BackgroundAgent*` session events.
///
/// F-152: the registry also owns a [`ResourceMonitor`] that ticks per-
/// instance resource samples while the instance is tracked. The monitor's
/// broadcast stream is forwarded onto the same `events` channel so the
/// shell's `session:event` forwarder delivers `Event::ResourceSample`
/// to the webview alongside every other `forge_core::Event` variant.
pub struct BackgroundAgentRegistry {
    orchestrator: Arc<Orchestrator>,
    agent_defs: Arc<Vec<AgentDef>>,
    tracked: Arc<Mutex<HashSet<AgentInstanceId>>>,
    events: broadcast::Sender<Event>,
    monitor: Arc<ResourceMonitor>,
    /// F-608 step 5: optional per-instance sidecar supervisor. When set
    /// AND the `FORGE_AGENT_SIDECAR` env flag is enabled at `start()`
    /// time, the registry forks a `forged-agent` child via this
    /// supervisor and feeds the child's PID into [`ResourceMonitor`] —
    /// closing F-451. Unset (default) preserves the legacy in-process
    /// path with the daemon-PID no-op guard on the monitor.
    sidecar_supervisor: Option<Arc<SidecarSupervisor>>,
    /// F-608 step 5: live sidecar handles, keyed by instance id. Held
    /// here so the `SidecarHandle::Drop` impl does not fire (and SIGKILL
    /// the child) the moment `start()` returns. Removed on terminal
    /// transition by the orchestrator-stream forwarder.
    sidecar_handles: Arc<Mutex<HashMap<AgentInstanceId, SidecarHandle>>>,
}

impl BackgroundAgentRegistry {
    /// Construct a new registry bound to `orchestrator` and `agent_defs`.
    /// The returned instance starts empty — call [`Self::start`] to spawn a
    /// background agent and [`Self::events`] to subscribe to lifecycle
    /// events.
    ///
    /// The resource monitor uses the platform default sampler (`/proc` on
    /// Linux, no-op stub elsewhere) and the [`DEFAULT_TICK`] cadence. A
    /// forwarder task pipes every `Event::ResourceSample` the monitor
    /// emits onto the registry's own `events` bus so subscribers see one
    /// unified stream.
    pub fn new(orchestrator: Arc<Orchestrator>, agent_defs: Arc<Vec<AgentDef>>) -> Self {
        Self::with_monitor(
            orchestrator,
            agent_defs,
            Arc::new(ResourceMonitor::new(default_sampler(), DEFAULT_TICK)),
        )
    }

    /// Construct with an externally-provided [`ResourceMonitor`]. Used by
    /// integration tests that need a deterministic sampler or tick
    /// cadence; production callers go through [`Self::new`].
    pub fn with_monitor(
        orchestrator: Arc<Orchestrator>,
        agent_defs: Arc<Vec<AgentDef>>,
        monitor: Arc<ResourceMonitor>,
    ) -> Self {
        let (events, _rx) = broadcast::channel(EVENT_BUS_CAPACITY);

        // Forward every ResourceSample the monitor emits onto the
        // registry's own events bus. Subscribers then see lifecycle
        // events and resource samples on a single channel — the shell's
        // existing `session:event` forwarder needs no extra plumbing.
        //
        // Production `with_monitor` is called from Tauri's setup hook, which
        // runs inside the Tauri-managed tokio runtime, so `Handle::current`
        // is always available. The `try_current` guard makes the function
        // safe to call from sync test harnesses (webview-test suite
        // constructs `BridgeState` outside any runtime); when no runtime is
        // available the monitor-forwarding task is simply skipped — tests
        // that assert bg_agents lifecycle don't depend on resource samples
        // flowing through this channel.
        let mut monitor_rx = monitor.events();
        let events_tx = events.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                loop {
                    match monitor_rx.recv().await {
                        Ok(ev) => {
                            let _ = events_tx.send(ev);
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        Self {
            orchestrator,
            agent_defs,
            tracked: Arc::new(Mutex::new(HashSet::new())),
            events,
            monitor,
            sidecar_supervisor: None,
            sidecar_handles: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// F-608 step 5: bind a [`SidecarSupervisor`] used when the
    /// `FORGE_AGENT_SIDECAR` env flag is enabled at `start()` time.
    ///
    /// Returns `self` so the caller can fluent-chain after `new` /
    /// `with_monitor`. The flag check happens on every `start()` call
    /// (not at construction time) so an operator can flip the behavior
    /// across sessions without recompiling.
    pub fn with_sidecar_supervisor(mut self, supervisor: Arc<SidecarSupervisor>) -> Self {
        self.sidecar_supervisor = Some(supervisor);
        self
    }

    /// Subscribe to background-agent lifecycle events. Each subscriber gets
    /// its own receiver; the stream emits `forge_core::Event::BackgroundAgentStarted`
    /// once per successful spawn and `BackgroundAgentCompleted` once per
    /// terminal transition (whether the orchestrator reported
    /// `Completed` or `Failed` — both collapse to the completion event).
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Start a new background agent by name.
    ///
    /// 1. Resolves `agent_name` against the registry's defs; returns
    ///    [`BgAgentError::UnknownAgent`] on miss.
    /// 2. Subscribes to the orchestrator's lifecycle stream **before**
    ///    calling `spawn` — the stream is a bounded broadcast, so a late
    ///    subscriber would miss a fast `Completed` event.
    /// 3. Calls `orchestrator.spawn` under `AgentScope::User` with the
    ///    caller-supplied `prompt` threaded via `SpawnContext::with_prompt`
    ///    (F-134 mandate plumbing).
    /// 4. Inserts the new instance id into the tracked set.
    /// 5. Emits `Event::BackgroundAgentStarted` on the local channel.
    /// 6. Spawns a detached task that forwards the terminal event for this
    ///    instance from the orchestrator's stream onto the local channel as
    ///    `Event::BackgroundAgentCompleted` and removes the id from the
    ///    tracked set. The task also exits when the stream lags or closes —
    ///    a missed completion leaves the id in `tracked` rather than
    ///    panicking, which is the right failure mode for an idle UI: the
    ///    user can still issue `promote`, and `list` reports the last-known
    ///    state.
    pub async fn start(
        &self,
        agent_name: &str,
        prompt: InitialPrompt,
    ) -> Result<AgentInstanceId, BgAgentError> {
        let def = match self
            .agent_defs
            .iter()
            .find(|d| d.name == agent_name)
            .cloned()
        {
            Some(d) => d,
            None => {
                // F-371: every Err branch emits a structured warning with the
                // `agent_name` field so operators can trace a misdirected
                // `start_background_agent` call back to the offending UI
                // without having to correlate stderr lines by timestamp.
                tracing::warn!(
                    target: "forge_session::bg_agents",
                    agent_name = %agent_name,
                    "rejected start: unknown agent",
                );
                return Err(BgAgentError::UnknownAgent(agent_name.to_string()));
            }
        };

        // Subscribe BEFORE spawn. `Orchestrator::state_stream` is a bounded
        // broadcast — a late subscriber (e.g. `spawn → tokio::spawn(listener)`)
        // would race a fast `stop(id)` in between and miss the terminal
        // event. Capturing the stream up-front closes the window.
        let mut stream = self.orchestrator.state_stream();

        let ctx = SpawnContext {
            scope: AgentScope::User,
            initial_prompt: Some(prompt),
        };
        let instance: AgentInstance = match self.orchestrator.spawn(def, ctx).await {
            Ok(inst) => inst,
            Err(e) => {
                // F-371: orchestrator-side spawn rejection (e.g. isolation
                // violation) ends the background-agent lifecycle before it
                // starts. Emit so operators see both the user's `start`
                // attempt and the orchestrator's refusal in one log.
                tracing::warn!(
                    target: "forge_session::bg_agents",
                    agent_name = %agent_name,
                    error = %e,
                    "rejected start: orchestrator spawn failed",
                );
                return Err(BgAgentError::Spawn(e));
            }
        };
        let id = instance.id.clone();

        self.tracked.lock().await.insert(id.clone());

        // F-371: lifecycle transition — `start` succeeded. Mirrors the
        // `forge_agents::orchestrator` "spawned" info log so a downstream
        // filter on `instance_id` correlates both sides of the wiring.
        tracing::info!(
            target: "forge_session::bg_agents",
            instance_id = %id,
            agent_name = %instance.def.name,
            "started",
        );

        // F-608 step 5 / F-451: when the sidecar flag is on AND a
        // supervisor is wired, fork the per-instance `forged-agent`
        // child and feed its real PID into the resource monitor — the
        // daemon-PID no-op guard then steps aside and per-instance
        // `ResourceSample` events begin flowing on the registry bus
        // (closing F-451). On supervisor spawn failure we fall through
        // to the legacy daemon-PID path so a misconfigured operator
        // still gets the lifecycle events; the failure is logged for
        // triage.
        let sidecar_pid = if sidecar_flag_enabled() {
            match &self.sidecar_supervisor {
                Some(supervisor) => {
                    let hello = build_sidecar_hello(&id, &instance.def);
                    let bridge: Arc<dyn EventSink> = Arc::new(SidecarSinkBridge {
                        instance_id: id.clone(),
                        events: self.events.clone(),
                        orchestrator: Arc::clone(&self.orchestrator),
                    });
                    match supervisor
                        .spawn(id.clone(), SpawnParams { hello }, bridge)
                        .await
                    {
                        Ok(handle) => {
                            let pid = handle.pid();
                            // Hold the handle for the lifetime of the
                            // instance — dropping it here would fire
                            // `SidecarHandle::Drop` and SIGKILL the
                            // child. The orchestrator-stream forwarder
                            // below removes the entry on terminal
                            // transition.
                            self.sidecar_handles.lock().await.insert(id.clone(), handle);
                            tracing::info!(
                                target: "forge_session::bg_agents",
                                instance_id = %id,
                                child_pid = pid,
                                "sidecar spawned for background agent",
                            );
                            Some(pid)
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "forge_session::bg_agents",
                                instance_id = %id,
                                error = %e,
                                "sidecar spawn failed; falling back to in-process path",
                            );
                            None
                        }
                    }
                }
                None => {
                    tracing::debug!(
                        target: "forge_session::bg_agents",
                        instance_id = %id,
                        "sidecar flag set but no supervisor wired; staying in-process",
                    );
                    None
                }
            }
        } else {
            None
        };

        // F-152 / F-370: with no real per-child PID we pass the
        // daemon's own PID, which hits the `ResourceMonitor::track`
        // daemon-PID guard and turns the call into a deliberate no-op.
        // With a real PID (sidecar path) the monitor begins emitting
        // per-instance `ResourceSample` events on the registry bus.
        let track_pid = sidecar_pid.unwrap_or_else(std::process::id);
        self.monitor.track(id.clone(), track_pid).await;

        // `broadcast::Sender::send` errors only when no subscribers are
        // attached — a valid warmup state for an embedded registry whose
        // events only matter once a webview has subscribed.
        let _ = self.events.send(Event::BackgroundAgentStarted {
            id: id.clone(),
            agent: AgentId::new(),
            at: Utc::now(),
        });

        // Forwarder: watch the orchestrator stream for this instance's
        // terminal event. Exits after the first match or on stream close.
        // Cloned handles so the task outlives `&self`.
        let target_id = id.clone();
        let events = self.events.clone();
        let tracked = Arc::clone(&self.tracked);
        let monitor = Arc::clone(&self.monitor);
        let sidecar_handles = Arc::clone(&self.sidecar_handles);
        tokio::spawn(async move {
            while let Some(next) = stream.next().await {
                let event = match next {
                    Ok(e) => e,
                    // `Lagged` means this forwarder missed events; `Closed`
                    // means the orchestrator went away. Either case, we bail
                    // and leave `tracked` as-is so `list_background_agents`
                    // still renders the last-known state rather than
                    // silently dropping the row.
                    Err(_) => return,
                };
                let (terminal_id, is_failed) = match &event {
                    forge_agents::AgentEvent::Completed { id, .. } => (id.clone(), false),
                    forge_agents::AgentEvent::Failed { id, reason, .. } => {
                        if *id == target_id {
                            // F-371: failure log carries the reason so an
                            // operator doesn't need to cross-reference the
                            // orchestrator's own event stream to find it.
                            tracing::warn!(
                                target: "forge_session::bg_agents",
                                instance_id = %id,
                                reason = %reason,
                                "failed",
                            );
                        }
                        (id.clone(), true)
                    }
                    _ => continue,
                };
                if terminal_id != target_id {
                    continue;
                }
                // F-152: stop sampling for the terminated instance so
                // the UI pills clear back to `—` (no further
                // `ResourceSample` events reach the webview for this
                // id).
                tracked.lock().await.remove(&target_id);
                monitor.untrack(&target_id).await;
                // F-608 step 5: drop the supervisor handle on terminal
                // transition. `SidecarHandle::Drop` cooperatively
                // signals the supervisor task to wind the child down;
                // the supervisor itself emits a `BackgroundAgentCompleted`
                // failure event on retry exhaustion (intercepted upstream
                // by `SidecarSinkBridge`), so dropping here is purely
                // cleanup, not lifecycle signalling.
                let _removed = sidecar_handles.lock().await.remove(&target_id);
                if !is_failed {
                    // F-371: `Completed` path gets its own info log. Failed
                    // already logged above with the reason attached.
                    tracing::info!(
                        target: "forge_session::bg_agents",
                        instance_id = %target_id,
                        "completed",
                    );
                }
                if let Err(e) = events.send(Event::BackgroundAgentCompleted {
                    id: target_id.clone(),
                    at: Utc::now(),
                }) {
                    // F-371: broadcast-send errors only happen when the
                    // session has dropped every subscriber. Log at warn so
                    // a teardown race is observable without panicking.
                    tracing::warn!(
                        target: "forge_session::bg_agents",
                        instance_id = %target_id,
                        error = %e,
                        "completion emit failed: no subscribers",
                    );
                }
                return;
            }
        });

        Ok(id)
    }

    /// Promote a background agent to an "active chat" view. Removes the id
    /// from the tracked set so `list` no longer surfaces it as a background
    /// agent. The underlying orchestrator instance is **not** stopped —
    /// promotion is a UX re-attribution, not a lifecycle transition, so the
    /// agent keeps running under the same `AgentInstanceId` in a regular
    /// chat pane. Idempotent: promoting an unknown or already-promoted id
    /// is a successful no-op.
    pub async fn promote(&self, id: &AgentInstanceId) {
        self.tracked.lock().await.remove(id);
    }

    /// Snapshot of every currently-tracked background agent. Ordering is
    /// arbitrary (HashSet-backed) — the UI is expected to sort on
    /// `agent_name` or start-time as it sees fit.
    pub async fn list(&self) -> Vec<BgAgentSummary> {
        let tracked = self.tracked.lock().await.clone();
        let mut out = Vec::with_capacity(tracked.len());
        for id in tracked {
            if let Some(inst) = self.orchestrator.get(&id).await {
                let state = match inst.state {
                    forge_agents::InstanceState::Running => BgAgentState::Running,
                    forge_agents::InstanceState::Completed => BgAgentState::Completed,
                    forge_agents::InstanceState::Failed { .. } => BgAgentState::Failed,
                };
                out.push(BgAgentSummary {
                    id: inst.id,
                    agent_name: inst.def.name,
                    state,
                });
            }
        }
        out
    }

    /// Test / embedder accessor so the caller can drive orchestrator
    /// lifecycle transitions directly (there is no step-executor yet).
    /// Integration tests use this to call `orchestrator.stop(id)` and
    /// observe the registry's forwarder emit
    /// `Event::BackgroundAgentCompleted`.
    pub fn orchestrator(&self) -> &Arc<Orchestrator> {
        &self.orchestrator
    }

    /// F-152 / F-370: accessor for the resource monitor. A future step
    /// executor that forks a provider sidecar per instance calls
    /// `registry.monitor().track(id, child_pid)` with the real child PID
    /// to start emitting `ResourceSample` events. Today `start()` only
    /// invokes `track` with the daemon's own PID, which hits the
    /// monitor's daemon-PID no-op guard, so no misleading per-instance
    /// sample reaches the webview until a real child PID is supplied.
    pub fn monitor(&self) -> &Arc<ResourceMonitor> {
        &self.monitor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_agents::Isolation;

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

    async fn fresh() -> BackgroundAgentRegistry {
        let orch = Arc::new(Orchestrator::new());
        let defs = Arc::new(vec![def("writer"), def("reviewer")]);
        BackgroundAgentRegistry::new(orch, defs)
    }

    /// Construct a registry with a fast-tick resource monitor so the
    /// end-to-end integration test below doesn't wait a full second.
    async fn fresh_with_fast_monitor() -> BackgroundAgentRegistry {
        use crate::resource_monitor::{fake_sample, FakeSampler, ResourceMonitor};
        let orch = Arc::new(Orchestrator::new());
        let defs = Arc::new(vec![def("writer"), def("reviewer")]);
        let fake = Arc::new(FakeSampler::new(fake_sample(0.001, Some(4096), Some(3))));
        let monitor = Arc::new(ResourceMonitor::new(
            fake as Arc<dyn crate::resource_monitor::Sampler>,
            std::time::Duration::from_millis(20),
        ));
        BackgroundAgentRegistry::with_monitor(orch, defs, monitor)
    }

    #[tokio::test]
    async fn start_registers_instance_and_emits_started_event() {
        let reg = fresh().await;
        let mut rx = reg.events();

        let id = reg.start("writer", Arc::from("go")).await.unwrap();

        let first = rx.recv().await.unwrap();
        match first {
            Event::BackgroundAgentStarted { id: ev_id, .. } => {
                assert_eq!(ev_id, id, "started event must carry the spawned id");
            }
            other => panic!("expected BackgroundAgentStarted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn start_forwards_prompt_onto_registered_instance() {
        let reg = fresh().await;
        let id = reg.start("writer", Arc::from("draft intro")).await.unwrap();

        let inst = reg.orchestrator().get(&id).await.expect("registered");
        assert_eq!(
            inst.initial_prompt.as_deref(),
            Some("draft intro"),
            "Registry::start must thread the prompt onto AgentInstance.initial_prompt \
             (F-137 mandate)"
        );
    }

    #[tokio::test]
    async fn unknown_agent_returns_typed_error() {
        let reg = fresh().await;
        let err = reg
            .start("does-not-exist", Arc::from(""))
            .await
            .unwrap_err();
        assert!(matches!(err, BgAgentError::UnknownAgent(ref n) if n == "does-not-exist"));
    }

    #[tokio::test]
    async fn list_returns_running_instance_after_start() {
        let reg = fresh().await;
        let id = reg.start("writer", Arc::from("p")).await.unwrap();

        let rows = reg.list().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].agent_name, "writer");
        assert_eq!(rows[0].state, BgAgentState::Running);
    }

    #[tokio::test]
    async fn promote_removes_from_list() {
        // Observable contract: promotion is a tracking-set transition, not a
        // lifecycle transition. Post-promote the id disappears from `list`,
        // but `orchestrator.get` still returns the instance (still running
        // under the same id) — a UI-level pane rebind is a follow-up.
        let reg = fresh().await;
        let id = reg.start("writer", Arc::from("p")).await.unwrap();
        assert_eq!(reg.list().await.len(), 1);

        reg.promote(&id).await;

        assert!(
            reg.list().await.is_empty(),
            "promote must remove the id from list() results"
        );
        assert!(
            reg.orchestrator().get(&id).await.is_some(),
            "promote is UX re-attribution; the orchestrator instance stays alive"
        );
    }

    #[tokio::test]
    async fn promote_unknown_id_is_a_noop() {
        let reg = fresh().await;
        reg.promote(&AgentInstanceId::new()).await;
        assert!(reg.list().await.is_empty());
    }

    #[tokio::test]
    async fn completion_event_fires_when_orchestrator_stops_instance() {
        // Proves the forwarder wiring: driving `orchestrator.stop(id)`
        // causes the registry to emit `BackgroundAgentCompleted` on its
        // local bus and drop the id from `tracked`.
        let reg = fresh().await;
        let mut rx = reg.events();
        let id = reg.start("writer", Arc::from("p")).await.unwrap();

        // Drain the Started event so the next recv() is unambiguously the Completed.
        match rx.recv().await.unwrap() {
            Event::BackgroundAgentStarted { .. } => {}
            other => panic!("expected Started, got {other:?}"),
        }

        // Drive the orchestrator to terminal state directly; the forwarder
        // picks this up and converts to `BackgroundAgentCompleted`.
        reg.orchestrator().stop(&id).await.unwrap();

        let completed = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("BackgroundAgentCompleted must arrive within the timeout")
            .expect("forwarder must not drop the completion event");

        match completed {
            Event::BackgroundAgentCompleted { id: ev_id, .. } => {
                assert_eq!(ev_id, id, "completion id must match the started id");
            }
            other => panic!("expected BackgroundAgentCompleted, got {other:?}"),
        }

        // List must no longer include the id — forwarder cleared `tracked`.
        assert!(
            reg.list().await.is_empty(),
            "completed background agent must drop from list"
        );
    }

    #[tokio::test]
    async fn failure_path_also_fires_completion_event() {
        // `BackgroundAgentCompleted` intentionally collapses both the
        // `Completed` and `Failed` orchestrator-level terminals into a
        // single user-visible event — the UI renders "completed" as a
        // neutral end-of-life marker and surfaces the failure reason via a
        // separate inspection (the event log), matching the wire shape
        // pinned in `forge_core::Event::BackgroundAgentCompleted { id, at }`.
        let reg = fresh().await;
        let mut rx = reg.events();
        let id = reg.start("writer", Arc::from("p")).await.unwrap();

        let _ = rx.recv().await; // drain Started

        reg.orchestrator()
            .fail(&id, "boom".to_string())
            .await
            .unwrap();

        let completed = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("a completion event must fire even on Failed")
            .expect("forwarder must not drop the event");

        assert!(
            matches!(completed, Event::BackgroundAgentCompleted { id: ev_id, .. } if ev_id == id),
        );
    }

    // F-152 / F-370: end-to-end wiring for the resource monitor. Today
    // `start()` has no per-child PID to attach, so it does NOT emit
    // `ResourceSample` (the daemon-PID guard in `ResourceMonitor::track`
    // makes that call a deliberate no-op). The forwarder wiring that
    // carries samples from the monitor's broadcast onto the registry's
    // event bus is still covered here — we just drive it by calling
    // `monitor().track(id, real_pid)` directly, mirroring what a future
    // step executor will do once per-child PIDs exist.

    /// Simulate a future step executor by handing the monitor a PID other
    /// than the daemon's — that's the real-world path the guard exists to
    /// protect, and the only case in which `ResourceSample` should fire.
    fn non_daemon_pid() -> u32 {
        std::process::id().wrapping_add(1)
    }

    #[tokio::test]
    async fn start_does_not_emit_resource_sample_for_background_agents() {
        // F-370 DoD invariant: a plain `start` (no real child PID) must
        // NOT stream `ResourceSample` on the registry bus — the daemon's
        // own process metrics are not a meaningful per-instance pill.
        // The UI falls back to the `—` placeholder because no event ever
        // arrives for this id.
        let reg = fresh_with_fast_monitor().await;
        let mut rx = reg.events();
        let id = reg.start("writer", Arc::from("p")).await.unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(Event::ResourceSample { instance_id, .. })) if instance_id == id => {
                    panic!(
                        "start() must not emit ResourceSample for bg agents \
                         until real per-child PIDs are available (F-370)"
                    );
                }
                _ => continue,
            }
        }
    }

    #[tokio::test]
    async fn monitor_track_with_real_pid_reaches_registry_event_bus() {
        // Forwarder wiring check: when a (future) caller hands the monitor
        // a real per-instance PID, the resulting `ResourceSample` reaches
        // the registry's subscribers on the same bus that carries
        // `BackgroundAgent*` events.
        let reg = fresh_with_fast_monitor().await;
        let mut rx = reg.events();
        let id = reg.start("writer", Arc::from("p")).await.unwrap();

        // Drain the Started event so the next ResourceSample stands out.
        match rx.recv().await.unwrap() {
            Event::BackgroundAgentStarted { .. } => {}
            other => panic!("expected Started, got {other:?}"),
        }

        reg.monitor().track(id.clone(), non_daemon_pid()).await;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut saw_sample = false;
        while std::time::Instant::now() < deadline {
            let next = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
            match next {
                Ok(Ok(Event::ResourceSample { instance_id, .. })) if instance_id == id => {
                    saw_sample = true;
                    break;
                }
                Ok(Ok(_other)) => continue,
                Ok(Err(_)) => break,
                Err(_elapsed) => continue,
            }
        }
        assert!(
            saw_sample,
            "registry event bus must surface ResourceSample when monitor is \
             tracking a real per-instance PID"
        );
    }

    #[tokio::test]
    async fn termination_stops_sample_emission_for_that_id() {
        // After the orchestrator drives an instance to terminal, the
        // `untrack(id)` side-effect in the forwarder must stop any further
        // `ResourceSample` events for that id. Any sample observed after a
        // wait window past completion fails the invariant.
        let reg = fresh_with_fast_monitor().await;
        let mut rx = reg.events();
        let id = reg.start("writer", Arc::from("p")).await.unwrap();
        // Stand in for a future step executor: wire the monitor to a real
        // PID so the forwarder has actual samples to propagate.
        reg.monitor().track(id.clone(), non_daemon_pid()).await;

        // Wait for at least one resource sample so we know the sampler is live.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await {
                Ok(Ok(Event::ResourceSample { instance_id, .. })) if instance_id == id => break,
                _ => continue,
            }
        }

        reg.orchestrator().stop(&id).await.unwrap();
        // Let the forwarder's `monitor.untrack(id)` land before draining.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Drain anything already queued.
        while rx.try_recv().is_ok() {}
        // Now sample for 200ms more — no ResourceSample for `id` should
        // arrive. Samples for other ids are fine (there are none in this
        // test, but the assertion tolerates them anyway).
        let post_deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        while std::time::Instant::now() < post_deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(Event::ResourceSample { instance_id, .. })) if instance_id == id => {
                    panic!("untrack failed: ResourceSample arrived after termination");
                }
                _ => continue,
            }
        }
    }
}
