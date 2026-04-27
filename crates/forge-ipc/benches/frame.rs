//! F-112: per-frame serialization bench.
//!
//! The DoD for F-112 requires ≥30% reduction in per-frame wall-time between
//! the pre-fix path (`Event -> serde_json::Value -> IpcEvent{event:Value} -> bytes`)
//! and the typed path (`Event -> IpcEvent{event:Event} -> bytes`). The
//! `assistant_delta_*` benches measure the single-subscriber case; the
//! `fanout_*` benches measure the two-subscriber case that surfaces the
//! `Arc<str>` cheap-clone win on top of the Value-elimination win.
//!
//! Run `cargo bench -p forge-ipc` to produce criterion reports under
//! `target/criterion/`. The first two reports (single-subscriber) are the
//! headline numbers referenced by the DoD checklist.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use forge_core::{Event, MessageId};
use forge_ipc::{read_frame, read_frame_into, IpcMessage};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::runtime::Runtime;

// F-565: counting allocator. Wraps the system allocator and bumps a
// global counter on every successful `alloc`. The bench prints
// per-iteration alloc counts for the per-call vs reused-buffer read
// paths so the ≥30% allocation-reduction DoD is checkable from the
// bench output without an external profiler.
struct CountingAlloc;

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(layout);
        if !p.is_null() {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        p
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn alloc_count() -> usize {
    ALLOC_COUNT.load(Ordering::Relaxed)
}

fn alloc_bytes() -> usize {
    ALLOC_BYTES.load(Ordering::Relaxed)
}

// The pre-fix shape of `IpcEvent` — retained here so the baseline bench can
// exercise the exact path that existed before F-112. Kept as a local type so
// this bench file is self-contained: the real `IpcEvent` carries `Event`
// directly after F-112.
#[derive(Debug, Serialize, Deserialize)]
struct IpcEventValue {
    seq: u64,
    event: serde_json::Value,
}

// The post-fix shape — matches `forge_ipc::IpcEvent` exactly. Using a local
// mirror keeps the bench self-contained and lets us write both codepaths
// against identical `Event` inputs.
#[derive(Debug, Serialize, Deserialize)]
struct IpcEventTyped {
    seq: u64,
    event: Event,
}

fn fixed_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 4, 18, 10, 0, 0).unwrap()
}

fn message_id(s: &str) -> MessageId {
    serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap()
}

// Realistic per-token delta size. LLM streaming tokens are typically
// 2-12 bytes for text, up to ~80 bytes for long punctuation/unicode runs.
// We bench the upper envelope so the measurement reflects the worst-case
// per-frame cost a user actually sees on an active stream.
const REALISTIC_DELTA: &str =
    "The quick brown fox jumps over the lazy dog — streaming token payload.";

fn make_delta(delta: Arc<str>) -> Event {
    Event::AssistantDelta {
        id: message_id("mid-bench"),
        at: fixed_time(),
        delta,
    }
}

// ── Baseline: Event -> Value -> IpcEvent -> bytes ─────────────────────────

fn baseline_event_to_bytes_via_value(event: &Event) -> Vec<u8> {
    let value = serde_json::to_value(event).expect("to_value");
    let frame = IpcEventValue {
        seq: 1,
        event: value,
    };
    serde_json::to_vec(&frame).expect("to_vec")
}

// ── Proposed: Event -> IpcEvent -> bytes (single traversal) ──────────────

fn typed_event_to_bytes(event: &Event) -> Vec<u8> {
    // Clone is a single `Arc::clone` for the `delta` field (no heap copy);
    // the struct clone itself is a small memcpy of the stack fields.
    let frame = IpcEventTyped {
        seq: 1,
        event: event.clone(),
    };
    serde_json::to_vec(&frame).expect("to_vec")
}

// ── Fanout variants: same delta, two subscribers ─────────────────────────
//
// The realistic production hot path fans a single delta out to N connected
// webview clients (dashboard + active session window at minimum; add more
// when additional panes subscribe). The `Arc<str>` cheap-clone shows its
// dominant win here — each fanout is a ref-count bump rather than a fresh
// `String::clone` heap allocation.

