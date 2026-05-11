# New Session Flow

> UI-only spec ([F-710](https://github.com/forge-ide/forge/issues/710)) — session-spawn flow triggered from the dashboard hero `+ New session` CTA. Wraps the daemon spawn the existing `forge run …` CLI performs (`session_new` in `crates/forge-cli/src/main.rs`) behind a Tauri IPC command `session_start` (delivered by F-725).

---

## Purpose

Let the operator start a Forge session from the Dashboard window without dropping to a terminal. The flow gathers the three inputs the daemon needs — workspace root, provider, agent — validates them client-side, and hands them to the `session_start` IPC. On success the Dashboard hands off to the freshly-opened Session window (the `v-fresh` blank-canvas mock in `docs/forge-mocks.html`); on failure the form re-enables and surfaces the verbatim daemon error.

## Where

The trigger is the `+ New session` button in the Dashboard hero (`DESIGN.md §Hero block`). The form mounts as a focus-trapped modal anchored at id `new-session-modal`, layered above the Dashboard root via the app-shell modal portal (see [`app-shell.md`](./app-shell.md)). Component path: `web/packages/app/src/components/dashboard/NewSessionModal.tsx`. The modal is the only entry point in V1 — the `Attach to session` hero CTA is a separate flow (session list re-entry) and does not pass through this spec.

## Size

Modal surface: `min-width: 480px`, content-driven height, capped at `max-height: 80vh` with internal scroll for the form body if the viewport is short. Mounted via the shared `<Modal>` primitive — `surface-2` card chrome, `var(--sp-6)` padding, focus trap, `Escape` dismisses, backdrop is `rgba(0,0,0,0.6)` and click-dismisses.

## Trigger

The Dashboard hero renders two CTAs (per the `v-dash` mock, lines 1106–1109 of `docs/forge-mocks.html`):

- `Attach to session` — ghost; out of scope for this spec.
- `+ New session` — primary, ember (`var(--color-ember-400)`); opens this modal.

Activation paths:

- Mouse click on `+ New session`.
- Keyboard: `n` while the Dashboard root has focus and no input is focused (mirrors the command-palette single-key conventions documented in [`command-palette.md`](./command-palette.md)).

On click, the modal opens to the `idle` state with the workspace field focused. There is no inline composer alternative — the Dashboard is intentionally flat (see [`dashboard.md`](./dashboard.md) §D.1) and inline form chrome would compete with the session roster for the same vertical band.

### Empty-workspace branch

When no workspace is currently open in the calling Dashboard context (the bridge's `cached_workspace_root` returns `None`), `+ New session` does **not** open the form directly. Instead it dispatches the native file picker via `tauri-plugin-dialog`'s directory-select mode, scoped to the user's home directory. Two outcomes:

- **Picker confirms a directory.** The selected absolute path becomes the prefilled `workspace_root` and the modal opens to the `idle` state with focus on `provider` (the workspace field is already valid).
- **Picker is cancelled.** Nothing further happens; no modal, no toast. The user remains on the Dashboard.

When a workspace *is* already cached, `+ New session` opens the modal with `workspace_root` prefilled from the cached value and focus on `workspace_root` (so the operator can override before submitting). This is the two-step contract: pick a workspace first, then fill the form.

## Form

```
┌─ NEW SESSION ───────────────────────────────────────────── [×] ─┐
│                                                                 │
│ WORKSPACE                                                       │
│ [ ~/code/acme-api                                    ] [Browse] │
│                                                                 │
│ PROVIDER                                                        │
│ [ anthropic                                              ▾ ]    │
│                                                                 │
│ AGENT                                                           │
│ [ orchestrator                                           ▾ ]    │
│                                                                 │
│                                          [Cancel] [Start session]│
└─────────────────────────────────────────────────────────────────┘
```

### Fields

| Field           | Type              | Default                                                  | Validation                                                                                  |
|-----------------|-------------------|----------------------------------------------------------|---------------------------------------------------------------------------------------------|
| `workspace_root` | text + `Browse`  | cached workspace (or value chosen in the empty-workspace branch) | required; non-empty; absolute path; `Browse` re-opens the directory picker.            |
| `provider`      | dropdown          | the `default_provider` recorded in user settings, falling back to the first `ready` entry returned by `dashboard_list_providers` | optional on the wire; the dropdown is always populated so a value is always sent.       |
| `agent`         | dropdown          | `orchestrator`                                           | optional on the wire; populated from `list_agents`; defaults to `orchestrator` if present. |

Labels use the `mono-xs` input-label style per [`component-principles.md` §Inputs](../design/component-principles.md). Inputs render with the standard token-driven borders — default `var(--color-iron-600)`, hover `var(--color-iron-300)`, focus `var(--color-ember-400)`, error `var(--color-ember-400)`.

Dropdown sourcing:

- **Provider list** comes from the same `dashboard_list_providers` IPC the Dashboard's Providers card uses (see the Providers section of `dashboard.md`; the standalone `providers-page.md` will own its admin chrome once authored). Entries whose status is `auth` or `error` render disabled with the verbatim status word in `var(--color-text-tertiary)` after the name — the user can see why a provider is greyed out without leaving the modal. Selecting a credential-blocked entry is impossible from the dropdown; the credential remediation lives in [`credentials-section.md`](./credentials-section.md).
- **Agent list** comes from `list_agents`. If the call returns an empty roster the dropdown collapses to a single `orchestrator` option (the built-in fallback). If the call rejects, the dropdown renders disabled with `agents · unavailable` and the form still submits — `agent` is optional on the wire so the daemon picks `orchestrator`.

### Primary / secondary actions

- **Cancel** — ghost (`btn-ghost`). Closes the modal without dispatching IPC. `Escape` is the keyboard equivalent.
- **Start session** — primary, ember (`btn-pri`). Disabled until `workspace_root` is non-empty. `Enter` while focus is inside the modal submits.

The action cluster sits flush-right at the foot of the modal, matching the form-modal pattern in [`approval-prompt.md` §10.1](./approval-prompt.md) (ghost leads, primary trails; primary carries the keyboard default).

## States

The modal renders all four `docs/design/component-principles.md` states distinctly:

| State           | Form chrome                                                                                                                          | Primary button                                  | Error region                                              |
|-----------------|--------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------|-----------------------------------------------------------|
| `idle`          | All fields enabled, no skeletons. The first invalid (or first if none) field is focused.                                             | Enabled iff `workspace_root` is non-empty.       | Absent.                                                   |
| `validating`    | Fields enabled, error region absent. Transient — typically a single tick while the client-side path check runs.                       | Disabled with the idle label.                   | Absent.                                                   |
| `spawning`      | Fields disabled (read-only borders, `iron-600` background per the disabled-button rule — never opacity).                              | Disabled, label swaps to `Starting…`, 6px pulse dot on the leading edge. | Absent.                                                   |
| `spawn-failed`  | Fields re-enabled, focus jumps to the field the error cites if parseable (`workspace_root` for path errors; otherwise `workspace_root` by default). | Enabled; label reverts to `Start session`.       | `role="alert"` line above the action cluster carrying the verbatim daemon string. |

State badges in the header use the `mono-xxs` status-pill style ([`DESIGN.md` §Status pill](../../DESIGN.md)) — `spawning` is the `streaming` variant (ember-200, pulsing); `spawn-failed` is the `auth` variant (error + glow). `idle` and `validating` render no badge — the form itself is the affordance.

The success path does not have a steady-state — when `session_start` resolves, the Dashboard window dispatches the session-window open event (the same path `open_session` walks today, see [`dashboard.md`](./dashboard.md) §D.4) and dismisses the modal. The freshly-opened Session window paints the `v-fresh` blank canvas (mocks line 1338).

## IPC contract

The form dispatches a single Tauri command, `session_start`, on `Start session` submit. F-725 implements it as a thin wrapper around the same daemon spawn `forge_cli::session_new` performs in `crates/forge-cli/src/main.rs` lines 144–198 — generate a `SessionId`, write a UDS socket path, exec the `forged` binary with `FORGE_SESSION_ID` / `FORGE_SOCKET_PATH` / `FORGE_WORKSPACE` env vars and `--agent` / `--provider` flags, wait for the socket to appear, return the id.

### Input

```ts
type SessionStartArgs = {
  workspace_root: string;     // absolute path; required
  provider?: string;          // provider id from `dashboard_list_providers`; omit to use daemon default
  agent?: string;             // agent name from `list_agents`; omit for `orchestrator`
};
```

### Output (success)

```ts
type SessionStartOk = {
  session_id: string;         // the `SessionId::new()` the daemon allocated
};
```

### Output (error)

A verbatim string per the F-673 standardized-prefix contract (`crates/forge-shell/src/ipc.rs` lines 54–93). The command-named prefix is `session_start: `; the wrapped daemon error follows untouched. Examples the form surfaces verbatim into `spawn-failed`:

- `session_start: workspace not accessible: No such file or directory (os error 2)`
- `session_start: provider 'openai' unavailable: credentials expired`
- `session_start: agent 'orchestrator' not found in agent roster`
- `session_start: forged failed to bind socket: Address already in use (os error 98)`

The form does not parse, summarize, or rewrite these. They paint as-is in the error region (`var(--font-mono)`, `--type-mono-xs`, `var(--color-text-primary)` text on `var(--color-bg)` — the same treatment the Sessions panel applies to `session_list` failures per [`dashboard.md`](./dashboard.md) §D.5). Per `voice-terminology.md` §8 ("show technical identifiers verbatim"), this is the user's only signal of *what* went wrong; collapsing it loses debuggability.

## Copy

- Modal title: `NEW SESSION` (mono, all-caps, letter-spaced — the `--type-mono-xs` label style).
- Field labels: `WORKSPACE`, `PROVIDER`, `AGENT`.
- Workspace browse button: `Browse`.
- Provider dropdown disabled-entry suffix: `<name> · <status>` (e.g. `OpenAI · auth`, `Mistral · error`).
- Agent dropdown unavailable placeholder: `agents · unavailable`.
- Primary action idle: `Start session`. Spawning: `Starting…`. (Ellipsis is a real `…`, not three dots.)
- Cancel action: `Cancel`.
- Empty-workspace picker title (passed to `tauri-plugin-dialog`): `Pick a workspace for the new session`.
- Picker-cancelled silent path: no toast, no copy.
- Error region prefix: none — the daemon string carries its own `session_start:` prefix.

## Keyboard

- `Tab` order inside the modal — `workspace_root` → `Browse` → `provider` → `agent` → `Cancel` → `Start session` → (wraps back to `workspace_root` via focus-trap).
- `Enter` submits from any field; `Escape` cancels and closes the modal.
- `n` on the Dashboard root opens the modal (or the empty-workspace picker first, per §Trigger).
- The modal carries `role="dialog"` + `aria-modal="true"` + `aria-labelledby` pointing to the `NEW SESSION` title; the error region carries `role="alert"` so screen readers announce daemon failures the moment they paint.

## Cross-spec references

- [`dashboard.md`](./dashboard.md) — root-window layout; the hero CTAs that trigger this flow live in the dashboard's top band, and the post-spawn handoff reuses the dashboard's `open_session` dispatch.
- [`app-shell.md`](./app-shell.md) — hosts the modal portal; the modal mounts at the shell's modal layer, not inside the Dashboard route.
- `providers-page.md` *(authored in parallel under F-708)* — owns the long-form provider management surface; the provider dropdown's entries originate from the same `dashboard_list_providers` query the providers page renders. Until that spec lands, the read source of truth is the Providers card on the Dashboard.
- [`credentials-section.md`](./credentials-section.md) — owns the remediation path when a provider is `auth` or `error`. The new-session form disables those provider entries; it does not relogin inline.
- [`approval-prompt.md`](./approval-prompt.md) §10.1 — form-modal action pattern this spec mirrors (ghost cancel, ember primary, keyboard default on primary).
- [`component-principles.md`](../design/component-principles.md) §Inputs / §Buttons — input border tokens and the button-disabled rule (`iron-600` background, never opacity).
- [`DESIGN.md` §Hero block](../../DESIGN.md) — the trigger CTAs; [`DESIGN.md` §Status pill](../../DESIGN.md) — the `spawning` / `spawn-failed` badge variants.
- `crates/forge-cli/src/main.rs` lines 144–198 — the existing CLI spawn path `session_start` wraps; the IPC carries no new daemon semantics, only a window-tier surface for the same call.
- `crates/forge-shell/src/ipc.rs` lines 54–93 — the F-673 error-prefix standard the verbatim error string follows.

## Doesn't do

- Does not let the operator create or edit a provider entry. The dropdown is read-only; provider management lives in the providers page.
- Does not relogin a credential-blocked provider. The entry is disabled in the dropdown; the operator routes through credentials-section to remediate.
- Does not let the operator pick a model. Model selection happens inside the session window (see [`provider-selector.md`](./provider-selector.md)); the new-session form picks only a provider.
- Does not retry on `spawn-failed`. The form re-enables and the operator chooses whether to amend the inputs and resubmit, or cancel. No exponential backoff, no auto-retry — the daemon failures are typically input or environment problems and a silent retry would just mask them.
- Does not surface the in-flight `forged` startup log. The modal shows only the verbatim error string on failure; deeper diagnosis routes through the daemon log via the regular session-window log surface.
