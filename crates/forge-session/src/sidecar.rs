//! F-608 step 4: per-instance sidecar supervisor.
//!
//! Each background agent or root-session run that opts in to the sidecar
//! path is fronted by a [`SidecarSupervisor::spawn`] call. The supervisor
//! binds a per-instance Unix domain socket, forks the `forged-agent`
//! binary, drives the `Hello` / `HelloAck` handshake within the existing
//! handshake deadline (mirrors `crates/forge-session/src/server.rs`'s
//! `HANDSHAKE_DEADLINE_DEFAULT`), and stands up bidirectional pump
//! tasks so the daemon can push commands and receive event frames
//! asynchronously.
//!
//! Restart policy follows `docs/architecture/agent-sidecar.md` §3:
//! up to three crashes inside a sliding 60-second window are recovered
//! transparently with the same `instance_id`. The fourth crash escalates
//! to a [`Event::BackgroundAgentCompleted`] (failure path) emission on
//! the supplied [`EventSink`], and the supervisor task exits — the
//! caller observes the silence on the event channel and surfaces the
//! failure to the user the same way a panicking in-process orchestrator
//! does today.
//!
//! Step 4 deliberately does **not** wire the supervisor into
//! [`crate::bg_agents::BackgroundAgentRegistry`]; that integration lands
//! in step 5 behind the `FORGE_AGENT_SIDECAR` flag.
//!
//! # Layout
//!
//! ```text
//!                ┌──────────────────────────────────────┐
//!                │            forged (daemon)           │
//!                │  SidecarSupervisor::spawn(id, …)     │
//!                │     │       ▲                        │
//!                │     │ cmd   │ events                 │
//!                │     ▼       │                        │
//!                │  ┌──────────┴───┐                    │
//!                │  │ supervisor   │  retry counter,    │
//!                │  │ task         │  60 s window,      │
//!                │  │              │  child process     │
//!                │  └──┬─────▲─────┘                    │
//!                └─────│─────│─────────────────────────-┘
//!                      │ UDS │
//!                      ▼     │
//!                ┌─────────────────────┐
//!                │ forged-agent child  │  IpcEventSink → SidecarMessage::Event
//!                └─────────────────────┘
//! ```

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use forge_core::{AgentInstanceId, Event, EventSink};
use forge_ipc::sidecar::{
    SidecarHello, SidecarHelloAck, SidecarMessage, SidecarShutdown, SIDECAR_SCHEMA_VERSION,
};
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Maximum time the supervisor waits for a freshly-forked child to send
/// its `Hello` frame, mirrors `forge_session::server`'s private
/// `HANDSHAKE_DEADLINE_DEFAULT` (10 s with `FORGE_IPC_HANDSHAKE_DEADLINE_MS`
/// override). A silent child past this window is killed — better to fail
/// fast and let the restart loop recover than to pin a runtime task
/// indefinitely.
const HANDSHAKE_DEADLINE_DEFAULT: Duration = Duration::from_secs(10);

/// Restart-policy window per `docs/architecture/agent-sidecar.md` §3.
const RETRY_WINDOW: Duration = Duration::from_secs(60);

/// Maximum crashes inside [`RETRY_WINDOW`] before the supervisor
/// escalates to `BackgroundAgentCompleted` (failure path).
const MAX_RETRIES_IN_WINDOW: usize = 3;

/// Cooperative shutdown grace window the supervisor sends to the child
/// in its [`SidecarMessage::Shutdown`] frame. Matches the
/// architecture-doc §4 default.
const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_millis(2000);

/// Buffer depth on the daemon → child command channel. Sized for the
/// realistic burst of approval frames a single turn can produce; an
/// over-eager producer is back-pressured rather than allowed to balloon
/// memory.
const COMMAND_CHANNEL_DEPTH: usize = 64;

