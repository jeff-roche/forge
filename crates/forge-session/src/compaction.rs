//! F-598 Context compaction.
//!
//! When a session's transcript byte budget approaches the configured ceiling
//! ([`crate::byte_budget::ByteBudget`], 500 MiB by default), older turns are
//! collapsed into a single privileged-summary assistant message so the
//! provider request stays bounded.
//!
//! # Algorithm
//!
//! 1. Read the durable event log and filter through [`apply_superseded`]
//!    to drop hidden branches / superseded re-runs.
//! 2. Group the filtered stream into *turns* — each turn is the contiguous
//!    sequence beginning at a [`Event::UserMessage`] and running up to (but
//!    not including) the next `UserMessage`. Pre-`UserMessage` events
//!    (e.g. `SessionStarted`) form an implicit "turn 0" and are never
//!    selected for compaction.
//! 3. Walk turns in chronological order, skipping pinned ones, accumulating
//!    bytes (UTF-8 length of each event's JSON encoding) until the running
//!    total hits `target_bytes` (~50% of total transcript bytes by default).
//! 4. Ask the active provider to summarize the selected text, then emit a
//!    single [`Event::AssistantMessage`] tagged as a summary.
//! 5. Emit [`Event::ContextCompacted`] *after* the summary message so
//!    consumers observe the replacement before the marker.
//!
//! # Constraints
//!
//! - The summary call MUST NOT recursively trigger compaction. The
//!   `ReleaseCompactingOnDrop` guard plus [`Session::is_compacting`] are
//!   the runtime check; the orchestrator's auto-trigger consults it
//!   before invoking [`compact`].
//! - Pinned turns are never selected, regardless of position or size.
//! - The active provider — whichever one the session was constructed
//!   with — handles the summary call. There is no provider fallback.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::Utc;
use forge_core::{
    apply_superseded,
    ids::{MessageId, ProviderId},
    read_since, CompactTrigger, Event,
};
use forge_providers::{ChatBlock, ChatChunk, ChatMessage, ChatRequest, ChatRole, Provider};
use futures::StreamExt;

use crate::session::Session;

/// Default fraction of total transcript bytes to compact away on each pass.
///
/// 0.5 matches the spec's "~50%" target. A smaller fraction would compact
/// too eagerly and lose detail; a larger one would leave too little
/// foreground context.
pub const DEFAULT_COMPACT_FRACTION: f64 = 0.5;

/// Threshold (as a fraction of [`ByteBudget::limit`](crate::byte_budget::ByteBudget::limit))
/// at which the orchestrator auto-triggers compaction.
pub const AUTO_COMPACT_THRESHOLD: f64 = 0.98;

/// F-657: hard ceiling on the summary text [`compact`] will accept from the
/// privileged provider call. Streams that would produce more than this are
/// truncated at a UTF-8 char boundary and accompanied by an
/// [`Event::CompactionTruncated`] marker.
///
/// 64 KiB matches the `MEMORY_WRITE_CONTENT_CAP` precedent
/// (F-650, in `forge-agents`): a model-emitted blob meant to live in the recurring system
/// prompt should never approach this size. A pathological or hostile
/// summary provider that emits megabytes of text would otherwise push the
/// post-compaction transcript back over the auto-trigger threshold and
/// re-enter compaction on the next turn — a feedback loop that may not
/// terminate. Capping below the freed-bytes target (50% of total
/// transcript) closes that loop.
pub const SUMMARY_CAP_BYTES: usize = 64 * 1024;

/// System prompt used for the privileged summary call.
///
/// Kept terse so the provider has the maximum budget for the actual
/// summary content. The phrase is stable across providers — every chat
/// model tested handles it identically.
pub const SUMMARY_SYSTEM_PROMPT: &str =
    "You are summarizing a transcript of an autonomous agent session. \
     Preserve key decisions, code paths, file paths, and error patterns. \
     Return one paragraph. Do not add preamble or commentary.";

/// User-side framing for the summary call. The transcript text sits between
/// the framing prefix and a closing tag so a model that accidentally
/// continues the transcript instead of summarizing it surfaces obviously.
const SUMMARY_USER_PREFIX: &str = "Summarize the following transcript:\n\n<transcript>\n";
const SUMMARY_USER_SUFFIX: &str = "\n</transcript>";

