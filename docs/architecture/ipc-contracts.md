# IPC Contracts

> Extracted from IMPLEMENTATION.md §4-5 — the two IPC boundaries, Tauri commands, events, UDS framing, handshake, and full message types

---

## 4. IPC contracts

Forge has **two distinct IPC boundaries**. They must not be confused.

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

> **Shipped coverage.** Phase 1 shipped `session_hello`, `session_subscribe`, `session_send_message`, `session_approve_tool`, `session_reject_tool`. Phase 2 extends that surface with 29 additional commands enumerated under **§4.1.1 Phase 2 additions — shipped** below. F-591 added the four roster-discovery commands (`list_skills`, `list_mcp_servers`, `list_agents`, `list_providers`) — see the "Roster discovery" table in §4.1.1. Anything that appears in the speculative block after §4.1.1 (e.g. `list_sessions`, `attach_session`, `list_containers`, …) is reserved for Phase 3+ and is **not** wired today. The canonical registration list is `forge-shell/src/ipc.rs::build_invoke_handler`'s `tauri::generate_handler!` invocation. See ADR-001 §4 for the matching subset note on the UDS boundary.

**Pattern.** Tauri `command` handlers for request/response (webview → host) and Tauri `events` for push (host → webview).

#### 4.1.1 Phase 2 additions — shipped

Registered in `crates/forge-shell/src/ipc.rs::build_invoke_handler`. Every command returns `Result<T, String>` on the wire; the `String` error carries a stable tag (e.g. `forbidden: window label mismatch`, `stop_background_agent: unknown instance`). Types are derived with `ts-rs` and regenerated into `web/packages/ipc/src/generated/`.

**Authz gate column.**

| Gate | Helper call | Meaning |
|------|-------------|---------|
| `session-{id}` | `require_window_label(&webview, "session-{session_id}")` | Only the exact session webview that owns the `session_id` may invoke |
| `dashboard` | `require_window_label_in(&webview, &["dashboard"], false)` | Only the dashboard window may invoke |
| `dashboard + session-{id}` | inline check against `"dashboard"` and `"session-{session_id}"` | Dashboard **or** the *specific* session's window may invoke; other `session-*` windows are rejected |
| `dashboard + session-*` | `require_window_label_in(&webview, &["dashboard"], true)` | Dashboard **or** any `session-*` webview may invoke |
| `any session-*` | `require_window_label_in(&webview, &[], true)` | Any `session-*` webview may invoke; dashboard is rejected |

**Turn-flow extensions** (reach the daemon via `SessionBridge`):

| Command | Args | Response | Authz |
|---------|------|----------|-------|
| `rerun_message` | `session_id: String, msg_id: String, variant: RerunVariant` | `()` | `session-{id}` |
| `select_branch` | `session_id: String, parent_id: String, variant_index: u32` | `()` | `session-{id}` |
| `delete_branch` | `session_id: String, parent_id: String, variant_index: u32` | `()` | `session-{id}` |

`RerunVariant` is `{ Replace, Branch, Fresh }` (see `forge-core/src/types.rs`).

**Persistent approvals** (F-036) — user/workspace scoped, read/written through `SessionBridge`:

| Command | Args | Response | Authz |
|---------|------|----------|-------|
| `get_persistent_approvals` | `workspace_root: String` | `Vec<PersistentApprovalEntry>` | `dashboard + session-*` |
| `save_approval` | `entry: ApprovalEntry, level: ApprovalLevel, workspace_root: String` | `()` | `dashboard + session-*` |
| `remove_approval` | `scope_key: String, level: ApprovalLevel, workspace_root: String` | `()` | `dashboard + session-*` |

`ApprovalLevel` is `{ Session, Workspace, User }`. `PersistentApprovalEntry { scope_key, tool_name, label, level }` mirrors `forge-core::ApprovalEntry` with the level tag carried through.

**Terminal** (F-125) — each spawn is owned by the calling webview label; subsequent write/resize/kill from a different label are rejected:

| Command | Args | Response | Authz |
|---------|------|----------|-------|
| `terminal_spawn` | `args: TerminalSpawnArgs { terminal_id, shell?, cwd, cols, rows }` | `()` | `any session-*` |
| `terminal_write` | `terminal_id: TerminalId, data: Vec<u8>` | `()` | `any session-*` (+ owner-label check) |
| `terminal_resize` | `terminal_id: TerminalId, cols: u16, rows: u16` | `()` | `any session-*` (+ owner-label check) |
| `terminal_kill` | `terminal_id: TerminalId` | `()` | `any session-*` (+ owner-label check) |

Output bytes flow back via the `terminal:bytes` Tauri event (see §4.1 events).

**Layouts** (F-131) — single-file on-disk store at `.forge/layouts.json`; UI is dashboard-only:

