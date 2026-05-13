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
//! - `anthropic`                           — defaults (api.anthropic.com, claude-sonnet-4-6)
//! - `anthropic:<model>`                   — custom model, default base_url
//! - `anthropic:<model>@<base_url>`        — both custom
//! - `openai`                              — defaults (api.openai.com, gpt-4o-mini)
//! - `openai:<model>`                      — custom model, default base_url
//! - `openai:<model>@<base_url>`           — both custom

use anyhow::{anyhow, Result};

pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434";
pub const DEFAULT_OLLAMA_MODEL: &str = "llama3.2";

/// Default base URL for the Anthropic Messages API.
pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
/// Default Claude model. Pinned to the current Sonnet line (4.6, dateless
/// pinned snapshot) per Anthropic's 2026 model catalog. Users override via
/// `--provider anthropic:<model>`.
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";

/// Default base URL for the OpenAI Chat Completions API.
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com";
/// Default OpenAI model. `gpt-4o-mini` is the cheapest stable
/// GPT-4-family model and is sufficient for the F-745 smoke-test surface;
/// users override via `--provider openai:<model>`.
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-mini";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderKind {
    Mock,
    Ollama {
        base_url: String,
        model: String,
    },
    /// F-745: direct Anthropic Messages API. The constructor-time `api_key`
    /// is supplied by `forged` from an env / keyring lookup at startup; the
    /// orchestrator's F-744 seam overrides it per-turn with the active
    /// credential.
    Anthropic {
        base_url: String,
        model: String,
    },
    /// F-745: direct OpenAI Chat Completions API. Same constructor-vs-seam
    /// posture as Anthropic.
    OpenAi {
        base_url: String,
        model: String,
    },
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
        "anthropic" => parse_anthropic_rest(rest),
        "openai" => parse_openai_rest(rest),
        other => Err(anyhow!(
            "unknown provider kind: {other:?} (supported: mock, ollama, anthropic, openai)"
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
    let (model, base_url) = parse_model_at_url(
        rest,
        "ollama",
        DEFAULT_OLLAMA_MODEL,
        DEFAULT_OLLAMA_BASE_URL,
    )?;
    Ok(ProviderKind::Ollama { base_url, model })
}

fn parse_anthropic_rest(rest: Option<&str>) -> Result<ProviderKind> {
    let (model, base_url) = parse_model_at_url(
        rest,
        "anthropic",
        DEFAULT_ANTHROPIC_MODEL,
        DEFAULT_ANTHROPIC_BASE_URL,
    )?;
    Ok(ProviderKind::Anthropic { base_url, model })
}

fn parse_openai_rest(rest: Option<&str>) -> Result<ProviderKind> {
    let (model, base_url) = parse_model_at_url(
        rest,
        "openai",
        DEFAULT_OPENAI_MODEL,
        DEFAULT_OPENAI_BASE_URL,
    )?;
    Ok(ProviderKind::OpenAi { base_url, model })
}

/// Shared `<model>@<base_url>` grammar used by every keyed-provider spec.
///
/// - `None` → both defaults.
/// - `Some("")` → empty after the `:` separator: error (model cannot be empty).
/// - `Some("model")` → custom model, default URL.
/// - `Some("model@url")` → both custom; either side empty errors explicitly.
fn parse_model_at_url(
    rest: Option<&str>,
    kind: &str,
    default_model: &str,
    default_url: &str,
) -> Result<(String, String)> {
    match rest {
        None => Ok((default_model.to_string(), default_url.to_string())),
        Some(rest) => match rest.split_once('@') {
            Some((m, u)) => {
                if m.is_empty() {
                    return Err(anyhow!("{kind} spec: model cannot be empty"));
                }
                if u.is_empty() {
                    return Err(anyhow!("{kind} spec: base_url cannot be empty"));
                }
                Ok((m.to_string(), u.to_string()))
            }
            None => {
                if rest.is_empty() {
                    return Err(anyhow!("{kind} spec: model cannot be empty"));
                }
                Ok((rest.to_string(), default_url.to_string()))
            }
        },
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
        let err = parse_provider_spec("not-a-real-provider:something").unwrap_err();
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

    // F-745: Anthropic provider spec parsing.
    //
    // Grammar:
    //   anthropic                         → defaults (api.anthropic.com, claude-sonnet-4-6)
    //   anthropic:<model>                 → custom model, default URL
    //   anthropic:<model>@<base_url>      → both custom

    #[test]
    fn parses_bare_anthropic_with_defaults() {
        let kind = parse_provider_spec("anthropic").unwrap();
        assert_eq!(
            kind,
            ProviderKind::Anthropic {
                base_url: DEFAULT_ANTHROPIC_BASE_URL.into(),
                model: DEFAULT_ANTHROPIC_MODEL.into(),
            }
        );
    }

    #[test]
    fn parses_anthropic_with_explicit_model() {
        let kind = parse_provider_spec("anthropic:claude-3-5-sonnet").unwrap();
        assert_eq!(
            kind,
            ProviderKind::Anthropic {
                base_url: DEFAULT_ANTHROPIC_BASE_URL.into(),
                model: "claude-3-5-sonnet".into(),
            }
        );
    }

    #[test]
    fn parses_anthropic_with_model_at_url() {
        let kind = parse_provider_spec("anthropic:claude-3-5-sonnet@https://anthropic.example.com")
            .unwrap();
        assert_eq!(
            kind,
            ProviderKind::Anthropic {
                base_url: "https://anthropic.example.com".into(),
                model: "claude-3-5-sonnet".into(),
            }
        );
    }

    #[test]
    fn rejects_anthropic_with_empty_model() {
        assert!(parse_provider_spec("anthropic:@https://host").is_err());
    }

    #[test]
    fn rejects_anthropic_with_empty_url() {
        assert!(parse_provider_spec("anthropic:claude-3-5-sonnet@").is_err());
    }

    // F-745: OpenAI provider spec parsing.

    #[test]
    fn parses_bare_openai_with_defaults() {
        let kind = parse_provider_spec("openai").unwrap();
        assert_eq!(
            kind,
            ProviderKind::OpenAi {
                base_url: DEFAULT_OPENAI_BASE_URL.into(),
                model: DEFAULT_OPENAI_MODEL.into(),
            }
        );
    }

    #[test]
    fn parses_openai_with_explicit_model() {
        let kind = parse_provider_spec("openai:gpt-4.1-nano").unwrap();
        assert_eq!(
            kind,
            ProviderKind::OpenAi {
                base_url: DEFAULT_OPENAI_BASE_URL.into(),
                model: "gpt-4.1-nano".into(),
            }
        );
    }

    #[test]
    fn parses_openai_with_model_at_url() {
        let kind = parse_provider_spec("openai:gpt-4o-mini@https://openai.example.com").unwrap();
        assert_eq!(
            kind,
            ProviderKind::OpenAi {
                base_url: "https://openai.example.com".into(),
                model: "gpt-4o-mini".into(),
            }
        );
    }

    #[test]
    fn rejects_openai_with_empty_model() {
        assert!(parse_provider_spec("openai:@https://host").is_err());
    }

    #[test]
    fn rejects_openai_with_empty_url() {
        assert!(parse_provider_spec("openai:gpt-4o-mini@").is_err());
    }

    // F-745: the resolver returns the correct kind for both new providers.

    #[test]
    fn resolve_provider_kind_with_anthropic_spec_returns_anthropic() {
        let kind = resolve_provider_kind(Some("anthropic")).unwrap();
        assert!(matches!(kind, ProviderKind::Anthropic { .. }));
    }

    #[test]
    fn resolve_provider_kind_with_openai_spec_returns_openai() {
        let kind = resolve_provider_kind(Some("openai")).unwrap();
        assert!(matches!(kind, ProviderKind::OpenAi { .. }));
    }
}