/// Resolve the supervisor's effective handshake deadline. Reads the
/// same `FORGE_IPC_HANDSHAKE_DEADLINE_MS` env override the daemon's
/// shell-facing handshake honors so `cargo test` can wind the value
/// down without recompiling.
fn handshake_deadline() -> Duration {
    std::env::var("FORGE_IPC_HANDSHAKE_DEADLINE_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(HANDSHAKE_DEADLINE_DEFAULT)
}

/// Parameters needed to launch one sidecar invocation. Owned by value
/// so the supervisor can re-issue the same `Hello` payload on a restart
/// without forcing the caller to keep a copy alive.
#[derive(Debug, Clone)]
pub struct SpawnParams {
    /// Initial daemon → child handshake payload. The `instance_id` field
    /// must match the `instance_id` arg passed to
    /// [`SidecarSupervisor::spawn`]; the supervisor logs and rejects a
    /// mismatch up front (see [`SidecarSupervisor::spawn`]).
    pub hello: SidecarHello,
}

/// Outbound side of the supervisor: send commands, observe shutdown.
///
/// `command_tx` is the same channel both the supervisor and any future
/// step-5 caller writes to. Sending while the child is mid-restart
/// queues the command; if the supervisor escalates to `Failed` before
/// flushing, the queued frames are dropped on the floor — the caller
/// observes the supervisor's [`Event::BackgroundAgentCompleted`]
/// emission and discards its outbox the same way the orchestrator does
/// for an in-process panic today.
#[derive(Debug)]
pub struct SidecarHandle {
    /// PID of the **current** child process. Atomic so the supervisor
    /// can update it after a transparent restart without forcing the
    /// caller to re-read a [`SidecarHandle`]. Read with
    /// [`SidecarHandle::pid`].
    pid: Arc<AtomicU32>,
    /// Logical instance id; immutable for the lifetime of this handle.
    pub instance_id: AgentInstanceId,
    /// Outbound command channel. Closed once the supervisor task exits.
    pub command_tx: mpsc::Sender<SidecarMessage>,
    /// Shutdown trigger. Sending the oneshot tells the supervisor to
    /// frame a [`SidecarMessage::Shutdown`] and join the child within
    /// the grace window; on timeout the supervisor falls back to
    /// SIGTERM-then-SIGKILL.
    shutdown: Option<oneshot::Sender<ShutdownRequest>>,
    /// JoinHandle for the supervisor task. The shutdown path awaits
    /// this so callers can confirm the child has fully exited.
    join: Option<JoinHandle<()>>,
}

impl SidecarHandle {
    /// Current child PID. Updates as the supervisor performs
    /// transparent restarts.
    pub fn pid(&self) -> u32 {
        self.pid.load(Ordering::Acquire)
    }

    /// Cooperatively shut the child down with the supplied grace
    /// window, then await the supervisor task's exit.
    ///
    /// The supervisor frames a [`SidecarMessage::Shutdown`] and waits
    /// up to `grace` for the child to exit cleanly. On timeout it
    /// escalates to SIGTERM, then SIGKILL after a further short
    /// window. The returned future resolves once the supervisor task
    /// itself has exited — i.e. all pump tasks have stopped and any
    /// final events have been drained.
    pub async fn shutdown(mut self) -> Result<()> {
        self.shutdown_with_grace(DEFAULT_SHUTDOWN_GRACE).await
    }

    /// Same as [`Self::shutdown`] with an explicit grace window.
    pub async fn shutdown_with_grace(&mut self, grace: Duration) -> Result<()> {
        if let Some(tx) = self.shutdown.take() {
            // The supervisor may already be gone (e.g. retry escalation
            // path); ignore a closed receiver.
            let _ = tx.send(ShutdownRequest { grace });
        }
        if let Some(join) = self.join.take() {
            // `await` only fails on a panicked task; surface that to the
            // caller as the supervisor failing to wind down cleanly.
            join.await.context("supervisor task panicked")?;
        }
        Ok(())
    }
}

impl Drop for SidecarHandle {
    fn drop(&mut self) {
        // Best-effort: trigger shutdown and let the supervisor task
        // SIGTERM/SIGKILL the child as needed. We can't await here; the
        // supervisor's own Drop on the `Child` handle has already been
        // wired to send SIGKILL on drop (tokio::process default), so
        // even in the worst case the child does not survive the daemon.
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(ShutdownRequest {
                grace: DEFAULT_SHUTDOWN_GRACE,
            });
        }
    }
}

/// Internal payload of the shutdown oneshot. A struct (rather than a
/// bare `()`) so we can extend with extra knobs (e.g. force-kill flag)
/// without breaking ABI.
#[derive(Debug, Clone, Copy)]
struct ShutdownRequest {
    grace: Duration,
}

/// Owns the `forged-agent` spawn discipline for one daemon. Holds the
/// per-session UDS directory and the path to the child binary.
///
/// Cheap to clone — both fields are shared `Arc`s. Multiple
/// background-agent registrations (step 5) may issue concurrent
/// [`Self::spawn`] calls.
#[derive(Debug, Clone)]
pub struct SidecarSupervisor {
    socket_dir: Arc<PathBuf>,
    forged_agent_path: Arc<PathBuf>,
}

