//! Usage accounting types — the wire shape behind the `usage_summary` IPC
//! command and the on-disk monthly aggregate at
//! `~/.config/forge/usage/<YYYY-MM>.json`.
//!
//! F-593 introduces these alongside the cost-calculation pipeline in
//! `forge_providers::pricing` and the orchestrator's session-end flush hook.
//! This module owns:
//!
//! - [`UsageRange`] / [`GroupBy`] — request-side discriminators consumed by
//!   the `usage_summary` Tauri command.
//! - [`UsageSummary`] / [`UsageBreakdown`] — response-side shape returned to
//!   the webview.
//! - [`Money`] — price/cost wire shape with explicit `currency` so a UI can
//!   format without guessing.
//! - [`MonthlyAggregate`] — on-disk shape for the persistent rollup, plus
//!   the [`MonthlyAggregate::flush`] atomic writer.
//!
//! The on-disk format intentionally retains every aggregator key
//! (`workspace_id`, `provider`, `model`, `scope`) so the `usage_summary`
//! command can re-group at query time without rebuilding history.
//!
//! Wire shapes use `#[serde(tag = "type")]` for tagged unions to match the
//! project-wide convention documented in
//! `docs/architecture/event-conventions.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::config_file::{load_json_or_default, save_json_atomic};
use crate::ids::{ProviderId, WorkspaceId};
use crate::roster::RosterScope;
use crate::Result;

// ---------------------------------------------------------------------------
// Request-side types
// ---------------------------------------------------------------------------

/// Time window for a `usage_summary` query.
///
/// `Today` covers the calendar day in UTC; `Last7` and `Last30` cover the
/// trailing 7/30 days inclusive of today; `All` returns every monthly file
/// the aggregator can find. `CustomRange` carries an explicit half-open
/// `[start, end)` window — both bounds are required so the UI cannot accidentally
/// request an unbounded query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type")]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub enum UsageRange {
    Today,
    Last7,
    Last30,
    All,
    CustomRange {
        #[ts(type = "string")]
        start: DateTime<Utc>,
        #[ts(type = "string")]
        end: DateTime<Utc>,
    },
}

/// How a `usage_summary` query groups its breakdown rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub enum GroupBy {
    Provider,
    Model,
    Scope,
}

// ---------------------------------------------------------------------------
// Money
// ---------------------------------------------------------------------------

/// A monetary amount with explicit currency.
///
/// `amount` is stored as `f64` rather than a fixed-point type because the
/// aggregator only sums display values; we never perform exact-cent
/// arithmetic. `currency` is an ISO-4217 code (e.g. `"USD"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct Money {
    pub amount: f64,
    pub currency: String,
}

impl Money {
    /// Construct a USD amount.
    pub fn usd(amount: f64) -> Self {
        Self {
            amount,
            currency: "USD".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Response-side types
// ---------------------------------------------------------------------------

/// One row of a [`UsageSummary`].
///
/// `key` is the human-readable group key (e.g. `"anthropic"`, `"gpt-4o"`,
/// or the JSON-stringified [`RosterScope`]). `cost` is `None` when *any*
/// contributing event lacked a price-table entry — the UI surfaces this as
/// "—" so a missing price never silently zeros out the column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct UsageBreakdown {
    pub key: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost: Option<Money>,
}

/// Aggregated usage over a [`UsageRange`], grouped by [`GroupBy`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct UsageSummary {
    pub range: UsageRange,
    pub group_by: GroupBy,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub total_cost: Option<Money>,
    pub breakdown: Vec<UsageBreakdown>,
}

// ---------------------------------------------------------------------------
// Live per-session totals (F-605)
// ---------------------------------------------------------------------------

/// Running token + cost totals for a single live session.
///
/// F-605: emitted alongside the new `session_id` tag on
/// [`crate::Event::UsageTick`] so the AgentMonitor §9.2 toolbar can render
/// "tokens consumed in session X right now" without forcing a flush of the
/// monthly aggregate. Computed by walking the session's `events.jsonl`
/// (see `forge-session`'s `usage_flush::live_session_totals`) — the
/// on-disk monthly aggregate stays untouched.
///
/// `cost` is `Option<Money>` to match [`UsageBreakdown`] / [`UsageBucket`]
/// semantics — once any contributing tick lacks a price-table row, the
/// session-wide cost surfaces as `null` rather than silently dropping the
/// missing increment to zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct SessionUsage {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost: Option<Money>,
}

