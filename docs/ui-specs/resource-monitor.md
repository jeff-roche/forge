# Resource Monitor

> Backend per-agent-instance sampler ([F-451 / #542](https://github.com/forge-ide/forge/issues/542) / F-152 Linux / F-156 macOS+Windows / F-140 UI chrome) — feeds the AgentMonitor inspector's `cpu` / `rss` / `fds` pills.

---

## Purpose

Give the AgentMonitor inspector live numeric proof that an agent process is actually doing work. The pills exist so an operator triaging a stuck or runaway agent can tell at a glance whether the process is busy, idle, or leaking — without leaving the app for `top` / `ps`.

## Where

The visible surface is the **Resource usage** sub-section of the AgentMonitor inspector (`agent-monitor.md §9.3` item 4). The data pipeline that feeds it lives in `crates/forge-session/src/resource_monitor.rs` (Rust backend). The pills themselves render in `web/packages/app/src/routes/AgentMonitor.tsx` inside the inspector aside.

## Size

Three inline pills, mono 10px, separated by the inspector's standard pill spacing. The pills sit in a single `<ul class="agent-monitor__pills">` row and wrap to a second line at narrow inspector widths. No fixed height — the row grows with the inspector's content-driven layout.

## Structure

```
┌─ Resource usage ───────────────────────────────┐
│ [ cpu 12.4% ]  [ rss 184MB ]  [ fds 73 ]       │
└────────────────────────────────────────────────┘
```

Each pill is an `<li>` rendered exactly as `<metric> <value>` separated by a single space:

- `cpu <pct>%` — rolling-average CPU percent. One decimal place.
- `rss <n>MB` — resident set size in MiB (rounded; the backend reports MiB integers).
- `fds <n>` — open file-descriptor count. Integer.

When a value is unavailable (no sample yet or `untrack` fired) the value field renders `—` (em dash, single character). The metric label is always rendered so the row's slot count stays stable across loading / ready / cleared states.

## Data pipeline

```
ResourceMonitor::track(instance_id, pid)
   └── per-instance tokio ticker (1Hz default; 1–5Hz tunable)
        └── Sampler::sample(pid)            ← /proc, libproc, or Win32 GetProcess*
             └── Event::ResourceSample { instance_id, sample }
                  └── broadcast bus  ────────►  AgentMonitor inspector pills
```

- Each `(instance_id, pid)` pair gets its own ticker task. `untrack` aborts the matching task; the event stream naturally stops for that id so the pills clear back to `—`.
- Dropping the monitor aborts every tracked task — the no-leak invariant.
- The platform sampler is *stateless* per F-575: each call returns a cumulative `Sample`, the ticker computes the delta against a per-task baseline, and CPU% is derived from the delta. This eliminates the prior per-instance HashMap mutex contention point.

The frontend has no opinion on which platform is active — it consumes `Event::ResourceSample` from the merged session event stream and reads `cpu` / `rss` / `fds` off the `sample` directly.

## States

- **Untracked.** The instance has no `(instance_id, pid)` registration yet. All three pills render `—`. This is the steady state before a sidecar process exists.
- **Sampling.** The ticker is running; samples arrive at the configured cadence. Pills render the latest values.
- **Stale.** A sample arrived more than `2 × tick interval` ago but `untrack` has not fired. Pills retain the last value rendered — the monitor does not stamp values stale today; if no new sample arrives the displayed numbers freeze. (A future revision may grey them after a staleness threshold; the inspector is the natural place for that signal.)
- **Cleared.** `untrack` fired; the broadcast stream stops emitting for this id. The inspector resets each pill's value to `—`.

The `agent-monitor.md §9.4` four-state contract for the *inspector* is unchanged — these states are the per-pill data states, layered underneath.

## Copy

- Section heading (in the inspector): `Resource usage` (sentence case — matches the other inspector headings `Definition`, `Allowed tools`, `Allowed paths`).
- Pill format: `cpu <n>%` / `rss <n>MB` / `fds <n>` / `<metric> —` for unavailable.
- The unit is *always* attached to the value (`%`, `MB`) — the metric label is the noun, the unit is the dimension; never collapse them.

## Color & typography

- Pills inherit the standard inspector pill chrome: `--color-surface-2` background, 1px `--color-border-1`, mono 10px text, `--color-text-primary`.
- No color encoding for thresholds today — a high CPU value carries the same chrome as a low one. Threshold coloring is a future iteration; the current contract is "show numbers honestly, let the operator interpret".

## Keyboard

The pills are non-interactive. Each pill is an `<li>` with no inherent focus stop; screen readers reach them via the inspector's content ordering. The future plan (per `agent-monitor.md §9.2`) is to expose values via per-cell `aria-label`s read on focus traversal — *not* via `aria-live`, because a 1Hz update inside a live region would re-announce the toolbar every tick (an APG anti-pattern).

## Performance contract

- Default tick: **1 Hz**. Allowed range: **1–5 Hz**. Low enough to keep `/proc` read amplification negligible on a big session, fast enough that a human sees the pill value change.
- Broadcast capacity: **1024** (`EVENT_BUS_CAPACITY`) — matches `forge_session::bg_agents::EVENT_BUS_CAPACITY` so a slow subscriber on the merged `session:event` stream doesn't drop resource samples before every other variant.
- The `tasks` map is guarded by `std::sync::Mutex` (not `tokio::sync::Mutex`) because the critical section is purely HashMap mutation — no `.await` ever runs while the lock is held. `Drop` is deterministic; every outstanding task is aborted synchronously.

## Cross-spec references

- [`agent-monitor.md §9.3`](./agent-monitor.md) — the inspector surface that hosts these pills.
- `docs/architecture/agent-sidecar.md` — the sidecar process model that yields real per-agent PIDs (without it the monitor falls back to the daemon's own PID and is elided by the daemon-PID guard).
- `docs/architecture/overview.md` — the daemon / sidecar split that makes per-instance accounting honest.

## Doesn't do

- Does not chart the values over time. The pills are point-in-time. A future "expand" affordance may reveal a sparkline; today the historical view is the operator's external tooling.
- Does not surface threshold alerts. No "CPU > 90%" badge today; that's threshold-coloring territory.
- Does not sample the daemon itself. The daemon-PID guard explicitly elides daemon samples — operators triage `forged` resource use via OS tooling.
- Does not work on platforms outside Linux / macOS / Windows. The sampler trait `compile_error!`s on other targets — F-156 removed the silent `Sample::default()` stub because all-zero pills masquerading as real readings are worse than a build error.
