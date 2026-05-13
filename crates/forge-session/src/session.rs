use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use forge_core::{Event, EventLog, EventSink};
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
    /// step opens immediately. Tool-in-flight semantics: the checkpoint
    /// sits **between** steps, so a paused orchestrator never aborts an
    /// in-flight model stream or tool call mid-flight; it parks at the
    /// next clean step boundary.
    paused: Arc<AtomicBool>,
    /// F-653: monotonic counter incremented on every `Running → Paused`
    /// transition (i.e. on every `try_pause` that performs the flip).
    /// The orchestrator captures this epoch at the top of each request
    /// loop iteration and passes it back into [`Self::wait_if_paused`];
    /// the checkpoint parks not only when the flag is currently set but
    /// also when the epoch has advanced since the orchestrator last
    /// observed it. Without the epoch, a tight `try_pause` → `try_resume`
    /// cycle that completed entirely *between* the orchestrator's prior
    /// step and the next checkpoint would leave the flag clear, the
    /// checkpoint would not park, and a tool effect could fire after
    /// `SessionPaused` was acknowledged on the wire.
    pause_epoch: Arc<AtomicU64>,
    /// F-666 hygiene: serialises the state-flip and the event emission for
    /// pause/resume. Without this lock, two concurrent pause/resume
    /// callers could win the atomic CAS in one order but reach
    /// `Session::emit` (which awaits the underlying log mutex) in the
    /// opposite order — consumers replaying the event stream would see
    /// `Paused`/`Resumed` events out of sequence with the actual state
    /// transitions. `pause_and_emit_if_transitioned` /
    /// `resume_and_emit_if_transitioned` hold this lock across both
    /// steps so the observable order is "transition → event"
    /// monotonically.
    pause_emit_lock: Arc<Mutex<()>>,
    /// F-603: paired with `paused`. The orchestrator's pause checkpoint
    /// awaits this `Notify` while the flag is set; `try_resume` calls
    /// `notify_one` to wake it. F-653: switched from `notify_waiters` to
    /// `notify_one` so a resume that lands *before* the orchestrator
    /// reaches the checkpoint retains a permit; the next `notified()`
    /// consumes it without parking, so the orchestrator never stalls
    /// after a pause/resume cycle that raced ahead of it.
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
        Ok(Self::with_log_at_seq(log_path, log, 0))
    }

    /// F-748: re-open an existing event log instead of truncating it. Used
    /// when a daemon is re-spawned for an existing `session_id` (crash-
    /// restart prompt). Seeds the seq counter from the last well-formed
    /// event so subsequent `emit` calls continue the monotonic sequence,
    /// and `current_seq()` returns the resume anchor for the bridge to
    /// pass as `Subscribe { since }`.
    ///
    /// F-748 review fix: a daemon SIGKILL'd mid-`write_all` can leave a
    /// truncated or malformed final line. The strict [`forge_core::read_since`]
    /// errors out on that line — the previous code path therefore failed
    /// to resume at all when a real crash corrupted the tail. The fix:
    /// run the recovery scanner [`forge_core::recover_tail`] which
    /// reports the byte offset of the last well-formed line; truncate
    /// the file to that offset before reopening for append. This makes
    /// both `resume()` AND future strict readers tolerant of one bad
    /// trailing line.
    ///
    /// On a missing log file this falls through to [`Self::create`] — the
    /// caller is expected to gate on file existence when it matters
    /// (`forge-session::main`).
    pub async fn resume(log_path: PathBuf) -> anyhow::Result<Self> {
        if !log_path.exists() {
            return Self::create(log_path).await;
        }
        let recovered = forge_core::recover_tail(&log_path)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

        // Truncate any corrupt suffix so the on-disk file is well-formed
        // again. `set_len` is durable across the upcoming `EventLog::open`
        // because the open() call re-opens the file under the same path.
        // We only call truncate when the file has actual garbage past the
        // last good byte; otherwise this is a no-op.
        let file_len = tokio::fs::metadata(&log_path).await?.len();
        if recovered.safe_byte_len < file_len {
            let file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&log_path)
                .await?;
            file.set_len(recovered.safe_byte_len).await?;
            file.sync_all().await?;
            drop(file);
        }

        let log = EventLog::open(&log_path)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(Self::with_log_at_seq(
            log_path,
            log,
            recovered.last_good_seq,
        ))
    }

    fn with_log_at_seq(log_path: PathBuf, log: EventLog, seq: u64) -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            log_path,
            event_tx: tx,
            log: Arc::new(Mutex::new(log)),
            seq: Arc::new(Mutex::new(seq)),
            compacting: Arc::new(AtomicBool::new(false)),
            parallel_group_seq: Arc::new(AtomicU32::new(1)),
            paused: Arc::new(AtomicBool::new(false)),
            pause_epoch: Arc::new(AtomicU64::new(0)),
            pause_emit_lock: Arc::new(Mutex::new(())),
            resume_notify: Arc::new(Notify::new()),
            interrupt_requested: Arc::new(AtomicBool::new(false)),
            interrupt_capture: Arc::new(Mutex::new(None)),
            interrupt_done: Arc::new(Notify::new()),
        }
    }

    /// F-603: flip the pause flag. Returns `true` if this call performed
    /// the `Running → Paused` transition (caller should emit
    /// `Event::SessionPaused`); `false` if the session was already paused
    /// (caller logs `debug!` and emits no event — idempotency contract).
    ///
    /// F-653: a successful transition also bumps `pause_epoch` so a
    /// subsequent [`Self::wait_if_paused`] can detect a pause that
    /// occurred (and possibly already resumed) since the orchestrator's
    /// last observed epoch. The bump happens **after** the CAS so a lost
    /// race never advances the epoch.
    pub fn try_pause(&self) -> bool {
        let transitioned = self
            .paused
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if transitioned {
            self.pause_epoch.fetch_add(1, Ordering::SeqCst);
        }
        transitioned
    }

    /// F-603: clear the pause flag and wake any orchestrator parked at the
    /// pause checkpoint. Returns `true` if this call performed the
    /// `Paused → Running` transition (caller should emit
    /// `Event::SessionResumed`); `false` if the session was already
    /// running (caller logs `debug!` and emits no event — idempotency
    /// contract).
    ///
    /// F-653: uses `notify_one` instead of `notify_waiters`. The
    /// difference is permit retention: `notify_one` stores a single
    /// permit when no waiter is registered, which the next
    /// `notified().await` consumes without parking. This closes the
    /// race where the orchestrator captures `last_observed_epoch < N`,
    /// the IPC handler completes a full pause/resume cycle (epoch → N,
    /// `notify_waiters` fires into the void), and the orchestrator's
    /// next checkpoint would otherwise stall on the now-defunct notify.
    pub fn try_resume(&self) -> bool {
        let transitioned = self
            .paused
            .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if transitioned {
            self.resume_notify.notify_one();
        }
        transitioned
    }

    /// F-653: read the current pause epoch. The orchestrator captures
    /// this at the top of each request-loop iteration and threads it
    /// back into [`Self::wait_if_paused`] so the checkpoint can detect
    /// pause/resume cycles that completed before the orchestrator
    /// arrived.
    pub fn pause_epoch(&self) -> u64 {
        self.pause_epoch.load(Ordering::SeqCst)
    }

    /// F-666 hygiene: atomically perform the `Running → Paused`
    /// transition **and** emit `Event::SessionPaused` under a single
    /// lock. Replaces the previous "call `try_pause` then conditionally
    /// `emit` from the caller" pattern — that pattern let two callers'
    /// events appear in the event log out of order relative to their
    /// state transitions (the CAS winner could lose the race to the
    /// log mutex). Returns `true` when this call performed the
    /// transition and emitted the event; `false` when the session was
    /// already paused (idempotent no-op; no event emitted).
    ///
    /// F-653: also bumps `pause_epoch` on transition so the
    /// orchestrator's pause checkpoint catches a quick pause/resume
    /// cycle that completed before the next step boundary.
    pub async fn pause_and_emit_if_transitioned(&self) -> Result<bool, SessionError> {
        let _guard = self.pause_emit_lock.lock().await;
        if self
            .paused
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(false);
        }
        self.pause_epoch.fetch_add(1, Ordering::SeqCst);
        self.emit(Event::SessionPaused { at: Utc::now() }).await?;
        Ok(true)
    }

    /// F-666 hygiene: companion to [`Self::pause_and_emit_if_transitioned`]
    /// for the `Paused → Running` transition. Holds the same lock so
    /// pause/resume events appear in the event log in the same order
    /// as the corresponding state transitions. Wakes any orchestrator
    /// parked on `wait_if_paused` only on a real transition.
    ///
    /// F-653: uses `notify_one` (not `notify_waiters`) so a resume that
    /// lands *before* the orchestrator reaches the checkpoint retains
    /// a permit — the next `notified()` consumes it without parking,
    /// avoiding the dropped-permit deadlock that
    /// `pause_resume_burst_before_turn_does_not_deadlock_or_leak_step`
    /// pins.
    pub async fn resume_and_emit_if_transitioned(&self) -> Result<bool, SessionError> {
        let _guard = self.pause_emit_lock.lock().await;
        if self
            .paused
            .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(false);
        }
        self.resume_notify.notify_one();
        self.emit(Event::SessionResumed { at: Utc::now() }).await?;
        Ok(true)
    }

    /// F-603: observe the pause flag without mutating it. Used in tests
    /// and (potentially) introspection IPC — the orchestrator checkpoint
    /// uses [`Self::wait_if_paused`] instead because it also needs to
    /// park.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// F-603: orchestrator's between-step checkpoint.
    ///
    /// F-653: race-free against tight `try_pause` → `try_resume` cycles
    /// that complete before the orchestrator arrives. `last_observed_epoch`
    /// is the orchestrator-side counter — initialised to `0` outside the
    /// request loop and updated on each return. The checkpoint exits
    /// only when both:
    ///
    ///   * the pause flag is currently clear, **and**
    ///   * the pause epoch matches the caller's `last_observed_epoch`
    ///     (no pause-cycle has slipped past unobserved).
    ///
    /// Otherwise the checkpoint parks on `resume_notify` and re-evaluates
    /// after each wake. Because `try_resume` uses `notify_one` (permit
    /// retention), a resume that fired before the call arrived simply
    /// satisfies the next `notified().await` without parking — the
    /// orchestrator picks up the bumped epoch on its next iteration and
    /// returns.
    ///
    /// Cancellation-safe — `Notify::notified()` does not consume a
    /// permit until polled to readiness, so dropping the future leaves
    /// pause state and any pending wake intact.
    pub async fn wait_if_paused(&self, last_observed_epoch: &mut u64) {
        loop {
            // Register interest BEFORE reading state so a `try_resume`
            // that lands between the load and the await is delivered to
            // us. (Permit retention via `notify_one` covers the case
            // where the resume completes BEFORE registration too.)
            let notified = self.resume_notify.notified();
            tokio::pin!(notified);

            let cur_epoch = self.pause_epoch.load(Ordering::SeqCst);
            let cur_paused = self.paused.load(Ordering::SeqCst);

            if !cur_paused && cur_epoch == *last_observed_epoch {
                // Steady state: no pause is active and no cycle has
                // happened since the orchestrator last checked.
                return;
            }

            // Either currently paused (wake comes from the next
            // `try_resume`) or a pause/resume cycle raced past us
            // (wake is the retained `notify_one` permit, consumed
            // immediately).
            notified.await;

            // Advance the caller's epoch view to whatever we observed
            // post-wake; the loop re-evaluates and either returns or
            // parks again if a fresh pause has since landed.
            *last_observed_epoch = self.pause_epoch.load(Ordering::SeqCst);
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

/// F-608 step 3: route the in-process orchestrator's event emissions
/// through the [`EventSink`] trait. Delegates to the existing
/// [`Session::emit`] body and re-wraps the typed [`SessionError`] into
/// `anyhow::Error` at the trait boundary so the orchestrator's
/// `anyhow::Result<()>` propagation shape is preserved.
#[async_trait]
impl EventSink for Session {
    async fn emit(&self, event: Event) -> anyhow::Result<()> {
        Session::emit(self, event).await.map_err(Into::into)
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
        let mut epoch = 0u64;
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            session.wait_if_paused(&mut epoch),
        )
        .await
        .expect("wait_if_paused must return immediately when running");
        assert_eq!(epoch, 0, "no pause cycle so observed epoch stays at 0");
    }

    #[tokio::test]
    async fn wait_if_paused_blocks_until_resume() {
        let (_dir, session) = fresh_session().await;
        session.try_pause();

        let session_clone = Arc::clone(&session);
        let waiter = tokio::spawn(async move {
            let mut epoch = 0u64;
            session_clone.wait_if_paused(&mut epoch).await;
            epoch
        });

        // Confirm the waiter has not yet resolved.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "waiter must park while paused");

        // Resume; the waiter should now complete.
        session.try_resume();
        let observed = tokio::time::timeout(std::time::Duration::from_millis(500), waiter)
            .await
            .expect("waiter must wake on resume")
            .unwrap();
        assert_eq!(observed, 1, "wake must advance observed epoch to 1");
    }

    // ---------- F-653: race-free pause/resume around the checkpoint ----------

    /// F-653: `try_pause` increments the epoch only on a real
    /// `Running → Paused` transition; idempotent calls do not.
    #[tokio::test]
    async fn try_pause_bumps_epoch_only_on_transition() {
        let (_dir, session) = fresh_session().await;
        assert_eq!(session.pause_epoch(), 0);
        assert!(session.try_pause());
        assert_eq!(session.pause_epoch(), 1);
        assert!(!session.try_pause(), "redundant pause is a no-op");
        assert_eq!(
            session.pause_epoch(),
            1,
            "redundant pause must not bump the epoch",
        );
        assert!(session.try_resume());
        assert_eq!(
            session.pause_epoch(),
            1,
            "resume does not advance the epoch",
        );
        assert!(session.try_pause());
        assert_eq!(session.pause_epoch(), 2);
    }

    /// F-653: a `try_pause` → `try_resume` cycle that completes BEFORE
    /// the orchestrator arrives at `wait_if_paused` must still cause the
    /// checkpoint to consume the resume permit and acknowledge the new
    /// epoch — without hanging. This is the regression the issue calls
    /// out: the previous `notify_waiters` implementation dropped the
    /// resume signal because there was no waiter parked yet.
    #[tokio::test]
    async fn wait_if_paused_handles_cycle_completed_before_arrival() {
        let (_dir, session) = fresh_session().await;
        // Full pause/resume cycle BEFORE the orchestrator arrives.
        assert!(session.try_pause());
        assert!(session.try_resume());
        assert!(!session.is_paused());
        assert_eq!(session.pause_epoch(), 1);

        // Orchestrator's last-observed epoch is still 0 — it never saw
        // the cycle. The checkpoint must NOT hang; the retained
        // `notify_one` permit satisfies the await, and the loop returns
        // with the epoch ack'd.
        let mut epoch = 0u64;
        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            session.wait_if_paused(&mut epoch),
        )
        .await
        .expect(
            "wait_if_paused must not hang after a pause/resume cycle that completed \
             before the orchestrator arrived",
        );
        assert_eq!(
            epoch, 1,
            "wait_if_paused must ack the epoch advanced by the missed cycle",
        );
    }

    /// F-653: after the orchestrator has already ack'd epoch N, a
    /// no-op call to `wait_if_paused(&mut N)` returns immediately
    /// without consuming any further permits.
    #[tokio::test]
    async fn wait_if_paused_returns_immediately_when_epoch_already_observed() {
        let (_dir, session) = fresh_session().await;
        assert!(session.try_pause());
        assert!(session.try_resume());
        let mut epoch = session.pause_epoch();
        assert_eq!(epoch, 1);

        // Orchestrator has already ack'd this epoch (or initialised
        // to it). Subsequent calls must be a fast path.
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            session.wait_if_paused(&mut epoch),
        )
        .await
        .expect("ack'd epoch must be a no-op fast path");
        assert_eq!(epoch, 1, "epoch unchanged on no-op");
    }

    /// F-653: after a missed cycle is ack'd, a fresh pause that lands
    /// AFTER the ack must still park the orchestrator.
    #[tokio::test]
    async fn wait_if_paused_parks_on_fresh_pause_after_acked_cycle() {
        let (_dir, session) = fresh_session().await;
        // Missed cycle.
        assert!(session.try_pause());
        assert!(session.try_resume());

        // Orchestrator catches up.
        let session_a = Arc::clone(&session);
        let mut epoch = 0u64;
        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            session_a.wait_if_paused(&mut epoch),
        )
        .await
        .unwrap();
        assert_eq!(epoch, 1);

        // Fresh pause — orchestrator's next checkpoint must park.
        assert!(session.try_pause());

        let session_b = Arc::clone(&session);
        let waiter = tokio::spawn(async move {
            let mut e = epoch;
            session_b.wait_if_paused(&mut e).await;
            e
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !waiter.is_finished(),
            "checkpoint must park on a fresh pause that lands after a previous ack",
        );

        assert!(session.try_resume());
        let observed = tokio::time::timeout(std::time::Duration::from_millis(500), waiter)
            .await
            .expect("waiter must wake on the second resume")
            .unwrap();
        assert_eq!(observed, 2, "second cycle advances the epoch to 2");
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

    // ---------- F-666 hygiene: pause/resume event ordering under lock ----------

    #[tokio::test]
    async fn pause_and_emit_if_transitioned_emits_event_under_lock() {
        let (_dir, session) = fresh_session().await;
        let mut rx = session.event_tx.subscribe();

        assert!(session.pause_and_emit_if_transitioned().await.unwrap());
        let (_, ev) = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("event arrives")
            .expect("subscriber alive");
        assert!(
            matches!(ev, Event::SessionPaused { .. }),
            "expected SessionPaused, got {ev:?}",
        );

        // Idempotent: redundant pause must not emit a second event.
        assert!(!session.pause_and_emit_if_transitioned().await.unwrap());
        assert!(rx.try_recv().is_err(), "no second event on no-op pause");

        assert!(session.resume_and_emit_if_transitioned().await.unwrap());
        let (_, ev) = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("event arrives")
            .expect("subscriber alive");
        assert!(
            matches!(ev, Event::SessionResumed { .. }),
            "expected SessionResumed, got {ev:?}",
        );
    }

    /// F-666: hammer pause/resume from many tasks at once and assert
    /// the event stream alternates `Paused, Resumed, Paused, Resumed,
    /// …` strictly. Pre-fix, the state-flip CAS and the `emit` log-
    /// mutex acquisition were separable, so two callers could see
    /// their events arrive in the opposite order from their
    /// transitions.
    #[tokio::test]
    async fn concurrent_pause_resume_events_match_transition_order() {
        let (_dir, session) = fresh_session().await;
        let mut rx = session.event_tx.subscribe();

        // 50 cycles of pause/resume issued from independent tasks.
        // We serialize each cycle (resume must observe a prior pause)
        // but the *emission* path is contended across tasks.
        let cycles = 50;
        let mut tasks = Vec::new();
        for _ in 0..cycles {
            let s = Arc::clone(&session);
            tasks.push(tokio::spawn(async move {
                // Spin until the pause transition lands (a concurrent
                // resumer may have flipped us back to Running already).
                while !s.pause_and_emit_if_transitioned().await.unwrap() {
                    tokio::task::yield_now().await;
                }
                while !s.resume_and_emit_if_transitioned().await.unwrap() {
                    tokio::task::yield_now().await;
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }

        // Drain all 2N events; assert strict alternation.
        let mut prev_was_paused: Option<bool> = None;
        let mut paused = 0;
        let mut resumed = 0;
        for _ in 0..(cycles * 2) {
            let (_, ev) = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("event arrives")
                .expect("subscriber alive");
            match ev {
                Event::SessionPaused { .. } => {
                    assert!(
                        prev_was_paused != Some(true),
                        "two consecutive SessionPaused events — state/emit \
                         interleaved out of order",
                    );
                    prev_was_paused = Some(true);
                    paused += 1;
                }
                Event::SessionResumed { .. } => {
                    assert!(
                        prev_was_paused != Some(false),
                        "two consecutive SessionResumed events — state/emit \
                         interleaved out of order",
                    );
                    assert!(
                        prev_was_paused == Some(true),
                        "SessionResumed before any SessionPaused",
                    );
                    prev_was_paused = Some(false);
                    resumed += 1;
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(paused, cycles);
        assert_eq!(resumed, cycles);
        assert!(!session.is_paused(), "final state must be Running");
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

    // F-748: a daemon that re-spawns for the same session id must NOT
    // truncate the event log — that would lose the user's transcript
    // across a crash-restart. `Session::resume` preserves the existing
    // log and seeds `seq` from the count of persisted events so the
    // subsequent `emit` continues monotonically.
    #[tokio::test]
    async fn resume_preserves_log_and_seeds_seq() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");

        let first = Arc::new(Session::create(log_path.clone()).await.unwrap());
        first
            .emit(Event::UserMessage {
                id: "m1".to_string().into(),
                at: Utc::now(),
                text: Arc::from("hello"),
                context: Vec::new(),
                branch_parent: None,
            })
            .await
            .unwrap();
        first
            .emit(Event::UserMessage {
                id: "m2".to_string().into(),
                at: Utc::now(),
                text: Arc::from("world"),
                context: Vec::new(),
                branch_parent: None,
            })
            .await
            .unwrap();
        assert_eq!(first.current_seq().await, 2);
        // Drop the first session so its background flush + file handle
        // release; resume opens the same path.
        drop(first);

        let resumed = Session::resume(log_path.clone()).await.unwrap();
        assert_eq!(
            resumed.current_seq().await,
            2,
            "resume must seed seq from the persisted event count"
        );
        // Subsequent emit lands at seq=3, not seq=1 — proves the log
        // header was preserved (open path) and the counter continued.
        resumed
            .emit(Event::UserMessage {
                id: "m3".to_string().into(),
                at: Utc::now(),
                text: Arc::from("after-restart"),
                context: Vec::new(),
                branch_parent: None,
            })
            .await
            .unwrap();
        assert_eq!(resumed.current_seq().await, 3);

        let all = forge_core::read_since(&log_path, 0).await.unwrap();
        assert_eq!(all.len(), 3, "resume must NOT truncate the existing log");
    }

    #[tokio::test]
    async fn resume_on_missing_log_creates_fresh() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Session::resume(log_path.clone()).await.unwrap();
        assert_eq!(session.current_seq().await, 0);
        // The log file exists with the schema header but no events.
        assert!(log_path.exists(), "resume must create the log when absent");
    }

    // F-748 review fix: a daemon SIGKILL'd mid-`write_all` can leave a
    // truncated or malformed final line in the JSONL log. `Session::resume`
    // must (a) seed seq from the last well-formed event (not the count of
    // attempted lines, which would over-count corrupt rows), and (b)
    // truncate the corrupt suffix so subsequent emits land at a contiguous
    // strictly-increasing seq AND the reopened log parses cleanly under
    // the strict reader.
    #[tokio::test]
    async fn resume_recovers_from_corrupt_trailing_line() {
        use tokio::io::AsyncWriteExt;

        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");

        // Write a clean log with two well-formed events.
        let first = Arc::new(Session::create(log_path.clone()).await.unwrap());
        first
            .emit(Event::UserMessage {
                id: "m1".to_string().into(),
                at: Utc::now(),
                text: Arc::from("good-prefix-1"),
                context: Vec::new(),
                branch_parent: None,
            })
            .await
            .unwrap();
        first
            .emit(Event::UserMessage {
                id: "m2".to_string().into(),
                at: Utc::now(),
                text: Arc::from("good-prefix-2"),
                context: Vec::new(),
                branch_parent: None,
            })
            .await
            .unwrap();
        // Flush + close the first session so the file handle releases.
        drop(first);

        // Simulate a SIGKILL mid-write by appending a partial /
        // unparseable line that does not end with a newline. This is the
        // exact shape a kernel-aborted `write_all` leaves behind when
        // the buffered writer flushed an incomplete envelope.
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .await
            .unwrap();
        f.write_all(b"{\"UserMessage\":{\"id\":\"truncated")
            .await
            .unwrap();
        f.sync_all().await.unwrap();
        drop(f);
        let corrupt_len = tokio::fs::metadata(&log_path).await.unwrap().len();

        // Resume must succeed; the seq must seed at the last GOOD event
        // (2), not bump for the corrupt half-line.
        let resumed = Session::resume(log_path.clone()).await.unwrap();
        assert_eq!(
            resumed.current_seq().await,
            2,
            "resume must seed from last good event seq, not count corrupt tail"
        );

        // The file should have been truncated past the corrupt suffix.
        let trimmed_len = tokio::fs::metadata(&log_path).await.unwrap().len();
        assert!(
            trimmed_len < corrupt_len,
            "resume must truncate the corrupt tail: was {corrupt_len} bytes, now {trimmed_len}",
        );

        // Strict reader must now parse the file cleanly with exactly the
        // two pre-crash events visible.
        let visible = forge_core::read_since(&log_path, 0).await.unwrap();
        assert_eq!(visible.len(), 2);

        // Subsequent emit must produce a contiguous, strictly-increasing
        // seq — the resumed counter is at 2, so the next emit is seq 3.
        resumed
            .emit(Event::UserMessage {
                id: "m3".to_string().into(),
                at: Utc::now(),
                text: Arc::from("after-recovery"),
                context: Vec::new(),
                branch_parent: None,
            })
            .await
            .unwrap();
        assert_eq!(resumed.current_seq().await, 3);

        // The strict reader still parses cleanly after the new write
        // landed — proves the truncation left the file in a state the
        // append path can extend without re-introducing corruption.
        let after = forge_core::read_since(&log_path, 0).await.unwrap();
        assert_eq!(after.len(), 3);
        assert!(matches!(
            &after[2].1,
            Event::UserMessage { text, .. } if &**text == "after-recovery"
        ));
    }
}