| Command | Args | Response | Authz |
|---------|------|----------|-------|
| `read_layouts` | `workspace_root: String` | `Layouts { active, named: HashMap<String, Layout> }` | `dashboard + session-*` |
| `write_layouts` | `workspace_root: String, layouts: Layouts` | `()` | `dashboard + session-*` |

**Filesystem** (F-126 / F-143 / F-150) — session-scoped and routed through the session daemon so edits stay inside the sandboxed workspace root:

| Command | Args | Response | Authz |
|---------|------|----------|-------|
| `read_file` | `session_id: String, path: String` | `FileContent { path, content, bytes, sha256 }` | `session-{id}` |
| `write_file` | `session_id: String, path: String, bytes: Vec<u8>` | `()` | `session-{id}` |
| `tree` | `session_id: String, root: String, depth: Option<u32>` | `TreeNodeDto { name, path, kind, children }` | `session-{id}` |
| `rename_path` | `session_id: String, from: String, to: String` | `()` | `session-{id}` |
| `delete_path` | `session_id: String, path: String` | `()` | `session-{id}` |

`TreeKindDto` is `{ File, Dir, Symlink, Other }`.

**LSP** (F-127) — caller resolves the binary (via `forge-lsp::Bootstrap::ensure`) before spawning; downloads stay outside the Tauri trust boundary. Server messages flow back via the `lsp_message` Tauri event.

| Command | Args | Response | Authz |
|---------|------|----------|-------|
| `lsp_start` | `args: LspStartArgs { server, binary_path, args: Vec<String> }` | `()` | `any session-*` |
| `lsp_stop` | `server: String` | `()` | `any session-*` |
| `lsp_send` | `server: String, message: serde_json::Value` | `()` | `any session-*` |

**Background agents** (F-137 / F-138) — quartet of lifecycle commands against the per-session `BgAgentRegistry`:

| Command | Args | Response | Authz |
|---------|------|----------|-------|
| `start_background_agent` | `session_id: String, agent_name: String, prompt: String` | `String` (instance id) | `session-{id}` |
| `promote_background_agent` | `session_id: String, instance_id: String` | `()` | `session-{id}` |
| `list_background_agents` | `session_id: String` | `Vec<BgAgentSummary { id, agent_name, state }>` | `session-{id}` |
| `stop_background_agent` | `session_id: String, instance_id: String` | `()` | `session-{id}` |

`BgAgentStateDto` is `{ Running, Completed, Failed }`.

**Settings** (F-151) — persistent user/workspace-scoped kv store; dashboard is the primary editor but session windows also read their effective values:

| Command | Args | Response | Authz |
|---------|------|----------|-------|
| `get_settings` | `workspace_root: String` | `AppSettings { notifications, windows, … }` | `dashboard + session-*` |
| `set_setting` | `key: String, value: serde_json::Value, level: SettingsLevel, workspace_root: String` | `()` | `dashboard + session-*` |

`SettingsLevel` is `{ User, Workspace }`.

**MCP** (F-132 / F-155) — session-scoped wrappers over `SessionBridge`'s `McpManager`. Server state transitions stream back via the session event log, not the command response.

| Command | Args | Response | Authz |
|---------|------|----------|-------|
| `session_list_mcp_servers` | `session_id: String` | `Vec<McpServerInfo { name, state, tools }>` | `session-{id}` |
| `toggle_mcp_server` | `session_id: String, name: String, enabled: bool` | `McpToggleResult { name, enabled_after, error }` | `session-{id}` |
| `import_mcp_config` | `session_id: String, source: String, apply: bool` | `McpImportResult { source, imported, destination_path, error }` | `session-{id}` |

> F-591 renamed the original `list_mcp_servers` to `session_list_mcp_servers` so the new roster command (next section) could take the bare `list_mcp_servers` name with the spec-mandated `(workspace_root, scope)` signature.

**Roster discovery** (F-591) — read-only loaders that surface every discoverable resource (skills, MCP servers, agents, providers) and filter by [`RosterScope`](../../crates/forge-core/src/roster.rs). Distinct from F-132's session-scoped MCP commands: roster reads canonical sources (skill loader, agent loader, on-disk `.mcp.json`, hardcoded built-in providers + merged settings) and works without a live session.

| Command | Args | Response | Authz |
|---------|------|----------|-------|
| `list_skills` | `workspace_root: String, scope: RosterScope` | `Vec<ScopedRosterEntry>` | `dashboard + session-*` |
| `list_mcp_servers` | `workspace_root: String, scope: RosterScope` | `Vec<ScopedRosterEntry>` | `dashboard + session-*` |
| `list_agents` | `workspace_root: String, scope: RosterScope` | `Vec<ScopedRosterEntry>` | `dashboard + session-*` |
| `list_providers` | `workspace_root: String, scope: RosterScope` | `Vec<ScopedRosterEntry>` | `dashboard + session-*` |

