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

**Header (Phase 2).** Agent name big, id small, live state chip (`running · step N` when running; otherwise the bare state). The chip uses an ember accent while running and the neutral surface chip for `queued` / `done` / `error`. `N` reads from `StepStarted.index` (1-based, [F-606](https://github.com/forge-ide/forge/issues/652)).

**Header (Phase 3 — `Pause` / `Kill` / `Promote` deferred to [F-449](https://github.com/forge-ide/forge/issues/504)).** The chip's `step N of M` form is gated on `StepStarted.total` being populated. Today the orchestrator streams steps turn-by-turn so `total` rides as `None` and the UI shows the bare `step N` form; rendering `of M` lights up automatically once the orchestrator pre-plans (or emits a retroactive total). The `Pause` / `Kill` buttons plus `Promote to pane` for background agents remain blocked on F-449. Phase-2 surfaces `Stop agent` via the Inspector only (§9.3).

**Toolbar (Phase 3 — deferred to [F-449](https://github.com/forge-ide/forge/issues/504)).** Elapsed, token in/out, cost, model, tools-used, spawned-by relationship — all in Fira Code 10px separated by `·`. Blocked on backend plumbing for cost/token/tool-use aggregation.

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
5. **Actions (Phase 2).** Single `Stop agent` action — wires through the `stop_background_agent` Tauri command onto `Orchestrator::stop(id)`.

**Actions (Phase 3 — deferred to [F-449](https://github.com/forge-ide/forge/issues/504)).** `Pause agent`, `Interrupt + refine` (opens a refine composer in context), `Export transcript` (JSONL), `Promote to pane` (background only). Blocked on backend primitives for pause, refine, transcript export, and pane promotion.

### 9.4 States

The agent list (§9.1) renders all four `component-principles.md` states distinctly — a `list_background_agents` rejection must never collapse into the empty placeholder:

- **Loading:** placeholder line `agents · probing` (noun + state per `voice-terminology.md` §8) while `list_background_agents` resolves.
- **Error:** visible block inside the list column with heading `AGENT LIST UNAVAILABLE`, the verbatim error detail (preserved exactly per `voice-terminology.md` §8 "show technical identifiers verbatim"), and a `RETRY` button that re-invokes `list_background_agents`.
- **Empty:** `// no agents` mono-comment placeholder once the fetch succeeds with zero rows.
- **Ready:** the filtered + sorted row list from §9.1.

**Doesn't do.**
- Does not let you edit agent definitions inline (opens the source file instead)
- Does not surface prompts verbatim — use `Export transcript` for the full record
