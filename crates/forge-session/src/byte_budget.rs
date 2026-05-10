//! Per-session aggregate byte budget (F-077).
//!
//! Per-op caps in `forge-fs` (10 MiB) and `forge-providers`
//! (1 MiB / 4 MiB per-line) bound the size of a single tool invocation
//! but do not compose into a session ceiling: a tool-chained adversary
//! can issue 1000 within-cap calls and exhaust host memory without
//! tripping any per-op limit. `ByteBudget` is the missing aggregate —
//! a monotonically-increasing counter shared across every tool
//! invocation in a session, refusing further ops once the configured
//! limit is reached.
//!
//! # Semantics
//!
//! Enforcement is **post-decrement**: the dispatcher executes the tool,
//! charges the budget by the bytes the result actually consumed
//! (content / stdout / stderr), then refuses the *next* call if the
//! budget is exhausted. A single op that overshoots the cap is allowed
//! to complete — the next call is refused. This matches the brief's
//! "refuses further ops when exhausted" wording and avoids forcing
//! tools to pre-declare their output size (`shell.exec` cannot know
//! its stdout volume until the child exits).
//!
//! Refusal happens at the `ToolDispatcher` boundary so every tool routes
//! through the same gate. The tool itself never runs after exhaustion;
//! the dispatcher returns `{"error": "session byte budget exceeded:
//! <consumed>/<limit> bytes"}` directly.
//!
//! # Default
//!
//! [`ByteBudget::default`] is **500 MiB** per session. The number is
//! large enough that a normal session of fs.read / fs.write / shell.exec
//! calls never trips it, and small enough that a runaway loop cannot
//! exhaust desktop or CI memory before the daemon refuses. Tests
//! configure smaller budgets (1 MiB-class) to exercise the boundary
//! without paying the memory cost of the production default.

use std::sync::atomic::{AtomicU64, Ordering};

/// Default aggregate byte budget per session: 500 MiB. See module docs.
pub const DEFAULT_BUDGET_BYTES: u64 = 500 * 1024 * 1024;

/// Per-tool-call fixed envelope written to the event log alongside the
/// tool's actual payload bytes.
///
/// Each tool invocation emits at minimum a `ToolCallStarted` event whose
/// JSON envelope adds framing around the caller-supplied `args` payload:
/// the discriminant tag (`"type":"tool_call_started"`), the `id`, `msg`,
/// `tool`, `at`, and `parallel_group` keys plus the trailing newline the
/// event log appends. The pinning test
/// `per_tool_call_overhead_matches_serializer` (in this module's
/// `#[cfg(test)] mod tests`) measures the
/// minimum-shape envelope live: 16-byte ULID ids, a single-char tool
/// name, `args: {}`, an epoch timestamp, and `parallel_group: None`
/// serialize to ~162 bytes minus the `"args":{}` payload. The constant
/// is pinned at a conservative floor below that (so longer-id real
/// emissions never undershoot the gate) and is asserted as a
/// lower-bound by the test. Subtracting `n * overhead` from the
/// remaining budget is therefore a strict floor: live envelopes pay
/// at least this much and usually more.
///
/// The `is_exhausted_with_overhead` gate folds this in:
/// `consumed + n_tool_calls * PER_TOOL_CALL_OVERHEAD_BYTES >= limit`
/// trips refusal earlier than the naive payload-only check, preventing
/// a long-running session from out-running the budget purely through
/// tool-call framing on chatty-but-tiny tools.
pub const PER_TOOL_CALL_OVERHEAD_BYTES: u64 = 128;

/// Monotonic per-session counter of bytes consumed by tool results.
///
/// `Ordering::Relaxed` is sufficient for both the load and the
/// fetch-add: the budget enforces a *cumulative* ceiling, not
/// happens-before ordering between writes. A single op may race past
/// the limit by the size of one in-flight tool call — that is the
/// "single op overshoot" already documented in the module preamble,
/// not a correctness gap.
#[derive(Debug)]
pub struct ByteBudget {
    consumed: AtomicU64,
    limit: u64,
}

impl ByteBudget {
    /// Construct a budget with `limit` bytes of headroom.
    pub fn new(limit: u64) -> Self {
        Self {
            consumed: AtomicU64::new(0),
            limit,
        }
    }

    /// Bytes consumed so far.
    pub fn consumed(&self) -> u64 {
        self.consumed.load(Ordering::Relaxed)
    }

    /// Configured ceiling.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// True iff `consumed() >= limit()` — subsequent dispatch calls
    /// will be refused.
    pub fn is_exhausted(&self) -> bool {
        self.consumed() >= self.limit
    }

    /// Overhead-aware variant of [`Self::is_exhausted`]. Counts
    /// `n_tool_calls * PER_TOOL_CALL_OVERHEAD_BYTES` against the
    /// remaining headroom in addition to the payload bytes already
    /// charged via [`Self::charge`]. Returns `true` when the projected
    /// total — payload plus per-call envelopes — would meet or exceed
    /// the limit.
    ///
    /// Use this at the dispatch boundary when the number of in-flight
    /// or already-emitted tool calls is known; use [`Self::is_exhausted`]
    /// when only the raw payload aggregate matters.
    pub fn is_exhausted_with_overhead(&self, n_tool_calls: u64) -> bool {
        let overhead = n_tool_calls.saturating_mul(PER_TOOL_CALL_OVERHEAD_BYTES);
        self.consumed().saturating_add(overhead) >= self.limit
    }

