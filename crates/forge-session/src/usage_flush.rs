//! Session-end usage-aggregate flush (F-593).
//!
//! Walks a finished session's event log, picks out every [`Event::UsageTick`],
//! computes its cost via [`forge_providers::pricing`], and merges the result
//! into the monthly aggregate at
//! `<usage_dir>/<YYYY-MM>.json`.
//!
//! ## Why "read the log" instead of accumulating in memory
//!
//! The session's event log is the durable source of truth — re-reading it on
//! flush means a daemon crash mid-session loses *no* usage that was already
//! durably appended to the log, and the function is naturally idempotent
//! across restart-then-flush sequences (we always end up with the same
//! aggregate). An in-memory accumulator would have to be persisted itself to
//! survive a crash, duplicating the log's job.
//!
//! ## Cost-table policy
//!
//! Every UsageTick is repriced at flush time using the embedded price table
//! rather than trusting the event's `cost_usd` field. The event field carries
//! whatever the provider reported at emission; the price table is the
//! authoritative single source for our reported cost. A model with no row
//! flushes as `cost: null` — the spec's "missing model surfaced as null cost."
//!
//! ## Atomicity
//!
//! [`MonthlyAggregate::flush`] writes via tmp+rename ([`forge_core::config_file`]).
//! A crash mid-flush leaves either the previous month-file intact or the new
//! one — never a torn partial.

use std::path::Path;

use chrono::{DateTime, Datelike, Utc};
use forge_core::usage::{
    monthly_path_in, user_usage_dir, MonthlyAggregate, SessionUsage, UsageBucket,
};
use forge_core::{read_since, Event, Result, SessionId, WorkspaceId};
use forge_providers::pricing::PriceTable;

/// Read every [`Event::UsageTick`] in `log_path` and merge it into the
/// monthly aggregate(s) under `usage_dir`.
///
/// `workspace_id` tags every appended bucket so the `usage_summary` query can
/// later filter to a single workspace.
///
/// ## Idempotency
///
/// A sentinel file `<log_path>.usage-flushed` is written when the merge
/// completes successfully. If the sentinel already exists this function
/// returns `Ok(0)` without rereading the log — so calling `flush` from both
/// the session-end hook AND an app-shutdown defense-in-depth hook (per the
/// F-593 DoD) double-flushes safely without corrupting the aggregate.
/// The sentinel lives alongside the log so it shares its lifetime: archive
/// or purge of the session dir takes the sentinel with it.
///
/// Returns the number of UsageTick events flushed. A log with zero ticks is
/// a no-op (no file is written, no sentinel is written either — the next
/// flush attempt on a re-emitting session is still allowed).
pub async fn flush_session_usage(
    log_path: &Path,
    workspace_id: &WorkspaceId,
    usage_dir: &Path,
) -> Result<usize> {
    let sentinel = sentinel_path(log_path);
    if tokio::fs::metadata(&sentinel).await.is_ok() {
        // Already flushed — second call is a deliberate no-op so the
        // orchestrator hook + shell shutdown hook can both fire safely.
        return Ok(0);
    }

    let events = read_since(log_path, 0).await?;
    let table = PriceTable::embedded();

    // Per-month accumulators, keyed by `YYYY-MM`. We load the existing file
    // once per month, fold every tick that falls in it, then flush.
    use std::collections::BTreeMap;
    let mut per_month: BTreeMap<String, MonthlyAggregate> = BTreeMap::new();
    let mut ticks = 0usize;

    // Use the *current* moment as a placeholder for ticks whose ordering we
    // can't recover (all ticks today). We instead bucket by the calendar day
    // of `now` for simplicity — the daemon emits UsageTicks during the
    // session and `flush_session_usage` runs immediately on session end, so
    // every tick falls in the same month as `now` in practice. A
    // session-spans-midnight edge is acceptable to round to "month of flush"
    // since the divergence is at most one day at a month boundary.
    let now = Utc::now();
    let bucket_key = month_key(now);

    for (_seq, event) in events {
        if let Event::UsageTick {
            provider,
            model,
            tokens_in,
            tokens_out,
            scope,
            ..
        } = event
        {
            ticks += 1;
            let cost = table.compute_cost(
                provider.to_string().as_str(),
                model.as_str(),
                tokens_in,
                tokens_out,
            );

            let agg = per_month
                .entry(bucket_key.clone())
                .or_insert_with(|| MonthlyAggregate {
                    month: bucket_key.clone(),
                    buckets: Vec::new(),
                });
            agg.record(
                workspace_id.clone(),
                provider,
                model,
                scope,
                tokens_in,
                tokens_out,
                cost,
                now,
            );
        }
    }

    if ticks == 0 {
        return Ok(0);
    }

    // Merge each per-month accumulator into its on-disk file (loading +
    // re-saving via the atomic helper).
    for (month_key, new_agg) in per_month {
        let path = monthly_path_in(usage_dir, parse_month_anchor(&month_key, now));
        let mut existing = MonthlyAggregate::load_or_default(&path).await;
        if existing.month.is_empty() {
            existing.month = month_key.clone();
        }
        for bucket in new_agg.buckets {
            merge_bucket(&mut existing, bucket);
        }
        existing.flush(&path).await?;
    }

    // Drop the sentinel last so a crash mid-flush (before the rename) leaves
    // the log un-flushed and the next attempt re-runs cleanly.
    if let Some(parent) = sentinel.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&sentinel, b"").await?;

    Ok(ticks)
}

