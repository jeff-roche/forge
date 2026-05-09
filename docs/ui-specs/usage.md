# Usage

> Top-level pane ([F-594](https://github.com/forge-ide/forge/issues/612)) — token chart, configured-limits table, and per-model breakdown over the `usage_summary` IPC.

---

## Purpose

Give the user one surface to see how many tokens (and dollars) Forge has spent across providers and models, over a chosen time range, with optional cross-workspace aggregation. Limits surface here so the user can correlate consumption with configured caps; per-model breakdown surfaces here so cost spikes are attributable.

## Where

`<UsagePane>` mounts as the Usage window's root. Component path: `web/packages/app/src/components/usage/UsagePane.tsx`. The Usage window opens from the app menu (window-level wiring lives in `web/packages/app/src/routes/`).

## Size

Fills the usage window. Single column with three stacked sections: chart, limits table, per-model table.

## Structure

```
┌─ [Today] [Last 7] [Last 30] [All]    ☐ Cross-workspace ────┐
├─────────────────────────────────────────────────────────────┤
│ TOKENS BY PROVIDER                                          │
│ ┌─────────────────────────────────────────────────────────┐ │
│ │ <horizontal stacked bar — one segment per provider>     │ │
│ └─────────────────────────────────────────────────────────┘ │
├─────────────────────────────────────────────────────────────┤
│ PROVIDER LIMITS                                             │
│ ┌─────────────────────────────────────────────────────────┐ │
│ │ provider │ in        │ out      │ cap          │ used   │ │
│ │ anthropic│ 1,240,800 │ 312,400  │ 2,000,000/mo │ 78%    │ │
│ │ openai   │ 410,200   │  98,300  │ — not cfg    │ —      │ │
│ └─────────────────────────────────────────────────────────┘ │
├─────────────────────────────────────────────────────────────┤
│ PER MODEL                                                   │
│ ┌─────────────────────────────────────────────────────────┐ │
│ │ model           │ requests │ tokens in/out │ $cost      │ │
│ │ sonnet-4.5      │      214 │ 1.1M / 280K   │ $4.32      │ │
│ └─────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

### Toolbar

- **Range selector.** Four buttons — `Today`, `Last 7`, `Last 30`, `All` — wired to `UsageRange` `Today` / `Last7` / `Last30` / `All`. Active range carries `aria-pressed=true`. Default: `Last 30`.
- **Cross-workspace toggle.** Checkbox: `Cross-workspace`. When checked, totals aggregate across all workspaces; the IPC's `crossWorkspace=true` semantics apply — the shell logs a warning when the active workspace root is `null`, but the call still resolves.

### Sections

Three labeled `<section>`s, each with an uppercase mono label:

- **`TOKENS BY PROVIDER`** — `<UsageChart>` horizontal stacked bar driven by the provider-grouped breakdown. Rows with zero tokens are dropped from the chart by the chart component.
- **`PROVIDER LIMITS`** — `<UsageLimitsTable>` one row per provider showing in / out tokens, configured monthly cap (when the optional `limits` prop carries a row for that provider), and used % of cap. When no cap is configured, the row renders `// not configured` per `voice-terminology.md` §8.
- **`PER MODEL`** — `<UsageModelTable>` rows per model with request count, in / out tokens, and cost.

## Live data

The pane fetches **two** parallel `usage_summary` IPC calls per change — one grouped by `Provider` (drives the chart + limits table), one grouped by `Model` (drives the per-model table). Two calls is the simplest path that respects the IPC contract; client-side regrouping would force the UI to reconstruct totals from raw monthly aggregates.

The fetch key is `(range, crossWorkspace, workspaceRoot)`; changing any of the three triggers a refetch.

## States

- **Loading.** Placeholder line `usage · probing` (noun + state per `voice-terminology.md §8`) while the parallel fetches resolve. Per [`dashboard.md §D.5.1`](./dashboard.md#d51-loading-state-primitives-f-684), the canonical chart-bearing surface uses 1 chart placeholder + 3 table-row placeholders — that contract still applies; the Phase 3 implementation paints the probing line first and the skeleton primitive lands as the surface evolves.
- **Error.** Visible block with heading `USAGE UNAVAILABLE`, the verbatim error detail (preserved per `voice-terminology.md §8`), and a `Retry` button that re-invokes both `usage_summary` calls. Reading `data()` in the `errored` state re-throws inside Solid's reactive scope — the pane gates on `data.state` to surface the error cleanly.
- **Empty.** When the fetched range carries zero `tokens_in + tokens_out`: the chart section paints a `// no usage recorded for this range` placeholder; the limits and per-model tables are suppressed. Distinct from "fetch failed" — the fetch succeeded, the totals are simply zero.
- **Ready.** The three sections rendered above.

## Copy

- Range buttons: `Today`, `Last 7`, `Last 30`, `All`. Aria role: range buttons sit in a `role="group"` labelled `Usage time range`.
- Cross-workspace toggle: `Cross-workspace`.
- Section labels: `TOKENS BY PROVIDER`, `PROVIDER LIMITS`, `PER MODEL`.
- Loading copy: `usage · probing`.
- Error heading: `USAGE UNAVAILABLE`.
- Empty placeholder: `// no usage recorded for this range`.
- Limits no-cap fallback: `// not configured` (mono comment style, per `voice-terminology.md §8`).
- Retry button: `Retry`.

## Color & typography

- Section labels: `--font-mono` 11px, `--color-text-tertiary`, uppercase.
- Range button: ghost `Button` styling; active state uses `--color-ember-400` border.
- Chart segments: provider accent palette per `docs/design/ai-patterns.md`.
- Numbers throughout: `--font-mono`, `--color-text-primary`. `% of cap` uses `--color-warn` once consumption crosses 80%, `--color-error` past 100%.

## Keyboard

- Tab — toolbar (range buttons → cross-workspace toggle) → each section's interactive elements (Retry button when present).
- Inside the range button group: Tab moves through buttons; Space / Enter activates. Arrow keys are *not* hijacked — the buttons are independent toggles, not a radiogroup, because each click is independently addressable for keyboard users.

## Cross-spec references

- [`dashboard.md §D.5.1`](./dashboard.md#d51-loading-state-primitives-f-684) — skeleton primitive contract (F-684) for the future chart-skeleton replacement of the probing line.
- [`pane-header.md`](./pane-header.md) — pane-level cost meter format; the `tokens in/out · $cost` formatting in the per-model table follows the same shape.
- `docs/design/ai-patterns.md` — provider accent palette used by the stacked-bar chart.
- `docs/design/voice-terminology.md §8` — `noun · state` placeholders + `// not configured` mono-comment idiom.

## Doesn't do

- Does not let the user edit caps inline. Limits are configured in the user / workspace settings; the table reflects them.
- Does not surface session-level breakdown. The IPC's `GroupBy` exposes `Provider` and `Model`; per-session attribution is a future view.
- Does not export the data. CSV / JSON export is a deferred follow-up; the pane is read-only today.
- Does not auto-refresh on a timer. The fetch is reactive to (range, cross-workspace, workspaceRoot) changes only — the user re-selects a range to refresh.
