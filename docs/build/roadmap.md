# Roadmap

> Extracted from IMPLEMENTATION.md §12-13 — Phase 0-4, first sprint tickets F-000 to F-017, sprint totals, and definition of done

---

## 12. Phased roadmap

The sequencing that builds toward v1.0. Milestones are outcome-based.

### Phase 0 — Foundations (weeks 1–2) ✅ Complete
**Outcome.** A new user can run `forge session new` and get a working session process with IPC.
- Repo scaffold, workspace, CI, rustfmt/clippy baselines
- `forge-core`, `forge-ipc` skeletons with `ts-rs` working
- `forge-session` bin: UDS handshake, event log append with schema header, mock provider chat round-trip
- `forge-cli`: `session new agent`, `session new provider`, `session list`, `session tail`, `session kill`
- `workspaces.toml` and `meta.toml` readers/writers
- **Ships**: a CLI-only preview; no GUI.

### Phase 1 — Single provider, minimal GUI (weeks 3–6) ✅ Complete
**Outcome.** A user can open Forge, start a session, chat with a local Ollama model, and see tool calls. No credentials required (deferred to Phase 3).
- `forge-providers::OllamaProvider` with streaming chat (NDJSON over HTTP `/api/chat`, model discovery via `/api/tags`)
- `forge-shell` Tauri bin + webview bootstrap (Solid app)
- Dashboard view (sessions list + Ollama daemon status) — both active and archived filters

> **Post-Phase-3.1 note.** The dedicated `OllamaProvider` crate, the dashboard's Ollama status card, and the `ProviderKind::Ollama` session dispatch path were retired in Phase 3.1. Ollama is now configurable as a `custom_openai` preset (auto-filled endpoint + default model, keyless). The Phase-1 bullets above describe what shipped at the time and are preserved for the historical record.
- Session window with single chat pane (layout system exists but splits come later)
- Tool call cards, four-scope inline approval UI
- File read/write tool implementations, process isolation level 1
- Session archive on end for `persist` sessions

### Phase 2 — Full layout, MCP, agents (weeks 7–10)
**Outcome.** Full session surface; external capabilities via MCP; agents spawn sub-agents and run in background.
- Pane layout: splits H/V/grid, drag-to-dock, minimum-width adaptations
- Editor pane with Monaco in iframe, `monaco-languageclient` LSP
- Terminal pane with `forge-term` (ghostty-vt) and xterm.js-compatible rendering
- Files sidebar toggle
- `forge-mcp` with stdio + http transport, universal-standard `.mcp.json` schema, `forge mcp import`
- `forge-agents` with `.agents/*.md` parsing, `AGENTS.md` auto-injection, orchestrator, sub-agent banners
- Background agents
- Agent monitor view
- @-context picker
- Re-run (Replace + Branch variants with full branch UI)

### Phase 3 — Breadth (weeks 11–14)
**Outcome.** Multi-provider with credential management, skills, catalog, usage, containers.
- Anthropic, OpenAI, custom OpenAI-compat providers (SSE streaming, tool use)
- Credential management (per-provider API keys via OS keyring, Dashboard credential prompts, credential rotation)
- Skills loader (agentskills.io format, `forge skill install` from Git URL + local path)
- Catalog view (skills/MCP/agents manager with scope-aware entries)
- Usage view with charts, limits table, and cross-workspace aggregation
- `forge-oci` container support; Level 2 sandbox
- Context compaction (auto at 98%, user-invoked anytime)
- Parallel read-only tool calls
- Opt-in cross-session memory

### Phase 4 — Polish and v1.0 (weeks 15–18)
**Outcome.** Ships.
- LSP bootstrap for the full 16 default servers
- Markdown preview pane (pulldown-cmark renderer), sticky toggles
- Built-in static preview server (`forge preview`)
- Command palette (additional commands beyond the Phase-2 registry)
- Settings UI (surfaces on top of the Phase-2 settings store)
- Install packages for macOS and Linux (dmg, deb, appimage)
- Windows install documentation for WSL2 path
- Public docs site
- Telemetry (opt-in, local-first)
- Tabbed-window mode (non-default)

### v1.1+ (post-ship)
- Image attachments in composer
- Browser OAuth provider login
- Remote sessions (`forge session attach ssh://...`)
- HTTP tarball skill distribution
- Plugin marketplace

### v1.3
- Native Windows build (named pipes IPC, job-object sandboxing)

---

## 13. First sprint

Live task tracking is managed via [GitHub Issues](https://github.com/forge-ide/forge/issues). Phase 0 tickets (F-000–F-017) are tracked under the [Phase 0: Foundations](https://github.com/forge-ide/forge/milestone/1) milestone.