    /// Remaining headroom after subtracting the per-tool-call envelope
    /// overhead for `n_tool_calls` already-emitted (or projected) calls.
    /// Saturates at zero — callers treat zero as "no further work is
    /// safe".
    pub fn remaining_with_overhead(&self, n_tool_calls: u64) -> u64 {
        let overhead = n_tool_calls.saturating_mul(PER_TOOL_CALL_OVERHEAD_BYTES);
        self.limit
            .saturating_sub(self.consumed())
            .saturating_sub(overhead)
    }

    /// Record `bytes` against the budget. Saturating add prevents
    /// counter wrap on a pathologically long-lived session: once
    /// the counter reaches `u64::MAX` the budget stays exhausted.
    pub fn charge(&self, bytes: u64) {
        let mut current = self.consumed.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(bytes);
            match self.consumed.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }
}

impl Default for ByteBudget {
    /// 500 MiB default budget (`DEFAULT_BUDGET_BYTES`).
    fn default() -> Self {
        Self::new(DEFAULT_BUDGET_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_at_zero_consumed() {
        let b = ByteBudget::new(1024);
        assert_eq!(b.consumed(), 0);
        assert_eq!(b.limit(), 1024);
        assert!(!b.is_exhausted());
    }

    #[test]
    fn charge_accumulates() {
        let b = ByteBudget::new(1024);
        b.charge(100);
        b.charge(200);
        assert_eq!(b.consumed(), 300);
        assert!(!b.is_exhausted());
    }

    #[test]
    fn is_exhausted_at_or_above_limit() {
        let b = ByteBudget::new(1024);
        b.charge(1024);
        assert!(b.is_exhausted());
        b.charge(1);
        assert!(b.is_exhausted());
    }

    #[test]
    fn charge_saturates_on_overflow() {
        let b = ByteBudget::new(u64::MAX);
        b.charge(u64::MAX - 10);
        b.charge(100); // would overflow without saturating_add
        assert_eq!(b.consumed(), u64::MAX);
    }

    #[test]
    fn default_is_500_mib() {
        assert_eq!(ByteBudget::default().limit(), 500 * 1024 * 1024);
        assert_eq!(DEFAULT_BUDGET_BYTES, 500 * 1024 * 1024);
    }

    /// Pin the per-tool-call envelope estimate against the live
    /// `Event::ToolCallStarted` serializer. The serialized envelope
    /// of a minimum-shape event (single-byte ids, empty `args: {}`,
    /// `parallel_group: None`) minus the `"args":{}` bytes is a
    /// concrete lower-bound on the framing every real tool call
    /// pays. The constant must be at or below that floor — over-
    /// estimating would short-circuit valid sessions, but under-
    /// estimating by a small fixed amount is fine (the gate is a
    /// conservative floor, not a tight bound).
    ///
    /// If this test fails because the event shape grew, recompute
    /// the constant from the printed `envelope_minus_args_bytes`
    /// number and update the doc comment to match.
    #[test]
    fn per_tool_call_overhead_matches_serializer() {
        use chrono::{DateTime, Utc};
        use forge_core::ids::{MessageId, ToolCallId};
        use forge_core::Event;
        use serde_json::json;

        let event = Event::ToolCallStarted {
            id: ToolCallId::new(),
            msg: MessageId::new(),
            tool: "t".to_string(),
            args: json!({}),
            at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            parallel_group: None,
        };
        let envelope = serde_json::to_vec(&event).expect("event serializes");
        let args_bytes = serde_json::to_vec(&json!({}))
            .expect("args serialize")
            .len() as u64;
        let envelope_minus_args = (envelope.len() as u64).saturating_sub(args_bytes);

        // Lower-bound the constant so a real event's framing always
        // dominates the estimate. Add a small safety margin
        // (the trailing newline the event log appends, plus any
        // tag-key drift that adds <=10 bytes).
        assert!(
            PER_TOOL_CALL_OVERHEAD_BYTES <= envelope_minus_args,
            "PER_TOOL_CALL_OVERHEAD_BYTES ({PER_TOOL_CALL_OVERHEAD_BYTES}) overshoots \
             empirical envelope ({envelope_minus_args} bytes for a minimum-shape \
             ToolCallStarted). Lower the constant.",
        );
        // And don't let the constant drift to zero — the gate would
        // silently become a no-op. Const-block assert so clippy
        // doesn't flag the constant comparison as always-true.
        const _: () = assert!(
            PER_TOOL_CALL_OVERHEAD_BYTES >= 64,
            "PER_TOOL_CALL_OVERHEAD_BYTES must remain non-trivial \
             (at least 64 bytes of framing).",
        );
    }

    #[test]
    fn is_exhausted_with_overhead_counts_tool_call_framing() {
        let b = ByteBudget::new(1_000);
        b.charge(500);
        // 500 bytes of payload + 0 tool calls of overhead = 500 ≪ 1000.
        assert!(!b.is_exhausted_with_overhead(0));
        // Now add enough tool-call envelopes to cross the line.
        // remaining headroom = 500 → div_ceil over the per-call overhead.
        let n = (1_000u64 - 500).div_ceil(PER_TOOL_CALL_OVERHEAD_BYTES);
        assert!(
            b.is_exhausted_with_overhead(n),
            "{n} envelopes ({} bytes) should saturate 500-byte headroom",
            n * PER_TOOL_CALL_OVERHEAD_BYTES,
        );
        // One fewer should not trip the gate.
        assert!(!b.is_exhausted_with_overhead(n - 1));
    }

    #[test]
    fn remaining_with_overhead_saturates_at_zero() {
        let b = ByteBudget::new(1_000);
        b.charge(900);
        assert_eq!(b.remaining_with_overhead(0), 100);
        // 1 envelope eats 180 bytes; 100 remaining → saturates at 0.
        assert_eq!(b.remaining_with_overhead(1), 0);
        assert_eq!(b.remaining_with_overhead(u64::MAX), 0);
    }
}
