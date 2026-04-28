# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- **Agent sidecar architecture is now default-on (F-608 follow-up).** The
  `FORGE_AGENT_SIDECAR` env var changes from an opt-in to an opt-out: empty,
  unset, `1`, `true`, or any other non-disable value keeps the per-instance
  `forged-agent` sidecar path enabled; `0`, `false`, or `off` (case-insensitive)
  falls back to the legacy in-process path. The production daemon
  (`forge-shell`) now wires a per-session `SidecarSupervisor` into every
  `BackgroundAgentRegistry`, so `start_background_agent` actually forks a
  `forged-agent` child by default. Sockets land under
  `${XDG_RUNTIME_DIR:-$TMPDIR}/forge/sidecars/<session-id>/` and the binary is
  resolved as a sibling of the shell executable (with PATH fall-back). The
  legacy in-process path will be removed one milestone after this flip.
  Architecture: `docs/architecture/agent-sidecar.md` §8.

### Added

- **Agent sidecar architecture (F-608).** Background agents and sub-agents now
  run in a per-instance `forged-agent` child process supervised by the daemon
  over a per-instance Unix domain socket, replacing the in-process tokio-task
  model. The daemon retains authority over credentials, persistence, MCP, the
  event log, and the shell-facing UDS; sidecars only drive the per-turn
  provider request loop. Crash isolation, bounded auto-restart (3 retries / 60
  s window), credential push, panic-hook crash dumps, and a criterion bench
  (`crates/forge-session/benches/sidecar_overhead.rs`) all land together.
  Measured on Linux x86_64: ~6 µs sidecar transport overhead per token and
  ~1.4 ms cold-start — well inside the 50 ms / 200 ms aspirations. Closes
  F-451: `ResourceMonitor` now receives real child PIDs and `Event::ResourceSample`
  reaches the AgentMonitor for every background instance. Originally gated
  opt-in via `FORGE_AGENT_SIDECAR=1`; flipped default-on in the post-soak
  follow-up listed under "Changed" above. Architecture:
  `docs/architecture/agent-sidecar.md`.

## [0.2.0] — Phase 2: Full Layout + MCP — 2026-04-26

Second milestone. A user can arrange the shell into a full multi-pane IDE
layout, edit code through an embedded Monaco editor backed by LSP, drive a
terminal with full VT semantics, browse workspace files, attach MCP servers
as tool sources, and watch an orchestrator drive sub-agents in parallel.
Approvals, settings, and layouts persist across sessions, and the command
palette makes every surface keyboard-discoverable. macOS is a first-class
CI target.

### Added

#### Pane layout and persistence
- Split-pane primitives (`SplitPane`, `GridContainer`) with resize and
  recursive nesting (F-117); drag-to-dock hook + drop-zone overlay for
  rearranging panes (F-118); minimum-width collapse rules so a pane never
  renders below its usable width (F-119); layout persistence to and
  restoration from `.forge/layouts.json` (F-120).
- Activity bar chrome + `FilesSidebar` component with a `tree` IPC for
  workspace navigation (F-126).
- `EditorPane` integrated into the `GridContainer` tree in place of the
  singleton editor slot (F-150).

#### Editor and language services
- `web/packages/monaco-host` iframe built on `monaco-languageclient`
  (F-121); `EditorPane` Solid component with `read_file` / `write_file` /
  `tree` IPC (F-122); frontend architecture §9.3 LSP-spawn model
  reconciled with the monaco-host parent-relay model (F-148).
- `forge-lsp` server discovery, download bootstrap, and stdio lifecycle
  (F-123).

#### Terminal
- `forge-term` crate wrapping ghostty-vt with PTY I/O and a byte stream
  interface (F-124); `TerminalPane` on `xterm.js` with PTY IPC for
  keyboard and resize (F-125); `libghostty-vt` wired as the authoritative
  Rust-side VT state with a zig-enabled CI path (F-146).

#### MCP
- `forge-mcp` crate skeleton + universal `.mcp.json` parser (F-127);
  stdio transport with JSON-RPC line framing (F-128); HTTP transport
  via SSE + long-poll over `reqwest` (F-129); `McpManager` lifecycle
  — start / stop / health-check / restart-backoff (F-130); `forge mcp
  import` six-source converter CLI (F-131); Tauri commands + tool-call
  routing through sessions (F-132); `McpManager` subprocess integration
  test on a serial CI job (F-154); unified shell + daemon `McpManager`
  so `toggle_mcp_server` affects running sessions (F-155).

#### Agents and sub-agents
- `AgentInstance` + orchestrator API in `forge-agents` (F-133); sub-agent
  spawning via the `agent.spawn` tool call with isolation enforcement
  (F-134); AGENTS.md auto-injection into the system prompt (F-135);
  sub-agent banners in `ChatPane` (F-136); session-orchestrator
  background-agent lifecycle + Tauri commands (F-137); status-bar
  background-agent indicator + completion notifications (F-138);
  fine-grained agent step events in `forge-session` (F-139).

