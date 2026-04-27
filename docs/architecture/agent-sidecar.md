# Agent Sidecar Architecture

> Status: Proposed (F-608). Resolves the open design questions captured in issue #654 so an implementer can pick this up directly. Companion plan for F-451 (real PIDs into `ResourceMonitor`).

## Overview

Today every Forge agent is a tokio task inside `forged`. The session orchestrator (`crates/forge-session/src/orchestrator.rs`) drives the provider request loop in-process; `BackgroundAgentRegistry::start` (`crates/forge-session/src/bg_agents.rs:199`) registers a logical `AgentInstance` but never forks. The result: `ResourceMonitor::track` is fed the daemon's own PID and is elided by the daemon-PID guard at `crates/forge-session/src/resource_monitor.rs:235`, samples never reach the UI (`crates/forge-session/src/bg_agents.rs:265`), and a single panicking provider stream takes the whole daemon down.

The sidecar architecture moves the per-turn request loop into a child process, one per `AgentInstanceId`. The daemon stays the authority: it owns credentials, persistence, MCP, the event log, and the shell-facing UDS. Sidecars are dumb workers — they receive a turn description, drive the provider, stream events back, and exit (or stay warm) on the daemon's command. The shell's IPC contract is unchanged.

```
                    ┌──────────────────────────────────┐
forge-shell ◀──UDS──┤             forged              │
(Tauri / CLI)       │  ┌──────────────────────────┐   │
                    │  │ Session orchestrator      │   │
                    │  │ event log, persistence    │   │
                    │  │ McpManager, Credentials   │   │
                    │  │ BackgroundAgentRegistry   │   │
                    │  │ ResourceMonitor           │   │
                    │  └──────────────────────────┘   │
                    │     │   ▲                       │
                    │     │   │ event frames          │
                    │     │   │ (forge_core::Event)   │
                    │     ▼   │                       │
                    │  ┌──────────────────────────┐   │
                    │  │ SidecarSupervisor         │   │
                    │  │  - spawns                 │   │
                    │  │  - per-instance UDS pair  │   │
                    │  │  - restart policy         │   │
                    │  └──────────────────────────┘   │
                    └──────────────┬───────────────────┘
                                   │ UDS (length-prefixed JSON)
                  ┌────────────────┼────────────────┐
                  ▼                ▼                ▼
          ┌───────────────┐ ┌───────────────┐ ┌───────────────┐
          │ forged-agent  │ │ forged-agent  │ │ forged-agent  │
          │ instance A    │ │ instance B    │ │ instance C    │
          │ provider loop │ │ provider loop │ │ provider loop │
          │ (Level-2: shells out to podman per tool)         │
          └───────────────┘ └───────────────┘ └───────────────┘
```

---

## Design Decisions

### 1. Binary

**Decision.** Ship a dedicated `forged-agent` binary in the existing `forge-session` crate (or a new sibling `forge-agent-host` crate, decided in the implementation plan below — leaning toward "new crate"). The `forge` user-facing CLI does **not** grow an `agent run` subcommand.

**Rationale.**
- The `forge` CLI is user-facing surface area; `forged-agent` is a private daemon-supervisor implementation detail. Mixing them invites users to run `forge agent run --instance-id=…` directly and discover undocumented IPC.
- A separate binary lets us link only the provider/orchestrator deps the sidecar needs and skip the shell-side argument parsing crates (`clap`, etc.) — meaningful because the sidecar boots on the user-visible time-to-first-token path.
- The `forged` daemon already lives in `forge-session/src/main.rs`; locating `forged-agent`'s `main.rs` next to it (binary entry: `crates/forge-session/src/bin/forged-agent.rs`, or a new crate `crates/forge-agent-host/`) keeps the supervisor → child binary search logic identical to today's `find_forged_binary` in `crates/forge-cli/src/main.rs:434`.

**Alternative considered.** `forge agent run --instance-id <id>` subcommand on `forge-cli`. Rejected: pulls IPC + provider deps into the user-facing CLI's compile graph and makes the sidecar's private wire protocol part of the apparent CLI surface.

---

### 2. IPC Transport

