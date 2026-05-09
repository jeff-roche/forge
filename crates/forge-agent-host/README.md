# forge-agent-host

The per-instance sidecar host: the `forged-agent` binary the daemon spawns under the F-608 sidecar architecture, plus the library half (`forge_agent_host`) that owns the IPC plumbing — handshake, heartbeat, panic-hook crash dump, dispatch loop, and the `IpcEventSink` adapter that lets the orchestrator's run loop emit events out the per-instance Unix domain socket. Intentionally thin: persistence, MCP, and the IPC server stay in `forge-session` so the sidecar's compile graph stays small (the architecture doc Open Questions §1 calls this out as the rationale for a separate crate).

## Role in the workspace

- Ships the `forged-agent` binary the daemon supervises one-per-`AgentInstanceId`. Resolved at runtime by `forge_shell::ipc::resolve_forged_agent_path` (sibling of the shell exe, falling back to `$PATH`).
- Depended on by: `forge-session` (test-only — the supervisor's integration tests spawn the real binary).
- Depends on: `forge-core`, `forge-ipc`, `secrecy`, `dirs`, `tokio`, `tracing-subscriber`. Pointedly does **not** depend on `forge-session`.

## Public API

- `AgentArgs` — the parsed CLI shape (`--socket <path> --instance-id <id> [--session-id <sess>]`). Hand-rolled parser; the architecture doc §1 calls out the time-to-first-token rationale for skipping `clap`.
- `run(args: AgentArgs) -> Result<()>` — drive one full sidecar lifecycle: connect → handshake → loop → exit. The library entry the binary's `main` calls; the integration tests call it directly without forking.
- `init_tracing()` — install the JSON-lines `tracing-subscriber` on stderr (architecture doc §10). Idempotent.
- `install_panic_hook(identity)` — install the panic hook that (1) writes a `CrashDump` JSON file under `<XDG_DATA_HOME or ~/.local/share>/forge/crashes/<session-id>/<instance-id>-<unix-ts>.json` (mode `0o700`) and (2) queues a `SidecarMessage::Crashed` frame for the dispatch loop to flush on shutdown. Best-effort — a hard segfault never produces either.
- `IpcEventSink` — `forge_core::EventSink` impl that frames every emit as `SidecarMessage::Event { seq, event }` and writes it on the daemon-facing UDS. Same trait the in-process `forge_session::orchestrator::run_turn` consumes; the orchestrator body is transport-agnostic.
- `CredentialStash` — per-sidecar `SecretString` map keyed by `provider_id`. `Credentials` frames inbound from the daemon land here; `Debug` is custom-redacted so a stray `tracing::debug!` cannot reveal cached identifiers.
- `DaemonHelloState` — single-slot store for the daemon-side `Hello` payload (agent definition, allowed paths, workspace, provider spec, sandbox level). First write wins; duplicates log at `warn` and are dropped.
- `EventSeq` — monotonic per-process sequence allocator threaded through both the heartbeat task and the run-turn handler so every outbound frame is totally ordered. Starts at 1 (0 is reserved as a "never emitted" sentinel).
- `build_hello_ack(daemon_pid)` — convenience constructor used by tests acting as the daemon side of the handshake.

## Integration with `SidecarSupervisor`

The supervisor lives in `forge_session::sidecar` and owns the socket bind, fork, restart, and tear-down. It interacts with this crate over the wire only — there is no Rust-level dependency edge from `forge-session` to `forge-agent-host` (that would re-introduce the compile-graph cost we wrote the binary to avoid). The contract:

1. Supervisor binds `<socket_dir>/<instance-id>.sock` (anti-TOCTOU, mode `0o700`); see `crates/forge-session/src/sidecar/mod.rs`.
2. Supervisor forks `forged-agent --socket <path> --instance-id <id> --session-id <sess>`.
3. Sidecar connects, sends `Hello`, awaits `HelloAck` (10 s deadline; supervisor enforces its own outer handshake deadline).
4. Heartbeat: 1 Hz `Heartbeat` frame; supervisor times out at 5 s and recycles the child.
5. `RunTurn` / `Credentials` / `ToolCallApproved|Rejected` / `CompactTranscript` / a daemon-side follow-up `Hello` flow daemon → sidecar; `Event` / `Heartbeat` / `Crashed` / `ToolCallApprovalRequest` flow sidecar → daemon.
6. `Shutdown { grace_ms }` triggers a clean half-close after a flush; absent that the supervisor falls back to SIGTERM-then-SIGKILL on `Drop`.

The full state diagram and restart policy are in [`docs/architecture/agent-sidecar.md`](../../docs/architecture/agent-sidecar.md) §3.

## Testing

- **Unit tests** (`cargo test -p forge-agent-host --lib`) — argv parsing, `EventSeq` monotonicity, `CredentialStash` `Debug` redaction. No network, no fork.
- **`forged_agent_lifecycle`** integration test — drives a real `forged-agent` child against a test-side UDS that acts as the daemon. Asserts handshake (`Hello` + `HelloAck`), heartbeat cadence, the stub `RunTurn` event sequence (`StepStarted` → `AssistantMessage { text: "[stub]" }` → `StepFinished`), and a clean shutdown drain.
- **`forged_agent_credentials`** integration test — pushes a `Credentials` frame under `FORGE_LOG=trace` and audits the captured stderr to confirm the credential value never appears at any tracing level (architecture doc §5 audit policy).
- **`FORGE_TEST_PANIC_ON_RUNTURN`** — undocumented test-only env var that drives the panic hook → on-disk crash-dump path end-to-end. Used by `forge-session`'s `sidecar_crash_dump` integration test; not a public contract.

Run the suite standalone with `cargo test -p forge-agent-host`. The supervisor side is exercised separately under `forge-session` (`tests/sidecar_supervisor.rs`, `tests/bg_agents_sidecar.rs`).

## Further reading

- [Agent sidecar architecture](../../docs/architecture/agent-sidecar.md) — the F-608 design doc, including the 9-step rollout, restart policy, and known security limitations.
- [IPC contracts](../../docs/architecture/ipc-contracts.md) — sidecar `SidecarMessage` shapes and the inline-vs-event rule (§4.0).
- [Crate architecture](../../docs/architecture/crate-architecture.md) — workspace dependency map.
