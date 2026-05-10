# Crate Architecture

> Extracted from IMPLEMENTATION.md §3 — all 12 crates with full responsibility breakdown, key types, and dependencies

---

## 3. Crate architecture

### 3.1 `forge-core`

The canonical types and shared utilities. Everything depends on this; it depends on nothing Forge-specific.

> See [ADR-004: `forge-core` Scope and the Persistence-Split Plan](./ADR-004-forge-core-scope-and-persistence-split.md) for the scope boundary this crate is expected to hold and the phased plan for moving `settings`, `approvals`, `transcript`, `event_log`, `meta`, and `workspaces` I/O into a sibling `forge-persist` crate. New filesystem / config-file code should not land here — redirect it per ADR-004 §1.

**Responsibilities.**
- Core types: `WorkspaceId`, `SessionId`, `AgentId`, `ProviderId`, `ToolCallId`, `MessageId`, `AgentInstanceId`
- `Config` loading (`~/.config/forge/config.toml`, `.agents/*.md` frontmatter, `.mcp.json`, `~/.mcp.json`, `AGENTS.md` workspace instructions)
- `Error` type (anyhow+thiserror hybrid: `ForgeError`)
- Event model (the append-only log): `Event`, `EventKind`, `Transcript`
- Credential storage trait + OS implementations (`security-framework`, `secret-service`)
- Path utilities: workspace root detection, `.forge/` management, gitignore scoping, `.forge/.gitignore` auto-init

**Key types (abbreviated).**
```rust
pub struct SessionId(pub [u8; 8]);          // displayed as hex like #a3f1b2c4

pub enum SessionPersistence { Persist, Ephemeral }
pub enum SessionState { Active, Archived, Ended }

pub enum RosterScope {
    SessionWide,
    Agent(AgentId),
    Provider(ProviderId),
}

pub enum RosterEntry {
    Provider { id: ProviderId, model: Option<String> },
    Skill { id: SkillId },
    Mcp { id: McpId },
    Agent { id: AgentId, background: bool },
}

pub struct ScopedRosterEntry {
    pub scope: RosterScope,
    pub entry: RosterEntry,
}

pub enum Event {
    SessionStarted { at: DateTime<Utc>, workspace: PathBuf, agent: Option<AgentId>, persistence: SessionPersistence },
    UserMessage { id: MessageId, at: DateTime<Utc>, text: Arc<str>, context: Vec<ContextRef>, branch_parent: Option<MessageId> },
    AssistantMessage { id: MessageId, provider: ProviderId, model: String, at: DateTime<Utc>, stream_finalised: bool, text: Arc<str>, branch_parent: Option<MessageId>, branch_variant_index: u32 },
    AssistantDelta { id: MessageId, at: DateTime<Utc>, delta: Arc<str> },
    BranchSelected { parent: MessageId, selected: MessageId },
    BranchDeleted { parent: MessageId, variant_index: u32 },                                    // F-145
    MessageSuperseded { old_id: MessageId, new_id: MessageId },                                 // F-143
    ToolCallStarted { id: ToolCallId, msg: MessageId, tool: String, args: Value, at: DateTime<Utc>, parallel_group: Option<u32> },
    ToolCallApprovalRequested { id: ToolCallId, preview: ApprovalPreview },
    ToolCallApproved { id: ToolCallId, by: ApprovalSource, scope: ApprovalScope, at: DateTime<Utc> },
    ToolCallRejected { id: ToolCallId, reason: Option<String> },
    ToolCallCompleted { id: ToolCallId, result: Value, duration_ms: u64, at: DateTime<Utc> },
    SubAgentSpawned { parent: AgentInstanceId, child: AgentInstanceId, from_msg: MessageId },
    BackgroundAgentStarted { id: AgentInstanceId, agent: AgentId, at: DateTime<Utc> },
    BackgroundAgentCompleted { id: AgentInstanceId, at: DateTime<Utc> },
    UsageTick { at: Option<DateTime<Utc>>, session_id: Option<SessionId>, provider: ProviderId, model: String, tokens_in: u64, tokens_out: u64, cost_usd: f64, scope: RosterScope },
    ContextCompacted { at: DateTime<Utc>, summarized_turns: u32, summary_msg_id: MessageId, trigger: CompactTrigger },
    SessionEnded { at: DateTime<Utc>, reason: EndReason, archived: bool },
    StepStarted { step_id: StepId, instance_id: Option<AgentInstanceId>, kind: StepKind, started_at: DateTime<Utc> },                  // F-139
    StepFinished { step_id: StepId, outcome: StepOutcome, duration_ms: u64, token_usage: Option<TokenUsage> },                        // F-139
    ToolInvoked { step_id: StepId, tool_call_id: ToolCallId, tool_id: String, args_digest: String },                                   // F-139
    ToolReturned { step_id: StepId, tool_call_id: ToolCallId, ok: bool, bytes_out: u64 },                                              // F-139
    McpState(McpStateEvent),                                                                                                           // F-155
    ResourceSample { instance_id: AgentInstanceId, cpu_pct: Option<f64>, rss_bytes: Option<u64>, fd_count: Option<u64>, sampled_at: DateTime<Utc> }, // F-152
}

pub enum ApprovalScope { Once, ThisFile, ThisPattern(String), ThisTool }
pub enum ApprovalLevel { Session, Workspace, User }  // F-149 — serialized lowercase
pub enum CompactTrigger { AutoAt98Pct, UserRequested }
```