impl Default for SessionUsage {
    fn default() -> Self {
        Self::zero()
    }
}

impl SessionUsage {
    /// All-zero starting point; cost is `Some(0 USD)` until a tick without a
    /// priced row arrives.
    pub fn zero() -> Self {
        Self {
            tokens_in: 0,
            tokens_out: 0,
            cost: Some(Money::usd(0.0)),
        }
    }

    /// Fold one priced [`crate::Event::UsageTick`] payload into this total.
    ///
    /// `cost` of `None` is *sticky*: once observed, every subsequent
    /// increment leaves the session cost `None`. Mirrors
    /// [`MonthlyAggregate::record`]'s missing-price policy so live and
    /// flushed totals report a missing price the same way.
    pub fn fold(&mut self, tokens_in: u64, tokens_out: u64, cost: Option<Money>) {
        self.tokens_in = self.tokens_in.saturating_add(tokens_in);
        self.tokens_out = self.tokens_out.saturating_add(tokens_out);
        self.cost = match (self.cost.take(), cost) {
            (Some(prev), Some(now)) if prev.currency == now.currency => Some(Money {
                amount: prev.amount + now.amount,
                currency: prev.currency,
            }),
            _ => None,
        };
    }
}

// ---------------------------------------------------------------------------
// On-disk monthly aggregate
// ---------------------------------------------------------------------------

/// One bucket of accumulated usage keyed by `(workspace, provider, model, scope)`.
///
/// `cost` is `Option<Money>` — `None` propagates through aggregation so a
/// query whose contributing rows include any missing price returns
/// `cost: null` for that group.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageBucket {
    /// Workspace this usage was incurred under. Persisted so the
    /// `usage_summary` command can choose to filter to a single workspace
    /// (default) or aggregate across all workspaces (cross-workspace flag).
    pub workspace_id: WorkspaceId,
    pub provider: ProviderId,
    pub model: String,
    pub scope: RosterScope,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost: Option<Money>,
    /// Wall-clock timestamp of the last increment. Used by `Today`/`Last7`/
    /// `Last30` filters; the bucket is *added* to a result set when its
    /// `last_updated` falls within the requested range.
    pub last_updated: DateTime<Utc>,
}

/// On-disk shape of a single month's aggregated usage.
///
/// Stored at `<config_dir>/forge/usage/<YYYY-MM>.json`. Schema:
///
/// ```json
/// {
///   "month": "2026-04",
///   "buckets": [
///     {
///       "workspace_id": "…",
///       "provider": "anthropic",
///       "model": "claude-3-5-sonnet-20241022",
///       "scope": { "type": "SessionWide" },
///       "tokens_in": 1234,
///       "tokens_out": 567,
///       "cost": { "amount": 0.012, "currency": "USD" },
///       "last_updated": "2026-04-26T12:34:56Z"
///     }
///   ]
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MonthlyAggregate {
    /// `YYYY-MM` of the month this file represents.
    #[serde(default)]
    pub month: String,
    #[serde(default)]
    pub buckets: Vec<UsageBucket>,
}

impl MonthlyAggregate {
    /// Construct an empty aggregate for the calendar month of `at` (UTC).
    pub fn for_month_of(at: DateTime<Utc>) -> Self {
        Self {
            month: format_month(at),
            buckets: Vec::new(),
        }
    }

