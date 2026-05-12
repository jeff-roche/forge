# Providers Page

> Full-page route (F-711) — the `/providers` workspace view targeted by the dashboard Providers card's `Manage` link. Owns the configuration surface for built-in and user-defined providers: add, edit, remove, enable/disable, and a one-shot reachability probe.

---

## Purpose

Provide a single, navigable surface for configuring every provider Forge will offer to sessions. The dashboard's Providers card ([`providers-section.md`](./providers-section.md)) is a *picker* against the active provider; this page is the *editor* behind it. Every CRUD operation that touches `[providers.*]` in user settings lands here so the dashboard card and the new-session picker can stay read-only.

## Route

- **Path:** `/providers` — mounted in the `Router` defined in `web/packages/app/src/App.tsx`.
- **Shell:** renders inside the app shell (see [`app-shell.md`](./app-shell.md)); the activity-bar `Providers` icon stays active while this route is mounted.
- **Triggered from:** the dashboard Providers card `Manage` action (F-721). Plain-text reference until `dashboard.md` is rewritten in parallel.
- **Component path:** `web/packages/app/src/pages/ProvidersPage.tsx`.

## List view

```
┌─ PROVIDERS                                                [+ Add provider] ─┐
│ ● anthropic                built-in   ⦿ key       ● ready 142ms  [Test][Edit][Remove] │
│ ● openai                   built-in   ⚠ key       ◌ unknown      [Test][Edit][Remove] │
│ ● custom_openai:ollama     custom                ● ready 12ms   [Test][Edit][Remove]  │
│ ● custom_openai:vllm       custom     ⦿ key       ● ready 38ms   [Test][Edit][Remove] │
└──────────────────────────────────────────────────────────────────────────────┘
```

### Header

A page-scoped header above the row list:
- **Left:** `Providers` title in `var(--font-display)` weight 800, uppercase, `letter-spacing: 0.02em`.
- **Right:** `+ Add provider` primary CTA (ember) — opens the §Add provider form modal.

### Row layout

One full-width row per provider. Row track: `auto 1fr auto auto auto auto` with `gap: var(--sp-3)`, padding `var(--sp-2) var(--sp-4)`, and a `1px solid var(--color-border-1)` bottom border (suppressed on `:last-child`). Cells, left to right:

1. **Enabled toggle.** A §Toggle switch primitive (per [`DESIGN.md` §Toggle switch](../../DESIGN.md)). Off-disabled providers are dimmed but still render every other cell. The toggle's hit target is the toggle cell only — clicking the row body does nothing.
2. **Identity stack.** Two stacked lines: provider id (`var(--font-mono)`, `--type-mono-xs`, `var(--color-text-primary)`) on top; kind label (`built-in` or `custom_openai`) plus model id when present on the second line in `var(--font-mono)` at `--type-mono-xxs` in `var(--color-text-tertiary)`.
3. **Credential indicator.** `⦿ key` (`--color-ok`) when stored, `⚠ key` (`--color-warn`) when required-but-missing, omitted entirely when the provider is keyless. Mirrors the dashboard card hint vocabulary so the surfaces read as one system.
4. **Connection-status pill.** A §Status pill (per [`DESIGN.md` §Status pill](../../DESIGN.md)) reflecting the most recent `test_provider_connection` outcome — `ready` (success, with `<latency_ms>ms`), `unreachable`, `auth-required`, or `unknown` (idle). The pill collapses to `unknown` on first paint; it does not auto-probe.
5. **Actions cluster.** Three `btn-ghost` chips with compact mono sizing: `Test`, `Edit`, `Remove`. `Remove` carries the destructive-action treatment from `docs/design/component-principles.md`.

## Add provider

A focus-trapped `role="dialog" aria-modal="true"` modal opened from the header CTA. Window-level `Escape` handler closes it; the focus trap can't swallow the keystroke.

### Fields