`RosterScope` is a tagged union: `{ type: "SessionWide" } | { type: "Agent", id: AgentId } | { type: "Provider", id: ProviderId }`. Filter semantics: `SessionWide` returns everything; `Agent(id)` narrows to entries bound to that agent (returns empty today for skills/agents/MCP since those surface as `SessionWide` until per-agent binding lands); `Provider(id)` narrows to that provider entry. Built-in providers (`anthropic`, `openai`, `ollama`) are hardcoded; `[providers.custom_openai.<name>]` entries from merged settings surface as `custom_openai:<name>`.

**Transcript export** (F-607) — thin wrapper over the daemon's on-disk `events.jsonl` so the AgentMonitor Inspector (F-449 §9.3) can pull a session transcript through IPC instead of the filesystem.

| Command | Args | Response | Authz |
|---------|------|----------|-------|
| `export_transcript` | `session_id: String, workspace_root: String` | `Vec<u8>` (raw `events.jsonl` bytes) | `dashboard + session-{id}` |

`session_id` is validated against the canonical [`SessionId`](../../crates/forge-core/src/ids.rs) wire shape (16 lowercase hex chars) before the path is built, so path-traversal / NUL / separator inputs are rejected. The path is composed via `forge_session::server::event_log_path` — the same resolver the daemon writes through. A session that has not yet written its first event returns an empty `Vec<u8>` (with a `tracing::warn`); a transcript larger than 50 MiB returns `transcript exceeds export cap: …`.

---

#### 4.1.2 Speculative — Phase 3 and later

All commands below are design sketches targeting later phases (multi-provider, containers, skills, memory, usage, preview). They are not registered in `build_invoke_handler` today and their exact shapes may change before they ship.

All commands live in `forge-shell/src/commands.rs`. Types in `forge-ipc/src/tauri.rs` are derived with `ts-rs` and regenerated into `web/packages/ipc/src/generated.ts`.

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

### 4.3 Type generation

All IPC types live in `crates/forge-ipc/src/`:
```
forge-ipc/
  src/
    lib.rs       # re-exports
    tauri.rs     # Tauri boundary types
    session.rs   # UDS boundary types (§5)
    common.rs    # shared IDs, enums, primitives
```

Every type that crosses a boundary derives `#[derive(Serialize, Deserialize, TS)]` and `#[ts(export)]`. The script `scripts/gen-ts-types.sh` runs `cargo test` with `ts-rs` flags, then copies the generated `.ts` files into `web/packages/ipc/src/generated/`.

CI fails if generated types drift.

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

> **Phase 1 coverage.** Phase 1 implements the following `IpcMessage` variants in `crates/forge-ipc`: `Hello`, `HelloAck`, `Subscribe`, `Event`, `SendUserMessage`, `ToolCallApproved`, `ToolCallRejected`. Every other variant listed below is reserved for subsequent milestones and is not wired in Phase 1. See ADR-001 §4 for the same subset note alongside its rationale.

Full discriminated union, `t` is the tag.

**Client → session:**
```
Hello, Subscribe, Unsubscribe,
SendUserMessage { text, context, provider_override?, branch_parent? },
StopStream,
RerunMessage { msg_id, variant, provider_override? },
SelectBranch { parent, variant },
CompactTranscript,
ApproveToolCall { id, scope },
RejectToolCall { id, reason? },
RevokeWhitelist { scope },
StartBackgroundAgent { agent, initial_message },
PauseSession,                          // F-603: AgentMonitor Pause
ResumeSession,                         // F-603: AgentMonitor Resume
InterruptSession,                      // F-604: Composer interrupt + refine
ReadFile { path, range? },
WriteFile { path, content },
ListTree { path, depth },
Tick                                   // keepalive
```

**Session → client:**
```
HelloAck, Event { seq, event },       // `event` is a Core::Event from §3.1
StateChanged { state },
FileContent { path, content, sha },
Tree { path, node },
RefineHandoff { partial_text, captured_at_step_id, captured_at_msg_id },  // F-604
Error { code, message, corr? },
Ack { corr }                           // correlation id echo
```

### 5.4 Event log persistence

- `.forge/sessions/<id>/events.jsonl` is the canonical log
- **First line of every file is the schema header:** `{"schema_version": 1}`
- Every emitted event gets a monotonic `seq` integer; persisted before send
- On restart (including post-archive reactivation), the session replays from the log, recomputing in-memory state
- Periodic snapshots (every 500 events or 5 minutes) go to `snapshots/<seq>.msgpack` to accelerate replay — optimization, not a requirement for correctness
- Clients can subscribe from any `seq`; session streams everything after

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