impl SidecarSupervisor {
    /// Build a supervisor rooted at `socket_dir` (one per-instance UDS
    /// per spawn) and bound to the `forged_agent_path` binary.
    ///
    /// The `socket_dir` is created with mode `0o700` lazily on the
    /// first [`Self::spawn`] — the daemon's session bootstrap (today
    /// in `crates/forge-session/src/server.rs`) typically owns the
    /// parent runtime dir, but we tighten our own subdir defensively
    /// so a misconfigured operator pointing at a permissive parent
    /// still gets a 0o700 sidecar dir.
    pub fn new(socket_dir: PathBuf, forged_agent_path: PathBuf) -> Self {
        Self {
            socket_dir: Arc::new(socket_dir),
            forged_agent_path: Arc::new(forged_agent_path),
        }
    }

    /// Path to the directory the supervisor binds sidecar sockets in.
    /// Exposed for diagnostics / tests; production code rarely needs it.
    pub fn socket_dir(&self) -> &Path {
        self.socket_dir.as_path()
    }

    /// Path to the `forged-agent` binary the supervisor execs.
    pub fn forged_agent_path(&self) -> &Path {
        self.forged_agent_path.as_path()
    }

    /// Compute the per-instance UDS path. Public so tests can read
    /// the same path the supervisor will bind without duplicating the
    /// scheme.
    pub fn socket_path_for(&self, instance_id: &AgentInstanceId) -> PathBuf {
        self.socket_dir.join(format!("{}.sock", instance_id))
    }

    /// Spawn a sidecar for `instance_id` and stand up its bidirectional
    /// pump tasks.
    ///
    /// The returned [`SidecarHandle`] is wired to a supervisor task
    /// that owns the restart loop: up to three crashes inside a 60-
    /// second window are recovered transparently. On the fourth crash
    /// the supervisor emits [`Event::BackgroundAgentCompleted`] on
    /// `event_sink` and exits.
    ///
    /// `event_sink` receives every `Event` the child emits over the
    /// IPC plus the supervisor's own escalation events. Cloning the
    /// supplied `Arc` is cheap; the supervisor task holds its own
    /// reference for the lifetime of the spawn.
    pub async fn spawn(
        &self,
        instance_id: AgentInstanceId,
        params: SpawnParams,
        event_sink: Arc<dyn EventSink>,
    ) -> Result<SidecarHandle> {
        ensure_socket_dir(&self.socket_dir).await?;

        let socket_path = self.socket_path_for(&instance_id);

        // Bind, handshake, fork: do this synchronously up front so
        // `spawn` returns an error to the caller (rather than swallowing
        // it inside the supervisor task) when the very first attempt
        // fails. Subsequent restarts run inside the supervisor task and
        // surface as `BackgroundAgentCompleted` on retry exhaustion.
        let initial = self
            .launch_and_handshake(&instance_id, &params.hello, &socket_path)
            .await
            .with_context(|| format!("first sidecar spawn for {instance_id}"))?;

        let (command_tx, command_rx) = mpsc::channel::<SidecarMessage>(COMMAND_CHANNEL_DEPTH);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<ShutdownRequest>();
        let pid_cell = Arc::new(AtomicU32::new(initial.child_pid));

        let supervisor = SupervisorTask {
            socket_dir: self.socket_dir.clone(),
            forged_agent_path: self.forged_agent_path.clone(),
            socket_path: socket_path.clone(),
            instance_id: instance_id.clone(),
            params,
            event_sink: event_sink.clone(),
            command_rx,
            shutdown_rx,
            pid_cell: pid_cell.clone(),
            crash_log: VecDeque::with_capacity(MAX_RETRIES_IN_WINDOW + 1),
        };

        let join = tokio::spawn(supervisor.run(initial));

        Ok(SidecarHandle {
            pid: pid_cell,
            instance_id,
            command_tx,
            shutdown: Some(shutdown_tx),
            join: Some(join),
        })
    }

