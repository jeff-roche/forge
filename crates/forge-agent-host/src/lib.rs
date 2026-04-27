//! F-608 step 2: `forged-agent` sidecar host.
//!
//! This crate ships the per-instance sidecar binary that the daemon spawns
//! when `FORGE_AGENT_SIDECAR=1`. It owns the IPC plumbing — handshake,
//! heartbeat, panic-hook crash dump, dispatch loop — and nothing else.
//! The actual `run_turn` body lives in `forge-session`; step 3 refactors
//! it behind an `EventSink` trait so the same body can run in-process or
//! through this crate's outbound frame writer.
//!
//! ## Wire flow
//!
//! ```text
//!   daemon                                    forged-agent
//!     │ spawn(--socket, --instance-id) ──────▶│
//!     │                                       │ connect(socket)
//!     │                                       │ install panic hook
//!     │                                ◀──────│ Hello
//!     │ HelloAck { pid, started_at }   ──────▶│
//!     │                                ◀──────│ Heartbeat (1 Hz, forever)
//!     │ RunTurn { … }                  ──────▶│
//!     │                                ◀──────│ Event::AssistantMessage  (stub)
//!     │                                ◀──────│ Event::StepFinished      (stub)
//!     │ Shutdown { grace_ms }          ──────▶│
//!     │                                       │ drain → exit(0)
//! ```
//!
//! ## Stub handlers
//!
//! Step 2 intentionally keeps the message handlers as stubs. `RunTurn`
//! emits a deterministic placeholder event sequence so an integration
//! test can verify the IPC plumbing end-to-end without depending on a
//! real provider. Step 3 replaces the stub with the refactored
//! `run_turn` body.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use forge_core::{MessageId, ProviderId};
use forge_ipc::sidecar::{
    SidecarCrashed, SidecarEvent, SidecarHeartbeat, SidecarHelloAck, SidecarMessage,
    SidecarRunTurn, SIDECAR_SCHEMA_VERSION,
};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::UnixStream;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, error, info, warn};

/// CLI arguments parsed by the binary's `main`. Kept in the library so
/// tests can construct the same shape without re-parsing.
#[derive(Debug, Clone)]
pub struct AgentArgs {
    /// UDS path the daemon already bound; the sidecar connects (does not
    /// bind) on startup.
    pub socket: PathBuf,
    /// Logical instance identifier the daemon assigned. Round-tripped
    /// through tracing fields and the `Hello` frame.
    pub instance_id: String,
}

impl AgentArgs {
    /// Parse `forged-agent --socket <path> --instance-id <id>`.
    ///
    /// Hand-rolled rather than `clap` because the sidecar boots on the
    /// time-to-first-token path; the architecture doc §1 calls out the
    /// startup-cost rationale for skipping CLI deps.
    pub fn parse_from<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut socket: Option<PathBuf> = None;
        let mut instance_id: Option<String> = None;
        let mut iter = args.into_iter().map(Into::into);
        // Skip argv[0] (program name) — `std::env::args` includes it.
        let _ = iter.next();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--socket" => {
                    socket = Some(PathBuf::from(
                        iter.next().context("--socket requires a value")?,
                    ));
                }
                "--instance-id" => {
                    instance_id = Some(iter.next().context("--instance-id requires a value")?);
                }
                other => {
                    anyhow::bail!("unknown argument: {other}");
                }
            }
        }
        Ok(Self {
            socket: socket.context("--socket is required")?,
            instance_id: instance_id.context("--instance-id is required")?,
        })
    }
}

/// Crash-channel shared with the panic hook. The hook serializes a
/// [`SidecarMessage::Crashed`] frame into the queued bytes and the
/// shutdown path drains it onto the wire before exit. A `Mutex<Option<…>>`
/// (rather than `OnceLock<…>`) keeps the hook idempotent on repeat panics
/// (only the first one wins) without forcing a global allocation when no
/// panic ever happens.
type CrashSink = Arc<Mutex<Option<Vec<u8>>>>;

