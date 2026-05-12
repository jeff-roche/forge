#![deny(rustdoc::broken_intra_doc_links, rustdoc::private_intra_doc_links)]
//! `forge-agents` — agent definitions, the `.agents/*.md` loader, and the
//! runtime orchestrator.
//!
//! The loader merges workspace and user-home agent definitions; the runtime
//! orchestrator instantiates them into live [`AgentInstance`]s and forwards
//! their lifecycle on a broadcast stream. See:
//!
//! - `docs/architecture/crate-architecture.md` §3.4 for the design
//! - `docs/design/ai-patterns.md` for the UX vocabulary

mod def;
mod error;
pub mod memory;
mod orchestrator;
pub mod skill_loader;

use std::{fs, path::Path};

/// Maximum byte size permitted for `AGENTS.md` injection.
///
/// Files larger than this cap are refused with [`Error::AgentsMdTooLarge`] rather
/// than read in full. 256 KiB is large enough for any reasonable workspace
/// instruction file while preventing unbounded token consumption from a
/// hostile or accidentally oversized file.
pub const AGENTS_MD_SIZE_CAP: u64 = 256 * 1024; // 256 KiB

pub use def::{validate_agent_name, AgentDef, Isolation, MAX_AGENT_NAME_BYTES};
pub use error::{Error, Result};
pub use memory::{
    assemble_system_prompt, Memory, MemoryFrontmatter, MemoryStore, WriteMode,
    MEMORY_ENVELOPE_CLOSE, MEMORY_ENVELOPE_OPEN, MEMORY_HEADING, MEMORY_WRITE_CONTENT_CAP,
};
pub use orchestrator::{
    AgentEvent, AgentInstance, AgentScope, InitialPrompt, InstanceState, Orchestrator, SpawnContext,
};
pub use skill_loader::{
    load_skills, load_user_skills, load_workspace_skills, parse_skill_file, SKILL_FILENAME,
};

use def::load_from_dir;

/// Canonical name of the built-in default agent.
///
/// The dashboard's new-session picker and the CLI's `--agent` flag both
/// default to this name so the user always has a usable selection even
/// before they author their first `.agents/*.md` file. Users can override
/// the built-in by dropping a definition with the same name into
/// `<user_home>/.agents/` or `<workspace>/.agents/` — the standard
/// workspace-over-user-over-builtin precedence applies.
pub const FORGE_DEFAULT_AGENT_NAME: &str = "forge-default";

/// In-memory definition for the [`FORGE_DEFAULT_AGENT_NAME`] built-in.
///
/// Plain `Process` isolation, no per-agent memory, empty body — the daemon
/// composes its system prompt from `AGENTS.md` and skill docs, so the
/// built-in only needs to exist in the roster for `agent_is_known` to
/// accept its name.
fn builtin_forge_default() -> AgentDef {
    AgentDef {
        name: FORGE_DEFAULT_AGENT_NAME.into(),
        description: Some("Default Forge agent — general-purpose assistant.".into()),
        body: String::new(),
        allowed_paths: Vec::new(),
        isolation: Isolation::Process,
        memory_enabled: false,
    }
}

/// Load agents from `<workspace_root>/.agents/*.md`, returning an empty vec if the directory is absent.
pub fn load_workspace_agents(workspace_root: &Path) -> anyhow::Result<Vec<AgentDef>> {
    load_from_dir(&workspace_root.join(".agents")).map_err(anyhow::Error::from)
}

/// Load agents from `<user_home>/.agents/*.md`, returning an empty vec if the directory is absent.
pub fn load_user_agents(user_home: &Path) -> anyhow::Result<Vec<AgentDef>> {
    load_from_dir(&user_home.join(".agents")).map_err(anyhow::Error::from)
}

/// Load and merge built-in, user-home, and workspace-local agent definitions.
///
/// Precedence (lowest → highest): built-ins, then user agents, then workspace
/// agents. On a name collision the higher-precedence definition replaces the
/// lower; agents present only at one tier are appended in load order. This
/// lets a project pin or override an agent without editing the user's home
/// directory, and lets a user override a built-in by dropping a file with
/// the matching name into `<user_home>/.agents/`.
pub fn load_agents(workspace_root: &Path, user_home: &Path) -> anyhow::Result<Vec<AgentDef>> {
    let workspace = load_workspace_agents(workspace_root)?;
    let user = load_user_agents(user_home)?;
    let mut merged: Vec<AgentDef> = vec![builtin_forge_default()];

    for next in user.into_iter().chain(workspace) {
        match merged.iter().position(|a| a.name == next.name) {
            Some(pos) => merged[pos] = next,
            None => merged.push(next),
        }
    }
    Ok(merged)
}

/// Read `<workspace_root>/AGENTS.md` if present, returning `Ok(None)` when the file is absent.
///
/// Returns [`Error::AgentsMdTooLarge`] if the file exceeds [`AGENTS_MD_SIZE_CAP`] bytes.
/// Callers should treat that error as "absent" (log a warning, skip injection) rather than
/// failing the session.
pub fn load_agents_md(workspace_root: &Path) -> Result<Option<String>> {
    let path = workspace_root.join("AGENTS.md");
    if !path.exists() {
        return Ok(None);
    }
    let metadata = fs::metadata(&path).map_err(anyhow::Error::from)?;
    let size = metadata.len();
    if size > AGENTS_MD_SIZE_CAP {
        return Err(Error::AgentsMdTooLarge {
            path,
            size,
            limit: AGENTS_MD_SIZE_CAP,
        });
    }
    let content = fs::read_to_string(&path).map_err(anyhow::Error::from)?;
    Ok(Some(content))
}

/// Bundle of merged agent definitions plus the optional workspace-level `AGENTS.md` preamble.
///
/// Constructed once per session via [`AgentLoader::load`] and then queried
/// through [`AgentLoader::agents`] and [`AgentLoader::agents_md`].
pub struct AgentLoader {
    agents: Vec<AgentDef>,
    agents_md: Option<String>,
}

impl AgentLoader {
    /// Load workspace + user agents and the workspace `AGENTS.md` in one pass.
    ///
    /// # Examples
    ///
    /// Point the loader at empty scratch roots — both `.agents/` dirs are
    /// absent, so the loader returns an empty bundle rather than failing:
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use forge_agents::AgentLoader;
    ///
    /// # fn example() -> anyhow::Result<()> {
    /// let loader = AgentLoader::load(
    ///     Path::new("/path/to/workspace"),
    ///     Path::new("/path/to/home"),
    /// )?;
    /// assert!(loader.agents().is_empty());
    /// assert!(loader.agents_md().is_none());
    /// # Ok(()) }
    /// ```
    pub fn load(workspace_root: &Path, user_home: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            agents: load_agents(workspace_root, user_home)?,
            agents_md: load_agents_md(workspace_root)?,
        })
    }

    /// Borrow the merged agent definitions, ordered user-first then workspace-only appended.
    pub fn agents(&self) -> &[AgentDef] {
        &self.agents
    }

    /// Borrow the workspace `AGENTS.md` contents, or `None` if the file was absent.
    pub fn agents_md(&self) -> Option<&str> {
        self.agents_md.as_deref()
    }
}