    /// Bind the per-instance UDS, fork the child binary, and complete
    /// the `Hello` / `HelloAck` handshake within the configured
    /// deadline. Returns the assembled connection halves + child handle
    /// the supervisor task will pump until the child exits or
    /// [`SidecarMessage::Shutdown`] is acknowledged.
    async fn launch_and_handshake(
        &self,
        instance_id: &AgentInstanceId,
        hello: &SidecarHello,
        socket_path: &Path,
    ) -> Result<LiveSidecar> {
        if hello.instance_id.to_string() != instance_id.to_string() {
            anyhow::bail!(
                "spawn params instance_id {} does not match supervisor instance_id {}",
                hello.instance_id,
                instance_id
            );
        }
        let listener = bind_uds_safely(socket_path).await?;
        // Tighten the socket file to 0o600 immediately to match the
        // server's discipline (`forge-session/src/server.rs`). bind(2)
        // creates the file with `0o777 & ~umask`, which is world-
        // connectable on most systems.
        if let Err(e) =
            tokio::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600)).await
        {
            // Best-effort: tests under `/tmp` can hit ENOTSUP on some
            // tmpfs configs. Log but don't bail — the parent dir's
            // 0o700 mode is the real defense.
            debug!(
                error = %e,
                "set_permissions(0o600) failed for sidecar socket; relying on parent dir"
            );
        }

        let mut child = Command::new(self.forged_agent_path.as_path())
            .arg("--socket")
            .arg(socket_path)
            .arg("--instance-id")
            .arg(instance_id.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn forged-agent at {}",
                    self.forged_agent_path.display()
                )
            })?;

        let child_pid = child.id().ok_or_else(|| {
            anyhow::anyhow!("forged-agent has no PID immediately after spawn (already exited?)")
        })?;

        // Accept within the same handshake deadline used for the
        // shell-facing UDS. A child that crashes before connect()
        // surfaces here as a timeout, which the supervisor task will
        // count as a crash on its retry loop.
        let deadline = handshake_deadline();
        let (stream, _addr) = match tokio::time::timeout(deadline, listener.accept()).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                let _ = child.kill().await;
                return Err(anyhow::Error::from(e).context("accept on sidecar uds"));
            }
            Err(_) => {
                let _ = child.kill().await;
                anyhow::bail!(
                    "forged-agent did not connect within {:?} (instance_id={})",
                    deadline,
                    instance_id
                );
            }
        };
        let (mut reader, mut writer) = tokio::io::split(stream);

        // Read the child's Hello. The architecture-doc §2 protocol has
        // the *child* send Hello first; we validate the proto version
        // and instance_id before completing the handshake.
        let mut buf = Vec::new();
        let hello_frame: SidecarMessage = match tokio::time::timeout(
            deadline,
            forge_ipc::read_frame_into::<_, SidecarMessage>(&mut reader, &mut buf),
        )
        .await
        {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => {
                let _ = child.kill().await;
                return Err(e.context("read Hello from forged-agent"));
            }
            Err(_) => {
                let _ = child.kill().await;
                anyhow::bail!("forged-agent did not send Hello within {:?}", deadline);
            }
        };
        match &hello_frame {
            SidecarMessage::Hello(h) => {
                if h.instance_id.to_string() != instance_id.to_string() {
                    let _ = child.kill().await;
                    anyhow::bail!(
                        "forged-agent reported instance_id={}, expected {}",
                        h.instance_id,
                        instance_id
                    );
                }
            }
            other => {
                let _ = child.kill().await;
                anyhow::bail!("expected Hello from sidecar, got {other:?}");
            }
        }

        // HelloAck the daemon's PID so the child can pin it for
        // tracing. The architecture-doc §9 wires *child* PID into the
        // ResourceMonitor on the daemon side; this ack is just for
        // symmetry with the existing IPC protocol.
        let ack = SidecarMessage::HelloAck(SidecarHelloAck {
            pid: std::process::id(),
            started_at: Utc::now(),
            schema_version: SIDECAR_SCHEMA_VERSION,
        });
        if let Err(e) = forge_ipc::write_frame(&mut writer, &ack).await {
            let _ = child.kill().await;
            return Err(e.context("write HelloAck to forged-agent"));
        }

        info!(
            target: "forge_session::sidecar",
            instance_id = %instance_id,
            child_pid,
            socket = %socket_path.display(),
            "sidecar handshake complete",
        );

        // Optionally pass the original daemon → child Hello payload
        // forward as a follow-up frame. Step 1's protocol shape is
        // child-initiated; the supervisor still needs to deliver
        // `agent_def`, `provider_spec`, etc. to the child for the
        // run-turn body to consume in step 5. We send the payload as a
        // second `Hello` frame so the child can pick it up via the
        // same dispatch loop. The current step-3 stub binary ignores
        // it as a "non-RunTurn daemon message" — wiring step 5 will
        // teach the child to consume it.
        let daemon_hello = SidecarMessage::Hello(hello.clone());
        if let Err(e) = forge_ipc::write_frame(&mut writer, &daemon_hello).await {
            // Non-fatal in step 4: the child stub ignores the frame. A
            // failure here implies the connection broke between ack and
            // first command — fold it into the restart loop by treating
            // it as a fresh crash.
            let _ = child.kill().await;
            return Err(e.context("forward daemon Hello to forged-agent"));
        }

        Ok(LiveSidecar {
            child,
            child_pid,
            reader,
            writer,
        })
    }
}

