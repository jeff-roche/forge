# IPC Contracts

> Extracted from IMPLEMENTATION.md §4-5 — the two IPC boundaries, Tauri commands, events, UDS framing, handshake, and full message types

---

## 4. IPC contracts

Forge has **two distinct IPC boundaries**. They must not be confused.

### 4.0 Response shape rule: inline-response vs event-driven

Every new IPC message — Boundary 1 (Tauri command) or Boundary 2 (UDS `IpcMessage` variant) — picks one of two response shapes. Pick by the **caller's wait semantics**, not by what feels symmetric.

| Caller semantic | Shape | Examples |
|-----------------|-------|----------|
| Caller needs a value back, or a synchronous ack that the operation completed | **Inline pair** — request variant + response variant carried on the same connection / awaited by the same Tauri command | `ListMcpServers` → `McpServersList`; `ToggleMcpServer` → `McpToggleResult`; `ImportMcpConfig` → `McpImportResult`; `InterruptSession` → `RefineHandoff`; every shipped Tauri command returning `Result<T, String>` (`read_file`, `tree`, `start_background_agent`, …) |
| Caller fires the command and observes the outcome through `session:event` (or another typed event channel) | **Event-driven** — request variant has no paired response; the daemon emits a `forge_core::Event` the webview is already subscribed to | `RerunMessage` → `Event::MessageRerun*`; `SelectBranch` → `Event::BranchSelected`; `DeleteBranch` → `Event::BranchDeleted`; `CompactTranscript` → `Event::ContextCompacted`; `PauseSession` / `ResumeSession` → `Event::SessionPaused` / `SessionResumed`; `SwitchProvider` (no event — pure side-effect on `SwappableProvider::swap`, in-flight turns finish on the previous provider) |

**Decision procedure for a new message:**

1. Does the caller need data the daemon computes (a list, a snapshot, a handoff payload)? → **Inline pair.** The response variant carries the data.
2. Does the caller need to know the operation failed in a way the event stream wouldn't surface (unknown server name, bad import slug)? → **Inline pair.** The response variant carries an `error: Option<String>` or `Result`.
3. Does the operation produce a `forge_core::Event` the webview is already subscribed to, and is the caller content to learn the outcome through that event? → **Event-driven.** Skip the response variant; the event *is* the contract.
4. Is the operation fire-and-forget with no observable outcome (a pure side-effect like `SwitchProvider`)? → **Event-driven.** Log on the daemon if the call is malformed; do not synthesize a response variant just to acknowledge receipt.

**Why the rule exists.** Without it, Phase 3 added inline pairs (`InterruptSession` → `RefineHandoff`) and event-driven commands (`PauseSession`, `RerunMessage`, `SwitchProvider`) under no consistent principle, and reviewers had to relitigate the choice each time. The rule above codifies the existing, working pattern so future messages slot in without a per-PR debate.

**Anti-patterns:**

- A response variant that carries no payload and no error — the caller learns nothing the underlying transport ack didn't already convey. Use event-driven instead.
- Emitting an event *and* returning an inline response carrying the same payload — pick one. The interrupt + refine flow is the one allowed exception (F-604) because two distinct subscribers (the Tauri command awaiting the IPC reply and any other webview / `forge session tail` consuming the event stream) need the same shape; the caller still only awaits the inline reply.
- An inline pair where the caller never blocks on the response — that's an event-driven flow disguised as a request/reply. Drop the response variant.

**Phase 3 audit (post-F-640).** Every message variant in `crates/forge-ipc/src/lib.rs::IpcMessage` matches the rule above:

| Variant | Shape | Rationale |
|---------|-------|-----------|
| `ListMcpServers` → `McpServersList` | inline pair | query, returns data |
| `ToggleMcpServer` → `McpToggleResult` | inline pair | sync ack with error path |
| `ImportMcpConfig` → `McpImportResult` | inline pair | sync ack with computed payload |
| `InterruptSession` → `RefineHandoff` | inline pair | returns captured partial text the caller needs synchronously |
| `RerunMessage` | event-driven | outcome surfaces through the rerun events on `session:event` |
| `SelectBranch` | event-driven | emits `Event::BranchSelected` |
| `DeleteBranch` | event-driven | emits `Event::BranchDeleted` |
| `CompactTranscript` | event-driven | emits `Event::ContextCompacted` |
| `PauseSession` / `ResumeSession` | event-driven | emit `Event::SessionPaused` / `SessionResumed` |
| `SwitchProvider` | event-driven | pure side-effect on `SwappableProvider`; no observable outcome the caller needs |

Boundary 1 (Tauri) is symmetric: every shipped `#[tauri::command]` returns `Result<T, String>` because Tauri's command machinery is request/reply by construction; the event-driven shape is encoded by *omitting* a command and emitting on the appropriate channel (`session:event`, `terminal:bytes`, etc.). The same decision procedure applies — when a webview interaction needs only an event, do not invent a Tauri command whose body returns `Ok(())`.

```
  ┌──────────────────────┐
  │  Webview (Solid)     │
  │  TypeScript          │
  └──────────┬───────────┘
             │  Boundary 1: Tauri commands + events
             │  (in-process, JSON over Tauri IPC)
  ┌──────────▼───────────┐
  │  forge-shell (Rust)  │
  │  Tauri host          │
  └──────────┬───────────┘
             │  Boundary 2: UDS + length-prefixed JSON
             │  (cross-process, multiple sessions)
  ┌──────────▼───────────┐    ┌───────────────────┐    ┌───────────────────┐
  │ forged session #1    │    │ forged session #2 │    │ forged session #3 │
  └──────────────────────┘    └───────────────────┘    └───────────────────┘
```

### 4.1 Boundary 1: Tauri ↔ webview

> **Shipped coverage.** **67 Tauri commands** are registered today across `crates/forge-shell/src/`. The session-bridge handler (`ipc.rs::build_invoke_handler`) registers the 63 turn-flow / filesystem / terminal / LSP / approval / settings / MCP / roster / context-fetch / credentials / containers / memory / providers / transcript commands; the production app builder in `window_manager::run` registers those plus the four dashboard-window commands (`provider_status`, `session_list`, `open_session`, `usage_summary`). The two registration sites must stay in lockstep until they are deduplicated. See §4.1.1 below for the full enumeration grouped by feature area, and ADR-001 §4 for the matching subset note on the UDS boundary.

**Pattern.** Tauri `command` handlers for request/response (webview → host) and Tauri `events` for push (host → webview).

