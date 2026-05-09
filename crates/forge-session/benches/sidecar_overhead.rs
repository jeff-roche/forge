#![allow(deprecated)] // F-652: tests/benches still drive the deprecated bare read_frame helpers.
//! F-608 step 8: in-process vs sidecar per-token + cold-start bench.
//!
//! ## What this bench measures
//!
//! Two transport paths exist for `Event` emission inside a turn:
//!
//! * **In-process** — the daemon's `Session` is itself the
//!   [`forge_core::EventSink`]; `events.emit(...)` writes to the durable
//!   event log + broadcasts on `tokio::broadcast` in the same process.
//! * **Sidecar** — a per-instance `forged-agent` runs in a child process
//!   and routes every `Event` through its [`forge_agent_host::IpcEventSink`]:
//!   each emit serializes a [`forge_ipc::sidecar::SidecarMessage::Event`]
//!   frame and writes it on the per-instance Unix domain socket. The
//!   daemon-side supervisor reads the frame back and re-emits the
//!   inner [`forge_core::Event`] into a local sink.
//!
//! `docs/architecture/agent-sidecar.md` §"Implementation Plan / Step 8"
//! sets the per-token sidecar-overhead aspiration at **< 50 µs p99**
//! (within the 50 ms per-turn budget) and the **cold-start** aspiration
//! at **< 200 ms p99**. This bench surfaces both numbers honestly so
//! reviewers can see the real overhead the architecture currently pays
//! on Linux x86_64 in CI; the bench does not "tune the implementation
//! to hit the number."
//!
//! ## Three benchmarks
//!
//! 1. `inproc_token_emission` — emit 1000 `Event::AssistantDelta`s into
//!    a real `Session` (the in-process [`EventSink`] target). The
//!    measurement window covers append-to-disk + flush + broadcast send.
//! 2. `sidecar_token_emission` — emit 1000 `Event::AssistantDelta`s
//!    into a real `IpcEventSink` whose underlying writer is one half of
//!    a `UnixStream::pair`. A reader task on the other half pulls each
//!    frame and re-emits the inner [`Event`] into the **same kind of
//!    `Session`** the in-process path uses, mirroring what the daemon's
//!    [`forge_session::sidecar::SidecarSupervisor`] does on the
//!    receiving end. This makes the two paths apples-to-apples:
//!    every benched event lands in a durable `EventLog` either way,
//!    and the delta measures only the transport overhead (serialize +
//!    frame write + kernel pipe + frame read + deserialize) the
//!    sidecar adds on top of the in-process path.
//! 3. `sidecar_cold_start` — spawn a real `forged-agent` via
//!    [`forge_session::sidecar::SidecarSupervisor`], complete the
//!    `Hello` / `HelloAck` handshake, then shut it down. The measurement
//!    window is the wall time from `supervisor.spawn(...)` returning a
//!    handle to the call's start — i.e. handshake-complete cost.
//!
//! Per-token p50/p99 are derived from criterion's per-iter wall time
//! divided by `TOKENS_PER_ITER`. Cold-start p50/p99 come from criterion's
//! native sample distribution.
//!
//! ## Sidecar binary discipline
//!
//! Cargo doesn't set `CARGO_BIN_EXE_<name>` for binaries outside the
//! package the bench lives in, so the cold-start path walks up from
//! `current_exe()` to `target/<profile>/forged-agent` (mirroring
//! `crates/forge-session/tests/sidecar_supervisor.rs`). If the binary
//! is missing — e.g. `cargo bench -p forge-session` was invoked without
//! a prior workspace build — the helper invokes
//! `cargo build -p forge-agent-host --bin forged-agent` once. Keeps the
//! bench self-contained.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use forge_core::{AgentInstanceId, Event, EventSink, MessageId};
use forge_ipc::sidecar::{
    SidecarAgentDef, SidecarHello, SidecarMessage, SidecarProviderSpec, SidecarSandboxLevel,
    SIDECAR_PROTO_VERSION,
};
use forge_session::session::Session;
use forge_session::sidecar::{SidecarSupervisor, SpawnParams};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::runtime::Runtime;
use tokio::sync::Mutex as AsyncMutex;

/// Number of `AssistantDelta` events each `criterion` iteration emits.
/// 1000 mirrors the architecture-doc step-8 contract ("Bench drives
/// 1000 mock tokens through both paths").
const TOKENS_PER_ITER: usize = 1_000;