#### Agent Monitor
- Three-column Agent Monitor view with Stop wired through (F-140);
  entry points from the command palette and a status-bar badge (F-153);
  backend resource monitor for cpu / rss / fds pills (F-152); macOS
  and Windows resource samplers for the pills (F-156).

#### Context picker and re-run variants
- `ContextPicker` component + `@`-trigger in the composer (F-141);
  context category resolvers + provider adaptation (F-142);
  context-picker spec §7 placement prose reconciled with the
  viewport-aware flip (F-147).
- Re-run Replace variant: truncate and regenerate (F-143); Re-run Branch
  variant: `branch_parent` threading + `BranchSelected` (F-144); Branch
  UI: variant selector, gutter indicator, metadata popover (F-145).

#### Settings and approvals
- User + workspace settings store with persistence and Tauri IPC
  (F-151).
- Persistent approval config at workspace and user level (F-036).

#### Shell chrome
- Command palette infrastructure (`Cmd/Ctrl+Shift+P`, registry API,
  fuzzy search) (F-157).

#### Platform
- macOS CI job: zig + cargo + rustdoc + tests + Tauri webview (F-158).

### Changed

- Sandbox PID limits now enforced via cgroup v2 `pids.max` for true
  per-sandbox ceilings, replacing the uid-wide `RLIMIT_NPROC` approach
  (F-149); sandbox NPROC uid-wide semantics documented with the
  cgroup-based per-sandbox limit plan (F-078).
- Supply-chain tooling consolidated onto a single `deny.toml` source,
  retiring the parallel `cargo-audit` path (F-115).

### Fixed

- Cfg-gate the `pid_file.rs` `/proc/self/stat` read for macOS so
  `forge-session` builds on non-Linux targets (F-338).
- Cfg-gate sandbox module exports at `forge-session` `lib.rs` to match
  the Linux-only module body (F-340).
- macOS XDG_RUNTIME_DIR fallback with documented onboarding so the UDS
  path resolves on macOS (F-339).
- Harden the `forge-cli` socket test against future `XDG_RUNTIME_DIR`
  readers by path-composing via dependency injection rather than
  mutating the environment (F-344).

## [0.1.0] — 2026-04-20 — Phase 1: Single Provider + GUI

First end-user-visible release. A user can launch Forge, start a session,
chat with a local Ollama model, watch tool calls stream in, and approve
or reject each tool invocation from the session window. No credentials
are required; provider auth is deferred to Phase 3.

### Added

#### Session orchestrator and providers
- `forge-session` daemon with a tool-call dispatcher that routes
  provider-requested tool calls through the approval flow and returns
  results to the streaming loop (F-028).
- `OllamaProvider` with streaming chat over NDJSON, tool-call parse path,
  and per-request model selection (F-021).
- Provider selection in `forged` via `--provider`, `FORGE_PROVIDER`, or
  `MockProvider` default; `session new agent --provider <spec>` matches
  at the CLI (F-038).
- Session archive on end with atomic rename into the on-disk archive
  directory, plus SIGTERM-triggered archival for persistent sessions
  (F-031, F-039).
- Rust → TypeScript event adapter that translates orchestrator events
  into the `SessionEvent` union consumed by the webview (F-037).

#### Filesystem tools
- `fs.read`, `fs.write`, and `fs.edit` with workspace-root path
  validation, atomic writes, and unified-diff apply (F-029).

#### Tauri shell, webview, and IPC
- `pnpm` workspace under `web/` with `@forge/app`, `@forge/ipc`, and
  `@forge/design` packages on Solid 1.9 + Vite 6 + strict TypeScript
  (F-018).
- `forge-shell` Tauri 2 binary that opens the Dashboard on launch and
  hosts the Solid app, gated behind an optional `webview` feature for
  headless unit tests (F-019).
- Tauri ↔ `forge-session` IPC bridge: five `#[tauri::command]` handlers
  (`session_hello`, `session_subscribe`, `session_send_message`,
  `session_approve_tool`, `session_reject_tool`) over UDS, backed by a
  process-wide `SessionConnections` registry (F-020).

#### Dashboard and session window
- Dashboard sessions list with filters and an Ollama status card
  (F-022, F-023).
- Session window shell, single-pane layout, and streaming chat
  rendering (F-024, F-025).
- Composer stays disabled through the full assistant turn and re-enables
  on stream finalisation or error (F-040).
- Tool call card with four-state `ToolCallStatus` enum, `batch_id`
  propagation, a `ToolCallFailed` event shape, and a one-line argument
  summary (F-026, F-041).
- Four-scope inline approval UI (Once / This file / This pattern /
  This tool) with R/A/F/P/T keyboard shortcuts and an inline pattern
  editor (F-027).

#### Process isolation
- Sandbox level 1 for `forge-session` child processes: seccomp filter,
  dropped capabilities, no-new-privs, and a minimal mount namespace
  (F-030).

#### CLI
- `forge` binary with `clap`-driven `list`, `tail`, `kill`, and `new`
  subcommands; strict `SessionId` format validation at parse time
  (F-057).

