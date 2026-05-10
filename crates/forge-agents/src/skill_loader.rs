//! agentskills.io skill loader.
//!
//! Parses one `SKILL.md` (YAML frontmatter + markdown body) into a
//! [`forge_core::Skill`] and discovers skills from workspace and user scopes
//! with deterministic precedence (workspace shadows user).
//!
//! Layout — folder-per-skill, matching the agentskills.io standard and
//! `docs/architecture/persistence.md`:
//!
//! ```text
//! <root>/.skills/<name>/SKILL.md     # workspace
//! <user_home>/.skills/<name>/SKILL.md # user
//! ```
//!
//! Loader does not enforce folder side-files (`scripts/`, `references/`);
//! only `SKILL.md` is required. Frontmatter fields beyond the documented
//! set are ignored with a `tracing::debug` so future agentskills.io
//! revisions don't break the loader.
//!
//! See `docs/architecture/skills.md` for the full format and load order.

use std::{collections::BTreeMap, fs, path::Path};

use anyhow::Context;
use forge_core::{Skill, SkillId, SkillIdError};
use gray_matter::{engine::YAML, Matter, ParsedEntity};
use serde::{de::IgnoredAny, Deserialize};

use crate::error::{Error, Result};

/// Filename Forge expects inside each `.skills/<name>/` folder.
pub const SKILL_FILENAME: &str = "SKILL.md";

/// YAML frontmatter shape for an agentskills.io `SKILL.md`.
///
/// `extra` collects every key not matched by the declared fields. Unknown
/// fields are preserved (forward-compat with future agentskills.io revisions)
/// but surfaced via `tracing::warn!` at load time so a typo like
/// `vesion: 1.0` cannot silently take effect.
#[derive(Deserialize, Default)]
struct Frontmatter {
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, IgnoredAny>,
}

/// Parse one `SKILL.md` file into a [`Skill`].
///
/// `id` is taken from the parent folder name. The frontmatter `name` field
/// is preserved separately as the human-readable display name; if absent
/// it defaults to the id.
pub fn parse_skill_file(path: &Path) -> Result<Skill> {
    let id_str = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            Error::Other(anyhow::anyhow!(
                "skill file must live in a named folder: {}",
                path.display()
            ))
        })?
        .to_string();

    let id = SkillId::new(id_str.clone()).map_err(|e: SkillIdError| {
        tracing::warn!(
            target: "forge_agents::skill_loader",
            path = %path.display(),
            error = %e,
            "rejected skill: invalid folder name as SkillId",
        );
        Error::Other(anyhow::anyhow!(
            "invalid skill id derived from folder {id_str:?}: {e}"
        ))
    })?;

    let raw = match fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
    {
        Ok(raw) => raw,
        Err(err) => {
            tracing::warn!(
                target: "forge_agents::skill_loader",
                path = %path.display(),
                error = %err,
                "failed to read skill file",
            );
            return Err(Error::from(err));
        }
    };

    let matter = Matter::<YAML>::new();
    let parsed: ParsedEntity<Frontmatter> = match matter
        .parse(&raw)
        .with_context(|| format!("parsing frontmatter in {}", path.display()))
    {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(
                target: "forge_agents::skill_loader",
                path = %path.display(),
                error = %err,
                "failed to parse YAML frontmatter",
            );
            return Err(Error::from(err));
        }
    };

    // Distinguish "no frontmatter at all" (legitimate body-only) from
    // "frontmatter present but failed to deserialize". `gray_matter`'s
    // `Matter::parse` returns `Ok` with `data: None` in the latter case
    // when the YAML parses to `Pod::Null` (e.g. a frontmatter block that
    // contains only a comment) — `deserialize_option` on `Pod::Null` visits
    // `None` rather than erroring. Falling through to `Frontmatter::default()`
    // would silently swallow that, violating the contract in
    // `docs/architecture/skills.md` §5.
    let fm = match parsed.data {
        Some(fm) => fm,
        None if parsed.matter.is_empty() => Frontmatter::default(),
        None => {
            tracing::warn!(
                target: "forge_agents::skill_loader",
                path = %path.display(),
                raw_matter = %parsed.matter,
                "frontmatter present but did not deserialize into a known shape",
            );
            return Err(Error::Other(anyhow::anyhow!(
                "frontmatter in {} did not deserialize into the expected shape",
                path.display()
            )));
        }
    };
    // Forward-compat: accept unknown fields silently from the loader's
    // perspective, but emit a warn per key so a YAML typo (e.g.
    // `desciption:` instead of `description:`) is observable in operator
    // logs rather than silently swallowed.
    for unknown in fm.extra.keys() {
        tracing::warn!(
            target: "forge_agents::skill_loader",
            path = %path.display(),
            unknown_field = %unknown,
            "unknown frontmatter field; ignored (forward-compat) — verify it is not a typo",
        );
    }
    let name = fm.name.unwrap_or_else(|| id.as_str().to_string());

    Ok(Skill {
        id,
        name,
        version: fm.version,
        description: fm.description,
        prompt: parsed.content,
        tools: fm.tools,
        source_path: path.to_path_buf(),
    })
}

