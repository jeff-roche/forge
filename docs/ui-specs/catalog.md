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
- Search: `<input type="search">` filtering across the active tab. Placeholder: `Filter skills, MCP, agents…`. Aria-label: `Filter catalog entries`. See [§Search and filters](#search-and-filters).
- On the MCP tab the header also carries a primary `+ Add server` button that opens the modal documented in [§Add MCP server](#add-mcp-server). The other two tabs omit it — skills and agents are filesystem-only.

### Tabs

Three tabs. Each carries a count badge that reflects the *post-filter* row count for that kind, so the search box's effect on every tab is visible without flipping through them. Tab IDs: `catalog-tab-{skills,mcp,agents}`. Panels: `catalog-panel-{kind}`.

### Tab panel

Rows are grouped by their `scope`: `Session-wide`, `Agent · <id>`, or `Provider · <id>`. Group label is rendered as an `<h3>`. Each row carries:

- **Body:** `name` (the roster id) + an optional `meta` line (provider's `model`, agent's `background` / `foreground`, MCP server's `kind` + transport detail).
- **Toggle:** the inline switch documented in [§Enablement toggles](#enablement-toggles).

## Live data

Each kind owns its own `createResource`, so a slow / failing skill loader does not block the MCP and Agents tabs from rendering. The active-tab badge counts re-derive from the filtered row list.

## Search and filters

A single search box plus a strip of filter chips sits below the header on every tab. Both feed the same client-side predicate; the predicate runs against the in-memory roster — no IPC roundtrip per keystroke.

- **Search input:** `<input type="search">`, placeholder reflects the active tab (`Search <N> <kind>…`, e.g. `Search 9 MCP servers…`). Matches case-insensitively on `name` and on the row's `meta` line.
- **Filter chips:** rendered as `role="radiogroup"` with `aria-label="Catalog filters"`. Each chip is `role="radio"`. Selecting one clears the others — chips are *not* additive (single-axis filter; `Enabled` and `Workspace` are mutually-exclusive intents, not stackable).
- **Chip set:**

  | Chip        | Tabs           | Predicate                                                 |
  |-------------|----------------|-----------------------------------------------------------|
  | `All`       | all            | `true` (default)                                          |
  | `Enabled`   | all            | `catalog.enabled.<kind>.<id> !== false`                   |
  | `Workspace` | all            | `row.scope.tier === "workspace"`                          |
  | `User`      | all            | `row.scope.tier === "user"`                               |
  | `stdio`     | MCP only       | `row.kind === "stdio"`                                    |
  | `http`      | MCP only       | `row.kind === "http"`                                     |

  The `stdio` / `http` chips are rendered only when the MCP tab is active; switching off MCP collapses them out and resets the chip selection to `All` if either was active.

- **Empty + search-miss:** when the active chip + search query exclude every row but the underlying roster is non-empty, the panel renders the search-miss empty state (see [§States](#states)) with the verbatim message `No matches`. The chip selection is *not* cleared automatically — the user can see which filter zeroed the list.

## Add MCP server

A modal launched from the `+ Add server` button on the MCP tab header. Captures the transport-discriminated shape that lands in `.mcp.json`. The modal is the only writer to that file from inside Forge — skills and agents stay filesystem-only.

### Form structure

```
┌─ Add MCP server ──────────────────────────────────────┐
│ Name           [______________________]               │
│ Scope          ( ) Workspace   ( ) User               │
│ Transport      ( ) stdio       ( ) http               │
│                                                       │
│ ── if stdio ──────────────────────────────────────────│
│ Command        [______________________]               │
│ Arguments      [______________________]  + add arg    │
│                                                       │
│ ── if http ───────────────────────────────────────────│
│ URL            [https://_____________]                │
│ Headers        [name] [value]            + add header │
│                                                       │
│ Credential     [••••••••••••••••••••]  (write-only)   │
│                                                       │
│                              [ CANCEL ]  [ ADD ]      │
└───────────────────────────────────────────────────────┘
```

Renders as a focus-trapped `role="dialog" aria-modal="true"` with heading `Add MCP server`. `Escape` cancels; the focus trap matches the [containers logs flyout pattern](./containers-section.md#logs-flyout).

### Fields

| Field        | Type                                       | Required             | Notes                                                                                                          |
|--------------|--------------------------------------------|----------------------|----------------------------------------------------------------------------------------------------------------|
| `name`       | text                                       | yes                  | Unique within the chosen scope. Matches `^[a-z0-9][a-z0-9_-]*$`.                                              |
| `scope`      | radio (`workspace` / `user`)               | yes                  | Determines target file (`<workspace>/.mcp.json` or `~/.mcp.json`).                                            |
| `kind`       | radio (`stdio` / `http`)                   | yes                  | Discriminates the `ServerKind` variant per `crates/forge-mcp/src/lib.rs`.                                     |
| `command`    | text                                       | yes when `stdio`     | Executable path or command name. Required-not-empty.                                                          |
| `args`       | repeatable text                            | no                   | One per row; empty rows are dropped before submit.                                                            |
| `url`        | text                                       | yes when `http`      | Validated as a URL with `http` or `https` scheme.                                                             |
| `headers`    | repeatable `(name, value)`                 | no                   | Header `name` must match `^[A-Za-z0-9_-]+$`. Duplicate names are rejected client-side.                        |
| `credential` | password                                   | no                   | Write-only. Persisted via the credential store, not into `.mcp.json`. Never read back into the form on edit. |

### Validation

Schema validation mirrors the agentskills.io / universal MCP loader at `crates/forge-mcp/src/lib.rs` (`McpServerSpec::try_from` via `build_server_kind`, `StrictFields::Reject`):

- `stdio` rejects any of `url`, `headers`.
- `http` rejects any of `command`, `args`, `env`.
- Unknown top-level keys are rejected (`#[serde(deny_unknown_fields)]`).

Client-side validation fires on submit and per-field on blur; the same shape is re-validated in the daemon. Errors render inline under the offending field, using the input error border token (`--color-ember-400`, per [`component-principles.md §Inputs`](../design/component-principles.md#inputs)).

### IPC contract

The modal is the only caller of the `add_mcp_server` Tauri command (introduced by F-734).

```
Input  ← {
  kind:       "stdio" | "http",
  name:       string,
  scope:      "workspace" | "user",
  command?:   string,      // stdio only
  args?:      string[],    // stdio only
  url?:       string,      // http only
  headers?:   { [name: string]: string },  // http only
  credential?: string,
}

Output → { id: string }      // verbatim daemon-assigned id on success
Error  → string              // verbatim error detail, per the F-673 IPC convention
                             //   (see crates/forge-shell/src/ipc.rs)
```

Daemon-side guarantees:

- `name` is unique within `scope` (re-checked under a lock; the modal's pre-flight is advisory).
- `credential`, if present, is stored via the credential store keyed by the assigned `id`; it never lands in `.mcp.json` and is never returned on subsequent `list_mcp` reads.
- A rejection surfaces as a `role="alert"` line inside the modal carrying `add_mcp_server failed: <detail>`. The modal stays open so the user can fix the input.

On success the modal closes, the MCP-tab `createResource` refetches, and the toast `MCP server "<name>" added` (info, 5 s auto-dismiss) confirms.

## Enablement toggles

Every row on every tab carries an inline toggle. The toggle is a [DESIGN.md §Toggle switch](../../DESIGN.md#toggle-switch) primitive — a 28×16 track with a 12px thumb on `--color-ember-400` (on) / `--color-text-tertiary` (off).

- **Wire:** each toggle calls `set_setting` with a key shaped `catalog.enabled.<kind>.<id>`:
  - `catalog.enabled.skills.<id>` for skills,
  - `catalog.enabled.mcp.<id>` for MCP servers,
  - `catalog.enabled.agents.<id>` for agents.

  The value is a boolean; absent keys default to `true`. Scope is settled by `set_setting`'s own `level` argument (`workspace` for workspace-scoped rosters, `user` otherwise) — the catalog inherits the row's `scope.tier`. This is the same IPC F-735 wires.

- **Semantics:** `role="switch" aria-checked="true|false"`. Click and Space both toggle. Hit target is the parent row (per the design primitive), not just the 28px track. The aria-label inverts the current state — `Enable <name>` when off, `Disable <name>` when on — to describe what the click *will* do.
- **Failure:** if `set_setting` rejects, the toggle reverts to its pre-click position and the section-level `catalog__action-error` line surfaces with the prefix `set_setting failed: <detail>` (verbatim daemon detail).
- **Cross-surface effect:** the `@`-picker and any roster consumer reads these same keys to filter; toggling here affects every downstream consumer immediately. See [`context-picker.md`](./context-picker.md).

## States

Every interactive surface here renders all four `docs/design/component-principles.md` states distinctly. Per-tab error never collapses into the per-tab empty placeholder.

### Tab panel

| State                    | Render                                                                                                                                                                                                          |
|--------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **Loading**              | Four `block`-variant row-shaped skeletons per [`dashboard.md §D.5.1`](./dashboard.md#d51-loading-state-primitives-f-684). Per-tab — switching tabs while one is mid-load doesn't restart the others.            |
| **Empty (zero rows)**    | Per-kind copy. Skills: `No skills installed` / `Drop a SKILL.md under .agent-skills/<name>/ in your workspace or ~/.agent-skills/.` · MCP: `No MCP servers configured` / `Add a server entry to .mcp.json in your workspace or ~/.mcp.json, or use + Add server.` · Agents: `No agents defined` / `Add a definition under .agents/<name>.md in your workspace or ~/.agents/.` |
| **Empty (filter miss)**  | Title `No matches`, hint `Nothing in <kind label> matches "<query>"` when a search query is set, or `No <kind label> match the "<chip>" filter.` when only a chip is active. Chip selection stays as the user left it. |
| **Error**                | `role="alert"` block, heading `<KIND LABEL> UNAVAILABLE`, verbatim daemon detail. Reading `resource()` in the `errored` state re-throws inside Solid's reactive scope — the panel gates on `resource.state` to surface the error cleanly. |
| **Ready**                | The grouped row list documented in [§Tab panel](#tab-panel).                                                                                                                                                    |

A separate `role="alert"` `catalog__action-error` line surfaces above the panel when a `set_setting` toggle call rejects; it carries the `set_setting failed: <detail>` prefix so the user can tell "I can't read the catalog" from "I tried to toggle and it failed".

### Add MCP server modal

| State        | Render                                                                                                                                                                                       |
|--------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **Idle**     | Empty form. Default scope = the currently-active scope tab (workspace if a workspace is open, else user). Default transport = `stdio`. Primary `ADD` button enabled iff required fields filled. |
| **Submitting** | `ADD` button shows `Adding…` and is disabled. All inputs are disabled. No skeleton — the modal is itself the loading affordance.                                                            |
| **Error**    | `role="alert"` inline line above the action row, carrying `add_mcp_server failed: <detail>`. The form re-enables; focus moves to the alert. Field-level validation errors render under the offending input with the input error border (`--color-ember-400`). |
| **Success**  | Modal closes; MCP tab refetches; toast `MCP server "<name>" added` (info variant, 5 s auto-dismiss) appears.                                                                                  |

## Copy

- Window title: `Catalog`
- Search placeholder: `Filter skills, MCP, agents…` (per-tab variant: `Search <N> <kind>…`).
- Tab labels: `Skills`, `MCP`, `Agents` (badge: integer count).
- Filter chips: `All`, `Enabled`, `Workspace`, `User`, `stdio`, `http` (last two: MCP only).
- Toggle aria-label: `Enable <name>` / `Disable <name>` (the inverse of the current state — what the click *will* do).
- Group labels: `Session-wide` / `Agent · <id>` / `Provider · <id>`.
- Add-server button: `+ Add server` (MCP tab only).
- Modal title: `Add MCP server`. Modal buttons: `ADD` / `CANCEL`. Submitting label: `Adding…`. Success toast: `MCP server "<name>" added`.
- Action-error prefixes: `set_setting failed: <detail>`, `add_mcp_server failed: <detail>`.
- Empty + filter-miss copy: see [§States](#states).

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
- [`containers-section.md §Logs flyout`](./containers-section.md#logs-flyout) — focus-trapped modal pattern reused by the add-MCP-server modal.
- [`component-principles.md §Inputs`](../design/component-principles.md#inputs) — input borders + error border token used by the add-MCP form.
- [`DESIGN.md §Toggle switch`](../../DESIGN.md#toggle-switch) — the primitive every catalog row toggle renders as.
- `crates/forge-mcp/src/lib.rs` — `McpServerSpec` / `ServerKind` (`Stdio` / `Http`) — authoritative shape mirrored by the add-MCP form.
- `crates/forge-agents/src/skill_loader.rs` — agentskills.io reference loader.
- `crates/forge-shell/src/ipc.rs` — the `set_setting` command and the F-673 verbatim-error IPC convention.
- `docs/frontend/architecture.md §9.2` — the `settings` store and the `setSetting` IPC.

## Doesn't do

- Does not install or remove skills or agents. Those are filesystem-only — the catalog refreshes on the next mount. MCP servers *are* writable from inside the catalog via [§Add MCP server](#add-mcp-server); the inverse (delete) stays a filesystem edit until a future revision.
- Does not surface providers as a top-level tab. Provider discovery is the Dashboard's `<ProvidersSection>` (per [F-586](https://github.com/forge-ide/forge/issues/604)). Catalog rows still attach to a `Provider` scope when the asset is provider-scoped, but the *primary* axis is the three asset kinds.
- Does not preview a skill's body or an agent's definition. That's a future "details" pane; v1 keeps the surface scannable.
- Does not let the user *edit* an existing MCP server inline. Edits stay in the user's editor of choice; the modal is add-only in v1.
- Does not stack filter chips. The chip strip is single-select on a single axis — combining `Workspace` and `stdio` is a v-next ask.
