# Architecture Overview

> Extracted from CONCEPT.md — tech stack rationale, full dependency matrix, and configuration conventions

---

## 7. Tech stack — Rust + Tauri with Monaco

Forge commits to a **Rust + Tauri shell with Monaco running in the webview for editing**.

### 7.1 The shape of the commitment

- **Rust** owns: session lifecycle and IPC, CLI, sandboxing, provider integration, agent orchestration, MCP, sub-agent spawning, container management, terminal VT.
- **Tauri 2** provides the shell: window management, webview hosting, OS integration, small binary size.
- **Webview (Solid app)** hosts: Monaco editor, the UI chrome (dashboard, session panes, catalog, usage), xterm-equivalent terminal rendering against Ghostty VT byte streams.
- **Monaco** is the editor widget. Runs in an iframe inside the webview to keep its globals and web workers isolated from our app code.

### 7.2 Why this shape

The crown-jewel features of Forge — session-as-process, CLI parity, multi-provider streaming, sandboxing, agent orchestration, transparent tool calls — are all non-editor concerns. They're Rust-native.

The editor itself is commodity infrastructure. Monaco is the best-in-class open code editor, MIT-licensed, maintained by Microsoft with guaranteed long-term availability (VS Code depends on it). Users arrive familiar with its behaviour from a decade of VS Code and GitHub's web IDE. Shipping with Monaco means editing feels complete from day one rather than sparse.

### 7.3 The editor is a leaf, not a load-bearing component

The boundary between Forge's shell and its editor is deliberately well-defined: a webview postMessage protocol that mirrors our IPC types. This means the editor is the one piece of Forge most easily replaceable. If Monaco's constraints start to bite — licensing changes, performance limits, or a Rust-native editor emerges as a stable library option — we can swap it without disturbing the rest of the system. This isn't a concrete v1.x roadmap item; it's architectural hygiene that keeps the option open.

### 7.4 Helix and Zed, briefly

`helix-core` is not currently published as a stable embedding library — the `helix-editor` crate on crates.io is a placeholder. Zed's editor core is GPL-3.0, which wouldn't fit our licensing. A truly-Rust-native editor built from primitives (`ropey` + `tree-sitter` + `tower-lsp`) would be a 6+ month project with no clear return since the editor isn't where Forge differentiates. Monaco ships.

---

## 8. Tech stack — everything else

| Concern | Choice |
|---|---|
| **Core lang** | Rust, stable toolchain |
| **Shell** | Tauri 2 |
| **Editor widget** | Monaco (MIT) in a webview iframe |
| **Frontend framework** | Solid — fine-grained reactivity fits Forge's streaming UI model |
| **Terminal backend** | `portable-pty` for PTY lifecycle + `libghostty-vt` for authoritative VT state, wired through `crates/forge-term` |
| **Terminal rendering** | xterm.js-compatible byte stream, rendered by our Solid app; Rust-side VT state is queryable via `TerminalSession::cursor_position`, `total_rows`, and `scrollback_rows` when `forge-term`'s `ghostty-vt` feature is enabled |
| **Provider SDKs** | `async-openai`, direct Anthropic SSE impl, OpenAI-compat for Ollama/Mistral/etc. |
| **MCP** | `rmcp` (official Rust MCP SDK) |
| **Agent protocol** | Native impl, MCP-compatible tool patterns |
| **Persistence** | Filesystem only — JSONL event logs, TOML config files. No SQLite. |
| **IPC (GUI ↔ session)** | Unix domain sockets + length-prefixed JSON (named pipes on Windows, post-v1) |
| **OCI** | `oci-spec-rs`, shell to `podman`/`docker` |
| **LSP** | `monaco-languageclient` in the webview, 16 bundled servers, user extensions via `.forge/languages.toml` |
| **Package** | `cargo` workspace, `pnpm` for the webview app |
| **Distribution** | Tauri bundler for Mac/Linux (x86_64 + arm64); Windows via WSL for v1 |

### 8.1 Platform support

**v1.0:**
- macOS 12+ (arm64 + x86_64) — native
- Linux glibc 2.31+ (arm64 + x86_64) — native
- Windows — via WSL2 running the Linux build. Documented install path, officially supported.

**v1.3:** native Windows (named pipes for IPC, Windows-native sandboxing via job objects).

### 8.2 Persistence — no database, files only

Forge uses the filesystem for everything persistent. No SQLite, no embedded DB.

```
~/.config/forge/
  config.toml              # user-global settings
  workspaces.toml          # known-workspaces registry
  credentials.toml         # keychain references (actual secrets in OS keychain)

~/.agents/<name>.md        # user-global agent definitions
~/.skills/<name>/SKILL.md  # user-global skills (agentskills.io format)
~/.mcp.json                # user-global MCP servers (universal standard)

<workspace>/
  AGENTS.md                # shared workspace instructions (cross-tool convention)
  .mcp.json                # workspace MCP servers (universal standard)
  .agents/<name>.md        # workspace agent definitions
  .skills/<name>/SKILL.md  # workspace skills
  .forge/                  # internal cache, self-gitignored
    sessions/<id>/
      meta.toml            # session metadata (id, agent, persistence, timestamps)
      events.jsonl         # append-only event log (first line: schema header)
      snapshots/           # optional speed-up snapshots; recomputable from events
    layouts.json           # per-workspace pane layout memory
    languages.toml         # user-extended LSP registry
    .gitignore             # internal, excludes everything from git
```