`text` / `delta` carry `Arc<str>` (F-112) for cheap fan-out to multiple
IPC subscribers; they serialize identically to `String` on the wire
(plain JSON strings), so the shape pinned by
`crates/forge-core/tests/event_wire_shape.rs` is unchanged.

**Key trait.**
```rust
pub trait Credentials: Send + Sync {
    async fn get(&self, provider: &ProviderId) -> Result<Option<Secret<String>>, ForgeError>;
    async fn set(&self, provider: &ProviderId, value: Secret<String>) -> Result<(), ForgeError>;
    async fn remove(&self, provider: &ProviderId) -> Result<(), ForgeError>;
}
```

**External deps.** `serde`, `serde_json`, `tokio`, `chrono`, `thiserror`, `anyhow`, `ts-rs` (for TS type generation), `toml`, `rand`.

**Planned (later phases).** `gray_matter` for `.agents/*.md` YAML frontmatter parsing arrives with `forge-agents` in Phase 2. `secrecy` (for `Secret<T>` in the `Credentials` trait above) arrives with credential management in Phase 3. See [`docs/build/roadmap.md`](../build/roadmap.md) §12 (Phase 2 — Full layout, MCP, agents; Phase 3 — Breadth).

---

### 3.2 `forge-providers`

The provider trait and built-in implementations.

**Trait.**
```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn display_name(&self) -> &str;
    fn accent_color(&self) -> &'static str;   // hex from DESIGN.md
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<ChatChunk, ProviderError>>, ProviderError>;
    async fn usage_summary(&self, range: UsageRange) -> Result<UsageSummary, ProviderError>;
}
```

**Implementations in v1.**
- `AnthropicProvider` (SSE)
- `OpenAIProvider` (SSE, works for any OpenAI-compatible endpoint including Mistral, Groq, etc.)
- `OllamaProvider` (local, OpenAI-compat)
- `CustomOpenAIProvider` (user-configured base URL)

**Shape of `ChatRequest`.** Provider-agnostic. Internal. Translated to provider-specific requests inside each impl. Uses our own `Message`/`Tool`/`ContextBlock` types, not any provider's SDK types. Includes `parallel_tool_calls_allowed: bool` (true for read-only tool sets).

**Streaming.**
- Every provider returns `BoxStream<Result<ChatChunk, _>>`
- `ChatChunk` variants: `TextDelta(String)`, `ToolCallStart { id, name, args }`, `ToolCallArgsDelta { id, delta }`, `ToolCallFinished { id }`, `UsageDelta { in, out }`, `Done { stop_reason }`
- Providers normalize their SSE formats into these chunks

**Cost calculation.** Each provider ships a `prices.toml` table shipped in `forge-providers/data/prices.toml`. Cost per 1M input / output tokens per model. Updated via version upgrades (no hot-update in v1).

**External deps.** `reqwest` (streaming), `eventsource-stream`, `async-trait`, `futures`, `tokio`.

---

### 3.3 `forge-mcp`

MCP client and server lifecycle manager.

> See [ADR-002: MCP Integration — Scope and Manager Consolidation](./ADR-002-mcp-integration-scope-and-manager-consolidation.md) for the rationale behind the lifecycle-types-in-`forge-core` placement, the single-manager-per-daemon decision, and the `ServerState::Disabled` variant.

