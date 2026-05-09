# Memory Section

> Dashboard section ([F-602](https://github.com/forge-ide/forge/issues/620)) — per-agent cross-session memory toggles, status, and entry points to the editor.

---

## Purpose

Let the user inspect, enable / disable, edit, and clear each loaded agent's cross-session memory file. The memory body is appended verbatim to that agent's system prompt on every session start, so the user needs a first-class surface for it — not just a config toggle buried in settings.

## Where

`<MemorySection>` mounts inside the Dashboard root, anchored at id `memory-section`. Component path: `web/packages/app/src/components/dashboard/MemorySection.tsx`. The editor flyout it opens is specced separately in [`memory-editor.md`](./memory-editor.md).

## Size

Fills the dashboard column width. Vertically scrolling row list — one row per loaded agent definition for the workspace.

## Structure

```
┌─ MEMORY ───────────────────────────────────────────────────┐
│ ⚠ DO NOT store secrets in memory — anything here is        │
│   appended verbatim to every system prompt for this agent. │
│                                                            │
│ ┌─ orchestrator                                          ─┐│
│ │  [✓] ENABLED (override)              [EDIT] [CLEAR]    ││
│ │  /home/u/.config/forge/memory/orchestrator.md          ││
│ │  4.2 KiB · v7 · updated 4/26/2026, 10:14:02 AM         ││
│ └────────────────────────────────────────────────────────┘│
│ ┌─ planner                                              ─┐│
│ │  [ ] DISABLED (inherit)              [EDIT]           ││
│ │  /home/u/.config/forge/memory/planner.md              ││
│ │  — bytes · v— · updated never                         ││
│ └────────────────────────────────────────────────────────┘│
└────────────────────────────────────────────────────────────┘
```

### Row anatomy

- **Head row.** Agent id (display font), enable toggle, action buttons.
  - Toggle label: `ENABLED (<source>)` or `DISABLED (<source>)` where `<source>` is `inherit` (no settings override — the def-level default applies) or `override` (workspace settings override is set).
  - `EDIT` opens the editor flyout. Read-only when the effective enabled flag is `false` — the editor still loads so the user can review what's stored.
  - `CLEAR` is shown only when `size_bytes !== null` (a non-empty file exists). Clicking opens the confirm modal below.
- **Path row.** Absolute path of the memory file on disk so the user can locate / back up the file outside the app.
- **Meta row.** `<size>` (`B` / `KiB` / `MiB` / `— bytes` for absent), `v<version>` (`v—` for absent), `updated <localized timestamp>` (`updated never` for absent).

### Persistent secret warning

A `role="note"` line at the top of the section: `DO NOT store secrets in memory — anything here is appended verbatim to every system prompt for this agent.` The same string is duplicated verbatim inside the editor flyout (see `memory-editor.md`) so the warning lands whether the user edits via the Dashboard or via an agent's `memory.write` tool.

### Clear confirmation modal

```
┌─ CLEAR MEMORY? ────────────────────────────┐
│ This wipes <agent>'s memory file. The      │
│ previous body cannot be recovered.         │
│ Continue?                                  │
├────────────────────────────────────────────┤
│                          [CANCEL]  [CLEAR] │
└────────────────────────────────────────────┘
```

Focus-trapped `role="dialog" aria-modal="true"`. Window-level `Escape` cancels.

## States

- **Loading.** First fetch via `listAgentMemory` — the section paints the warning + an empty list. A list-fetch rejection degrades silently to an empty list (the warning still renders); the toggle / clear actions surface their own errors.
- **Empty.** `No agent definitions loaded for this workspace.` placeholder when `entries.length === 0`. (Distinct from "agents loaded but none have memory files" — every agent gets a row regardless.)
- **Error.** `role="alert"` line under the warning whenever a `setSetting` (toggle) or `clearAgentMemory` call rejects. Carries the verbatim error detail.
- **Ready.** The row list above.

Each row's effective state is a function of `def_enabled` and `settings_override`: `effectiveEnabled(entry) = settings_override ?? def_enabled`. The toggle reflects the effective state; the source label tells the user whether it's an override or an inherited default.

## Copy

- Section label: `MEMORY`
- Persistent warning (verbatim, duplicated in `docs/architecture/memory.md` and the editor): `DO NOT store secrets in memory — anything here is appended verbatim to every system prompt for this agent.`
- Empty placeholder: `No agent definitions loaded for this workspace.`
- Toggle labels: `ENABLED (override)`, `ENABLED (inherit)`, `DISABLED (override)`, `DISABLED (inherit)`.
- Action buttons: `EDIT`, `CLEAR`, `CANCEL`.
- Size formatting: `<n> B` (n<1024), `<n> KiB` (n<1 MiB, 1 decimal), `<n> MiB` (≥1 MiB, 2 decimals), `— bytes` (null).
- Timestamp formatting: `new Date(iso).toLocaleString()` for present values; `never` for null.
- Confirm title: `CLEAR MEMORY?`
- Confirm body: verbatim — "This wipes <strong>{agentId}</strong>'s memory file. The previous body cannot be recovered. Continue?"

## Color & typography

- Section label: `--font-display`, uppercase.
- Warning: `--color-warn` text on `--color-surface-2` background, `role="note"`.
- Path: `--font-mono`, `--color-text-tertiary`.
- Meta line: `--font-mono` 11px, `--color-text-tertiary`, separator `·` between fields.
- `CLEAR` confirm button uses `Button` `variant="primary"` (ember accent) — destructive but deliberate; the modal is the gate.

## Keyboard

- Tab — toggle → `EDIT` → `CLEAR` → next row.
- Inside the clear modal — focus is trapped. `Escape` cancels (window-level listener).
- Inside the editor flyout — see [`memory-editor.md`](./memory-editor.md).

## Cross-spec references

- [`memory-editor.md`](./memory-editor.md) — the Markdown editor flyout this section opens.
- [`dashboard.md`](./dashboard.md) — root-window layout.
- `docs/architecture/memory.md` — backend `read_agent_memory` / `save_agent_memory` / `clear_agent_memory` contract and the version / size / mtime invariants.
- `docs/design/voice-terminology.md §8` — the noun + state placeholder convention used by `inherit` / `override` / `never`.

## Doesn't do

- Does not show the body inline — that's the editor's job. The row is intentionally compact so the user can scan all agents at a glance.
- Does not surface non-agent memory (project notes, scratchpads). The keyspace is `[memory.enabled.<agent>]` only.
- Does not let the user *delete* the file — only `CLEAR` (wipe to empty). Delete is a config-file operation today.
- Does not auto-disable on enable-flag flip — the running session keeps its prior memory snapshot until the next session start. The session daemon consults the merged settings on session start (F-602 wiring in `serve_with_session`).