- **Kind** (`<select>`): built-in templates (`anthropic`, `openai`) or `custom_openai`. Kind selection drives which subsequent fields are visible. Selecting `custom_openai` reveals a **Preset** picker (e.g. *Ollama*, *LM Studio*, *vLLM*, *Blank*); picking *Ollama* auto-fills `endpoint = http://127.0.0.1:11434/v1`, `model = llama3.2`, and `auth = none` so the user only needs to confirm the name.
- **Name** (`<input>`): unique slug per provider. For built-ins, the kind itself is the canonical slug and the field is read-only with a `Hint: rename to add a second instance` affordance. For `custom_openai`, the field is required and must match `[A-Za-z0-9_-]+` (mirrors `validate_provider_id`'s suffix charset).
- **Built-in kind fields:**
  - `model` — default model id; pre-filled from the kind's built-in default; editable.
  - `endpoint` — optional override; placeholder shows the built-in's canonical URL.
- **Custom OpenAI fields:**
  - `endpoint` — required; must parse as an `http`/`https` URL.
  - `model` — required; non-empty.
- **Credential** — see §Credential entry below.

### Validation

- `name` is unique against the merged settings' provider keys (built-in id list + `[providers.custom_openai.*]`).
- `endpoint` parses via `URL` constructor; non-`http`/`https` schemes are rejected before submit.
- `credential` is non-empty when the kind's `credential_required` flag is set.

### Submit

`Add` → calls `add_provider` IPC. On success, the modal closes, the row list refetches, and the new row scrolls into view with a one-tick `provider-row--just-added` highlight. On failure, the modal stays open and the error renders inline above the action row (verbatim).

## Credential entry

Write-only by contract — the value is never echoed back to the DOM, never serialized into a row, never carried in a tracing field. The form holds the typed value in a local `draft()` signal cleared on submit-resolve or modal-close.

### Field

- `<input type="password" autocomplete="off" spellcheck="false">` labelled `API key`.
- For keyless kinds the field is hidden entirely.
- For required-key kinds with no key stored, the label reads `Add key`; when re-opening an existing provider with a key already on the keyring, the label flips to `Replace key` and the submit chip reads `ROTATE` instead of `STORE`.

### Storage

The underlying IPC contract — `login_provider`, `logout_provider`, `has_credential` — is defined in [`credentials-section.md`](./credentials-section.md). This page is a *caller* of those commands, not their definition surface. The Add / Edit submit flow chains `add_provider` (or `update_provider`) → `login_provider` so the row's row-side `has_credential` probe reflects the new state on the next refetch.

This page is the only UI surface that opens the credential field; the dashboard's credentials card has been removed.

## Test connection

### Trigger

The per-row `Test` chip. The button is `aria-busy=true` while a probe is in flight; clicking it again is a no-op.

### Result

`test_provider_connection` returns `{ ok, latency_ms?, model_count? }`. On `ok = true`, the connection-status pill renders `ready <latency_ms>ms` in `var(--color-success)`; if `model_count` is present, the row's model-id second line appends `(<n> models)` in `var(--color-text-tertiary)`. On failure, the pill flips to `unreachable` (or `auth-required` if the verbatim error begins with `test_provider_connection: auth `) and a `role="alert"` line surfaces under the row carrying the verbatim error.

Failures do not block subsequent edits — a provider can be edited or removed in any state.

## Edit/Remove

### Edit

`Edit` opens the same modal pre-populated from the current row. The `kind` field is read-only on edit — switching a provider's kind would invalidate every dependent setting. Submit calls `update_provider`; success closes the modal and triggers a refetch.

If the credential field is left blank on edit, the stored key is untouched. Typing a new value flows through this page's rotation-confirm modal so the user explicitly acknowledges the keyring overwrite before `login_provider` fires (see [`credentials-section.md` §Destructive-action contract](./credentials-section.md) for the wire-level rationale).

### Remove

`Remove` opens a destructive confirm per `docs/design/component-principles.md` destructive-action contract:

```
┌────────────────────────────────────────────┐
│ REMOVE PROVIDER?                           │
├────────────────────────────────────────────┤
│ Removing <provider-id> deletes its config  │
│ from this workspace and clears its stored  │
│ credential. Active sessions stay on the    │
│ provider they started with.                │
├────────────────────────────────────────────┤
│                          [CANCEL] [REMOVE] │
└────────────────────────────────────────────┘
```

On confirm:
1. `remove_provider` IPC fires.
2. If the removed provider was the active selection, the page clears `[providers.active]` via `set_active_provider` with an empty id (allowed under F-586 semantics — an empty active id leaves the next session to fall back to the catalog default).
3. The keyring entry is removed in the same submit via `logout_provider` (no separate user confirmation — the destructive modal already covers it).
4. The row list refetches; new-session pickers ([`new-session-flow.md`](./new-session-flow.md) — authored in parallel) drop the entry on their next read.

## Enabled toggle

Per-row §Toggle switch. Calls `set_provider_enabled` and refetches; the switch is `aria-busy=true` for the duration of the call. Disabled providers stay in the list (so the user can re-enable or remove them) but do not appear in the new-session provider picker.

The disabled state is purely advisory — it does not remove the credential, does not interfere with running sessions, and does not affect `dashboard_list_providers` (the dashboard card always shows every configured provider so the user can see what's available before they spawn).

## States

The page renders all four `docs/design/component-principles.md` states distinctly, plus per-form / per-test sub-states.

### Page level

- **Loading.** Skeleton placeholder — three `block`-variant skeletons sized to the row height (toggle + identity + credential + pill + actions). The header paints immediately; the `+ Add provider` CTA renders disabled while loading.
- **Empty.** No providers configured (post-removal of every entry): a single full-row placeholder `var(--font-mono)` at `--type-mono-xs` in `var(--color-text-tertiary)` reading `No providers configured. Add one to start a session.` plus an inline `+ Add provider` repeat of the header CTA centred in the row.
- **Error.** A `role="alert"` line above the list reading `Couldn't load providers — <verbatim error>` with a `RETRY` link. The header CTA stays enabled — adding a provider does not require a successful list read.
- **Ready.** The row list above.

### Per-form (Add / Edit modal)

- **Idle.** Modal open, fields editable, submit enabled when validation passes.
- **Validating.** A field-level red helper line under the offending field; submit disabled until cleared.
- **Saving.** Submit chip `aria-busy=true`; modal action row reads `Saving…`; all fields disabled.
- **Save-failed.** Inline `role="alert"` line above the action row carrying the verbatim IPC rejection (e.g. `add_provider: name already exists`). Fields re-enable so the user can correct and retry.

### Per-test (per-row probe)

- **Idle.** Pill reads `unknown`; `Test` chip enabled.
- **Probing.** Pill reads `probing` (`var(--color-text-secondary)`, animated dot via the `pulse` keyframes); `Test` chip `aria-busy=true`.
- **Probe-ok.** Pill flips to `ready <latency_ms>ms` (`var(--color-success)`); model-id line annotates with model count when returned.
- **Probe-failed.** Pill flips to `unreachable` or `auth-required` (`var(--color-error)`); inline `role="alert"` under the row with the verbatim error.

## IPC contracts

Every command in this section follows the F-673 standard: the outer error string returned to the webview begins with `<command_name>: `, where `<command_name>` matches the wire name of the Tauri command. See `crates/forge-shell/src/ipc.rs` "Error handling (F-673)" header for the canonical rationale.

### `add_provider` (F-730)

```
input:  { kind: string, name: string, model?: string, endpoint?: string }
output: { provider_id: string }
error:  "add_provider: <reason>"
```

Validates `name` against the existing provider id-set, validates `endpoint` as `http`/`https`, persists the new `[providers.custom_openai.<name>]` or built-in instance entry through `apply_setting_update`, and returns the canonical `provider_id` (e.g. `custom_openai:vllm`). Does not store the credential — chain `login_provider` from the caller for that.

### `update_provider` (F-731)

```
input:  { provider_id: string, name?: string, model?: string, endpoint?: string, kind?: never }
output: { provider_id: string }
error:  "update_provider: <reason>"
```

`kind` is intentionally absent — kind transitions are out of scope. Renames (`name` change) re-key the settings entry and migrate the keyring id by chaining a `logout_provider` of the old id and a `login_provider` of the new id when the caller passes a fresh credential. The IPC itself does not touch the keyring.

### `remove_provider` (F-732)

```
input:  { provider_id: string }
output: {}
error:  "remove_provider: <reason>"
```

Deletes the settings entry. Clears `[providers.active]` if the removed id was active. Does not remove the keyring entry — the caller chains `logout_provider` for that so the keyring write is observable as its own IPC trace.

### `set_provider_enabled` (F-733)

```
input:  { provider_id: string, enabled: bool }
output: {}
error:  "set_provider_enabled: <reason>"
```

Toggles the `enabled` field on the settings entry. Emits `PROVIDER_CHANGED_EVENT` so the new-session picker refreshes on the next paint.

### `test_provider_connection` (F-733)

```
input:  { provider_id: string }
output: { ok: bool, latency_ms?: number, model_count?: number }
error:  "test_provider_connection: <reason>"
```

Issues a single low-cost probe against the provider's models endpoint (built-ins use the canonical URL; `custom_openai` uses the configured `endpoint`). Carries a 5s wall-clock deadline. `auth-required` is signalled by the verbatim error beginning `test_provider_connection: auth ` so the renderer can route the pill to its `auth-required` variant without parsing the rest of the string. Does not mutate state.

### Credential-related IPCs

`login_provider`, `logout_provider`, and `has_credential` are *not* defined by this page — they belong to the credential surface. See [`credentials-section.md`](./credentials-section.md) for their canonical wire format, error prefixes, and authz policy.

## Copy

- Page title: `Providers`
- Header CTA: `+ Add provider`
- Row-action chips: `Test`, `Edit`, `Remove`
- Connection-status pills (lowercase, mono): `ready <ms>`, `unreachable`, `auth-required`, `probing`, `unknown`
- Credential indicators: `⦿ key` (stored) / `⚠ key` (missing)
- Empty placeholder: `No providers configured. Add one to start a session.`
- List-error line: `Couldn't load providers — <verbatim error>` with `RETRY` link
- Remove-confirm title: `REMOVE PROVIDER?`
- Remove-confirm body: `Removing <strong>{providerId}</strong> deletes its config from this workspace and clears its stored credential. Active sessions stay on the provider they started with.`
- Remove-confirm buttons: `CANCEL`, `REMOVE`
- Add-modal title: `ADD PROVIDER`
- Edit-modal title: `EDIT PROVIDER`
- Modal action buttons: `CANCEL`, `STORE` (no key yet) / `ROTATE` (key present, value typed) / `SAVE` (no credential change)

## Color & typography

- Provider id: `var(--font-mono)`, `--type-mono-xs`, `var(--color-text-primary)`.
- Kind + model second line: `var(--font-mono)`, `--type-mono-xxs`, `var(--color-text-tertiary)`.
- Credential indicators: `⦿ key` → `var(--color-ok)`; `⚠ key` → `var(--color-warn)`.
- Connection pill: per [`DESIGN.md` §Status pill](../../DESIGN.md) — `ready` uses `var(--color-success)` with the live-dot glow; `auth-required` uses `var(--color-error)` with the error live-dot glow; `unreachable` uses `var(--color-error)` static; `unknown` uses `var(--color-text-secondary)` no dot; `probing` uses `var(--color-text-secondary)` with animated `pulse` dot.
- Row dividers: `1px solid var(--color-border-1)`; no border on the last row.
- Toggle: per [`DESIGN.md` §Toggle switch](../../DESIGN.md) — track `var(--color-border-1)` (off) / `var(--color-ember-900)` (on); thumb `var(--color-text-tertiary)` (off) / `var(--color-ember-400)` (on).
- Modal chrome: `var(--color-surface-2)` background, `1px solid var(--color-border-1)`, `var(--r-lg)` radius, `var(--sp-6)` padding.

## Keyboard

- Tab — page enters at the `+ Add provider` CTA, then each row's enabled-toggle → `Test` → `Edit` → `Remove` in document order.
- Enter inside a row's `Edit` or `Remove` chip — invokes the action (modal opens for `Edit`; destructive confirm opens for `Remove`).
- Space on a focused toggle — flips the enabled state.
- Escape inside any modal — cancels (window-level handler so the focus trap can't swallow it).
- Arrow keys do not move focus between rows — rows are not a radiogroup here (the page is the editor, not the picker; the dashboard's providers card owns radio semantics).

## Destructive-action contract

`Remove` is irreversible — the settings entry and the keyring entry both go in one submit. The confirm modal gates it per `docs/design/component-principles.md`. `set_provider_enabled` is single-step (toggling is reversible by toggling back). `update_provider` is single-step unless the credential is being rotated, in which case this page's rotation-confirm modal gates the `login_provider` call (see [`credentials-section.md` §Destructive-action contract](./credentials-section.md)).

## Cross-spec references

- [`app-shell.md`](./app-shell.md) — the page mounts inside the shell; activity-bar `Providers` icon stays active.
- `dashboard.md` — the dashboard Providers card's `Manage` link targets `/providers`. Plain-text reference until F-721 rewrites the dashboard spec to add the link.
- [`providers-section.md`](./providers-section.md) — the read-only dashboard card whose underlying `dashboard_list_providers` IPC reflects every change made on this page.
- [`credentials-section.md`](./credentials-section.md) — canonical IPC reference for `login_provider`, `logout_provider`, `has_credential`.
- `new-session-flow.md` — the new-session picker reads the provider list this page edits. Plain-text reference until the file is authored in parallel.
- [`provider-selector.md`](./provider-selector.md) — composer-time per-turn switcher; reads the same provider list.
- `docs/design/component-principles.md` — destructive-action contract and four-state rule.
- `docs/architecture/credentials.md` — backend keyring contract.

## Doesn't do

- Does not store usage / cost data — see [`usage.md`](./usage.md).
- Does not let the user *view* a stored key — the IPC contract is one-way (see [`credentials-section.md`](./credentials-section.md)).
- Does not switch the active provider — that's the dashboard Providers card (see [`providers-section.md`](./providers-section.md)).
- Does not configure session-time overrides — see [`provider-selector.md`](./provider-selector.md).
- Does not auto-probe connection status on load — `Test` is user-driven so the page does not generate background network traffic.
