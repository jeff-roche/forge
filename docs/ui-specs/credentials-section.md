# Credentials IPC Reference

> **Note**: As of V1, credential entry happens in the full-page `/providers` route (see [`providers-page.md`](./providers-page.md)).
> This file is retained as the authoritative IPC reference for the credential commands
> (`login_provider`, `logout_provider`, `has_credential`). The dashboard credentials card is removed.

---

## Purpose

Define the wire contract every UI surface uses to read and mutate per-provider credential state. The IPC layer is the single source of truth — every consumer (the `/providers` page, the first-run banner copy, the new-session picker's `has_credential` hint) calls these commands rather than reaching into the keyring directly.

## Where

Lives wherever provider credentials are entered (currently [`providers-page.md`](./providers-page.md)) and wherever a "credential present?" hint is rendered (the dashboard Providers card status pill, the new-session picker). Backed by `crates/forge-shell/src/credentials_ipc.rs`.

## Commands

Every command in this section follows the F-673 standard: the outer error string returned to the webview begins with `<command_name>: `, where `<command_name>` matches the wire name of the Tauri command. See `crates/forge-shell/src/ipc.rs` "Error handling (F-673)" header for the canonical rationale.

All three commands are authz-gated to the `dashboard` window label via `crate::ipc::require_window_label`. A session window invoking any of them is rejected before validation runs.

### `login_provider` (F-587)

```
input:  { provider_id: string, key: string }
output: ()
error:  "login_provider: <reason>"
```

Writes `key` to the active credential store under `provider_id`. Validation rejects empty `provider_id`, empty `key`, `provider_id` longer than `MAX_PROVIDER_ID_BYTES`, and `key` longer than `MAX_API_KEY_BYTES` (8 KiB). The inbound `key` is wrapped in `secrecy::SecretString` immediately so downstream `Debug` / `format!` calls redact it. Storage semantics are overwrite-on-write — the previous entry is replaced unconditionally; callers that need rotation confirmation gate it at the UI layer (see [`providers-page.md` §Credential entry](./providers-page.md)).

### `logout_provider` (F-587)

```
input:  { provider_id: string }
output: ()
error:  "logout_provider: <reason>"
```

Removes the credential entry for `provider_id` from the active store. Idempotent — removing an absent entry resolves `Ok(())`. Validation rejects empty / oversized `provider_id`. The keyring write is irreversible from the IPC surface (no undo); reversibility is only via a subsequent `login_provider` with a freshly-typed key.

### `has_credential` (F-587)

```
input:  { provider_id: string }
output: bool
error:  "has_credential: <reason>"
```

Presence probe. Returns `true` iff the active store reports a credential entry for `provider_id`. Never returns the value — the IPC contract is strictly write-only once stored. Validation matches `logout_provider`'s. Probe failure is propagated to the caller verbatim (the renderer typically degrades to "no stored credential" so the user can still type a key and recover).

## Error format

The F-673 contract for each command:

- `login_provider: <reason>` — validation failures, keyring write failures.
- `logout_provider: <reason>` — validation failures, keyring delete failures.
- `has_credential: <reason>` — validation failures, keyring read failures.

`<reason>` is the verbatim store-layer error string (no stripping, no rewrapping). UI surfaces echo the message into a `role="alert"` line per `docs/design/component-principles.md` four-state rule.

## Keyring integration

Production wiring is `LayeredStore<KeyringStore, EnvFallbackStore>` — the OS keyring is primary, the environment is read-only fallback for `get`/`has`. Tests substitute `MemoryStore`. The store boundary is `forge_core::Credentials`; the IPC layer holds an `Arc<dyn Credentials>` in `CredentialsState` and never inspects the concrete backend. See `docs/architecture/credentials.md` for the backend contract (rotation-overwrite invariant, env-fallback read semantics, platform cfg-gating of `KeyringStore`).

## Security contract

The IPC layer is the canonical enforcement point for Forge's credential rules. Reviewers should treat any drift as a security issue:

- The inbound `key` is wrapped in `SecretString` before any downstream call. No long-lived `String` copy is taken.
- Tracing fields never carry the value — only `provider_id` and outcome (`hit`, `miss`, `error_kind`). This holds at every level, including `trace!`.
- No command returns the stored value. `has_credential` is the only read surface and yields a `bool`.
- The dashboard-only authz gate ensures session windows cannot reach the credential surface.

## Destructive-action contract

`logout_provider` is destructive — the keyring entry is gone after a successful call and the user must re-enter the key to restore it. Callers gate the invocation behind a user-facing confirm appropriate to their surface (the `/providers` page chains it inside the `REMOVE PROVIDER?` modal; see [`providers-page.md` §Edit/Remove](./providers-page.md)). The IPC itself does not prompt — confirmation is a UI concern.

`login_provider` is single-step at the wire level but overwrites unconditionally; rotation confirmation (when an entry is already present) is gated at the UI layer per [`providers-page.md` §Credential entry](./providers-page.md).

## Cross-spec references

- [`providers-page.md`](./providers-page.md) — primary consumer; the `/providers` page is the only UI surface that opens the credential input field. Chains `login_provider` / `logout_provider` from its Add / Edit / Remove flows.
- [`providers-section.md`](./providers-section.md) — the dashboard Providers card; reads `has_credential` (via the higher-level `provider_status` aggregate) for the readiness pill.
- [`dashboard.md`](./dashboard.md) — the Providers card's status sentence references credential readiness in plain prose.
- `docs/architecture/credentials.md` — backend keyring contract (rotation-overwrite invariant, env-fallback read semantics).
- `docs/design/component-principles.md` — four-state rule and destructive-action contract honored by UI callers.

## Doesn't do

- Does not surface keyless providers (e.g. Ollama). Callers should suppress the credential field for any provider whose `credential_required` flag is unset.
- Does not let the user *view* a stored key. The IPC contract is one-way once stored — only `has_credential` is queryable.
- Does not export / back up keys. The user's keyring is the system of record.
- Does not own provider activation or configuration — that's [`providers-page.md`](./providers-page.md) and [`providers-section.md`](./providers-section.md).