Cross-workspace queries (like usage across all projects) scan `.forge/sessions/*/events.jsonl` across known workspaces, or read from a lightweight monthly aggregate — the exact strategy is deferred to phase 3 when we have real usage-volume signal.

### 8.3 Event log schema versioning

Every `events.jsonl` file opens with a schema header as its first line:

```jsonl
{"schema_version": 1}
{"t": "SessionStarted", "seq": 1, "at": "...", ...}
{"t": "UserMessage", "seq": 2, ...}
...
```

Forge refuses to read a file without a recognized schema version. Future schema bumps come with migration functions that run at session open when needed.

### 8.4 Credentials

- macOS: Keychain via `security-framework`
- Linux: Secret Service via `secret-service` crate (GNOME Keyring, KDE Wallet)
- Fallback: `age`-encrypted file with passphrase prompt

Never plain-text credentials on disk.

### 8.5 Background agents — sidecar processes

Background agents and sub-agents do not run as in-process tokio tasks inside `forged`. Each `AgentInstanceId` runs in a dedicated `forged-agent` child process supervised by the daemon over a per-instance UDS, so a panicking provider stream cannot take the daemon down and `ResourceMonitor` reads real per-agent CPU/RSS from `/proc/<pid>/stat`. The daemon retains authority — credentials, persistence, MCP, the event log, and the shell-facing UDS all live in `forged`; sidecars only drive the per-turn provider request loop. Default-on; opt out with `FORGE_AGENT_SIDECAR=0`. See `docs/architecture/agent-sidecar.md` for the full design.

---

## 9. Configuration — standard conventions

Forge reads the conventions of the ecosystem it drops into. No `forge.json`. No `.forgerc`. The files it writes or consumes are either community-standard or ecosystem-neutral.

### 9.1 Workspace files

| File | Purpose | Standard / convention |
|---|---|---|
| `AGENTS.md` | Shared workspace instructions auto-injected into every agent's system prompt | Cross-tool convention (Claude Code, Cursor, Aider) |
| `.mcp.json` | MCP server declarations, `mcpServers` schema | Proposed universal standard (MCP repo discussion #2218) |
| `.agents/<name>.md` | Agent definitions, YAML frontmatter + prose | Tool-neutral location |
| `.skills/<name>/SKILL.md` | Skills, folder-per-skill | [agentskills.io](https://agentskills.io) open standard |
| `.editorconfig`, `.gitignore` | Already respected | Industry standards |
| `Makefile`, `justfile` | Surfaced in command palette | Industry standards |

### 9.2 User-global files

| Path | Purpose |
|---|---|
| `~/.mcp.json` | User-global MCP servers |
| `~/.agents/<name>.md` | User-global agent definitions |
| `~/.skills/<name>/SKILL.md` | User-global skills |
| `~/.config/forge/config.toml` | Forge settings (window mode, notifications, keybinds, etc.) |
| `~/.config/forge/workspaces.toml` | Known-workspaces registry |
| `~/.config/forge/credentials.toml` | Keychain references only |
| `~/.config/forge/memory/<agent>.md` | Opt-in cross-session memory (§10.5) |

The tool-neutral `~/.agents/`, `~/.skills/`, `~/.mcp.json` paths follow the universal-standard proposal and make configuration portable to other agent tools that adopt the same paths. `~/.config/forge/` holds only things that are genuinely Forge-specific.

### 9.3 `.forge/` is never user-authored

The `.forge/` directory is internal cache — transcripts, layouts, session metadata, LSP extensions. Forge writes a `.gitignore` *inside* `.forge/` that excludes everything, so it's invisible to git by default without modifying the user's top-level `.gitignore`. Users never directly edit files inside `.forge/`.

### 9.4 No Forge-specific fields in shared config files

Forge does not introduce its own frontmatter keys into `SKILL.md` or `.agents/*.md`. If we need something the standard doesn't express, we either:
1. Propose it to the upstream spec, or
2. Store it Forge-side in `~/.config/forge/config.toml`, keyed by skill/agent name

This keeps skills and agents fully portable across tools.

### 9.5 Cross-tool MCP import

```
forge mcp import --from vscode   # or: cursor, claude-desktop, continue, kiro, codex
```

Reads the source tool's MCP config from its standard location, converts to the universal `.mcp.json` schema, writes to workspace root (or user-global with `--user`). One-shot migration utility. Forge runtime only reads `.mcp.json` / `~/.mcp.json`.