/// Realistic per-token delta payload. LLM streams typically produce
/// 2-12 byte deltas for ASCII text and up to ~80 bytes for long
/// punctuation/Unicode runs. The upper-envelope payload keeps the
/// measurement honest about worst-case framing cost.
const REALISTIC_DELTA: &str =
    "The quick brown fox jumps over the lazy dog — streaming token payload.";

fn fixed_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 4, 27, 0, 0, 0).unwrap()
}

fn make_delta_event(msg_id: &MessageId) -> Event {
    Event::AssistantDelta {
        id: msg_id.clone(),
        at: fixed_time(),
        delta: Arc::from(REALISTIC_DELTA),
    }
}

// ──────────────────────────────────────────────────────────────────────────
// In-process path: Session as EventSink. Mirrors `run_turn`'s call site.
// ──────────────────────────────────────────────────────────────────────────

async fn inproc_emit_n(session: &dyn EventSink, msg_id: &MessageId, n: usize) {
    for _ in 0..n {
        // Cheap clone — `Event::AssistantDelta::delta` is `Arc<str>`,
        // `id` is an `Arc<str>`-backed `MessageId`, so each emit pays
        // the same per-event cost the production hot loop pays.
        let event = make_delta_event(msg_id);
        session.emit(event).await.expect("session emit");
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Sidecar path: IpcEventSink → UDS → frame reader.
//
// We avoid spawning the actual `forged-agent` binary for the per-token
// measurement because the step-3 stub binary only emits 3 events per
// `RunTurn`; the IPC wire path is what matters for "overhead per token,"
// and we can exercise that path faithfully with a `UnixStream::pair`
// driven by the same `IpcEventSink` the binary uses.
// ──────────────────────────────────────────────────────────────────────────

/// Minimal sidecar-side `EventSink`. Mirrors
/// `forge_agent_host::IpcEventSink`'s body: frame the event as a
/// `SidecarMessage::Event { seq, event }` and write it on the shared
/// writer half. Re-implemented in the bench (rather than depending on
/// `forge-agent-host` as a dev-dep) to keep the bench's compile graph
/// minimal — a dev-dep would force the whole `forge-agent-host` crate
/// to rebuild on every bench iteration, and the wire shape is small
/// enough to inline.
#[derive(Clone)]
struct BenchIpcEventSink {
    writer: Arc<AsyncMutex<tokio::io::WriteHalf<UnixStream>>>,
    seq: Arc<std::sync::atomic::AtomicU64>,
}

#[async_trait]
impl EventSink for BenchIpcEventSink {
    async fn emit(&self, event: Event) -> anyhow::Result<()> {
        let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let frame = SidecarMessage::Event(forge_ipc::sidecar::SidecarEvent { seq, event });
        let mut w = self.writer.lock().await;
        forge_ipc::write_frame(&mut *w, &frame).await?;
        Ok(())
    }
}

/// Drive `n` `Event::AssistantDelta`s through `sink` and wait for the
/// reader task to drain all frames before returning. The reader-side
/// drain is load-bearing: without it the measurement would close the
/// criterion-iter window before the kernel had finished moving the
/// final byte across the pipe, flattering the sidecar path.
async fn sidecar_emit_n(
    sink: &BenchIpcEventSink,
    msg_id: &MessageId,
    drain: &tokio::sync::Notify,
    drained_target: &std::sync::atomic::AtomicUsize,
    n: usize,
) {
    drained_target.store(n, std::sync::atomic::Ordering::SeqCst);
    for _ in 0..n {
        sink.emit(make_delta_event(msg_id)).await.expect("ipc emit");
    }
    // Block until the reader task has consumed all `n` frames.
    drain.notified().await;
}

/// Spawn the per-iteration reader task. Reads exactly `target` frames
/// from `read_half`, re-emits each inner [`Event`] into the supplied
/// daemon-side [`EventSink`] (a real `Session`, mirroring what the
/// supervisor does on the receiving end), then signals `done`.
/// Long-lived rather than per-iter so the measurement window for
/// [`sidecar_emit_n`] excludes task-spawn cost; the per-iter target
/// counter is the ready/done handshake.
fn spawn_reader_with_sink(
    rt: &Runtime,
    mut read_half: tokio::io::ReadHalf<UnixStream>,
    target: Arc<std::sync::atomic::AtomicUsize>,
    done: Arc<tokio::sync::Notify>,
    daemon_sink: Arc<dyn EventSink>,
) -> tokio::task::JoinHandle<()> {
    rt.spawn(async move {
        let mut buf = Vec::new();
        loop {
            let n = target.load(std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                tokio::task::yield_now().await;
                continue;
            }
            for _ in 0..n {
                let frame: SidecarMessage = forge_ipc::read_frame_into(&mut read_half, &mut buf)
                    .await
                    .expect("read_frame_into");
                if let SidecarMessage::Event(ev) = frame {
                    daemon_sink.emit(ev.event).await.expect("daemon sink emit");
                }
            }
            target.store(0, std::sync::atomic::Ordering::SeqCst);
            done.notify_one();
        }
    })
}

// ──────────────────────────────────────────────────────────────────────────
// Sidecar cold-start path: real `forged-agent` spawn via `SidecarSupervisor`.
// ──────────────────────────────────────────────────────────────────────────

/// In-memory `EventSink` for the cold-start bench. The supervisor only
/// emits `BackgroundAgentCompleted` on retry exhaustion — never on a
/// happy-path spawn → shutdown — so this sink is effectively a no-op
/// for the cold-start measurement, but the supervisor's API requires
/// one.
#[derive(Default)]
struct DiscardingSink;

#[async_trait]
impl EventSink for DiscardingSink {
    async fn emit(&self, _event: Event) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Resolve the freshly-built `forged-agent` binary. Mirrors the
/// discovery used by `crates/forge-session/tests/sidecar_supervisor.rs`
/// so the bench composes with the same `cargo build` outputs the
/// sidecar tests rely on.
///
/// `cargo bench` runs the bench binary out of `target/release/deps/` —
/// `current_exe()` walks up to `target/release/` accordingly. The
/// helper looks for `forged-agent` next to the bench binary, then
/// falls back to building it in the matching profile (release by
/// default for `cargo bench`).
fn forged_agent_path() -> PathBuf {
    // current_exe lives at target/<profile>/deps/<bench>-<HASH> for
    // a criterion bench binary. Walk up two parents to land on
    // target/<profile>/.
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe
        .parent()
        .and_then(|p| p.parent())
        .expect("target/<profile>/")
        .to_path_buf();
    let candidate = dir.join("forged-agent");
    if candidate.exists() {
        return candidate;
    }

    // Detect bench profile from the directory name. `cargo bench`
    // resolves to `target/release/`; `cargo build --benches` to
    // `target/debug/`. We forward `--release` to the build invocation
    // when needed so the freshly-built `forged-agent` lands next to
    // the bench binary that just asked for it.
    let profile_dir = dir.file_name().and_then(|s| s.to_str()).unwrap_or("debug");
    let mut args: Vec<&str> = vec!["build", "-p", "forge-agent-host", "--bin", "forged-agent"];
    if profile_dir == "release" {
        args.push("--release");
    }
    let status = std::process::Command::new(env!("CARGO"))
        .args(&args)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("cargo build -p forge-agent-host failed: {s}"),
        Err(e) => eprintln!("cargo build -p forge-agent-host invocation failed: {e}"),
    }
    candidate
}

fn fixture_hello(instance_id: &AgentInstanceId) -> SidecarHello {
    SidecarHello {
        proto: SIDECAR_PROTO_VERSION,
        instance_id: instance_id.clone(),
        agent_def: SidecarAgentDef {
            name: "bench".into(),
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

// ──────────────────────────────────────────────────────────────────────────
// Criterion glue
// ──────────────────────────────────────────────────────────────────────────

fn bench_inproc_token_emission(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio rt");
    // Build the session once so the measurement window excludes the
    // EventLog open / broadcast-channel construction cost — the
    // production hot path reuses a long-lived `Session`, so the bench
    // does too.
    let dir = TempDir::new().expect("tempdir");
    let log_path = dir.path().join("events.jsonl");
    let session: Arc<Session> =
        rt.block_on(async { Arc::new(Session::create(log_path).await.expect("session create")) });
    let msg_id = MessageId::new();

    let mut group = c.benchmark_group("inproc_token_emission");
    group.throughput(Throughput::Elements(TOKENS_PER_ITER as u64));
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(8));
    group.bench_function("1000_assistant_deltas", |b| {
        b.to_async(&rt).iter(|| async {
            inproc_emit_n(session.as_ref(), &msg_id, TOKENS_PER_ITER).await;
        });
    });
    group.finish();
}

fn bench_sidecar_token_emission(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio rt");

    // Build a single UDS pair + reader task and reuse it across all
    // criterion samples. The kernel pipe is the only resource that
    // would have meaningful per-iter setup cost; reusing it focuses
    // the measurement on the per-frame serialize/write/read/deserialize
    // cost the way `inproc_token_emission` focuses on per-event
    // log-append/broadcast cost.
    //
    // The reader-side sink is a real `Session` (same shape as the
    // in-process bench) so both paths persist every event to a
    // durable `EventLog`. The benched delta therefore reflects only
    // the additional transport cost the sidecar adds — the
    // architecture's actual "overhead per token" question.
    let dir = TempDir::new().expect("tempdir");
    let daemon_log = dir.path().join("daemon-events.jsonl");
    let daemon_sink: Arc<dyn EventSink> = rt.block_on(async {
        Arc::new(
            Session::create(daemon_log)
                .await
                .expect("daemon session create"),
        )
    });

    let (sink, drain_done, drain_target, _reader_join) = rt.block_on(async {
        let (a, b) = UnixStream::pair().expect("uds pair");
        let (read_half, _drop_a_write) = tokio::io::split(a);
        let (_drop_b_read, write_half) = tokio::io::split(b);
        // We deliberately leak the unused halves so they outlive the
        // bench iters — the reader task only needs the read_half from
        // `a`, the sink only needs the write_half from `b`. Dropping
        // either at iter time would cause spurious EOFs on the live
        // half.
        std::mem::forget(_drop_a_write);
        std::mem::forget(_drop_b_read);

        let target = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let done = Arc::new(tokio::sync::Notify::new());
        let join = spawn_reader_with_sink(
            &rt,
            read_half,
            target.clone(),
            done.clone(),
            daemon_sink.clone(),
        );
        let sink = BenchIpcEventSink {
            writer: Arc::new(AsyncMutex::new(write_half)),
            seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        (sink, done, target, join)
    });
    let msg_id = MessageId::new();

    let mut group = c.benchmark_group("sidecar_token_emission");
    group.throughput(Throughput::Elements(TOKENS_PER_ITER as u64));
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(8));
    group.bench_function("1000_assistant_deltas", |b| {
        b.to_async(&rt).iter(|| async {
            sidecar_emit_n(&sink, &msg_id, &drain_done, &drain_target, TOKENS_PER_ITER).await;
        });
    });
    group.finish();
}

fn bench_sidecar_cold_start(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio rt");
    let agent_path = forged_agent_path();
    if !agent_path.exists() {
        eprintln!(
            "[sidecar_overhead] forged-agent binary missing at {}; skipping cold-start group",
            agent_path.display()
        );
        return;
    }

    let mut group = c.benchmark_group("sidecar_cold_start");
    // Spawning a real child process is expensive; cap the sample size
    // so the bench wall-clock stays sane on CI. Criterion still
    // produces p50/p99 (median + 99th percentile of the sample
    // distribution) at this size, just with wider confidence
    // intervals.
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(20));
    group.bench_function("spawn_handshake_shutdown", |b| {
        b.to_async(&rt).iter_custom(|iters| {
            let agent_path = agent_path.clone();
            async move {
                let mut total = Duration::ZERO;
                for i in 0..iters {
                    let tmp = TempDir::new().expect("tmpdir");
                    let supervisor =
                        SidecarSupervisor::new(tmp.path().to_path_buf(), agent_path.clone());
                    let id = AgentInstanceId::from_string(format!("bench-cold-start-{i}"));
                    let sink: Arc<dyn EventSink> = Arc::new(DiscardingSink);

                    let start = Instant::now();
                    let handle = supervisor
                        .spawn(
                            id.clone(),
                            SpawnParams {
                                hello: fixture_hello(&id),
                            },
                            sink.clone(),
                        )
                        .await
                        .expect("supervisor spawn");
                    // Cold start is "spawn returned" — i.e. handshake
                    // complete. Stop the clock here so the supervisor's
                    // own background pump tasks (heartbeat, command
                    // forward) don't pollute the cold-start number.
                    let elapsed = start.elapsed();
                    total += elapsed;

                    // Best-effort cooperative shutdown to reap the
                    // child before the next iter. Failures here would
                    // leak a process but not corrupt the measurement;
                    // ignore.
                    let _ = handle.shutdown().await;
                }
                total
            }
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_inproc_token_emission,
    bench_sidecar_token_emission,
    bench_sidecar_cold_start
);
criterion_main!(benches);
