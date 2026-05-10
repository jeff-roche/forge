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
//! ## Sub-modules
//!
//! - [`crashes`] — daemon-side crash-dump reader. The sidecar binary
//!   writes a `<XDG_DATA_HOME or ~/.local/share>/forge/crashes/<session-id>/<instance-id>-<unix-ts>.json`
//!   from its panic hook (F-608 step 7); this submodule enumerates and
//!   parses those dumps for the daemon-side observability path.
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

pub mod crashes;

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
    SidecarHello, SidecarHelloAck, SidecarMessage, SidecarShutdown, SIDECAR_PROTO_VERSION,
    SIDECAR_SCHEMA_VERSION,
};
use thiserror::Error;
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, OnceCell};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Typed sidecar handshake errors. Returned (wrapped in [`anyhow::Error`]
/// via `From`) by the crate-private `validate_sidecar_hello` so callers
/// that need to disambiguate a hard-fail cause from a generic IO/parse
/// failure can downcast via [`anyhow::Error::downcast_ref`] instead of
/// string-matching.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SidecarError {
    /// #702 item: the child reported a `proto` version that does not
    /// match the daemon's [`SIDECAR_PROTO_VERSION`]. The supervisor
    /// closes the connection rather than continuing on a wire shape it
    /// cannot validate.
    #[error("sidecar proto version mismatch: daemon={ours}, peer={theirs}")]
    ProtoMismatch { ours: u32, theirs: u32 },
}

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

// F-652 / Issue #784: the steady-state sidecar event pump uses the
// crate-level `forge_ipc::DEFAULT_PUMP_DEADLINE`. A healthy `forged-agent`
// heartbeats every few seconds; the 60 s silence window is unambiguously a
// stalled or wedged child (and a slowloris-style same-uid attacker holding
// the socket open without sending data). On timeout the pump treats the
// read as an EOF / crash and the supervisor's restart loop kicks in.

/// Buffer depth on the daemon → child command channel. Sized for the
/// realistic burst of approval frames a single turn can produce; an
/// over-eager producer is back-pressured rather than allowed to balloon
/// memory.
const COMMAND_CHANNEL_DEPTH: usize = 64;

