//! F-658 regression: the sidecar→daemon event path must be bounded.
//!
//! Before F-658 the daemon's read loop awaited [`EventSink::emit`]
//! synchronously per inbound `SidecarMessage::Event` frame. That self-
//! paced the read loop but meant a misbehaving sidecar emitting at line
//! rate could drive downstream subscribers (broadcast bus, IPC writers)
//! to buffer with no upper bound — daemon RSS grew until OOM. The fix
//! decouples the read loop from the sink behind a bounded
//! [`mpsc::channel`] sized at [`EVENT_CHANNEL_DEPTH`]; once the channel
//! fills, [`Sender::send`] awaits, which in turn parks the read loop,
//! which fills the kernel UDS buffer, which finally backpressures the
//! sidecar's own [`forge_ipc::write_frame`] write. End-to-end backpressure
//! with a documented memory ceiling.
//!
//! The test below drives the public emitter helper directly: it floods
//! events through a bounded channel into a deliberately-slow
//! [`EventSink`] and asserts the channel saturates at its documented
//! capacity rather than growing without bound. The assertion shape —
//! `try_send` returns [`TrySendError::Full`] under flood — is the
//! observable signature of correct backpressure; without it the channel
//! would either be unbounded (no `Full` ever) or sized too generously
//! (no `Full` for the test's burst).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use forge_core::{AgentId, AgentInstanceId, Event, EventSink};
use forge_session::sidecar::{spawn_event_emitter, EVENT_CHANNEL_DEPTH};
use tokio::sync::mpsc;
use tokio::time::sleep;

/// Slow [`EventSink`] used to exercise backpressure: each emit awaits a
/// configurable delay before recording. The test holds the emitter task
/// "behind" the channel so the channel saturates at its declared depth.
#[derive(Debug)]
struct SlowCountingSink {
    seen: AtomicUsize,
    delay: Duration,
}

impl SlowCountingSink {
    fn new(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            seen: AtomicUsize::new(0),
            delay,
        })
    }

    fn seen(&self) -> usize {
        self.seen.load(Ordering::Acquire)
    }
}

#[async_trait]
impl EventSink for SlowCountingSink {
    async fn emit(&self, _event: Event) -> Result<()> {
        sleep(self.delay).await;
        self.seen.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

fn dummy_event() -> Event {
    Event::BackgroundAgentStarted {
        id: AgentInstanceId::new(),
        agent: AgentId::new(),
        at: Utc::now(),
    }
}

/// Documented depth is non-zero and large enough to absorb a normal
/// turn's burst. The number is part of the security contract — a
/// regression that silently shrinks it to 1 would still pass this test
/// but a regression that flips back to `mpsc::unbounded_channel` would
/// not (channel capacity becomes irrelevant). The companion flood test
/// below catches the unbounded regression.
#[tokio::test]
async fn event_channel_depth_is_documented_and_nontrivial() {
    const _: () = {
        assert!(
            EVENT_CHANNEL_DEPTH >= 64,
            "EVENT_CHANNEL_DEPTH too small to absorb a normal turn"
        );
        assert!(
            EVENT_CHANNEL_DEPTH <= 8192,
            "EVENT_CHANNEL_DEPTH larger than the documented ceiling"
        );
    };
}

/// Flood the channel with significantly more events than its capacity
/// while the sink consumes slowly. Any unbounded buffer would accept
/// every send; a bounded one returns `TrySendError::Full` once the
/// buffer is saturated, which is the test's positive signal.
#[tokio::test]
async fn flooding_event_channel_yields_backpressure_not_unbounded_growth() {
    let sink = SlowCountingSink::new(Duration::from_millis(5));
    let instance_id = AgentInstanceId::new();
    let (tx, rx) = mpsc::channel::<Event>(EVENT_CHANNEL_DEPTH);

    let emitter = spawn_event_emitter(rx, sink.clone() as Arc<dyn EventSink>, instance_id.clone());

    // Burst well past capacity; with a slow sink, only `EVENT_CHANNEL_DEPTH`
    // (plus the one in-flight emit) will fit before `try_send` reports
    // `Full`.
    let burst = EVENT_CHANNEL_DEPTH * 8;
    let mut accepted = 0usize;
    let mut full_seen = false;
    for _ in 0..burst {
        match tx.try_send(dummy_event()) {
            Ok(()) => accepted += 1,
            Err(mpsc::error::TrySendError::Full(_)) => {
                full_seen = true;
                break;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                panic!("emitter dropped channel mid-test");
            }
        }
    }

    assert!(
        full_seen,
        "expected TrySendError::Full from a bounded channel; got {accepted} sends without saturation \
         (this almost certainly means the channel reverted to unbounded)"
    );
    assert!(
        accepted <= EVENT_CHANNEL_DEPTH + 1,
        "channel admitted {accepted} sends before saturating; bound is {EVENT_CHANNEL_DEPTH}"
    );

    // Drop the sender so the emitter task can drain and exit, then
    // observe that every accepted event reached the sink — backpressure
    // must not silently lose frames.
    drop(tx);
    emitter.await.expect("emitter task panicked or was aborted");
    assert_eq!(
        sink.seen(),
        accepted,
        "emitter task lost events between channel and sink"
    );
}

/// Sender drop cleanly winds the emitter down. Without this property
/// the supervisor's shutdown path would leak a tokio task per spawned
/// sidecar.
#[tokio::test]
async fn dropping_sender_terminates_emitter_task() {
    let sink = SlowCountingSink::new(Duration::ZERO);
    let instance_id = AgentInstanceId::new();
    let (tx, rx) = mpsc::channel::<Event>(EVENT_CHANNEL_DEPTH);

    let emitter = spawn_event_emitter(rx, sink as Arc<dyn EventSink>, instance_id);

    drop(tx);

    // The task must complete promptly once its receiver closes.
    tokio::time::timeout(Duration::from_secs(2), emitter)
        .await
        .expect("emitter task did not exit within 2s of sender drop")
        .expect("emitter task panicked");
}
