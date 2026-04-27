use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use forge_core::{Event, EventLog};
use tokio::sync::{broadcast, Mutex, Notify};

use crate::error::SessionError;

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
}