**Decision.** Per-instance Unix domain socket using the same length-prefixed JSON framing (`forge_ipc::write_frame` / `read_frame` at `crates/forge-ipc/src/lib.rs:247`) the daemon ↔ shell wire already uses. The daemon binds the socket before spawning the sidecar; the sidecar receives the path via `--socket /run/user/$uid/forge/<session>/<instance-id>.sock` and connects on startup.

**Rationale.**
- Reusing `forge-ipc`'s framing pattern (same `[u32 BE length][JSON body]` shape, same 4 MiB cap, same `serde(tag = "t")` discriminated union) means zero new framing code and one mental model for IPC throughout Forge.
- A dedicated socket per instance gives natural multiplexing: the supervisor holds N sockets, each with its own pump task, and a stuck/crashed sidecar cannot wedge a peer.
- stdio JSONL was considered and rejected — capturing stderr for tracing is then awkward (we want stderr free for `tracing` output captured by the supervisor), and crash dumps over stdio are racy.
- A new wire crate would be premature; the sidecar protocol lives in `forge-ipc` as a sibling tagged-union (different from but adjacent to `IpcMessage`).

**Protocol sketch.** New types in `forge-ipc` (or a new `forge_ipc::sidecar` submodule) under tag `t`:

| Direction | Variant | Payload |
|---|---|---|
| daemon → sidecar | `Hello` | `{ proto: u32, instance_id, agent_def, allowed_paths, workspace_path, provider_spec, sandbox_level, telemetry_endpoint }` |
| sidecar → daemon | `HelloAck` | `{ pid, started_at, schema_version }` |
| daemon → sidecar | `RunTurn` | `{ turn_id, msg_id, text, agents_md, branch_parent, branch_variant_index, byte_budget }` |
| daemon → sidecar | `Credentials` | `{ provider_id, secret_handle }` (push model — see §5) |
| daemon → sidecar | `ToolCallApproved` / `ToolCallRejected` | mirrors `crates/forge-ipc/src/lib.rs:191` |
| daemon → sidecar | `CompactTranscript` | `{}` (proxied from the shell) |
| daemon → sidecar | `Shutdown` | `{ grace_ms: u64 }` |
| sidecar → daemon | `Event` | `{ seq: u64, event: forge_core::Event }` — same `IpcEvent` shape as today |
| sidecar → daemon | `ToolCallApprovalRequest` | `{ id, name, args }` (so the daemon can forward to the shell) |
| sidecar → daemon | `Heartbeat` | `{ at: DateTime<Utc>, pending_turns: u32 }` (1 Hz; supervisor times out at 5 s) |
| sidecar → daemon | `Crashed` | `{ panic_message: String, backtrace: Option<String> }` (best-effort, written before the process exits via the panic hook) |

Note that `forge_core::Event` is the canonical wire shape on both legs; the sidecar produces it directly, the daemon's session writes it to the event log unchanged. No translation layer.

---

### 3. Lifecycle

**Decision.** Daemon spawns + supervises with bounded auto-restart (3 retries inside a 60-second window, then escalate to `SessionFailed`). Background agents own a `SidecarHandle` instead of just an `AgentInstanceId`.

**Restart policy:**
- Crash detected (Heartbeat timeout, EOF before `Shutdown` ack, or non-zero exit code): increment retry counter for that `AgentInstanceId`.
- If counter ≤ 3 within the last 60 s: re-spawn with the same `instance_id`, replay any pending `ToolCallApprovalRequest` from the daemon's outbox.
- Otherwise: emit `Event::AgentEvent::Failed { id, reason: "sidecar crashed N times" }` and `Event::BackgroundAgentCompleted` (failure path — already tested at `crates/forge-session/src/bg_agents.rs:570`).

**State diagram (per instance):**

