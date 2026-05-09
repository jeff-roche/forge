//! `usage_summary` Tauri command (F-593).
//!
//! Reads every monthly aggregate file under `<config>/forge/usage/`, applies
//! the requested [`UsageRange`] / [`GroupBy`] / `cross_workspace` filter, and
//! returns a [`UsageSummary`] suitable for the (deferred F-594) usage view.
//!
//! ## Cross-workspace toggle
//!
//! - `cross_workspace = false` (default): the dashboard's currently-selected
//!   workspace is the filter. The shell resolves the active workspace from
//!   `BridgeState`'s cache via the same path used by other workspace-bound
//!   commands.
//! - `cross_workspace = true`: every bucket in every monthly file is folded
//!   into the result, regardless of which workspace recorded it.
//!
//! ## Authorisation
//!
//! Only the dashboard window may call this — the same pattern used by
//! `dashboard_list_providers`. Other webviews (per-session windows) get a
//! permission-denied error.

use std::path::PathBuf;

use forge_core::usage::{
    summarize, user_usage_dir, GroupBy, MonthlyAggregate, UsageRange, UsageSummary,
};
use forge_core::WorkspaceId;
use tauri::{Runtime, Webview};

use crate::ipc::{require_size, MAX_WORKSPACE_ROOT_BYTES};

/// F-674: pure validation helper exposed for unit tests. Mirrors the
/// optional-string check inside [`usage_summary`] without dragging in the
/// Tauri runtime. Optional inputs follow the standardization rule
/// documented in `crate::ipc`: when `Some`, the string is bounded via
/// the canonical `require_size` helper; when `None`, the check is skipped.
pub fn validate_optional_workspace_root(workspace_root: Option<&str>) -> Result<(), String> {
    if let Some(root) = workspace_root {
        require_size("workspace_root", root, MAX_WORKSPACE_ROOT_BYTES)?;
    }
    Ok(())
}

#[cfg(feature = "webview")]
async fn read_all_monthly_files(usage_dir: &std::path::Path) -> Vec<MonthlyAggregate> {
    let mut out = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(usage_dir).await else {
        return out;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let agg = MonthlyAggregate::load_or_default(&path).await;
        out.push(agg);
    }
    out
}

#[cfg(feature = "webview")]
fn resolve_usage_dir() -> Option<PathBuf> {
    user_usage_dir()
}