/// One logical turn in the transcript.
///
/// `start_idx` / `end_idx` are inclusive indices into the filtered
/// `(seq, Event)` slice the [`select_oldest_unpinned_turns`] caller passed in.
/// `bytes` is the cumulative UTF-8 length of every event in the span when
/// serialized as JSON. `user_msg_id` is the id of the originating
/// `UserMessage`; pre-`UserMessage` prelude events form a synthetic turn
/// with `user_msg_id = None` that callers refuse to select.
#[derive(Debug, Clone)]
pub struct Turn {
    pub user_msg_id: Option<MessageId>,
    pub start_idx: usize,
    pub end_idx: usize,
    pub bytes: u64,
}

impl Turn {
    /// Render the turn into a plain-text excerpt suitable for embedding in
    /// the privileged summary prompt.
    ///
    /// We deliberately serialize only `UserMessage` and finalised
    /// `AssistantMessage` text. Tool calls, deltas, and lifecycle events
    /// are skipped — the model does not need wire-format chatter to write
    /// a human-readable summary, and including it would waste budget.
    pub fn render_text(&self, history: &[(u64, Event)]) -> String {
        let mut out = String::new();
        for (_, ev) in &history[self.start_idx..=self.end_idx] {
            match ev {
                Event::UserMessage { text, .. } => {
                    if !out.is_empty() {
                        out.push_str("\n\n");
                    }
                    out.push_str("user: ");
                    out.push_str(text);
                }
                Event::AssistantMessage {
                    text,
                    stream_finalised: true,
                    ..
                } => {
                    if !out.is_empty() {
                        out.push_str("\n\n");
                    }
                    out.push_str("assistant: ");
                    out.push_str(text);
                }
                _ => {}
            }
        }
        out
    }
}

/// Group a filtered `(seq, Event)` history into [`Turn`]s.
///
/// A turn opens on each [`Event::UserMessage`]; pre-`UserMessage` prelude
/// events (e.g. [`Event::SessionStarted`]) collapse into a synthetic turn
/// whose `user_msg_id` is `None`. Selection helpers refuse to pick the
/// synthetic turn so prelude metadata always survives compaction.
pub fn group_into_turns(history: &[(u64, Event)]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    let mut current: Option<Turn> = None;

    for (idx, (_, ev)) in history.iter().enumerate() {
        let event_bytes = serde_json::to_string(ev)
            .map(|s| s.len() as u64)
            .unwrap_or(0);
        match ev {
            Event::UserMessage { id, .. } => {
                if let Some(t) = current.take() {
                    turns.push(t);
                }
                current = Some(Turn {
                    user_msg_id: Some(id.clone()),
                    start_idx: idx,
                    end_idx: idx,
                    bytes: event_bytes,
                });
            }
            _ => match current.as_mut() {
                Some(t) => {
                    t.end_idx = idx;
                    t.bytes = t.bytes.saturating_add(event_bytes);
                }
                None => {
                    // Prelude events before the first UserMessage. Bucket
                    // them into a synthetic turn that selection refuses.
                    let prelude = current.get_or_insert(Turn {
                        user_msg_id: None,
                        start_idx: idx,
                        end_idx: idx,
                        bytes: 0,
                    });
                    prelude.end_idx = idx;
                    prelude.bytes = prelude.bytes.saturating_add(event_bytes);
                }
            },
        }
    }

    if let Some(t) = current {
        turns.push(t);
    }

    turns
}

/// Select the oldest non-pinned turns whose cumulative byte count meets or
/// exceeds `target_bytes`.
///
/// Returns the originating `UserMessage` ids, in chronological order.
///
/// Edge cases (covered by the unit tests below):
/// - all turns pinned → empty selection;
/// - single huge turn ≥ `target_bytes` → that one turn;
/// - empty history → empty selection;
/// - prelude-only history → empty selection (no `UserMessage`-anchored turn);
/// - cumulative overshoot — once the running total *meets* the target,
///   selection stops (we never include a fourth turn just because the third
///   landed slightly under).
pub fn select_oldest_unpinned_turns(
    turns: &[Turn],
    target_bytes: u64,
    pinned: &HashSet<MessageId>,
) -> Vec<MessageId> {
    if target_bytes == 0 {
        return Vec::new();
    }
    let mut selected: Vec<MessageId> = Vec::new();
    let mut acc: u64 = 0;
    for turn in turns {
        let Some(id) = turn.user_msg_id.as_ref() else {
            continue; // synthetic prelude
        };
        if pinned.contains(id) {
            continue;
        }
        selected.push(id.clone());
        acc = acc.saturating_add(turn.bytes);
        if acc >= target_bytes {
            break;
        }
    }
    selected
}

