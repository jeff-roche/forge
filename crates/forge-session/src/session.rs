use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use forge_core::{Event, EventLog};
use tokio::sync::{broadcast, Mutex, Notify};

use crate::error::SessionError;

/// F-604: in-memory snapshot of an interrupt-driven refine handoff. The
/// orchestrator populates this when its stream loop notices the
/// interrupt flag and breaks out at a clean chunk boundary; the daemon
/// IPC handler reads it back to compose the `RefineHandoff` response and
/// the `Event::SessionInterrupted` payload.
///
/// Lives in `forge-session` (not `forge-ipc`) so the Session struct does
/// not pull in IPC types — the server-side adapter at the IPC boundary
/// translates between this snapshot and the wire shape.
#[derive(Debug, Clone, Default)]
pub struct InterruptCapture {
    /// Assistant text accumulated up to the interrupt point. Empty when
    /// the interrupt landed before any `AssistantDelta` could fire (or
    /// when no assistant turn was in flight at all).
    pub partial_text: String,
    /// `MessageId` of the assistant turn that owned the partial. Empty
    /// `String` shape when no turn was in flight (no-op interrupt).
    pub captured_at_msg_id: String,
    /// `StepId` of the model step that was interrupted. Empty when no
    /// turn was in flight.
    pub captured_at_step_id: String,
}

pub struct Session {
    pub log_path: PathBuf,
    pub event_tx: broadcast::Sender<(u64, Event)>,
    log: Arc<Mutex<EventLog>>,
    seq: Arc<Mutex<u64>>,
    /// F-598: tripped while [`crate::compaction::compact`] is running so the
    /// orchestrator's auto-trigger never re-enters compaction during the
    /// privileged summary call. The summary stream emits events that
    /// re-enter the same session log; without the guard a misbehaving
    /// provider that drives the byte budget over the threshold mid-summary
    /// could fire a second compaction concurrently.
    compacting: Arc<AtomicBool>,
    /// F-599: monotonic counter for `parallel_group` ids assigned to
    /// concurrent tool batches. Lifted to session scope so ids stay
    /// unique across model passes — pre-fix the counter reset on every
    /// `dispatch_tool_calls` call, so any UI consumer that keys on
    /// `(session_id, parallel_group)` saw collisions across passes.
    parallel_group_seq: Arc<AtomicU32>,
    /// F-603: orchestrator pause state (in-memory only, not persisted).
    ///
    /// Set by `try_pause` on the daemon's IPC handler thread; observed by
    /// `wait_if_paused` between steps in the orchestrator's request loop.
    /// `try_resume` clears the flag and notifies any waiter so the next
    /// step opens immediately — `Notify::notify_waiters` only fires on a
    /// real `Paused → Running` transition (the IPC handler suppresses
    /// redundant calls), so a parked orchestrator reliably wakes exactly
    /// once per resume. Tool-in-flight semantics: the checkpoint sits
    /// **between** steps, so a paused orchestrator never aborts an
    /// in-flight model stream or tool call mid-flight; it parks at the
    /// next clean step boundary.
    paused: Arc<AtomicBool>,
    /// F-603: paired with `paused`. The orchestrator's pause checkpoint
    /// awaits this `Notify` while the flag is set; `try_resume` calls
    /// `notify_waiters` to wake it. `Notify` is sufficient (no payload)
    /// — the wakers re-check the flag and decide whether to keep
    /// looping; they do **not** trust the wake itself as a state
    /// transition signal.
    resume_notify: Arc<Notify>,
    /// F-604: orchestrator interrupt request flag (in-memory only, not
    /// persisted). Set by `request_interrupt` on the daemon's IPC
    /// handler thread; observed by the orchestrator's stream loop on
    /// every chunk so a mid-stream request takes effect at the next
    /// chunk boundary (a clean point where `assistant_text` reflects
    /// every delta the daemon has already emitted). Differs from
    /// `paused` in two ways: (1) consumed-on-detect — the orchestrator
    /// clears it after handling so a subsequent turn starts in a clean
    /// state; (2) it kills the in-flight turn rather than parking it.
    interrupt_requested: Arc<AtomicBool>,
    /// F-604: handoff payload populated by the orchestrator at the
    /// interrupt boundary. The daemon IPC handler reads it back to
    /// compose the `RefineHandoff` response and the
    /// `Event::SessionInterrupted` payload, then calls
    /// `Session::take_interrupt_capture` to clear the slot.
    /// `notify_waiters` on `interrupt_done` fires when the slot is
    /// populated so a waiting IPC handler unblocks promptly.
    interrupt_capture: Arc<Mutex<Option<InterruptCapture>>>,
    /// F-604: paired with `interrupt_capture`. The IPC handler awaits
    /// this `Notify` while the orchestrator captures the partial; the
    /// orchestrator calls `notify_waiters` after publishing the
    /// capture. The handler re-checks the slot on each wake and the
    /// flag on a deadline so a no-op interrupt (no in-flight turn)
    /// resolves promptly via the deadline path rather than stalling.
    interrupt_done: Arc<Notify>,
}

