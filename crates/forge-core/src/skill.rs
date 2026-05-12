//! Skill definition and `SkillId` newtype.
//!
//! A `Skill` is a reusable capability pack — prompts, system instructions,
//! optional tool-binding hints, example I/O — that an agent can load. Forge
//! follows the [agentskills.io](https://agentskills.io) open standard: a
//! folder containing `SKILL.md` (YAML frontmatter + markdown body), with
//! optional `scripts/` and `references/` subdirectories.
//!
//! `Skill` is the parsed shape of one `SKILL.md`. Loading and discovery live
//! in `forge-agents::skill_loader`; this module contains only the data model
//! and `SkillId` validation so `forge-core` callers (the roster, IPC) can
//! reference skills without pulling in the parser dependency.
//!
//! See `docs/architecture/skills.md` for the format, scopes, and load order.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use ts_rs::TS;

/// Identifier for a [`Skill`], unique within its load scope.
///
/// Validated to be a non-empty string with no path separators (`/` or `\`)
/// and no leading dot. Skills are user-named (the parent folder name in
/// `.agent-skills/<name>/SKILL.md`), unlike random-hex IDs in [`crate::ids`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(
    export,
    export_to = "../../../web/packages/ipc/src/generated/",
    type = "string"
)]
#[serde(try_from = "String", into = "String")]
pub struct SkillId(String);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SkillIdError {
    #[error("skill id must not be empty")]
    Empty,
    #[error("skill id must not contain a path separator: {0:?}")]
    PathSeparator(String),
    #[error("skill id must not start with '.': {0:?}")]
    LeadingDot(String),
    #[error("skill id must not contain whitespace: {0:?}")]
    Whitespace(String),
}

impl SkillId {
    /// Construct a [`SkillId`] from a string, validating shape.
    ///
    /// Rejects empty strings, ids containing `/` or `\`, ids starting with
    /// `.`, and ids containing any ASCII whitespace. These constraints
    /// exist because the id is the directory name on disk under `.agent-skills/` —
    /// accepting any of the rejected forms would let a hostile or careless
    /// skill folder traverse out of the scope root, or render confusingly
    /// in CLI/UI surfaces (a trailing space is invisible at a glance).
    pub fn new(s: impl Into<String>) -> Result<Self, SkillIdError> {
        let s = s.into();
        if s.is_empty() {
            return Err(SkillIdError::Empty);
        }
        if s.contains('/') || s.contains('\\') {
            return Err(SkillIdError::PathSeparator(s));
        }
        if s.starts_with('.') {
            return Err(SkillIdError::LeadingDot(s));
        }
        if s.chars().any(|c| c.is_ascii_whitespace()) {
            return Err(SkillIdError::Whitespace(s));
        }
        Ok(Self(s))
    }

    /// Borrow the id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SkillId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for SkillId {
    type Error = SkillIdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<SkillId> for String {
    fn from(id: SkillId) -> Self {
        id.0
    }
}

/// Parsed shape of one `SKILL.md` file.
///
/// `id` is the parent directory name (e.g. `forge-milestone-planner` for
/// `.agent-skills/forge-milestone-planner/SKILL.md`). `prompt` carries the
/// markdown body with the YAML frontmatter stripped. `tools` is an optional
/// list of tool-binding hints surfaced from frontmatter; the loader does
/// not enforce their meaning — agents that consume the skill decide.
///
/// `source_path` points at the `SKILL.md` file on disk. Useful for error
/// messages and for the catalog UI to surface "edit this skill" links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub id: SkillId,
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub prompt: String,
    pub tools: Vec<String>,
    pub source_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_id_accepts_simple_names() {
        assert!(SkillId::new("foo").is_ok());
        assert!(SkillId::new("forge-milestone-planner").is_ok());
        assert!(SkillId::new("a_b_c").is_ok());
    }

    #[test]
    fn skill_id_rejects_empty() {
        assert_eq!(SkillId::new(""), Err(SkillIdError::Empty));
    }

    #[test]
    fn skill_id_rejects_path_separators() {
        assert!(matches!(
            SkillId::new("foo/bar"),
            Err(SkillIdError::PathSeparator(_))
        ));
        assert!(matches!(
            SkillId::new("foo\\bar"),
            Err(SkillIdError::PathSeparator(_))
        ));
        assert!(matches!(
            SkillId::new("../escape"),
            Err(SkillIdError::PathSeparator(_))
        ));
    }

    #[test]
    fn skill_id_rejects_leading_dot() {
        assert!(matches!(
            SkillId::new(".hidden"),
            Err(SkillIdError::LeadingDot(_))
        ));
    }

    #[test]
    fn skill_id_rejects_whitespace() {
        // Trailing space, leading space, internal space, tab, newline.
        for bad in ["planner ", " planner", "plan ner", "plan\tner", "plan\nner"] {
            assert!(
                matches!(SkillId::new(bad), Err(SkillIdError::Whitespace(_))),
                "expected whitespace rejection for {bad:?}",
            );
        }
    }

    #[test]
    fn skill_id_serde_roundtrip() {
        let id = SkillId::new("planner").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"planner\"");
        let back: SkillId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn skill_id_serde_rejects_invalid_wire_value() {
        let err = serde_json::from_str::<SkillId>("\"foo/bar\"").unwrap_err();
        assert!(
            err.to_string().contains("path separator"),
            "deserialization should reject invalid ids: {err}"
        );
    }

    #[test]
    fn skill_id_display_round_trips_value() {
        let id = SkillId::new("planner").unwrap();
        assert_eq!(id.to_string(), "planner");
        assert_eq!(id.as_str(), "planner");
    }
}
