//! Static price-table parser + cost calculator (F-593).
//!
//! The committed [`PRICES_TOML`] string is `include_str!`-embedded at compile
//! time so the calculator works in every test, daemon, and shell binary
//! without a runtime file dependency. Manual updates only — see the file
//! header for the rationale.
//!
//! Cost calc is the simple rate × tokens identity per the spec:
//!   `tokens_in × prompt_rate + tokens_out × completion_rate`
//! with rates expressed as USD per million tokens. A missing
//! `(provider, model)` key returns `None`, which the [`forge_core::usage`]
//! aggregator surfaces as `cost: null`.
//!
//! ## Wildcard
//!
//! A row with `model = "*"` matches any model under that provider. Useful
//! as a catch-all for local / self-hosted endpoints where every model
//! shares the same cost profile (typically zero) — a single wildcard row
//! covers them all without enumerating every checkpoint a user might pull.
//!
//! Lookup precedence is *exact match first*, wildcard fallback. A provider
//! that ships both a wildcard and an exact row picks the exact row.

use std::sync::OnceLock;

use forge_core::usage::Money;
use serde::Deserialize;

/// Compile-time embedding of the committed price table. Lives in
/// `crates/forge-providers/data/prices.toml`.
pub const PRICES_TOML: &str = include_str!("../data/prices.toml");

#[derive(Debug, Clone, Deserialize)]
struct PriceRow {
    provider: String,
    model: String,
    prompt_per_million_tokens: f64,
    completion_per_million_tokens: f64,
    currency: String,
    /// Tracking-only; not consumed by the calculator. Held in the row so a
    /// caller (e.g. a future "outdated price" warning) can read it without a
    /// re-parse.
    #[serde(default)]
    #[allow(dead_code)]
    last_updated: String,
}

#[derive(Debug, Deserialize)]
struct PriceFile {
    #[serde(default, rename = "entry")]
    entries: Vec<PriceRow>,
}

#[derive(Debug)]
pub struct PriceTable {
    rows: Vec<PriceRow>,
}

impl PriceTable {
    /// Parse a TOML body. Errors only on malformed input; an empty file
    /// yields an empty table (every lookup returns `None`).
    pub fn parse(toml_body: &str) -> Result<Self, toml::de::Error> {
        let file: PriceFile = toml::from_str(toml_body)?;
        Ok(Self { rows: file.entries })
    }

    /// The compile-time–embedded table from `data/prices.toml`. Panics on a
    /// malformed in-tree file (a release blocker — caught by the
    /// `embedded_table_parses` test below).
    ///
    /// Backed by a process-lifetime `OnceLock` so the TOML parse runs
    /// at most once per process. Every subsequent call returns the
    /// same `&'static PriceTable` reference — there is no per-call
    /// allocation, no per-call parse, and no lock contention beyond
    /// the OnceLock's one-time initialization fence. Safe to call
    /// from any thread, including the hot per-tick pricing path that
    /// runs on every provider chunk.
    pub fn embedded() -> &'static Self {
        static TABLE: OnceLock<PriceTable> = OnceLock::new();
        TABLE.get_or_init(|| {
            PriceTable::parse(PRICES_TOML)
                .expect("embedded prices.toml must parse — release blocker")
        })
    }

    /// Look up the row for `(provider, model)`, preferring an exact match
    /// over a wildcard fallback. `None` when neither matches.
    fn lookup(&self, provider: &str, model: &str) -> Option<&PriceRow> {
        let exact = self
            .rows
            .iter()
            .find(|r| r.provider == provider && r.model == model);
        if exact.is_some() {
            return exact;
        }
        self.rows
            .iter()
            .find(|r| r.provider == provider && r.model == "*")
    }

    /// Cost of `tokens_in` prompt + `tokens_out` completion at the rate
    /// listed for `(provider, model)`. Returns `None` when no row matches —
    /// callers must surface this as `null`, never as `0`, so the UI can
    /// distinguish "free" from "we don't know."
    pub fn compute_cost(
        &self,
        provider: &str,
        model: &str,
        tokens_in: u64,
        tokens_out: u64,
    ) -> Option<Money> {
        let row = self.lookup(provider, model)?;
        let amount = (tokens_in as f64) * row.prompt_per_million_tokens / 1_000_000.0
            + (tokens_out as f64) * row.completion_per_million_tokens / 1_000_000.0;
        Some(Money {
            amount,
            currency: row.currency.clone(),
        })
    }
}