static CRASH_SINK: OnceLock<CrashSink> = OnceLock::new();

/// Install the panic hook. Idempotent — the underlying `OnceLock` only
/// stores the first sink, so the integration test (which spawns the
/// binary fresh per-test) gets a clean install every process.
///
/// The hook serializes a [`SidecarMessage::Crashed`] payload into the
/// shared sink. The main loop's shutdown path drains the sink onto the
/// wire before exit. We deliberately do **not** write to the socket from
/// inside the hook itself: the hook may run on any thread (including
/// blocking ones with no tokio runtime context), and a blocking write
/// inside `panic` invites deadlocks. Best-effort delivery is fine — the
/// supervisor falls back to EOF + non-zero exit detection per
/// architecture doc §10.
pub fn install_panic_hook() -> CrashSink {
    let sink: CrashSink = CRASH_SINK
        .get_or_init(|| Arc::new(Mutex::new(None)))
        .clone();
    let hook_sink = sink.clone();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Capture a stringified panic message. `panic_info::message` is
        // unstable, so we fall back to `payload`-downcast like the
        // standard library's default hook.
        let payload = info.payload();
        let panic_message = if let Some(s) = payload.downcast_ref::<&'static str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "panic with non-string payload".to_string()
        };
        let backtrace = std::backtrace::Backtrace::capture();
        let backtrace_str = match backtrace.status() {
            std::backtrace::BacktraceStatus::Captured => Some(backtrace.to_string()),
            _ => None,
        };
        let frame = SidecarMessage::Crashed(SidecarCrashed {
            panic_message,
            backtrace: backtrace_str,
        });
        if let Ok(bytes) = serde_json::to_vec(&frame) {
            if let Ok(mut guard) = hook_sink.lock() {
                if guard.is_none() {
                    *guard = Some(bytes);
                }
            }
        }
        // Defer to the previous hook so the panic still prints to stderr
        // for `tracing-subscriber` users / CI logs.
        prev(info);
    }));
    sink
}

/// Drain any queued crash payload into a single [`SidecarMessage::Crashed`]
/// frame on the writer. No-op when no panic happened.
async fn flush_crash_if_any<W>(writer: &mut W, sink: &CrashSink)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let payload = match sink.lock() {
        Ok(mut guard) => guard.take(),
        Err(_) => return,
    };
    let Some(bytes) = payload else { return };
    let Ok(frame) = serde_json::from_slice::<SidecarMessage>(&bytes) else {
        return;
    };
    if let Err(e) = forge_ipc::write_frame(writer, &frame).await {
        // Best-effort. The supervisor will fall back to EOF detection.
        debug!(error = %e, "failed to flush crash frame to socket");
    }
}

/// Build the [`SidecarMessage::Hello`] frame the sidecar sends on
/// startup. Mostly placeholder values today — step 3 wires this from the
/// real environment when the sidecar is plugged into the supervisor's
/// spawn path.
fn make_hello(instance_id: &str) -> SidecarMessage {
    use forge_ipc::sidecar::{
        SidecarAgentDef, SidecarHello, SidecarProviderSpec, SidecarSandboxLevel,
        SIDECAR_PROTO_VERSION,
    };
    SidecarMessage::Hello(SidecarHello {
        proto: SIDECAR_PROTO_VERSION,
        instance_id: forge_core::AgentInstanceId::from_string(instance_id.to_string()),
        agent_def: SidecarAgentDef {
            name: instance_id.to_string(),
            description: None,
            body: String::new(),
            allowed_paths: Vec::new(),
            isolation: "process".to_string(),
            memory_enabled: false,
        },
        allowed_paths: Vec::new(),
        workspace_path: String::new(),
        provider_spec: SidecarProviderSpec {
            kind: "stub".to_string(),
            model: "stub".to_string(),
            base_url: None,
        },
        sandbox_level: SidecarSandboxLevel::Level1,
        telemetry_endpoint: None,
    })
}