/// Build the privileged `ChatRequest` that asks the active provider to
/// summarize a transcript excerpt.
///
/// The shape is intentionally minimal: a fixed system prompt, one
/// user-role message containing the joined transcript text. No tool
/// definitions are advertised — the summary call is read-only and must
/// not call any tool.
pub fn build_summary_request(transcript_text: &str) -> ChatRequest {
    let body = format!("{SUMMARY_USER_PREFIX}{transcript_text}{SUMMARY_USER_SUFFIX}");
    ChatRequest {
        system: Some(Arc::from(SUMMARY_SYSTEM_PROMPT)),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ChatBlock::Text(body)],
        }],
        parallel_tool_calls_allowed: false,
    }
}

/// Outcome of a successful [`compact`] call.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub summary_msg_id: MessageId,
    pub summarized_turns: Vec<MessageId>,
    pub trigger: CompactTrigger,
}

/// Releases the per-session compaction guard on drop. Paired with the
/// `try_claim_compacting()` call at the top of [`compact`]; on early
/// return, panic, or normal completion the flag is cleared.
struct ReleaseCompactingOnDrop {
    session: Arc<Session>,
}

impl Drop for ReleaseCompactingOnDrop {
    fn drop(&mut self) {
        self.session.release_compacting();
    }
}

