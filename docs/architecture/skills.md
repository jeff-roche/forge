# Skills

> Format, scopes, and load order for `SKILL.md` files in Forge. Source of truth: `crates/forge-core/src/skill.rs` (types) and `crates/forge-agents/src/skill_loader.rs` (parser + discovery).

## 1. What a skill is

A **skill** is a reusable capability pack — prompts, system instructions, optional tool-binding hints, example I/O — that an agent can load. Skills are distinct from agents: an agent has identity, isolation, and a default provider; a skill is content that an agent (or the bare-provider path) loads on demand.

Forge follows the [agentskills.io](https://agentskills.io) open standard. A skill is a folder containing a `SKILL.md` file (YAML frontmatter + markdown body), optionally with `scripts/` and `references/` subdirectories. The format is cross-vendor; skills authored for Forge work in any agentskills.io-compatible tool, and vice versa.

## 2. On-disk layout

```
<scope_root>/.agent-skills/<skill-id>/SKILL.md
                                      /scripts/    (optional, not loaded by Forge)
                                      /references/ (optional, not loaded by Forge)
```

The folder name is the canonical `SkillId`. It must be non-empty, must not contain `/` or `\`, must not start with `.`, and must not contain ASCII whitespace (space, tab, newline). These rules are enforced at parse time — folders that violate them are rejected with a typed error rather than silently mapped to a different id.

Forge currently loads only `SKILL.md`. The companion `scripts/` and `references/` subdirectories are reserved for future phases (skill-as-tool execution, retrieval-augmented prompting); they are not parsed today.

## 3. Frontmatter format

```markdown
---
name: Planner
version: 1.2.0
description: Breaks a feature request into ordered tasks
tools:
  - shell
  - fs.read
---

You are a planning skill. Given a feature description, produce ordered tasks.
...
```

Recognized fields:

| Field | Type | Required | Notes |
|---|---|---|---|
| `name` | string | no | Display name. Defaults to the folder name (`SkillId`) when absent. |
| `version` | string | no | Free-form. No semver enforcement at this layer. |
| `description` | string | no | One-line summary surfaced in the catalog UI. |
| `tools` | array of strings | no | Tool-binding hints. Forge does **not** enforce them today — agents that consume the skill decide what to do with them. Reserved for the F-591 roster + future tool-binding work. |

Frontmatter is optional. A `SKILL.md` with no `---` block is parsed as a body-only skill.

**Forward compatibility.** Unknown frontmatter fields are ignored (logged at `debug` for visibility). The agentskills.io spec is pre-v1.0 (target H2 2026); a strict parser would force a Forge release on every spec addition. Strictness is reserved for the trust-boundary configs (`approvals.toml`); skill metadata is treated as forward-compatible.

## 4. Scopes and load order

Skills load from two on-disk scopes plus an in-memory session overlay:

| Scope | Path | Mutability |
|---|---|---|
| **Workspace** | `<workspace_root>/.agent-skills/<id>/SKILL.md` | Per-project; checked into git by default |
| **User** | `<user_home>/.agent-skills/<id>/SKILL.md` | Per-user, cross-workspace |
| **Session overlay** | in-memory only | Per-session, applied on top of disk state at session start |

`<user_home>/.agent-skills/` is the universal-standard path described in `docs/architecture/overview.md` §9.2 — *not* `~/.config/forge/skills/`. The intent is that skills written for Forge are also discoverable by any other agentskills.io-compatible tool the user has installed.

> **Discrepancy note.** F-589's spec text reads "workspace (`.forge/skills/`), user (`~/.config/forge/skills/`)". Every other architecture document in the repo (overview.md, persistence.md, core-concepts.md) anchors on `.agent-skills/` and `~/.agent-skills/`, and that's what the agentskills.io standard prescribes. The implementation follows the architecture docs and the standard. If the spec text is later treated as authoritative we will rev the loader, but the standard-aligned path is the preferred convention.

### Precedence

`load_skills(workspace_root, user_home)` returns one entry per `SkillId`. On collision, **workspace shadows user**. This matches the precedence used by `load_agents` and lets a project pin or override a user-global skill without editing the user's home directory.

Session-overlay skills are not loaded from disk; they are inserted by the session orchestrator and shadow both workspace and user entries for the lifetime of the session. Overlay wiring lands with F-591 (roster) — the loader exposes the disk-derived `Vec<Skill>` it consumes.

### Determinism

The returned `Vec<Skill>` is sorted lexicographically by `SkillId`. Filesystem `read_dir` order is unspecified; sorting at the loader keeps successive calls over the same disk state byte-identical for snapshot tests, telemetry, and roster equality checks.

## 5. Errors

Loader-side errors surface as `forge_agents::Error::Other(anyhow::Error)`. The interesting cases:

- **Invalid `SkillId`** (folder name with `/`, `\`, leading `.`, or whitespace) — rejected with a clear message naming the offending folder.
- **Malformed YAML frontmatter** — the underlying `gray_matter` syntax / type error is wrapped with the offending file path. The skill is not loaded; the entire load aborts with that error so the caller sees the failure rather than a partial roster.
- **Frontmatter present but un-deserializable** — when the YAML parses but produces a value that doesn't fit the expected shape (e.g. a frontmatter block containing only a comment, which `gray_matter` returns as `Ok` with `data: None`), the loader rejects it explicitly rather than falling back to defaults. Body-only `SKILL.md` files (no `---` block at all) remain valid.
- **Unreadable directory entry** — a single broken `DirEntry` in `.agent-skills/` (stale NFS, EACCES on one folder) is logged at `warn` and skipped; other skills in the same scope continue to load. This is the one case where the loader is intentionally fault-tolerant.

All cases emit a `tracing::warn` event under target `forge_agents::skill_loader` so the Agent Monitor can surface them.

## 6. Public API

```rust
// crates/forge-core/src/skill.rs
pub struct Skill {
    pub id: SkillId,
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub prompt: String,
    pub tools: Vec<String>,
    pub source_path: PathBuf,
}

pub struct SkillId(/* String, validated */);

// crates/forge-agents/src/skill_loader.rs
pub fn parse_skill_file(path: &Path) -> Result<Skill>;
pub fn load_workspace_skills(workspace_root: &Path) -> Result<Vec<Skill>>;
pub fn load_user_skills(user_home: &Path) -> Result<Vec<Skill>>;
pub fn load_skills(workspace_root: &Path, user_home: &Path) -> Result<Vec<Skill>>;
```

All four functions return `forge_agents::Result<_>` (the typed crate error). Callers can pattern-match on `Error::Other(anyhow::Error)` for parse / IO failures or fall through to the existing `IsolationViolation` / `AgentsMdTooLarge` variants if a future revision starts emitting them.

`SkillId` round-trips as a JSON string on the wire (matching the existing id types in `crate::ids`) and exports to TypeScript as `export type SkillId = string`. Deserialization re-runs the validation rules so a malformed id from an IPC client is rejected at the boundary rather than at use.

## 7. What's *not* here yet

F-589 defines types, parsing, and discovery; F-590 adds installation. The following are explicitly deferred:

- **Roster integration** (F-591) — `RosterEntry::Skill { id }` referencing a loaded `Skill`.
- **Skill-as-tool execution** — running `scripts/` from a sandboxed agent.
- **Catalog UI** — scope-aware listing in the dashboard.
- **Tool-binding enforcement** — today the `tools:` field is a hint, not a constraint.

When those land they consume the types defined here; this module's surface should not change.

## 8. Installation (F-590)

`forge skill install` fetches a skill from a git URL or a local path and copies it into the target scope. Validation runs through [`forge_agents::skill_loader::parse_skill_file`] before anything is written to disk; a parse failure aborts the install with a clear error naming the offending field.

```sh
# Install from a local checkout into the user scope (default).
forge skill install ./path/to/skill

# Install from an HTTPS git URL into the workspace scope.
forge skill install https://github.com/example/my-skill.git --target workspace

# List installed skills across both scopes.
forge skill list

# Remove a workspace-scoped skill (default when the id exists in both scopes).
forge skill remove planner --scope workspace
```

### Resolvers

| Resolver | Recognized prefixes | Behavior |
|---|---|---|
| **Local path** | anything else | Canonicalizes the path, requires `SKILL.md` at the root, parses it, then copies the directory tree into `<scope>/.agent-skills/<id>/`. Side files (`scripts/`, `references/`) come along. |
| **Git** | `https://`, `http://`, `git://`, `ssh://`, `user@host:path` | Shells out to the system `git` binary. Clones to `<cache_root>/<sha256(url)>/`. Cache hits run `git fetch + reset --hard origin/HEAD` so re-installing pulls the latest commit. The repository's root must contain `SKILL.md`. |

The cache root is `~/.cache/forge/skills/`. The cache subdirectory name is the lowercase hex sha256 of the source URL.

### Why shell out to `git`?

We deliberately do not use `git2`. The skill-installation surface is rarely exercised, the dependency is heavy, and `git` is universally on PATH for any user authoring skills. Shell-out keeps the binary lean and the failure mode obvious (the user can re-run the failing command directly). The fallback path is to switch to `git2` if a future task needs in-process git operations.

### Refusal contract

Install refuses (and writes nothing to the target scope) when:

- the source path does not exist or is not a directory,
- the source has no `SKILL.md` at its root,
- the `SKILL.md` fails F-589 parse validation (invalid frontmatter, bad `SkillId`, etc.),
- a skill with the same id is already installed in the same scope (run `forge skill remove` first),
- the `git` binary is missing or the clone/fetch returns a non-zero exit code.
