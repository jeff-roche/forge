# Catalog

> Top-level pane ([F-592](https://github.com/forge-ide/forge/issues/610)) — three tabs (Skills / MCP / Agents) over the F-591 `list_*` IPC, with a shared search and per-row enable / disable persistence.

---

## Purpose

Give the user one surface to inspect every roster of installable assets — skills, MCP servers, and agent definitions — across user and workspace scopes, search across them, and toggle individual entries on or off. Toggles persist via `set_setting` under the `catalog.enabled.<kind>.<id>` keyspace (default `true`).

## Where

`<CatalogPane>` mounts as the Catalog window's root. Component path: `web/packages/app/src/components/catalog/CatalogPane.tsx`. The Catalog window opens from the app menu / Cmd-Shift-K (window-level wiring lives in `web/packages/app/src/routes/`).

## Size

Fills the catalog window. Single column — header (title + search) on top, tab strip, then a tab panel that scrolls vertically.

## Structure

```
┌─ Catalog ─────────────────────────────────────────[ filter… ]┐
├──────────────────────────────────────────────────────────────┤
│ [ Skills (4) ]  [ MCP (2) ]  [ Agents (3) ]                  │
├──────────────────────────────────────────────────────────────┤
│ Session-wide                                                 │
│   tdd                                            [ ✓ enabled]│
│   forge-finish-task                              [ ✓ enabled]│
│ Agent · planner                                              │
│   plan-driven-development                        [   disabled]│
└──────────────────────────────────────────────────────────────┘
```

### Header

- Title: `Catalog` (display font).
- Search: `<input type="search">` filtering across the active tab. Placeholder: `Filter skills, MCP, agents…`. Aria-label: `Filter catalog entries`.

### Tabs

Three tabs. Each carries a count badge that reflects the *post-filter* row count for that kind, so the search box's effect on every tab is visible without flipping through them. Tab IDs: `catalog-tab-{skills,mcp,agents}`. Panels: `catalog-panel-{kind}`.

### Tab panel

Rows are grouped by their `scope`: `Session-wide`, `Agent · <id>`, or `Provider · <id>`. Group label is rendered as an `<h3>`. Each row carries:

- **Body:** `name` (the roster id) + an optional `meta` line (provider's `model`, agent's `background` / `foreground`, empty string for skills / MCP).
- **Toggle:** `role="switch"` `<input type="checkbox">` with the `enabled` / `disabled` text label adjacent.

## Live data

Each kind owns its own `createResource`, so a slow / failing skill loader does not block the MCP and Agents tabs from rendering. The active-tab badge counts re-derive from the filtered row list.

## States

Per tab, four states distinct (and the per-tab error never collapses into the per-tab empty placeholder):

- **Loading.** Skeleton placeholder per [`dashboard.md §D.5.1`](./dashboard.md#d51-loading-state-primitives-f-684) — four `block`-variant row-shaped skeletons inside the active tab. Loading is per-tab — switching tabs while one is mid-load doesn't restart the others.
- **Empty (zero entries across all scopes).** Per-kind copy:
  - Skills: title `No skills installed`, hint `Drop a SKILL.md under .skills/<name>/ in your workspace or ~/.skills/.`
  - MCP: title `No MCP servers configured`, hint `Add a server entry to .mcp.json in your workspace or ~/.mcp.json.`
  - Agents: title `No agents defined`, hint `Add a definition under .agents/<name>.md in your workspace or ~/.agents/.`
- **Empty (search miss).** When the kind has rows but the current filter excludes them all: title `No matches`, hint `Nothing in <kind label> matches "<query>".`
- **Error.** A `role="alert"` block with heading `<KIND LABEL> UNAVAILABLE` and the verbatim error detail. Distinct from empty — a `list_*` rejection must never collapse into the no-entries placeholder. (Reading `resource()` in the `errored` state re-throws inside Solid's reactive scope — the panel gates on `resource.state` to surface the error cleanly.)
- **Ready.** The grouped row list above.

A separate `role="alert"` `catalog__action-error` line surfaces above the panel when a `setSetting` toggle call rejects; it carries the `set_setting failed: <detail>` prefix so the user can tell "I can't read the catalog" from "I tried to toggle and it failed".

## Copy

- Window title: `Catalog`
- Search placeholder: `Filter skills, MCP, agents…`
- Tab labels: `Skills`, `MCP`, `Agents` (badge: integer count).
- Toggle labels: `enabled` / `disabled` (lowercase — switch state, not button verb).
- Toggle aria-label: `Enable <name>` / `Disable <name>` (the inverse of the current state — what the click *will* do).
- Group labels: `Session-wide` / `Agent · <id>` / `Provider · <id>`.
- Action-error prefix: `set_setting failed: <detail>`.
- Empty + search-miss copy: see States.

## Color & typography

- Title: `--font-display`.
- Group labels (`<h3>`): `--font-display`, `--color-text-secondary`.
- Row name: `--font-body`, `--color-text-primary`. Meta: `--font-mono`, `--color-text-tertiary`.
- Search input: matches the standard `@forge/design` input — `--color-surface-2`, 1px `--color-border-1`, focus border `--color-ember-400`.

## Keyboard

- Tab — moves through: search input → tab strip → first row → row toggle → next row.
- Inside the tab strip: Arrow Left / Right cycles tabs (handled by `@forge/design` `Tabs` primitive).
- Inside a row: Space toggles the switch; Enter on the row body is inert (toggling is the only row affordance).

## Cross-spec references

- [`dashboard.md`](./dashboard.md) — neighbouring root surface; catalog opens from the dashboard menu.
- [`dashboard.md §D.5.1`](./dashboard.md#d51-loading-state-primitives-f-684) — skeleton primitive contract (F-684).
- [`providers-section.md`](./providers-section.md) — provider discovery lives there; the catalog deliberately does *not* expose providers as a top-level tab.
- [`context-picker.md`](./context-picker.md) — composer-time `@`-picker that filters its results by these `catalog.enabled.*` flags.
- `docs/frontend/architecture.md §9.2` — the `settings` store and the `setSetting` IPC.

## Doesn't do

- Does not install or remove assets — the catalog is a discovery + toggle surface. Adding a skill / MCP server / agent is a filesystem edit; the catalog refreshes on the next mount.
- Does not surface providers as a top-level tab. Provider discovery is the Dashboard's `<ProvidersSection>` (per [F-586](https://github.com/forge-ide/forge/issues/604)). Catalog rows still attach to a `Provider` scope when the asset is provider-scoped, but the *primary* axis is the three asset kinds.
- Does not preview a skill's body or an agent's definition. That's a future "details" pane; v1 keeps the surface scannable.
- Does not let the user edit `.mcp.json` inline. MCP edits stay in the user's editor of choice.