#### 4.1.1 Shipped commands

Every command returns `Result<T, String>` on the wire; the `String` error carries a stable tag (e.g. `forbidden: window label mismatch`, `stop_background_agent: unknown instance`). Types are derived with `ts-rs` and regenerated into `web/packages/ipc/src/generated/`.

**Authz gate column.** Post-F-668, every IPC module routes through exactly two canonical helpers in `crates/forge-shell/src/ipc.rs`: `require_window_label` (strict, single-label) and `require_window_label_in` (allow-list + optional `allow_any_session` admission). A handful of dashboard/owner commands also hold an inline owner-label check after the helper call.

| Gate | Helper call | Meaning |
|------|-------------|---------|
| `session-{id}` | `require_window_label(&webview, "session-{session_id}", "<cmd>")` | Only the exact session webview that owns the `session_id` may invoke |
| `dashboard` | `require_window_label(&webview, "dashboard", "<cmd>")` | Only the dashboard window may invoke (post-F-668: strict helper, not the allow-list shape) |
| `dashboard + session-{id}` | `require_window_label("dashboard")` OR `require_window_label("session-{session_id}")` (inline, mutually exclusive) | Dashboard **or** the *specific* session's window may invoke; other `session-*` windows are rejected |
| `dashboard + session-*` | `require_window_label_in(&webview, &["dashboard"], true, "<cmd>")` | Dashboard **or** any `session-*` webview may invoke (`allow_any_session=true`) |
| `any session-*` | `require_window_label_in(&webview, &[], true, "<cmd>")` | Any `session-*` webview may invoke; dashboard is rejected |

**Session lifecycle** — the original Phase 1 set, registered in both call sites:

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `session_hello` | `ipc.rs` | `session_id: String` | `HelloAck { session_id, workspace, started_at, event_seq, schema_version }` | `session-{id}` |
| `session_subscribe` | `ipc.rs` | `session_id: String, since: Option<u64>` | `()` (events flow on `session:event`) | `session-{id}` |
| `session_send_message` | `ipc.rs` | `session_id: String, text: String` | `()` | `session-{id}` |
| `session_cancel` | `ipc.rs` | `session_id: String` | `()` | `session-{id}` |
| `session_approve_tool` | `ipc.rs` | `session_id: String, tool_call_id: String, scope: ApprovalScope` | `()` | `session-{id}` |
| `session_reject_tool` | `ipc.rs` | `session_id: String, tool_call_id: String, reason: Option<String>` | `()` | `session-{id}` |

**Pause / Resume / Interrupt / Switch** (F-603 / F-604 / F-640) — turn-control primitives layered on the orchestrator. See §5.7 (pause/resume) and §5.8 (interrupt+refine) for the UDS-side semantics.

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `session_pause` | `ipc.rs` | `session_id: String` | `()` | `session-{id}` |
| `session_resume` | `ipc.rs` | `session_id: String` | `()` | `session-{id}` |
| `session_interrupt_and_refine` | `ipc.rs` | `session_id: String` | `RefineHandoff { partial_text, captured_at_step_id, captured_at_msg_id }` | `session-{id}` |
| `session_switch_provider` | `ipc.rs` | `session_id: String, provider_id: String` | `()` | `session-{id}` |

**Turn-flow extensions** (reach the daemon via `SessionBridge`):

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `rerun_message` | `ipc.rs` | `session_id: String, msg_id: String, variant: RerunVariant` | `()` | `session-{id}` |
| `select_branch` | `ipc.rs` | `session_id: String, parent_id: String, variant_index: u32` | `()` | `session-{id}` |
| `delete_branch` | `ipc.rs` | `session_id: String, parent_id: String, variant_index: u32` | `()` | `session-{id}` |
| `compact_transcript` | `ipc.rs` | `session_id: String` | `()` (success surfaces on the event stream as `Event::ContextCompacted`) | `session-{id}` |

`RerunVariant` is `{ Replace, Branch, Fresh }` (see `forge-core/src/types.rs`).

**Persistent approvals** (F-036) — user/workspace scoped, read/written through `SessionBridge`:

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `get_persistent_approvals` | `ipc.rs` | `workspace_root: String` | `Vec<PersistentApprovalEntry>` | `dashboard + session-*` |
| `save_approval` | `ipc.rs` | `entry: ApprovalEntry, level: ApprovalLevel, workspace_root: String` | `()` | `dashboard + session-*` |
| `remove_approval` | `ipc.rs` | `scope_key: String, level: ApprovalLevel, workspace_root: String` | `()` | `dashboard + session-*` |

`ApprovalLevel` is `{ Session, Workspace, User }`. `PersistentApprovalEntry { scope_key, tool_name, label, level }` mirrors `forge-core::ApprovalEntry` with the level tag carried through.

**Terminal** (F-125) — each spawn is owned by the calling webview label; subsequent write/resize/kill from a different label are rejected:

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `terminal_spawn` | `ipc.rs` | `args: TerminalSpawnArgs { terminal_id, shell?, cwd, cols, rows }` | `()` | `any session-*` |
| `terminal_write` | `ipc.rs` | `terminal_id: TerminalId, data: Vec<u8>` | `()` | `any session-*` (+ owner-label check) |
| `terminal_resize` | `ipc.rs` | `terminal_id: TerminalId, cols: u16, rows: u16` | `()` | `any session-*` (+ owner-label check) |
| `terminal_kill` | `ipc.rs` | `terminal_id: TerminalId` | `()` | `any session-*` (+ owner-label check) |

Output bytes flow back via the `terminal:bytes` Tauri event (see §4.1 events).

**Layouts** (F-131) — single-file on-disk store at `.forge/layouts.json`; UI is dashboard-only:

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `read_layouts` | `ipc.rs` | `workspace_root: String` | `Layouts { active, named: HashMap<String, Layout> }` | `dashboard + session-*` |
| `write_layouts` | `ipc.rs` | `workspace_root: String, layouts: Layouts` | `()` | `dashboard + session-*` |

**Filesystem** (F-126 / F-143 / F-150) — session-scoped and routed through the session daemon so edits stay inside the sandboxed workspace root:

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `read_file` | `ipc.rs` | `session_id: String, path: String` | `FileContent { path, content, bytes, sha256 }` | `session-{id}` |
| `write_file` | `ipc.rs` | `session_id: String, path: String, bytes: Vec<u8>` | `()` | `session-{id}` |
| `tree` | `ipc.rs` | `session_id: String, root: String, depth: Option<u32>` | `TreeNodeDto { name, path, kind, children }` | `session-{id}` |
| `rename_path` | `ipc.rs` | `session_id: String, from: String, to: String` | `()` | `session-{id}` |
| `delete_path` | `ipc.rs` | `session_id: String, path: String` | `()` | `session-{id}` |