impl Session {
    pub async fn create(log_path: PathBuf) -> anyhow::Result<Self> {
        if let Some(parent) = log_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let log = EventLog::create(&log_path)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        let (tx, _) = broadcast::channel(1024);
        Ok(Self {
            log_path,
            event_tx: tx,
            log: Arc::new(Mutex::new(log)),
            seq: Arc::new(Mutex::new(0)),
            compacting: Arc::new(AtomicBool::new(false)),
            parallel_group_seq: Arc::new(AtomicU32::new(1)),
            paused: Arc::new(AtomicBool::new(false)),
            resume_notify: Arc::new(Notify::new()),
            interrupt_requested: Arc::new(AtomicBool::new(false)),
            interrupt_capture: Arc::new(Mutex::new(None)),
            interrupt_done: Arc::new(Notify::new()),
        })
    }

    /// F-603: flip the pause flag. Returns `true` if this call performed
    /// the `Running → Paused` transition (caller should emit
    /// `Event::SessionPaused`); `false` if the session was already paused
    /// (caller logs `debug!` and emits no event — idempotency contract).
    pub fn try_pause(&self) -> bool {
        self.paused
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// F-603: clear the pause flag and wake any orchestrator parked at the
    /// pause checkpoint. Returns `true` if this call performed the
    /// `Paused → Running` transition (caller should emit
    /// `Event::SessionResumed`); `false` if the session was already
    /// running (caller logs `debug!` and emits no event — idempotency
    /// contract). `notify_waiters` only fires on a real transition, so a
    /// no-op resume does not spuriously wake the orchestrator.
    pub fn try_resume(&self) -> bool {
        let transitioned = self
            .paused
            .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if transitioned {
            self.resume_notify.notify_waiters();
        }
        transitioned
    }

    /// F-603: observe the pause flag without mutating it. Used in tests
    /// and (potentially) introspection IPC — the orchestrator checkpoint
    /// uses [`Self::wait_if_paused`] instead because it also needs to
    /// park.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// F-603: orchestrator's between-step checkpoint. Returns immediately
    /// when the session is running; parks on `resume_notify` when the
    /// session is paused, re-checking the flag on each wake to defend
    /// against spurious notifications. Cancellation-safe — dropping the
    /// returned future leaves both the flag and the notify in a
    /// consistent state because `Notify::notified()` does not consume a
    /// permit until polled to readiness.
    pub async fn wait_if_paused(&self) {
        while self.paused.load(Ordering::SeqCst) {
            // Register interest BEFORE re-checking the flag to defeat the
            // pause/notify/wait race: if `try_resume` flips the flag and
            // calls `notify_waiters` between our load above and the
            // `notified().await` below, registering first guarantees the
            // notification is delivered to us rather than dropped.
            let notified = self.resume_notify.notified();
            tokio::pin!(notified);
            // Re-check after registration; resume may have landed in
            // between the outer `load` and the registration above.
            if !self.paused.load(Ordering::SeqCst) {
                break;
            }
            notified.await;
        }
    }

    /// F-604: flip the interrupt-request flag. Returns `true` if this
    /// call performed the `clear → set` transition (i.e. there was no
    /// outstanding interrupt request). The orchestrator's stream loop
    /// observes the flag on every chunk and breaks out at the next
    /// chunk boundary; if no assistant turn is in flight, the flag is
    /// cleared by the no-op handoff path on the IPC handler side
    /// (see `try_take_no_inflight_handoff`).
    pub fn request_interrupt(&self) -> bool {
        self.interrupt_requested
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// F-604: observe the interrupt flag without consuming it. The
    /// orchestrator's stream loop polls this on every chunk; a `true`
    /// return is the signal to break out of the loop and capture the
    /// partial.
    pub fn is_interrupt_requested(&self) -> bool {
        self.interrupt_requested.load(Ordering::SeqCst)
    }

    /// F-604: orchestrator-side: publish the captured handoff and clear
    /// the request flag. Wakes any IPC handler awaiting the response
    /// via `await_interrupt_capture`.
    pub async fn publish_interrupt_capture(&self, capture: InterruptCapture) {
        {
            let mut slot = self.interrupt_capture.lock().await;
            *slot = Some(capture);
        }
        // Clear the request flag AFTER publishing the capture so a
        // racing `is_interrupt_requested` poll on the orchestrator side
        // either sees the flag still set (and breaks out — but the
        // capture is now visible) or the flag clear (and proceeds
        // normally — which is the post-handoff steady state).
        self.interrupt_requested.store(false, Ordering::SeqCst);
        self.interrupt_done.notify_waiters();
    }

    /// F-604: IPC-side: take ownership of any pending interrupt capture
    /// the orchestrator has published, clearing the slot. Returns
    /// `None` when the orchestrator has not yet published (the IPC
    /// handler should park on `await_interrupt_capture` then) or when
    /// the slot was already drained by an earlier call.
    pub async fn take_interrupt_capture(&self) -> Option<InterruptCapture> {
        self.interrupt_capture.lock().await.take()
    }

    /// F-604: IPC-side: park until the orchestrator publishes a capture
    /// or `timeout` elapses. Used by the daemon's
    /// `IpcMessage::InterruptSession` handler to bound the wait so a
    /// no-in-flight-turn case doesn't stall the request loop. Returns
    /// `Some(capture)` on success; `None` on timeout (caller should
    /// then synthesize the no-op shape).
    pub async fn await_interrupt_capture(
        &self,
        timeout: std::time::Duration,
    ) -> Option<InterruptCapture> {
        // Fast path: capture may have been published before we arrived.
        if let Some(cap) = self.take_interrupt_capture().await {
            return Some(cap);
        }
        // Register the notify waiter BEFORE taking again so a publish
        // racing with our second `take` cannot land between the two
        // (otherwise we'd register after the wake fired and stall).
        let notified = self.interrupt_done.notified();
        tokio::pin!(notified);
        if let Some(cap) = self.take_interrupt_capture().await {
            return Some(cap);
        }
        match tokio::time::timeout(timeout, notified).await {
            Ok(()) => self.take_interrupt_capture().await,
            Err(_) => None,
        }
    }

    /// F-604: clear the interrupt request flag without publishing a
    /// capture. Used by the IPC handler's no-op shortcut when the flag
    /// flip happened but the orchestrator has no in-flight turn to
    /// observe it (e.g. the daemon idled between turns). Without this
    /// the flag would persist into the next turn and short-circuit it.
    pub fn clear_interrupt_request(&self) {
        self.interrupt_requested.store(false, Ordering::SeqCst);
    }

    /// F-599: allocate a fresh `parallel_group` id for a concurrent tool
    /// batch. Monotonic across the session lifetime so `(session_id,
    /// parallel_group)` is unique even across model passes — UI consumers
    /// can key on it as a stable batch identifier.
    pub fn next_parallel_group(&self) -> u32 {
        self.parallel_group_seq.fetch_add(1, Ordering::Relaxed)
    }

    /// F-598: returns `true` if a [`crate::compaction::compact`] pass has
    /// claimed the guard slot, `false` if one was already in flight. The
    /// caller MUST pair a successful claim with [`Self::release_compacting`]
    /// (typically via a guard struct) so a panic mid-compaction doesn't
    /// strand the flag set forever.
    pub fn try_claim_compacting(&self) -> bool {
        self.compacting
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// F-598: drop the in-flight compaction marker. Idempotent — calling on
    /// a clear flag is a no-op.
    pub fn release_compacting(&self) {
        self.compacting.store(false, Ordering::SeqCst);
    }

    /// F-598: observe the in-flight compaction flag without claiming it.
    /// Used by the orchestrator's auto-trigger to skip a second pass while
    /// one is already running.
    pub fn is_compacting(&self) -> bool {
        self.compacting.load(Ordering::SeqCst)
    }

    /// Append `event` to the durable event log and broadcast it to
    /// subscribers.
    ///
    /// F-076: returns the typed [`SessionError`] so callers can
    /// distinguish an append failure (event never staged) from a flush
    /// failure (event staged but durability uncertain). The broadcast
    /// `send` failure is intentionally swallowed — `broadcast::Sender`
    /// returns `Err` when zero receivers are subscribed, which is the
    /// normal warmup state and not an error condition.
    pub async fn emit(&self, event: Event) -> Result<(), SessionError> {
        let mut seq = self.seq.lock().await;
        *seq += 1;
        let seq_num = *seq;

        let mut log = self.log.lock().await;
        log.append(&event)
            .await
            .map_err(SessionError::EventLogAppend)?;
        log.flush().await.map_err(SessionError::EventLogFlush)?;
        drop(log);
        drop(seq);

        let _ = self.event_tx.send((seq_num, event));
        Ok(())
    }

    pub async fn current_seq(&self) -> u64 {
        *self.seq.lock().await
    }
}

#[cfg(test)]
mod tests {
    //! F-603: unit coverage for the pause/resume primitives on `Session`.
    //! End-to-end pause-mid-stream coverage lives in
    //! `tests/pause_resume.rs`.
    use super::*;
    use tempfile::TempDir;

    async fn fresh_session() -> (TempDir, Arc<Session>) {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log).await.unwrap());
        (dir, session)
    }

    #[tokio::test]
    async fn try_pause_returns_true_only_on_running_to_paused_transition() {
        let (_dir, session) = fresh_session().await;
        assert!(!session.is_paused());
        assert!(session.try_pause(), "first pause must transition");
        assert!(session.is_paused());
        assert!(
            !session.try_pause(),
            "redundant pause must not re-transition (idempotency)",
        );
        assert!(session.is_paused());
    }

    #[tokio::test]
    async fn try_resume_returns_true_only_on_paused_to_running_transition() {
        let (_dir, session) = fresh_session().await;
        assert!(
            !session.try_resume(),
            "resume on a running session must be a no-op",
        );
        assert!(!session.is_paused());
        assert!(session.try_pause());
        assert!(session.try_resume(), "first resume must transition");
        assert!(!session.is_paused());
        assert!(
            !session.try_resume(),
            "redundant resume must not re-transition",
        );
    }

    #[tokio::test]
    async fn wait_if_paused_returns_immediately_when_running() {
        let (_dir, session) = fresh_session().await;
        // Without a pause, the future should resolve right away.
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            session.wait_if_paused(),
        )
        .await
        .expect("wait_if_paused must return immediately when running");
    }