/// Per-instance event sequence number. Threaded through the heartbeat
/// task and the run-turn handler so every outbound `Event` frame is
/// totally ordered. Starts at 1 (0 is reserved as a "never emitted"
/// sentinel for downstream consumers).
#[derive(Debug, Clone, Default)]
pub struct EventSeq(Arc<AtomicU64>);

impl EventSeq {
    /// Allocate the next sequence number.
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Shared writer half. Multiple tasks emit (heartbeat, run-turn handler,
/// shutdown drain), so the half lives behind a tokio mutex.
pub type SharedWriter = Arc<AsyncMutex<WriteHalf<UnixStream>>>;

/// Spawn the 1 Hz heartbeat task. Returns the join handle so the
/// shutdown path can abort it before draining the writer for a clean
/// last-frame ordering.
fn spawn_heartbeat(
    writer: SharedWriter,
    pending_turns: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        // First tick fires immediately; skip it so we don't double-pulse
        // right after `Hello`/`HelloAck`.
        interval.tick().await;
        loop {
            interval.tick().await;
            let frame = SidecarMessage::Heartbeat(SidecarHeartbeat {
                at: Utc::now(),
                pending_turns: pending_turns.load(Ordering::Relaxed) as u32,
            });
            let mut w = writer.lock().await;
            if let Err(e) = forge_ipc::write_frame(&mut *w, &frame).await {
                // The peer has gone away — heartbeat task exits, the
                // main read loop will pick up the EOF on its read side
                // and drive the same exit.
                debug!(error = %e, "heartbeat write failed; exiting heartbeat task");
                return;
            }
        }
    })
}

/// Stub `RunTurn` handler. Emits a deterministic event sequence:
/// `AssistantMessage { text: "[stub]" }` then `StepFinished`. Step 3
/// replaces this with the refactored `run_turn` body.
async fn handle_run_turn_stub(
    writer: &SharedWriter,
    seq: &EventSeq,
    turn: SidecarRunTurn,
) -> Result<()> {
    use forge_core::{Event, StepId, StepKind, StepOutcome};

    let now = Utc::now();
    let assistant_id = MessageId::new();
    let step_id = StepId::new();

    // Step started so the UI sees a step boundary even from the stub.
    let step_started = Event::StepStarted {
        step_id: step_id.clone(),
        instance_id: None,
        kind: StepKind::Model,
        started_at: now,
        index: 1,
        total: Some(1),
    };
    let assistant = Event::AssistantMessage {
        id: assistant_id,
        provider: ProviderId::from_string("stub".to_string()),
        model: "stub".to_string(),
        at: now,
        stream_finalised: true,
        text: std::sync::Arc::from("[stub]"),
        branch_parent: turn.branch_parent.clone(),
        branch_variant_index: turn.branch_variant_index.unwrap_or(0),
    };
    let step_finished = Event::StepFinished {
        step_id,
        outcome: StepOutcome::Ok,
        duration_ms: 0,
        token_usage: None,
    };

    for ev in [step_started, assistant, step_finished] {
        let frame = SidecarMessage::Event(SidecarEvent {
            seq: seq.next(),
            event: ev,
        });
        let mut w = writer.lock().await;
        forge_ipc::write_frame(&mut *w, &frame).await?;
    }
    Ok(())
}

/// Result of the dispatch loop driving us into shutdown.
enum LoopExit {
    /// Daemon sent `Shutdown { grace_ms }`.
    Shutdown { grace_ms: u64 },
    /// Daemon closed the connection unexpectedly. Treated as a hard
    /// shutdown (no drain window).
    PeerClosed,
}

