# Memory Editor

> Dashboard-scoped Markdown editor flyout ([F-602](https://github.com/forge-ide/forge/issues/620)) — opens from `memory-section.md`'s `EDIT` button to read and write one agent's memory file.

---

## Purpose

Edit a single agent's `~/.config/forge/memory/<agent>.md` file in a Monaco-backed Markdown editor without leaving the Dashboard. Decoupled from the row list so the editor can claim the full overlay viewport, host a Monaco iframe, and trap focus for the duration of the edit.

## Where

`<MemoryEditor>` mounts from `<MemorySection>` whenever `editing()` is non-null. Component path: `web/packages/app/src/components/dashboard/MemoryEditor.tsx`. Lifecycle is parent-owned — the section sets `editing` on `EDIT` click and clears it from the editor's `onClose`.

## Size

Modal overlay; ≈ 80% of the viewport (the modal class — `memory-editor` — owns the sizing). Backdrop covers the full window so the user can dismiss by clicking outside the dialog body.

## Structure

```
┌─ MEMORY — orchestrator    /home/u/.config/forge/memory/orchestrator.md   [CLOSE] ─┐
├───────────────────────────────────────────────────────────────────────────────────┤
│ ⚠ DO NOT store secrets in memory — anything here is appended verbatim to every    │
│    system prompt for this agent.                                                  │
│                                                                                   │
│  (read-only banner appears here when memory is disabled for this agent)           │
│                                                                                   │
│ ┌───────────────────────────────────────────────────────────────────────────────┐ │
│ │  <Monaco iframe>                                                              │ │
│ │  loaded language=markdown, uri=memory://buffer                                │ │
│ └───────────────────────────────────────────────────────────────────────────────┘ │
├───────────────────────────────────────────────────────────────────────────────────┤
│ unsaved                                                                  [SAVE]   │
└───────────────────────────────────────────────────────────────────────────────────┘
```

### Header

- Title: `MEMORY — <agentId>` (display font, em dash with single space either side).
- Path: absolute file path beside the title (mono, secondary).
- `CLOSE` button (ghost, top-right).

### Body

- Persistent secret warning (verbatim — duplicated from `memory-section.md`).
- **Read-only banner** (only when `readOnly` prop is `true`): `Memory is disabled for this agent — editor is read-only.` `role="status"`.
- Loading line: `loading…` (mono 11px, `role="status"`) until the initial `readAgentMemory` call resolves.
- **Editor body** — Monaco iframe in production, `<textarea>` test seam in jsdom (parent passes `useTextareaForTest`).

### Footer

- Status line (left). One of:
  - Error path: `error: <verbatim message>`
  - After save: `saved v<version>`
  - Dirty (unsaved): `unsaved`
  - Clean (saved or unmodified): `clean`
- `SAVE` button (right, primary). Hidden when `readOnly`. `disabled` while saving / loading / when `!isDirty`.

## Iframe protocol

The editor drives the Monaco iframe via the F-121 `postMessage` protocol. The full table is in `web/packages/monaco-host/README.md`; this spec records what the editor sends and listens for:

| Direction | `kind` | Payload | When |
|-----------|--------|---------|------|
| host → iframe | `open` | `{ uri, languageId: 'markdown', value }` | After `readAgentMemory` resolves and on every change to `loading()` |
| host → iframe | `save` | `{ uri }` | On `SAVE` click or `Cmd/Ctrl+S` |
| iframe → host | `ready` | `—` | Iframe boots; host replies with `open` |
| iframe → host | `change` | `{ uri, value }` | Every keystroke |
| iframe → host | `save` | `{ uri, value }` | Iframe responds to host's `save` request |

Origin discipline: every `postMessage` pins a concrete `targetOrigin` derived from the iframe `src`. Wildcards (`*`) and opaque (`null`) origins are refused — production resolves a real origin.

## States

- **Loading.** `readAgentMemory` in flight — `loading…` line; iframe / textarea hidden.
- **Loaded — clean.** Buffer matches `originalBody`. Status: `clean`. `SAVE` disabled.
- **Loaded — dirty.** User has typed; `body() !== originalBody()`. Status: `unsaved`. `SAVE` enabled.
- **Saving.** `saveAgentMemory` in flight; `SAVE` reports `aria-busy=true` and is disabled. Status keeps prior text.
- **Saved.** Status: `saved v<version>`. `originalBody` advances to the saved value so the editor re-enters `clean`.
- **Error.** `readAgentMemory` or `saveAgentMemory` rejected; status reads `error: <verbatim message>`. The user can fix and re-save; the editor does not auto-close on error.
- **Read-only.** When `readOnly` prop is true: read-only banner shows, `SAVE` button is suppressed, the iframe / textarea is locked. Body still loads so the user can review what's stored.

## Copy

- Title: `MEMORY — <agentId>` (em dash, single space either side).
- Close button: `CLOSE`. Save button: `SAVE`. Aria-label on close: `Close memory editor`.
- Read-only banner: `Memory is disabled for this agent — editor is read-only.`
- Status strings: `clean`, `unsaved`, `saved v<version>`, `error: <verbatim>`, `loading…`.
- Persistent warning: identical to `memory-section.md`'s warning string.

## Color & typography

- Title: `--font-display`. Path: `--font-mono`, `--color-text-tertiary`.
- Status line: `--font-mono` 11px, `--color-text-tertiary` (clean / unsaved / saved); `--color-warn` (error).
- Read-only banner: `--color-warn` background tint per `Button danger / status` tokens.

## Keyboard

- `Cmd+S` / `Ctrl+S` — request save (same as clicking `SAVE`). Window-level handler so it works from any focus location inside the dialog.
- `Escape` — closes the editor (window-level handler — the focus trap doesn't swallow it).
- Tab — focus is trapped inside the dialog (`useFocusTrap`).

## Security contract

- Editor draft state lives only in the component's local signal and the iframe's buffer. Nothing is written to disk until the user clicks `SAVE`.
- The persistent secret-warning banner appears in *every* mode (loaded, read-only, saving, error). The user is reminded each time the editor opens.
- `readOnly` mode hides `SAVE` and locks the textarea / iframe `readOnly` flag so editing is impossible — the only path that mutates the file is a server-side rejection of an attempted save when memory is disabled, and the UI never lets that path fire.
- Iframe `postMessage` calls always pin a concrete target origin so the body cannot leak to an unintended document if the iframe navigates.

## Cross-spec references

- [`memory-section.md`](./memory-section.md) — the Dashboard surface that opens this editor.
- `web/packages/monaco-host/README.md` — authoritative Monaco-iframe `kind`-tagged protocol reference.
- `docs/architecture/memory.md` — backend `read_agent_memory` / `save_agent_memory` invariants (the `version` advance returned in the save reply is what powers the `saved v<n>` status).
- `docs/frontend/architecture.md §9.3` — Monaco-hosting model.

## Doesn't do

- Does not version-diff or roll back. The `version` value is informational; the file is single-blob and the previous body is not preserved.
- Does not preview the rendered Markdown. Memory is a system-prompt suffix, not a published doc — there's no rendered "view" mode.
- Does not auto-save on close. Closing while dirty discards the draft (warning text already on screen serves as the gating affordance).
- Does not surface schema errors (frontmatter parse failures, etc.) — the backend rejects malformed saves and the verbatim message lands in the status line. A future iteration may add inline diagnostics.
