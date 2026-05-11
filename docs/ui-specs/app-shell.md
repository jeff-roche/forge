# App Shell

> Unified chrome ([F-709](https://github.com/forge-ide/forge/issues/709)) — the activity bar, primary-nav sidebar, and status bar that V1 mounts on every route. [F-716](https://github.com/forge-ide/forge/issues/716) extracts the shell from the Session window so Dashboard, Catalog, Usage, and Providers receive it too.

---

## Purpose

A single chrome surface that survives route changes. Today the shell elements (`ActivityBar`, primary nav, `StatusBar`) only mount inside the Session window — every other route renders bare. F-716 will lift these into a route-agnostic layout component; this spec is the contract that work implements.

The shell owns the three persistent surfaces that frame every route: the **Activity bar** (vertical route switcher), the **Sidebar** (primary navigation with count badges), and the **Status bar** (workspace breadcrumb on the left, provider / streaming / runtime on the right). Route content slots between Sidebar and Status bar.

## Where

The shell wraps every top-level route — Dashboard, Session, Catalog (Skills / MCP servers / Agents), Usage, Providers, Settings. It mounts above the router outlet so navigating between routes never unmounts the chrome. Component paths after F-716:

- `web/packages/app/src/shell/AppShell.tsx` — new root layout (F-716).
- `web/packages/app/src/shell/ActivityBar.tsx` — existing, lifted out of `SessionWindow`.
- `web/packages/app/src/shell/StatusBar.tsx` — existing, lifted out of `SessionWindow`.
- `web/packages/app/src/shell/Sidebar.tsx` — new primary-nav sidebar (F-716).

> **Not** `web/packages/app/src/shell/FilesSidebar.tsx`. That component is the **workspace file tree** ([`shell.md §2`](./shell.md#2-session-window-shell)), shown only inside the Session route in the activity-bar-driven sidebar slot. The Sidebar described in this spec is a different surface — fixed primary navigation, always mounted, identical on every route. Sessions render *both*: the primary-nav Sidebar at 240px, then the FilesSidebar inside the session viewport.

## Size

```
┌─ 44 ─┬─ 240 ─┬─────── route content (flex) ──────────┐
│      │       │                                       │
│  AB  │  SB   │              <Outlet/>                │
│      │       │                                       │
├──────┴───────┴───────────────────────────────────────┤
│ status bar — 22                                      │
└──────────────────────────────────────────────────────┘
```

- Activity bar: `44px` fixed width, full viewport height minus the status bar.
- Sidebar: `240px` fixed width, full viewport height minus the status bar.
- Status bar: `22px` fixed height, spans the full viewport width — the brand-exception surface ([`component-principles.md §Status bar`](../design/component-principles.md#status-bar)).

---

## Activity bar

A vertical strip of icon buttons that switch the top-level route. Implementation lives at `web/packages/app/src/shell/ActivityBar.tsx`.

### Anatomy

```
┌──┐
│⌂ │  Dashboard       ← selected (background + ember accent rail)
│▣ │  Session
│★ │  Catalog
│∑ │  Usage
│◆ │  Providers
│  │
│  │  (spacer)
│  │
│⚙ │  Settings        ← pinned to the bottom
└──┘
```

Each item is a 28×28 `IconButton` centered in the 44px track. The selected item paints `background: var(--color-surface-2)` and renders a 2px `var(--color-ember-400)` accent rail on its left edge (`::before`), matching the `.activity-bar button.active` pattern in `docs/forge-mocks.html`. Hover paints `var(--color-surface-3)`. Disabled items keep visual chrome but no hover; never reduce opacity to signal disabled per [`component-principles.md §Buttons`](../design/component-principles.md#buttons).

### Items

Top stack — route switchers in order:

1. **Dashboard** → `/`
2. **Session** → `/session/<id>` (disabled when no active session)
3. **Catalog** → `/catalog` (Skills / MCP servers / Agents tabs)
4. **Usage** → `/usage`
5. **Providers** → `/providers`

Bottom (pinned via `margin-top: auto`):

6. **Settings** → `/settings`

Tooltip format: `<Label> (<shortcut>)` — e.g. `Dashboard (Cmd/Ctrl+Shift+D)`. Shortcuts land with F-716; until then the label alone surfaces.

### States

- **Loading.** Not applicable. The activity bar is route metadata, not data — it renders instantly from the static `ACTIVITIES` list.
- **Empty.** Not applicable. The list is fixed at six items.
- **Error.** Not applicable. Navigation failures are surfaced by the route content, not by re-coloring the bar.
- **Ready.** The icon stack above. Selected item paints with the accent rail; the rest render in `var(--color-text-secondary)`.

### Tokens

- Track: `width: 44px`, `background: var(--color-surface-1)`, `border-right: 1px solid var(--color-border-1)`.
- Item: `width: 28px`, `height: 28px`, `color: var(--color-text-secondary)` (default) / `var(--color-text-primary)` (active).
- Accent rail (active): `2px` × full item height, `background: var(--color-ember-400)`.
- Icon stroke: `1.7px`, `viewBox="0 0 24 24"`, `stroke="currentColor"` so theme color cascades.

---

## Sidebar

A 240px fixed primary-nav rail. Distinct from `FilesSidebar.tsx` (see callout in §Where). Implementation lands as `web/packages/app/src/shell/Sidebar.tsx` in F-716.

### Anatomy

```
┌────────────────────────┐
│ ▲ Forge                │  ← brand block
│   any ai. one editor.  │
├────────────────────────┤
│ WORKSPACE              │  ← group label (mono-xs)
│ ▣ Sessions          2  │  ← active row (ember rail + surface-2)
│ ⌒ Recent            7  │
│ ⎇ Git               6  │
│                        │
│ AI                     │
│ ◆ Providers         4  │
│ ★ Skills           12  │
│ ⚙ MCP servers       6  │
│ ☉ Agents            9  │
│                        │
│ SYSTEM                 │
│ ⎈ Containers        3  │
│ ∑ Usage                │
│ ⚙ Settings             │
└────────────────────────┘
```

### Nav contract

Order is fixed. Every group label is mono-xs uppercase `var(--color-text-secondary)` per [`component-principles.md §Inputs`](../design/component-principles.md#inputs) labels. Each row is a `<a>` with `aria-current="page"` when its route matches.

| Group | Item | Route | Badge source |
|---|---|---|---|
| Workspace | **Sessions** | `/` (sessions tab) | active session count |
| Workspace | **Recent** | `/recent` | last-7-day session count |
| Workspace | **Git** | `/git` | changed-file count for active workspace |
| AI | **Providers** | `/providers` | configured provider count |
| AI | **Skills** | `/catalog/skills` | enabled-for-workspace count |
| AI | **MCP servers** | `/catalog/mcp` | enabled-for-workspace count |
| AI | **Agents** | `/catalog/agents` | enabled-for-workspace count |
| System | **Containers** | `/containers` | active sandbox count ([`containers-section.md`](./containers-section.md)) |
| System | **Usage** | `/usage` | — |
| System | **Settings** | `/settings` | — |

Count badges render as `<span class="sidebar__count">` to the right of the row, `font-family: var(--font-mono)`, `font-size: var(--type-mono-xxs)`, `color: var(--color-text-tertiary)`. Zero-count rows render the row but suppress the badge — not `0`.

### States

- **Loading.** Brand block + group labels + nav rows paint immediately; badges render `—` (em dash, `var(--color-text-tertiary)`) until their data source resolves. Rows remain clickable during this window — the route they navigate to owns its own loading state.
- **Empty.** Not applicable as a sidebar-level state. Individual count badges suppress when the count is zero (see above). The nav contract itself is fixed and never empty.
- **Error.** Per-badge fail-silent: if a badge's data source rejects, the badge renders `—` and the route content surfaces the error. The sidebar never paints `role="alert"` — chrome failures must not steal focus from the route.
- **Ready.** Rows render with their counts; the active row paints `background: var(--color-surface-2)` and a 2px `var(--color-ember-400)` left rail (matching the activity bar's selected treatment).

### Verbatim copy

- Brand wordmark: `Forge`
- Brand tagline: `any ai. one editor.`
- Group labels: `WORKSPACE`, `AI`, `SYSTEM`
- Row labels (in order): `Sessions`, `Recent`, `Git`, `Providers`, `Skills`, `MCP servers`, `Agents`, `Containers`, `Usage`, `Settings`
- Zero-state badge: row renders with no badge element (not `0`, not `—` — em dash is reserved for loading)
- Loading badge: `—`

### Tokens

- Track: `width: 240px`, `background: var(--color-surface-1)`, `border-right: 1px solid var(--color-border-1)`.
- Row: `padding: var(--sp-2) var(--sp-3)`, `gap: var(--sp-2)`, `color: var(--color-text-secondary)`.
- Row hover: `background: var(--color-surface-2)`, `color: var(--color-text-primary)`.
- Row active: `background: var(--color-surface-2)`, 2px `var(--color-ember-400)` left rail via `::before`.
- Group label: `font-family: var(--font-mono)`, `font-size: var(--type-mono-xs)`, `letter-spacing: 0.2em`, `color: var(--color-text-secondary)`, `padding: var(--sp-3) var(--sp-3) var(--sp-1)`.
- Count badge: `font-family: var(--font-mono)`, `font-size: var(--type-mono-xxs)`, `color: var(--color-text-tertiary)`, `margin-left: auto`.

---

## Status bar

The 22px Ember 400 strip pinned to the bottom of every route. Implementation lives at `web/packages/app/src/shell/StatusBar.tsx`. The brand-exception surface — never re-colored, never themed ([`component-principles.md §Status bar`](../design/component-principles.md#status-bar)).

### Anatomy

```
┌──────────────────────────────────────────────────────────────────────┐
│ ▲ forge · ~/code/acme-api · session #a3f1 · anthropic · sonnet-4.5 · │
│                              streaming ●     $0.23 · 60.3k tok · 7d  │
│                                                       · podman up   │
└──────────────────────────────────────────────────────────────────────┘
   └──── left slot ────┘                         └──── right slot ────┘
```

### Slots

- **Left slot** — workspace breadcrumb. Forge mark, then the route-appropriate path:
  - Dashboard: `forge · <workspace-name> · <branch>`
  - Session: `forge · <session-name> · <workspace-path> · #<8-char id>`
  - Catalog: `forge · catalog · <section>` (skills / mcp / agents)
  - Usage: `forge · usage · <range>`
  - Providers: `forge · providers`
  - Settings: `forge · settings · <section>`
- **Right slot** — provider pill, streaming pill, runtime stats. `margin-left: auto`. Segments:
  - Provider pill: `<provider> · <model>` (e.g. `anthropic · sonnet-4.5`)
  - Streaming pill: `streaming` + 6px live dot, or `idle` (no dot), or `awaiting approval`
  - Runtime stats: `$<spend> · <tokens> tok · <range>` (Dashboard / Usage) or `ln <N> · col <N> · <lang> · lsp <state>` (Session editor focus)
  - Sandbox: `sandbox: <kind>` (Session only)

Adjacent groups are separated by a literal `·` at `opacity: 0.5`. Group-internal items use `gap: var(--sp-2)`.

### States

- **Loading.** First mount paints the slots with placeholder copy: left renders `forge · …`; right renders `idle` and suppresses the provider pill until the provider store resolves. No skeleton primitives — the strip is mono text, not a card.
- **Empty.** Not applicable. The status bar always has at least the Forge mark and route label in the left slot.
- **Error.** A provider-side failure (auth expired, runtime down) paints the streaming pill as `auth` (`var(--color-error)`, error live-dot glow per the [`status pill`](../design/component-principles.md#layout-primitives) `auth` variant). Background-agent IPC failures are logged to `console.error` and do not surface on the bar — chrome must not steal focus from the route's own error state.
- **Ready.** All slots populated as in the anatomy above. The streaming pill animates its dot when the variant is live (`streaming`, `awaiting approval`). The background-agent badge appears in the right slot when count > 0, rendered on a `var(--color-surface-2)` chip to clear WCAG AA independently of the Ember background.

### Verbatim copy

- Forge mark: rendered via `<svg><use href="#forge-mark"/></svg>` — no text alternative needed (the wordmark `forge` follows immediately).
- Loading left slot: `forge · …`
- Idle streaming pill: `idle`
- Streaming pill: `streaming`
- Awaiting-approval pill: `awaiting approval`
- Auth-error pill: `auth`
- Background-agent badge: `<count> bg`
- Empty-provider state (no provider configured): omit the provider pill entirely — do not render `no provider`.

### Tokens

Per [`DESIGN.md §Layout primitives → Status bar`](../../DESIGN.md#layout-primitives):

- Container: `height: 22px`, `flex-shrink: 0`, `display: flex`, `align-items: center`, `padding: 0 var(--sp-3)`, `gap: var(--sp-4)`, `background: var(--color-ember-400)`, `color: var(--color-text-inverted)`.
- Typography: `font-family: var(--font-mono)`, `font-size: var(--type-mono-xxs)`, `letter-spacing: 0.05em`.
- Separator: literal `·` at `opacity: 0.5`.
- Live dot: `6px × 6px`, `background: var(--color-text-inverted)`, `opacity: 0.9`. Never glows on this surface.
- Agent-badge chip: `background: var(--color-surface-2)`, `border-radius: 10px` (pill), `padding: 0 var(--sp-2)` — clears WCAG AA against the Ember background.

### Brand-exception

The Ember 400 × white pairing computes to ~3.35:1 — below WCAG AA 4.5:1 for normal text. This is an accepted exception, pinned by `web/packages/app/src/shell/StatusBar.css.test.ts`. Interactive controls (the background-agent badge) render on an iron chip so they clear AA independently. Do not re-color the bar under any theme.

---

## Doesn't do

- Does not host route content. The shell is chrome; the router outlet between Sidebar and StatusBar owns content.
- Does not mount the workspace file tree. `FilesSidebar` is session-internal and lives inside the Session route, not in this spec.
- Does not surface route-specific banners (e.g. the container-runtime banner). Those belong to the route — see [`containers-section.md §First-run banner`](./containers-section.md#first-run-banner).
- Does not animate route transitions. Chrome stays static; only the outlet swaps.

## Cross-spec references

- [`dashboard.md`](./dashboard.md) — Dashboard route content that mounts inside this shell.
- [`shell.md`](./shell.md) — Session-window chrome (title bar, files sidebar, tab bar). The Session route extends this app shell with its own internal layout.
- [`containers-section.md`](./containers-section.md) — Containers route content; its sidebar nav badge surfaces the active sandbox count.
- [`usage.md`](./usage.md) — Usage route content.
- [`providers-section.md`](./providers-section.md) — Providers route content.
- [`catalog.md`](./catalog.md) — Catalog route (Skills / MCP servers / Agents tabs).
- [`session-roster.md`](./session-roster.md) — Sessions row used inside the Dashboard's primary card and reachable from the Sidebar's `Sessions` row.
- [`../design/component-principles.md`](../design/component-principles.md) — interaction-state contract (loading / empty / error / ready) and the status-bar brand exception.
- [`../../DESIGN.md#layout-primitives`](../../DESIGN.md#layout-primitives) — token bindings for the status bar and other V1 primitives.