```
       ┌─────────────────────┐
       │       (none)        │
       └──────────┬──────────┘
                  │ start(agent_name, prompt)
                  ▼
       ┌─────────────────────┐
       │      Spawning       │  fork forged-agent, wait for HelloAck
       └──────────┬──────────┘
                  │ HelloAck received
                  ▼
       ┌─────────────────────┐
       │      Running        │  RunTurn / Event pumping
       └──────────┬──┬──┬────┘
                  │  │  │
       Heartbeat  │  │  └─ Shutdown (grace_ms) ──┐
       timeout    │  │                            │
       │          │  └─ Crashed frame             │
       ▼          ▼                               ▼
       ┌─────────────────────┐         ┌──────────────────┐
       │     Restarting      │         │   Stopping       │
       │  (retries < 3 in    │         │  wait grace_ms   │
       │   60s window)       │         │  then SIGTERM    │
       └──────────┬──────────┘         └────────┬─────────┘
                  │ retry exhausted              │ exit
                  ▼                              ▼
       ┌─────────────────────┐         ┌──────────────────┐
       │       Failed        │         │     Stopped      │
       │ emits Failed event  │         │ untrack monitor  │
       └─────────────────────┘         └──────────────────┘
```

`BackgroundAgentRegistry::stop` (today: orchestrator.stop) drives the `Stopping` transition. `promote` is unchanged — it stays a UX re-attribution and does not touch sidecar lifecycle. The forwarder logic at `crates/forge-session/src/bg_agents.rs:288-355` continues to translate orchestrator-level `Completed`/`Failed` into `BackgroundAgentCompleted`; the new wrinkle is that the orchestrator's `Failed` is now the supervisor's escalation, not an in-process panic.

---

### 4. State Migration

**Decision.** Sidecars die with the daemon. SIGTERM on the daemon cascades a `Shutdown { grace_ms: 2000 }` to every sidecar; daemon `Drop` falls back to SIGTERM-then-SIGKILL on the children.

**Survives daemon restart:** the event log on disk (the canonical replay source — `crates/forge-session/src/server.rs:13` already pins `read_since`-based replay), the workspace, agent definitions on disk, MCP config.

**Does not survive:** in-flight turns, pending tool-call approvals (already true today), any per-sidecar warm caches (provider HTTP client connection pools, tokenizers, etc.).

**Why.** The daemon owns authority context: credential store handles, the active workspace path, the `McpManager` lifecycle. A sidecar that outlives its daemon is a sidecar without an authority — the credential store is gone, the supervisor is gone, the event log is gone. Re-attaching to a new daemon would require renegotiating identity end-to-end and serializing all the orchestrator state. That's a multi-quarter project; skip it.

**Resume after daemon restart:** the next `forged` boot reads the event log, finds the last `BackgroundAgentStarted` without a matching `Completed`, and either drops the row (current behaviour for crashed sessions) or — phase-4 follow-up — re-spawns a sidecar from the persisted last user message. F-608 chooses the simpler "drop on restart" path; the user's existing motion is to re-issue the prompt.

---

### 5. Authority Model

**Decision.** Daemon **pushes** credentials over the IPC at sidecar startup (or on a `RunTurn` that needs them). Sidecars do **not** open the keyring directly.

**Rationale.**
- Forge's keyring story (F-587, see `CredentialContext` at `crates/forge-session/src/orchestrator.rs:38`) already pulls a single per-turn credential just-in-time. Pushing it onto the sidecar at the same beat keeps the audit log narrow: one process — the daemon — interacts with the keyring.
- A compromised sidecar cannot enumerate the keyring; its blast radius is exactly the credentials the daemon chose to send (which today is one provider per turn).
- The `secrecy` crate's `SecretString` already wraps these values; the IPC message uses `secret_handle: Vec<u8>` zeroized after deserialization on the sidecar side. The `serde(skip)` discipline applied to `CredentialContext::store` (today at `crates/forge-session/src/orchestrator.rs:43-52`) provides the template.

**Audit story.** The daemon keeps logging `provider_id` + `hit/miss` at `trace` level (already done at `crates/forge-session/src/orchestrator.rs:71`). The new emission is a `tracing::trace!` on credential **push** ("pushed credential to sidecar instance_id=…, provider_id=…"). The sidecar logs **receipt only** (no value, no length); a sidecar that receives an unexpected `Credentials` frame (e.g. for a provider it never asked for) drops the message and logs at `warn`.

