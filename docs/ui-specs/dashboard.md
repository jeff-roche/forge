# Dashboard

> Root window of the desktop app ([F-708](https://github.com/forge-ide/forge/issues/708)) — the V1 dashboard renders the app shell, a hero block with dual CTAs, and a 12-column grid of five cards (Sessions, Providers, Usage, Enabled, Containers).

---

## Purpose

First view in the Dashboard window. Confirms the workspace state at a glance (active sessions, configured providers, recent usage, enabled extensions, sandbox containers) and gives the operator one click to attach to an existing session or spawn a new one.

## Where

Root route (`/`) of the Dashboard window. There is exactly one Dashboard window per app instance. Mounted inside the [app shell](./app-shell.md): the Activity bar and Sidebar frame the route on the left, the Status bar pins to the bottom, and `<Dashboard/>` renders into the router outlet between them.

Component path: `web/packages/app/src/routes/Dashboard/Dashboard.tsx`.

## Size

Fills the outlet. Hero block sits at the top with `padding: var(--sp-8)`; the grid below uses the `DESIGN.md §Layout grid` 12-column track with `var(--sp-6) var(--sp-8)` outer padding and `var(--sp-4)` cell gaps. Content-driven height — the page scrolls when the grid exceeds the viewport.

## Structure

```
┌──────────────────────────────────────────────────────────────────────┐
│  app shell — activity bar · sidebar · status bar                     │
├──────────────────────────────────────────────────────────────────────┤
│  HERO                                                                │
│  Welcome back.                                                       │
│  Forge something.                            [Attach] [+ New session]│
│  Two sessions active. One agent paused awaiting approval. …          │
├──────────────────────────────────────────────────────────────────────┤
│  ┌──────────────────────────── col-8 ────────────────┐ ┌── col-4 ──┐ │
│  │ Sessions          active · 2 archived · 14        │ │ Providers │ │
│  │ ● refactor-payment-service          streaming     │ │           │ │
│  │ ● doc-site-rewrite          awaiting approval     │ │ Anthropic │ │
│  │ ● test-harness-migration              done        │ │ OpenAI    │ │
│  │ ● sql-query-optimizer                 idle        │ │ Ollama    │ │
│  └───────────────────────────────────────────────────┘ │ Mistral   │ │
│                                                        └───────────┘ │
│  ┌──────────────── col-6 ─────────────┐ ┌────────── col-6 ──────────┐│
│  │ Usage · last 7 days       7D 30D MTD│ │ Enabled · workspace      ││
│  │ $18.42       4.2M         1.1M      │ │ typescript-review    on  ││
│  │ spend        tokens in    tokens out│ │ postgres-schemata    on  ││
│  │ ──sparkline──────────────────────── │ │ github               on  ││
│  └─────────────────────────────────────┘ └───────────────────────────┘│
│  ┌─────────────────────────── col-12 ──────────────────────────────┐ │
│  │ Containers  3 running · 2.4 GB · podman          [Prune] [Logs] │ │
│  │ ● refactor-sandbox-a3f1     oci.io/forge/rust-tools  [term][stop]│ │
│  │ ● docs-preview              oci.io/forge/node-preview [term][stop]│ │
│  │ ◐ pg-test-db                postgres:16              [term][stop]│ │
│  └─────────────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────────┘
```

Reading order, top to bottom: app shell chrome → hero → primary row (Sessions / Providers) → secondary row (Usage / Enabled) → full-bleed row (Containers).

## App shell

The Dashboard does not render its own chrome — see [`app-shell.md`](./app-shell.md). The Activity bar paints `Dashboard` as the active route, the Sidebar paints the `Sessions` row as `aria-current="page"`, and the Status bar's left slot renders `forge · <workspace-name> · <branch>` while the right slot renders the active provider / streaming / cost segments. Route content slots between Sidebar and Status bar.

## Hero block

A two-column hero — see `DESIGN.md §Hero block`. Headline on the left, CTA cluster on the right, aligned to the baseline.

### Anatomy

```
┌─────────────────────────────────────────────────────────────────────┐
│ Welcome back.                                                       │
│ Forge something.                          [Attach] [+ New session]  │
│                                                                     │
│ Two sessions active. One agent paused awaiting approval.            │
│ Anthropic and local Ollama connected — OpenAI awaiting credentials. │
└─────────────────────────────────────────────────────────────────────┘
```

- **Headline.** Two lines. First line `Welcome back.` in `var(--color-text-primary)`. Second line `<em>Forge</em> something.` with the brand word in `var(--color-ember-400)` (inline `<em>`, `font-style: normal`). Display font, weight 800, uppercase, `letter-spacing: 0.02em`.
- **Status sentence.** A single rendered paragraph beneath the headline summarizing the workspace state. Body font, `var(--color-text-secondary)`, `max-width: 560px`. The sentence is templated from live data: active-session count, paused-agent count, provider readiness summary.
- **CTA cluster.** Exactly two buttons, `gap: var(--sp-2)`, primary on the right:
  - `Attach to session` — ghost secondary. Opens the attach picker (F-727). Disabled when there are zero active sessions.
  - `+ New session` — ember primary, leading `+` glyph (12px). Opens the new-session flow (F-726) — see new-session-flow.md when it lands. Always enabled.

### States

| State    | Trigger                            | Visual                                                     | Verbatim copy                                                                                  |
| -------- | ---------------------------------- | ---------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| loading  | workspace summary pending          | headline + buttons paint immediately; status sentence renders as `forge · …` placeholder | `forge · …`                                                                                    |
| empty    | zero sessions, zero providers      | headline + buttons paint; status sentence narrates the empty state | `No sessions yet. Configure a provider to get started.`                                        |
| error    | workspace summary rejects          | status sentence collapses to the verbatim error fragment; CTAs remain enabled | `Couldn't load workspace status.`                                                              |
| ready    | summary resolves                   | full hero — headline, status sentence, two CTAs            | `Welcome back.` / `Forge something.` / `Two sessions active. One agent paused awaiting approval. Anthropic and local Ollama connected — OpenAI awaiting credentials.` |

## Grid

The dashboard body uses the `DESIGN.md §Layout grid` 12-column track. Card spans:

| Row       | Card                  | Span    | Notes                                                                            |
| --------- | --------------------- | ------- | -------------------------------------------------------------------------------- |
| primary   | [Sessions](#sessions-card)        | `col-8` | Left, primary surface — most-recent sessions inline.                              |
| primary   | [Providers](#providers-card)      | `col-4` | Right, secondary surface — provider readiness at a glance.                        |
| secondary | [Usage](#usage-card)              | `col-6` | KPI tile + spark chart over the last range.                                       |
| secondary | [Enabled](#enabled-card)          | `col-6` | Workspace-enabled skills, MCP servers, agents.                                    |
| third     | [Containers](#containers-card)    | `col-12`| Full-bleed sandbox container card. Body owned by [`containers-section.md#container-card-on-dashboard`](./containers-section.md#container-card-on-dashboard). |

Sub-12 widths require an ADR per `DESIGN.md §Layout grid`.

## Sessions card

The `col-8` primary card. Surfaces the workspace's recent sessions inline so the operator can resume a thread in one click.

### Anatomy

```
┌─ SESSIONS   active · 2 | archived · 14    2 running · 5 idle    [View all] ─┐
│ ⚒  ● refactor-payment-service  #a3f1                       streaming        │
│      ~/code/acme-api · claude-sonnet-4.5 · 3 agents · 12m                   │
│ ☉  ● doc-site-rewrite  #b8c0                       awaiting approval        │
│      ~/code/docs-v2 · llama-3.3-70b · 1 agent · 41m                         │
│ ✓  ● test-harness-migration  #77d3                             done         │
│      ~/code/acme-api · 64 steps · $2.41 · 2h ago                            │
│ ▣  ● sql-query-optimizer  #120e                                idle         │
│      ~/code/analytics · gpt-4.1 · idle · yesterday                          │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Header

- **Label.** `Sessions` — mono-xs uppercase, `var(--color-text-secondary)`.
- **Tab pair.** Two segments rendered as a single mono chip group inside `var(--color-surface-3)`: `active · <n>` (selected; `var(--color-surface-2)` background) and `archived · <n>`. Switches the body list. Singular collapses (`active · 1`); zero renders `active · 0` (the chip is always present).
- **Aggregate meta.** `<running> running · <idle> idle`, `var(--color-text-tertiary)`, mono. Suppressed when zero.
- **Header action.** `View all` — ghost button. Navigates to the full sessions view (Sidebar → `Sessions`).

### Row layout

Each session is one row inside the card body, no per-row card chrome. Row track: `auto 1fr auto` with `gap: var(--sp-3)`, vertical padding `var(--sp-2) var(--sp-4)`, and a `1px solid var(--color-border-1)` bottom border (suppressed on `:last-child`). Cells:

1. **Status icon.** A 16px glyph keyed to the session lifecycle — wrench (active editing), agent (sub-agent active), check (completed), sessions (idle). Color follows the lifecycle: ember for active, success for done, text-tertiary for idle, warning for awaiting approval.
2. **Identity stack.** Two stacked lines. Line one: provider dot (`p-dot` keyed to provider id), session name (medium weight, body), and the 4-char id in mono-xs `var(--color-text-tertiary)`. Line two: `<workspace-path> · <model> · <agent-count> agents · <last-event>` in mono-xs, `var(--color-text-tertiary)`.
3. **Status pill.** Right-aligned. Variants per `DESIGN.md §Status pill`: `streaming`, `awaiting approval`, `done`, `idle`.

### Click target

The full row is clickable — opens the session in the Session window (`open_session` Tauri command). Keyboard: each row is `tabindex={0}` and responds to `Enter` / `Space`.

### States

| State    | Trigger                              | Visual                                                                                | Verbatim copy                          |
| -------- | ------------------------------------ | ------------------------------------------------------------------------------------- | -------------------------------------- |
| loading  | `session_list` pending               | 4 row-shaped skeletons matching the row height (see [`dashboard.md §Loading skeletons`](#loading-skeletons)) | —                                      |
| empty    | `session_list` ok, zero sessions     | single full-row placeholder; `View all` disabled                                       | `No active sessions yet.`              |
| error    | `session_list` rejects               | `role="alert"` error block above the row list with verbatim error detail and a `RETRY` link; `View all` remains enabled | `Couldn't load sessions.`              |
| ready    | `session_list` ok, sessions > 0      | populated row list (anatomy above)                                                    | —                                      |

The error state is distinct from empty — a `session_list` rejection must not collapse to the empty placeholder. The verbatim error detail is preserved (per `voice-terminology.md §8` "show technical identifiers verbatim").

## Providers card

The `col-4` secondary card on the primary row. Mirrors the provider list from the Providers view in a condensed form — one row per provider, readiness pill on the right.

### Anatomy

```
┌─ PROVIDERS                                          [Manage] ─┐
│ ● Anthropic                                        ● ready     │
│   sonnet-4.5 · opus-4.1                                        │
│ ● OpenAI                                           ● auth      │
│   credentials expired                                          │
│ ● Ollama                                           ● ready     │
│   llama-3.3 · qwen-2.5                                         │
│ ● Mistral                                          ● ready     │
│   medium-2508                                                  │
└────────────────────────────────────────────────────────────────┘
```

### Header

- **Label.** `Providers` — mono-xs uppercase, `var(--color-text-secondary)`.
- **Header action.** `Manage` — ghost button. Navigates to the Providers route (see providers-section.md).

### Row layout

Each provider is one row. Track: `auto 1fr auto` with `gap: var(--sp-3)`, padding `var(--sp-3)`. Cells:

1. **Provider dot.** A 10px circle filled with the provider accent (`var(--p-anthropic)`, `var(--p-openai)`, `var(--p-local)`, `var(--p-custom)` — see `docs/design/ai-patterns.md`).
2. **Identity stack.** Two stacked lines. Line one: provider name (medium weight, body). Line two: model list or status detail in mono-xs, `var(--color-text-tertiary)` — e.g. `sonnet-4.5 · opus-4.1` or `credentials expired`.
3. **Status pill.** `ready` (success + live-dot glow) or `auth` (error + live-dot glow) per `DESIGN.md §Status pill`.

### States

| State    | Trigger                                | Visual                                                          | Verbatim copy                          |
| -------- | -------------------------------------- | --------------------------------------------------------------- | -------------------------------------- |
| loading  | `provider_status` pending              | 4 row-shaped skeletons sized to the row height                  | —                                      |
| empty    | `provider_status` ok, zero providers   | single full-row placeholder pointing at the Providers route     | `No providers configured.`             |
| error    | `provider_status` rejects              | `role="alert"` error block above the row list with verbatim error detail and a `RETRY` link | `Couldn't load providers.`             |
| ready    | `provider_status` ok, providers > 0    | populated row list (anatomy above)                              | —                                      |

Per-row provider failures (single provider unreachable) surface as the `auth` pill on that row, not as a card-level error.

## Usage card

The `col-6` left card on the secondary row. KPI tiles for the last range plus a 7-day spark chart — see `DESIGN.md §KPI tile` and `DESIGN.md §Spark chart`.

### Anatomy

```
┌─ USAGE · last 7 days                              [7D] 30D MTD ─┐
│  $18.42         4.2M                1.1M                         │
│  spend          tokens in           tokens out                   │
│  ↑ 24% vs last 7d   ↑ 18%           ↑ 31%                        │
│  ─────────────── 7-day spark line ─────────────────────────────  │
└──────────────────────────────────────────────────────────────────┘
```

### Header

- **Label.** `Usage · last 7 days` — mono-xs uppercase, the trailing range fragment lowercase. Range follows the selected tab.
- **Range tabs.** Three mono chips — `7D` (default), `30D`, `MTD`. Selected chip paints `var(--color-text-primary)` on `var(--color-surface-3)`; the rest paint `var(--color-text-tertiary)` with transparent background.

### Body

Three KPI tiles laid out as a `repeat(3, 1fr)` grid with `gap: var(--sp-4)`. Each tile uses the `DESIGN.md §KPI tile` primitive — label, large value with optional unit, delta line.

| Tile         | Format                | Delta semantic                              |
| ------------ | --------------------- | ------------------------------------------- |
| `spend`      | `$<value>`            | up → warning (more spend); down → success   |
| `tokens in`  | `<value><unit>`       | up → warning; down → success                |
| `tokens out` | `<value><unit>`       | up → success (more output); down → warning  |

Beneath the KPI grid: a single `DESIGN.md §Spark chart` filling the card width, painting the same range. Latest day point renders at `r="2.5"` with a 1px white stroke.

### States

| State    | Trigger                       | Visual                                                                   | Verbatim copy                          |
| -------- | ----------------------------- | ------------------------------------------------------------------------ | -------------------------------------- |
| loading  | usage query pending           | KPI grid renders 3 tile-shaped skeletons; spark renders 1 chart-shaped skeleton | —                                |
| empty    | usage query ok, zero events   | KPI grid renders zero-valued tiles (`$0.00`, `0`, `0`); delta lines hide; spark renders the empty-baseline path | `No usage recorded yet.`               |
| error    | usage query rejects           | `role="alert"` error block in place of the KPI grid with a `RETRY` link; spark suppresses | `Couldn't load usage.`                 |
| ready    | usage query ok, events > 0    | KPI tiles + spark per anatomy                                            | —                                      |

The empty state renders zero-valued tiles rather than a single placeholder — the structure is informative even when the numbers are flat.

## Enabled card

The `col-6` right card on the secondary row. Shows the workspace's enabled skills, MCP servers, and agents in a single mini-list with inline toggles.

### Anatomy

```
┌─ ENABLED · workspace                                  acme-api ─┐
│  ★ typescript-review       on    ●——                            │
│    skill · .forge/skills                                        │
│  ★ postgres-schemata       on    ●——                            │
│    skill · anthropic/official                                   │
│  ⚙ github                  on    ●——                            │
│    mcp · stdio · v0.9.2                                         │
│  ⚙ sentry                  off   ——●  connecting…               │
│    mcp · http                                                   │
│  ☉ refactor-bot            on    ●——                            │
│    agent · process-isolated                                     │
└─────────────────────────────────────────────────────────────────┘
```

### Header

- **Label.** `Enabled · workspace` — mono-xs uppercase, trailing fragment lowercase.
- **Workspace name.** Right-aligned, mono-xs, `var(--color-text-tertiary)` — e.g. `acme-api`.

### Row layout

Each entry is one row. Track: `auto 1fr auto auto` with `gap: var(--sp-3)`, padding `var(--sp-2) var(--sp-3)`. Cells:

1. **Kind icon.** A 14px glyph keyed to the entry type — skill, mcp, agent.
2. **Identity stack.** Two stacked lines. Line one: entry name (medium weight, body). Line two: `<kind> · <source>` (or `<kind> · <transport> · <state>` for MCP) in mono-xs, `var(--color-text-tertiary)`.
3. **State chip.** `on` (ember-tinted background) or `off` (text-tertiary on transparent). Mono-xxs.
4. **Toggle.** A `DESIGN.md §Toggle switch` — the visual indicator; the row is the hit target.

### States

| State    | Trigger                            | Visual                                                       | Verbatim copy                          |
| -------- | ---------------------------------- | ------------------------------------------------------------ | -------------------------------------- |
| loading  | enabled-list pending               | 5 row-shaped skeletons                                       | —                                      |
| empty    | enabled-list ok, zero entries      | single full-row placeholder pointing at the Catalog route    | `Nothing enabled in this workspace yet.` |
| error    | enabled-list rejects               | `role="alert"` error block above the row list with verbatim error detail and a `RETRY` link | `Couldn't load enabled extensions.`    |
| ready    | enabled-list ok, entries > 0       | populated row list (anatomy above)                           | —                                      |

A single-row failure (e.g. MCP server stuck in `connecting…`) does not raise the card-level error — it surfaces inline on that row as the trailing `<state>` fragment in the identity stack.

## Containers card

The `col-12` full-bleed card on the third row. The dashboard owns the placement; the card's anatomy, header meta, row layout, states, and action contract are defined in [`containers-section.md#container-card-on-dashboard`](./containers-section.md#container-card-on-dashboard).

The runtime banner (when the runtime is missing / broken / rootless-unavailable) anchors above the grid, between the hero and the primary row — see [`containers-section.md §First-run banner`](./containers-section.md#first-run-banner).

## Loading skeletons

Per `docs/design/ai-patterns.md §"Interaction states"`, every fetching surface paints a skeleton or the streaming cursor — never plain "loading…" text, never a spinner. The dashboard cards all surface multi-row / grid layouts, so they use skeletons (the streaming cursor is reserved for inline assistant output in `ChatPane`).

| Card        | Choice    | Shape                                                                                  |
| ----------- | --------- | -------------------------------------------------------------------------------------- |
| Sessions    | skeleton  | 4 row-shaped block placeholders matching the row height                                |
| Providers   | skeleton  | 4 row-shaped block placeholders                                                        |
| Usage       | skeleton  | 3 KPI-tile placeholders + 1 chart placeholder                                          |
| Enabled     | skeleton  | 5 row-shaped block placeholders                                                        |
| Containers  | skeleton  | per [`containers-section.md §States`](./containers-section.md) — 3 row-shaped placeholders |

The shared `<Skeleton>` primitive lives in `@forge/design` (`variant`: `block` / `text` / `card`; `count` for stacked rows). It carries `role="status"` + `aria-busy="true"` + `aria-live="polite"` so screen readers register the load without spamming on every paint, and respects `prefers-reduced-motion` by suppressing the shimmer.

## Live data

Card data sources subscribe to Tauri events to refresh in place — no polling timer at the page level:

| Card        | Query                  | Refresh trigger                                                                |
| ----------- | ---------------------- | ------------------------------------------------------------------------------ |
| Sessions    | `session_list`         | `SESSIONS_CHANGED_EVENT` (session open / archive / done)                        |
| Providers   | `provider_status`      | `PROVIDERS_CHANGED_EVENT` (credential update / reachability change)             |
| Usage       | `usage_summary`        | `USAGE_TICK_EVENT` (debounced 5s on streaming completion)                       |
| Enabled     | `workspace_enabled`    | `WORKSPACE_CONFIG_CHANGED_EVENT` (catalog toggle / config save)                 |
| Containers  | `container_list`       | `CONTAINERS_CHANGED_EVENT` ([`containers-section.md §Live data`](./containers-section.md#live-data)) |

## Keyboard

- Tab order follows document order: hero CTAs → Sessions header → Sessions rows → Providers header → Providers rows → Usage header → Enabled rows → Containers header → Containers rows.
- `Enter` / `Space` on a session row opens the session.
- `Enter` on `+ New session` opens the new-session flow (see new-session-flow.md when it lands).
- `Esc` from any focused row returns focus to the card header.

## Color & typography

- Card chrome: `background: var(--color-surface-1)`, `border: 1px solid var(--color-border-1)`, `border-radius: var(--r-lg)`.
- Card header label: mono-xs uppercase, `letter-spacing: 0.22em`, `var(--color-text-secondary)`.
- Card header meta: mono-xxs, `var(--color-text-tertiary)`.
- Row identity name: body font, weight 500, `var(--color-text-primary)`.
- Row identity detail: mono-xs, `var(--color-text-tertiary)`.
- Status pills: per `DESIGN.md §Status pill` — `streaming` (ember-200, animated dot), `awaiting approval` (warning, animated dot), `done` (success, static dot), `idle` (text-secondary, no dot), `ready` (success + glow), `auth` (error + glow).
- Provider dots: `var(--p-anthropic)`, `var(--p-openai)`, `var(--p-local)`, `var(--p-custom)` per `docs/design/ai-patterns.md`.

## Cross-spec references

- [`app-shell.md`](./app-shell.md) — the chrome the Dashboard mounts inside (Activity bar, Sidebar, Status bar).
- [`containers-section.md`](./containers-section.md) — owns the `col-12` Containers card anatomy and action contract; see [`#container-card-on-dashboard`](./containers-section.md#container-card-on-dashboard).
- [`providers-section.md`](./providers-section.md) — the full Providers route; the dashboard's Providers card links to it via the `Manage` header action. (See also providers-page.md, in flight.)
- [`session-roster.md`](./session-roster.md) — session-internal roster; the dashboard's Sessions card opens the session window where the roster lives.
- [`usage.md`](./usage.md) — the full Usage route; the dashboard's Usage card is a condensed surface that links into it.
- [`catalog.md`](./catalog.md) — the full Skills / MCP / Agents catalog; the dashboard's Enabled card is the workspace-scoped slice.
- [`provider-selector.md`](./provider-selector.md) — composer-time provider switching inside a session. The dashboard's Providers card is read-only status, not a switcher.
- [`../design/component-principles.md`](../design/component-principles.md) — interaction-state contract (loading / empty / error / ready) every card honors.
- [`../../DESIGN.md`](../../DESIGN.md) — `Layout grid`, `Hero block`, `KPI tile`, `Spark chart`, `Toggle switch`, `Status pill` primitives consumed by the cards above.
- new-session-flow.md — F-726 new-session entry flow triggered by the hero `+ New session` CTA. <!-- TODO: link once new-session-flow.md lands -->

## Doesn't do

- Does not author skills, MCP servers, or agents — that surface lives in the Catalog route ([`catalog.md`](./catalog.md)). The Enabled card is a workspace-scoped toggle surface, not an authoring surface.
- Does not create workspaces — workspace creation is a Settings-time concern; the dashboard reflects the active workspace's state.
- Does not configure providers — provider config lives in `~/.config/forge/providers.toml` and the Providers route; the Providers card only reflects status.
- Does not host the session canvas — clicking a session row opens the Session window ([`shell.md`](./shell.md)); the dashboard never embeds the chat pane.
- Does not surface non-Level-2 sandboxes (cgroup-only Level-1) in the Containers card. Those are session-internal — see [`containers-section.md`](./containers-section.md).