    /// Add a usage event into the aggregate, merging into an existing bucket
    /// when one matches the `(workspace, provider, model, scope)` key.
    ///
    /// `cost` of `None` is *sticky*: once a bucket has observed a missing
    /// price, every subsequent increment leaves it `None`. This matches the
    /// query-time aggregation policy in [`UsageSummary`].
    ///
    /// The argument list mirrors the `(workspace, provider, model, scope)`
    /// bucket key plus the per-tick payload — wrapping them in a struct
    /// adds boilerplate without improving call sites, so we silence the
    /// `too_many_arguments` lint here.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        workspace_id: WorkspaceId,
        provider: ProviderId,
        model: String,
        scope: RosterScope,
        tokens_in: u64,
        tokens_out: u64,
        cost: Option<Money>,
        at: DateTime<Utc>,
    ) {
        if let Some(bucket) = self.buckets.iter_mut().find(|b| {
            b.workspace_id == workspace_id
                && b.provider == provider
                && b.model == model
                && b.scope == scope
        }) {
            bucket.tokens_in = bucket.tokens_in.saturating_add(tokens_in);
            bucket.tokens_out = bucket.tokens_out.saturating_add(tokens_out);
            bucket.cost = match (bucket.cost.take(), cost) {
                (Some(prev), Some(now)) if prev.currency == now.currency => Some(Money {
                    amount: prev.amount + now.amount,
                    currency: prev.currency,
                }),
                // Differing currencies or any missing leg → null cost.
                _ => None,
            };
            bucket.last_updated = at;
        } else {
            self.buckets.push(UsageBucket {
                workspace_id,
                provider,
                model,
                scope,
                tokens_in,
                tokens_out,
                cost,
                last_updated: at,
            });
        }
    }

    /// Atomically write this aggregate to `path` via tmp+rename.
    /// Creates `path.parent()` if absent.
    pub async fn flush(&self, path: &Path) -> Result<()> {
        save_json_atomic(path, self).await
    }

    /// Read the aggregate at `path`, returning a default-constructed
    /// (empty) value if the file is absent or malformed. Mirrors the
    /// JSON-config degradation policy in
    /// [`crate::config_file::load_json_or_default`] — a half-written file
    /// must not brick the next session's flush.
    pub async fn load_or_default(path: &Path) -> Self {
        load_json_or_default(path).await
    }
}

/// Resolve the canonical user-scope usage directory:
/// `<config_dir>/forge/usage/`.
pub fn user_usage_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|base| base.join("forge").join("usage"))
}

/// Test-friendly variant of [`user_usage_dir`] anchored at `config_dir`.
pub fn user_usage_dir_in(config_dir: &Path) -> PathBuf {
    config_dir.join("forge").join("usage")
}

/// Resolve the monthly aggregate path for `at` under `usage_dir`.
pub fn monthly_path_in(usage_dir: &Path, at: DateTime<Utc>) -> PathBuf {
    usage_dir.join(format!("{}.json", format_month(at)))
}

/// Format a UTC timestamp as `YYYY-MM`.
pub fn format_month(at: DateTime<Utc>) -> String {
    format!("{:04}-{:02}", at.year(), at.month())
}

// ---------------------------------------------------------------------------
// Aggregation: monthly buckets → UsageSummary
// ---------------------------------------------------------------------------