**Responsibilities.**
- Parse `.mcp.json` (workspace) and `~/.mcp.json` (user) using the universal `mcpServers` schema
- Import conversion for other tool formats (`forge mcp import --from ...`)
- Spawn servers: stdio transport (subprocess pipes) or http transport
- Health-check (periodic `tools/list` ping, 30s cadence)
- Restart policy: exponential backoff, max 5 tries per 10 minutes, then manual
- Expose MCP tools to sessions via a uniform `Tool` shape (matching provider-agnostic tool format)
- Surface connection state changes as events
- Classify tools as read-only or mutating via MCP `readOnly` hint (default: mutating when unset)

**API.**
```rust
pub struct McpManager { /* ... */ }

impl McpManager {
    // Construction is synchronous and cheap; connect-on-use. Spec map comes
    // from `forge_mcp::config::{load_workspace, load_user, load_merged}`.
    // Production callers use `new`; tests reach for `with_tuning` to compress
    // the 30s health-check cadence.
    pub fn new(config: BTreeMap<String, McpServerSpec>) -> Self;
    pub fn with_tuning(config: BTreeMap<String, McpServerSpec>, tuning: LifecycleTuning) -> Self;

    pub async fn start(&self, name: &str) -> Result<()>;
    pub async fn stop(&self, name: &str) -> Result<()>;
    pub async fn disable(&self, name: &str) -> Result<()>; // F-155: distinct from stop
    pub async fn enable(&self, name: &str) -> Result<()>;  // F-155: thin wrapper over start
    pub async fn call(&self, name: &str, tool: &str, args: serde_json::Value)
        -> Result<serde_json::Value>;
    pub fn state_stream(&self) -> BoxStream<'static, McpStateEvent>;
    pub async fn list(&self) -> Vec<McpServerInfo>;
}

// Import is a free function module, not a method on McpManager. Each source
// converts raw file contents into the universal spec map; filesystem I/O
// stays in the caller so converters are trivially fixture-testable.
pub enum ImportSource { VsCode, Cursor, ClaudeDesktop, Continue, Kiro, Codex }

impl ImportSource {
    pub fn convert(self, raw: &str) -> Result<BTreeMap<String, McpServerSpec>>;
}

pub fn detect_all(workspace_root: &Path, home: &Path) -> Result<DetectionReport>;
```

**F-155: cross-crate move of state types.** `McpStateEvent` and `ServerState` live in `forge-core` (`crates/forge-core/src/mcp_state.rs`), not `forge-mcp`. The move exists so `forge_core::Event::McpState(McpStateEvent)` can carry lifecycle events across the IPC boundary without creating a `forge-core → forge-mcp` dependency cycle. `forge-mcp` re-exports both types unchanged, so existing callers that reach for `forge_mcp::ServerState` / `forge_mcp::McpStateEvent` still compile.

```rust
pub enum ServerState {
    Starting,
    Healthy,
    Degraded { reason: String },
    Failed   { reason: String },
    Disabled { reason: String }, // F-155: explicit user toggle-off
}
```

`Disabled` is deliberately distinct from `Failed { reason: "stopped" }`: it marks an explicit user toggle-off so `McpManager::call` can surface the canonical `"MCP server <name> is disabled"` error string, and so `toggle_mcp_server(name, true)` knows to transition back through `Starting`. Restart-history accounting is preserved across disable/enable flaps.

**Transport.**
- stdio: `tokio::process::Command` + line-delimited JSON-RPC
- http: `reqwest` long-poll or SSE depending on server

**External deps.** `rmcp` (official MCP SDK), `tokio::process`, `reqwest`.

---

### 3.4 `forge-agents`

Agent definitions and orchestration.

**Responsibilities.**
- Parse `.agents/*.md` and `~/.agents/*.md` (frontmatter + prose) into `AgentDef` — workspace wins on name collision
- Parse `AGENTS.md` (workspace) and inject into every agent's system prompt
- Instantiate `AgentInstance` — the live running version
- Orchestration: spawn sub-agents, route tool calls, manage the per-agent event stream
- Manage background agents: user-initiated top-level agents running alongside the active chat
- Enforce `allowed_tools`, `allowed_paths`, `isolation` levels
- Cross-session memory (opt-in): read/write `~/.config/forge/memory/<agent>.md`, inject into system prompt when enabled

