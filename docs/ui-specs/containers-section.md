# Containers Section

> Dashboard section ([F-597](https://github.com/forge-ide/forge/issues/615)) — active sandbox containers + first-run runtime banner + tail-style logs flyout.

---

## Purpose

Surface the Level-2 sandbox containers Forge sessions create, let the operator stop / remove them outside of session teardown, and tail their logs without dropping to a terminal. Sessions register containers via `ContainerRegistryState::register`; this section is the read-and-control surface.

## Where

`<ContainersSection>` mounts inside the Dashboard root, anchored at id `containers-section`. The accompanying `<ContainerRuntimeBanner>` mounts at the top of the Dashboard when `detect_container_runtime` reports the runtime is missing / broken / rootless-unavailable. Component path: `web/packages/app/src/components/dashboard/ContainersSection.tsx`.

## Size

Fills the dashboard column width. Vertically scrolling row list — bounded only by the active-container count. The logs flyout is a focus-trapped overlay, sized ≈ 80% of viewport.

## Structure

### Section

```
┌─ CONTAINERS ───────────────────────────────────────────────┐
│ ┌─ a3f1c2b4d5e6  ghcr.io/forge/sandbox:debian12 ───────┐   │
│ │ session: 8c2a1f0e            12m ago                 │   │
│ │                              [LOGS] [STOP] [REMOVE]  │   │
│ └──────────────────────────────────────────────────────┘   │
│ ┌─ b1c8e0f2a93d  …                          [stopped] ─┐   │
│ │ session: 4f9d2e7a            1h ago                  │   │
│ │                              [LOGS]    —    [REMOVE] │   │
│ └──────────────────────────────────────────────────────┘   │
└────────────────────────────────────────────────────────────┘
```

Row anatomy:
- **Head:** 12-char container id (full id surfaced in `title` tooltip), image ref, optional `stopped` pip (color-warn) when the container exited but hasn't been removed yet.
- **Meta:** session id (8-char), relative `started_at` (`<60s` → `Ns ago`, `<1h` → `Nm ago`, `<24h` → `Nh ago`, otherwise an ISO date).
- **Actions:** `LOGS` toggle, `STOP` (disabled when `stopped`), `REMOVE` (primary — destructive but the container is already isolated).

### First-run banner

```
┌─ ⚠  Container runtime not installed (podman) ──────────────┐
│ Forge sessions will fall back to Level-1 isolation         │
│ (cgroup + seccomp). See install instructions.              │
│                                       [DON'T SHOW AGAIN]   │
└────────────────────────────────────────────────────────────┘
```

Anchored above the container list. Dismissable via "Don't show again" — persists `dashboard.container_banner_dismissed = true` in user-tier settings ([F-151](https://github.com/forge-ide/forge/issues/296)) so the banner stays gone across launches.

### Logs flyout

Toggled by the row's `LOGS` button. Renders a focus-trapped `role="dialog" aria-modal="true"` with:
- Header: `LOGS — <12-char id>` plus a `CLOSE` button.
- Body: `<pre>` log pane that paints stdout / stderr lines, stderr in `--color-warn`. Streams via `containerLogs` polling on a 2 s interval; bounded buffer of 1000 lines; tail of 200 on first poll.
- Window-level `Escape` closes (so the focus trap can't swallow it).

## Live data

Refreshes are event-driven, not timer-driven: the section subscribes to the `CONTAINERS_CHANGED_EVENT` Tauri event bus, which `stop_container` / `remove_container` / session teardown all emit. Local `STOP` / `REMOVE` clicks also issue an inline `refetch()` so the row updates within the same tick.

## States

The section renders all four `docs/design/component-principles.md` states distinctly:

- **Loading.** Skeleton placeholder per [`dashboard.md §D.5.1`](./dashboard.md#d51-loading-state-primitives-f-684) — three `block`-variant skeletons sized to row height.
- **Empty.** `// no active sandbox containers. They appear here when a session uses Level-2 isolation.` placeholder, mono 11px, `--color-text-tertiary`.
- **Error.** A `role="alert"` `containers-section__error` line above the list when the most recent action (`stopContainer` / `removeContainer`) rejects, carrying the verbatim error detail. List read failures degrade silently to an empty list — the banner already covers the runtime-unavailable case so a runtime-side rejection never surfaces twice.
- **Ready.** The row list above.

The flyout has its own loading state — first poll paints into an empty `<pre>` with no skeleton (logs are append-only mono text, the empty buffer is itself the loading affordance). A polling rejection surfaces as a `role="alert"` line inside the flyout without closing it.

## Copy

- Section label: `CONTAINERS`
- Empty: `No active sandbox containers. They appear here when a session uses Level-2 isolation.`
- Stopped pip: `stopped`
- Action buttons: `LOGS` / `CLOSE LOGS`, `STOP`, `REMOVE`, `CLOSE`
- Banner headline (one of, by `RuntimeStatus.kind`):
  - `Container runtime ready` (`available` — never rendered; the banner suppresses)
  - `Container runtime not installed (<tool>)` (`missing`)
  - `Container runtime broken (<tool>)` (`broken`)
  - `Rootless mode unavailable (<tool>)` (`rootless_unavailable`)
  - `Container runtime probe failed` (`unknown`)
- Banner detail: per status — see component for verbatim copy. Truncated reason strings cap at 160 chars.
- Banner CTA: `DON'T SHOW AGAIN` / `See install instructions`.
- Logs title: `LOGS — <12-char id>` (mono, em dash with single space either side).

## Color & typography

- Stopped pip: `--color-warn` text on `--color-surface-2`.
- Stderr log lines: `--color-warn` text. Stdout: `--color-text-primary`.
- Log pane font: `--font-mono` 11px. Timestamp prefix: `--color-text-tertiary`.
- Banner icon: `--color-warn`. Banner is `role="alert"` — semantically assertive even though sessions still fall back to Level-1 (per [F-596](https://github.com/forge-ide/forge/issues/614)).

## Keyboard

- Tab inside the row list — natural document order: row → row.
- Inside the logs flyout — focus is trapped by `useFocusTrap`. `Escape` closes (window-level handler).
- The `<pre>` log pane is `tabindex={0}` so screen-reader users can focus and scroll it directly.

## Destructive-action contract

`STOP` and `REMOVE` are inline buttons today — Forge's destructive-confirmation pattern is being introduced for this section in F-689. Until F-689 lands, the buttons fire immediately on click; the cgroup teardown is itself bounded and idempotent. When F-689 ships, this spec is updated to require a confirm step on `REMOVE` — `STOP` stays single-step (a stopped container can be re-started via session resume; removal is the irreversible step).

## Cross-spec references

- [`dashboard.md`](./dashboard.md) — root-window layout; the runtime banner sits above the providers section.
- [`dashboard.md §D.5.1`](./dashboard.md#d51-loading-state-primitives-f-684) — skeleton primitive contract.
- `docs/architecture/isolation-model.md` — Level-2 / Level-1 isolation model and the `forge-oci` runtime adapter.
- `docs/dev/sandbox-limits.md` — per-tool limits the sandbox enforces.

## Doesn't do

- Does not let the user *create* a container directly — sessions own that. The Dashboard surface is read + control.
- Does not stream logs server-push — it polls every 2 s. A future revision may swap to a Tauri event channel; the row contract stays the same.
- Does not let the user `exec` into a container. Manual triage is a deliberate non-goal — sessions own the workflow.
- Does not surface non-Level-2 sandboxes (cgroup-only Level-1) in this list. Those are session-internal.