/// Collapse a set of monthly aggregates into a [`UsageSummary`] for the
/// requested `range` and `group_by`.
///
/// `workspace_filter` of `Some(id)` keeps only buckets matching `id`; `None`
/// is the cross-workspace path (the `cross_workspace` toggle on the IPC
/// command). `now` is injected so the `Today`/`Last7`/`Last30` boundaries
/// are deterministic in tests.
pub fn summarize(
    months: &[MonthlyAggregate],
    range: UsageRange,
    group_by: GroupBy,
    workspace_filter: Option<&WorkspaceId>,
    now: DateTime<Utc>,
) -> UsageSummary {
    let (start, end) = range_bounds(&range, now);

    let mut groups: BTreeMap<String, (u64, u64, Option<f64>, bool, String)> = BTreeMap::new();
    let mut total_in: u64 = 0;
    let mut total_out: u64 = 0;
    let mut total_cost: Option<f64> = Some(0.0);
    let mut total_currency: Option<String> = None;
    let mut total_seen_missing = false;

    for monthly in months {
        for bucket in &monthly.buckets {
            if let Some(filter) = workspace_filter {
                if &bucket.workspace_id != filter {
                    continue;
                }
            }
            if let Some(start) = start {
                if bucket.last_updated < start {
                    continue;
                }
            }
            if let Some(end) = end {
                if bucket.last_updated >= end {
                    continue;
                }
            }
            let key = group_key(group_by, bucket);
            let entry = groups
                .entry(key)
                .or_insert((0, 0, Some(0.0), false, String::from("USD")));
            entry.0 = entry.0.saturating_add(bucket.tokens_in);
            entry.1 = entry.1.saturating_add(bucket.tokens_out);
            match &bucket.cost {
                Some(money) => {
                    if !entry.3 {
                        if let Some(sum) = entry.2.as_mut() {
                            *sum += money.amount;
                            entry.4 = money.currency.clone();
                        }
                    }
                }
                None => {
                    entry.3 = true;
                    entry.2 = None;
                }
            }

            total_in = total_in.saturating_add(bucket.tokens_in);
            total_out = total_out.saturating_add(bucket.tokens_out);
            match &bucket.cost {
                Some(money) => {
                    if !total_seen_missing {
                        if let Some(sum) = total_cost.as_mut() {
                            *sum += money.amount;
                        }
                        total_currency = Some(money.currency.clone());
                    }
                }
                None => {
                    total_seen_missing = true;
                    total_cost = None;
                }
            }
        }
    }

    let breakdown: Vec<UsageBreakdown> = groups
        .into_iter()
        .map(|(key, (tin, tout, sum, missing, currency))| {
            let cost = if missing {
                None
            } else {
                sum.map(|amount| Money { amount, currency })
            };
            UsageBreakdown {
                key,
                tokens_in: tin,
                tokens_out: tout,
                cost,
            }
        })
        .collect();

    let total_cost_money = if total_seen_missing {
        None
    } else {
        total_cost.map(|amount| Money {
            amount,
            currency: total_currency.unwrap_or_else(|| "USD".to_string()),
        })
    };

    UsageSummary {
        range,
        group_by,
        total_tokens_in: total_in,
        total_tokens_out: total_out,
        total_cost: total_cost_money,
        breakdown,
    }
}

fn group_key(group_by: GroupBy, bucket: &UsageBucket) -> String {
    match group_by {
        GroupBy::Provider => bucket.provider.to_string(),
        GroupBy::Model => bucket.model.clone(),
        GroupBy::Scope => {
            serde_json::to_string(&bucket.scope).unwrap_or_else(|_| String::from("\"<scope>\""))
        }
    }
}