/// Read loop: dispatch incoming daemon → sidecar messages until a
/// `Shutdown` frame or EOF. Uses a hoisted buffer so the streaming-token
/// path stays allocation-free per frame (mirrors `forge-ipc`'s
/// `read_frame_into` discipline).
async fn dispatch_loop(
    reader: &mut ReadHalf<UnixStream>,
    writer: SharedWriter,
    seq: EventSeq,
    pending_turns: Arc<AtomicU64>,
) -> Result<LoopExit> {
    let mut buf = Vec::new();
    loop {
        let frame: SidecarMessage = match forge_ipc::read_frame_into(reader, &mut buf).await {
            Ok(m) => m,
            Err(e) => {
                // EOF or any read error is treated as the peer going
                // away. The sidecar drops the connection and exits;
                // the supervisor's restart logic owns the rest.
                debug!(error = %e, "read loop: peer closed or error");
                return Ok(LoopExit::PeerClosed);
            }
        };
        match frame {
            SidecarMessage::RunTurn(turn) => {
                pending_turns.fetch_add(1, Ordering::Relaxed);
                let res = handle_run_turn_stub(&writer, &seq, turn).await;
                pending_turns.fetch_sub(1, Ordering::Relaxed);
                if let Err(e) = res {
                    error!(error = %e, "run_turn stub failed");
                }
            }
            SidecarMessage::ToolCallApproved(_)
            | SidecarMessage::ToolCallRejected(_)
            | SidecarMessage::Credentials(_)
            | SidecarMessage::CompactTranscript(_) => {
                // Step 2 leaves these as no-ops; step 3 wires them to
                // the refactored run_turn.
                debug!("ignoring non-RunTurn daemon message in step-2 stub handler");
            }
            SidecarMessage::Shutdown(s) => {
                info!(grace_ms = s.grace_ms, "received shutdown");
                return Ok(LoopExit::Shutdown {
                    grace_ms: s.grace_ms,
                });
            }
            // Sidecar → daemon variants on the daemon → sidecar half are
            // a protocol violation; warn and continue rather than crash
            // (we don't want the sidecar to take itself down on a
            // confused supervisor).
            SidecarMessage::Hello(_)
            | SidecarMessage::HelloAck(_)
            | SidecarMessage::Event(_)
            | SidecarMessage::ToolCallApprovalRequest(_)
            | SidecarMessage::Heartbeat(_)
            | SidecarMessage::Crashed(_) => {
                warn!("unexpected sidecar→daemon variant received from peer; ignoring");
            }
        }
    }
}