/// Sentinel path for [`flush_session_usage`]'s idempotency marker. Public
/// because the integration tests want to assert on its presence.
pub fn sentinel_path(log_path: &Path) -> std::path::PathBuf {
    let mut name = log_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".usage-flushed");
    match log_path.parent() {
        Some(parent) => parent.join(name),
        None => std::path::PathBuf::from(name),
    }
}

/// Convenience for production callers: resolve `usage_dir` from
/// [`user_usage_dir`] and dispatch to [`flush_session_usage`]. Returns
/// `Ok(0)` when no user usage dir can be resolved (no `$HOME`,
/// extremely unusual) so the daemon doesn't crash on a degenerate
/// environment.
pub async fn flush_session_usage_to_user_dir(
    log_path: &Path,
    workspace_id: &WorkspaceId,
) -> Result<usize> {
    match user_usage_dir() {
        Some(dir) => flush_session_usage(log_path, workspace_id, &dir).await,
        None => Ok(0),
    }
}

/// F-605: walk `log_path` and fold every [`Event::UsageTick`] tagged with
/// `session_id` into a single [`SessionUsage`].
///
/// Backs the AgentMonitor §9.2 live token/cost counters: a window opening
/// mid-session (or surviving a webview reload) calls this to backfill the
/// totals it missed, then resumes from the live event stream. Read-only —
/// the flush sentinel is **not** written, so this is safe to call any
/// number of times during a session without affecting the eventual
/// monthly-aggregate flush.
///
/// Cost is repriced from the embedded [`PriceTable`] rather than trusting
/// the event's `cost_usd` field, mirroring [`flush_session_usage`]. A tick
/// for an unknown `(provider, model)` contributes `None` to `cost`, which
/// is sticky — a session that mixes priced and unpriced ticks reports
/// `cost: null` to the UI, never a partial sum.
///
/// Ticks tagged with a *different* session id (multiplexed log, defensive
/// against future re-use) and ticks emitted before F-605 (`session_id:
/// None`) are skipped. Returns [`SessionUsage::zero`] when the log has no
/// matching ticks yet — the UI renders zeros without erroring.
pub async fn live_session_totals(log_path: &Path, session_id: &SessionId) -> Result<SessionUsage> {
    let table = PriceTable::embedded();
    let events = read_since(log_path, 0).await?;

    let mut totals = SessionUsage::zero();
    for (_seq, event) in events {
        if let Event::UsageTick {
            session_id: tick_session,
            provider,
            model,
            tokens_in,
            tokens_out,
            ..
        } = event
        {
            if tick_session.as_ref() != Some(session_id) {
                continue;
            }
            let cost = table.compute_cost(
                provider.to_string().as_str(),
                model.as_str(),
                tokens_in,
                tokens_out,
            );
            totals.fold(tokens_in, tokens_out, cost);
        }
    }
    Ok(totals)
}

fn month_key(at: DateTime<Utc>) -> String {
    format!("{:04}-{:02}", at.year(), at.month())
}

