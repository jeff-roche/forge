//! Provider spec parser for `forged --provider <spec>` and `FORGE_PROVIDER` env.
//!
//! Grammar: `<kind>` or `<kind>:<rest>`. The first colon separates kind from
//! rest.

use anyhow::{anyhow, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderKind {
    Mock,
}

pub fn parse_provider_spec(spec: &str) -> Result<ProviderKind> {
    if spec.is_empty() {
        return Err(anyhow!("provider spec is empty"));
    }
    let (kind, _rest) = match spec.split_once(':') {
        Some((k, r)) => (k, Some(r)),
        None => (spec, None),
    };
    match kind {
        "mock" => Ok(ProviderKind::Mock),
        other => Err(anyhow!(
            "unknown provider kind: {other:?} (supported: mock)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_mock() {
        assert_eq!(parse_provider_spec("mock").unwrap(), ProviderKind::Mock);
    }

    #[test]
    fn rejects_empty_spec() {
        assert!(parse_provider_spec("").is_err());
    }

    #[test]
    fn rejects_unknown_kind() {
        let err = parse_provider_spec("anthropic:claude").unwrap_err();
        assert!(err.to_string().contains("unknown"));
    }
}