fn baseline_fanout_two(event: &Event) -> (Vec<u8>, Vec<u8>) {
    // Simulate "two subscribers each get their own frame" the way
    // `server.rs` pre-F-112 did: two fresh `to_value` walks, two fresh
    // `to_vec` walks.
    let v1 = baseline_event_to_bytes_via_value(event);
    let v2 = baseline_event_to_bytes_via_value(event);
    (v1, v2)
}

fn typed_fanout_two(event: &Event) -> (Vec<u8>, Vec<u8>) {
    let v1 = typed_event_to_bytes(event);
    let v2 = typed_event_to_bytes(event);
    (v1, v2)
}

fn bench_frame(c: &mut Criterion) {
    let delta: Arc<str> = Arc::from(REALISTIC_DELTA);
    let event = make_delta(Arc::clone(&delta));

    let mut group = c.benchmark_group("assistant_delta_frame");
    // Sample enough to make the 30% gap statistically detectable on noisy
    // developer laptops; criterion's default 100 samples is fine but the
    // measurement window at ~1µs/frame is tight, so use a longer window.
    group.measurement_time(Duration::from_secs(5));

    group.bench_function("baseline_event_to_value_to_bytes", |b| {
        b.iter(|| {
            let bytes = baseline_event_to_bytes_via_value(black_box(&event));
            black_box(bytes);
        })
    });

    group.bench_function("typed_event_to_bytes", |b| {
        b.iter(|| {
            let bytes = typed_event_to_bytes(black_box(&event));
            black_box(bytes);
        })
    });

    group.finish();

    let mut group = c.benchmark_group("assistant_delta_fanout_two");
    group.measurement_time(Duration::from_secs(5));

    group.bench_function("baseline_fanout_two", |b| {
        b.iter(|| {
            let out = baseline_fanout_two(black_box(&event));
            black_box(out);
        })
    });

    group.bench_function("typed_fanout_two", |b| {
        b.iter(|| {
            let out = typed_fanout_two(black_box(&event));
            black_box(out);
        })
    });

    group.finish();
}

// ── F-565: read_frame allocation comparison ──────────────────────────────
//
// The DoD requires ≥30% allocation reduction at 1 KB body between the
// pre-fix `read_frame` (fresh `Vec<u8>` per call) and the post-fix
// `read_frame_into` (caller-owned, reused `Vec<u8>`). We exercise both
// over an in-memory `UnixStream::pair` carrying a synthetic `Hello`
// frame whose JSON body is padded to the requested length, then report
// per-iteration alloc counts using the counting allocator above.

fn padded_hello(body_len: usize) -> IpcMessage {
    let pad = "x".repeat(body_len.saturating_sub(64));
    IpcMessage::Hello(forge_ipc::Hello {
        proto: forge_ipc::PROTO_VERSION,
        client: forge_ipc::ClientInfo {
            kind: pad,
            pid: 1,
            user: "b".to_string(),
        },
    })
}

