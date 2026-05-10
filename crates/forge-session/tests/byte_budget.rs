//! F-077: per-session aggregate byte budget regression tests.
//!
//! The per-op caps in `forge-fs` (10 MiB read/write) and `forge-providers`
//! (1 MiB / 4 MiB per-line) protect against single-op blow-up but compose
//! into no aggregate ceiling — a tool-chained adversary can issue many
//! within-cap calls and exhaust host memory. These tests pin the
//! per-session aggregate enforcement at the dispatcher boundary.

use forge_session::byte_budget::ByteBudget;
use forge_session::tools::{FsReadTool, ToolCtx, ToolDispatcher};
use serde_json::json;
use std::sync::Arc;

/// A small helper that materialises a file of `size` bytes inside `dir` and
/// returns its absolute path as a `String`.
fn make_file(dir: &std::path::Path, name: &str, size: usize) -> String {
    let path = dir.join(name);
    let body = vec![b'x'; size];
    std::fs::write(&path, body).unwrap();
    let canonical = std::fs::canonicalize(&path).unwrap();
    canonical.to_str().unwrap().to_string()
}

/// Chained `fs.read` calls whose summed bytes exceed the configured budget
/// must be refused once the budget is exhausted. The error shape carries
/// the budget identifier so a reviewer (or operator parsing logs) can
/// distinguish budget exhaustion from any other tool error.
#[tokio::test]
async fn chained_fs_reads_exceeding_budget_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let canonical_root = std::fs::canonicalize(dir.path()).unwrap();
    let allowed = format!("{}/**", canonical_root.to_str().unwrap());

    // 10 files of 200 KiB each = ~2 MiB total. Budget at 1 MiB so the
    // sequence overshoots cleanly partway through.
    const FILE_BYTES: usize = 200 * 1024;
    const NUM_FILES: usize = 10;
    const BUDGET: u64 = 1024 * 1024;

    let mut paths = Vec::with_capacity(NUM_FILES);
    for i in 0..NUM_FILES {
        paths.push(make_file(dir.path(), &format!("f{i}.bin"), FILE_BYTES));
    }

    let mut d = ToolDispatcher::new();
    d.register(Box::new(FsReadTool)).unwrap();

    let budget = Arc::new(ByteBudget::new(BUDGET));
    let ctx = ToolCtx {
        allowed_paths: vec![allowed],
        byte_budget: Some(budget.clone()),
        ..ToolCtx::default()
    };

    let mut succeeded = 0usize;
    let mut refused = 0usize;
    let mut last_error: Option<String> = None;
    for path in &paths {
        let result = d
            .dispatch("fs.read", &json!({ "path": path }), &ctx)
            .await
            .unwrap();
        if result.get("error").is_some() {
            refused += 1;
            last_error = result["error"].as_str().map(str::to_owned);
        } else {
            succeeded += 1;
        }
    }

    assert!(
        succeeded > 0 && succeeded < NUM_FILES,
        "expected partial success: succeeded={succeeded}, refused={refused}"
    );
    assert!(refused > 0, "expected at least one budget refusal");
    let err = last_error.expect("refused call must surface an error string");
    assert!(
        err.contains("byte budget exceeded") || err.contains("byte budget exhausted"),
        "error string does not identify budget exhaustion: {err}"
    );
    assert!(
        budget.consumed() >= BUDGET,
        "budget should be marked exhausted after refusal: consumed={}, limit={}",
        budget.consumed(),
        BUDGET
    );
}

/// Once the budget is exhausted, subsequent calls must be refused
/// immediately at the dispatcher — the underlying tool must not run.
/// Distinguishing "tool ran but result was an error" from "dispatcher
/// short-circuited" matters for the DoS guarantee: an attacker who can
/// drive the tool past the cap (even with no payload returned) defeats
/// the budget. Use a write-side observation: a `fs.read` of an
/// allow-listed file must NOT return content / sha256 once budget is
/// exhausted; the result must be the budget error only.
#[tokio::test]
async fn dispatcher_short_circuits_after_budget_exhaustion() {
    let dir = tempfile::tempdir().unwrap();
    let canonical_root = std::fs::canonicalize(dir.path()).unwrap();
    let allowed = format!("{}/**", canonical_root.to_str().unwrap());
    let path = make_file(dir.path(), "f.bin", 100 * 1024);

    let mut d = ToolDispatcher::new();
    d.register(Box::new(FsReadTool)).unwrap();

    // Budget sized so the first 100 KiB read completes (its result_byte_cost
    // plus the per-tool-call envelope overhead lands at the limit) but the
    // pre-check on the second call sees `consumed + overhead >= limit` and
    // short-circuits. Picks the file size as the budget so the first
    // charge clearly exhausts the headroom.
    let budget = Arc::new(ByteBudget::new(100 * 1024));
    let ctx = ToolCtx {
        allowed_paths: vec![allowed],
        byte_budget: Some(budget.clone()),
        ..ToolCtx::default()
    };

    // First call: tool runs and consumes the budget.
    let _ = d
        .dispatch("fs.read", &json!({ "path": &path }), &ctx)
        .await
        .unwrap();
    assert!(budget.is_exhausted(), "budget should be exhausted now");

    // Second call: must be short-circuited — no content, only an error.
    let result = d
        .dispatch("fs.read", &json!({ "path": &path }), &ctx)
        .await
        .unwrap();
    assert!(
        result.get("content").is_none(),
        "tool must not run after budget exhaustion: result={result}"
    );
    assert!(
        result.get("error").is_some(),
        "expected budget error: {result}"
    );
    let err = result["error"].as_str().unwrap();
    assert!(
        err.contains("byte budget"),
        "error must identify budget: {err}"
    );
}

/// `ToolCtx::default()` (no budget configured) must still dispatch
/// successfully — the budget check is opt-in so existing tests and
/// production code paths that haven't been migrated keep working
/// during rollout.
#[tokio::test]
async fn dispatcher_with_no_budget_runs_unrestricted() {
    let dir = tempfile::tempdir().unwrap();
    let canonical_root = std::fs::canonicalize(dir.path()).unwrap();
    let allowed = format!("{}/**", canonical_root.to_str().unwrap());
    let path = make_file(dir.path(), "f.bin", 16 * 1024);

    let mut d = ToolDispatcher::new();
    d.register(Box::new(FsReadTool)).unwrap();

    let ctx = ToolCtx {
        allowed_paths: vec![allowed],
        ..ToolCtx::default()
    };
    // 100 unrestricted calls succeed; no budget configured.
    for _ in 0..100 {
        let result = d
            .dispatch("fs.read", &json!({ "path": &path }), &ctx)
            .await
            .unwrap();
        assert!(
            result.get("error").is_none(),
            "no-budget dispatch should succeed: {result}"
        );
    }
}

/// `ByteBudget::default()` must match the documented production budget
/// (500 MiB). Pinning this constant in a test prevents silent drift
/// between code and `docs/dev/security.md`.
#[test]
fn default_byte_budget_is_500_mib() {
    let budget = ByteBudget::default();
    assert_eq!(budget.limit(), 500 * 1024 * 1024);
}