/// Convenience: cost lookup against the embedded table.
pub fn compute_cost(provider: &str, model: &str, tokens_in: u64, tokens_out: u64) -> Option<Money> {
    PriceTable::embedded().compute_cost(provider, model, tokens_in, tokens_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_table_parses() {
        // Release blocker: the in-tree prices.toml must be parseable. If
        // someone breaks it, every cost lookup at runtime panics on first
        // call. Catch it in unit tests instead.
        let table = PriceTable::embedded();
        assert!(!table.rows.is_empty(), "embedded table must not be empty");
    }

    #[test]
    fn embedded_table_includes_every_required_row() {
        // Anthropic latest x3, OpenAI latest x4.
        let table = PriceTable::embedded();
        for m in [
            "claude-3-5-sonnet-20241022",
            "claude-3-5-haiku",
            "claude-3-opus",
        ] {
            assert!(
                table
                    .rows
                    .iter()
                    .any(|r| r.provider == "anthropic" && r.model == m),
                "anthropic/{m} missing from prices.toml",
            );
        }
        for m in ["gpt-4o", "gpt-4o-mini", "o1-preview", "o1-mini"] {
            assert!(
                table
                    .rows
                    .iter()
                    .any(|r| r.provider == "openai" && r.model == m),
                "openai/{m} missing from prices.toml",
            );
        }
    }

    #[test]
    fn compute_cost_known_model_uses_rate_per_million() {
        // claude-3-5-sonnet-20241022: $3/MTok in, $15/MTok out.
        let cost = compute_cost(
            "anthropic",
            "claude-3-5-sonnet-20241022",
            1_000_000,
            1_000_000,
        )
        .expect("known model must price");
        assert!((cost.amount - 18.0).abs() < 1e-9, "got {}", cost.amount);
        assert_eq!(cost.currency, "USD");
    }

    #[test]
    fn compute_cost_partial_million_scales_linearly() {
        // 100k tokens in × $3/MTok = $0.30; 50k tokens out × $15/MTok = $0.75.
        let cost = compute_cost("anthropic", "claude-3-5-sonnet-20241022", 100_000, 50_000)
            .expect("known model must price");
        assert!((cost.amount - 1.05).abs() < 1e-9);
    }

    #[test]
    fn compute_cost_unknown_model_returns_none() {
        // Spec: "missing model surfaced as null cost" — must not crash.
        assert!(compute_cost("anthropic", "claude-99-imaginary", 1000, 1000).is_none());
        assert!(compute_cost("unknown-provider", "any", 1000, 1000).is_none());
    }

    #[test]
    fn exact_match_beats_wildcard() {
        // Synthetic table: a wildcard at $1/$2 plus an exact row at $0/$0.
        // A lookup for the exact name must pick the exact row (free), not
        // the wildcard.
        let body = r#"
[[entry]]
provider = "p"
model = "*"
prompt_per_million_tokens = 1.0
completion_per_million_tokens = 2.0
currency = "USD"
last_updated = "2026-04-26"

[[entry]]
provider = "p"
model = "exact"
prompt_per_million_tokens = 0.0
completion_per_million_tokens = 0.0
currency = "USD"
last_updated = "2026-04-26"
"#;
        let table = PriceTable::parse(body).unwrap();
        let cost = table
            .compute_cost("p", "exact", 1_000_000, 1_000_000)
            .unwrap();
        assert_eq!(cost.amount, 0.0, "exact row must beat wildcard");
        let cost = table
            .compute_cost("p", "anything-else", 1_000_000, 1_000_000)
            .unwrap();
        assert!((cost.amount - 3.0).abs() < 1e-9, "wildcard fallback");
    }

    #[test]
    fn malformed_toml_surfaces_error() {
        let err = PriceTable::parse("this is = not [[ valid").unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn zero_tokens_yield_zero_cost() {
        let cost = compute_cost("anthropic", "claude-3-5-sonnet-20241022", 0, 0)
            .expect("known model must price");
        assert_eq!(cost.amount, 0.0);
    }
}