#### Documentation and supply-chain hygiene
- Phase 1 docs backlog: dashboard and pane-header UI specs (F-087);
  roadmap synced with the Phase 1 / Phase 3 scope split (F-032);
  documented DoS-ceiling semantics with a session-level aggregate byte
  budget (F-077); `ipc-contracts.md`, `crate-architecture.md`,
  `security.md`, `token-pipeline.md`, `scope.md`, and design-doc
  cross-links brought up to date, plus per-crate and per-web-package
  READMEs for 12 crates and 3 packages (F-094, F-095, F-096, F-100,
  F-101, F-102, F-103, F-104); rustdoc coverage added to `forge-agents`
  and `forge-providers` domain types (F-098, F-099); rustdoc warnings
  eliminated and guarded by `deny` lints (F-097).
- Supply-chain CI: `cargo audit` and `cargo deny check` on every PR,
  workspace licences declared `MIT OR Apache-2.0`, unmaintained
  transitive advisories tracked with explicit expiry in `deny.toml`,
  and pnpm `audit` mode in CI (F-070).

### Changed

- **Phase 1 performance audit:** wrapped `fs` tools in `spawn_blocking`
  (F-106), switched ID wrapper types and hot IPC fields to `Arc<str>`
  with a typed `IpcEvent` union (F-107, F-112), typed-deserialized
  Ollama NDJSON for an 8.8× drop in per-token allocations (F-108),
  released the `SessionConnections` lock before `write_frame().await`
  (F-109), swapped `std::fs::rename` for `tokio::fs::rename` in
  `archive_or_purge` (F-110), and compressed patch apply to a
  single-buffer O(1)-allocation path (F-111).
- **Quality refactors:** unified all TS IPC `invoke` call-sites behind
  a mockable wrapper (F-073); replaced silent fallbacks in
  `forge-session` IPC handlers with explicit typed errors (F-074);
  extracted shared tool-argument helpers (F-075); swapped `anyhow`
  wrapping in `Session::emit` for a typed `SessionError` (F-076);
  surfaced `invoke()` rejections in `ChatPane` and `SessionsPanel`
  (F-079); plus the Phase 1 quality-review debt backlog
  of six minor fixes (F-080).
- **Frontend polish and accessibility:** `ProviderPanel` probe-failure
  state (F-082); `:focus-visible` coverage on every button (F-083);
  button typography aligned with `component-principles` (F-084);
  `ApprovalPrompt` active-state transform (F-085), `role="alertdialog"`
  + `aria-live` (F-088), and focus trap / restoration (F-089);
  composer disabled state swapped to token-based darkening (F-086);
  remaining raw-hex and non-token pixel values tokenized (F-090);
  `PaneHeader` provider pill colors by active provider id (F-091);
  `SessionsPanel` pulse animation gated behind
  `prefers-reduced-motion` (F-092).

### Fixed

- `fs.write` preview no longer leaks the target file's prior contents
  (F-042).

### Security

Phase 1 security audit landed 26 hardening fixes before release,
grouped by trust boundary:

- **UDS trust boundary (`forge-session`):** refuse the `/tmp/forge-{uid}`
  fallback, require `XDG_RUNTIME_DIR`, chmod the bound socket to
  `0o600` and the parent directory to `0o700` (F-044); close the
  pre-unlink TOCTOU with a liveness probe and orphan-only unlink
  (F-056); scope `allowed_paths` to the session's workspace root
  (F-043); cap NPROC/NOFILE/FSIZE in the sandbox `pre_exec` (F-055);
  validate `shell.exec` cwd and surface it in previews (F-054); clamp
  `shell.exec` `timeout_ms` to 10 minutes (F-066); keep
  `SandboxedChild` live across timeout for clean reaping (F-047);
  record the client-supplied `ApprovalScope` faithfully without
  silent widening (F-053).
- **Tauri / webview boundary:** restrictive production CSP replacing
  the null policy (F-050); per-session IPC authorization via webview
  labels (F-051); drop webview-supplied `socket_path` from
  `session_hello` (F-052); scope `session:event` to the owning webview
  (F-062); validate session-id format before building window labels
  (F-063); narrow `session:event` payloads at the Rust → TS boundary
  (F-064); require the Dashboard label on `provider_status` (F-072);
  cap untyped-string fields on session commands (F-068); type
  `session_approve_tool` scope as an enum (F-069).
- **Provider transport:** three-level NDJSON framing bounds against
  DoS (F-045); connect/read timeouts on the Ollama `reqwest` client
  (F-046); validate `OLLAMA_BASE_URL` scheme and host (F-058); cap
  `list_models` response body at 1 MiB (F-059).
- **Storage and framing:** bound per-line reads in the event log and
  transcript (F-060); cap `fs.read`/`write`/`edit` byte sizes
  (F-061); `deny_unknown_fields` on on-disk meta and workspace
  records (F-065); bump `ToolCallId` to 128-bit entropy (F-067).
- **CLI:** reject non-positive pids before `libc::kill` (F-048);
  race-free `session_kill` via `pidfd` plus process start-time
  verification (F-049).

[Unreleased]: https://github.com/forge-ide/forge/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/forge-ide/forge/releases/tag/v0.1.0