/// State the supervisor task threads through the restart loop.
struct SupervisorTask {
    socket_dir: Arc<PathBuf>,
    forged_agent_path: Arc<PathBuf>,
    socket_path: PathBuf,
    instance_id: AgentInstanceId,
    params: SpawnParams,
    event_sink: Arc<dyn EventSink>,
    command_rx: mpsc::Receiver<SidecarMessage>,
    shutdown_rx: oneshot::Receiver<ShutdownRequest>,
    pid_cell: Arc<AtomicU32>,
    /// Crash timestamps inside the rolling [`RETRY_WINDOW`]. Front of
    /// the queue is the oldest; entries older than the window are
    /// pruned on every push.
    crash_log: VecDeque<Instant>,
}

/// Active connection + child handle the supervisor pumps until the
/// next exit / crash.
struct LiveSidecar {
    child: Child,
    child_pid: u32,
    reader: ReadHalf<UnixStream>,
    writer: WriteHalf<UnixStream>,
}

/// Outcome of pumping one live sidecar to its end.
enum LifecycleOutcome {
    /// The child exited cleanly (status 0) following our `Shutdown`
    /// frame, or the caller's `shutdown_rx` fired and we drove the
    /// child down successfully. No retry — supervisor exits.
    Stopped,
    /// The child exited unexpectedly (non-zero status, EOF without a
    /// preceding `Shutdown`, panic, kill). Retry policy decides
    /// whether to re-spawn.
    Crashed(String),
}