/// Drive one full sidecar lifecycle: connect → handshake → loop → exit.
/// Library-level entry so the integration test can drive the same body
/// without spawning the binary.
pub async fn run(args: AgentArgs) -> Result<()> {
    let crash_sink = install_panic_hook();
    info!(
        instance_id = %args.instance_id,
        socket = %args.socket.display(),
        "forged-agent starting"
    );

    let stream = UnixStream::connect(&args.socket)
        .await
        .with_context(|| format!("connect uds {}", args.socket.display()))?;
    let (mut reader, write_half) = tokio::io::split(stream);
    let writer: SharedWriter = Arc::new(AsyncMutex::new(write_half));

    // Send Hello immediately so the supervisor's handshake deadline is
    // observed.
    {
        let mut w = writer.lock().await;
        let hello = make_hello(&args.instance_id);
        forge_ipc::write_frame(&mut *w, &hello).await?;
    }

    // Await HelloAck. The supervisor produces this frame today (per
    // architecture doc §2) — we wait for it so the daemon owns the
    // moment the sidecar is "live". Use a generous deadline; the
    // supervisor's own handshake-deadline logic owns the upper bound.
    {
        let mut buf = Vec::new();
        let frame: SidecarMessage = forge_ipc::read_frame_into_with_deadline(
            &mut reader,
            &mut buf,
            Duration::from_secs(10),
        )
        .await
        .context("waiting for HelloAck")?;
        match frame {
            SidecarMessage::HelloAck(ack) => {
                info!(
                    daemon_pid = ack.pid,
                    schema = ack.schema_version,
                    "handshake complete"
                );
            }
            other => {
                anyhow::bail!("expected HelloAck, got {other:?}");
            }
        }
    }

    // Heartbeat task.
    let pending_turns = Arc::new(AtomicU64::new(0));
    let seq = EventSeq::default();
    let hb_handle = spawn_heartbeat(writer.clone(), pending_turns.clone());

    // Main dispatch loop.
    let exit = dispatch_loop(&mut reader, writer.clone(), seq, pending_turns).await?;

    // Stop the heartbeat first so the last frames on the wire are
    // ours, not a stray heartbeat after Shutdown.
    hb_handle.abort();
    let _ = hb_handle.await;

    match exit {
        LoopExit::Shutdown { grace_ms } => {
            // Drain the writer side: nothing buffered today, but the
            // future tool-call approval queue lands here. The
            // grace_ms ceiling is enforced by tokio::time::timeout.
            let mut w = writer.lock().await;
            let _ = tokio::time::timeout(Duration::from_millis(grace_ms), async {
                tokio::io::AsyncWriteExt::flush(&mut *w).await
            })
            .await;
            flush_crash_if_any(&mut *w, &crash_sink).await;
            // Half-close the write side so the supervisor sees a clean
            // EOF rather than an abrupt drop.
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut *w).await;
        }
        LoopExit::PeerClosed => {
            // Peer is gone. Try to flush the crash payload anyway —
            // the kernel may still let us write into the socket if the
            // peer hasn't fully closed both halves.
            let mut w = writer.lock().await;
            flush_crash_if_any(&mut *w, &crash_sink).await;
        }
    }

    info!("forged-agent exiting cleanly");
    Ok(())
}

/// Build the `HelloAck` frame the daemon side should send. Lives here
/// so the integration test can act as the daemon without re-implementing
/// the trivial constructor.
pub fn build_hello_ack(daemon_pid: u32) -> SidecarMessage {
    SidecarMessage::HelloAck(SidecarHelloAck {
        pid: daemon_pid,
        started_at: Utc::now(),
        schema_version: SIDECAR_SCHEMA_VERSION,
    })
}

/// Initialize a JSON-lines tracing subscriber on stderr per architecture
/// doc §10. Idempotent — repeat calls (e.g. in tests) silently no-op.
pub fn init_tracing() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let filter = EnvFilter::try_from_env("FORGE_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let layer = fmt::layer()
        .json()
        .with_writer(std::io::stderr)
        .with_target(true);
    // `try_init` returns Err if a global subscriber is already installed
    // (the integration test harness does this); swallow that.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_happy_path() {
        let parsed = AgentArgs::parse_from([
            "forged-agent",
            "--socket",
            "/tmp/foo.sock",
            "--instance-id",
            "inst-1",
        ])
        .unwrap();
        assert_eq!(parsed.socket, PathBuf::from("/tmp/foo.sock"));
        assert_eq!(parsed.instance_id, "inst-1");
    }

    #[test]
    fn parse_args_missing_socket() {
        let err = AgentArgs::parse_from(["forged-agent", "--instance-id", "x"]).unwrap_err();
        assert!(err.to_string().contains("--socket"));
    }

    #[test]
    fn parse_args_missing_instance_id() {
        let err = AgentArgs::parse_from(["forged-agent", "--socket", "/tmp/x.sock"]).unwrap_err();
        assert!(err.to_string().contains("--instance-id"));
    }

    #[test]
    fn parse_args_unknown_flag() {
        let err = AgentArgs::parse_from(["forged-agent", "--frobnicate"]).unwrap_err();
        assert!(err.to_string().contains("unknown argument"));
    }

    #[test]
    fn event_seq_is_monotonic() {
        let seq = EventSeq::default();
        assert_eq!(seq.next(), 1);
        assert_eq!(seq.next(), 2);
        assert_eq!(seq.next(), 3);
    }
}
