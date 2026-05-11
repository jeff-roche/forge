# Containers Section

> Dashboard section ([F-597](https://github.com/forge-ide/forge/issues/615)) — active sandbox containers rendered as full-width rows on the dashboard's container card, with header runtime stats and inline per-row controls.

---

## Purpose

Surface the Level-2 sandbox containers Forge sessions create, let the operator stop them or open a terminal inline, and aggregate runtime stats in the card header. Sessions register containers via `ContainerRegistryState::register`; this section is the read-and-control surface.

## Where

`<ContainersSection>` mounts inside the Dashboard root as the `col-12` card on the secondary row, anchored at id `containers-section`. The accompanying `<ContainerRuntimeBanner>` mounts at the top of the Dashboard when `detect_container_runtime` reports the runtime is missing / broken / rootless-unavailable. Component path: `web/packages/app/src/components/dashboard/ContainersSection.tsx`.

## Size

Spans the full 12-column dashboard width (see `DESIGN.md §Layout grid`). Body is a vertically stacking row list — bounded only by the active-container count. The card head and body share the dashboard card chrome; no overlay surfaces.

## Structure

### Section

```
┌─ CONTAINERS  3 running · 2.4 GB · podman ──────────── [Prune] [Logs] ─┐
│ ●  refactor-sandbox-a3f1                                              │
│    sha256:8b9f… · mounts: ~/code/acme-api:ro                          │
│    oci.io/forge/rust-tools:0.3.1    cpu 0.8 · ram 412M   [term] [stop]│
│ ●  docs-preview                                                       │
│    sha256:2c0e… · ports: 4173:80                                      │
│    oci.io/forge/node-preview:20     cpu 0.1 · ram 96M    [term] [stop]│
│ ◐  pg-test-db                                                         │
│    sha256:5a1d… · idle 14m                                            │
│    docker.io/library/postgres:16    cpu 0.0 · ram 51M    [term] [stop]│
└───────────────────────────────────────────────────────────────────────┘
```

### Header meta

The card head carries two clusters separated by `margin-left: auto`:
- **Left:** the `CONTAINERS` label plus an aggregate stats line — running count, total RAM, runtime name, joined by `·` — rendered in `var(--font-mono)` at `var(--type-mono-xxs)` in `var(--color-text-tertiary)`. Pattern: `<n> running · <total-ram> · <runtime>`. Singular collapses (`1 running`); zero suppresses the stats line entirely (only the label remains).
- **Right:** two header actions — `Prune` (ghost; clears stopped + dangling containers via the runtime adapter) and `Logs` (ghost; opens the full-runtime log stream — see §Logs below). Both use the `btn-ghost` variant from `DESIGN.md` (small chip sizing: compact horizontal padding, `var(--type-mono-xs)`).

### Row layout

Each container is one full-width row inside the card body, no per-row card chrome. Row track: `auto 1fr auto auto auto` with `gap: var(--sp-3)`, vertical padding `var(--sp-2) var(--sp-4)`, and a `1px solid var(--color-border-1)` bottom border (suppressed on `:last-child`). Cells, left to right:

1. **Liveness dot.** A `live-dot` pip — `var(--color-success)` for running, `var(--color-warn)` for `stopped` / `unhealthy`. Pulses when running (shared `pulse` keyframes from `DESIGN.md §Status pill`).
2. **Identity stack.** Two stacked lines: container name (medium weight, body type) on top; image digest plus a short context fragment (mount, port, or idle-for) on the second line in `var(--font-mono)` at `11px` (`--type-mono-xs`) in `var(--color-text-tertiary)`. Full digest surfaces in `title`.
3. **Image ref.** `var(--font-mono)`, `--type-mono-xs`, `var(--color-text-primary)` — registry-qualified image tag.
4. **Resource readout.** `var(--font-mono)` at `10px` (`--type-mono-xxs`), `var(--color-text-secondary)`. Format: `cpu <n.n> · ram <Mn|Gn>`. Zero CPU collapses to `cpu 0.0` (it disambiguates idle from missing data).
5. **Actions.** A 4px-gap flex of two `btn-ic` icon buttons — `term` (drops the operator into the container via the session terminal pane) then `stop` (disabled when the container is already stopped). Both carry `aria-label` and a `title` tooltip; no destructive-confirm prompt at this tier — stop is reversible via session resume.

### First-run banner

```
┌─ ⚠  Container runtime not installed (podman) ──────────────┐
│ Forge sessions will fall back to Level-1 isolation         │
│ (cgroup + seccomp). See install instructions.              │
│                                       [DON'T SHOW AGAIN]   │
└────────────────────────────────────────────────────────────┘
```

Anchored above the container card. Dismissable via "Don't show again" — persists `dashboard.container_banner_dismissed = true` in user-tier settings ([F-151](https://github.com/forge-ide/forge/issues/296)) so the banner stays gone across launches.

### Logs

The header `Logs` action opens a full-runtime log pane (no longer a focus-trapped flyout per row). It renders inline beneath the row list when toggled, framed by the same card chrome:
- Header: `LOGS — <runtime>` plus a `CLOSE` action that collapses the pane.
- Body: `<pre>` log pane that paints stdout / stderr lines, stderr in `var(--color-warn)`. Streams via `containerLogs` polling on a 2 s interval; bounded buffer of 1000 lines; tail of 200 on first open. Stream is multiplexed across all running containers; each line is prefixed with the originating container's 12-char id in `var(--color-text-tertiary)`.

Per-row inline log toggles were removed when row actions were trimmed to `term` and `stop`; the header `Logs` action is the only entry point.

## Actions

Action contract summary:

| Action       | Where       | Behavior                                                                                          |
|--------------|-------------|---------------------------------------------------------------------------------------------------|
| `Prune`      | card head   | Invokes the runtime adapter's `prune` — stopped containers and dangling images cleared in one go. |
| `Logs`       | card head   | Toggles the inline multiplexed log pane.                                                          |
| `term`       | per-row     | Opens a terminal in the host session bound to the container.                                      |
| `stop`       | per-row     | Stops the container. Disabled when the row is already `stopped`. Emits `CONTAINERS_CHANGED_EVENT`. |

There is no per-row `remove` in V1 — removal happens via `Prune` (sweeping stopped containers) or via session teardown. Surfacing `remove` per-row returns when destructive-confirmation lands (F-689) so the irreversible action gates behind a prompt.

## Live data

Refreshes are event-driven, not timer-driven: the section subscribes to the `CONTAINERS_CHANGED_EVENT` Tauri event bus, which `stop_container` / `prune_containers` / session teardown all emit. Local `stop` and header `Prune` clicks also issue an inline `refetch()` so rows update within the same tick. The header meta line (running count, total RAM) recomputes off the same query result — no separate stats fetch.

## States

The section renders all four `docs/design/component-principles.md` states distinctly:

- **Loading.** Skeleton placeholder per [`dashboard.md §D.5.1`](./dashboard.md#d51-loading-state-primitives-f-684) — three `block`-variant skeletons sized to the new full-width row height (identity stack + image + resources + actions cells). The card head paints immediately with the `CONTAINERS` label only; the meta stats line is held until the first paint resolves so the count never flashes from `0` to `n`. The `Prune` / `Logs` header actions render disabled while loading.
- **Empty.** `No active sandbox containers. They appear here when a session uses Level-2 isolation.` rendered as a single full-row placeholder, `var(--font-mono)` at `11px` in `var(--color-text-tertiary)`. The header meta line collapses (label only); `Prune` / `Logs` render disabled.
- **Error.** A `role="alert"` `containers-section__error` line above the row list when the most recent action (`stopContainer` / `pruneContainers`) rejects, carrying the verbatim error detail. List read failures degrade silently to an empty list — the banner already covers the runtime-unavailable case so a runtime-side rejection never surfaces twice.
- **Ready.** The row list above.

The inline log pane has its own loading state — first poll paints into an empty `<pre>` with no skeleton (logs are append-only mono text, the empty buffer is itself the loading affordance). A polling rejection surfaces as a `role="alert"` line inside the pane without collapsing it.

## Copy

- Section label: `CONTAINERS`
- Header meta pattern: `<n> running · <total-ram> · <runtime>` (e.g. `3 running · 2.4 GB · podman`)
- Empty: `No active sandbox containers. They appear here when a session uses Level-2 isolation.`
- Stopped liveness affordance: pip-only (no text label); `aria-label="stopped"` on the dot.
- Header actions: `Prune`, `Logs` / `Close logs`
- Per-row actions (icon, tooltip only): terminal icon → `Open terminal`; stop icon → `Stop container`
- Banner headline (one of, by `RuntimeStatus.kind`):
  - `Container runtime ready` (`available` — never rendered; the banner suppresses)
  - `Container runtime not installed (<tool>)` (`missing`)
  - `Container runtime broken (<tool>)` (`broken`)
  - `Rootless mode unavailable (<tool>)` (`rootless_unavailable`)
  - `Container runtime probe failed` (`unknown`)
- Banner detail: per status — see component for verbatim copy. Truncated reason strings cap at 160 chars.
- Banner CTA: `DON'T SHOW AGAIN` / `See install instructions`.
- Inline log pane title: `LOGS — <runtime>` (mono, em dash with single space either side).

## Color & typography

- Liveness dot: `var(--color-success)` running, `var(--color-warn)` stopped / unhealthy. Pulse via the shared `pulse` keyframes.
- Stderr log lines: `var(--color-warn)` text. Stdout: `var(--color-text-primary)`.
- Log pane font: `var(--font-mono)` at `--type-mono-xs`. Container-id prefix: `var(--color-text-tertiary)`.
- Row dividers: `1px solid var(--color-border-1)` between rows; no border on the last row.
- Header meta line: `var(--font-mono)` at `--type-mono-xxs`, `var(--color-text-tertiary)`.
- Banner icon: `var(--color-warn)`. Banner is `role="alert"` — semantically assertive even though sessions still fall back to Level-1 (per [F-596](https://github.com/forge-ide/forge/issues/614)).

## Keyboard

- Tab inside the card head — `Prune` → `Logs` → first row.
- Tab inside the row list — natural document order, row by row, hitting `term` then `stop` per row.
- Inline log pane — focusable region; the `<pre>` log pane is `tabindex={0}` so screen-reader users can focus and scroll it directly. `Escape` while focused inside the pane collapses it.

## Destructive-action contract

Per-row `stop` is single-step — a stopped container can be re-started via session resume, so the action is reversible at the session tier. Header `Prune` is irreversible (it removes stopped containers and dangling images in one sweep); a confirm step gates it once destructive-confirmation lands (F-689). Until F-689 ships, `Prune` fires immediately on click; the runtime adapter's prune is itself bounded and idempotent.

## Cross-spec references

<a id="container-card-on-dashboard"></a>

### Container card on dashboard

The container card is the `col-12` cell on the dashboard's secondary row (see [`dashboard.md`](./dashboard.md)). The dashboard spec defers its anatomy here; this section owns the header meta, row layout, and action contract. The skeleton entry in [`dashboard.md §D.5.1`](./dashboard.md#d51-loading-state-primitives-f-684) tracks the row shape defined above.

- [`dashboard.md`](./dashboard.md) — root-window layout; the runtime banner sits above the providers section, and the container card is the `col-12` row beneath the Usage / Workspace toggles.
- [`dashboard.md §D.5.1`](./dashboard.md#d51-loading-state-primitives-f-684) — skeleton primitive contract.
- [`DESIGN.md` §Layout primitives](../../DESIGN.md) — `Layout grid` (12-col / `col-12` span), `Status pill` (pulse keyframes), and `Status bar` (container-status slot) tokens consumed here.
- `docs/architecture/isolation-model.md` — Level-2 / Level-1 isolation model and the `forge-oci` runtime adapter.
- `docs/dev/sandbox-limits.md` — per-tool limits the sandbox enforces.

## Doesn't do

- Does not let the user *create* a container directly — sessions own that. The Dashboard surface is read + control.
- Does not stream logs server-push — it polls every 2 s. A future revision may swap to a Tauri event channel; the row contract stays the same.
- Does not surface non-Level-2 sandboxes (cgroup-only Level-1) in this list. Those are session-internal.
- Does not expose per-row `remove` in V1 — see §Actions. Removal is `Prune` (sweep) or session teardown.
