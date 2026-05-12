# Core Concepts

> Extracted from CONCEPT.md — the six vocabulary items that form the complete mental model of Forge

---

## 2. Core concepts

Six concepts form the vocabulary of Forge. Everything else is implementation.

### 2.1 Workspace
A directory on disk plus Forge-local metadata (`.forge/`). Workspaces map to projects. A workspace may contain any number of sessions. Standard ecosystem conventions are respected: git repo root, `.editorconfig`, `AGENTS.md`, `.mcp.json`, `Makefile`, `justfile`.

### 2.2 Session
A unit of agentic work scoped to a workspace. A session holds:
- A **layout** (the pane arrangement: chat, terminal, editor — composed via standard editor split semantics)
- A **roster** of enabled providers, skills, MCP servers, and agents (each scoped: session-wide, per-agent, or per-provider)
- A **transcript** (persistent, append-only event log; read-only from the user's perspective)
- An **environment** (working dir, env vars, optional OCI container, resource limits)

Sessions are **independently invokable**. `forge session new`, `forge session attach`, `forge session list`. They can run headless via `forge run`. Each session is its own OS process. This is the single most important architectural commitment in the product.

### 2.3 Provider
A backend that speaks a supported agent protocol. Anthropic and OpenAI are first-class built-ins; any OpenAI-compatible endpoint — including local model servers like Ollama, LM Studio, and vLLM — is configured as a `custom_openai` provider entry (the Add Provider modal ships a local-Ollama preset that auto-fills the endpoint, default model, and keyless auth). Providers have accent colors (per DESIGN.md) and credentials. Multiple providers can be active within one session. Providers can be invoked directly without defining an agent — `forge run provider anthropic/sonnet-4.5` is a legitimate way to start work.

### 2.4 Skill
A reusable capability pack — prompts, system instructions, tool bindings, example I/O — that an agent can load. Forge follows the [agentskills.io](https://agentskills.io) open standard: a folder with `SKILL.md` (YAML frontmatter + markdown body), optionally containing `scripts/` and `references/` subdirectories. The format is cross-vendor; skills built for Forge work in any agentskills.io-compatible tool, and vice versa.

### 2.5 MCP Server
An external tool/data source speaking Model Context Protocol. Declared in `.mcp.json` (workspace) or `~/.mcp.json` (user). Forge manages lifecycle: spawn, health-check, restart, sandbox. The config schema follows the emerging universal standard (`mcpServers` object keyed by name, with `command`, `args`, `env`, `url`, `headers`, `type`).

### 2.6 Agent
A named role definition (system prompt, allowed tools, allowed MCP servers, allowed skills, default provider, isolation level). Declared in `.agents/<name>.md` (workspace) or `~/.agents/<name>.md` (user-global) — markdown with YAML frontmatter for structured fields, prose body for the system prompt. Agents are the *specialized* path for invoking AI; the bare-provider path (§2.3) is the general path. Both are first-class.

**Note on `AGENTS.md`.** `AGENTS.md` at workspace root is a *shared workspace instructions* file, following the convention adopted by Claude Code, Cursor, Aider, and others. Forge auto-injects its contents into every agent's system prompt. It is *not* a location for agent definitions — those live in `.agents/`.

### Relationship sketch

```
Workspace
  └── Session (1..N, can be headless, can be ephemeral)
        ├── Roster: Providers × Skills × MCP servers × Agents
        │           (each entry has a scope: session-wide | agent:<name> | provider:<id>)
        ├── Layout: pane composition (chat | terminal | editor)
        ├── Transcript: append-only event log (read-only)
        └── Environment: cwd, env, container (optional), limits
```
