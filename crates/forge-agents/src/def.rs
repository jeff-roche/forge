//! Agent definition types and the `.agents/*.md` loader.
//!
//! `AgentDef` is the parsed shape of one agent file; [`crate::AgentLoader`]
//! bundles the workspace + user merge plus the optional `AGENTS.md` preamble.

use anyhow::Context;
use gray_matter::{engine::YAML, Matter, ParsedEntity};
use serde::{de::IgnoredAny, Deserialize};
use std::{collections::BTreeMap, fs, path::Path};

use crate::error::{Error, Result};

/// Maximum byte length for an agent name. Mirrors `MAX_AGENT_ID_BYTES`
/// in `forge-shell::memory_ipc` so the IPC and parse paths agree.
pub const MAX_AGENT_NAME_BYTES: usize = 64;

/// F-649: validate an agent name as a path-safe filesystem stem.
///
/// Agent names land on disk as `<memory_root>/<name>.md`, so any input that
/// could escape the root (path separators, `..`, leading `.`, whitespace) or
/// blow up the filename (oversized stems) is rejected up front. Modeled on
/// [`forge_core::skill::SkillId`] with an added 64-byte length cap matching
/// the IPC `MAX_AGENT_ID_BYTES` ceiling.
pub fn validate_agent_name(name: &str) -> Result<()> {
    let invalid = |reason: &str| {
        Err(Error::InvalidAgentName {
            name: name.to_string(),
            reason: reason.to_string(),
        })
    };

    if name.is_empty() {
        return invalid("name must not be empty");
    }
    if name.len() > MAX_AGENT_NAME_BYTES {
        return invalid(&format!(
            "name too large: {} bytes exceeds cap of {} bytes",
            name.len(),
            MAX_AGENT_NAME_BYTES
        ));
    }
    if name.contains('/') || name.contains('\\') {
        return invalid("name must not contain a path separator ('/' or '\\\\')");
    }
    if name.starts_with('.') {
        return invalid("name must not start with '.'");
    }
    if name == ".." || name.contains("..") {
        return invalid("name must not contain '..'");
    }
    if name.chars().any(|c| c.is_ascii_whitespace()) {
        return invalid("name must not contain whitespace");
    }
    if name
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    {
        return invalid("name must contain only [A-Za-z0-9_-]");
    }
    Ok(())
}

/// Runtime isolation level applied to a live [`AgentInstance`](crate::AgentInstance).
///
/// `Trusted` bypasses sandboxing and is reserved for built-in skills shipped
/// with Forge — user-authored agents are rejected both at parse time and at
/// [`crate::Orchestrator::spawn`] if they declare it. `Process` is the
/// default for user agents. `Container(_)` is planned for later phases; for
/// now we encode the variant as `Container` (unit) so the enum is closed and
/// future work can attach a `ContainerSpec` without a breaking rename.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Isolation {
    /// No sandbox; reserved for built-in skills shipped with Forge.
    Trusted,
    /// Sandboxed subprocess isolation (default for user agents).
    #[default]
    Process,
    /// Container-backed isolation; reserved for a later phase.
    Container,
}

/// Canonical agent definition parsed from a Markdown file with YAML frontmatter.
///
/// `name` defaults to the file stem when frontmatter is absent or omits it,
/// `body` holds the prompt content with the frontmatter stripped, and
/// `allowed_paths` scopes filesystem access for tools the agent may invoke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    /// Agent identifier; defaults to the source file stem when frontmatter omits it.
    pub name: String,
    /// Human-readable description surfaced in pickers and UI banners.
    pub description: Option<String>,
    /// Prompt body with the YAML frontmatter stripped.
    pub body: String,
    /// Filesystem scopes the agent's tools are permitted to access.
    pub allowed_paths: Vec<String>,
    /// Runtime isolation policy applied when this def is spawned.
    pub isolation: Isolation,
    /// F-601: opt-in cross-session memory.
    ///
    /// Defaults to `false`. When `true`, the loader gates two behaviors:
    ///
    /// 1. The agent's `~/.config/forge/memory/<name>.md` body is appended to
    ///    the system prompt under a `## Memory` heading after `AGENTS.md`.
    /// 2. The `memory.write` tool is exposed to the agent so it can update
    ///    its own memory.
    ///
    /// Both default OFF; the agent author opts in by setting `memory: true`
    /// (or the explicit `memory_enabled: true`) in the agent's YAML
    /// frontmatter.
    pub memory_enabled: bool,
}