/// Parse `YYYY-MM` back to a `DateTime<Utc>` anchored at the first of the
/// month. Falls back to `fallback` on malformed input — only happens if the
/// caller mutated the bucket key, which we control.
fn parse_month_anchor(key: &str, fallback: DateTime<Utc>) -> DateTime<Utc> {
    use chrono::TimeZone;
    let parts: Vec<&str> = key.split('-').collect();
    if parts.len() == 2 {
        if let (Ok(y), Ok(m)) = (parts[0].parse::<i32>(), parts[1].parse::<u32>()) {
            if let Some(t) = Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).single() {
                return t;
            }
        }
    }
    fallback
}

fn merge_bucket(agg: &mut MonthlyAggregate, new: UsageBucket) {
    agg.record(
        new.workspace_id,
        new.provider,
        new.model,
        new.scope,
        new.tokens_in,
        new.tokens_out,
        new.cost,
        new.last_updated,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::roster::RosterScope;
    use forge_core::{EventLog, ProviderId, SessionId};
    use tempfile::TempDir;

    fn provider(s: &str) -> ProviderId {
        ProviderId::from_string(s.to_string())
    }

    fn ws(s: &str) -> WorkspaceId {
        WorkspaceId::from_string(s.to_string())
    }

    /// F-605 fixture session id. The flush aggregator deliberately ignores
    /// it — these tests confirm `(workspace, provider, model, scope)`
    /// remains the bucket key regardless of the originating session.
    fn fixture_session() -> SessionId {
        SessionId::from_string("deadbeefcafebabe".to_string())
    }

    async fn write_events(path: &std::path::Path, events: &[Event]) {
        let mut log = EventLog::create(path).await.expect("create log");
        for e in events {
            log.append(e).await.expect("append");
        }
        log.flush().await.expect("flush log");
    }

    #[tokio::test]
    async fn flush_with_no_ticks_is_noop() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.jsonl");
        write_events(&log, &[]).await;

        let usage_dir = tmp.path().join("usage");
        let n = flush_session_usage(&log, &ws("w1"), &usage_dir)
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert!(!usage_dir.exists() || usage_dir.read_dir().unwrap().count() == 0);
    }

    #[tokio::test]
    async fn flush_one_tick_writes_monthly_file() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.jsonl");
        write_events(
            &log,
            &[Event::UsageTick {
                session_id: Some(fixture_session()),
                provider: provider("anthropic"),
                model: "claude-3-5-sonnet-20241022".into(),
                tokens_in: 1000,
                tokens_out: 500,
                cost_usd: 0.0,
                scope: RosterScope::SessionWide,
            }],
        )
        .await;

        let usage_dir = tmp.path().join("usage");
        let n = flush_session_usage(&log, &ws("w1"), &usage_dir)
            .await
            .unwrap();
        assert_eq!(n, 1);

        let now = Utc::now();
        let path = monthly_path_in(&usage_dir, now);
        assert!(path.exists(), "monthly file must be written");

        let loaded = MonthlyAggregate::load_or_default(&path).await;
        assert_eq!(loaded.buckets.len(), 1);
        assert_eq!(loaded.buckets[0].tokens_in, 1000);
        assert_eq!(loaded.buckets[0].tokens_out, 500);
        // Cost uses the embedded table, NOT the event's cost_usd field.
        // claude-3-5-sonnet-20241022 @ $3/MTok in + $15/MTok out:
        //   1000 * 3 / 1e6 + 500 * 15 / 1e6 = 0.003 + 0.0075 = 0.0105
        let cost = loaded.buckets[0].cost.as_ref().unwrap();
        assert!((cost.amount - 0.0105).abs() < 1e-9);
    }

    #[tokio::test]
    async fn flush_unknown_model_records_null_cost() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.jsonl");
        write_events(
            &log,
            &[Event::UsageTick {
                session_id: Some(fixture_session()),
                provider: provider("anthropic"),
                model: "claude-imaginary-99".into(),
                tokens_in: 1000,
                tokens_out: 500,
                cost_usd: 0.0,
                scope: RosterScope::SessionWide,
            }],
        )
        .await;

        let usage_dir = tmp.path().join("usage");
        flush_session_usage(&log, &ws("w1"), &usage_dir)
            .await
            .unwrap();

        let now = Utc::now();
        let path = monthly_path_in(&usage_dir, now);
        let loaded = MonthlyAggregate::load_or_default(&path).await;
        assert!(loaded.buckets[0].cost.is_none(), "missing model → null");
    }

    #[tokio::test]
    async fn flush_merges_into_existing_monthly_file() {
        let tmp = TempDir::new().unwrap();
        let log1 = tmp.path().join("session1.jsonl");
        let log2 = tmp.path().join("session2.jsonl");

        let tick = Event::UsageTick {
            session_id: Some(fixture_session()),
            provider: provider("anthropic"),
            model: "claude-3-5-sonnet-20241022".into(),
            tokens_in: 100,
            tokens_out: 50,
            cost_usd: 0.0,
            scope: RosterScope::SessionWide,
        };
        write_events(&log1, std::slice::from_ref(&tick)).await;
        write_events(&log2, &[tick]).await;

        let usage_dir = tmp.path().join("usage");
        flush_session_usage(&log1, &ws("w1"), &usage_dir)
            .await
            .unwrap();
        flush_session_usage(&log2, &ws("w1"), &usage_dir)
            .await
            .unwrap();

        let path = monthly_path_in(&usage_dir, Utc::now());
        let loaded = MonthlyAggregate::load_or_default(&path).await;
        assert_eq!(loaded.buckets.len(), 1, "same key, single bucket");
        assert_eq!(loaded.buckets[0].tokens_in, 200);
        assert_eq!(loaded.buckets[0].tokens_out, 100);
    }

    #[tokio::test]
    async fn flush_filters_non_usage_events() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.jsonl");
        write_events(
            &log,
            &[
                Event::UsageTick {
                    session_id: Some(fixture_session()),
                    provider: provider("anthropic"),
                    model: "claude-3-5-sonnet-20241022".into(),
                    tokens_in: 100,
                    tokens_out: 50,
                    cost_usd: 0.0,
                    scope: RosterScope::SessionWide,
                },
                Event::SessionEnded {
                    at: Utc::now(),
                    reason: forge_core::EndReason::Completed,
                    archived: false,
                },
            ],
        )
        .await;

        let usage_dir = tmp.path().join("usage");
        let n = flush_session_usage(&log, &ws("w1"), &usage_dir)
            .await
            .unwrap();
        assert_eq!(n, 1, "only UsageTick events count");
    }

    #[tokio::test]
    async fn flush_is_idempotent_via_sentinel() {
        // F-593: orchestrator hook + shell shutdown hook may both call flush
        // for the same session. The second call must NOT double-count.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.jsonl");
        write_events(
            &log,
            &[Event::UsageTick {
                session_id: Some(fixture_session()),
                provider: provider("anthropic"),
                model: "claude-3-5-sonnet-20241022".into(),
                tokens_in: 100,
                tokens_out: 50,
                cost_usd: 0.0,
                scope: RosterScope::SessionWide,
            }],
        )
        .await;

        let usage_dir = tmp.path().join("usage");
        let n1 = flush_session_usage(&log, &ws("w1"), &usage_dir)
            .await
            .unwrap();
        assert_eq!(n1, 1);
        // Second call must observe the sentinel and return 0 without merging.
        let n2 = flush_session_usage(&log, &ws("w1"), &usage_dir)
            .await
            .unwrap();
        assert_eq!(n2, 0, "second flush must be a no-op");

        // And the on-disk total must still be tokens_in=100, not 200.
        let path = monthly_path_in(&usage_dir, Utc::now());
        let loaded = MonthlyAggregate::load_or_default(&path).await;
        assert_eq!(loaded.buckets[0].tokens_in, 100, "no double-count");
    }

    #[tokio::test]
    async fn sentinel_path_sits_beside_log() {
        let p = std::path::Path::new("/a/b/events.jsonl");
        assert_eq!(
            sentinel_path(p),
            std::path::PathBuf::from("/a/b/events.jsonl.usage-flushed")
        );
    }

    #[tokio::test]
    async fn flush_is_atomic_against_stale_tmp() {
        // Crash-residue guard: a previous flush left a `.tmp` file. The new
        // flush must overwrite it cleanly and the on-disk JSON must be the
        // new payload.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.jsonl");
        write_events(
            &log,
            &[Event::UsageTick {
                session_id: Some(fixture_session()),
                provider: provider("anthropic"),
                model: "claude-3-5-sonnet-20241022".into(),
                tokens_in: 100,
                tokens_out: 50,
                cost_usd: 0.0,
                scope: RosterScope::SessionWide,
            }],
        )
        .await;

        let usage_dir = tmp.path().join("usage");
        std::fs::create_dir_all(&usage_dir).unwrap();
        let now = Utc::now();
        let path = monthly_path_in(&usage_dir, now);
        let mut tmp_path = path.clone();
        tmp_path.set_extension("json.tmp");
        // Pre-seed the stale tmp.
        std::fs::write(&tmp_path, b"this would be a partial payload").unwrap();

        flush_session_usage(&log, &ws("w1"), &usage_dir)
            .await
            .unwrap();

        let loaded = MonthlyAggregate::load_or_default(&path).await;
        assert_eq!(loaded.buckets.len(), 1, "new payload, not partial residue");
    }

    // ---------------------------------------------------------------------
    // F-605: live_session_totals
    // ---------------------------------------------------------------------

    fn alt_session() -> SessionId {
        SessionId::from_string("c0ffeec0ffeec0ff".to_string())
    }

    #[tokio::test]
    async fn live_totals_accumulate_across_multiple_turns() {
        // F-605 DoD: tokens accumulate across multiple ticks in one session.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.jsonl");
        let sid = fixture_session();
        write_events(
            &log,
            &[
                Event::UsageTick {
                    session_id: Some(sid.clone()),
                    provider: provider("anthropic"),
                    model: "claude-3-5-sonnet-20241022".into(),
                    tokens_in: 100,
                    tokens_out: 50,
                    cost_usd: 0.0,
                    scope: RosterScope::SessionWide,
                },
                Event::UsageTick {
                    session_id: Some(sid.clone()),
                    provider: provider("anthropic"),
                    model: "claude-3-5-sonnet-20241022".into(),
                    tokens_in: 200,
                    tokens_out: 100,
                    cost_usd: 0.0,
                    scope: RosterScope::SessionWide,
                },
            ],
        )
        .await;

        let totals = live_session_totals(&log, &sid).await.unwrap();
        assert_eq!(totals.tokens_in, 300);
        assert_eq!(totals.tokens_out, 150);
        // claude-3-5-sonnet-20241022 @ $3/MTok in + $15/MTok out:
        //   300 * 3 / 1e6 + 150 * 15 / 1e6 = 0.0009 + 0.00225 = 0.00315
        let cost = totals.cost.as_ref().expect("priced model");
        assert!((cost.amount - 0.00315).abs() < 1e-9, "got {}", cost.amount);
        assert_eq!(cost.currency, "USD");
    }

    #[tokio::test]
    async fn live_totals_partial_million_scales_linearly() {
        // F-605 DoD: cost reflects partial-million scaling — confirms the
        // live walker uses the same `(rate × tokens / 1e6)` calc as the
        // monthly flush.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.jsonl");
        let sid = fixture_session();
        write_events(
            &log,
            &[Event::UsageTick {
                session_id: Some(sid.clone()),
                provider: provider("anthropic"),
                model: "claude-3-5-sonnet-20241022".into(),
                tokens_in: 100_000,
                tokens_out: 50_000,
                cost_usd: 0.0,
                scope: RosterScope::SessionWide,
            }],
        )
        .await;

        let totals = live_session_totals(&log, &sid).await.unwrap();
        // 100k × $3/MTok = $0.30, 50k × $15/MTok = $0.75 → $1.05.
        let cost = totals.cost.as_ref().expect("priced model");
        assert!((cost.amount - 1.05).abs() < 1e-9, "got {}", cost.amount);
    }

    #[tokio::test]
    async fn live_totals_missing_model_yields_null_cost() {
        // F-605 DoD: missing-model cost surfaces as null, consistent with
        // F-593's flush-time `MonthlyAggregate::record` policy.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.jsonl");
        let sid = fixture_session();
        write_events(
            &log,
            &[
                Event::UsageTick {
                    session_id: Some(sid.clone()),
                    provider: provider("anthropic"),
                    model: "claude-3-5-sonnet-20241022".into(),
                    tokens_in: 1000,
                    tokens_out: 500,
                    cost_usd: 0.0,
                    scope: RosterScope::SessionWide,
                },
                Event::UsageTick {
                    session_id: Some(sid.clone()),
                    provider: provider("anthropic"),
                    model: "claude-imaginary-99".into(),
                    tokens_in: 100,
                    tokens_out: 50,
                    cost_usd: 0.0,
                    scope: RosterScope::SessionWide,
                },
            ],
        )
        .await;

        let totals = live_session_totals(&log, &sid).await.unwrap();
        // Tokens still accumulate even when one tick lacks pricing.
        assert_eq!(totals.tokens_in, 1100);
        assert_eq!(totals.tokens_out, 550);
        assert!(
            totals.cost.is_none(),
            "missing-model cost is sticky-null, got {:?}",
            totals.cost,
        );
    }

    #[tokio::test]
    async fn live_totals_filters_by_session_id() {
        // F-605: a tick tagged with a different session id must not bleed
        // into this session's running totals (defense for any future
        // multiplexed log layout).
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.jsonl");
        let sid = fixture_session();
        let other = alt_session();
        write_events(
            &log,
            &[
                Event::UsageTick {
                    session_id: Some(sid.clone()),
                    provider: provider("anthropic"),
                    model: "claude-3-5-sonnet-20241022".into(),
                    tokens_in: 100,
                    tokens_out: 50,
                    cost_usd: 0.0,
                    scope: RosterScope::SessionWide,
                },
                Event::UsageTick {
                    session_id: Some(other),
                    provider: provider("anthropic"),
                    model: "claude-3-5-sonnet-20241022".into(),
                    tokens_in: 9_999,
                    tokens_out: 9_999,
                    cost_usd: 0.0,
                    scope: RosterScope::SessionWide,
                },
            ],
        )
        .await;

        let totals = live_session_totals(&log, &sid).await.unwrap();
        assert_eq!(totals.tokens_in, 100, "other session's tick must not leak");
        assert_eq!(totals.tokens_out, 50);
    }

    #[tokio::test]
    async fn live_totals_skip_legacy_unattributed_ticks() {
        // F-605: ticks written before the field landed deserialize as
        // `session_id: None`. Live totals can't attribute them, so the
        // walker skips — never silently mis-credits an arbitrary session.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.jsonl");
        let sid = fixture_session();
        write_events(
            &log,
            &[Event::UsageTick {
                session_id: None,
                provider: provider("anthropic"),
                model: "claude-3-5-sonnet-20241022".into(),
                tokens_in: 100,
                tokens_out: 50,
                cost_usd: 0.0,
                scope: RosterScope::SessionWide,
            }],
        )
        .await;

        let totals = live_session_totals(&log, &sid).await.unwrap();
        assert_eq!(totals.tokens_in, 0);
        assert_eq!(totals.tokens_out, 0);
    }

    #[tokio::test]
    async fn live_totals_does_not_force_flush() {
        // F-605 DoD: backend can answer "session totals as of now?" without
        // forcing a flush. Asserts the sentinel is NOT written by the live
        // walker (so the eventual session-end flush still runs).
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.jsonl");
        let sid = fixture_session();
        write_events(
            &log,
            &[Event::UsageTick {
                session_id: Some(sid.clone()),
                provider: provider("anthropic"),
                model: "claude-3-5-sonnet-20241022".into(),
                tokens_in: 100,
                tokens_out: 50,
                cost_usd: 0.0,
                scope: RosterScope::SessionWide,
            }],
        )
        .await;

        let _ = live_session_totals(&log, &sid).await.unwrap();
        let sentinel = sentinel_path(&log);
        assert!(
            !sentinel.exists(),
            "live totals must not write the flush sentinel",
        );
    }

    #[tokio::test]
    async fn live_totals_empty_log_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.jsonl");
        write_events(&log, &[]).await;
        let totals = live_session_totals(&log, &fixture_session()).await.unwrap();
        assert_eq!(totals, SessionUsage::zero());
    }
}