    #[tokio::test]
    async fn wait_if_paused_blocks_until_resume() {
        let (_dir, session) = fresh_session().await;
        session.try_pause();

        let session_clone = Arc::clone(&session);
        let waiter = tokio::spawn(async move { session_clone.wait_if_paused().await });

        // Confirm the waiter has not yet resolved.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "waiter must park while paused");

        // Resume; the waiter should now complete.
        session.try_resume();
        tokio::time::timeout(std::time::Duration::from_millis(500), waiter)
            .await
            .expect("waiter must wake on resume")
            .unwrap();
    }

    // ---------- F-604: interrupt / refine primitive ----------

    #[tokio::test]
    async fn request_interrupt_returns_true_only_on_clear_to_set_transition() {
        let (_dir, session) = fresh_session().await;
        assert!(!session.is_interrupt_requested());
        assert!(session.request_interrupt(), "first request must transition");
        assert!(session.is_interrupt_requested());
        assert!(
            !session.request_interrupt(),
            "redundant request must not re-transition (idempotency)",
        );
    }

    #[tokio::test]
    async fn publish_interrupt_capture_clears_request_and_publishes_capture() {
        let (_dir, session) = fresh_session().await;
        session.request_interrupt();
        let capture = InterruptCapture {
            partial_text: "half an answer".into(),
            captured_at_msg_id: "mid-int".into(),
            captured_at_step_id: "step-int".into(),
        };
        session.publish_interrupt_capture(capture).await;
        assert!(
            !session.is_interrupt_requested(),
            "publishing must clear the request flag",
        );
        let taken = session
            .take_interrupt_capture()
            .await
            .expect("capture must be retrievable");
        assert_eq!(taken.partial_text, "half an answer");
        assert_eq!(taken.captured_at_msg_id, "mid-int");
        assert_eq!(taken.captured_at_step_id, "step-int");
        // Second take returns None — the slot is consumed.
        assert!(session.take_interrupt_capture().await.is_none());
    }