**Key types.**
```rust
pub struct AgentDef {
    pub id: AgentId,
    pub name: String,
    pub provider: ProviderId,
    pub model: Option<String>,
    pub system_prompt: String,
    pub allowed_tools: Vec<ToolPattern>,
    pub allowed_paths: Vec<PathPattern>,
    pub allowed_mcp: Vec<McpId>,
    pub isolation: Isolation,
    pub max_tokens: u32,
    pub approval_timeout_sec: Option<u32>,
}

pub enum Isolation {
    Trusted,                         // only valid for built-in skills
    Process,                         // default for user-defined
    Container(ContainerSpec),
}

pub struct AgentInstance {
    pub id: AgentInstanceId,
    pub def: AgentDef,
    pub parent: Option<AgentInstanceId>,
    pub is_background: bool,
    pub state: AgentState,
    // internal: event tx, tool call dispatcher, sandbox handle
}
```

**Isolation validation.** At parse time, reject any user-defined `.agents/*.md` that declares `isolation: trusted` with a clear error: "trusted isolation is reserved for built-in skills." Only `Process` and `Container(_)` are valid for user-authored agents.

**Sub-agent spawning.**
When an agent requests `agent.spawn(name, message)`, the orchestrator:
1. Checks the parent's `allowed_tools` for `agent.spawn:*`
2. Loads the named agent def
3. Validates the child's isolation (still user-agent constrained — Level 0 unavailable)
4. Instantiates in the same session, emits `SubAgentSpawned` event
5. Sub-agent is isolated per its *own* `isolation` level, not the parent's

**Background agents.** Top-level agents started by the user via `forge run` or the UI's "start background" action. `BackgroundAgentStarted` event emitted; agent runs alongside any active user chat. Completion notifies per `notifications.bg_agents` setting.

---

### 3.5 `forge-session`

The session process binary — `forged` — the heart of Forge.

> See [ADR-003: Sandbox Resource Enforcement — cgroup v2 / rlimit Hybrid](./ADR-003-sandbox-resource-enforcement-cgroup-v2-rlimit-hybrid.md) for the rationale behind the per-sandbox cgroup v2 leaf, pre_exec enrollment, and rlimit backstop fallback used by `crates/forge-session/src/sandbox.rs`.

**Entrypoint.**
```rust
// crates/forge-session/src/main.rs
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut session = Session::new(args).await?;
    session.listen_on(uds_path(&session.id())).await?;
    session.run().await
}
```

**Responsibilities.**
- Own the session's event log (append to `.forge/sessions/<id>/events.jsonl`, prepended with schema header)
- Write session metadata to `.forge/sessions/<id>/meta.toml`
- Host the agent orchestrator
- Host the tool dispatcher (fs, shell, agent.spawn, mcp.*)
- Expose an IPC server over a Unix domain socket at `$XDG_RUNTIME_DIR/forge/sessions/<id>.sock`. As of F-044 the daemon refuses to start without `XDG_RUNTIME_DIR` set — a shared-tmp fallback would be world-connectable and is no longer offered. macOS systems that want to host `forged` must set `XDG_RUNTIME_DIR` themselves (to a per-user `0o700` directory) or override the path via `FORGE_SOCKET_PATH`.
- Handle approval prompts (by emitting an event and waiting for an `ApproveToolCall` command)
- Track usage counters in-memory, flush to monthly aggregate on session end (strategy TBD per CONCEPT.md §13)
- Execute parallel reads; serialize writes/exec
- Perform context compaction at 98% or on explicit user request
- On end: archive (move to `.forge/sessions/archived/<id>/` if persist) or purge (if ephemeral)

**Lifecycle.** `forge-session` does not run an internal `Spawned/Initializing/Ready/Running/Draining/Stopped` state machine. The `SessionState` enum in `crates/forge-core/src/types.rs` (see §3.1) has exactly three variants — `Active`, `Archived`, `Ended` — and the daemon is a long-running listener whose progress is observable through the event log, not through a session-level state field.

```
  [Active] ──(SIGTERM/SIGINT, ephemeral)──> [Ended]   (session dir purged)
     │
     └──(SIGTERM/SIGINT, persist)──────────> [Archived] (moved to .forge/sessions/archived/<id>/)
```