impl SupervisorTask {
    /// Drive the lifecycle of one or more child invocations until either
    /// a clean shutdown or retry-budget exhaustion.
    async fn run(mut self, mut live: LiveSidecar) {
        loop {
            let outcome = self.pump_until_exit(&mut live).await;
            match outcome {
                LifecycleOutcome::Stopped => {
                    info!(
                        target: "forge_session::sidecar",
                        instance_id = %self.instance_id,
                        "sidecar supervisor exiting after clean shutdown"
                    );
                    return;
                }
                LifecycleOutcome::Crashed(reason) => {
                    warn!(
                        target: "forge_session::sidecar",
                        instance_id = %self.instance_id,
                        reason = %reason,
                        "sidecar crashed; evaluating restart policy"
                    );
                    // Drain any residual child handle (already exited
                    // by the time we get here, but kill() is idempotent
                    // and prevents zombies on the slim race where the
                    // process recorded the EOF before fully exiting).
                    let _ = live.child.kill().await;

                    let now = Instant::now();
                    self.prune_crash_log(now);
                    self.crash_log.push_back(now);

                    if self.crash_log.len() > MAX_RETRIES_IN_WINDOW {
                        error!(
                            target: "forge_session::sidecar",
                            instance_id = %self.instance_id,
                            crashes = self.crash_log.len(),
                            window_secs = RETRY_WINDOW.as_secs(),
                            "sidecar exhausted retry budget; escalating to BackgroundAgentCompleted (failure)"
                        );
                        self.emit_failure(reason).await;
                        return;
                    }

                    // Re-spawn with the same `instance_id`. The orphan
                    // socket from the previous bind is unlinked by
                    // `bind_uds_safely`'s recovery path on the next
                    // attempt.
                    let supervisor_view = SidecarSupervisor {
                        socket_dir: self.socket_dir.clone(),
                        forged_agent_path: self.forged_agent_path.clone(),
                    };
                    match supervisor_view
                        .launch_and_handshake(
                            &self.instance_id,
                            &self.params.hello,
                            &self.socket_path,
                        )
                        .await
                    {
                        Ok(next) => {
                            self.pid_cell.store(next.child_pid, Ordering::Release);
                            live = next;
                            info!(
                                target: "forge_session::sidecar",
                                instance_id = %self.instance_id,
                                child_pid = live.child_pid,
                                attempt = self.crash_log.len(),
                                "sidecar restarted within retry window"
                            );
                        }
                        Err(e) => {
                            // A failed re-spawn counts as the same
                            // crash event — retry budget already
                            // pushed above. Loop again so the next
                            // crash either succeeds or escalates.
                            warn!(
                                target: "forge_session::sidecar",
                                instance_id = %self.instance_id,
                                error = %e,
                                "sidecar restart failed; will count toward retry budget on next loop"
                            );
                            // Synthesize a placeholder LiveSidecar so
                            // the next iteration's `pump_until_exit`
                            // immediately observes the dead child and
                            // re-enters the restart branch.
                            //
                            // We can't construct a `LiveSidecar`
                            // without a real child — instead, fall
                            // through to a fresh emit_failure check
                            // by registering the failure as a crash
                            // and looping; if budget is exhausted,
                            // escalate now.
                            self.crash_log.push_back(Instant::now());
                            if self.crash_log.len() > MAX_RETRIES_IN_WINDOW {
                                error!(
                                    target: "forge_session::sidecar",
                                    instance_id = %self.instance_id,
                                    "sidecar restart-after-crash budget exhausted; escalating"
                                );
                                self.emit_failure(format!("restart failed: {e}")).await;
                                return;
                            }
                            // Otherwise sleep briefly to avoid a hot
                            // loop on a totally broken binary, then
                            // retry the launch directly.
                            tokio::time::sleep(Duration::from_millis(250)).await;
                            match supervisor_view
                                .launch_and_handshake(
                                    &self.instance_id,
                                    &self.params.hello,
                                    &self.socket_path,
                                )
                                .await
                            {
                                Ok(next) => {
                                    self.pid_cell.store(next.child_pid, Ordering::Release);
                                    live = next;
                                }
                                Err(e2) => {
                                    error!(
                                        target: "forge_session::sidecar",
                                        instance_id = %self.instance_id,
                                        error = %e2,
                                        "sidecar relaunch retry failed; escalating"
                                    );
                                    self.emit_failure(format!("relaunch retry failed: {e2}"))
                                        .await;
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Pump one connected child until it exits, the caller fires a
    /// shutdown, or we observe EOF / IO error on the read half.
    async fn pump_until_exit(&mut self, live: &mut LiveSidecar) -> LifecycleOutcome {
        let mut buf = Vec::new();
        loop {
            tokio::select! {
                biased;
                shutdown = &mut self.shutdown_rx => {
                    let req = shutdown.unwrap_or(ShutdownRequest { grace: DEFAULT_SHUTDOWN_GRACE });
                    return self.drive_shutdown(live, req).await;
                }
                cmd = self.command_rx.recv() => {
                    match cmd {
                        Some(msg) => {
                            if let Err(e) = forge_ipc::write_frame(&mut live.writer, &msg).await {
                                warn!(
                                    target: "forge_session::sidecar",
                                    instance_id = %self.instance_id,
                                    error = %e,
                                    "command write to sidecar failed"
                                );
                                return LifecycleOutcome::Crashed(format!("command write failed: {e}"));
                            }
                        }
                        None => {
                            // The handle was dropped without an
                            // explicit shutdown. Treat as a graceful
                            // request with the default grace.
                            return self
                                .drive_shutdown(live, ShutdownRequest { grace: DEFAULT_SHUTDOWN_GRACE })
                                .await;
                        }
                    }
                }
                read = forge_ipc::read_frame_into::<_, SidecarMessage>(&mut live.reader, &mut buf) => {
                    match read {
                        Ok(frame) => {
                            if let Some(reason) = self.handle_inbound(frame).await {
                                // `Crashed` frame: emit and treat as a
                                // crash so the restart loop kicks in.
                                return LifecycleOutcome::Crashed(reason);
                            }
                        }
                        Err(e) => {
                            // EOF or IO error mid-stream. Wait briefly
                            // for the child's exit code to disambiguate
                            // a clean shutdown from a crash.
                            return self.classify_exit(live, e.to_string()).await;
                        }
                    }
                }
            }
        }
    }

    /// Drive a cooperative shutdown: frame `Shutdown { grace_ms }`,
    /// wait up to `grace` for the child to exit, then SIGTERM, then
    /// SIGKILL.
    async fn drive_shutdown(
        &mut self,
        live: &mut LiveSidecar,
        req: ShutdownRequest,
    ) -> LifecycleOutcome {
        let grace_ms = req.grace.as_millis() as u64;
        let frame = SidecarMessage::Shutdown(SidecarShutdown { grace_ms });
        if let Err(e) = forge_ipc::write_frame(&mut live.writer, &frame).await {
            debug!(
                target: "forge_session::sidecar",
                instance_id = %self.instance_id,
                error = %e,
                "shutdown write failed; falling through to signal"
            );
        }
        // Half-close our write side so the child observes EOF on its
        // read after draining.
        let _ = live.writer.shutdown().await;

        match tokio::time::timeout(req.grace, live.child.wait()).await {
            Ok(Ok(status)) => {
                debug!(
                    target: "forge_session::sidecar",
                    instance_id = %self.instance_id,
                    status = ?status,
                    "child exited within grace window"
                );
            }
            Ok(Err(e)) => {
                warn!(
                    target: "forge_session::sidecar",
                    instance_id = %self.instance_id,
                    error = %e,
                    "child wait() failed during shutdown"
                );
            }
            Err(_) => {
                // Grace expired; escalate.
                warn!(
                    target: "forge_session::sidecar",
                    instance_id = %self.instance_id,
                    "shutdown grace expired; sending SIGTERM"
                );
                let pid = live.child_pid as i32;
                #[cfg(unix)]
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
                let sigkill_window = Duration::from_millis(500);
                if tokio::time::timeout(sigkill_window, live.child.wait())
                    .await
                    .is_err()
                {
                    warn!(
                        target: "forge_session::sidecar",
                        instance_id = %self.instance_id,
                        "SIGTERM grace expired; sending SIGKILL"
                    );
                    let _ = live.child.start_kill();
                    let _ = live.child.wait().await;
                }
            }
        }
        // Best-effort: unlink the socket so the next spawn doesn't have
        // to recover it via the EADDRINUSE branch.
        let _ = tokio::fs::remove_file(&self.socket_path).await;
        LifecycleOutcome::Stopped
    }

    /// Disambiguate "child closed cleanly" from "child crashed" after
    /// an EOF on the read half.
    async fn classify_exit(
        &mut self,
        live: &mut LiveSidecar,
        read_error: String,
    ) -> LifecycleOutcome {
        // Wait briefly for the child's status. A clean shutdown that
        // raced our shutdown branch (e.g. the child closed write before
        // we sent `Shutdown`) still surfaces as a non-error exit.
        let wait = tokio::time::timeout(Duration::from_secs(2), live.child.wait()).await;
        match wait {
            Ok(Ok(status)) if status.success() => {
                debug!(
                    target: "forge_session::sidecar",
                    instance_id = %self.instance_id,
                    "child exited 0 after EOF; treating as clean shutdown"
                );
                LifecycleOutcome::Stopped
            }
            Ok(Ok(status)) => {
                LifecycleOutcome::Crashed(format!("non-zero exit: {status}; eof: {read_error}"))
            }
            Ok(Err(e)) => LifecycleOutcome::Crashed(format!("wait failed: {e}; eof: {read_error}")),
            Err(_) => {
                // Child is still alive after EOF on its socket. Force
                // kill and treat as a crash.
                let _ = live.child.kill().await;
                LifecycleOutcome::Crashed(format!("eof but child alive: {read_error}"))
            }
        }
    }

    /// Process one inbound frame from the child. Returns `Some(reason)`
    /// when the frame should escalate the supervisor into the crash
    /// branch (a `Crashed` panic dump from the child's panic hook).
    async fn handle_inbound(&self, frame: SidecarMessage) -> Option<String> {
        match frame {
            SidecarMessage::Event(ev) => {
                if let Err(e) = self.event_sink.emit(ev.event).await {
                    warn!(
                        target: "forge_session::sidecar",
                        instance_id = %self.instance_id,
                        error = %e,
                        "event_sink emit failed"
                    );
                }
                None
            }
            SidecarMessage::Heartbeat(_) => {
                // F-608 step 4: heartbeat-watchdog deferred — the
                // EOF-on-read path catches an unresponsive child via
                // its TCP-style RST. A 5-second silence watchdog lands
                // when we wire ResourceMonitor to surface stuck
                // sidecars in step 9.
                None
            }
            SidecarMessage::ToolCallApprovalRequest(_) => {
                // Step 5 forwards these to the shell. For step 4 the
                // event sink-only path is sufficient — drop with a
                // debug log.
                debug!(
                    target: "forge_session::sidecar",
                    instance_id = %self.instance_id,
                    "tool-call approval request received; not yet forwarded (step 5)"
                );
                None
            }
            SidecarMessage::Crashed(c) => Some(format!(
                "panic: {} ({})",
                c.panic_message,
                c.backtrace.unwrap_or_default()
            )),
            SidecarMessage::HelloAck(_) | SidecarMessage::Hello(_) => {
                warn!(
                    target: "forge_session::sidecar",
                    instance_id = %self.instance_id,
                    "unexpected handshake frame after handshake complete; ignoring"
                );
                None
            }
            // Daemon → sidecar variants on the inbound path are a
            // protocol violation. Don't take the supervisor down on a
            // confused peer; warn and continue.
            SidecarMessage::RunTurn(_)
            | SidecarMessage::Credentials(_)
            | SidecarMessage::ToolCallApproved(_)
            | SidecarMessage::ToolCallRejected(_)
            | SidecarMessage::CompactTranscript(_)
            | SidecarMessage::Shutdown(_) => {
                warn!(
                    target: "forge_session::sidecar",
                    instance_id = %self.instance_id,
                    "received daemon→sidecar variant on inbound channel; ignoring"
                );
                None
            }
        }
    }

    /// Emit `BackgroundAgentCompleted` (failure path) on retry
    /// exhaustion. The architecture-doc §3 also calls for an
    /// `AgentEvent::Failed`; in the current `forge_core::Event` shape
    /// that is delivered by `forge_agents::Orchestrator::fail` (which
    /// step 5's wiring will invoke) — for the standalone supervisor
    /// the visible escalation is the completion event.
    async fn emit_failure(&self, _reason: String) {
        let event = Event::BackgroundAgentCompleted {
            id: self.instance_id.clone(),
            at: Utc::now(),
        };
        if let Err(e) = self.event_sink.emit(event).await {
            warn!(
                target: "forge_session::sidecar",
                instance_id = %self.instance_id,
                error = %e,
                "failed to emit BackgroundAgentCompleted on retry exhaustion"
            );
        }
        // Best-effort cleanup of the bound socket.
        let _ = tokio::fs::remove_file(&self.socket_path).await;
    }

    /// Drop crash-log entries older than [`RETRY_WINDOW`].
    fn prune_crash_log(&mut self, now: Instant) {
        while let Some(&front) = self.crash_log.front() {
            if now.duration_since(front) > RETRY_WINDOW {
                self.crash_log.pop_front();
            } else {
                break;
            }
        }
    }
}

/// Mode-700 the per-supervisor socket dir. Idempotent: a pre-existing
/// dir is left in place and chmodded back to 0o700 defensively.
async fn ensure_socket_dir(dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("create sidecar socket dir at {}", dir.display()))?;
    let _ = tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await;
    Ok(())
}

/// Bind a `UnixListener` at `path` without the classic pre-unlink
/// TOCTOU. Mirrors the discipline in
/// `crates/forge-session/src/server.rs::bind_uds_safely`: try `bind`
/// first; on `EADDRINUSE` only unlink an entry that `symlink_metadata`
/// confirms is a real socket file (rejecting symlinks and regular
/// files that an attacker could plant in a shared parent dir). The
/// architecture-doc §4 names this contract for sidecar sockets.
async fn bind_uds_safely(path: &Path) -> Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            // `EADDRINUSE` means either another supervisor is live
            // (extremely unlikely — same-instance restart serializes
            // through this same supervisor task) or a previous run
            // crashed mid-shutdown leaving the socket file behind.
            // Verify the leftover entry is a real socket before
            // unlinking.
            let meta = tokio::fs::symlink_metadata(path).await.with_context(|| {
                format!(
                    "failed to stat {} while recovering from EADDRINUSE",
                    path.display()
                )
            })?;
            if !meta.file_type().is_socket() {
                anyhow::bail!(
                    "refusing to unlink {}: entry is not a socket (type={:?})",
                    path.display(),
                    meta.file_type()
                );
            }
            tokio::fs::remove_file(path).await.with_context(|| {
                format!(
                    "failed to unlink stale sidecar socket at {}",
                    path.display()
                )
            })?;
            UnixListener::bind(path)
                .with_context(|| format!("retry bind failed at {}", path.display()))
        }
        Err(e) => Err(e).with_context(|| format!("bind failed at {}", path.display())),
    }
}