/// Run one compaction pass against `session`.
///
/// 1. Read the on-disk event log and filter through [`apply_superseded`].
/// 2. Group into turns and select the oldest non-pinned ones until ~50% of
///    total bytes (or `min_select_bytes`, whichever is larger) are queued.
/// 3. Build a privileged summary `ChatRequest` and stream it through the
///    active provider, draining text deltas.
/// 4. Emit a synthetic [`Event::AssistantMessage`] holding the summary text
///    (`branch_parent: None`, `branch_variant_index: 0`).
/// 5. Emit [`Event::ContextCompacted`] referring back to the summary.
///
/// `provider_id` and `model` are stamped onto the synthetic
/// `AssistantMessage` so a future Agent Monitor / replay correlation
/// keys the summary against the same provider+model the session is
/// active on, instead of an orphan id. Callers must pass the values the
/// session uses for live turns; the orchestrator's auto-trigger and the
/// IPC server both do so.
///
/// Returns the new summary message id and the user-message ids of the
/// summarised turns. The function is idempotent in the sense that a second
/// call without intervening writes selects the same turns again — callers
/// (typically the orchestrator's auto-trigger) gate on
/// `summarizing_in_flight` to avoid that.
#[allow(clippy::too_many_arguments)]
pub async fn compact<P: Provider>(
    session: Arc<Session>,
    provider: Arc<P>,
    provider_id: ProviderId,
    model: String,
    fraction: f64,
    pinned: &HashSet<MessageId>,
    trigger: CompactTrigger,
    // F-744: auth seam threaded from the orchestrator so the privileged
    // summary call uses the same per-turn credential as the live turn.
    // `ProviderAuth::None` preserves the pre-F-744 behaviour for keyless
    // providers (Ollama) and for callers that don't wire the seam
    // (existing unit tests, embedders).
    auth: forge_providers::ProviderAuth,
) -> Result<CompactionResult> {
    // Refuse to recurse: the privileged summary call below could in
    // principle drive the byte budget further, but auto-triggering must
    // already gate on [`Session::is_compacting`]. This second-line check
    // closes any path where a future caller forgets the gate.
    if !session.try_claim_compacting() {
        return Err(anyhow!("compact: another compaction is already in flight"));
    }
    let _guard = ReleaseCompactingOnDrop {
        session: Arc::clone(&session),
    };

    let history = read_since(&session.log_path, 0)
        .await
        .map_err(|e| anyhow!("compact: read event log: {e}"))?;
    let history = apply_superseded(history);

    let turns = group_into_turns(&history);
    let total_bytes: u64 = turns.iter().map(|t| t.bytes).sum();
    if total_bytes == 0 || turns.is_empty() {
        return Err(anyhow!("compact: nothing to compact (empty transcript)"));
    }
    let target_bytes = ((total_bytes as f64) * fraction).ceil() as u64;
    let selected = select_oldest_unpinned_turns(&turns, target_bytes, pinned);
    if selected.is_empty() {
        return Err(anyhow!(
            "compact: no eligible (non-pinned) turns to summarize"
        ));
    }

    // Render the selected turns into a single transcript excerpt.
    let selected_set: HashSet<&MessageId> = selected.iter().collect();
    let mut excerpt = String::new();
    for turn in &turns {
        let Some(id) = turn.user_msg_id.as_ref() else {
            continue;
        };
        if !selected_set.contains(id) {
            continue;
        }
        if !excerpt.is_empty() {
            excerpt.push_str("\n\n");
        }
        excerpt.push_str(&turn.render_text(&history));
    }

    // Drive the privileged summary call. The summary text accumulates from
    // text-delta chunks until the stream completes; tool-call chunks are
    // ignored because the request advertises no tools (any provider that
    // emits one is misbehaving and we treat it as a soft failure that
    // still produces whatever text accumulated so far).
    //
    // F-657: hard byte cap on the accumulated summary. Once we cross
    // `SUMMARY_CAP_BYTES`, drop further deltas and continue draining so
    // the upstream stream completes cleanly (otherwise the provider task
    // can be left holding a half-read response). `accumulated_bytes` is
    // the unfiltered post-cap total — used to tell the caller how much was
    // discarded via the [`Event::CompactionTruncated`] marker emitted below.
    let req = build_summary_request(&excerpt);
    // F-744: route through the auth seam so the privileged summary call
    // uses the per-turn credential supplied by the orchestrator.
    let mut stream = provider.chat_with_auth(req, auth).await?;
    let mut summary_text = String::new();
    let mut truncated = false;
    let mut accumulated_bytes: u64 = 0;
    while let Some(chunk) = stream.next().await {
        match chunk {
            ChatChunk::TextDelta(d) => {
                accumulated_bytes = accumulated_bytes.saturating_add(d.len() as u64);
                if truncated {
                    continue;
                }
                if summary_text.len().saturating_add(d.len()) <= SUMMARY_CAP_BYTES {
                    summary_text.push_str(&d);
                } else {
                    // Take only enough of this delta to land exactly at
                    // (or just under) the cap, sliced on a UTF-8 char
                    // boundary so the accumulated string stays valid.
                    let remaining = SUMMARY_CAP_BYTES.saturating_sub(summary_text.len());
                    let mut take = remaining.min(d.len());
                    while take > 0 && !d.is_char_boundary(take) {
                        take -= 1;
                    }
                    summary_text.push_str(&d[..take]);
                    truncated = true;
                }
            }
            ChatChunk::Done(_) => break,
            ChatChunk::ToolCall { .. } => {
                // Privileged summary call must not invoke tools — ignore.
            }
            ChatChunk::Error { kind, message } => {
                return Err(anyhow!(
                    "compact: provider stream aborted ({kind:?}): {message}"
                ));
            }
        }
    }
    if summary_text.is_empty() {
        return Err(anyhow!("compact: provider returned an empty summary"));
    }

    // Compute the wire count up-front. The wire shape pins `u32`; refuse
    // to silently truncate if a future caller somehow stages more than
    // 4B turns. Doing this before the AssistantMessage emit means we
    // never publish a summary message we can't correlate via marker.
    let summarized_turns_count: u32 = selected.len().try_into().map_err(|_| {
        anyhow!(
            "compact: summarized_turns count {} exceeds u32::MAX",
            selected.len()
        )
    })?;

    // Emit the summary message FIRST so consumers observing the
    // ContextCompacted event can already resolve `summary_msg_id`. The
    // provider/model fields match the session's active provider — see
    // the function-level doc-comment.
    let summary_msg_id = MessageId::new();
    session
        .emit(Event::AssistantMessage {
            id: summary_msg_id.clone(),
            provider: provider_id,
            model,
            at: Utc::now(),
            stream_finalised: true,
            text: Arc::from(summary_text.as_str()),
            branch_parent: None,
            branch_variant_index: 0,
        })
        .await?;

    // Emit ContextCompacted AFTER the summary — consumers rely on the
    // ordering (the marker references `summary_msg_id`).
    session
        .emit(Event::ContextCompacted {
            at: Utc::now(),
            summarized_turns: summarized_turns_count,
            summary_msg_id: summary_msg_id.clone(),
            trigger: trigger.clone(),
        })
        .await?;

    // F-657: surface the cap hit AFTER the marker so observability
    // consumers can correlate the truncation back to the same
    // `summary_msg_id` they already saw on the `ContextCompacted`. We
    // also log here so a runaway summary provider is visible in daemon
    // logs without needing an event subscriber.
    if truncated {
        tracing::warn!(
            target: "forge_session::compaction",
            summary_msg_id = %summary_msg_id,
            cap_bytes = SUMMARY_CAP_BYTES,
            original_bytes = accumulated_bytes,
            "compaction summary truncated at byte cap",
        );
        session
            .emit(Event::CompactionTruncated {
                at: Utc::now(),
                summary_msg_id: summary_msg_id.clone(),
                cap_bytes: SUMMARY_CAP_BYTES as u64,
                original_bytes: accumulated_bytes,
            })
            .await?;
    }

    Ok(CompactionResult {
        summary_msg_id,
        summarized_turns: selected,
        trigger,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use forge_providers::MockProvider;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn user_event(id: &MessageId, text: &str) -> Event {
        Event::UserMessage {
            id: id.clone(),
            at: Utc::now(),
            text: Arc::from(text),
            context: vec![],
            branch_parent: None,
        }
    }

    fn assistant_event(id: &MessageId, text: &str) -> Event {
        Event::AssistantMessage {
            id: id.clone(),
            provider: ProviderId::new(),
            model: "mock".into(),
            at: Utc::now(),
            stream_finalised: true,
            text: Arc::from(text),
            branch_parent: None,
            branch_variant_index: 0,
        }
    }

    #[test]
    fn group_into_turns_groups_user_then_assistant() {
        let u1 = MessageId::new();
        let a1 = MessageId::new();
        let u2 = MessageId::new();
        let a2 = MessageId::new();
        let history = vec![
            (1, user_event(&u1, "hello")),
            (2, assistant_event(&a1, "hi")),
            (3, user_event(&u2, "again")),
            (4, assistant_event(&a2, "still here")),
        ];
        let turns = group_into_turns(&history);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].user_msg_id.as_ref(), Some(&u1));
        assert_eq!(turns[0].start_idx, 0);
        assert_eq!(turns[0].end_idx, 1);
        assert_eq!(turns[1].user_msg_id.as_ref(), Some(&u2));
        assert_eq!(turns[1].start_idx, 2);
        assert_eq!(turns[1].end_idx, 3);
        assert!(turns[0].bytes > 0);
    }

    #[test]
    fn group_into_turns_buckets_prelude_into_synthetic_turn() {
        // Prelude events (no leading UserMessage) collapse into a synthetic
        // turn with `user_msg_id == None` that selection refuses.
        let u1 = MessageId::new();
        let history = vec![
            (
                1,
                Event::SessionStarted {
                    at: Utc::now(),
                    workspace: std::path::PathBuf::from("/tmp"),
                    agent: None,
                    persistence: forge_core::SessionPersistence::Persist,
                },
            ),
            (2, user_event(&u1, "first")),
        ];
        let turns = group_into_turns(&history);
        assert_eq!(turns.len(), 2);
        assert!(turns[0].user_msg_id.is_none(), "prelude turn is synthetic");
        assert_eq!(turns[1].user_msg_id.as_ref(), Some(&u1));
    }

    #[test]
    fn select_returns_empty_when_all_turns_pinned() {
        let u1 = MessageId::new();
        let u2 = MessageId::new();
        let turns = vec![
            Turn {
                user_msg_id: Some(u1.clone()),
                start_idx: 0,
                end_idx: 0,
                bytes: 100,
            },
            Turn {
                user_msg_id: Some(u2.clone()),
                start_idx: 1,
                end_idx: 1,
                bytes: 100,
            },
        ];
        let pinned: HashSet<MessageId> = [u1, u2].into_iter().collect();
        let selected = select_oldest_unpinned_turns(&turns, 100, &pinned);
        assert!(selected.is_empty(), "all pinned → nothing selected");
    }

    #[test]
    fn select_returns_single_huge_turn_when_meets_target() {
        let u1 = MessageId::new();
        let turns = vec![Turn {
            user_msg_id: Some(u1.clone()),
            start_idx: 0,
            end_idx: 0,
            bytes: 10_000,
        }];
        let selected = select_oldest_unpinned_turns(&turns, 5_000, &HashSet::new());
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0], u1);
    }

    #[test]
    fn select_returns_empty_for_empty_turns() {
        let selected = select_oldest_unpinned_turns(&[], 1_000, &HashSet::new());
        assert!(selected.is_empty());
    }

    #[test]
    fn select_skips_synthetic_prelude_turn() {
        let u1 = MessageId::new();
        let turns = vec![
            Turn {
                user_msg_id: None,
                start_idx: 0,
                end_idx: 0,
                bytes: 50,
            },
            Turn {
                user_msg_id: Some(u1.clone()),
                start_idx: 1,
                end_idx: 1,
                bytes: 100,
            },
        ];
        let selected = select_oldest_unpinned_turns(&turns, 100, &HashSet::new());
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0], u1);
    }

    #[test]
    fn select_stops_once_target_met() {
        // Three 100-byte turns, target 150 → first two cover it.
        let u1 = MessageId::new();
        let u2 = MessageId::new();
        let u3 = MessageId::new();
        let turns = vec![
            Turn {
                user_msg_id: Some(u1.clone()),
                start_idx: 0,
                end_idx: 0,
                bytes: 100,
            },
            Turn {
                user_msg_id: Some(u2.clone()),
                start_idx: 1,
                end_idx: 1,
                bytes: 100,
            },
            Turn {
                user_msg_id: Some(u3.clone()),
                start_idx: 2,
                end_idx: 2,
                bytes: 100,
            },
        ];
        let selected = select_oldest_unpinned_turns(&turns, 150, &HashSet::new());
        assert_eq!(selected, vec![u1, u2]);
    }

    #[test]
    fn select_skips_pinned_in_chronological_order() {
        // Pin the middle turn; selection must reach across it without
        // including it.
        let u1 = MessageId::new();
        let u2 = MessageId::new();
        let u3 = MessageId::new();
        let turns = vec![
            Turn {
                user_msg_id: Some(u1.clone()),
                start_idx: 0,
                end_idx: 0,
                bytes: 100,
            },
            Turn {
                user_msg_id: Some(u2.clone()),
                start_idx: 1,
                end_idx: 1,
                bytes: 100,
            },
            Turn {
                user_msg_id: Some(u3.clone()),
                start_idx: 2,
                end_idx: 2,
                bytes: 100,
            },
        ];
        let pinned: HashSet<MessageId> = [u2.clone()].into_iter().collect();
        let selected = select_oldest_unpinned_turns(&turns, 150, &pinned);
        assert_eq!(selected, vec![u1, u3]);
    }

    #[test]
    fn build_summary_request_has_system_prompt_and_one_user_message() {
        let req = build_summary_request("hello world");
        assert!(req.system.is_some());
        assert!(req.system.as_deref().unwrap().contains("summarizing"));
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, ChatRole::User);
        // Must not advertise parallel tool calls (and it's a no-tool call
        // anyway — the assertion guards against accidental defaults flipping).
        assert!(!req.parallel_tool_calls_allowed);
    }

    #[test]
    fn build_summary_request_wraps_transcript_in_tags() {
        let req = build_summary_request("the body");
        let body = match &req.messages[0].content[0] {
            ChatBlock::Text(s) => s.clone(),
            _ => panic!("expected text block"),
        };
        assert!(body.contains("<transcript>"));
        assert!(body.contains("</transcript>"));
        assert!(body.contains("the body"));
    }

    #[tokio::test]
    async fn compact_replaces_oldest_turns_and_emits_marker_after_summary() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());

        // Seed three full turns.
        let u1 = MessageId::new();
        let a1 = MessageId::new();
        let u2 = MessageId::new();
        let a2 = MessageId::new();
        let u3 = MessageId::new();
        let a3 = MessageId::new();
        for ev in [
            user_event(&u1, "first question"),
            assistant_event(&a1, "first answer"),
            user_event(&u2, "second question"),
            assistant_event(&a2, "second answer"),
            user_event(&u3, "third question"),
            assistant_event(&a3, "third answer"),
        ] {
            session.emit(ev).await.unwrap();
        }

        // Summary script: one delta + done.
        let script = "{\"delta\":\"summary text\"}\n{\"done\":\"end_turn\"}\n".to_string();
        let provider = Arc::new(MockProvider::from_responses(vec![script]).unwrap());

        // Subscribe BEFORE compact() so we observe the order of the live
        // emissions deterministically.
        let mut rx = session.event_tx.subscribe();

        // Pin a deterministic provider id so the assertion below can verify
        // the summary AssistantMessage carries the caller-supplied value
        // rather than a fresh `ProviderId::new()` (B3 regression guard).
        let provider_id = ProviderId::new();
        let res = compact(
            Arc::clone(&session),
            Arc::clone(&provider),
            provider_id.clone(),
            "active-model".to_string(),
            DEFAULT_COMPACT_FRACTION,
            &HashSet::new(),
            CompactTrigger::AutoAt98Pct,
            forge_providers::ProviderAuth::None,
        )
        .await
        .unwrap();

        // Drain the emissions: AssistantMessage(summary) THEN ContextCompacted,
        // in that order. Anything before the AssistantMessage must not be
        // a ContextCompacted marker.
        let mut saw_summary = false;
        let mut saw_marker = false;
        while let Ok((_, ev)) = rx.try_recv() {
            match ev {
                Event::AssistantMessage {
                    id,
                    text,
                    provider,
                    model,
                    ..
                } if id == res.summary_msg_id => {
                    assert!(!saw_marker, "marker must follow summary, not precede it");
                    assert!(text.contains("summary text"));
                    assert_eq!(
                        provider, provider_id,
                        "summary message must carry the caller-supplied provider id"
                    );
                    assert_eq!(
                        model, "active-model",
                        "summary message must carry the caller-supplied model"
                    );
                    saw_summary = true;
                }
                Event::ContextCompacted {
                    summary_msg_id,
                    summarized_turns,
                    trigger,
                    ..
                } => {
                    assert!(saw_summary, "summary message must arrive before marker");
                    assert_eq!(summary_msg_id, res.summary_msg_id);
                    assert!(summarized_turns >= 1);
                    assert_eq!(trigger, CompactTrigger::AutoAt98Pct);
                    saw_marker = true;
                }
                _ => {}
            }
        }
        assert!(saw_summary && saw_marker, "both events must fire");
        assert!(!res.summarized_turns.is_empty());
    }

    #[tokio::test]
    async fn compact_refuses_when_all_turns_pinned() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());

        let u1 = MessageId::new();
        session.emit(user_event(&u1, "x")).await.unwrap();
        session
            .emit(assistant_event(&MessageId::new(), "y"))
            .await
            .unwrap();

        let provider = Arc::new(
            MockProvider::from_responses(vec!["{\"done\":\"end_turn\"}\n".into()]).unwrap(),
        );
        let pinned: HashSet<MessageId> = [u1].into_iter().collect();

        let err = compact(
            session,
            provider,
            ProviderId::new(),
            "active-model".to_string(),
            DEFAULT_COMPACT_FRACTION,
            &pinned,
            CompactTrigger::UserRequested,
            forge_providers::ProviderAuth::None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("non-pinned"));
    }

    #[tokio::test]
    async fn compact_truncates_oversized_summary_and_emits_marker() {
        // F-657: a pathological provider emitting a summary larger than the
        // configured cap must be truncated in place, must emit a
        // `CompactionTruncated` event, and the resulting summary message
        // must be at most `SUMMARY_CAP_BYTES` bytes — small enough that a
        // freshly seeded transcript would not immediately re-trigger
        // compaction on the next turn.
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());

        // Seed three full turns so compaction has enough to chew on.
        for i in 0..3 {
            session
                .emit(user_event(&MessageId::new(), &format!("q{i}")))
                .await
                .unwrap();
            session
                .emit(assistant_event(&MessageId::new(), &format!("a{i}")))
                .await
                .unwrap();
        }

        // Build a script whose single text delta is twice the cap. Use a
        // multi-byte char ('ß' = 2 bytes UTF-8) to force the truncation
        // logic to land on a char boundary rather than mid-codepoint.
        let oversized = "ß".repeat(SUMMARY_CAP_BYTES); // 2 * cap bytes
        let oversized_len = oversized.len() as u64;
        let script = format!(
            "{{\"delta\":{}}}\n{{\"done\":\"end_turn\"}}\n",
            serde_json::to_string(&oversized).unwrap()
        );
        let provider = Arc::new(MockProvider::from_responses(vec![script]).unwrap());

        let mut rx = session.event_tx.subscribe();

        let res = compact(
            Arc::clone(&session),
            Arc::clone(&provider),
            ProviderId::new(),
            "active-model".to_string(),
            DEFAULT_COMPACT_FRACTION,
            &HashSet::new(),
            CompactTrigger::AutoAt98Pct,
            forge_providers::ProviderAuth::None,
        )
        .await
        .unwrap();

        // Drain emissions and capture the relevant ones.
        let mut summary_text_bytes: usize = 0;
        let mut saw_marker = false;
        let mut saw_truncated_event = false;
        let mut truncated_cap: u64 = 0;
        let mut truncated_original: u64 = 0;
        while let Ok((_, ev)) = rx.try_recv() {
            match ev {
                Event::AssistantMessage { id, text, .. } if id == res.summary_msg_id => {
                    summary_text_bytes = text.len();
                }
                Event::ContextCompacted { summary_msg_id, .. } => {
                    assert_eq!(summary_msg_id, res.summary_msg_id);
                    saw_marker = true;
                }
                Event::CompactionTruncated {
                    summary_msg_id,
                    cap_bytes,
                    original_bytes,
                    ..
                } => {
                    assert_eq!(summary_msg_id, res.summary_msg_id);
                    truncated_cap = cap_bytes;
                    truncated_original = original_bytes;
                    saw_truncated_event = true;
                }
                _ => {}
            }
        }

        assert!(
            saw_marker,
            "ContextCompacted must still fire when truncated"
        );
        assert!(
            saw_truncated_event,
            "CompactionTruncated event must fire when the cap is hit"
        );
        assert!(
            summary_text_bytes > 0 && summary_text_bytes <= SUMMARY_CAP_BYTES,
            "summary text must be capped: {summary_text_bytes} > {SUMMARY_CAP_BYTES}",
        );
        assert_eq!(truncated_cap, SUMMARY_CAP_BYTES as u64);
        assert!(
            truncated_original >= oversized_len,
            "original_bytes ({truncated_original}) should reflect the unfiltered \
             stream length (>= {oversized_len})"
        );
    }

    #[tokio::test]
    async fn capped_summary_byte_size_is_bounded_regardless_of_provider_output() {
        // F-657 regression: the *amplification* mechanism the bug describes
        // is "summary larger than the freed bytes re-triggers compaction".
        // The defensive property we lock down here is the upper bound on
        // the summary AssistantMessage's text length: capped summary text
        // length is `<= SUMMARY_CAP_BYTES`, no matter how big the provider
        // stream gets. Without that cap, this test would observe a summary
        // text size proportional to whatever the (hostile) provider sent.
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());

        for i in 0..3 {
            session
                .emit(user_event(&MessageId::new(), &format!("q{i}")))
                .await
                .unwrap();
            session
                .emit(assistant_event(&MessageId::new(), &format!("a{i}")))
                .await
                .unwrap();
        }

        // Stream 4× the cap as a single delta — guarantees the truncation
        // path runs and that "without the cap" the summary would be 4×
        // larger than the cap.
        let oversized_text = "z".repeat(SUMMARY_CAP_BYTES * 4);
        let script = format!(
            "{{\"delta\":{}}}\n{{\"done\":\"end_turn\"}}\n",
            serde_json::to_string(&oversized_text).unwrap()
        );
        let provider = Arc::new(MockProvider::from_responses(vec![script]).unwrap());

        let mut rx = session.event_tx.subscribe();
        let res = compact(
            Arc::clone(&session),
            Arc::clone(&provider),
            ProviderId::new(),
            "active-model".to_string(),
            DEFAULT_COMPACT_FRACTION,
            &HashSet::new(),
            CompactTrigger::AutoAt98Pct,
            forge_providers::ProviderAuth::None,
        )
        .await
        .unwrap();

        let mut summary_bytes = 0usize;
        while let Ok((_, ev)) = rx.try_recv() {
            if let Event::AssistantMessage { id, text, .. } = ev {
                if id == res.summary_msg_id {
                    summary_bytes = text.len();
                }
            }
        }

        assert!(
            summary_bytes > 0,
            "summary must contain at least the partial truncated content"
        );
        assert!(
            summary_bytes <= SUMMARY_CAP_BYTES,
            "capped summary text ({summary_bytes}B) must not exceed cap ({SUMMARY_CAP_BYTES}B); \
             without F-657 the provider's {oversized}B stream would land verbatim",
            oversized = oversized_text.len(),
        );
    }

    #[tokio::test]
    async fn compact_refuses_on_empty_transcript() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());

        let provider = Arc::new(
            MockProvider::from_responses(vec!["{\"done\":\"end_turn\"}\n".into()]).unwrap(),
        );
        let err = compact(
            session,
            provider,
            ProviderId::new(),
            "active-model".to_string(),
            DEFAULT_COMPACT_FRACTION,
            &HashSet::new(),
            CompactTrigger::UserRequested,
            forge_providers::ProviderAuth::None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("empty"));
    }
}
