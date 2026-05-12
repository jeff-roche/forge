# Forge — Agent Instructions

Forge is a native desktop workshop for agentic work: Rust + Tauri shell, SolidJS webview, Monaco in an isolated iframe. The unit of work is a **session**, not a file or workspace.

---

## Task tracking

Tasks are tracked as GitHub Issues on [forge-ide/forge](https://github.com/forge-ide/forge/issues). Before starting work, check the open issues. Milestones map to phases (Phase 0–4). Use `gh issue list --repo forge-ide/forge` to query current tasks.

---

## Mental model

- Think in **sessions**, not files. Every AI action is an event appended to an immutable log.
- Transparency is the product. Every tool call, approval, and sub-agent spawn must be visible in the UI.
- CLI and GUI are siblings. No feature exists only in one. All logic routes through `forge-core`.

---

## Repository layout

```
crates/
  forge-core        canonical types, config, credentials trait, path utils
  forge-providers   Provider trait + Anthropic/OpenAI/CustomOpenAI impls, streaming
  forge-mcp         MCP server lifecycle, .mcp.json parsing
  forge-agents      agent def parsing, AGENTS.md injection, orchestration
  forge-session     forged binary — session process, event log, IPC server
  forge-oci         container runtime (shells to podman/docker)
  forge-fs          path-validated filesystem ops, diff/patch
  forge-lsp         LSP server install/update management
  forge-term        ghostty-vt integration
  forge-ipc         shared IPC types only; ts-rs derives generate TS
  forge-cli         forge binary, thin clap wrapper
  forge-shell       Tauri binary, bridges Tauri commands ↔ UDS
web/
  packages/app          SolidJS shell app
  packages/monaco-host  Monaco in isolated iframe
  packages/ipc          generated TS types + typed IPC client
  packages/design       design tokens
docs/               architecture, build, design, product, ui-specs
```

---

## Invariants — never violate these

- **No direct writes.** All filesystem writes go through `forge-fs`. The session dispatcher rejects direct writes.
- **No `isolation: trusted` in user agents.** Reject at parse time — reserved for built-in skills only.
- **Parallel tool calls only when the entire batch is `read_only: true`.**
- **Schema header required.** `forge-core` refuses any `events.jsonl` missing `{"schema_version": 1}` as its first line.
- **No SQLite.** Persistence is filesystem-only: JSONL event logs, TOML config, Markdown agent defs.
- **Generated TS types must stay in sync.** CI fails if `web/packages/ipc/src/generated/` drifts from Rust types. Run `./scripts/gen-ts-types.sh` after touching `forge-ipc`.
- **No GUI-only features.** Every user-facing capability must be reachable from the CLI.

---

## IPC — two boundaries, don't mix them

**Boundary 1: Webview ↔ Tauri shell** — Tauri commands (request/response) and `forge://event` emissions. Types in `forge-ipc/src/tauri.rs`.

**Boundary 2: Tauri shell ↔ session process** — Unix domain socket (UDS), length-prefixed JSON frames (u32 BE + UTF-8 JSON, max 4 MiB). Handshake: `Hello` → `HelloAck` → `Subscribe`. Multiple clients may attach; events replay from `since` seq on connect.

---

## Key types

| Type | Crate | Notes |
|------|-------|-------|
| `Event` | forge-core | Source of truth; state is recomputable from the log |
| `AgentDef` | forge-agents | Parsed from `.agents/*.md` frontmatter |
| `Isolation` | forge-core | `Trusted` (built-in only), `Process` (default), `Container(spec)` |
| `ChatChunk` | forge-providers | Streaming token from any provider |
| `Tool` | forge-core | Unified shape for MCP and provider tools |

---

## Build commands

Dev workflows are wrapped in a top-level `justfile`. Install once with
`cargo install just` (or `brew install just`, `apt install just`), then:

```bash
just                # list recipes
just dev            # full Rust + webview + Tauri loop (spawns Vite at :5173)
just build          # Rust workspace + full pnpm workspace
just bundle         # production Tauri installers; defaults to all formats
just bundle rpm     # narrow to one format (rpm, deb, appimage, dmg, msi, …)
just check          # fmt --check, cargo check, clippy -D warnings, rustdoc -D warnings, typecheck, token drift
just test           # cargo test --all + pnpm -r test
just smoke          # Phase 1 CLI-only UAT gate
just ts-check       # verify web/packages/ipc/src/generated/ is in sync
```

CI (`.github/workflows/ci.yml`) calls the same `just check-rust` / `test-rust`
/ `check-web` / `test-web` recipes, so green-locally == green-in-CI for
everything except the supply-chain actions (cargo deny, pnpm audit).

Raw commands if you prefer them:

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --check

pnpm install                   # from web/
pnpm --filter app dev          # Vite dev server (use `just dev` for full Tauri loop)
```

TS types under `web/packages/ipc/src/generated/` regenerate as a side effect
of `cargo build` via `ts-rs`'s `export_to` attribute — no separate script.
Touch anything in `forge-ipc` and rebuild to see the diff.

---

## Config file locations

| Path | Purpose |
|------|---------|
| `~/.config/forge/config.toml` | User settings |
| `~/.config/forge/workspaces.toml` | Known workspaces |
| `<workspace>/.forge/sessions/<id>/events.jsonl` | Session event log (append-only) |
| `<workspace>/.agents/*.md` | Workspace agent definitions |
| `<workspace>/AGENTS.md` | Injected into every agent's system prompt (this file) |
| `<workspace>/.mcp.json` | Workspace MCP servers |

Never write to `.forge/` directly — it is self-gitignored and managed by `forged`.
