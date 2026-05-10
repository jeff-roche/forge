# forge-oci

Container lifecycle for agent isolation. Defines the [`ContainerRuntime`] trait and ships [`PodmanRuntime`] — a rootless, daemonless `podman` shell-out — as the first concrete implementation.

## Role in the workspace

- Depended on by: future agent isolation paths in `forge-agents` / `forge-session` will consume `ContainerRuntime` to launch sandboxed agents.
- Depends on: `tokio` (process spawn), `serde_json` (parsing `podman info` / `podman stats`), `thiserror`, `async-trait`, `tracing`. Intentionally lean.

## Key types / entry points

- `ContainerRuntime` — async trait: `pull / create / start / exec / stop / remove / stats`.
- `ImageRef` — typed image reference (`{ registry, name, tag }`) with `parse(&str)` round-trip.
- `ContainerHandle` — opaque container ID returned by `create`.
- `ExecResult { exit_code, stdout, stderr }` — captured exec output.
- `Stats { cpu_percent, memory_bytes, pids }` — single-shot resource snapshot.
- `OciError` — typed errors: `RuntimeMissing` (binary absent), `RuntimeBroken` (binary present but `podman info` failed — cgroup delegation, newuidmap, SELinux), `RootlessUnavailable` (binary works but rootless explicitly disabled), `CommandFailed`, `InvalidImageRef`, `Io`, `InvalidJson`.
- `PodmanRuntime::detect()` — first-run probe: confirms `podman --version` and parses `podman info` JSON for `host.security.rootless = true`.

## Testing

- Unit tests cover argv-shaping for every method via the `RecordingRunner` mock — no `podman` binary required.
- Integration test `tests/podman_integration.rs` is `cfg(target_os = "linux")` and `#[ignore]`-gated, so CI's default `cargo test` skips it cleanly. It also requires the `test-support` feature (see below) because it consumes the mock-runner toolkit; cargo skips compiling the target on plain `cargo test -p forge-oci`. Run locally with rootless podman configured:

  ```sh
  cargo test -p forge-oci --features test-support -- --ignored
  ```

  It pulls `docker.io/library/alpine:3.19`, runs `echo hello`, asserts stdout, and verifies cleanup via `podman inspect`. The test fails loudly when podman is missing or misconfigured rather than auto-skipping (which would mask CI regressions).

### `test-support` feature

`forge-oci` re-exports its mock-runner toolkit (`RecordingRunner`, `StubResponse`, `RecordedCalls`, `RecordedCall`) only when the `test-support` Cargo feature is enabled. The default published API surface ships just the production runtime types (`PodmanRuntime`, `CommandRunner`, `TokioCommandRunner`, `CommandOutcome`, the `signature` module).

Downstream crates that drive `PodmanRuntime::with_runner` from their own tests opt in via dev-dependencies:

```toml
[dev-dependencies]
forge-oci = { path = "../forge-oci", features = ["test-support"] }
```

Cargo unifies dev-dep features additively into test/bench builds only, so the production library build still sees the lean default API.

## Further reading

- [Crate architecture — `forge-oci`](../../docs/architecture/crate-architecture.md#36-forge-oci)
- [Isolation model](../../docs/architecture/isolation-model.md)