#[derive(Deserialize, Default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    isolation: Option<String>,
    allowed_paths: Option<Vec<String>>,
    /// F-601: terse alias `memory: true` accepted alongside `memory_enabled: true`.
    memory: Option<bool>,
    memory_enabled: Option<bool>,
    /// Catch-all for keys not matched above. Forward-compat preserves silent
    /// acceptance, but the loader emits a `tracing::warn!` per key so typos
    /// (e.g. `isolaton:`) surface in operator logs instead of being swallowed.
    #[serde(flatten)]
    extra: BTreeMap<String, IgnoredAny>,
}

pub(crate) fn parse_agent_file(path: &Path) -> Result<AgentDef> {
    let raw = match fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
    {
        Ok(raw) => raw,
        Err(err) => {
            tracing::warn!(
                target: "forge_agents::def",
                path = %path.display(),
                error = %err,
                "failed to read agent file",
            );
            return Err(Error::from(err));
        }
    };

    let matter = Matter::<YAML>::new();
    let parsed: ParsedEntity<Frontmatter> = match matter
        .parse(&raw)
        .with_context(|| format!("parsing frontmatter in {}", path.display()))
    {
        Ok(parsed) => parsed,
        Err(err) => {
            tracing::warn!(
                target: "forge_agents::def",
                path = %path.display(),
                error = %err,
                "failed to parse YAML frontmatter",
            );
            return Err(Error::from(err));
        }
    };

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    match parsed.data {
        Some(fm) => {
            for unknown in fm.extra.keys() {
                tracing::warn!(
                    target: "forge_agents::def",
                    path = %path.display(),
                    unknown_field = %unknown,
                    "unknown frontmatter field; ignored (forward-compat) — verify it is not a typo",
                );
            }
            let name = fm.name.unwrap_or_else(|| stem.clone());
            if let Err(err) = validate_agent_name(&name) {
                tracing::warn!(
                    target: "forge_agents::def",
                    path = %path.display(),
                    agent_name = %name,
                    error = %err,
                    "rejected agent: invalid name",
                );
                return Err(err);
            }
            let isolation = match fm.isolation.as_deref() {
                Some("trusted") => {
                    tracing::warn!(
                        target: "forge_agents::def",
                        path = %path.display(),
                        agent_name = %name,
                        "rejected agent: isolation=trusted not allowed for user-defined agents",
                    );
                    return Err(Error::IsolationViolation {
                        name,
                        path: Some(path.to_path_buf()),
                    });
                }
                Some("container") => Isolation::Container,
                Some("process") | None => Isolation::Process,
                Some(other) => {
                    tracing::warn!(
                        target: "forge_agents::def",
                        path = %path.display(),
                        isolation = %other,
                        "unknown isolation level in agent frontmatter",
                    );
                    return Err(Error::Other(anyhow::anyhow!(
                        "unknown isolation level '{other}' in {}",
                        path.display()
                    )));
                }
            };
            let memory_enabled = fm.memory_enabled.or(fm.memory).unwrap_or(false);
            Ok(AgentDef {
                name,
                description: fm.description,
                body: parsed.content,
                allowed_paths: fm.allowed_paths.unwrap_or_default(),
                isolation,
                memory_enabled,
            })
        }
        None => {
            if let Err(err) = validate_agent_name(&stem) {
                tracing::warn!(
                    target: "forge_agents::def",
                    path = %path.display(),
                    agent_name = %stem,
                    error = %err,
                    "rejected agent: invalid file stem",
                );
                return Err(err);
            }
            Ok(AgentDef {
                name: stem,
                description: None,
                body: parsed.content,
                allowed_paths: vec![],
                isolation: Isolation::Process,
                memory_enabled: false,
            })
        }
    }
}