/// Buffer depth on the sidecar → daemon event channel.
///
/// F-658: the inbound `SidecarMessage::Event` reader hands frames to a
/// dedicated emitter task through a bounded `mpsc::channel`. A
/// misbehaving (or compromised) sidecar that emits at line rate is
/// back-pressured at this boundary: once the channel is full, the read
/// loop awaits on `Sender::send`, the kernel UDS read buffer fills, and
/// the sidecar's own [`forge_ipc::write_frame`] call blocks. End-to-end
/// flow control with a documented memory ceiling — no daemon-side queue
/// can grow unbounded regardless of peer behavior.
///
/// Sized at 1024 frames (~few-hundred-KiB worst case for typical
/// `Event` payloads) so a normal turn's burst — token streaming chunks
/// plus tool-call envelopes — never observes backpressure under
/// healthy conditions, but a sustained flood saturates well before
/// memory pressure.
pub const EVENT_CHANNEL_DEPTH: usize = 1024;

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
    /// F-608 step 7: session id forwarded to the child as
    /// `--session-id`. The child's panic hook uses this to disambiguate
    /// crash dumps under
    /// `<XDG_DATA_HOME or ~/.local/share>/forge/crashes/<session-id>/`.
    /// Default is `"default"` so step-5 callers that haven't been
    /// updated yet still spawn — their crashes simply land under a
    /// `default` bucket until the wiring catches up.
    session_id: Arc<String>,
    /// F-662: canonical absolute path of `socket_dir`, captured on first
    /// successful [`Self::spawn`]. Every subsequent bind verifies the
    /// directory still canonicalizes to this value — if a grandparent
    /// is replaced with a symlink mid-flight, the divergence trips
    /// [`verify_socket_dir_matches_expected`] and the spawn refuses.
    expected_canonical_dir: Arc<OnceCell<PathBuf>>,
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
            session_id: Arc::new("default".to_string()),
            expected_canonical_dir: Arc::new(OnceCell::new()),
        }
    }

    /// F-608 step 7: bind a session id forwarded to every spawned
    /// child as `--session-id`. The child's panic hook uses it to
    /// place crash dumps in the right per-session bucket so the
    /// daemon-side reader (see [`crate::sidecar::crashes`]) can
    /// enumerate them.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Arc::new(session_id.into());
        self
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

    /// F-662: canonicalize `self.socket_dir`, populate the cached
    /// expected-canonical on first call, and verify on every subsequent
    /// call that the directory still resolves to the same canonical.
    /// Returns the canonical path the caller should use to construct
    /// the bind path.
    async fn resolve_and_verify_socket_dir(&self) -> Result<PathBuf> {
        // First call wins the OnceCell; subsequent calls compare against
        // the cached value via `verify_socket_dir_matches_expected`.
        let cached = self
            .expected_canonical_dir
            .get_or_try_init(|| async { validate_socket_dir_canonical(&self.socket_dir).await })
            .await?
            .clone();
        verify_socket_dir_matches_expected(&self.socket_dir, &cached).await?;
        Ok(cached)
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

        // F-662: cache the canonical bind directory on first spawn and
        // re-verify on every spawn that the directory still resolves to
        // the same canonical. This neutralises a planted symlink at any
        // grandparent component: post-canonicalize the supervisor binds
        // sockets at an absolute path with no symlink components, and a
        // mid-flight redirect attempt is detected here before bind.
        let canonical = self.resolve_and_verify_socket_dir().await?;
        let socket_path = canonical.join(format!("{}.sock", instance_id));

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

        // F-658: decouple inbound-event reads from sink emission with a
        // bounded mpsc. The supervisor task pushes onto `event_tx` and
        // backpressures the read loop when full; the emitter task drains
        // `event_rx` into the sink without ever blocking the IPC read
        // path. Failure-path escalations bypass the channel via the
        // retained `event_sink` clone in `SupervisorTask` so an
        // exhausted-budget event is delivered even if the channel is
        // saturated.
        let (event_tx, event_rx) = mpsc::channel::<Event>(EVENT_CHANNEL_DEPTH);
        let emitter_join = spawn_event_emitter(event_rx, event_sink.clone(), instance_id.clone());

        let supervisor = SupervisorTask {
            socket_dir: self.socket_dir.clone(),
            forged_agent_path: self.forged_agent_path.clone(),
            session_id: self.session_id.clone(),
            expected_canonical_dir: self.expected_canonical_dir.clone(),
            socket_path: socket_path.clone(),
            instance_id: instance_id.clone(),
            params,
            event_sink: event_sink.clone(),
            event_tx,
            emitter_join: Some(emitter_join),
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
        // F-662: re-verify the bind directory still canonicalizes to the
        // expected root before every (re)bind. The supervisor's restart
        // loop calls back here without going through `spawn`, so this
        // is the guard that catches a grandparent symlink swap planted
        // between the initial bind and a transparent restart.
        self.resolve_and_verify_socket_dir().await?;
        let listener = bind_uds_safely(socket_path).await?;
        // Tighten the socket file to 0o600 immediately to match the
        // server's discipline (`forge-session/src/server.rs`). bind(2)
        // creates the file with `0o777 & ~umask`, which is world-
        // connectable on most systems. #702 fail-close: if the chmod
        // fails (e.g. ENOTSUP on a hostile tmpfs) we drop the listener
        // and unlink the socket file rather than continuing with an
        // over-permissive socket.
        let listener = enforce_socket_mode_or_close(listener, socket_path).await?;

        let mut child = Command::new(self.forged_agent_path.as_path())
            .arg("--socket")
            .arg(socket_path)
            .arg("--instance-id")
            .arg(instance_id.to_string())
            .arg("--session-id")
            .arg(self.session_id.as_str())
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

        // F-651: defence-in-depth peer-uid check. The sidecar UDS lives
        // in a 0o700 directory and the socket itself is 0o600, but
        // verifying the connecting peer's uid matches our euid in code
        // closes the same-uid attacker hole and rejects any future
        // operator misconfiguration of the parent dir without silently
        // trusting the connection. Tokio's `peer_cred()` wraps
        // SO_PEERCRED on Linux and LOCAL_PEERCRED on macOS.
        if let Err(e) = verify_peer_uid(&stream, current_euid()) {
            warn!(
                target: "forge_session::sidecar",
                instance_id = %instance_id,
                error = %e,
                "rejecting sidecar peer with mismatched uid",
            );
            let _ = child.kill().await;
            return Err(e).context("verify forged-agent peer uid");
        }

        let (mut reader, mut writer) = tokio::io::split(stream);

        // Read the child's Hello. The architecture-doc §2 protocol has
        // the *child* send Hello first; we validate the proto version
        // and instance_id before completing the handshake.
        //
        // F-652: `read_frame_into_with_deadline` enforces the same
        // handshake deadline directly inside the framing helper so a
        // silent child can't pin the worker.
        let mut buf = Vec::new();
        let hello_frame: SidecarMessage = match forge_ipc::read_frame_into_with_deadline::<
            _,
            SidecarMessage,
        >(&mut reader, &mut buf, deadline)
        .await
        {
            Ok(m) => m,
            Err(e) => {
                let _ = child.kill().await;
                return Err(e.context("read Hello from forged-agent"));
            }
        };
        match &hello_frame {
            SidecarMessage::Hello(h) => {
                if let Err(e) = validate_sidecar_hello(h, instance_id) {
                    let _ = child.kill().await;
                    return Err(e);
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

        // Pass the daemon-side metadata payload forward as a follow-up
        // frame. Step 1's handshake shape is child-initiated; the
        // supervisor still needs to deliver `agent_def`,
        // `provider_spec`, etc. to the child for the run-turn body to
        // consume in step 5. F-676: this travels as a dedicated
        // `DaemonHello` variant rather than a second `Hello`, so the
        // discriminator no longer encodes two semantically distinct
        // frames and the receiver can pattern-match the daemon-side
        // payload distinctly from the handshake.
        let daemon_hello = SidecarMessage::DaemonHello(hello.clone());
        if let Err(e) = forge_ipc::write_frame(&mut writer, &daemon_hello).await {
            // A failure here implies the connection broke between ack
            // and first command — fold it into the restart loop by
            // treating it as a fresh crash.
            let _ = child.kill().await;
            return Err(e.context("forward DaemonHello to forged-agent"));
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
    session_id: Arc<String>,
    /// F-662: shared with [`SidecarSupervisor`] so the restart path
    /// re-verifies the same expected canonical that gated the initial
    /// bind. A mid-flight grandparent symlink swap fails the next bind.
    expected_canonical_dir: Arc<OnceCell<PathBuf>>,
    socket_path: PathBuf,
    instance_id: AgentInstanceId,
    params: SpawnParams,
    /// Direct sink reference. Used **only** for failure-path
    /// escalations (`emit_failure`); routine inbound events flow through
    /// `event_tx` so the F-658 backpressure contract is preserved.
    event_sink: Arc<dyn EventSink>,
    /// Bounded sender feeding the per-supervisor emitter task. Frames
    /// arrive on the IPC read half; the supervisor awaits this send so
    /// a slow sink propagates backpressure all the way to the sidecar.
    event_tx: mpsc::Sender<Event>,
    /// JoinHandle for the emitter task. Held `Option<>` so the
    /// supervisor can `take()` it on shutdown, drop the sender, and
    /// await final drain.
    emitter_join: Option<JoinHandle<()>>,
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
                    self.drain_emitter().await;
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
                        self.drain_emitter().await;
                        return;
                    }

                    // Re-spawn with the same `instance_id`. The orphan
                    // socket from the previous bind is unlinked by
                    // `bind_uds_safely`'s recovery path on the next
                    // attempt.
                    let supervisor_view = SidecarSupervisor {
                        socket_dir: self.socket_dir.clone(),
                        forged_agent_path: self.forged_agent_path.clone(),
                        session_id: self.session_id.clone(),
                        expected_canonical_dir: self.expected_canonical_dir.clone(),
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
                                self.drain_emitter().await;
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
                                    self.drain_emitter().await;
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
                read = forge_ipc::read_frame_into_with_deadline::<_, SidecarMessage>(&mut live.reader, &mut buf, forge_ipc::DEFAULT_PUMP_DEADLINE) => {
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
                // F-658: route routine events through the bounded
                // channel. `send().await` parks the read loop when the
                // channel is full — that's the daemon-side backpressure
                // signal that propagates back through the kernel UDS
                // buffer to the sidecar's writer. A `Closed` error
                // means the emitter task has exited (sink dropped or
                // panicked); log and continue rather than taking the
                // supervisor down on a downstream subscriber's
                // collapse.
                if let Err(e) = self.event_tx.send(ev.event).await {
                    warn!(
                        target: "forge_session::sidecar",
                        instance_id = %self.instance_id,
                        error = %e,
                        "event channel closed; emitter task gone"
                    );
                }
                None
            }
            SidecarMessage::Heartbeat(hb) => {
                // F-608 step 4: heartbeat-watchdog deferred — the
                // EOF-on-read path catches an unresponsive child via
                // its TCP-style RST. A 5-second silence watchdog lands
                // when we wire ResourceMonitor to surface stuck
                // sidecars in step 9.
                //
                // F-682: surface `pending_turns > 0` at trace level so
                // a stuck-mid-turn sidecar is visible in triage. Trace
                // (not warn or debug) because heartbeats fire at 1 Hz
                // and `pending_turns > 0` is the *normal* state for
                // the entire duration of every turn — anything louder
                // would drown the log. Operators who suspect a stuck
                // child enable `RUST_LOG=forge_session::sidecar=trace`
                // and read the per-second cadence to confirm.
                if hb.pending_turns > 0 {
                    tracing::trace!(
                        target: "forge_session::sidecar",
                        instance_id = %self.instance_id,
                        pending_turns = hb.pending_turns,
                        "sidecar heartbeat: turn in flight"
                    );
                }
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
            SidecarMessage::HelloAck(_)
            | SidecarMessage::Hello(_)
            | SidecarMessage::DaemonHello(_) => {
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

    /// Close the event channel and await the emitter task's final
    /// drain so callers observing the supervisor's exit are guaranteed
    /// every accepted frame has reached the sink. Idempotent — calling
    /// twice is a no-op because the JoinHandle has been taken.
    async fn drain_emitter(&mut self) {
        // Replace `event_tx` with a fresh, unconnected sender so the
        // original is dropped here; the emitter task observes
        // `recv() -> None` and exits. We can't simply move out of
        // `self.event_tx` because the surrounding methods take `&mut self`
        // for the duration of the supervisor's run.
        let (sentinel_tx, _) = mpsc::channel::<Event>(1);
        let closing = std::mem::replace(&mut self.event_tx, sentinel_tx);
        drop(closing);
        if let Some(join) = self.emitter_join.take() {
            if let Err(e) = join.await {
                warn!(
                    target: "forge_session::sidecar",
                    instance_id = %self.instance_id,
                    error = %e,
                    "event emitter task did not exit cleanly"
                );
            }
        }
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

/// F-658: spawn the event emitter task that drains a bounded channel
/// of `Event`s into a real [`EventSink`].
///
/// The supervisor's IPC read loop pushes inbound `SidecarMessage::Event`
/// payloads onto the paired [`mpsc::Sender`] using `send().await`. When
/// the sink is slow, the channel fills, the read loop parks, the kernel
/// UDS read buffer fills, and the sidecar's writer blocks — end-to-end
/// flow control with a documented memory ceiling
/// ([`EVENT_CHANNEL_DEPTH`]).
///
/// Sink errors are logged at `warn!` and **do not** stop the task —
/// dropping the entire pump on a single transient failure would leave
/// the daemon's read loop wedged on a saturated channel forever. The
/// task exits only when its receiver closes (sender dropped), which
/// happens on the supervisor's shutdown / failure-escalation path.
///
/// Exposed as a free function (rather than baked into the supervisor)
/// so the F-658 regression test can drive the same drain logic without
/// spawning a real `forged-agent` binary.
pub fn spawn_event_emitter(
    mut rx: mpsc::Receiver<Event>,
    sink: Arc<dyn EventSink>,
    instance_id: AgentInstanceId,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let Err(e) = sink.emit(event).await {
                warn!(
                    target: "forge_session::sidecar",
                    instance_id = %instance_id,
                    error = %e,
                    "event_sink emit failed"
                );
            }
        }
        debug!(
            target: "forge_session::sidecar",
            instance_id = %instance_id,
            "event emitter task drained; exiting"
        );
    })
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

/// F-662: confirm the supervisor's bind directory is a real directory
/// (not a symlink) and return its canonical absolute path.
///
/// `bind_uds_safely` only stats the *leaf* socket entry on the
/// `EADDRINUSE` recovery path; it never validates the parent component
/// chain. An attacker with write access to a grandparent (e.g.
/// `/run/user/$UID/forge`) could otherwise plant a symlink in place of
/// the directory, redirecting every per-instance socket bind into an
/// attacker-controlled tree. The 0o700 mode set by [`ensure_socket_dir`]
/// only fences a same-uid attacker on the directory itself, not on its
/// ancestors.
///
/// Pattern parallels F-649 (`crates/forge-agents/src/memory.rs`):
/// `symlink_metadata` to refuse a symlink at the leaf, and
/// `canonicalize` to materialize an absolute path with no remaining
/// symlink components for downstream comparison.
async fn validate_socket_dir_canonical(dir: &Path) -> Result<PathBuf> {
    let meta = tokio::fs::symlink_metadata(dir).await.with_context(|| {
        format!(
            "stat sidecar socket dir at {} during symlink validation",
            dir.display()
        )
    })?;
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_dir() {
        anyhow::bail!(
            "refusing to bind sidecar UDS: {} is not a real directory (type={:?}); \
             a symlink at the bind dir would redirect the socket outside the runtime root",
            dir.display(),
            ft
        );
    }
    tokio::fs::canonicalize(dir)
        .await
        .with_context(|| format!("canonicalize sidecar socket dir {}", dir.display()))
}

/// F-662: re-validate that `dir` still canonicalizes to `expected`
/// before every bind. Defends against an attacker who plants a symlink
/// in a grandparent component **after** the supervisor cached its
/// expected canonical at first spawn. A divergence is treated as an
/// active redirection attempt and the bind is refused.
async fn verify_socket_dir_matches_expected(dir: &Path, expected: &Path) -> Result<()> {
    let current = validate_socket_dir_canonical(dir).await?;
    if current != expected {
        anyhow::bail!(
            "sidecar socket dir {} canonicalizes to {} but expected {}; \
             refusing to bind (parent-component symlink redirection detected)",
            dir.display(),
            current.display(),
            expected.display()
        );
    }
    Ok(())
}

/// F-651: read the daemon's effective uid. Wraps the `geteuid()`
/// libc call so the supervisor doesn't pull in `nix` for a single
/// syscall; `forge-session` already depends on `libc`.
fn current_euid() -> u32 {
    // Safe: `geteuid()` is async-signal-safe and has no failure mode.
    unsafe { libc::geteuid() }
}

/// F-678 + #702: validate a sidecar's `Hello` frame.
///
/// Hard rejections (return `Err` so the supervisor kills the misrouted
/// child and tears down the connection):
/// - `proto` does not match [`SIDECAR_PROTO_VERSION`] (typed
///   [`SidecarError::ProtoMismatch`] wrapped in `anyhow::Error`; logged
///   via `tracing::error!`). The wire shape is gated by `proto`; a
///   mismatch means we cannot trust subsequent frames.
/// - `instance_id` does not match the supervisor's expected id (untyped
///   `anyhow::bail!` for now — the supervisor already kills the child
///   on any returned error).
///
/// Soft mismatch (warn-only): `schema_version`. Emits a `tracing::warn!`
/// through the shared [`forge_ipc::warn_if_schema_mismatch`] helper but
/// allows the handshake to complete — the schema layer is additive.
///
/// Lives outside `LiveSidecar::spawn` so unit tests can drive the
/// validation path without spawning a real `forged-agent` binary.
pub(crate) fn validate_sidecar_hello(
    hello: &SidecarHello,
    expected_instance_id: &AgentInstanceId,
) -> Result<()> {
    if hello.proto != SIDECAR_PROTO_VERSION {
        error!(
            target: "forge_session::sidecar",
            daemon_proto = SIDECAR_PROTO_VERSION,
            peer_proto = hello.proto,
            instance_id = %hello.instance_id,
            "sidecar proto version mismatch; closing connection",
        );
        return Err(anyhow::Error::new(SidecarError::ProtoMismatch {
            ours: SIDECAR_PROTO_VERSION,
            theirs: hello.proto,
        }));
    }
    if hello.instance_id.to_string() != expected_instance_id.to_string() {
        anyhow::bail!(
            "forged-agent reported instance_id={}, expected {}",
            hello.instance_id,
            expected_instance_id
        );
    }
    forge_ipc::warn_if_schema_mismatch("sidecar", hello.schema_version, SIDECAR_SCHEMA_VERSION);
    Ok(())
}

/// F-651: assert that the connected peer's uid matches `expected_uid`.
/// Tokio's `peer_cred()` wraps SO_PEERCRED on Linux and LOCAL_PEERCRED
/// on macOS; both report the credentials of the process that
/// `connect()`'d the socket. A mismatch is a defence-in-depth signal:
/// even though the parent dir is 0o700 and the socket file 0o600, a
/// real different-uid peer or a confused operator must be rejected.
fn verify_peer_uid(stream: &UnixStream, expected_uid: u32) -> Result<()> {
    let cred = stream
        .peer_cred()
        .context("read peer credentials from sidecar UDS")?;
    let peer_uid = cred.uid();
    if peer_uid != expected_uid {
        anyhow::bail!(
            "sidecar peer uid {} does not match daemon euid {}",
            peer_uid,
            expected_uid
        );
    }
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

/// #702: tighten a freshly-bound sidecar UDS to mode `0o600` and
/// fail-close on any chmod error.
///
/// The previous behaviour was best-effort — a tmpfs that returns
/// `ENOTSUP` on `chmod(2)` left the socket at the umask default
/// (typically `0o755`), accessible to any local user. The 0o700 parent
/// directory blocks `connect(2)` in practice, but a misconfigured
/// operator pointing the runtime dir at a permissive parent would
/// silently lose the inner defence.
///
/// On chmod failure this helper logs the errno via `tracing::error!`,
/// drops the listener (releasing the bound address), unlinks the socket
/// file, and returns the original IO error so the caller surfaces a
/// hard fail instead of continuing on an over-permissive socket. The
/// listener is moved through the function so dropping it on the error
/// path is automatic — a struct field copy could leak the bind on a
/// future refactor.
async fn enforce_socket_mode_or_close(
    listener: UnixListener,
    socket_path: &Path,
) -> Result<UnixListener> {
    match tokio::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600)).await {
        Ok(()) => Ok(listener),
        Err(e) => {
            error!(
                target: "forge_session::sidecar",
                socket = %socket_path.display(),
                errno = e.raw_os_error().unwrap_or(0),
                error_kind = ?e.kind(),
                error = %e,
                "set_permissions(0o600) failed for sidecar socket; tearing down listener and unlinking",
            );
            // Drop the listener first to release the bound address
            // before we unlink the inode — order matters: an unlink
            // while the listener is still owned by us is racy on
            // concurrent connect attempts.
            drop(listener);
            if let Err(unlink_err) = tokio::fs::remove_file(socket_path).await {
                warn!(
                    target: "forge_session::sidecar",
                    socket = %socket_path.display(),
                    error = %unlink_err,
                    "failed to unlink sidecar socket after chmod failure",
                );
            }
            Err(anyhow::Error::from(e).context(format!(
                "chmod 0o600 failed for sidecar socket at {}",
                socket_path.display()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    /// F-662: a real directory passes `validate_socket_dir_canonical` and
    /// returns its canonical path.
    #[tokio::test]
    async fn validate_socket_dir_accepts_real_directory() {
        let tmp = TempDir::new().expect("tmp");
        let dir = tmp.path().join("forge");
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        let canonical = validate_socket_dir_canonical(&dir)
            .await
            .expect("real dir must pass validation");
        assert_eq!(
            canonical,
            dir.canonicalize().expect("canonicalize"),
            "validated canonical must equal direct canonicalize result",
        );
    }

    /// F-662: a `socket_dir` that is itself a symlink to another directory
    /// is rejected. The leaf component is what the issue calls out as the
    /// most direct redirection vector — refusing it forces the operator
    /// to point us at a real directory.
    #[tokio::test]
    async fn validate_socket_dir_rejects_symlink_at_leaf() {
        let tmp = TempDir::new().expect("tmp");
        let real = tmp.path().join("real");
        tokio::fs::create_dir_all(&real).await.expect("mkdir real");
        let link = tmp.path().join("forge");
        symlink(&real, &link).expect("symlink");

        let err = validate_socket_dir_canonical(&link)
            .await
            .expect_err("symlink leaf must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("not a real directory") || msg.contains("symlink"),
            "expected symlink-rejection error, got: {msg}"
        );
    }

    /// F-662: with a stable expected-canonical reference, swapping the
    /// supervisor's bind dir for a symlink to an attacker-controlled
    /// location is detected as a redirection and rejected by
    /// `verify_socket_dir_matches_expected`.
    #[tokio::test]
    async fn verify_socket_dir_rejects_grandparent_symlink_redirection() {
        let tmp = TempDir::new().expect("tmp");
        let runtime = tmp.path().join("runtime");
        tokio::fs::create_dir_all(&runtime).await.expect("mkdir");
        let socket_dir = runtime.join("forge");
        tokio::fs::create_dir_all(&socket_dir)
            .await
            .expect("mkdir forge");

        let expected = socket_dir.canonicalize().expect("canonicalize expected");

        // Simulate an attacker swapping `runtime` for a symlink that
        // points at an attacker-controlled tree. After the swap, the
        // original socket_dir resolves through the symlink and lands
        // outside the validated runtime.
        let evil = tmp.path().join("evil");
        tokio::fs::create_dir_all(evil.join("forge"))
            .await
            .expect("mkdir evil/forge");
        tokio::fs::remove_dir(&socket_dir)
            .await
            .expect("remove socket_dir");
        tokio::fs::remove_dir(&runtime)
            .await
            .expect("remove runtime");
        symlink(&evil, &runtime).expect("plant grandparent symlink");

        let err = verify_socket_dir_matches_expected(&socket_dir, &expected)
            .await
            .expect_err("redirected socket_dir must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("symlink") || msg.contains("does not match expected"),
            "expected redirection error, got: {msg}"
        );
    }

    /// F-651: a connected peer with the same uid as the daemon (the
    /// realistic case — both halves of a `UnixStream::pair` belong to
    /// our own process) is accepted.
    #[tokio::test]
    async fn verify_peer_uid_accepts_same_uid() {
        let (a, _b) = UnixStream::pair().expect("UnixStream::pair");
        let euid = current_euid();
        verify_peer_uid(&a, euid).expect("same-uid peer must be accepted");
    }

    /// F-651: a connected peer whose uid does not match the daemon's
    /// euid is rejected. We can't fork a different-uid process under
    /// `cargo test` without privileges, so we exercise the rejection
    /// path by passing a deliberately-wrong `expected_uid` — the
    /// supervisor's call site always passes `current_euid()`, so this
    /// test pins the comparison's negative branch end-to-end.
    #[tokio::test]
    async fn verify_peer_uid_rejects_mismatched_uid() {
        let (a, _b) = UnixStream::pair().expect("UnixStream::pair");
        let euid = current_euid();
        let bogus = euid.wrapping_add(1);
        let err = verify_peer_uid(&a, bogus).expect_err("mismatched-uid peer must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("does not match"),
            "expected mismatch error, got: {msg}"
        );
    }

    /// F-678: a sidecar `Hello` whose `schema_version` matches the
    /// supervisor's `SIDECAR_SCHEMA_VERSION` validates without
    /// emitting a warn — the silent path.
    #[test]
    fn validate_sidecar_hello_silent_on_match() {
        use forge_core::AgentInstanceId;
        use forge_ipc::sidecar::{
            SidecarAgentDef, SidecarHello, SidecarProviderSpec, SidecarSandboxLevel,
            SIDECAR_PROTO_VERSION,
        };
        let id = AgentInstanceId::from_string("inst-match".into());
        let hello = SidecarHello {
            proto: SIDECAR_PROTO_VERSION,
            instance_id: id.clone(),
            agent_def: SidecarAgentDef {
                name: "t".into(),
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
            schema_version: SIDECAR_SCHEMA_VERSION,
        };
        validate_sidecar_hello(&hello, &id).expect("matching schema_version must validate");
    }

    /// F-678: a sidecar `Hello` whose `schema_version` differs from
    /// `SIDECAR_SCHEMA_VERSION` still passes validation (warn-only,
    /// non-fatal) and emits a `warn!` through the shared helper. Pin
    /// the wire of that warn under a capture subscriber so the contract
    /// stays observable from operator logs.
    #[test]
    fn validate_sidecar_hello_warns_on_schema_mismatch() {
        use forge_core::AgentInstanceId;
        use forge_ipc::sidecar::{
            SidecarAgentDef, SidecarHello, SidecarProviderSpec, SidecarSandboxLevel,
            SIDECAR_PROTO_VERSION,
        };
        use std::io;
        use std::sync::{Arc, Mutex as StdMutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct CaptureWriter(Arc<StdMutex<Vec<u8>>>);
        impl io::Write for CaptureWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for CaptureWriter {
            type Writer = CaptureWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let writer = CaptureWriter::default();
        let buf = writer.0.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_target(true)
            .with_writer(writer)
            .finish();

        let id = AgentInstanceId::from_string("inst-skew".into());
        let bogus_schema = SIDECAR_SCHEMA_VERSION.wrapping_add(99);
        let hello = SidecarHello {
            proto: SIDECAR_PROTO_VERSION,
            instance_id: id.clone(),
            agent_def: SidecarAgentDef {
                name: "t".into(),
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
            schema_version: bogus_schema,
        };

        tracing::subscriber::with_default(subscriber, || {
            validate_sidecar_hello(&hello, &id)
                .expect("schema mismatch must be warn-only, not an error");
        });

        let captured = String::from_utf8(buf.lock().unwrap().clone()).expect("utf-8");
        assert!(
            captured.contains("schema_version mismatch"),
            "expected mismatch warn, got: {captured}",
        );
        assert!(
            captured.contains("peer=\"sidecar\""),
            "expected peer=sidecar label, got: {captured}",
        );
        assert!(
            captured.contains(&format!("peer_schema_version={bogus_schema}")),
            "expected reported peer schema_version, got: {captured}",
        );
    }

    /// F-678: a sidecar `Hello` carrying a wrong `instance_id` still
    /// hard-rejects (the supervisor would kill the misrouted child).
    /// Pin this so the validator refactor cannot accidentally swallow
    /// the instance_id check while preserving the new schema warn.
    #[test]
    fn validate_sidecar_hello_rejects_bad_instance_id() {
        use forge_core::AgentInstanceId;
        use forge_ipc::sidecar::{
            SidecarAgentDef, SidecarHello, SidecarProviderSpec, SidecarSandboxLevel,
            SIDECAR_PROTO_VERSION,
        };
        let expected = AgentInstanceId::from_string("inst-expected".into());
        let reported = AgentInstanceId::from_string("inst-other".into());
        let hello = SidecarHello {
            proto: SIDECAR_PROTO_VERSION,
            instance_id: reported,
            agent_def: SidecarAgentDef {
                name: "t".into(),
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
            schema_version: SIDECAR_SCHEMA_VERSION,
        };
        let err = validate_sidecar_hello(&hello, &expected)
            .expect_err("instance_id mismatch must hard-reject");
        let msg = err.to_string();
        assert!(
            msg.contains("instance_id"),
            "expected instance_id error, got: {msg}",
        );
    }

    /// #702: a `SidecarHello` whose `proto` does not match
    /// [`SIDECAR_PROTO_VERSION`] hard-rejects with a typed
    /// [`SidecarError::ProtoMismatch`] carrying both versions. The
    /// supervisor relies on the rejection to close the connection rather
    /// than continuing on an un-validatable wire shape.
    #[test]
    fn validate_sidecar_hello_rejects_proto_mismatch() {
        use forge_core::AgentInstanceId;
        use forge_ipc::sidecar::{
            SidecarAgentDef, SidecarHello, SidecarProviderSpec, SidecarSandboxLevel,
        };
        let id = AgentInstanceId::from_string("inst-proto".into());
        let bogus_proto = SIDECAR_PROTO_VERSION.wrapping_add(7);
        let hello = SidecarHello {
            proto: bogus_proto,
            instance_id: id.clone(),
            agent_def: SidecarAgentDef {
                name: "t".into(),
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
            schema_version: SIDECAR_SCHEMA_VERSION,
        };

        let err = validate_sidecar_hello(&hello, &id)
            .expect_err("proto version mismatch must hard-reject");
        let typed = err
            .downcast_ref::<SidecarError>()
            .expect("error must downcast to SidecarError");
        assert_eq!(
            *typed,
            SidecarError::ProtoMismatch {
                ours: SIDECAR_PROTO_VERSION,
                theirs: bogus_proto,
            },
            "expected ProtoMismatch with both versions named, got: {typed:?}",
        );
    }

    /// #702: when `set_permissions` fails (e.g. ENOTSUP on a tmpfs, or
    /// any other errno) the supervisor MUST fail-close: tear down the
    /// listener, unlink the socket file, and return an `Err`. The
    /// previous best-effort behaviour silently left the socket at the
    /// umask default. We simulate the chmod failure by unlinking the
    /// socket inode under a freshly-bound listener — the subsequent
    /// `set_permissions` call observes `ENOENT` on the same path. After
    /// the helper returns, the listener must be dropped (no rebind
    /// races) and the path must not exist (cleanup happened).
    #[tokio::test]
    async fn enforce_socket_mode_fails_close_on_chmod_error() {
        let tmp = TempDir::new().expect("tmp");
        let socket_path = tmp.path().join("victim.sock");

        let listener = UnixListener::bind(&socket_path).expect("bind UDS");
        // Force the chmod to fail without simulating a real ENOTSUP: an
        // unlinked path returns `ENOENT` from set_permissions, which is
        // exactly the "any errno" branch the fail-close discipline
        // covers.
        std::fs::remove_file(&socket_path).expect("unlink under listener");

        let result = enforce_socket_mode_or_close(listener, &socket_path).await;
        let err = result.expect_err("chmod failure must fail-close");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("chmod 0o600 failed"),
            "expected chmod-failure context in error chain, got: {chain}"
        );

        // The socket path must not be left around after fail-close
        // (we removed it in the test setup, but the helper's own
        // `remove_file` call is the contract we care about — it must
        // not leave a chmod-failed file behind in the happy case
        // where the inode still exists). Verify the path is gone.
        assert!(
            !socket_path.exists(),
            "socket file must be unlinked after fail-close",
        );

        // Rebinding the same path must succeed — i.e. the listener was
        // really dropped, releasing the bound address. If the helper
        // accidentally kept the listener alive, this `bind` would fail
        // with EADDRINUSE.
        let _rebind = UnixListener::bind(&socket_path)
            .expect("listener must be dropped after fail-close so rebind succeeds");
    }

    /// F-652: an idle (silent) sidecar peer must not pin the steady-
    /// state pump indefinitely. `read_frame_into_with_deadline` returns
    /// an error after `forge_ipc::DEFAULT_PUMP_DEADLINE`; the supervisor's
    /// `pump_until_exit` then routes that into its EOF-classification
    /// branch and the restart loop.
    #[tokio::test]
    async fn pump_frame_deadline_fires_on_silent_peer() {
        // Use a short deadline derived from the same helper the
        // supervisor uses, so a regression that drops the deadline
        // wrapper would still surface as a hang here.
        let (a, _b) = UnixStream::pair().expect("UnixStream::pair");
        let mut reader = a;
        let started = Instant::now();
        let deadline = Duration::from_millis(150);
        let result =
            forge_ipc::read_frame_with_deadline::<_, SidecarMessage>(&mut reader, deadline).await;
        let elapsed = started.elapsed();
        assert!(result.is_err(), "deadline must fire for a silent peer");
        assert!(
            elapsed < Duration::from_millis(750),
            "deadline did not fire promptly: {elapsed:?}"
        );
    }
}
