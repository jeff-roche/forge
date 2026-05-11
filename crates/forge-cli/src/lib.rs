pub mod display;
pub mod mcp;
pub mod skill;
pub mod socket;
pub mod spawn;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Clap `value_parser` for `<session-id>` arguments.
///
/// Enforces the canonical `SessionId` wire format (`^[0-9a-f]{16}$`) *at
/// parse time* — before any command body runs — so attacker-controlled ids
/// like `../../tmp/evil` can never reach `socket::pid_path` or
/// `socket::socket_path`. See F-057 (T12a, path-traversal vector for raw-PID
/// SIGTERM).
///
/// Returning `Err(String)` here produces a typed clap error message of the
/// form `invalid value '<id>' for '<session-id>': ...`, which is what the
/// DoD calls a "typed validation error".
fn parse_session_id(raw: &str) -> Result<String, String> {
    if socket::session_id_is_valid(raw) {
        Ok(raw.to_string())
    } else {
        Err(format!(
            "session id must be 16 lowercase hex characters (got {:?})",
            raw
        ))
    }
}

#[derive(Parser, Debug)]
#[command(name = "forge", about = "Forge IDE command-line interface")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Manage forge sessions.
    Session {
        #[command(subcommand)]
        cmd: SessionCommands,
    },
    /// One-shot ephemeral agent run.
    Run {
        #[command(subcommand)]
        cmd: RunCommands,
    },
    /// Manage MCP (Model Context Protocol) servers.
    Mcp {
        #[command(subcommand)]
        cmd: McpCommands,
    },
    /// Manage skills (agentskills.io packs).
    Skill {
        #[command(subcommand)]
        cmd: SkillCommands,
    },
}

#[derive(Subcommand, Debug)]
pub enum SessionCommands {
    /// Start a new session.
    New {
        #[command(subcommand)]
        kind: SessionNewKind,
    },
    /// List known sessions and their state.
    List,
    /// Stream events from a session to stdout.
    Tail {
        /// Session ID to tail (16 lowercase hex characters).
        #[arg(value_parser = parse_session_id)]
        id: String,
    },
    /// Send SIGTERM to a session process.
    Kill {
        /// Session ID to kill (16 lowercase hex characters).
        #[arg(value_parser = parse_session_id)]
        id: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum SessionNewKind {
    /// Start an agent session.
    Agent {
        /// Agent name (must exist in .agents/).
        name: String,
        /// Workspace root directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Provider spec (e.g. "ollama:qwen2.5:0.5b" or "mock"). Falls back
        /// to FORGE_PROVIDER env, then MockProvider.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Start a bare provider session.
    Provider {
        /// Provider spec string.
        spec: String,
        /// Workspace root directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum RunCommands {
    /// Run an agent with a single input message and exit.
    Agent {
        /// Agent name.
        name: String,
        /// Input source: "-" reads from stdin, otherwise a file path.
        #[arg(long, default_value = "-")]
        input: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum McpCommands {
    /// Import MCP server definitions from another tool's config.
    ///
    /// Dry-run by default: prints a unified diff against the workspace's
    /// existing `.mcp.json`. Pass `--apply` to write.
    ///
    /// Auto source mode (`--source=auto`, the default) walks every known
    /// location; later sources in the list override earlier ones on name
    /// collision (ordered: vscode → cursor → claude → continue → kiro →
    /// codex).
    Import {
        /// Which source format to read. Omit or set to `auto` to walk all
        /// known locations.
        #[arg(long, default_value = "auto", value_parser = parse_import_source)]
        source: ImportSourceFlag,
        /// Write the converted config to `.mcp.json` instead of showing a
        /// diff.
        #[arg(long)]
        apply: bool,
        /// Workspace root. Defaults to the current working directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
}

/// CLI-visible form of [`forge_mcp::import::ImportSource`], plus an
/// explicit `Auto` variant so `--source=auto` is a first-class value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSourceFlag {
    Auto,
    Source(forge_mcp::import::ImportSource),
}

/// `forge skill` subcommands (F-590).
///
/// `install` accepts a CLI source string distinguished at runtime: anything
/// matching [`skill::looks_like_git_url`] is fetched via `git`, otherwise
/// it is treated as a local path. The `--target` flag selects the install
/// scope and defaults to `user`.
#[derive(Subcommand, Debug)]
pub enum SkillCommands {
    /// Install a skill from a git URL or a local path.
    Install {
        /// Source: HTTPS/SSH/git URL, or a local directory containing
        /// `SKILL.md`.
        source: String,
        /// Where to install the skill. Defaults to `user`.
        #[arg(long, default_value = "user", value_parser = parse_skill_scope)]
        target: SkillScopeFlag,
    },
    /// List installed skills (workspace + user scopes).
    List {
        /// Workspace root used to look up the workspace scope. Defaults to
        /// the current working directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Remove an installed skill.
    Remove {
        /// Skill id (folder name under `.skills/`).
        id: String,
        /// Which scope to remove from. Defaults to `workspace` if the id
        /// exists in both scopes, else the only scope it exists in.
        #[arg(long, value_parser = parse_skill_scope)]
        scope: Option<SkillScopeFlag>,
        /// Workspace root used to look up the workspace scope. Defaults to
        /// the current working directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
}

/// CLI-side wrapper over [`skill::SkillScope`] so the clap layer doesn't
/// pull `forge-agents` into its public surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScopeFlag {
    User,
    Workspace,
}

impl From<SkillScopeFlag> for skill::SkillScope {
    fn from(f: SkillScopeFlag) -> Self {
        match f {
            SkillScopeFlag::User => skill::SkillScope::User,
            SkillScopeFlag::Workspace => skill::SkillScope::Workspace,
        }
    }
}

fn parse_skill_scope(raw: &str) -> Result<SkillScopeFlag, String> {
    match raw {
        "user" => Ok(SkillScopeFlag::User),
        "workspace" => Ok(SkillScopeFlag::Workspace),
        other => Err(format!(
            "unknown skill scope {other:?}; expected `user` or `workspace`"
        )),
    }
}

fn parse_import_source(raw: &str) -> Result<ImportSourceFlag, String> {
    if raw == "auto" {
        return Ok(ImportSourceFlag::Auto);
    }
    forge_mcp::import::ImportSource::from_slug(raw)
        .map(ImportSourceFlag::Source)
        .ok_or_else(|| {
            format!(
                "unknown import source {raw:?}; expected one of: auto, vscode, cursor, claude, continue, kiro, codex"
            )
        })
}