pub(crate) fn load_from_dir(dir: &Path) -> Result<Vec<AgentDef>> {
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut paths: Vec<_> = fs::read_dir(dir)
        .map_err(|e| Error::Other(anyhow::Error::from(e)))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    paths.sort();
    paths.iter().map(|p| parse_agent_file(p)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// F-649: Path-traversal characters in `name` must be rejected before any
    /// downstream consumer (e.g. `MemoryStore`) can join them onto a root.
    #[test]
    fn validate_agent_name_rejects_traversal_attempts() {
        let traversal_inputs = [
            "../../../etc/passwd",
            "../escape",
            "..",
            "foo/../bar",
            "foo/bar",
            "foo\\bar",
            "/abs/path",
            ".hidden",
            ".",
            "",
            "with space",
            "tab\there",
            "new\nline",
            "weird:colon",
            "ünïcödé",
        ];
        for input in traversal_inputs {
            let err = validate_agent_name(input).unwrap_err();
            assert!(
                matches!(err, Error::InvalidAgentName { .. }),
                "expected InvalidAgentName for {input:?}, got {err:?}",
            );
        }
    }

    #[test]
    fn validate_agent_name_rejects_oversized_input() {
        let too_long = "a".repeat(MAX_AGENT_NAME_BYTES + 1);
        let err = validate_agent_name(&too_long).unwrap_err();
        assert!(matches!(err, Error::InvalidAgentName { .. }));
    }

    #[test]
    fn validate_agent_name_accepts_safe_stems() {
        for ok in ["scribe", "agent_1", "code-reviewer", "ABC123", "_private"] {
            validate_agent_name(ok).expect(ok);
        }
        // Exact-cap length is fine.
        let exact = "a".repeat(MAX_AGENT_NAME_BYTES);
        validate_agent_name(&exact).unwrap();
    }

    /// F-649: a hostile agent definition with `name: "../../../tmp/owned"` in
    /// frontmatter must be rejected at parse time, not happily handed downstream.
    #[test]
    fn parse_agent_file_rejects_traversal_in_frontmatter_name() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("legit.md");
        fs::write(&path, "---\nname: \"../../../tmp/owned\"\n---\n\nbody").unwrap();
        let err = parse_agent_file(&path).unwrap_err();
        assert!(
            matches!(err, Error::InvalidAgentName { .. }),
            "expected InvalidAgentName, got {err:?}",
        );
    }

    #[test]
    fn parse_agent_file_rejects_path_separator_in_frontmatter_name() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("legit.md");
        fs::write(&path, "---\nname: \"foo/bar\"\n---\n\nbody").unwrap();
        let err = parse_agent_file(&path).unwrap_err();
        assert!(matches!(err, Error::InvalidAgentName { .. }));
    }

    #[test]
    fn parse_agent_file_rejects_dotfile_stem_fallback() {
        // Frontmatter omits `name`, so the loader falls back to the file
        // stem. A `.hidden.md` stem is `.hidden` — must be rejected just like
        // a hostile frontmatter name.
        let dir = tempdir().unwrap();
        let path = dir.path().join(".hidden.md");
        fs::write(&path, "---\ndescription: x\n---\n\nbody").unwrap();
        let err = parse_agent_file(&path).unwrap_err();
        assert!(matches!(err, Error::InvalidAgentName { .. }));
    }

    #[test]
    fn parse_agent_file_accepts_safe_name() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("scribe.md");
        fs::write(&path, "---\nname: scribe\n---\n\nbody").unwrap();
        let def = parse_agent_file(&path).unwrap();
        assert_eq!(def.name, "scribe");
    }
}
