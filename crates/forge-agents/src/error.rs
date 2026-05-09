//! Typed errors for `forge-agents`.
//!
//! A typed error enum rather than bare `anyhow` so the isolation-violation
//! branch can be pattern-matched by runtime callers (sub-agent spawners, IPC
//! layers) without string-matching.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// A user-authored agent (parsed from `.agents/*.md` or `~/.agents/*.md`,
    /// or constructed programmatically with `AgentScope::User`) declared
    /// `isolation: trusted`. That level is reserved for built-in skills
    /// shipped with Forge itself.
    #[error("isolation: trusted is not allowed for user-defined agents ({name}{location})",
            location = source_hint(path))]
    IsolationViolation { name: String, path: Option<PathBuf> },

    /// `AGENTS.md` exceeds the maximum permitted size. The file is not
    /// injected into the system prompt. Callers should log a warning and
    /// treat the file as absent rather than failing the session.
    ///
    /// The cap exists to prevent unbounded token consumption and to limit the
    /// blast radius of a hostile or accidentally large `AGENTS.md` in an
    /// untrusted repository.
    #[error("AGENTS.md at {path} is {size} bytes, which exceeds the {limit}-byte cap; injection skipped")]
    AgentsMdTooLarge {
        path: PathBuf,
        size: u64,
        limit: u64,
    },

    /// F-649: an agent definition's `name` (or its file-stem fallback) failed
    /// path-traversal-safe validation. The id lands on disk as
    /// `<memory_root>/<name>.md`; accepting `/`, `\`, `..`, leading `.`,
    /// whitespace, or oversized stems would let a hostile or careless agent
    /// definition escape the memory root or render confusingly in CLI/UI.
    /// Mirrors the rules used by [`forge_core::skill::SkillId`] plus a
    /// 64-byte length cap matching the IPC `MAX_AGENT_ID_BYTES` ceiling.
    #[error("invalid agent name {name:?}: {reason}")]
    InvalidAgentName { name: String, reason: String },

    /// F-650: a `memory.write` call submitted a content payload larger than
    /// [`crate::memory::MEMORY_WRITE_CONTENT_CAP`]. Distinct from
    /// [`Error::InvalidAgentName`] so callers (the `memory.write` tool, the
    /// Dashboard editor, IPC consumers) can distinguish a path-traversal
    /// rejection from a size-cap rejection without string-matching.
    ///
    /// The bound exists to prevent two failure modes:
    ///
    /// 1. **DoS** — an unbounded blob persisted to disk and re-injected into
    ///    every subsequent system prompt exhausts context windows and
    ///    provider tokens.
    /// 2. **Persistent prompt injection** — a large blob magnifies the
    ///    surface area for crafted control sequences in the system prompt.
    #[error("memory.write content is {size} bytes, which exceeds the {limit}-byte cap")]
    MemoryContentTooLarge { size: usize, limit: usize },

    /// Parsing / IO / other non-isolation failures.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

fn source_hint(p: &Option<PathBuf>) -> String {
    match p {
        Some(path) => format!(" from {}", path.display()),
        None => String::new(),
    }
}

pub type Result<T> = std::result::Result<T, Error>;