/// F-565: one-shot alloc-count comparison printed before criterion runs.
/// Criterion tracks wall time, not allocation count; this side-channel
/// makes the ≥30% allocation-reduction DoD checkable from the bench
/// output directly. Both paths read the same N frames over a fresh UDS
/// pair; the only difference is whether the read buffer is hoisted.
fn report_alloc_reduction(rt: &Runtime) {
    const N: usize = 256;
    for body_len in [64usize, 1024, 32 * 1024] {
        let msg = padded_hello(body_len);
        let frame_bytes = serde_json::to_vec(&msg).expect("to_vec");

        let (per_call_n, per_call_b) = rt.block_on(async {
            let (mut a, mut srv) = UnixStream::pair().expect("pair");
            let bytes = frame_bytes.clone();
            let writer = tokio::spawn(async move {
                for _ in 0..N {
                    a.write_u32(bytes.len() as u32).await.ok();
                    a.write_all(&bytes).await.ok();
                }
                a.shutdown().await.ok();
            });
            let n_before = alloc_count();
            let b_before = alloc_bytes();
            for _ in 0..N {
                let _: IpcMessage = read_frame(&mut srv).await.expect("read_frame");
            }
            let n_after = alloc_count();
            let b_after = alloc_bytes();
            writer.await.ok();
            (n_after - n_before, b_after - b_before)
        });

        let (reused_n, reused_b) = rt.block_on(async {
            let (mut a, mut srv) = UnixStream::pair().expect("pair");
            let bytes = frame_bytes.clone();
            let writer = tokio::spawn(async move {
                for _ in 0..N {
                    a.write_u32(bytes.len() as u32).await.ok();
                    a.write_all(&bytes).await.ok();
                }
                a.shutdown().await.ok();
            });
            let mut buf: Vec<u8> = Vec::with_capacity(4096);
            let n_before = alloc_count();
            let b_before = alloc_bytes();
            for _ in 0..N {
                let _: IpcMessage = read_frame_into(&mut srv, &mut buf)
                    .await
                    .expect("read_frame_into");
            }
            let n_after = alloc_count();
            let b_after = alloc_bytes();
            writer.await.ok();
            (n_after - n_before, b_after - b_before)
        });

        let per_call = per_call_n;
        let reused = reused_n;

        // Per-frame allocs: divide by N to factor out the fixed
        // tokio/UDS setup cost that pollutes both runs equally. The
        // DoD's "≥30% allocation reduction at 1 KB body" is a per-frame
        // claim; the absolute per-bench numbers above include setup.
        let per_call_pf = per_call as f64 / N as f64;
        let reused_pf = reused as f64 / N as f64;
        let reduction = if per_call_pf <= reused_pf {
            0.0
        } else {
            100.0 * (per_call_pf - reused_pf) / per_call_pf
        };
        let bytes_pf_per = per_call_b as f64 / N as f64;
        let bytes_pf_reu = reused_b as f64 / N as f64;
        let bytes_red = if bytes_pf_per <= bytes_pf_reu {
            0.0
        } else {
            100.0 * (bytes_pf_per - bytes_pf_reu) / bytes_pf_per
        };
        eprintln!(
            "[F-565] body={:>6}B frames={N} count: {:.2}->{:.2}/frame ({:.1}% red) | bytes: {:.0}->{:.0}/frame ({:.1}% red)",
            body_len, per_call_pf, reused_pf, reduction, bytes_pf_per, bytes_pf_reu, bytes_red
        );
    }
}

fn bench_read_paths(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio rt");
    report_alloc_reduction(&rt);
    let mut group = c.benchmark_group("read_frame_alloc");
    group.measurement_time(Duration::from_secs(4));

    for body_len in [64usize, 1024, 32 * 1024] {
        // Number of frames per single `iter` invocation. Higher batch
        // amortizes UDS connect cost and gives the alloc counter a
        // wider gap to reason about.
        let n: usize = 64;
        let msg = padded_hello(body_len);

        group.bench_with_input(
            BenchmarkId::new("per_call_fresh_vec", body_len),
            &body_len,
            |b, _| {
                b.iter(|| {
                    rt.block_on(async {
                        let (mut a, mut srv) = UnixStream::pair().expect("pair");
                        let frame_bytes = serde_json::to_vec(&msg).expect("to_vec");
                        let writer = tokio::spawn(async move {
                            for _ in 0..n {
                                a.write_u32(frame_bytes.len() as u32).await.ok();
                                a.write_all(&frame_bytes).await.ok();
                            }
                            a.shutdown().await.ok();
                        });
                        let before = alloc_count();
                        for _ in 0..n {
                            let frame: IpcMessage = read_frame(&mut srv).await.expect("read_frame");
                            black_box(frame);
                        }
                        let allocs = alloc_count() - before;
                        writer.await.ok();
                        black_box(allocs);
                    });
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("reused_vec", body_len),
            &body_len,
            |b, _| {
                b.iter(|| {
                    rt.block_on(async {
                        let (mut a, mut srv) = UnixStream::pair().expect("pair");
                        let frame_bytes = serde_json::to_vec(&msg).expect("to_vec");
                        let writer = tokio::spawn(async move {
                            for _ in 0..n {
                                a.write_u32(frame_bytes.len() as u32).await.ok();
                                a.write_all(&frame_bytes).await.ok();
                            }
                            a.shutdown().await.ok();
                        });
                        let mut buf: Vec<u8> = Vec::with_capacity(4096);
                        let before = alloc_count();
                        for _ in 0..n {
                            let frame: IpcMessage = read_frame_into(&mut srv, &mut buf)
                                .await
                                .expect("read_frame_into");
                            black_box(frame);
                        }
                        let allocs = alloc_count() - before;
                        writer.await.ok();
                        black_box(allocs);
                    });
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_frame, bench_read_paths);
criterion_main!(benches);
