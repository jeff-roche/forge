//! Provider spec parser for `forged --provider <spec>` and `FORGE_PROVIDER` env.
//!
//! Grammar: `<kind>` or `<kind>:<rest>`. The first colon separates kind from
//! rest.
//!
//! Per-kind grammars:
//! - `mock`                                — no configuration
//! - `ollama`                              — defaults (localhost:11434, llama3.2)
//! - `ollama:<model>`                      — custom model, default base_url
//! - `ollama:<model>@<base_url>`           — both custom

use anyhow::{anyhow, Result};

pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434";
pub const DEFAULT_OLLAMA_MODEL: &str = "llama3.2";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderKind {
    Mock,
    Ollama { base_url: String, model: String },
}

pub fn parse_provider_spec(spec: &str) -> Result<ProviderKind> {
    if spec.is_empty() {
        return Err(anyhow!("provider spec is empty"));
    }
    let (kind, rest) = match spec.split_once(':') {
        Some((k, r)) => (k, Some(r)),
        None => (spec, None),
    };
    match kind {
        "mock" => Ok(ProviderKind::Mock),
        "ollama" => parse_ollama_rest(rest),
        other => Err(anyhow!(
            "unknown provider kind: {other:?} (supported: mock, ollama)"
        )),
    }
}

/// Resolve a provider kind from the optional `--provider` flag / env var.
///
/// F-743: the production `forged` binary must NOT fall back to
/// `MockProvider` when no spec is supplied. Callers (other than `cfg(test)`
/// test code constructing Mock directly) must pass an explicit spec; this
/// resolver returns an error otherwise so the daemon refuses to start
/// rather than silently serving a scripted source.
pub fn resolve_provider_kind(spec: Option<&str>) -> Result<ProviderKind> {
    match spec {
        Some(s) => parse_provider_spec(s),
        None => Err(anyhow!(
            "provider_spec_required: --provider flag or FORGE_PROVIDER env must be set"
        )),
    }
}

fn parse_ollama_rest(rest: Option<&str>) -> Result<ProviderKind> {
    let (model, base_url) = match rest {
        None => (
            DEFAULT_OLLAMA_MODEL.to_string(),
            DEFAULT_OLLAMA_BASE_URL.to_string(),
        ),
        Some(rest) => match rest.split_once('@') {
            Some((m, u)) => {
                if m.is_empty() {
                    return Err(anyhow!("ollama spec: model cannot be empty"));
                }
                if u.is_empty() {
                    return Err(anyhow!("ollama spec: base_url cannot be empty"));
                }
                (m.to_string(), u.to_string())
            }
            None => {
                if rest.is_empty() {
                    return Err(anyhow!("ollama spec: model cannot be empty"));
                }
                (rest.to_string(), DEFAULT_OLLAMA_BASE_URL.to_string())
            }
        },
    };
    Ok(ProviderKind::Ollama { base_url, model })
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

    // F-743: Ollama provider spec parsing.
    //
    // Grammar:
    //   ollama                           → defaults (localhost:11434, llama3.2)
    //   ollama:<model>                   → custom model, default URL
    //   ollama:<model>@<base_url>        → both custom

    #[test]
    fn parses_bare_ollama_with_defaults() {
        let kind = parse_provider_spec("ollama").unwrap();
        assert_eq!(
            kind,
            ProviderKind::Ollama {
                base_url: "http://localhost:11434".into(),
                model: "llama3.2".into(),
            }
        );
    }

    #[test]
    fn parses_ollama_with_explicit_model() {
        let kind = parse_provider_spec("ollama:qwen2.5-coder").unwrap();
        assert_eq!(
            kind,
            ProviderKind::Ollama {
                base_url: "http://localhost:11434".into(),
                model: "qwen2.5-coder".into(),
            }
        );
    }

    #[test]
    fn parses_ollama_with_model_at_url() {
        let kind = parse_provider_spec("ollama:llama3@http://host:1234").unwrap();
        assert_eq!(
            kind,
            ProviderKind::Ollama {
                base_url: "http://host:1234".into(),
                model: "llama3".into(),
            }
        );
    }

    #[test]
    fn parses_ollama_with_https_url() {
        // F-743: ensure `@` splitting tolerates `https://` URLs (the `:` in
        // the scheme must not interfere with the `@` separator).
        let kind = parse_provider_spec("ollama:llama3@https://ollama.example.com").unwrap();
        assert_eq!(
            kind,
            ProviderKind::Ollama {
                base_url: "https://ollama.example.com".into(),
                model: "llama3".into(),
            }
        );
    }

    #[test]
    fn rejects_ollama_with_empty_model() {
        // `ollama:@http://host` would imply an empty model — reject explicitly.
        assert!(parse_provider_spec("ollama:@http://host").is_err());
    }

    // F-743 DoD #1: forged must NOT fall back to MockProvider outside cfg(test).
    // The resolver requires an explicit provider spec; production binary main
    // errors out before binding the UDS socket when one is absent.

    #[test]
    fn resolve_provider_kind_with_explicit_mock_spec_returns_mock() {
        assert_eq!(
            resolve_provider_kind(Some("mock")).unwrap(),
            ProviderKind::Mock
        );
    }

    #[test]
    fn resolve_provider_kind_with_ollama_spec_returns_ollama() {
        let kind = resolve_provider_kind(Some("ollama")).unwrap();
        assert!(matches!(kind, ProviderKind::Ollama { .. }));
    }

    #[test]
    fn resolve_provider_kind_with_no_spec_errors_with_required_message() {
        let err = resolve_provider_kind(None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("provider_spec_required"),
            "expected provider_spec_required, got: {msg}"
        );
    }
}