fn range_bounds(
    range: &UsageRange,
    now: DateTime<Utc>,
) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    use chrono::Duration;
    match range {
        UsageRange::Today => {
            let start = now
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .map(|n| DateTime::<Utc>::from_naive_utc_and_offset(n, Utc));
            (start, None)
        }
        UsageRange::Last7 => (Some(now - Duration::days(7)), None),
        UsageRange::Last30 => (Some(now - Duration::days(30)), None),
        UsageRange::All => (None, None),
        UsageRange::CustomRange { start, end } => (Some(*start), Some(*end)),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn ws(s: &str) -> WorkspaceId {
        WorkspaceId::from_string(s.to_string())
    }

    fn provider(s: &str) -> ProviderId {
        ProviderId::from_string(s.to_string())
    }

    fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
    }

    #[test]
    fn usage_range_today_wire_shape() {
        let json = serde_json::to_string(&UsageRange::Today).unwrap();
        assert_eq!(json, "{\"type\":\"Today\"}");
        let back: UsageRange = serde_json::from_str(&json).unwrap();
        assert_eq!(back, UsageRange::Today);
    }

    #[test]
    fn usage_range_custom_round_trips_bounds() {
        let start = at(2026, 4, 1);
        let end = at(2026, 5, 1);
        let r = UsageRange::CustomRange { start, end };
        let json = serde_json::to_string(&r).unwrap();
        let back: UsageRange = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn group_by_wire_shape() {
        let json = serde_json::to_string(&GroupBy::Provider).unwrap();
        assert_eq!(json, "\"Provider\"");
        let back: GroupBy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, GroupBy::Provider);
    }

    #[test]
    fn money_usd_helper() {
        let m = Money::usd(1.5);
        assert_eq!(m.amount, 1.5);
        assert_eq!(m.currency, "USD");
    }

    #[test]
    fn format_month_pads_zero() {
        let t = at(2026, 4, 26);
        assert_eq!(format_month(t), "2026-04");
        let t = at(2026, 12, 1);
        assert_eq!(format_month(t), "2026-12");
    }

    #[test]
    fn record_merges_existing_bucket() {
        let mut agg = MonthlyAggregate::for_month_of(at(2026, 4, 1));
        agg.record(
            ws("w1"),
            provider("anthropic"),
            "claude-3-5-sonnet-20241022".into(),
            RosterScope::SessionWide,
            100,
            50,
            Some(Money::usd(0.10)),
            at(2026, 4, 1),
        );
        agg.record(
            ws("w1"),
            provider("anthropic"),
            "claude-3-5-sonnet-20241022".into(),
            RosterScope::SessionWide,
            200,
            100,
            Some(Money::usd(0.20)),
            at(2026, 4, 2),
        );
        assert_eq!(agg.buckets.len(), 1);
        assert_eq!(agg.buckets[0].tokens_in, 300);
        assert_eq!(agg.buckets[0].tokens_out, 150);
        let cost = agg.buckets[0].cost.as_ref().unwrap();
        assert!((cost.amount - 0.30).abs() < 1e-9);
        assert_eq!(cost.currency, "USD");
    }

    #[test]
    fn record_distinct_keys_create_separate_buckets() {
        let mut agg = MonthlyAggregate::for_month_of(at(2026, 4, 1));
        agg.record(
            ws("w1"),
            provider("anthropic"),
            "claude-3-5-sonnet-20241022".into(),
            RosterScope::SessionWide,
            100,
            50,
            Some(Money::usd(0.10)),
            at(2026, 4, 1),
        );
        agg.record(
            ws("w1"),
            provider("openai"),
            "gpt-4o".into(),
            RosterScope::SessionWide,
            200,
            100,
            Some(Money::usd(0.20)),
            at(2026, 4, 1),
        );
        assert_eq!(agg.buckets.len(), 2);
    }

    #[test]
    fn record_missing_cost_propagates() {
        let mut agg = MonthlyAggregate::for_month_of(at(2026, 4, 1));
        agg.record(
            ws("w1"),
            provider("custom"),
            "unknown-model".into(),
            RosterScope::SessionWide,
            100,
            50,
            None,
            at(2026, 4, 1),
        );
        agg.record(
            ws("w1"),
            provider("custom"),
            "unknown-model".into(),
            RosterScope::SessionWide,
            100,
            50,
            Some(Money::usd(0.10)),
            at(2026, 4, 1),
        );
        // Once any contributing increment is None, the bucket cost is None.
        assert!(agg.buckets[0].cost.is_none());
    }

    #[tokio::test]
    async fn flush_then_load_roundtrips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("2026-04.json");
        let mut agg = MonthlyAggregate::for_month_of(at(2026, 4, 1));
        agg.record(
            ws("w1"),
            provider("anthropic"),
            "claude-3-5-sonnet-20241022".into(),
            RosterScope::SessionWide,
            123,
            45,
            Some(Money::usd(0.05)),
            at(2026, 4, 26),
        );
        agg.flush(&path).await.unwrap();
        assert!(path.exists());

        let loaded = MonthlyAggregate::load_or_default(&path).await;
        assert_eq!(loaded.month, "2026-04");
        assert_eq!(loaded.buckets.len(), 1);
        assert_eq!(loaded.buckets[0].tokens_in, 123);
    }

    #[tokio::test]
    async fn flush_is_atomic_on_existing_tmp_residue() {
        // F-593 atomicity guard: a stale `.tmp` left behind by an aborted
        // previous flush must be overwritten + renamed cleanly. The on-disk
        // file must hold the new payload, never a partial.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("2026-04.json");

        let agg1 = MonthlyAggregate::for_month_of(at(2026, 4, 1));
        agg1.flush(&path).await.unwrap();

        let tmp = crate::config_file::tmp_path_for(&path);
        tokio::fs::write(&tmp, b"this would be a partial JSON payload")
            .await
            .unwrap();
        assert!(tmp.exists());

        let mut agg2 = MonthlyAggregate::for_month_of(at(2026, 4, 1));
        agg2.record(
            ws("w1"),
            provider("anthropic"),
            "claude-3-5-sonnet-20241022".into(),
            RosterScope::SessionWide,
            10,
            5,
            Some(Money::usd(0.01)),
            at(2026, 4, 26),
        );
        agg2.flush(&path).await.unwrap();
        assert!(!tmp.exists(), "stale tmp must be consumed");

        let loaded = MonthlyAggregate::load_or_default(&path).await;
        assert_eq!(loaded.buckets.len(), 1, "loaded body must be the new write");
    }

    #[tokio::test]
    async fn load_or_default_on_missing_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope.json");
        let agg = MonthlyAggregate::load_or_default(&path).await;
        assert_eq!(agg.buckets.len(), 0);
    }

    #[test]
    fn summarize_groups_by_provider_and_sums() {
        let mut april = MonthlyAggregate::for_month_of(at(2026, 4, 1));
        april.record(
            ws("w1"),
            provider("anthropic"),
            "claude-3-5-sonnet-20241022".into(),
            RosterScope::SessionWide,
            100,
            50,
            Some(Money::usd(0.10)),
            at(2026, 4, 1),
        );
        april.record(
            ws("w1"),
            provider("openai"),
            "gpt-4o".into(),
            RosterScope::SessionWide,
            200,
            100,
            Some(Money::usd(0.20)),
            at(2026, 4, 2),
        );

        let summary = summarize(
            &[april],
            UsageRange::All,
            GroupBy::Provider,
            Some(&ws("w1")),
            at(2026, 4, 30),
        );
        assert_eq!(summary.total_tokens_in, 300);
        assert_eq!(summary.total_tokens_out, 150);
        assert_eq!(summary.breakdown.len(), 2);
        let anthropic = summary
            .breakdown
            .iter()
            .find(|b| b.key == "anthropic")
            .unwrap();
        assert_eq!(anthropic.tokens_in, 100);
        let total_cost = summary.total_cost.as_ref().unwrap();
        assert!((total_cost.amount - 0.30).abs() < 1e-9);
    }

    #[test]
    fn summarize_filters_by_workspace() {
        let mut agg = MonthlyAggregate::for_month_of(at(2026, 4, 1));
        agg.record(
            ws("w1"),
            provider("anthropic"),
            "claude-3-5-sonnet-20241022".into(),
            RosterScope::SessionWide,
            100,
            50,
            Some(Money::usd(0.10)),
            at(2026, 4, 1),
        );
        agg.record(
            ws("w2"),
            provider("anthropic"),
            "claude-3-5-sonnet-20241022".into(),
            RosterScope::SessionWide,
            500,
            250,
            Some(Money::usd(0.50)),
            at(2026, 4, 1),
        );

        let only_w1 = summarize(
            std::slice::from_ref(&agg),
            UsageRange::All,
            GroupBy::Provider,
            Some(&ws("w1")),
            at(2026, 4, 30),
        );
        assert_eq!(only_w1.total_tokens_in, 100);

        let cross = summarize(
            std::slice::from_ref(&agg),
            UsageRange::All,
            GroupBy::Provider,
            None,
            at(2026, 4, 30),
        );
        assert_eq!(cross.total_tokens_in, 600);
    }

    #[test]
    fn summarize_missing_cost_propagates_to_total() {
        let mut agg = MonthlyAggregate::for_month_of(at(2026, 4, 1));
        agg.record(
            ws("w1"),
            provider("anthropic"),
            "known-model".into(),
            RosterScope::SessionWide,
            100,
            50,
            Some(Money::usd(0.10)),
            at(2026, 4, 1),
        );
        agg.record(
            ws("w1"),
            provider("custom"),
            "unknown-model".into(),
            RosterScope::SessionWide,
            100,
            50,
            None,
            at(2026, 4, 1),
        );
        let summary = summarize(
            std::slice::from_ref(&agg),
            UsageRange::All,
            GroupBy::Provider,
            Some(&ws("w1")),
            at(2026, 4, 30),
        );
        // Total cost is None when any group has missing pricing.
        assert!(summary.total_cost.is_none());
        let unknown = summary
            .breakdown
            .iter()
            .find(|b| b.key == "custom")
            .unwrap();
        assert!(unknown.cost.is_none());
        let known = summary
            .breakdown
            .iter()
            .find(|b| b.key == "anthropic")
            .unwrap();
        assert!(known.cost.is_some());
    }

    #[test]
    fn summarize_today_excludes_yesterday() {
        let mut agg = MonthlyAggregate::for_month_of(at(2026, 4, 1));
        agg.record(
            ws("w1"),
            provider("anthropic"),
            "claude-3-5-sonnet-20241022".into(),
            RosterScope::SessionWide,
            100,
            50,
            Some(Money::usd(0.10)),
            at(2026, 4, 25), // yesterday
        );
        agg.record(
            ws("w1"),
            provider("anthropic"),
            "claude-3-5-sonnet-20241022".into(),
            RosterScope::SessionWide,
            10,
            5,
            Some(Money::usd(0.01)),
            at(2026, 4, 26), // today
        );
        // Before merge: separate buckets via different last_updated? No,
        // bucket dedupes on key — last_updated is overwritten. So drop one
        // record, query only the second.
        let mut agg2 = MonthlyAggregate::for_month_of(at(2026, 4, 1));
        agg2.record(
            ws("w1"),
            provider("anthropic"),
            "claude-3-5-sonnet-20241022".into(),
            RosterScope::SessionWide,
            100,
            50,
            Some(Money::usd(0.10)),
            at(2026, 4, 25),
        );
        let now = at(2026, 4, 26);
        let summary = summarize(
            std::slice::from_ref(&agg2),
            UsageRange::Today,
            GroupBy::Provider,
            Some(&ws("w1")),
            now,
        );
        assert_eq!(summary.total_tokens_in, 0, "yesterday excluded by Today");
    }

    #[test]
    fn summarize_custom_range_half_open() {
        let mut agg = MonthlyAggregate::for_month_of(at(2026, 4, 1));
        agg.record(
            ws("w1"),
            provider("anthropic"),
            "m".into(),
            RosterScope::SessionWide,
            100,
            50,
            Some(Money::usd(0.10)),
            at(2026, 4, 10),
        );
        let summary = summarize(
            std::slice::from_ref(&agg),
            UsageRange::CustomRange {
                start: at(2026, 4, 1),
                end: at(2026, 4, 10), // exclusive — 4-10 is OUT
            },
            GroupBy::Provider,
            Some(&ws("w1")),
            at(2026, 4, 30),
        );
        assert_eq!(summary.total_tokens_in, 0);

        let summary = summarize(
            std::slice::from_ref(&agg),
            UsageRange::CustomRange {
                start: at(2026, 4, 1),
                end: at(2026, 4, 11),
            },
            GroupBy::Provider,
            Some(&ws("w1")),
            at(2026, 4, 30),
        );
        assert_eq!(summary.total_tokens_in, 100);
    }
}
