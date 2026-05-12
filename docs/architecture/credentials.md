# Credential Storage

> Status: shipped in F-587 (Phase 3 — Breadth). Trait + storage backends + Tauri commands + orchestrator pull.

Forge resolves a per-provider API key on every turn. The lookup is funneled through one trait — [`Credentials`](../../crates/forge-core/src/credentials/mod.rs) — so the rest of the codebase sees a single shape regardless of where the key actually lives (OS keyring, environment variable, in-process mock).

## Trait surface

```rust
#[async_trait]
pub trait Credentials: Send + Sync {
    async fn get(&self, provider_id: &str) -> Result<Option<SecretString>, ForgeError>;
    async fn set(&self, provider_id: &str, value: SecretString) -> Result<(), ForgeError>;
    async fn remove(&self, provider_id: &str) -> Result<(), ForgeError>;
    async fn has(&self, provider_id: &str) -> Result<bool, ForgeError>; // default impl
}
```

Contract:

- `provider_id` is a stable lowercase ASCII slug — `"anthropic"`, `"openai"`. Implementations may namespace internally (e.g. the keyring uses a `service = "forge"` field) but the caller's id is the source of truth.
- A missing entry is `Ok(None)`, **not** `Err(_)`. `Err(_)` is reserved for backend failures (keyring locked, DBus offline, malformed entry). Callers — notably `LayeredStore` — rely on this to fall through cleanly.
- The credential value is wrapped in [`secrecy::SecretString`](https://docs.rs/secrecy) at every API boundary. The backing buffer is zeroed on drop, and `Debug`-printing the wrapper redacts the value rather than emitting it.

## Implementations

| Type                                    | Purpose                                                                           |
| --------------------------------------- | --------------------------------------------------------------------------------- |
| `MemoryStore`                           | In-process map. Tests, headless contexts.                                         |
| `EnvFallbackStore`                      | Read-only view over `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` (default mapping).     |
| `LayeredStore<P, F>`                    | Two-tier composition. Primary is consulted first; fallback handles miss.          |
| `KeyringStore`                          | Platform-native. Linux Secret Service, macOS Keychain, Windows DPAPI / Cred Mgr.  |

### `KeyringStore` (per-platform)

| Target                | Backend                 | Crate                                      |
| --------------------- | ----------------------- | ------------------------------------------ |
| `cfg(target_os = "linux")`   | DBus Secret Service     | [`secret-service`](https://crates.io/crates/secret-service) (rt-tokio + crypto-rust) |
| `cfg(target_os = "macos")`   | System Keychain         | [`security-framework`](https://crates.io/crates/security-framework) |
| `cfg(target_os = "windows")` | DPAPI / Credential Mgr  | [`keyring`](https://crates.io/crates/keyring) (project-chosen fallback) |

Construction is pure — no DBus, Keychain, or DPAPI handshake until the first `get` / `set` / `remove`.

All entries are stored under a single service namespace (default `"forge"`) with `provider_id` as the account/username. This keeps Forge entries grouped in OS UI (Keychain Access shows them in one row group; GNOME's `seahorse` shows them under one collection alias) and makes cleanup a single API call.

## Production wiring

```rust
let layered = LayeredStore::new(
    KeyringStore::new(),
    EnvFallbackStore::default(),
);
```

This is what `CredentialsState::production()` builds in `forge-shell`. It is the wiring every dashboard and every session daemon should use.

The lookup order is:

1. OS keyring under service `"forge"`, account `provider_id`. If present, return.
2. Environment variable per the default mapping (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`). If set and non-empty, return.
3. Otherwise return `Ok(None)`.

`set` and `remove` always target the keyring. The environment is read-only by design — Forge never mutates the parent shell's environment.

### Headless / CI deployments

In an environment with no Secret Service daemon (Docker, headless SSH, CI runners), the keyring layer's first `get` call may return an `Err(_)` rather than `Ok(None)`. **`LayeredStore` only falls through to the env-var layer on `Ok(None)`; an `Err(_)` propagates and fails the turn.** This is intentional — see [§ Orchestrator integration](#orchestrator-integration) for the rationale on why a locked / offline keyring fails fast instead of silently downgrading.

The intended pattern for these environments: don't reach for `KeyringStore` at all. Wire the env-var store directly:

```rust
// Headless wiring — env-only, no keyring layer.
CredentialsState::new(Arc::new(EnvFallbackStore::default()))
```

`CredentialsState::production()` always builds the layered store; a deployment-specific entry point should substitute the env-only path. We deliberately do not auto-fallback on connect failure: an unexpected dbus disappearance on a workstation that previously had it is a real environmental drift the operator needs to notice, not a silent state we paper over.

## Environment-variable fallback

| `provider_id` | Variable             |
| ------------- | -------------------- |
| `anthropic`   | `ANTHROPIC_API_KEY`  |
| `openai`      | `OPENAI_API_KEY`     |

Read at every `get` call (not cached at startup; the env may change between turns in a long-lived daemon, e.g. when an operator rotates a key via `systemctl edit`). An empty string is treated as "not set".

The mapping is configurable — `EnvFallbackStore::with_mapping` lets a downstream crate (third-party providers, enterprise builds) extend the table without patching `forge-core`.

## Tauri commands

Three commands, all dashboard-scoped (`require_window_label(&webview, "dashboard", ...)` rejects any window label other than `"dashboard"`):

| Command            | Args                              | Returns        |
| ------------------ | --------------------------------- | -------------- |
| `login_provider`   | `provider_id: String, key: String` | `()`           |
| `logout_provider`  | `provider_id: String`              | `()`           |
| `has_credential`   | `provider_id: String`              | `bool`         |

`has_credential` returns presence only — never the value. There is no command to read the credential back from the webview; the only consumer of the actual secret is the daemon's per-turn pull.

Per-field caps:

- `provider_id`: 64 bytes. ASCII slugs are well under.
- `key`: 8 KiB. Anthropic keys are ~108 bytes, OpenAI ~51 bytes.

The inbound key string is wrapped in `SecretString` immediately on receipt; from that moment onward the redaction window applies to every `Debug` / `format!` call downstream.

## Orchestrator integration

`run_turn` accepts an `Option<CredentialContext>`:

```rust
pub struct CredentialContext {
    pub store: Arc<dyn Credentials>,
    pub provider_id: String,
}
```

When `Some`, the orchestrator calls `store.get(provider_id)` exactly once at turn start, **before** the request loop opens its model step. The pulled value is held only for the duration of the request loop construction (dropped immediately for keyless providers — e.g. a `custom_openai` preset with `auth.shape = "none"` for a local Ollama or LM Studio endpoint; handed into the per-request auth shape via `secrecy::ExposeSecret::expose_secret` at the network boundary for Anthropic / OpenAI / authed `custom_openai`).

Failure modes:

- **Backend error** (`Err(_)` from the store) — propagates as a turn error. A misconfigured Secret Service daemon or a locked Keychain is more useful as a surfaced failure than a silent fall-through to "no auth" that the provider would later 401 on.
- **Missing entry** (`Ok(None)`) — turn proceeds. The keyless path stays available; the credential pull is just observed to have missed.

## Logging

Emissions in this layer use `tracing::trace!` / `tracing::warn!` only. The credential value is **never** in a tracing field — not even at `trace`. Provider id and outcome (`hit`, `miss`, `error_kind`) are the observable surface. Operators who need to confirm a key landed should use `has_credential` rather than fishing through logs.

## Test coverage

| Surface                                              | Test                                                                          |
| ---------------------------------------------------- | ------------------------------------------------------------------------------- |
| Trait shape (round trip, idempotent remove, miss)    | `crates/forge-core/src/credentials/mod.rs` (`memory_store_*`, `arc_dyn_*`)     |
| `EnvFallbackStore` (read, miss, read-only set/remove)| `crates/forge-core/src/credentials/env.rs` (`reads_*`, `set_is_rejected`, …)   |
| `LayeredStore` ordering                              | `crates/forge-core/src/credentials/layered.rs`                                  |
| `KeyringStore` shape (cfg-gated unit tests)          | `crates/forge-core/src/credentials/keyring.rs`                                  |
| Linux integration via mock backend                   | `crates/forge-core/tests/credentials_keyring_mock.rs`                           |
| Tauri command validators + state                     | `crates/forge-shell/src/credentials_ipc.rs` (`tests::*`)                        |
| Orchestrator per-turn pull (hit / miss / err / off)  | `crates/forge-session/tests/credentials_pull.rs`                                |
| `SecretString::Debug` redaction                      | `crates/forge-core/src/credentials/mod.rs` (`secret_string_debug_does_not_leak`)|

The Linux integration test uses an in-memory mock that mimics the Secret Service's observable behavior (overwrite-on-`set`, idempotent `remove`, byte-exact `get`). Driving the live `secret-service` crate would require a session-scoped DBus daemon that CI runners typically don't expose; the mock pins the trait contract that the orchestrator-side path relies on.
