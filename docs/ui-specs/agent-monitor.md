# Agent Monitor

> Extracted from SPECS.md §9 — three-column layout: agent list, trace timeline, and inspector panel

---

## 9. Agent monitor

**Purpose.** Observe, trace, and control every agent across all sessions from one view.

**Layout.** Three columns: list (280px) | trace (flex) | inspector (340px).

### 9.1 Agent list (left)

**Row.**
```
[provider dot] name                            #a3f1.1
              sub-agent · 6/11 steps
[============================            ]          ← progress bar, 3px
running · 3m · $0.09              sonnet-4.5        ← meta row
```
- Progress bar colors by state: done=ok, running=warn, queued=text-tertiary, error=err
- Active agent has 2px left border in ember and `--color-surface-2` bg
- Hover: `--color-surface-2`

**Background agent marker.** Agents that are user-initiated top-level background workers (not sub-agents of anything) show a small `BG` pill in the meta row. Sub-agents show `↳ parent-name` instead.

**Sort.** Default: running first, then queued, then error, then done — within each group, most recent first.

**Filter.** Tabs at the top: `All · Running · Background · Session · Failed`.

### 9.2 Trace (middle)

**Header.** Agent name big, id small, live state chip. The chip uses an ember accent while running and the neutral surface chip for `queued` / `done` / `error`. Running format is `running · step N of M` when both [F-606](https://github.com/forge-ide/forge/issues/652)'s `index` and `total` are populated, falling back to `running · step N` when the orchestrator streams step-by-step (today's common case — `total` rides as `None` until the orchestrator pre-plans or emits a retroactive total) and to bare `running` for legacy events with no index. `Stop` / `Pause` / `Resume` / `Interrupt + refine` / `Export transcript` / `Promote to pane` are Inspector-side actions (§9.3) — the trace-header focuses on observation so the toolbar stays purely informational.

**Toolbar.** Elapsed (`mm:ss`, live 1Hz; caps at `99:59+` for sessions over 100 minutes; `—` while `started_at` is unknown), model label, tools-used count (declared `SubAgentSpawned.tool_count` for sub-agents, live aggregation of `ToolCallStarted.tool` for the session-root), spawned-by parent name (`↳ orchestrator` style; falls back to a truncated 8-char id when the parent row isn't loaded). All cells render in Fira Code 10px separated by `·`. The toolbar is intentionally NOT an `aria-live` region — the elapsed cell ticks every second and wrapping the toolbar in `aria-live=polite` would re-announce the entire toolbar every tick (a WCAG live-region anti-pattern); cells expose values via `aria-label`s and are read on demand via focus traversal. Token in/out and cost cells are deferred to a follow-up task — they'll join the toolbar once the §9.2 design is reconciled with the existing `live_session_totals` walker.

**Timeline.** Vertical list of steps. Each step:
- 16px filled dot, colored by state (done/ok, run/warn, queued/text-tertiary, err/error)
- 2px vertical rail connecting dots
- Running step has a pulsing ring (2s infinite, opacity 0→1)
- Content: step kind chip (`tool`, `think`, `spawn`, `mcp`), title line, optional description, optional preview box (mono 11px)

**Step kinds and colors.**
| Kind | Chip bg | Chip text | Meaning |
|---|---|---|---|
| `tool` | `rgba(255,209,102,.05)` | ember-100 | Tool invocation (fs, shell, etc.) |
| `mcp` | `info-bg` | info | MCP server call |
| `spawn` | `rgba(255,74,18,.05)` | ember-400 | Child agent spawned |
| `think` | surface-2 | text-secondary | Model reasoning pass |

**Interaction.**
- Click step: expands preview inline
- Double-click: opens a detail drawer from the right

### 9.3 Inspector (right)

Five sections:

1. **Definition.** name, source (file + line), provider, model, isolation level, max tokens.
2. **Allowed tools.** Pills, each with click-to-view policy.
3. **Allowed paths.** Pills with mono text; glob patterns rendered verbatim.
4. **Resource usage.** cpu, rss, fd open, net connections — live, 1Hz update.
5. **Actions.**
   - `Stop agent` — wires through the `stop_background_agent` Tauri command onto `Orchestrator::stop(id)`. Available on every row.
   - `Pause` / `Resume` ([F-603](https://github.com/forge-ide/forge/issues/603)) — `session_pause` / `session_resume` IPC. Idempotent on the backend; the button label flips off `Event::SessionPaused` / `SessionResumed` so the UI and the daemon stay in lock-step without an optimistic toggle. Session-root row only.
   - `Interrupt + refine` ([F-604](https://github.com/forge-ide/forge/issues/604)) — `session_interrupt_and_refine` IPC. Returns a [`RefineHandoff`](../../web/packages/ipc/src/generated/RefineHandoff.ts) carrying the partial assistant text + capture anchors (`captured_at_step_id`, `captured_at_msg_id`), opens a refine composer dialog seeded with the partial text. The composer's `COPY + CLOSE` writes to the clipboard for paste into the session window's chat composer (the AgentMonitor route can't reach the chat composer directly today). Session-root row only.
   - `Export transcript` ([F-607](https://github.com/forge-ide/forge/issues/607)) — `export_transcript` IPC returns the raw `events.jsonl` bytes (capped at 50 MiB). Triggers a Blob/anchor-click download in the webview as `forge-transcript-<sessionId>.jsonl`. Session-root row only; a future iteration may swap in `@tauri-apps/plugin-dialog`'s `save` dialog once that plugin is allow-listed.
   - `Promote to pane` — `promote_background_agent` IPC; navigates back to the session window so the promoted agent's transcript is in front of the user. Background-agent + sub-agent rows only.

### 9.4 States

The agent list (§9.1) renders all four `component-principles.md` states distinctly — a `list_background_agents` rejection must never collapse into the empty placeholder:

- **Loading:** placeholder line `agents · probing` (noun + state per `voice-terminology.md` §8) while `list_background_agents` resolves.
- **Error:** visible block inside the list column with heading `AGENT LIST UNAVAILABLE`, the verbatim error detail (preserved exactly per `voice-terminology.md` §8 "show technical identifiers verbatim"), and a `RETRY` button that re-invokes `list_background_agents`.
- **Empty:** `// no agents` mono-comment placeholder once the fetch succeeds with zero rows.
- **Ready:** the filtered + sorted row list from §9.1.

**Doesn't do.**
- Does not let you edit agent definitions inline (opens the source file instead)
- Does not surface prompts verbatim — use `Export transcript` for the full record
