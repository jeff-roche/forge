# Isolation Model

> Extracted from CONCEPT.md §6 and IMPLEMENTATION.md §8 — the three isolation levels, approval model, and sandboxing implementation

---

## 6. Sandboxing model

Agents and MCP servers are untrusted code running with access to the user's files and network. The sandbox story has to be real.

### 6.1 Three levels of isolation

| Level | Mechanism | Who uses it |
|---|---|---|
| **0 — Trusted** | None. Runs in session process. | **Built-in skills only.** User-defined agents cannot declare this. |
| **1 — Process** | Separate OS process, restricted env, fs-scope per `allowed_paths`. | **Default for user-defined agents and MCP servers.** |
| **2 — Container** | OCI (podman preferred, docker fallback). Per-session rootfs and user namespace. Network policy, resource caps, capability hardening, and image trust are designed but not yet wired (see §8.3). | Opt-in for risky agents or CI-style runs. |

User-defined agents that omit `isolation:` get Level 1 automatically. Level 0 is reserved for code Forge ships.

### 6.2 Frontmatter declaration

```yaml
---
name: refactor-bot
provider: anthropic
model: sonnet-4.5
isolation: process              # or: container (trusted is built-in only)
allowed_tools: [fs.read, fs.write, shell.exec]
allowed_paths: ["./src", "./tests"]
allowed_mcp: [github]
max_tokens: 8000
---
```

Prose body (after frontmatter) is the system prompt.

### 6.3 Approval and isolation are orthogonal

Sandbox enforces **runtime containment**. Approval enforces **human-in-the-loop**. Both apply independently.