---

### 6. Sandbox Interaction

**Decision.** The agent sidecar sits **outside** the Level-2 podman container. Tool invocations inside the sidecar still go through `Level2Session::exec_step` (`crates/forge-session/src/sandbox/level2.rs:242`) per-tool. The container is one-per-session (or one-per-agent-instance, configurable) — the sidecar does not get its own container.

**Rationale.**
- Level-2 is a **per-tool** boundary, not a per-agent boundary. The existing `Level2Session::create` is called once at session start and amortizes the ~2 s pull/create cost across every tool call (`crates/forge-session/src/sandbox/level2.rs:198-230`). Wrapping the sidecar in the container would re-incur that cost for every sidecar startup and tie crash recovery to `podman rm -f` cleanups.
- Persistent file-system state inside the container survives across tool calls within a session (the user's expectation); putting the sidecar inside the container with a per-instance lifetime would cycle that state on every restart.
- The sidecar's own footprint is a regular OS process, accountable via `/proc/$pid/stat` — exactly what `ResourceMonitor`'s Linux sampler already reads.

**Open follow-up (out of scope for F-608):** when F-595/F-596 evolves to per-agent containers (rather than per-session), the container becomes the sidecar's wrapper. That's a deliberate ordering: sidecar fork lands first, then the per-agent container layers on top.

---

### 7. Performance

**Budget.**
- **Sidecar startup cost ceiling:** 200 ms (cold start), measured from `fork(2)` to `HelloAck` on the daemon side. Sidecars are long-lived per agent instance, so this is paid once per `start_background_agent` and once per session-root spawn — never on the per-turn path.
- **Per-turn IPC overhead ceiling:** < 50 ms p99 added to the time-to-first-token, where:
  - `RunTurn` serialize + write: ~1 ms
  - `Event::AssistantDelta` round-trip: a few µs each, but at high token rates this dominates — measure by counting frames emitted per token vs. baseline.
- **Memory ceiling:** each sidecar adds ~15 MiB resident (its own tokio runtime + provider HTTP client + tokenizer). 5 concurrent background agents = ~75 MiB above baseline. Acceptable.

**Measurement approach.**
- Add a `forge-bench` regression: spawn one sidecar, drive 1000 mock-provider tokens through it, compare per-token wallclock vs. the in-process baseline. Fail the build if p99 regression > 50 ms.
- The existing `Mock` provider (`forge_providers::MockProvider`) is the right tool: it generates deterministic streams without network cost.
- Hook the `tracing::span!` already wrapping `run_turn` so a `tokio-console` user can see the IPC overhead per turn.

---

### 8. Rollout

**Decision.** Feature-flag via env var `FORGE_AGENT_SIDECAR=1`. The flag flips on at the `BackgroundAgentRegistry` boundary: `start()` either takes the new sidecar path or the legacy in-process path (today's behaviour). Default is **off** for the F-608 PR; flipped to **on** in a follow-up PR after a one-week soak in nightly.

**Rationale.**
- The unit-test suite for `BackgroundAgentRegistry` (`crates/forge-session/src/bg_agents.rs:415-720`) does not depend on the fork path. Keeping the legacy code path means those tests continue to validate the lifecycle invariants while the sidecar path validates the new ones.
- Hard-flip in one release would gate every PR on the entire sidecar story being green; the flag lets us land the seam first and the implementation in pieces.
- The flag lives at one site (the `start` function); we are not threading it through the request loop. No combinatorial test explosion.

**Sunset.** Remove the flag and the legacy path one milestone after the on-by-default flip — same disposition as F-565 / F-575.

---

### 9. Resource Monitor Hook (F-451)

**Decision.** `BackgroundAgentRegistry::start` keeps its signature (`Result<AgentInstanceId, BgAgentError>`) but its body changes:

```rust
let instance = orchestrator.spawn(def, ctx).await?;
let id = instance.id.clone();
let handle = supervisor.spawn(id.clone(), spawn_params).await?;
//                                                       ^^ contains child_pid
self.monitor.track(id.clone(), handle.pid).await;
```

`handle.pid` is the real child PID. The daemon-PID guard at `crates/forge-session/src/resource_monitor.rs:235` keeps catching wired-but-misused calls; production now flows real PIDs and `Event::ResourceSample` reaches subscribers exactly as today's tests at `crates/forge-session/src/bg_agents.rs:641-677` already prove.

**Sampling source:** the existing `/proc/<pid>/stat` Linux probe (`crates/forge-session/src/resource_monitor.rs:1-100` describes the architecture). No sidecar-self-reported metrics: the daemon owns the truth, sidecars never tell the daemon how much CPU they're using because that creates a malicious-sidecar misreporting vector.

**On supervisor restart:** `monitor.untrack(&id).await` before the new `track(id, new_pid)` (the function's idempotency guard at `crates/forge-session/src/resource_monitor.rs:249-257` already permits this without leak).

---

### 10. Observability

**Logs.** Each sidecar runs a `tracing-subscriber` writing JSON-lines to `stderr`. The supervisor pipes both streams through a per-instance forwarder that prefixes with `instance_id=…` and re-emits via the daemon's tracing infrastructure. No new file paths.

**Crash dumps.** On panic, the sidecar's panic hook (installed in `forged-agent`'s `main`) writes a single-frame `Crashed { panic_message, backtrace }` to its IPC socket *before* exiting — best-effort, may be lost on a hard segfault. The supervisor catches the frame (or the EOF) and writes the dump under `~/.local/share/forge/crashes/<session-id>/<instance-id>-<unix-ts>.json`. Project-local `.forge/crashes/` was considered and rejected: workspace dirs are user-edited, crash dumps under VCS are noise.

**Traces.** The sidecar's spans (`run_turn`, `run_request_loop`, etc.) become daemon-side spans with `instance_id` attached. A `forge_session::sidecar` tracing target makes the supervisor-emitted lifecycle events filterable.

**Metrics surfaced via existing channels.**
- `Event::ResourceSample` (already on the wire): real per-sidecar CPU/RSS/FD pills land in the AgentMonitor automatically once §9 wires the PID.
- New event variant — **deliberately deferred**. F-608 does not introduce sidecar-lifecycle wire events; the existing `BackgroundAgentStarted` / `BackgroundAgentCompleted` / `AgentEvent::Failed` cover the three transitions the UI needs. The supervisor's restarts are invisible to the shell.

---

## Implementation Plan

Each step is independently reviewable; later steps assume earlier ones merged.

### Step 1: Define the sidecar wire protocol in `forge-ipc`

- **Files to touch:** `crates/forge-ipc/src/lib.rs` (add a `pub mod sidecar` with the new tagged enum from §2; export `SidecarMessage`, `SidecarRunTurn`, `SidecarHello`, `SidecarShutdown`, etc.)
- **Contract:**
  - Frames use the existing `read_frame_into` / `write_frame` helpers — no new framing.
  - 4 MiB cap is reused.
  - Include round-trip serde tests (`sidecar_message_roundtrip`).
- **Acceptance:** `cargo test -p forge-ipc` passes; the new module exports compile from a downstream `forge-session` `use forge_ipc::sidecar::...` line.

### Step 2: Create the `forged-agent` binary

- **Files to touch:** new `crates/forge-agent-host/` (or `crates/forge-session/src/bin/forged-agent.rs`); add to workspace `Cargo.toml`.
- **Contract:**
  - Binary signature: `forged-agent --socket <path> --instance-id <id>`.
  - On startup: connect, send `Hello`, await `HelloAck`-equivalent, install panic hook that writes `Crashed`.
  - Process the `RunTurn`, `ToolCallApproved/Rejected`, `Credentials`, `Shutdown` messages by calling into a refactored `run_turn` (see step 3).
  - Heartbeat task: 1 Hz `Heartbeat` frame.
  - Tracing: subscriber on stderr, JSON-lines.
- **Acceptance:** `forged-agent` connects to a test UDS, exchanges a synthetic `RunTurn` with a `MockProvider`, emits `Event::AssistantMessage` frames, exits cleanly on `Shutdown`.

### Step 3: Refactor `run_turn` to be transport-agnostic

The current `run_turn` (`crates/forge-session/src/orchestrator.rs:165`) takes an `Arc<Session>` and calls `session.emit(Event)` directly. We need to split the emission target so the same body runs in-process (writing to the local `Session`) and in-sidecar (writing frames out the IPC).

- **Files to touch:** `crates/forge-session/src/orchestrator.rs`, `crates/forge-session/src/session.rs`.
- **Contract:**
  - Introduce a `trait EventSink { async fn emit(&self, e: Event) -> Result<()>; }`.
  - `Session` implements `EventSink` with the existing body.
  - A new `IpcEventSink` (in the sidecar binary) writes `SidecarMessage::Event(IpcEvent { seq, event })` frames.
  - `run_turn` and `run_request_loop` take `&dyn EventSink` (or `Arc<dyn EventSink>`) instead of `Arc<Session>`.
- **Acceptance:** Existing `crates/forge-session/tests/step_events.rs` still passes (the in-process `Session` sink is the only one used by those tests); `cargo test -p forge-session` is green.

### Step 4: Implement the `SidecarSupervisor`

- **Files to touch:** new `crates/forge-session/src/sidecar.rs`; wire from `crates/forge-session/src/server.rs`.
- **Contract:**
  - `SidecarSupervisor::new(socket_dir, forged_agent_path) -> Self`.
  - `SidecarSupervisor::spawn(instance_id, params) -> Result<SidecarHandle>` that:
    - Binds a UDS at `<socket_dir>/<instance_id>.sock` with the same anti-TOCTOU pattern used by `bind_uds_safely` at `crates/forge-session/src/server.rs:678`.
    - Forks `forged-agent`, captures its `pid`.
    - Awaits `Hello` from the child within the existing handshake deadline (`crates/forge-session/src/server.rs:51`).
    - Sets up bidirectional pump tasks (daemon → sidecar command tx; sidecar → daemon event rx).
  - `SidecarHandle { pid, instance_id, command_tx, shutdown }` — `shutdown` sends `Shutdown { grace_ms: 2000 }` and joins.
  - Restart logic per §3.
- **Acceptance:** New tests `crates/forge-session/tests/sidecar_supervisor.rs`:
  - `spawn_returns_real_pid`
  - `crash_within_window_restarts`
  - `crash_exhausts_retries_emits_failed`
  - `shutdown_grace_window_then_sigterm`

### Step 5: Wire `BackgroundAgentRegistry` to the supervisor (gated by `FORGE_AGENT_SIDECAR`)

- **Files to touch:** `crates/forge-session/src/bg_agents.rs`.
- **Contract:**
  - When the env flag is set: call `supervisor.spawn(...)` after `orchestrator.spawn(...)`, pass `handle.pid` into `monitor.track`.
  - When unset: today's behaviour (no fork, daemon-PID no-op).
  - Both paths emit `BackgroundAgentStarted` identically.
- **Acceptance:**
  - Existing `crates/forge-session/src/bg_agents.rs:451-720` tests still pass with flag unset (default).
  - New gated test `start_with_sidecar_flag_real_pid_emits_resource_sample` proves §9: with `FORGE_AGENT_SIDECAR=1`, `Event::ResourceSample` arrives on the registry bus for the new instance — closing F-451.

### Step 6: Plumb credential push (§5)

- **Files to touch:** `crates/forge-session/src/orchestrator.rs` (the `pull_active_credential` path at line 68), the new sidecar message handlers.
- **Contract:**
  - Daemon-side: when `CredentialContext` is present, push a `Credentials` frame to the sidecar before sending `RunTurn`.
  - Sidecar-side: stash the credential in a `SecretString`, hand it to the provider's per-request auth shape (Phase-1 keyless OllamaProvider — no-op; Anthropic/OpenAI when they land).
- **Acceptance:** Trace log emits `pushed credential` with `provider_id` + `instance_id`. No credential value appears in any log at any level.

### Step 7: Crash-dump writer and observability glue (§10)

- **Files to touch:** new `crates/forge-session/src/sidecar/crashes.rs`; add panic-hook installation in `forged-agent`'s `main`.
- **Contract:**
  - Crash dumps land at `<XDG_DATA_HOME or ~/.local/share>/forge/crashes/<session-id>/<instance-id>-<unix-ts>.json`.
  - Filename collision impossible (timestamp + instance id); directory created with mode 0o700.
- **Acceptance:** Test injects a panicking provider into a sidecar, asserts the crash file appears with the panic message intact.

### Step 8: Performance benchmark

- **Files to touch:** new `crates/forge-session/benches/sidecar_overhead.rs`.
- **Contract:** Bench drives 1000 mock tokens through both the in-process and sidecar paths, prints p50/p99 deltas.
- **Acceptance:** Sidecar p99 overhead per token < 50 µs (well within the 50 ms per-turn budget); cold-start p99 < 200 ms. Hand-checked on Linux x86_64 in CI.

### Step 9: Documentation + flip default

- **Files to touch:** this document; `docs/architecture/overview.md` cross-link; `CHANGELOG.md`.
- **Acceptance:** A follow-up PR (post-soak) flips `FORGE_AGENT_SIDECAR=1` to default-on by inverting the gate and removes the flag in a subsequent milestone.

---

## Open Questions (decide during impl)

1. **Crate placement of the new binary.** New `crates/forge-agent-host/` vs. `crates/forge-session/src/bin/forged-agent.rs`. **Recommended default:** new crate, because it lets the sidecar avoid pulling in `forge-session`'s persistence and IPC-server code. If the dep graph turns out to require half of `forge-session` anyway, fall back to the `bin/` placement.
2. **One sidecar per agent instance vs. one per session.** F-608 chooses **per-instance** because that's the only way `ResourceMonitor`'s per-instance pills are honest. A future "warm pool" optimization could keep one shared sidecar per provider × auth — defer until the per-instance startup cost actually shows up in a real benchmark.
3. **`Heartbeat` timeout cadence.** 1 Hz heartbeat / 5 s timeout is a starting point; tune based on the perf bench in step 8. If healthy turns regularly approach 5 s of cooperative silence (e.g. Anthropic during long tool execution), bump to 15 s.
4. **MCP within the sidecar.** Today's `McpManager` lives in the daemon (`crates/forge-session/src/server.rs:421`). The sidecar calls into MCP **via the daemon** — i.e. tool invocations that need MCP go back over the IPC as a `ToolCallApprovalRequest`-shaped message and the daemon executes against `McpManager`. Alternative: clone the MCP client list into each sidecar. **Recommended default:** keep MCP in the daemon — one connection pool, one health story. Re-evaluate if MCP-tool-heavy turns dominate the latency budget.

---

## Risks

1. **Risk: IPC chatter on streaming-token paths.** Every `AssistantDelta` becomes one IPC frame, where today it's an in-process broadcast send. At 100 tok/s the IPC adds 100 syscalls/s per sidecar. **Mitigation:** the per-frame cost is dominated by `serde_json::to_vec` + a `write_all`, both microsecond-scale on UDS. The benchmark in step 8 measures it directly; if it's a problem, batch deltas into 10 ms windows on the sidecar side (a small buffer + flush, no protocol change).

2. **Risk: Restart loops on a poisoned `RunTurn`.** A turn that triggers a panic in the provider crate gets retried up to 3 times — three crash files, three IPC frames, three log spasms before the daemon escalates. **Mitigation:** the supervisor stamps each retry with the same `turn_id`; if the same turn_id panics the same way twice, escalate immediately rather than running the third retry. (This is a small addition to the restart logic in step 4.)

3. **Risk: Credential leakage via logs.** The sidecar runs its own `tracing-subscriber`. A careless `tracing::debug!("req = {:?}", req)` somewhere in `forge-providers` could expose a header value. **Mitigation:** Add a clippy lint or a CI grep for `Debug` impl on credential-bearing types; the existing `CredentialContext::Debug` impl (`crates/forge-session/src/orchestrator.rs:43`) is the template — never print the secret. Audit `forge-providers` for the same discipline before flipping the flag default-on.