    #[tokio::test]
    async fn await_interrupt_capture_resolves_when_orchestrator_publishes() {
        let (_dir, session) = fresh_session().await;
        let session_clone = Arc::clone(&session);
        let waiter = tokio::spawn(async move {
            session_clone
                .await_interrupt_capture(std::time::Duration::from_secs(2))
                .await
        });

        // Yield once to let the waiter register its `notified()` slot
        // BEFORE the publish fires — exercises the race-defending
        // re-check path.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let capture = InterruptCapture {
            partial_text: "raced answer".into(),
            captured_at_msg_id: "mid-r".into(),
            captured_at_step_id: "step-r".into(),
        };
        session.publish_interrupt_capture(capture).await;

        let resolved = tokio::time::timeout(std::time::Duration::from_millis(500), waiter)
            .await
            .expect("waiter must resolve")
            .unwrap();
        let cap = resolved.expect("await_interrupt_capture must yield Some");
        assert_eq!(cap.partial_text, "raced answer");
    }

    #[tokio::test]
    async fn await_interrupt_capture_times_out_when_no_publish() {
        let (_dir, session) = fresh_session().await;
        // No publish; the deadline path must fire.
        let result = session
            .await_interrupt_capture(std::time::Duration::from_millis(100))
            .await;
        assert!(
            result.is_none(),
            "await must yield None on deadline expiry (no orchestrator publish)",
        );
    }

    #[tokio::test]
    async fn clear_interrupt_request_drops_flag_without_publishing() {
        let (_dir, session) = fresh_session().await;
        assert!(session.request_interrupt());
        assert!(session.is_interrupt_requested());
        session.clear_interrupt_request();
        assert!(!session.is_interrupt_requested());
        // No capture was published, so take returns None.
        assert!(session.take_interrupt_capture().await.is_none());
    }
}