/// Walk `<scope_root>/.skills/<name>/SKILL.md` for every immediate
/// subdirectory and parse each into a [`Skill`].
///
/// `<scope_root>` is the workspace root or the user home directory; the
/// loader appends `.skills/`. Subdirectories without a `SKILL.md` are
/// skipped silently. Result is sorted by [`SkillId`] for deterministic
/// output regardless of filesystem readdir order.
fn load_from_scope(scope_root: &Path) -> Result<Vec<Skill>> {
    let skills_dir = scope_root.join(".skills");
    if !skills_dir.exists() {
        return Ok(vec![]);
    }

    let entries = fs::read_dir(&skills_dir)
        .with_context(|| format!("reading {}", skills_dir.display()))
        .map_err(Error::Other)?;

    let mut skills: Vec<Skill> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                // Don't abort the whole scope on a single broken entry —
                // a stale NFS mount or a permission glitch on one folder
                // shouldn't make every other skill in the scope vanish.
                // We log loudly so the failure is observable.
                tracing::warn!(
                    target: "forge_agents::skill_loader",
                    dir = %skills_dir.display(),
                    error = %err,
                    "skipping unreadable directory entry",
                );
                continue;
            }
        };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_md = path.join(SKILL_FILENAME);
        if !skill_md.exists() {
            tracing::debug!(
                target: "forge_agents::skill_loader",
                folder = %path.display(),
                "skipping skill folder: no SKILL.md",
            );
            continue;
        }
        skills.push(parse_skill_file(&skill_md)?);
    }

    skills.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    Ok(skills)
}

/// Load skills from `<workspace_root>/.skills/`, returning an empty vec if
/// the directory is absent.
pub fn load_workspace_skills(workspace_root: &Path) -> Result<Vec<Skill>> {
    load_from_scope(workspace_root)
}

/// Load skills from `<user_home>/.skills/`, returning an empty vec if the
/// directory is absent.
pub fn load_user_skills(user_home: &Path) -> Result<Vec<Skill>> {
    load_from_scope(user_home)
}

/// Load and merge user-home and workspace-local skills.
///
/// Workspace skills shadow user skills on [`SkillId`] collision. The
/// returned `Vec` is sorted by id so successive calls over the same disk
/// state return identical ordering. This matches the precedence used by
/// [`crate::load_agents`] and is documented in `docs/architecture/skills.md`.
pub fn load_skills(workspace_root: &Path, user_home: &Path) -> Result<Vec<Skill>> {
    let user = load_user_skills(user_home)?;
    let workspace = load_workspace_skills(workspace_root)?;

    let mut by_id: BTreeMap<String, Skill> = BTreeMap::new();
    for s in user {
        by_id.insert(s.id.as_str().to_string(), s);
    }
    for s in workspace {
        by_id.insert(s.id.as_str().to_string(), s);
    }

    Ok(by_id.into_values().collect())
}