| Tool category | Level 0 | Level 1 | Level 2 |
|---|---|---|---|
| Read | auto-approved | auto-approved | auto-approved |
| Write | approval required | approval required | approval required |
| Execute | not allowed | approval required | approval required |
| Network | not allowed | open (no approval per call) | open today; `allowed_hosts` enforcement is future work ([F-642](https://github.com/forge-ide/forge/issues/678)) |

A containerized agent still needs approval for writes. A trusted built-in skill doing a read doesn't need approval. The two systems do different jobs.

### 6.4 Level 1 networking is open

Process-isolated agents can reach the network freely. Forge does not firewall at the process level. MCP servers and built-in tools like `fetch` do their own allow-listing. This is a deliberate tradeoff — Level 1 is a filesystem and privilege sandbox, not a network sandbox.

Level 2 is **also open by default today** — `--network none` and CNI allow-listing are tracked as future work ([F-642](https://github.com/forge-ide/forge/issues/678)). Until that lands, neither level enforces network restriction at the runtime boundary.

### 6.5 Sub-agents use independent isolation

A spawned sub-agent uses its own declared isolation level, not the parent's. Since Level 0 is built-in-only, this means user-defined sub-agents can only be Level 1 or Level 2 — no escape hatch exists for user code to gain trusted status via spawn.

### 6.6 Approval granularity

Approval scope is chosen at the prompt. The user picks:
- **Once** — approve this exact call only; next one prompts again
- **This file** — approve this tool for this specific file/path for the session
- **This pattern** — approve this tool for the matching glob (e.g. `./src/*`) for the session
- **This tool** — approve the tool type entirely for the session (e.g. all `fs.write`)

Whitelist scope is **session only** — never persisted across sessions. At session end, all approvals reset. Keyboard: `R` reject, `A` approve once, `F` approve file, `P` approve pattern, `T` approve tool.

### 6.7 Container management

Forge ships an OCI manager using `oci-spec-rs` and shelling to `podman` or `docker`. v1 requires the user to have podman or docker installed; bundling a runtime is deferred. Dashboard onboarding detects missing runtimes and surfaces install instructions. Images pulled on first use, layers cached.

---

## 8. Sandboxing implementation

### 8.1 Level 0 — Trusted
Tool calls run in the session process. **Only built-in skills** (code Forge ships, never user-authored agents). No subprocess invocation. Enforced at agent parse time: any `isolation: trusted` in a user-authored `.agents/*.md` is rejected.

### 8.2 Level 1 — Process (default for user agents + MCP servers)

Implementation:
- `tokio::process::Command`
- `clearenv`; re-inject whitelisted env vars only (`PATH`, `HOME`, `LANG`, `LC_*`, session-specific `FORGE_SESSION_ID`)
- Path access enforced by `forge-fs`: every `fs.*` tool validates the path against the agent's `allowed_paths` glob
- **Network is open at Level 1.** No per-agent firewall. MCP servers and the built-in `fetch` tool do their own allow-listing. Users who need network restriction use Level 2.
- CPU/RAM: soft limits via `setrlimit` (Linux/macOS)
- **Per-sandbox process ceiling via cgroup v2 `pids.max` (F-149).** Each sandbox gets its own leaf under the daemon's cgroup parent so a misbehaving tool cannot starve sibling sandboxes or the daemon itself. Linux-only; requires the host to delegate the `pids` controller to the daemon's slice (default on systemd user sessions). On non-delegated hosts (cgroup v1, containers without delegation, non-Linux) setup is skipped silently and `RLIMIT_NPROC` becomes the only ceiling. `RLIMIT_NPROC` is retained as a uid-wide backstop regardless. See [`docs/dev/sandbox-limits.md`](../dev/sandbox-limits.md) for the full operator-facing reference.
- Kill on session end: process group guarantees cleanup

### 8.3 Level 2 — Container

Implemented in `crates/forge-session/src/sandbox/level2.rs` (F-596),
backed by the `forge_oci::ContainerRuntime` trait shipped in F-595
(today: `PodmanRuntime`).

> **Honest scope.** Level 2 today provides **rootless-podman process /
> filesystem isolation only**. Resource caps, capability hardening,
> read-only rootfs, and network policy are designed but **not yet
> wired**. Each gap below links to the security finding that tracks
> the fix. Treat the bare unqualified phrases "Level 2 hardening" or
> "Level 2 resource limits" as aspirational until those issues land.

#### Lifecycle (pre-warm + reuse)

A session that opts into Level 2 brings up exactly **one** container
for the duration of the session. The lifecycle, owned by
`Level2Session`:

1. **Detect** — `runtime.detect()` probes `podman --version` then
   `podman info` for rootless mode. Three outcomes are folded into
   `Level2Unavailable` and trigger auto-fallback (see below):
   `RuntimeMissing`, `RuntimeBroken`, `RootlessUnavailable`.
2. **Pull** — `runtime.pull(image)`. Idempotent; layers cached.
3. **Create** — `runtime.create(image, ["sleep", "infinity"])`. The
   `sleep infinity` init keeps the container alive between `exec`
   calls. Resource limits, capability hardening, and network policy
   are **NOT** attached here in the shipped code — see "Resource
   limits", "Capability hardening", and "Image trust" below.
   `Level2Session::create` emits a `tracing::warn!` whenever a
   non-default `ContainerLimits` is passed so operators are not
   surprised at runtime.
4. **Start** — `runtime.start(handle)`. Container is now ready for
   `exec`.
5. **Exec, repeated** — every step in the session runs through
   `runtime.exec(handle, argv)`. The container is reused; there is
   no per-step create cost.
6. **Stop + Remove** — on session teardown, `runtime.stop(handle)`
   then `runtime.remove(handle)`. The `-f` on `rm` reaps even if
   `stop` lost the race; we swallow `stop` errors so the more useful
   `rm -f` error is what surfaces.

The `SandboxedCommand::execute` entry point branches on
`SandboxLevel`: `Level1` runs the existing host-side seccomp +
`setrlimit` + cgroup pipeline; `Level2 { session: Arc<Level2Session> }`
delegates to `session.exec_step(argv)`. The unified return shape is
`StepOutcome { exit_code, stdout, stderr }` so callers (e.g. the
`shell.exec` tool) do not need to know which level ran.

> **Deviation from the F-596 DoD:** the spec wrote the variant as
> `Level2 { runtime: Box<dyn ContainerRuntime> }`. We use
> `Arc<dyn ContainerRuntime>` (wrapped in a `Level2Session` carrying
> the runtime, image, and handle): a session spawns many
> `SandboxedCommand` instances per turn that all need to share the
> same pre-warmed container, and `Box` cannot be cloned across those
> handles.

#### Resource limits

> **Currently NOT enforced.** `ContainerLimits` is captured into
> `Level2Session` and exposed via `session.limits()` for
> observability, but it is **never passed to `podman create`**.
> Containers run with whatever limits the host slice applies —
> for rootless podman, that is the user's slice, not the values
> on `ContainerLimits`. `Level2Session::create` emits a
> `tracing::warn!` whenever a non-default `ContainerLimits` is
> configured so operators see the gap at runtime. Tracked as
> [F-654 / #690](https://github.com/forge-ide/forge/issues/690).

Intended design (when F-654 lands): per-step caps land on the
container's cgroup v2 leaf at **create time**, not exec time —
`podman exec` does not accept resource flags. `ContainerLimits`
captures the three caps Phase 1 cares about:

| Field | podman flag (intended) | Maps to (intended) |
|---|---|---|
| `cpus: Option<f32>` | `--cpus <N>` | cgroup v2 `cpu.max` |
| `memory_bytes: Option<u64>` | `--memory <bytes>` | cgroup v2 `memory.max` |
| `pids_max: Option<u64>` | `--pids-limit <N>` | cgroup v2 `pids.max` |

These map directly onto the same intent as the Level-1
`SandboxConfig` — `cpu_seconds` ↔ `--cpus`, `address_space_bytes`
↔ `--memory`, `max_processes` ↔ `--pids-limit` — but with cgroup
enforcement (per-container) instead of `setrlimit` (per-process /
per-uid).

> **Wiring gap.** F-595's `ContainerRuntime::create` signature
> accepts only `(image, argv)`. To preserve the F-595 public API
> (per F-596's constraints), `Level2Session::create` currently
> stores the `ContainerLimits` on the session for observability
> rather than passing them through to `podman create`. The
> argv-shaping helper `level2::limits_to_create_flags` pins the
> canonical podman flag rendering (verified by unit test) so the
> eventual wiring — once the trait grows a `create_with_limits`
> method — is a one-line change. Tracked as
> [F-654 / #690](https://github.com/forge-ide/forge/issues/690).

#### Capability hardening (currently NOT applied)

The shipped `PodmanRuntime::create` argv contains only
`["create", &image_string]` plus the caller-supplied init argv
(`sleep infinity`). The following hardening flags are **not**
injected:

| Flag | What it would do | Status |
|---|---|---|
| `--security-opt no-new-privileges` | Block setuid escalation inside the container | not applied |
| `--cap-drop ALL` | Drop all Linux capabilities (currently inherits rootless-podman defaults) | not applied |
| `--read-only` | Mount the container rootfs read-only | not applied |
| `--network none` | Disable container networking by default | not applied |

Containers therefore run with rootless podman's default capability
set and **unrestricted network**. The "blast radius" trade-off
elsewhere in this doc must be read with that in mind: the
namespace + rootfs boundary is real, but a compromised tool inside
the container can still reach the network and exercise the
default cap set. Tracked as
[F-642 / #678](https://github.com/forge-ide/forge/issues/678).

#### Image trust (currently NOT enforced)

Images are pulled by floating tag (e.g. `alpine:3.19`,
`oci.io/forge/rust-tools:<ver>`) with **no digest pinning** and
**no signature verification**. A registry compromise or a
tag-mutation attack can swap the image bytes between sessions
without detection. Tracked as
[F-643 / #679](https://github.com/forge-ide/forge/issues/679).

#### Auto-fallback to Level 1

The F-596 contract: if the container runtime is unreachable,
fall back transparently to Level 1 with a logged warning rather
than failing the session. `level2::detect_or_fall_back` does this:

- `OciError::RuntimeMissing(tool)` → `Level2Unavailable::RuntimeMissing`
- `OciError::RuntimeBroken { tool, stderr }` → `Level2Unavailable::RuntimeBroken`
- `OciError::RootlessUnavailable { runtime, reason }` → `Level2Unavailable::RootlessUnavailable`
- Any other `OciError` (e.g. `CommandFailed`, `Io`) is also folded
  into a logged `RuntimeBroken` because the F-596 contract is
  "auto-fallback if container runtime unreachable" — an unexpected
  probe error is the same situation from the caller's perspective.

Every fallback emits `tracing::warn!` with the variant name and
reason as structured fields so operators can filter on them
without re-running the probe. Variant names are pinned strings
(`RuntimeMissing`, `RuntimeBroken`, `RootlessUnavailable`) so log
queries don't break on Rust enum renames.

> **Fallback runs at session start, not mid-session.**
> `detect_or_fall_back` is intended to be invoked once, before the
> session commits to a level. The branching inside
> `SandboxedCommand::execute` does *not* re-attempt fallback when a
> mid-session `runtime.exec` returns `OciError`: those errors
> propagate as `Err(io::Error)` to the caller. Mid-session demotion
> Level 2 → Level 1 would silently relax the user-visible
> isolation guarantee partway through a session, which is exactly
> the surprise the isolation model is supposed to prevent.
> Operators see one consistent level for the whole session.

#### Container teardown and panic safety

`Level2Session` ships two teardown paths:

- **Async, preferred:** `Level2Session::teardown()` runs `stop`
  then `remove(-f)` through the `ContainerRuntime` trait. Callers
  on the clean shutdown path should always reach for this.
- **Sync, panic-safety net (post-construction only):**
  `Level2Session`'s `Drop` impl fire-and-forgets
  `podman rm -f <id>` via `std::process::Command::spawn` whenever
  `teardown()` did not complete. This protects against panic,
  early `?`, and task cancellation **after `Level2Session` is
  constructed**. The Drop is detached (no `wait()`), so a slow or
  hung `podman` cannot block the panicking thread. A successful
  async `teardown()` arms a flag that disarms the Drop net so the
  cleanup does not run twice.

> **Known gap: partial-create failures bypass Drop.**
> `Level2Session::create` runs `pull → create → start → construct
> Self`. If `start()` (or anything before `Self` is built) fails,
> `Self` is never constructed and `Drop` therefore never fires —
> a container created by `runtime.create()` but not started, or
> one started but never wrapped in `Self`, leaks. The safety net
> only covers post-construction crashes. Tracked as
> [F-655 / #691](https://github.com/forge-ide/forge/issues/691).

The Drop path hard-codes `podman` because `PodmanRuntime` is the
only `ContainerRuntime` implementation today; introducing a second
runtime should add a tiny per-impl teardown-argv abstraction.

#### Level guard on `SandboxedCommand::spawn()`

`SandboxedCommand::spawn()` is **Level 1 only**. Calling it on a
command configured for Level 2 returns
`io::Error::other("SandboxedCommand::spawn() is Level 1 only; use
execute() for Level 2")` rather than silently bypassing the
container — without this guard, a caller who reached for
`spawn()` (perhaps because they want a `SandboxedChild` handle for
streaming) would unintentionally run the work on the host with
no isolation. Use `execute()` for any path that may run at either
level.

#### Image strategy (future work)

- Base images maintained by us: `oci.io/forge/rust-tools:<ver>`,
  `oci.io/forge/node-tools:<ver>`, `oci.io/forge/py-tools:<ver>`.
- User may specify their own in `.agents/<name>.md`:
  ```yaml
  isolation:
    kind: container
    image: docker.io/library/python:3.12
  ```

#### Mounts (future work)

- Workspace mounted at `/workspace` read-write by default, read-only if declared.
- `~/.config/forge/certs/` mounted at `/etc/forge/certs/` for provider access.
- No home dir, no `/tmp` cross-mount.

#### Network (future work)

- Default: no network.
- Declared hosts (for MCP or tools): CNI policy allows only those.

#### Trade-offs vs Level 1

| Concern | Level 1 | Level 2 (shipped) |
|---|---|---|
| Blast radius of a compromised tool | Process tree of one sandbox | Container rootfs + user namespace; no `--cap-drop`, no `--read-only`, default network ([F-642](https://github.com/forge-ide/forge/issues/678)) |
| Cold-start cost | Microseconds (fork+exec) | Image pull (one-off) + container create+start (~hundreds of ms, once per session) |
| Per-step cost | fork+exec | `podman exec` (~tens of ms) |
| Network containment | None (open network) | None today (open network); CNI policy is future work ([F-642](https://github.com/forge-ide/forge/issues/678)) |
| Filesystem containment | `forge-fs` path checks | Container rootfs by construction (rootfs is read-write — `--read-only` not yet applied) |
| Resource limits | `setrlimit` (per-process / per-uid) + cgroup v2 `pids.max` (per-sandbox) | None enforced — `ContainerLimits` captured but not passed to `podman create` ([F-654](https://github.com/forge-ide/forge/issues/690)) |
| Image trust | n/a | Floating tags; no digest pinning, no signature verification ([F-643](https://github.com/forge-ide/forge/issues/679)) |
| Operator burden | Linux + cgroup v2 | Linux + cgroup v2 + rootless `podman` |

### 8.4 Approval — orthogonal to isolation

Sandbox enforces runtime containment. Approval enforces human-in-the-loop. They operate independently. Writes, exec, and network-side-effect tools require approval regardless of isolation level, per the matrix in §6.3.

Approval granularity comes in four scopes (once/file/pattern/tool) — see SPECS.md §10. Whitelists are session-local; no persistent whitelists.