`TreeKindDto` is `{ File, Dir, Symlink, Other }`.

**LSP** (F-127) — caller resolves the binary (via `forge-lsp::Bootstrap::ensure`) before spawning; downloads stay outside the Tauri trust boundary. Server messages flow back via the `lsp_message` Tauri event.

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `lsp_start` | `ipc.rs` | `args: LspStartArgs { server, binary_path, args: Vec<String> }` | `()` | `any session-*` |
| `lsp_stop` | `ipc.rs` | `server: String` | `()` | `any session-*` |
| `lsp_send` | `ipc.rs` | `server: String, message: serde_json::Value` | `()` | `any session-*` |
| `lsp_list` | `ipc.rs` | (none) | `Vec<LspListEntry { id, state: LspStateInfo { state } }>` (filtered to caller's owner label) | `any session-*` |

**URL context fetch** (F-359) — server-side fetcher that replaces the webview's direct `fetch()`. The allowlist lives in `AllowedHostsState` on the Rust side; the dashboard's settings panel writes it via `set_context_allowed_hosts`, and `context_fetch_url` consults the snapshot on each call.

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `context_fetch_url` | `ipc.rs` | `session_id: String, url: String` | `FetchedUrl { body, status, content_type?, truncated }` | `session-{id}` |
| `set_context_allowed_hosts` | `ipc.rs` | `hosts: Vec<String>` | `()` | `dashboard + session-*` |

**Background agents** (F-137 / F-138) — quartet of lifecycle commands against the per-session `BgAgentRegistry`:

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `start_background_agent` | `ipc.rs` | `session_id: String, agent_name: String, prompt: String` | `String` (instance id) | `session-{id}` |
| `promote_background_agent` | `ipc.rs` | `session_id: String, instance_id: String` | `()` | `session-{id}` |
| `list_background_agents` | `ipc.rs` | `session_id: String` | `Vec<BgAgentSummary { id, agent_name, state }>` | `session-{id}` |
| `stop_background_agent` | `ipc.rs` | `session_id: String, instance_id: String` | `()` | `session-{id}` |

`BgAgentStateDto` is `{ Running, Completed, Failed }`.

**Settings** (F-151) — persistent user/workspace-scoped kv store; dashboard is the primary editor but session windows also read their effective values:

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `get_settings` | `ipc.rs` | `workspace_root: String` | `AppSettings { notifications, windows, … }` | `dashboard + session-*` |
| `set_setting` | `ipc.rs` | `key: String, value: serde_json::Value, level: SettingsLevel, workspace_root: String` | `()` | `dashboard + session-*` |

`SettingsLevel` is `{ User, Workspace }`.

**MCP** (F-132 / F-155) — session-scoped wrappers over `SessionBridge`'s `McpManager`. Server state transitions stream back via the session event log, not the command response.

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `session_list_mcp_servers` | `ipc.rs` | `session_id: String` | `Vec<McpServerInfo { name, state, tools }>` | `session-{id}` |
| `toggle_mcp_server` | `ipc.rs` | `session_id: String, name: String, enabled: bool` | `McpToggleResult { name, enabled_after, error }` | `session-{id}` |
| `import_mcp_config` | `ipc.rs` | `session_id: String, source: String, apply: bool` | `McpImportResult { source, imported, destination_path, error }` | `session-{id}` |

> F-591 renamed the original `list_mcp_servers` to `session_list_mcp_servers` so the new roster command (next section) could take the bare `list_mcp_servers` name with the spec-mandated `(workspace_root, scope)` signature.

**Roster discovery** (F-591) — read-only loaders that surface every discoverable resource (skills, MCP servers, agents, providers) and filter by [`RosterScope`](../../crates/forge-core/src/roster.rs). Distinct from F-132's session-scoped MCP commands: roster reads canonical sources (skill loader, agent loader, on-disk `.mcp.json`, hardcoded built-in providers + merged settings) and works without a live session.

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `list_skills` | `ipc.rs` | `workspace_root: String, scope: RosterScope` | `Vec<ScopedRosterEntry>` | `dashboard + session-*` |
| `list_mcp_servers` | `ipc.rs` | `workspace_root: String, scope: RosterScope` | `Vec<ScopedRosterEntry>` | `dashboard + session-*` |
| `list_agents` | `ipc.rs` | `workspace_root: String, scope: RosterScope` | `Vec<ScopedRosterEntry>` | `dashboard + session-*` |
| `list_providers` | `ipc.rs` | `workspace_root: String, scope: RosterScope` | `Vec<ScopedRosterEntry>` | `dashboard + session-*` |

`RosterScope` is a tagged union: `{ type: "SessionWide" } | { type: "Agent", id: AgentId } | { type: "Provider", id: ProviderId }`. Filter semantics: `SessionWide` returns everything; `Agent(id)` narrows to entries bound to that agent (returns empty today for skills/agents/MCP since those surface as `SessionWide` until per-agent binding lands); `Provider(id)` narrows to that provider entry. Built-in providers (`anthropic`, `openai`, `ollama`) are hardcoded; `[providers.custom_openai.<name>]` entries from merged settings surface as `custom_openai:<name>`.

**Transcript export** (F-607) — thin wrapper over the daemon's on-disk `events.jsonl` so the AgentMonitor Inspector (F-449 §9.3) can pull a session transcript through IPC instead of the filesystem.

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `export_transcript` | `transcript_ipc.rs` | `session_id: String, workspace_root: String` | `Vec<u8>` (raw `events.jsonl` bytes) | `dashboard + session-{id}` |

`session_id` is validated against the canonical [`SessionId`](../../crates/forge-core/src/ids.rs) wire shape (16 lowercase hex chars) before the path is built, so path-traversal / NUL / separator inputs are rejected. The path is composed via `forge_session::server::event_log_path` — the same resolver the daemon writes through. A session that has not yet written its first event returns an empty `Vec<u8>` (with a `tracing::warn`); a transcript larger than 50 MiB returns `transcript exceeds export cap: …`.

**Dashboard sessions** (F-449) — registered only in `window_manager::run` because the dashboard window is the only legitimate caller. `session_list` reads the on-disk roster the daemon maintains; `open_session` spawns or focuses the matching `session-{id}` webview window through `window_spec::open_or_focus`.

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `session_list` | `dashboard_sessions.rs` | (none) | `Vec<SessionSummary { id, subject, state, persistence, created_at, last_event_at }>` | `dashboard` |
| `open_session` | `dashboard_sessions.rs` | `id: String` | `()` | `dashboard` |

**Provider status / selection** (F-585 / F-586) — the Dashboard's provider picker. `provider_status` polls the active provider's health behind a `ProviderStatusCache`; the picker trio reads/writes the dashboard's `[providers.active]` setting and broadcasts a `provider:changed` Tauri event app-wide so each session's `session_switch_provider` listener can swap on the next turn (see F-640 wiring in §5).

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `provider_status` | `dashboard.rs` | (none) | `ProviderStatus { reachable, base_url, models, last_checked, error_kind? }` | `dashboard` |
| `dashboard_list_providers` | `providers_ipc.rs` | (none) | `Vec<ProviderEntry { id, display_name, credential_required, has_credential, model_available, model? }>` | `dashboard` |
| `get_active_provider` | `providers_ipc.rs` | (none) | `Option<String>` (provider id) | `dashboard` |
| `set_active_provider` | `providers_ipc.rs` | `provider_id: String` | `()` (also emits `provider:changed`) | `dashboard` |

> The wire-name collision rationale: F-591's roster command takes the bare `list_providers` slot with the `(workspace_root, scope)` signature; the Dashboard's settings panel uses `dashboard_list_providers` for its `(has_credential, is_active)`-augmented shape. Both are registered.

**Per-provider credentials** (F-587) — the Dashboard's keyring panel. Each command runs `require_window_label(&webview, "dashboard", "<cmd>")` and refuses any `session-*` label.

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `login_provider` | `credentials_ipc.rs` | `provider_id: String, key: String` | `()` | `dashboard` |
| `logout_provider` | `credentials_ipc.rs` | `provider_id: String` | `()` | `dashboard` |
| `has_credential` | `credentials_ipc.rs` | `provider_id: String` | `bool` | `dashboard` |

**Containers** (F-597) — the Dashboard's container lifecycle UI. `detect_container_runtime` probes the host (Podman vs Docker vs none); the rest operate on the registry plus the `forge-oci::PodmanRuntime` impl. All five share the `dashboard` window-label gate.

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `detect_container_runtime` | `containers_ipc.rs` | (none) | `RuntimeStatus` (tagged enum: `Available` / `Missing { tool }` / `Broken { tool, reason }` / `RootlessUnavailable { tool, reason }` / `Unknown { reason }`) | `dashboard` |
| `list_active_containers` | `containers_ipc.rs` | (none) | `Vec<ContainerInfo { session_id, container_id, image, started_at, stopped }>` | `dashboard` |
| `stop_container` | `containers_ipc.rs` | `container_id: String` | `()` | `dashboard` |
| `remove_container` | `containers_ipc.rs` | `container_id: String` | `()` | `dashboard` |
| `container_logs` | `containers_ipc.rs` | `container_id: String, since: Option<String>, tail: Option<usize>` | `Vec<LogLine { stream, line, timestamp? }>` | `dashboard` |

**Agent memory** (F-602) — the Dashboard's Memory section reads/writes per-agent memory files under the workspace's `.forge/memory/` tree. Post-F-668 these run the strict `require_window_label(&webview, "dashboard", …)` helper directly — they no longer admit `session-*` callers.

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `list_agent_memory` | `memory_ipc.rs` | `workspace_root: String` | `Vec<AgentMemoryEntry { agent_id, path, size_bytes?, updated_at? }>` | `dashboard` |
| `read_agent_memory` | `memory_ipc.rs` | `agent_id: String` | `String` (raw memory body, possibly empty) | `dashboard` |
| `save_agent_memory` | `memory_ipc.rs` | `agent_id: String, body: String` | `AgentMemorySavedDto { version, updated_at }` | `dashboard` |
| `clear_agent_memory` | `memory_ipc.rs` | `agent_id: String` | `()` | `dashboard` |

**Usage** (F-594) — the Dashboard's Usage view. `cross_workspace=false` filters via `workspace_root` to the active workspace; `true` aggregates every monthly file under `<config>/forge/usage/`.

| Command | File | Args | Response | Authz |
|---------|------|------|----------|-------|
| `usage_summary` | `usage_ipc.rs` | `range: UsageRange, group_by: GroupBy, cross_workspace: bool, workspace_root: Option<String>` | `UsageSummary { range, group_by, total_tokens_in, total_tokens_out, total_cost?, breakdown }` | `dashboard` |

`UsageRange` is `{ Today, Last7, Last30, All, CustomRange { start, end } }`. `GroupBy` is `{ Provider, Model, Scope }`. See `forge-core/src/usage.rs` for `UsageBreakdown` / `Money` shapes.

---

#### 4.1.2 Speculative — not yet shipped

The block below is a historical design sketch from before Phase 3. **The shipped shapes are in §4.1.1 above** — when a name collides (`rerun_message`, `select_branch`, `compact_transcript`, `read_file`, `tree`, `toggle_mcp_server`, `import_mcp_config`, `start_background_agent`, `promote_background_agent`, `usage_summary`, `stop_container`, `container_logs`, `set_setting`, `open_session`), the §4.1.1 entry is canonical and the sketch below should be considered superseded. The remaining unshipped sketches (`list_sessions`, `attach_session`, `detach_session`, `kill_session`, `archive_session`, `send_message`, `stop_stream`, `revoke_whitelist`, `list_workspaces`, `get_memory_enabled` / `set_memory_enabled` / `read_memory`, `list_containers`, `get_setting`, `preview_start`, `preview_stop`) are still design ideas — their exact shapes may change before they ship and they are **not** registered today.

```rust
// Session lifecycle
#[tauri::command] async fn list_sessions(filter: SessionFilter) -> Result<Vec<SessionSummary>, IpcError>;
#[tauri::command] async fn open_session(workspace: PathBuf, target: SessionTarget, persistence: Option<SessionPersistence>) -> Result<SessionId, IpcError>;
#[tauri::command] async fn attach_session(id: SessionId) -> Result<SessionHandle, IpcError>;
#[tauri::command] async fn detach_session(id: SessionId) -> Result<(), IpcError>;
#[tauri::command] async fn kill_session(id: SessionId) -> Result<(), IpcError>;
#[tauri::command] async fn archive_session(id: SessionId) -> Result<(), IpcError>;  // manual trigger

pub enum SessionTarget {
    Agent(AgentId),
    Provider { id: ProviderId, model: Option<String> },
}
pub enum SessionFilter { Active, Archived, All }

// Message flow
#[tauri::command] async fn send_message(session: SessionId, text: String, context: Vec<ContextRef>, provider_override: Option<ProviderOverride>) -> Result<MessageId, IpcError>;
#[tauri::command] async fn stop_stream(session: SessionId) -> Result<(), IpcError>;
#[tauri::command] async fn rerun_message(session: SessionId, msg: MessageId, variant: RerunVariant, provider_override: Option<ProviderOverride>) -> Result<MessageId, IpcError>;
#[tauri::command] async fn select_branch(session: SessionId, parent: MessageId, variant: MessageId) -> Result<(), IpcError>;
#[tauri::command] async fn compact_transcript(session: SessionId) -> Result<CompactReport, IpcError>;

pub enum RerunVariant { Replace, Branch, Fresh }

// Approval
#[tauri::command] async fn approve_tool_call(session: SessionId, id: ToolCallId, scope: ApprovalScope) -> Result<(), IpcError>;
#[tauri::command] async fn reject_tool_call(session: SessionId, id: ToolCallId, reason: Option<String>) -> Result<(), IpcError>;
#[tauri::command] async fn revoke_whitelist(session: SessionId, pattern: ApprovalScope) -> Result<(), IpcError>;

// Workspace
#[tauri::command] async fn list_workspaces() -> Result<Vec<WorkspaceSummary>, IpcError>;
#[tauri::command] async fn read_file(session: SessionId, path: PathBuf, range: Option<Range>) -> Result<FileContent, IpcError>;
#[tauri::command] async fn tree(session: SessionId, path: PathBuf, depth: u32) -> Result<TreeNode, IpcError>;

// Providers / skills / MCP / agents
// (`list_providers`, `list_mcp_servers`, `list_skills`, `list_agents` now
// shipped under F-591 — see the "Roster discovery" table above. F-587 also
// shipped `login_provider` / `logout_provider` / `has_credential`.)
#[tauri::command] async fn toggle_mcp_server(id: McpId, enabled: bool) -> Result<(), IpcError>;
#[tauri::command] async fn import_mcp_config(source: ImportSource, dest_scope: Scope) -> Result<ImportReport, IpcError>;

// Background agents
#[tauri::command] async fn start_background_agent(session: SessionId, agent: AgentId, initial_message: String) -> Result<AgentInstanceId, IpcError>;
#[tauri::command] async fn promote_background_agent(session: SessionId, id: AgentInstanceId) -> Result<(), IpcError>;

// Memory (opt-in feature)
#[tauri::command] async fn get_memory_enabled() -> Result<bool, IpcError>;
#[tauri::command] async fn set_memory_enabled(enabled: bool) -> Result<(), IpcError>;
#[tauri::command] async fn read_memory(agent: AgentId) -> Result<Option<String>, IpcError>;

// Usage (F-593)
//
// `cross_workspace = false` filters to the dashboard's currently-active
// workspace via `workspace_root`; `true` aggregates every monthly file
// under `<config>/forge/usage/`. Restricted to the dashboard window.
#[tauri::command] async fn usage_summary(range: UsageRange, group_by: GroupBy, cross_workspace: bool, workspace_root: Option<String>) -> Result<UsageSummary, IpcError>;

// Containers
#[tauri::command] async fn list_containers() -> Result<Vec<ContainerSummary>, IpcError>;
#[tauri::command] async fn container_logs(id: ContainerId, tail: u32) -> Result<String, IpcError>;
#[tauri::command] async fn stop_container(id: ContainerId) -> Result<(), IpcError>;

// Settings
#[tauri::command] async fn get_setting(key: String) -> Result<JsonValue, IpcError>;
#[tauri::command] async fn set_setting(key: String, value: JsonValue) -> Result<(), IpcError>;

// Dev server (static HTML preview)
#[tauri::command] async fn preview_start(workspace: PathBuf, entry: Option<PathBuf>) -> Result<PreviewInfo, IpcError>;
#[tauri::command] async fn preview_stop() -> Result<(), IpcError>;
```

**Every command.**
- Is `async`
- Returns `Result<T, IpcError>` where `IpcError` is a tagged enum with display strings
- Has a TS-generated type at `web/packages/ipc/src/generated.ts`
- Is wrapped in a typed client helper in `web/packages/ipc/src/client.ts`

#### Events (host → webview)

Events are emitted per-topic with per-window routing — never `emit_all`. Each emit call targets `EventTarget::webview_window(label)` so a forged payload cannot redirect delivery and siblings never see each other's traffic. The label is bound at subscription / spawn time, not re-read from the payload.

| Channel | Payload type | Target label | Source |
|---------|--------------|--------------|--------|
| `session:event` | `SessionEventPayload` | `session-{session_id}` | `crates/forge-shell/src/ipc.rs` (`AppHandleSink::emit`) |
| `terminal:bytes` | `TerminalBytesEvent` | owner webview label | `crates/forge-shell/src/ipc.rs` (`spawn_event_forwarder`) |
| `terminal:exit` | `TerminalExitEvent` | owner webview label | `crates/forge-shell/src/ipc.rs` (`spawn_event_forwarder`) |
| `lsp_message` | `LspMessageEvent` | owner webview label | `crates/forge-shell/src/ipc.rs` (`spawn_lsp_forwarder`) |

```rust
// session:event — carries every session-scoped event the daemon emits.
// `event` is the tagged union from `forge_core::Event` (see §5.3).
pub struct SessionEventPayload {
    pub session_id: String,
    pub seq: u64,
    pub event: forge_core::Event,
}

// terminal:bytes — raw PTY chunk for xterm.js.
pub struct TerminalBytesEvent {
    pub terminal_id: TerminalId,
    pub data: Vec<u8>,
}

// terminal:exit — fired once when the child reaps.
pub struct TerminalExitEvent {
    pub terminal_id: TerminalId,
    pub code: Option<i32>,
    pub killed_by_drop: bool,
}

// lsp_message — opaque JSON-RPC frame from the language server.
pub struct LspMessageEvent {
    pub server: String,
    pub message: serde_json::Value,
}
```

`forge_core::Event` is a `#[serde(tag = "type")]` union covering session, provider, MCP, usage, and agent state. MCP state transitions ride inside it as `Event::McpState(McpStateEvent)` — F-155 retired the per-topic `mcp:state` channel, so there is no longer a top-level event for MCP state. All MCP state changes arrive on `session:event`.

All internally-tagged enums that cross the IPC boundary — `Event`, `ServerState` — share the discriminator name `type` as of F-380. `StepOutcome` retains `tag = "status"` as a pinned exception. See `docs/architecture/event-conventions.md` for the full rules and pinned exceptions.

```rust
// crates/forge-core/src/mcp_state.rs — re-exported as `forge_mcp::{McpStateEvent, ServerState}`.
pub struct McpStateEvent {
    pub server: String,        // server name — matches the key in the loaded spec map
    pub state: ServerState,
    pub at: DateTime<Utc>,     // F-380: RFC3339 wall-clock; renamed from `ts: SystemTime`
}

#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerState {
    Starting,                         // spawn/connect in progress; `initialize` handshake not yet complete
    Healthy,                          // last health-check succeeded
    Degraded { reason: String },      // one health-check failed; manager will restart after backoff
    Failed   { reason: String },      // terminal until re-enabled — restart window exhausted or crashed beyond policy
    Disabled { reason: String },      // F-155: explicit user toggle-off; surfaces `"MCP server <name> is disabled"` from `McpManager::call` and `toggle_mcp_server(name, true)` re-enters via `Starting`
}
```

`Disabled` is distinct from `Failed { reason: "stopped" }` — it marks an explicit toggle-off so the manager surfaces a canonical error on `call()` and the running-session toggle can resume through `Starting`. The TS mirror lives at `web/packages/ipc/src/generated/McpStateEvent.ts` (and `ServerState.ts`) — regenerated from the Rust source, not hand-edited.

The webview subscribes with `listen<T>(channel, handler)` from `@tauri-apps/api/event` (typically wrapped by a helper in `web/packages/ipc`).

Events are fire-and-forget; critical state is also fetchable via commands for late-join cases.

### 4.2 Boundary 2: shell ↔ session (UDS)

See §5 for the full protocol. Shell maintains **one UDS connection per open session**. Events from the session are wrapped in `SessionEventPayload` and forwarded on the `session:event` channel to that session's webview window.

### 4.3 Boundary 3: daemon ↔ sidecar (UDS)

F-608 introduced a third IPC boundary: the daemon (`forged`) ↔ per-instance agent sidecar (`forged-agent`) wire. Each `AgentInstanceId` gets its own `forged-agent` child process — the per-turn provider loop runs there, the daemon stays the authority for credentials / persistence / MCP / event log. The shell-facing surface (Boundary 1 and Boundary 2) is unchanged.

**Transport.** Same length-prefixed JSON framing as the session protocol (4 MiB cap, UDS pair created by `SidecarSupervisor`). The wire enum lives in `crates/forge-ipc/src/sidecar.rs::SidecarMessage` (`#[serde(tag = "t")]`, sibling shape to [`IpcMessage`](#53-message-types)). See `docs/architecture/agent-sidecar.md` for the supervisor lifecycle / restart policy and §5 of that doc for the field-level payload definitions.

**Schema versioning.** Two independent constants:
- `PROTO_VERSION` (currently `1`) — the discriminator vocabulary; bumped only on a true wire break.
- `SIDECAR_SCHEMA_VERSION` (currently `2`) — bumped whenever an existing field's serialization changes (rename, type widening, etc.). History: `1` = initial F-608 step 1; `2` = step 6, `Credentials.secret_handle: String` widened to `Credentials.secret: SecretBytes`. The supervisor logs the value on `HelloAck` but does not gate startup on it today.

**Variants.** 12 total. The single `SidecarMessage` enum carries both directions; the `t` discriminator's prose comments mark each variant's direction.

| Variant | Direction | Payload (selected fields) | Purpose |
|---------|-----------|---------------------------|---------|
| `Hello` | daemon → sidecar | `proto, instance_id, agent_def, allowed_paths, workspace_path, provider_spec, sandbox_level, telemetry_endpoint?` | First frame after connect. Carries everything the child needs to bootstrap its provider loop without re-reading daemon state. |
| `RunTurn` | daemon → sidecar | `turn_id, msg_id, text, agents_md, branch_parent?, branch_variant_index?, byte_budget` | Drive one turn of the request loop. Sidecar streams events back as `Event` frames; turn completion is observed via `Event::TurnCompleted`. |
| `Credentials` | daemon → sidecar | `provider_id, secret: SecretBytes` | Push a per-provider credential just-in-time. `SecretBytes` redacts in `Debug` / `Display`; the receive side wraps into `secrecy::SecretString`. |
| `ToolCallApproved` | daemon → sidecar | `id, scope` | Approve a pending tool call. Mirrors [`crate::ToolCallApproved`] shape. |
| `ToolCallRejected` | daemon → sidecar | `id, reason?` | Reject a pending tool call. Mirrors [`crate::ToolCallRejected`]. |
| `CompactTranscript` | daemon → sidecar | (empty) | F-598: proxied from the shell. Compact the active transcript. |
| `Shutdown` | daemon → sidecar | `grace_ms` | Cooperative shutdown signal. Sidecar drains its outbox, closes the write half, exits within `grace_ms` or the supervisor escalates to SIGTERM. |
| `HelloAck` | sidecar → daemon | `pid, started_at` | Acknowledges `Hello`. The supervisor pins `pid` into `ResourceMonitor::track` (closes F-451). |
| `Event` | sidecar → daemon | `seq, event: forge_core::Event` | One event produced inside the sidecar's run loop. `seq` is monotonic per sidecar process; the daemon writes the payload to the session event log unchanged. |
| `ToolCallApprovalRequest` | sidecar → daemon | (provider-shaped tool call) | The provider loop hit a tool call needing human approval. The daemon forwards it to the shell and replies with `ToolCallApproved` / `ToolCallRejected`. |
| `Heartbeat` | sidecar → daemon | (empty) | 1 Hz liveness ping. The supervisor's heartbeat watchdog times out at 5 s of silence and escalates to a restart. |
| `Crashed` | sidecar → daemon | `panic_message?` | Best-effort panic dump from the sidecar's panic hook before the process exits. May be lost on hard segfault — fallback is EOF + non-zero exit detection. |

**Authoritative reference.** [`crates/forge-ipc/src/sidecar.rs`](../../crates/forge-ipc/src/sidecar.rs) is canonical for the payload structs (`SidecarHello`, `SidecarRunTurn`, `SidecarCredentials`, …). The full struct field set is intentionally documented in source via doc-comments — those doc-comments are the contract. Anything in `agent-sidecar.md` that diverges should be treated as out-of-date.

### 4.4 Type generation

All IPC types live in `crates/forge-ipc/src/`:
```
forge-ipc/
  src/
    lib.rs       # IpcMessage, shell ↔ daemon variants, framing helpers
    sidecar.rs   # SidecarMessage, daemon ↔ sidecar variants (F-608)
```

Every type that crosses a boundary derives `#[derive(Serialize, Deserialize, TS)]` and `#[ts(export)]`. ts-rs writes a sibling test case per type that emits the corresponding `.ts` file under `web/packages/ipc/src/generated/` when `cargo test` runs. The export only fires during `cargo test` — `cargo build` does not regenerate bindings.

**Auto-generated files.** Everything under `web/packages/ipc/src/generated/` is owned by ts-rs. Do not hand-edit; the next regen will overwrite. Each file carries a `// This file was generated by [ts-rs] ... Do not edit this file manually.` header.

**Regenerate locally** after changing any Rust type that derives `TS`:

```bash
just generate-ts   # runs `cargo test --workspace --tests export_bindings_`
```

Then commit the regenerated `.ts` files alongside the Rust change.

**CI drift gate.** `.github/workflows/ci.yml` runs `just ts-check`, which regenerates the bindings and then `git diff --exit-code` against the committed copy. The job fails if anything regenerated differently from what's checked in — so a Rust type change without the matching TS commit is caught at PR time. See issue #624.

---

## 5. Session process protocol

The UDS protocol between shell and session. This is a firm contract — agents, headless CLI, and future remote session support all depend on it.

### 5.1 Transport

- Unix domain socket (stream) on Mac/Linux
- Named pipes (`\\.\pipe\forge-sessions-<id>`) on Windows (native v1.3; WSL uses UDS)
- **Length-prefixed JSON frames**: `[u32 big-endian length][UTF-8 JSON body]`
- Max frame size: 4 MiB (reject larger; session closes connection and logs)

### 5.2 Handshake

On connect, the **client** (shell, CLI, or other) sends:
```json
{"t":"Hello","proto":1,"client":{"kind":"shell","pid":12345,"user":"alice"}}
```
The **session** responds:
```json
{"t":"HelloAck","session_id":"a3f1b2c4","workspace":"/home/alice/code/acme-api","started_at":"2026-04-15T14:22:00Z","event_seq":1842,"schema_version":1}
```
The client then sends either:
```json
{"t":"Subscribe","since":1842}           // live only
{"t":"Subscribe","since":0}              // full replay + live
{"t":"Subscribe","since":1500}           // catch-up from seq
```

### 5.3 Message types

`IpcMessage` is a single `#[serde(tag = "t")]` discriminated union covering both directions of the shell ↔ daemon wire. **22 variants total** today; canonical source is [`crates/forge-ipc/src/lib.rs`](../../crates/forge-ipc/src/lib.rs). Variant names match exactly what goes on the wire as the `t` value.

| Variant | Direction | Payload (selected fields) | Since | Purpose |
|---------|-----------|---------------------------|-------|---------|
| `Hello` | client → daemon | `proto, client: ClientIdentity` | Phase 1 | Handshake. Asserts the peer's protocol version. |
| `HelloAck` | daemon → client | `session_id, workspace, started_at, event_seq, schema_version` | Phase 1 | Handshake reply. Carries the session's resume cursor. |
| `Subscribe` | client → daemon | `since: u64` | Phase 1 | Subscribe to the event stream from `seq=since`. `0` = full replay + live; current seq = live only. |
| `Event` | daemon → client | `seq, event: forge_core::Event` | Phase 1 | One session event. The full tagged union is documented in [`event-conventions.md`](event-conventions.md). |
| `SendUserMessage` | client → daemon | `text, context, provider_override?, branch_parent?` | Phase 1 | Open a new turn. The daemon responds via the event stream (`StepStarted`, `AssistantDelta`, …). |
| `ToolCallApproved` | client → daemon | `id, scope` | Phase 1 | Approve a pending tool call. `scope` selects the persistence level. |
| `ToolCallRejected` | client → daemon | `id, reason?` | Phase 1 | Reject a pending tool call. The orchestrator emits a `ToolCallRejected` event. |
| `RerunMessage` | client → daemon | `msg_id, variant: RerunVariant` | F-143 | Re-run an assistant message. `variant ∈ { Replace, Branch, Fresh }`. |
| `SelectBranch` | client → daemon | `parent_id, variant_index: u32` | F-144 | Activate a specific branch variant. |
| `DeleteBranch` | client → daemon | `parent_id, variant_index: u32` | F-145 | Tombstone a branch variant. |
| `CompactTranscript` | client → daemon | (empty) | F-598 | Compact the active transcript. The daemon dispatches to `forge_session::compaction::compact` and emits `Event::ContextCompacted` on success — there is no direct response payload. |
| `ListMcpServers` | client → daemon | (empty) | F-155 | Request the daemon's authoritative MCP server list. Response arrives as `McpServersList`. |
| `McpServersList` | daemon → client | `servers: Vec<McpServerInfo>` | F-155 | Response to `ListMcpServers`. Snapshot of `McpManager::list`. |
| `ToggleMcpServer` | client → daemon | `name, enabled: bool` | F-155 | Toggle an MCP server on/off on the daemon's authoritative `McpManager`. Affects running session tool dispatch. Response arrives as `McpToggleResult`. |
| `McpToggleResult` | daemon → client | `name, enabled_after: bool, error?: String` | F-155 | Response to `ToggleMcpServer`. `error` is `Some` when the name is unknown or the lifecycle transition failed. |
| `ImportMcpConfig` | client → daemon | `source: String, apply: bool` | F-155 | Import a third-party MCP config into the workspace `.mcp.json`. `apply=false` runs a dry import (computes the new server set without rewriting the file). |
| `McpImportResult` | daemon → client | `source, imported: Vec<String>, destination_path?, error?: String` | F-155 | Response to `ImportMcpConfig`. |
| `PauseSession` | client → daemon | (empty) | F-603 | Pause the orchestrator at the next inter-step checkpoint. Daemon emits `Event::SessionPaused`. Already-paused is a no-op. See §5.7. |
| `ResumeSession` | client → daemon | (empty) | F-603 | Resume a paused orchestrator. Daemon emits `Event::SessionResumed`. Already-running is a no-op. See §5.7. |
| `InterruptSession` | client → daemon | (empty) | F-604 | Interrupt the in-flight assistant turn at the next chunk boundary. Distinct from cancel (terminal) and pause (resumable). Response arrives as `RefineHandoff`. See §5.8. |
| `RefineHandoff` | daemon → client | `partial_text, captured_at_step_id, captured_at_msg_id` | F-604 | Response to `InterruptSession`. Carries the partial assistant text captured at the interrupt boundary. |
| `SwitchProvider` | client → daemon | `provider_id: String` | F-640 | Swap the in-process `SwappableProvider`'s inner. `provider_id` matches the dashboard's `[providers.active]` shape (`"ollama"`, `"anthropic"`, `"openai"`, `"custom_openai:<name>"`). In-flight turns finish on the previous provider; the next `run_turn` dispatches to the new one. |

> Future work tracked in F-701 §4: a doc-test or CI script that asserts the variant count in this table matches the discriminant count of `forge_ipc::IpcMessage`, so the doc cannot drift from the enum without breaking the build.

### 5.4 Event log persistence

- `.forge/sessions/<id>/events.jsonl` is the canonical log
- **First line of every file is the schema header:** `{"schema_version": 1}`
- Every emitted event gets a monotonic `seq` integer; persisted before send
- On restart (including post-archive reactivation), the session replays from the log, recomputing in-memory state
- Periodic snapshots (every 500 events or 5 minutes) go to `snapshots/<seq>.msgpack` to accelerate replay — optimization, not a requirement for correctness
- Clients can subscribe from any `seq`; session streams everything after
- **`Event::UsageTick`** carries an optional `at: DateTime<Utc>` (F-593-followup, issue #646) tagging the wall-clock emission moment of the tick. The post-flush monthly aggregator buckets by this timestamp so a session crossing a month boundary records each tick into the correct calendar month. Logs predating the field deserialize `at` as `None` and the aggregator falls back to flush time for those rows — pre-fix behavior preserved for replay.

### 5.5 Schema versioning and migrations

- The first line schema header governs how the rest of the file is interpreted
- Forge refuses to read a jsonl file without a recognized `schema_version`
- Schema bumps come with migration functions registered in `forge-core::migrations`
- Migrations run at session open when the file's schema is below current

### 5.6 Multi-client semantics

- Multiple shells can attach to one session (the GUI and a `forge session tail` simultaneously)
- Any can send commands; the session logs `ClientIdentity` alongside the resulting event
- Conflicting commands resolve last-write-wins with a 50ms coalescing window for identical approvals

### 5.7 Pause / Resume (F-603)

The orchestrator carries a `Running | Paused` state that AgentMonitor's Pause/Resume buttons drive over the UDS:

- **Frames.** Client → session sends `PauseSession` / `ResumeSession`. Daemon → client emits `Event::SessionPaused` / `Event::SessionResumed` exactly once per real transition.
- **Checkpoint.** The pause takes effect at the orchestrator's *between-step* boundary in `forge_session::orchestrator::run_request_loop` — never mid-step. An in-flight provider stream and any tool call dispatched from it run to completion (their `StepFinished` lands), then the orchestrator parks at `Session::wait_if_paused` before opening the next `StepStarted(Model)`.
- **Idempotency.** `PauseSession` while paused and `ResumeSession` while running are no-ops: the daemon logs `debug!` and emits no event. They never error.
- **Persistence.** State is in-memory only. A daemon restart returns to `Running`.
- **Tool-in-flight semantics.** Pause does *not* abort an in-flight tool call. The approval / dispatch path keeps flowing while the pause flag is set; only the *next* model step waits.

### 5.8 Interrupt + Refine (F-604)

A third turn-control primitive sits beside cancel and pause. Where cancel is terminal (ends the session) and pause is resumable (keeps the same turn), **interrupt** cuts the in-flight assistant turn cleanly and hands the captured partial text back as a refine handoff. The Composer's interrupt button drives this over the UDS as a request-response exchange:

- **Frames.** Client → session sends `InterruptSession`. The daemon replies on the same connection with `RefineHandoff { partial_text, captured_at_step_id, captured_at_msg_id }`. The shell's `SessionBridge::interrupt_session` routes the response to the awaiting Tauri command via the per-kind reply slot pattern (same machinery as `ListMcpServers` → `McpServersList`). The `session_interrupt_and_refine(session_id) -> RefineHandoff` Tauri command surfaces the handoff to the webview.
- **Event.** The orchestrator also emits `Event::SessionInterrupted { partial_text, captured_at_step_id, captured_at_msg_id, at }` on the session event stream, so any subscriber (a separate webview, a `forge session tail`) sees the same payload through the normal event pipeline.
- **Checkpoint.** Interrupt takes effect at the *next chunk boundary* of the in-flight provider stream in `forge_session::orchestrator::run_request_loop`. The orchestrator polls `Session::is_interrupt_requested` after every `ChatChunk` and breaks out at the next clean point — `assistant_text` reflects every `AssistantDelta` already on the wire. Distinct from the F-603 pause checkpoint (between-step, keeps the turn alive) — interrupt is mid-stream and ends the current turn.
- **Finalisation.** On the interrupt branch, the orchestrator emits `AssistantMessage(stream_finalised: true, text: <partial>)` followed by `SessionInterrupted` and `StepFinished(outcome: Error { reason: "interrupted" })`. Tool calls buffered in the stream are dropped without emitting partial Tool* events (same drop semantics as the `ChatChunk::Error` arm).
- **Quiescent state.** After interrupt, the session stays alive; the next `SendUserMessage` opens a fresh turn normally. The interrupt-request flag is cleared by `Session::publish_interrupt_capture` so it does not carry into the next turn.
- **No-op shape.** An interrupt with no in-flight assistant turn replies with an empty handoff (`partial_text: ""`, both anchors empty) and emits no `SessionInterrupted` event. The daemon's IPC handler clears the flag via `clear_interrupt_request` after a 5s deadline so a stray request cannot short-circuit a future turn.
- **Persistence.** State is in-memory only. A daemon restart returns to `Running`.