- The daemon starts in `Active` and emits an `Event::SessionStarted` to `events.jsonl` (`crates/forge-core/src/event.rs`).
- Approval waiting is **event-driven, not a state transition**: when a tool call needs approval, the session emits `ToolCallApprovalRequested` and waits for an `ApproveToolCall` / `RejectToolCall` command from any connected client. The session remains `Active` throughout — there is no `PausedForApproval` state, and other reads can continue in parallel.
- On `SIGTERM` / `SIGINT`, `crates/forge-session/src/server.rs` emits `Event::SessionEnded { archived }` and calls `archive_or_purge` from `crates/forge-session/src/archive.rs` (per F-039). Persistent sessions are renamed into `.forge/sessions/archived/<id>/` and `meta.toml.state` is rewritten to `Archived`; ephemeral sessions have their session directory removed and the in-memory state is `Ended` (no `meta.toml` survives).
- The richer `Spawned → Initializing → Ready → Draining → Stopped` machine in earlier drafts is **not implemented** and is not currently planned for Phase 1. If a future phase grows the enum, update this section and `SessionState` in lockstep.

**Client connections.**
- Multiple clients (GUI instances, `forge session tail`, `forge session attach`) can connect simultaneously
- Each client gets a full event replay on connect, then live events
- Commands from any connected client are accepted (but approval prompts show *who* approved)

---

### 3.6 `forge-oci`

Container lifecycle.

**Scope.** Shell to `podman` (preferred) or `docker` — *not* embed a runtime in v1. We generate OCI runtime specs using `oci-spec-rs` but hand them to an external runtime.

**API.**
```rust
pub trait ContainerRuntime: Send + Sync {
    async fn pull(&self, image: &ImageRef) -> Result<(), OciError>;
    async fn create(&self, spec: Spec) -> Result<ContainerHandle, OciError>;
    async fn start(&self, handle: &ContainerHandle) -> Result<(), OciError>;
    async fn exec(&self, handle: &ContainerHandle, argv: Vec<String>) -> Result<ExecResult, OciError>;
    async fn stop(&self, handle: &ContainerHandle) -> Result<(), OciError>;
    async fn remove(&self, handle: &ContainerHandle) -> Result<(), OciError>;
    async fn stats(&self, handle: &ContainerHandle) -> Result<Stats, OciError>;
}

pub struct PodmanRuntime { /* ... */ }
pub struct DockerRuntime { /* fallback */ }
```

**First-run check.** On first container use, verify podman (or docker) is installed. If not, show a dashboard banner with install instructions for the user's OS. Do not bundle the runtime in v1.

---

### 3.7 `forge-fs`, `forge-lsp`, `forge-term`, `forge-ipc`, `forge-cli`, `forge-shell`

Briefly:

- **forge-fs** — scoped FS ops with path validation, diff generation (using `similar` crate), patch application, glob matching for approval patterns. All writes go through here; the session dispatcher refuses direct writes.
- **forge-lsp** — LSP server bootstrap: downloads and updates the 16 default servers on first use of their language. The actual LSP client runs in the webview via `monaco-languageclient` (see §9), so this crate is a *management* layer — it doesn't proxy LSP messages.
- **forge-term** — terminal backend. One `TerminalSession` per pane. Spawns children under a PTY via `portable-pty` and forwards raw bytes to consumers on a tokio `mpsc::Receiver<TerminalEvent>` (xterm.js-compatible by construction). With the off-by-default `ghostty-vt` cargo feature the PTY reader tees bytes into a dedicated driver thread owning a `libghostty_vt::Terminal`; the session then exposes `cursor_position`, `total_rows`, and `scrollback_rows` as authoritative VT-state queries. The byte stream stays byte-identical across feature settings — xterm.js remains the renderer; Rust is the source of truth for queries. Building the feature requires `zig` on the host (the sys crate vendor-fetches Ghostty C sources at build time); CI installs zig alongside the Rust toolchain.
- **forge-ipc** — the shared type definitions and framing for *both* the Tauri ↔ webview boundary and the shell ↔ session UDS boundary. Pure types + `ts-rs` derives. No runtime code.
- **forge-cli** — `clap`-based CLI binary. Reuses `forge-core` heavily. Thin. Subcommand grammar matches CONCEPT.md §5.
- **forge-shell** — Tauri binary. Registers Tauri commands (see §4.1), hosts the webview, orchestrates the IPC dance between user, UDS connections, and webview.