/// `usage_summary(range, group_by, cross_workspace)` — Tauri command.
///
/// `workspace_root` is the dashboard's currently-active workspace path. When
/// `cross_workspace` is `false`, results are filtered to that workspace's
/// derived id; when `true`, the filter is dropped.
#[cfg(feature = "webview")]
#[tauri::command]
pub async fn usage_summary<R: Runtime>(
    webview: Webview<R>,
    range: UsageRange,
    group_by: GroupBy,
    cross_workspace: bool,
    workspace_root: Option<String>,
) -> Result<UsageSummary, String> {
    crate::ipc::require_window_label(&webview, "dashboard", "usage_summary")?;
    validate_optional_workspace_root(workspace_root.as_deref())?;

    let usage_dir = match resolve_usage_dir() {
        Some(d) => d,
        // No platform config dir: return an empty summary rather than crash.
        None => {
            tracing::warn!(
                target: "forge_shell::usage",
                "usage_summary could not resolve usage dir; returning empty summary",
            );
            return Ok(summarize(&[], range, group_by, None, chrono::Utc::now()));
        }
    };

    let monthly_files = read_all_monthly_files(&usage_dir).await;

    let workspace_filter: Option<WorkspaceId> = if cross_workspace {
        None
    } else {
        workspace_root.map(WorkspaceId::from_string)
    };

    let summary = summarize(
        &monthly_files,
        range,
        group_by,
        workspace_filter.as_ref(),
        chrono::Utc::now(),
    );
    tracing::trace!(
        target: "forge_shell::usage",
        cross_workspace = cross_workspace,
        files = monthly_files.len(),
        breakdown_rows = summary.breakdown.len(),
        "usage_summary ok",
    );
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::roster::RosterScope;
    use forge_core::usage::{Money, MonthlyAggregate};
    use forge_core::ProviderId;
    use tempfile::TempDir;

    fn provider(s: &str) -> ProviderId {
        ProviderId::from_string(s.to_string())
    }

    fn ws(s: &str) -> WorkspaceId {
        WorkspaceId::from_string(s.to_string())
    }

    #[test]
    fn validate_optional_workspace_root_accepts_none() {
        assert!(validate_optional_workspace_root(None).is_ok());
    }

    #[test]
    fn validate_optional_workspace_root_accepts_within_cap() {
        assert!(validate_optional_workspace_root(Some("/path/to/workspace")).is_ok());
    }

    #[test]
    fn validate_optional_workspace_root_rejects_oversize() {
        let huge = "a".repeat(MAX_WORKSPACE_ROOT_BYTES + 1);
        let err = validate_optional_workspace_root(Some(&huge)).unwrap_err();
        // F-068 marker: stable across the codebase so UI handling and other
        // tests can pattern-match on the prefix.
        assert!(err.contains("payload too large"), "got: {err}");
        assert!(err.contains("workspace_root"), "got: {err}");
    }

    #[tokio::test]
    async fn read_all_monthly_files_skips_non_json() {
        let tmp = TempDir::new().unwrap();
        // Drop a .json that *parses* and a .txt that should be ignored.
        let agg = MonthlyAggregate {
            month: "2026-04".to_string(),
            buckets: Vec::new(),
        };
        agg.flush(&tmp.path().join("2026-04.json")).await.unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "ignored").unwrap();

        let files = read_all_monthly_files(tmp.path()).await;
        assert_eq!(files.len(), 1);
    }

    #[tokio::test]
    async fn read_all_monthly_files_missing_dir_is_empty() {
        let tmp = TempDir::new().unwrap();
        let files = read_all_monthly_files(&tmp.path().join("nope")).await;
        assert_eq!(files.len(), 0);
    }

    #[tokio::test]
    async fn end_to_end_session_to_summary() {
        // F-593 integration: simulate a session that recorded UsageTicks
        // (via `flush_session_usage`'s logic, here via direct `record`),
        // confirm the on-disk aggregate is queryable through the same
        // `summarize` call the IPC command uses.
        use chrono::Utc;
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut agg = MonthlyAggregate {
            month: "2026-04".to_string(),
            buckets: Vec::new(),
        };
        agg.record(
            ws("/path/to/workspace"),
            provider("anthropic"),
            "claude-3-5-sonnet-20241022".into(),
            RosterScope::SessionWide,
            10_000,
            5_000,
            Some(Money::usd(0.105)),
            Utc::now(),
        );
        agg.flush(&dir.join("2026-04.json")).await.unwrap();

        let files = read_all_monthly_files(dir).await;
        let summary = summarize(
            &files,
            UsageRange::All,
            GroupBy::Provider,
            Some(&ws("/path/to/workspace")),
            Utc::now(),
        );
        assert_eq!(summary.total_tokens_in, 10_000);
        assert_eq!(summary.breakdown.len(), 1);
        assert_eq!(summary.breakdown[0].key, "anthropic");
    }

    #[tokio::test]
    async fn cross_workspace_aggregates_across_ids() {
        use chrono::Utc;
        let tmp = TempDir::new().unwrap();
        let mut agg = MonthlyAggregate {
            month: "2026-04".to_string(),
            buckets: Vec::new(),
        };
        agg.record(
            ws("/ws/a"),
            provider("anthropic"),
            "claude-3-5-sonnet-20241022".into(),
            RosterScope::SessionWide,
            100,
            50,
            Some(Money::usd(0.01)),
            Utc::now(),
        );
        agg.record(
            ws("/ws/b"),
            provider("anthropic"),
            "claude-3-5-sonnet-20241022".into(),
            RosterScope::SessionWide,
            200,
            100,
            Some(Money::usd(0.02)),
            Utc::now(),
        );
        agg.flush(&tmp.path().join("2026-04.json")).await.unwrap();
        let files = read_all_monthly_files(tmp.path()).await;

        let only_a = summarize(
            &files,
            UsageRange::All,
            GroupBy::Provider,
            Some(&ws("/ws/a")),
            Utc::now(),
        );
        assert_eq!(only_a.total_tokens_in, 100);

        let cross = summarize(&files, UsageRange::All, GroupBy::Provider, None, Utc::now());
        assert_eq!(cross.total_tokens_in, 300);
    }
}
