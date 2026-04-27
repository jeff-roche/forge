// Agent Monitor route (F-140). Three-column layout per
// `docs/ui-specs/agent-monitor.md` §9: list (280px) | trace (flex) | inspector
// (340px). Renders the union of background agents (fetched via
// `list_background_agents`) and sub-agents surfaced through `session:event`
// (F-134 `SubAgentSpawned`, F-139 Step*), plus a session-root row so the
// active turn's trace has a home when no sub-agent exists.
//
// Event plumbing kept intentionally thin: the view subscribes to the existing
// `sessionEvents` store and re-derives timelines from its most-recent-event
// signal. A richer aggregator can land alongside resource sampling (cpu/rss/
// fds) — see §9.3 "Resource usage" — without reshaping this file.

import {
  createEffect,
  createMemo,
  createResource,
  createSignal,
  For,
  on,
  onCleanup,
  Show,
  type Component,
  type JSX,
} from 'solid-js';
import { createMountedSubscription } from '../ipc/useEventListener';
import { useNavigate, useParams, useSearchParams } from '@solidjs/router';
import { Button, IconButton, Tab } from '@forge/design';
import { useFocusTrap } from '../lib/useFocusTrap';
import type { BgAgentSummary } from '@forge/ipc';
import {
  onSessionEvent,
  type SessionEventPayload,
  listBackgroundAgents,
  promoteBackgroundAgent,
  stopBackgroundAgent,
} from '../ipc/session';
import './AgentMonitor.css';

// ---------------------------------------------------------------------------
// Wire shapes + domain types
// ---------------------------------------------------------------------------

/** Filter tabs per spec §9.1. */
export type AgentFilter = 'all' | 'running' | 'background' | 'session' | 'failed';

/** Row state drives progress-bar color + pulsing ring. */
export type AgentRowState = 'running' | 'queued' | 'done' | 'error';

// F-381: `StepKind` mirrors the Rust `StepKind` enum. Defined locally until
// ts-rs codegen adds it to `@forge/ipc`; re-exported so existing imports from
// `./AgentMonitor` keep working without change.
export type StepKind = 'plan' | 'tool' | 'model' | 'wait' | 'spawn' | 'mcp';

/** Status for a timeline step — `running` renders the pulsing ring. */
export type StepStatus = 'running' | 'done' | 'error';

/** Classification used for sorting + filter assignment. */
export type AgentCategory = 'session' | 'background' | 'sub-agent';

export interface AgentRow {
  id: string;
  name: string;
  category: AgentCategory;
  state: AgentRowState;
  /** Parent row id when `category === 'sub-agent'`. */
  parentId?: string;
  /** 0..1 — drives left-column progress bar width. */
  progress: number;
  /** Relative start ISO timestamp; `undefined` collapses the meta row. */
  startedAt?: string;
  /** Free-form model label; shown on the meta row. */
  model?: string;
}

export interface AgentStep {
  id: string;
  kind: StepKind;
  title: string;
  status: StepStatus;
  startedAt: string;
  /** Optional preview shown in the drawer; mono 11px per spec §9.2. */
  preview?: string;
}

/** Inspector pane datum — fields absent from the backend render as `-`. */
export interface AgentInspectorData {
  source?: string;
  provider?: string;
  model?: string;
  isolation?: string;
  maxTokens?: number;
  allowedTools: string[];
  allowedPaths: string[];
  /** `undefined` renders as `-`; no placeholder zeros so missing data is visible. */
  resources: {
    cpu?: number;
    rss?: number;
    fds?: number;
  };
}

/** Per-instance resource-sample snapshot folded from F-152
 * `Event::ResourceSample`. Stored in the live state map so the Inspector's
 * cpu / rss / fds pills read the most recent value. `null` fields in the
 * wire payload survive as `undefined` here so the pill renders the `—`
 * placeholder the design calls out. */
export interface ResourceSnapshot {
  cpu?: number;
  rss?: number;
  fds?: number;
}

// ---------------------------------------------------------------------------
// Filter + sort
// ---------------------------------------------------------------------------

/** Public so the tests can exercise the sort invariant directly. */
export function filterAgents(rows: AgentRow[], filter: AgentFilter): AgentRow[] {
  switch (filter) {
    case 'all':
      return rows;
    case 'running':
      return rows.filter((r) => r.state === 'running');
    case 'background':
      return rows.filter((r) => r.category === 'background');
    case 'session':
      return rows.filter((r) => r.category === 'session' || r.category === 'sub-agent');
    case 'failed':
      return rows.filter((r) => r.state === 'error');
  }
}

// Spec §9.1 sort: running → queued → error → done, each group most-recent-first.
const STATE_ORDER: Record<AgentRowState, number> = {
  running: 0,
  queued: 1,
  error: 2,
  done: 3,
};

export function sortAgents(rows: AgentRow[]): AgentRow[] {
  return [...rows].sort((a, b) => {
    const byState = STATE_ORDER[a.state] - STATE_ORDER[b.state];
    if (byState !== 0) return byState;
    // Recent-first — missing startedAt sinks.
    const aStarted = a.startedAt ? Date.parse(a.startedAt) : -Infinity;
    const bStarted = b.startedAt ? Date.parse(b.startedAt) : -Infinity;
    return bStarted - aStarted;
  });
}

// ---------------------------------------------------------------------------
// Live agent state assembled from bg-agents + session events
// ---------------------------------------------------------------------------

interface InternalAgentState {
  sessionRoot: AgentRow;
  rows: AgentRow[];
  stepsByAgent: Record<string, AgentStep[]>;
}

/** Snapshot of the live event-driven state. Exported for the unit test
 * that pins the session-root upsert path without mounting a full route.
 */
export interface LiveAgentState {
  subAgents: AgentRow[];
  stepsByAgent: Record<string, AgentStep[]>;
  /** F-152: most recent resource sample per instance id. Undefined keys
   * render as `—` pills in the Inspector. A completion event clears the
   * entry so the pills reset automatically. */
  resourcesByAgent: Record<string, ResourceSnapshot>;
  /**
   * F-449: client-side aggregation of unique tool names observed via
   * `ToolCallStarted` per instance id. Drives the trace-toolbar's
   * `tools-used` summary. Insertion order is preserved (first-seen first)
   * so the toolbar renders deterministically. Cleared on terminal
   * transition alongside `resourcesByAgent`.
   */
  toolsByAgent: Record<string, string[]>;
}

/**
 * Pure folder for a `session:event` payload. Adding sub-agent rows, step
 * timelines, and session-root rows lands here so the logic can be pinned
 * without mounting `<AgentMonitor>`. Callers pass the prior state and get
 * back the post-event snapshot; a no-op event returns the input reference
 * unchanged so a SolidJS setter short-circuits.
 */
export function applyEventToState(
  prev: LiveAgentState,
  payload: { event: unknown },
): LiveAgentState {
  const ev = payload.event as Record<string, unknown> | null;
  if (!ev || typeof ev !== 'object') return prev;
  const type = ev['type'];

  if (type === 'sub_agent_spawned') {
    const parent = ev['parent'];
    const child = ev['child'];
    if (typeof parent !== 'string' || typeof child !== 'string') return prev;
    const model = typeof ev['model'] === 'string' ? (ev['model'] as string) : undefined;
    const next: AgentRow = {
      id: child,
      name: `sub-agent ${child.slice(0, 8)}`,
      category: 'sub-agent',
      state: 'running',
      parentId: parent,
      progress: 0.3,
      startedAt: new Date().toISOString(),
      ...(model ? { model } : {}),
    };
    return {
      ...prev,
      subAgents: [...prev.subAgents.filter((r) => r.id !== child), next],
    };
  }

  if (type === 'session_started') {
    // F-449: stamp the session-root's `startedAt` from `SessionStarted.at` so
    // the trace-toolbar's elapsed clock derives from a real wall-clock anchor
    // instead of the first `step_started`'s arrival time. Upserts a session
    // row when none exists; preserves a row already created by an earlier
    // `step_started` event (the timestamps disagree by at most a few ms).
    const at = ev['at'];
    if (typeof at !== 'string') return prev;
    const idx = prev.subAgents.findIndex((r) => r.id === 'session-root');
    if (idx === -1) {
      const next: AgentRow = {
        id: 'session-root',
        name: 'session',
        category: 'session',
        state: 'running',
        progress: 0.3,
        startedAt: at,
      };
      return { ...prev, subAgents: [...prev.subAgents, next] };
    }
    const existing = prev.subAgents[idx]!;
    if (existing.startedAt === at) return prev;
    const updated: AgentRow = { ...existing, startedAt: at };
    const nextSubAgents = prev.subAgents.slice();
    nextSubAgents[idx] = updated;
    return { ...prev, subAgents: nextSubAgents };
  }

  if (type === 'assistant_message') {
    // F-449: capture the model label per `AssistantMessage.model` so the
    // trace-toolbar surfaces what produced the most-recent assistant turn.
    // The event has no `instance_id`, so it attributes to the session-root —
    // sub-agents publish their own model via `SubAgentSpawned.model`.
    const model = ev['model'];
    if (typeof model !== 'string') return prev;
    const idx = prev.subAgents.findIndex((r) => r.id === 'session-root');
    if (idx === -1) return prev;
    const existing = prev.subAgents[idx]!;
    if (existing.model === model) return prev;
    const updated: AgentRow = { ...existing, model };
    const nextSubAgents = prev.subAgents.slice();
    nextSubAgents[idx] = updated;
    return { ...prev, subAgents: nextSubAgents };
  }

  if (type === 'tool_call_started') {
    // F-449: client-side aggregation of unique tool names. The wire event
    // has no `instance_id`, so aggregation attributes to the session-root —
    // the only scope where we can prove the tool ran. Sub-agent toolbars
    // surface their own `tool_count` from `SubAgentSpawned` instead.
    const tool = ev['tool'];
    if (typeof tool !== 'string') return prev;
    const key = 'session-root';
    const existing = prev.toolsByAgent[key] ?? [];
    if (existing.includes(tool)) return prev;
    return {
      ...prev,
      toolsByAgent: { ...prev.toolsByAgent, [key]: [...existing, tool] },
    };
  }

  if (type === 'step_started') {
    const stepId = ev['step_id'];
    const kind = ev['kind'];
    const instanceId = ev['instance_id'];
    if (typeof stepId !== 'string' || typeof kind !== 'string') return prev;
    const agentId =
      typeof instanceId === 'string' && instanceId.length > 0
        ? instanceId
        : 'session-root';
    // Upsert a row so session-root steps (live `StepStarted.instance_id`)
    // attach to a selectable row in the left column. The legacy
    // `'session-root'` fallback covers a pre-F-140 daemon where
    // `instance_id` is `None` — the UI still needs a row to hang steps off.
    let subAgents = prev.subAgents;
    const hasRow = prev.subAgents.some((r) => r.id === agentId);
    if (!hasRow) {
      const label =
        agentId === 'session-root'
          ? 'session'
          : `session ${agentId.slice(0, 8)}`;
      subAgents = [
        ...prev.subAgents,
        {
          id: agentId,
          name: label,
          category: 'session',
          state: 'running',
          progress: 0.3,
          startedAt: new Date().toISOString(),
        },
      ];
    }
    const startedAt =
      typeof ev['started_at'] === 'string'
        ? (ev['started_at'] as string)
        : new Date().toISOString();
    const newStep: AgentStep = {
      id: stepId,
      kind: normaliseKind(kind),
      title: `${kind} step`,
      status: 'running',
      startedAt,
    };
    const existing = prev.stepsByAgent[agentId] ?? [];
    return {
      ...prev,
      subAgents,
      stepsByAgent: { ...prev.stepsByAgent, [agentId]: [...existing, newStep] },
    };
  }

  if (type === 'step_finished') {
    const stepId = ev['step_id'];
    if (typeof stepId !== 'string') return prev;
    // F-573: scan once for the owning agent + step index instead of
    // rebuilding every agent's array. Previously a single step_finished
    // event allocated O(agents × steps) — at 1000 events/60Hz across 20
    // agents that froze the route. Now an event allocates one new step
    // object + one new array for the owning agent only; siblings keep
    // identity, so `<For>`'s reference-keyed children don't re-render.
    let ownerKey: string | null = null;
    let ownerSteps: AgentStep[] | null = null;
    let stepIndex = -1;
    for (const k of Object.keys(prev.stepsByAgent)) {
      const steps = prev.stepsByAgent[k];
      if (!steps) continue;
      const idx = steps.findIndex((s) => s.id === stepId);
      if (idx !== -1) {
        ownerKey = k;
        ownerSteps = steps;
        stepIndex = idx;
        break;
      }
    }
    if (ownerKey === null || ownerSteps === null) return prev;
    const nextStatus: StepStatus =
      outcomeOf(ev['outcome']) === 'error' ? 'error' : 'done';
    const existing = ownerSteps[stepIndex];
    if (!existing || existing.status === nextStatus) return prev;
    const updated: AgentStep = { ...existing, status: nextStatus };
    const nextSteps = ownerSteps.slice();
    nextSteps[stepIndex] = updated;
    return {
      ...prev,
      stepsByAgent: { ...prev.stepsByAgent, [ownerKey]: nextSteps },
    };
  }

  if (type === 'background_agent_completed') {
    // F-140: when the Stop button (or a natural terminal transition) fires
    // a `BackgroundAgentCompleted` event on `session:event`, flip the
    // matching sub-agent row to its terminal variant in-place instead of
    // dropping it silently. The row stays in the left column so the user
    // can still inspect the trace + definition — the progress bar + pulse
    // turn off, the state chip reads "done".
    //
    // Background agents (category `'background'`) are refetched from
    // `list_background_agents` on this event elsewhere in the route; those
    // rows re-render from backend state. This branch handles the sub-agent
    // + session-root rows that only live in the in-memory `subAgents`
    // signal.
    const id = ev['id'];
    if (typeof id !== 'string') return prev;
    let changed = false;
    const nextSubAgents = prev.subAgents.map((r) => {
      if (r.id !== id || r.state === 'done' || r.state === 'error') return r;
      changed = true;
      return { ...r, state: 'done' as AgentRowState, progress: 1 };
    });
    // F-152: clearing the resources map entry on terminal transition is
    // the mechanism behind "pills clear back to '—' when the instance
    // terminates" — the inspector reads from this map and an absent key
    // resolves to undefined → dash.
    const hadResources = id in prev.resourcesByAgent;
    // F-449: same contract for the tools-used aggregation — drop it on
    // terminal transition so a fresh run for the same id (after promote +
    // re-spawn) starts from zero rather than carrying stale tool names.
    const hadTools = id in prev.toolsByAgent;
    if (!changed && !hadResources && !hadTools) return prev;
    let nextResources = prev.resourcesByAgent;
    if (hadResources) {
      const { [id]: _cleared, ...rest } = prev.resourcesByAgent;
      nextResources = rest;
    }
    let nextTools = prev.toolsByAgent;
    if (hadTools) {
      const { [id]: _cleared, ...rest } = prev.toolsByAgent;
      nextTools = rest;
    }
    return {
      ...prev,
      subAgents: changed ? nextSubAgents : prev.subAgents,
      resourcesByAgent: nextResources,
      toolsByAgent: nextTools,
    };
  }

  if (type === 'resource_sample') {
    // F-152: fold the sampler's per-instance emission into the live
    // resources map. Missing fields (Option<None> on the Rust side) arrive
    // as `null` on the wire; they're preserved as `undefined` here so the
    // pill renders the `—` placeholder without falsely reading as zero.
    const instanceId = ev['instance_id'];
    if (typeof instanceId !== 'string') return prev;
    const snapshot: ResourceSnapshot = {};
    const cpu = ev['cpu_pct'];
    if (typeof cpu === 'number') snapshot.cpu = cpu;
    const rss = ev['rss_bytes'];
    if (typeof rss === 'number') snapshot.rss = rss;
    const fds = ev['fd_count'];
    if (typeof fds === 'number') snapshot.fds = fds;
    return {
      ...prev,
      resourcesByAgent: { ...prev.resourcesByAgent, [instanceId]: snapshot },
    };
  }

  return prev;
}

function bgState(s: BgAgentSummary['state']): AgentRowState {
  switch (s) {
    case 'Running':
      return 'running';
    case 'Completed':
      return 'done';
    case 'Failed':
      return 'error';
    default:
      return 'queued';
  }
}

function toBgRow(s: BgAgentSummary): AgentRow {
  return {
    id: s.id,
    name: s.agent_name,
    category: 'background',
    state: bgState(s.state),
    progress: s.state === 'Completed' ? 1 : s.state === 'Failed' ? 1 : 0.5,
  };
}

// ---------------------------------------------------------------------------
// Left column — agent list
// ---------------------------------------------------------------------------

/**
 * F-401: four-state async coverage for the agent list column.
 *
 * `status` drives the list column's sole render branch per
 * `agent-monitor.md §9.4` — loading / error / empty / ready must each be
 * visually distinct. Collapsing "backend rejected" into "zero agents" was
 * the precise regression F-401 flagged.
 *
 * - `loading` — `list_background_agents` is in flight. Renders `agents · probing`
 *   (noun + state per `voice-terminology.md` §8).
 * - `error`   — the resource rejected. Renders the `AGENT LIST UNAVAILABLE`
 *   block with the verbatim error detail and a RETRY action.
 * - `empty`   — the resource resolved with zero rows. Renders `// no agents`.
 * - `ready`   — the row list (already filtered + sorted by the caller).
 *
 * The loading / error / empty branches also carry the tabpanel wiring from
 * F-416 so the filter tabs' `aria-controls` always resolves to a
 * `role="tabpanel"` element regardless of which branch renders.
 */
export type AgentListStatus =
  | { kind: 'loading' }
  | { kind: 'error'; detail: string; onRetry: () => void }
  | { kind: 'ready' };

export const AgentList: Component<{
  rows: AgentRow[];
  filter: AgentFilter;
  onFilter: (f: AgentFilter) => void;
  selectedId: string | null;
  onSelect: (id: string) => void;
  /** Optional — when omitted the list behaves as pre-F-401: ready / empty. */
  status?: AgentListStatus;
}> = (props) => {
  const visible = createMemo(() => sortAgents(filterAgents(props.rows, props.filter)));
  const status = (): AgentListStatus => props.status ?? { kind: 'ready' };

  // F-416: tabs ↔ tabpanel association (WAI-ARIA APG tabs pattern). A
  // single tabpanel hosts the row list; its aria-labelledby always points
  // at the currently-selected filter tab so assistive tech can announce
  // the panel's context when filter selection changes.
  const filterTabId = (f: AgentFilter) => `agent-filter-tab-${f}`;
  const rowsPanelId = 'agent-filter-rows-panel';

  return (
    <aside class="agent-monitor__list" aria-label="Agents">
      <div role="tablist" class="agent-monitor__filters">
        <For each={FILTERS}>
          {(f) => (
            <Tab
              id={filterTabId(f)}
              aria-controls={rowsPanelId}
              selected={props.filter === f}
              class={`agent-monitor__filter${props.filter === f ? ' agent-monitor__filter--active' : ''}`}
              onClick={() => props.onFilter(f)}
            >
              {f}
            </Tab>
          )}
        </For>
      </div>
      <Show when={status().kind === 'loading'}>
        <p
          class="agent-monitor__loading"
          id={rowsPanelId}
          role="tabpanel"
          aria-labelledby={filterTabId(props.filter)}
        >
          agents · probing
        </p>
      </Show>
      <Show when={status().kind === 'error'}>
        {(() => {
          const err = status() as Extract<AgentListStatus, { kind: 'error' }>;
          return (
            <div
              class="agent-monitor__error"
              id={rowsPanelId}
              role="tabpanel"
              aria-labelledby={filterTabId(props.filter)}
            >
              <div class="agent-monitor__error-body" role="alert">
                <p class="agent-monitor__error-title">AGENT LIST UNAVAILABLE</p>
                <p class="agent-monitor__error-detail">{err.detail}</p>
                <Button
                  variant="ghost"
                  size="sm"
                  class="agent-monitor__retry"
                  onClick={() => err.onRetry()}
                >
                  RETRY
                </Button>
              </div>
            </div>
          );
        })()}
      </Show>
      <Show when={status().kind === 'ready'}>
        <Show
          when={visible().length > 0}
          fallback={
            <p
              class="agent-monitor__empty"
              id={rowsPanelId}
              role="tabpanel"
              aria-labelledby={filterTabId(props.filter)}
            >
              // no agents
            </p>
          }
        >
          <ul
            class="agent-monitor__rows"
            id={rowsPanelId}
            role="tabpanel"
            aria-labelledby={filterTabId(props.filter)}
          >
            <For each={visible()}>
              {(row) => (
                <AgentListRow
                  row={row}
                  active={props.selectedId === row.id}
                  onSelect={props.onSelect}
                />
              )}
            </For>
          </ul>
        </Show>
      </Show>
    </aside>
  );
};

const FILTERS: AgentFilter[] = ['all', 'running', 'background', 'session', 'failed'];

const AgentListRow: Component<{
  row: AgentRow;
  active: boolean;
  onSelect: (id: string) => void;
}> = (props) => {
  const widthStyle = (): JSX.CSSProperties => ({
    width: `${Math.max(0, Math.min(1, props.row.progress)) * 100}%`,
  });
  return (
    <li>
      <button
        type="button"
        class="agent-monitor__row"
        classList={{
          'agent-monitor__row--active': props.active,
          'agent-monitor__row--running': props.row.state === 'running',
        }}
        aria-label={`Select agent ${props.row.name}`}
        aria-current={props.active ? 'true' : undefined}
        onClick={() => props.onSelect(props.row.id)}
      >
        <span class="agent-monitor__row-name">
          <Show when={props.row.state === 'running'}>
            <span class="agent-monitor__row-pulse" aria-hidden="true" />
          </Show>
          {props.row.name}
        </span>
        <span class="agent-monitor__row-meta">
          {props.row.category === 'background' ? 'BG' : props.row.category}
          {props.row.model ? ` · ${props.row.model}` : ''}
        </span>
        <span
          class="agent-monitor__progress"
          data-state={props.row.state}
          aria-hidden="true"
        >
          <span class="agent-monitor__progress-fill" style={widthStyle()} />
        </span>
      </button>
    </li>
  );
};

// ---------------------------------------------------------------------------
// F-449: trace-header toolbar — elapsed / model / tools-used / spawned-by.
//
// The toolbar lives directly under the live-state chip and renders every
// cell in Fira Code 10px separated by `·` per `agent-monitor.md §9.2`.
// Cells with no data render the `—` placeholder (vs. omitting) so the
// toolbar's visual rhythm stays stable while the backend prereqs (token
// counters, cost, total-step count) land in F-605/F-606.
// ---------------------------------------------------------------------------

/**
 * Format `mm:ss` elapsed since `startedAt` against `nowMs`. Returns the
 * placeholder dash when `startedAt` is missing or the parse fails so the
 * toolbar cell renders as `—` instead of `NaN:NaN`. Negative deltas (clock
 * skew between the daemon's `SessionStarted.at` and the webview's wall
 * clock) clamp to `00:00`.
 *
 * Hours past 99:59 wrap to `99:59+` so the cell width stays bounded — agent
 * monitor sessions that long are vanishingly rare and the toolbar's job is a
 * glance, not an audit log.
 */
export function formatElapsed(
  startedAt: string | undefined,
  nowMs: number,
): string {
  if (!startedAt) return '—';
  const start = Date.parse(startedAt);
  if (Number.isNaN(start)) return '—';
  const delta = Math.max(0, nowMs - start);
  const totalSeconds = Math.floor(delta / 1000);
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  if (minutes > 99) return '99:59+';
  const mm = String(minutes).padStart(2, '0');
  const ss = String(seconds).padStart(2, '0');
  return `${mm}:${ss}`;
}

/** Lookup label for a parent row id, falling back to the truncated id when
 * the parent is not present in the rows list (cross-session, race, etc.).
 */
export function spawnedByLabel(
  parentId: string | undefined,
  rows: AgentRow[],
): string | undefined {
  if (!parentId) return undefined;
  const parent = rows.find((r) => r.id === parentId);
  if (parent) return parent.name;
  return parentId.slice(0, 8);
}

/**
 * Trace-header toolbar component. Renders the four ready-now cells —
 * elapsed, model, tools-used, spawned-by — in a single mono-text row. The
 * deferred cells (tokens, cost, step-of-total) are owned by F-449's
 * follow-up PR once F-603 / F-605 / F-606 land.
 */
export const AgentTraceToolbar: Component<{
  agent: AgentRow;
  /** Unique tool names the selected agent has issued so far. */
  tools: string[];
  /** Resolved spawned-by label (parent-name or id8); `undefined` for non-sub-agents. */
  spawnedBy?: string | undefined;
  /** Live wall-clock; injected by tests to pin the elapsed cell. */
  now: number;
}> = (props) => {
  const elapsed = (): string => formatElapsed(props.agent.startedAt, props.now);
  return (
    <div
      class="agent-monitor__trace-toolbar"
      role="status"
      aria-live="polite"
      aria-label="Trace toolbar"
    >
      <span
        class="agent-monitor__trace-toolbar-cell"
        data-cell="elapsed"
        aria-label={`elapsed ${elapsed()}`}
      >
        {elapsed()}
      </span>
      <span class="agent-monitor__trace-toolbar-sep" aria-hidden="true">·</span>
      <span
        class="agent-monitor__trace-toolbar-cell"
        data-cell="model"
        aria-label={`model ${props.agent.model ?? 'unknown'}`}
      >
        {props.agent.model ?? '—'}
      </span>
      <span class="agent-monitor__trace-toolbar-sep" aria-hidden="true">·</span>
      <span
        class="agent-monitor__trace-toolbar-cell"
        data-cell="tools"
        aria-label={`tools used ${props.tools.length}`}
      >
        {props.tools.length > 0 ? `tools ${props.tools.length}` : 'tools 0'}
      </span>
      <Show when={props.spawnedBy}>
        <span class="agent-monitor__trace-toolbar-sep" aria-hidden="true">·</span>
        <span
          class="agent-monitor__trace-toolbar-cell"
          data-cell="spawned-by"
          aria-label={`spawned by ${props.spawnedBy}`}
        >
          {`↳ ${props.spawnedBy}`}
        </span>
      </Show>
    </div>
  );
};

// ---------------------------------------------------------------------------
// Middle column — trace timeline
// ---------------------------------------------------------------------------

export const AgentTrace: Component<{
  agent: AgentRow | null;
  steps: AgentStep[];
  onStepClick: (step: AgentStep) => void;
  /** F-449: unique tool names observed for the selected agent. */
  tools?: string[] | undefined;
  /** F-449: resolved spawned-by label (parent name or id8). */
  spawnedBy?: string | undefined;
  /** F-449: live wall-clock for the elapsed cell. Tests inject a fixed value. */
  now?: number | undefined;
}> = (props) => {
  return (
    <section class="agent-monitor__trace" aria-label="Trace">
      <Show
        when={props.agent}
        fallback={<p class="agent-monitor__empty">// select an agent</p>}
      >
        {(agent) => (
          <>
            <header class="agent-monitor__trace-head">
              <h2 class="agent-monitor__trace-name">{agent().name}</h2>
              <span class="agent-monitor__trace-id">{agent().id}</span>
              {/* F-407: Phase-2 live-chip — name + id + state chip is the
                  subset of the §9.2 trace header we can render without the
                  backend-plumbed toolbar (elapsed / tokens / cost / tools /
                  spawned-by). Running agents show `running · step N` where
                  N is the observed step count; `step N of M` returns once
                  the backend ships a total. See F-449 for the Phase-3
                  follow-up that restores the full chrome. */}
              <span
                class="agent-monitor__trace-chip"
                data-state={agent().state}
                aria-label={`agent state ${agent().state}`}
              >
                {agent().state === 'running'
                  ? `running · step ${props.steps.length}`
                  : agent().state}
              </span>
            </header>
            {/* F-449: ready-now subset of the §9.2 toolbar — elapsed / model
                / tools-used / spawned-by. Tokens, cost, and full step-of-M
                ride F-605 / F-606 and ship in a follow-up PR. */}
            <AgentTraceToolbar
              agent={agent()}
              tools={props.tools ?? []}
              spawnedBy={props.spawnedBy}
              now={props.now ?? Date.now()}
            />
            <Show
              when={props.steps.length > 0}
              fallback={<p class="agent-monitor__empty">// no steps yet</p>}
            >
              <ol class="agent-monitor__steps">
                <For each={props.steps}>
                  {(step) => <AgentTraceStep step={step} onClick={props.onStepClick} />}
                </For>
              </ol>
            </Show>
          </>
        )}
      </Show>
    </section>
  );
};

const AgentTraceStep: Component<{
  step: AgentStep;
  onClick: (step: AgentStep) => void;
}> = (props) => {
  return (
    <li class="agent-monitor__step">
      <button
        type="button"
        class="agent-monitor__step-btn"
        aria-label={`Open step ${props.step.title}`}
        onClick={() => props.onClick(props.step)}
      >
        <span
          class="agent-monitor__step-dot"
          classList={{
            'agent-monitor__step-dot--running': props.step.status === 'running',
            'agent-monitor__step-dot--error': props.step.status === 'error',
          }}
          aria-hidden="true"
        />
        <span class="agent-monitor__step-body">
          <span
            class="agent-monitor__step-chip"
            data-kind={props.step.kind}
            aria-label={`step kind ${props.step.kind}`}
          >
            {props.step.kind}
          </span>
          <span class="agent-monitor__step-title">{props.step.title}</span>
        </span>
      </button>
    </li>
  );
};

// ---------------------------------------------------------------------------
// Right column — inspector
// ---------------------------------------------------------------------------

export const AgentInspector: Component<{
  agent: AgentRow | null;
  data: AgentInspectorData | null;
  onStop: (id: string) => void;
  /**
   * F-449: optional promote-to-pane handler. When supplied AND the selected
   * row is a background agent or sub-agent (i.e. promotable), an additional
   * `PROMOTE TO PANE` action renders alongside `STOP AGENT`. The button is
   * idempotent — clicking on an already-promoted row resolves silently
   * because the backend's `promote_background_agent` is a no-op on unknown
   * ids (`forge-session::bg_agents::promote`).
   */
  onPromote?: (id: string) => void;
}> = (props) => {
  const isPromotable = (): boolean => {
    const row = props.agent;
    if (!row) return false;
    return row.category === 'background' || row.category === 'sub-agent';
  };
  return (
    <aside class="agent-monitor__inspector" aria-label="Inspector">
      <Show
        when={props.agent && props.data}
        fallback={<p class="agent-monitor__empty">// select an agent</p>}
      >
        <section>
          <h3 class="agent-monitor__inspector-heading">Definition</h3>
          <dl class="agent-monitor__def">
            <dt>name</dt>
            <dd>{props.agent!.name}</dd>
            <dt>source</dt>
            <dd>{props.data!.source ?? '—'}</dd>
            <dt>provider</dt>
            <dd>{props.data!.provider ?? '—'}</dd>
            <dt>model</dt>
            <dd>{props.data!.model ?? '—'}</dd>
            <dt>isolation</dt>
            <dd>{props.data!.isolation ?? '—'}</dd>
          </dl>
        </section>
        <section>
          <h3 class="agent-monitor__inspector-heading">Allowed tools</h3>
          <Show
            when={props.data!.allowedTools.length > 0}
            fallback={<p class="agent-monitor__empty">// none</p>}
          >
            <ul class="agent-monitor__pills">
              <For each={props.data!.allowedTools}>
                {(tool) => <li class="agent-monitor__pill">{tool}</li>}
              </For>
            </ul>
          </Show>
        </section>
        <section>
          <h3 class="agent-monitor__inspector-heading">Allowed paths</h3>
          <Show
            when={props.data!.allowedPaths.length > 0}
            fallback={<p class="agent-monitor__empty">// none</p>}
          >
            <ul class="agent-monitor__pills">
              <For each={props.data!.allowedPaths}>
                {(p) => (
                  <li class="agent-monitor__pill agent-monitor__pill--mono">{p}</li>
                )}
              </For>
            </ul>
          </Show>
        </section>
        <section>
          <h3 class="agent-monitor__inspector-heading">Resource usage</h3>
          <ul class="agent-monitor__pills">
            <li class="agent-monitor__pill">
              cpu {props.data!.resources.cpu != null ? `${props.data!.resources.cpu.toFixed(1)}%` : '—'}
            </li>
            <li class="agent-monitor__pill">
              rss {props.data!.resources.rss != null ? `${props.data!.resources.rss}MB` : '—'}
            </li>
            <li class="agent-monitor__pill">
              fds {props.data!.resources.fds != null ? props.data!.resources.fds : '—'}
            </li>
          </ul>
        </section>
        <section class="agent-monitor__actions">
          <Button
            variant="danger"
            class="agent-monitor__stop"
            onClick={() => props.onStop(props.agent!.id)}
          >
            STOP AGENT
          </Button>
          <Show when={props.onPromote && isPromotable()}>
            <Button
              variant="ghost"
              class="agent-monitor__promote"
              onClick={() => props.onPromote!(props.agent!.id)}
            >
              PROMOTE TO PANE
            </Button>
          </Show>
        </section>
      </Show>
    </aside>
  );
};

// ---------------------------------------------------------------------------
// Step-detail drawer
// ---------------------------------------------------------------------------

export const StepDrawer: Component<{
  step: AgentStep | null;
  onClose: () => void;
}> = (props) => {
  const onKey = (e: KeyboardEvent) => {
    if (e.key === 'Escape') props.onClose();
  };

  createEffect(() => {
    if (props.step) {
      document.addEventListener('keydown', onKey);
      onCleanup(() => document.removeEventListener('keydown', onKey));
    }
  });

  // F-402: extract the dialog body so its mount/unmount drives useFocusTrap —
  // focus lands inside the drawer on open, Tab traps within, focus restores
  // to the previously-focused element on close.
  const Body: Component<{ step: AgentStep }> = (p) => {
    let dialogRef: HTMLDivElement | undefined;
    useFocusTrap(() => dialogRef);
    return (
      <div
        ref={dialogRef}
        class="agent-monitor__drawer"
        role="dialog"
        aria-label="Step detail"
        aria-modal="true"
      >
        <header class="agent-monitor__drawer-head">
          <h3>{p.step.title}</h3>
          <IconButton
            size="sm"
            class="agent-monitor__drawer-close"
            label="Close step detail"
            onClick={props.onClose}
            icon={'×'}
          />
        </header>
        <dl class="agent-monitor__def">
          <dt>kind</dt>
          <dd>{p.step.kind}</dd>
          <dt>status</dt>
          <dd>{p.step.status}</dd>
          <dt>started</dt>
          <dd>{p.step.startedAt}</dd>
        </dl>
        <Show when={p.step.preview}>
          <pre class="agent-monitor__drawer-preview">{p.step.preview}</pre>
        </Show>
      </div>
    );
  };

  return (
    <Show when={props.step}>
      {(step) => <Body step={step()} />}
    </Show>
  );
};

// ---------------------------------------------------------------------------
// Route shell — assembles columns + data sources
// ---------------------------------------------------------------------------

// F-401: surface backend failure as a distinct resource state. Previously
// this swallowed all `list_background_agents` rejections and returned `[]`,
// which collapsed "backend failed" into "zero agents" and violated
// `component-principles.md`'s four-state coverage rule. Now the rejection
// propagates to the SolidJS resource's `error` field, and the list column
// renders `AGENT LIST UNAVAILABLE <detail>` per `agent-monitor.md §9.4`.
async function fetchBgAgents(sessionId: string | null): Promise<BgAgentSummary[]> {
  if (!sessionId) return [];
  const result = await listBackgroundAgents(sessionId as import('@forge/ipc').SessionId);
  return Array.isArray(result) ? result : [];
}

/**
 * F-449: promote a background or sub-agent into the session's active chat
 * pane. Wraps the existing `promote_background_agent` Tauri command (which
 * just removes the id from the tracked set on the backend) so the
 * Inspector's PROMOTE TO PANE button stays idempotent on stale ids.
 *
 * The orchestrator instance keeps running under the same `AgentInstanceId`
 * — promotion is a UX re-attribution. The webview side responds by
 * navigating to the session window so the promoted agent's transcript is
 * back in front of the user, as called for in §9.3 ("Promote to pane —
 * background-agent only; hoists into a dedicated pane").
 */
export async function promoteAgentInstance(
  deps: { promoteBackgroundAgent: typeof promoteBackgroundAgent },
  sessionId: string | null,
  instanceId: string,
): Promise<'ok' | 'skipped' | 'failed'> {
  if (!sessionId) return 'skipped';
  try {
    await deps.promoteBackgroundAgent(
      sessionId as import('@forge/ipc').SessionId,
      instanceId,
    );
    return 'ok';
  } catch {
    return 'failed';
  }
}

/**
 * Stop a running agent instance by calling the `stop_background_agent`
 * Tauri command (F-138). Exported so the component test can exercise the
 * wiring without mounting the full `AgentMonitor` route shell (which needs
 * a router, live session event bus, and a resource loader).
 *
 * Returns a discriminated result instead of throwing so the Inspector's
 * click handler can stay idempotent — a stale id, a missing session id, or
 * an invoke rejection all collapse to `skipped` / `failed` without
 * bubbling.
 *
 * The Tauri boundary sends `{ sessionId, instanceId }`; the backend's
 * `stop_background_agent` command authorizes via `require_window_label`
 * and delegates to `Orchestrator::stop`, which is a silent no-op on
 * stale ids — so the "click Stop on an already-terminal row" race is
 * provably safe even end-to-end.
 */
export async function stopAgentInstance(
  deps: { stopBackgroundAgent: typeof stopBackgroundAgent },
  sessionId: string | null,
  instanceId: string,
): Promise<'ok' | 'skipped' | 'failed'> {
  if (!sessionId) return 'skipped';
  try {
    await deps.stopBackgroundAgent(
      sessionId as import('@forge/ipc').SessionId,
      instanceId,
    );
    return 'ok';
  } catch {
    return 'failed';
  }
}

/** Inspector stub — real data lands when the backend exposes def metadata.
 *
 * F-152: the `resources` field accepts the live snapshot from
 * `resourcesByAgent` so the pills show real sampler output instead of
 * placeholder dashes. Passing `undefined` preserves pre-F-152 behavior
 * (pills show `—`), which is exactly what happens when an instance is
 * untracked or terminated.
 *
 * The snapshot carries `rss` in bytes (Rust wire shape `rss_bytes`); the
 * pill renders the label "MB", so we divide by 1024*1024 here to keep the
 * display units honest. `cpu` is already a percent (0..=100); `fds` is a
 * raw count.
 */
export function inspectorStub(
  row: AgentRow,
  resources?: ResourceSnapshot,
): AgentInspectorData {
  const data: AgentInspectorData = {
    allowedTools: [],
    allowedPaths: [],
    resources: {},
  };
  if (resources) {
    if (resources.cpu !== undefined) data.resources.cpu = resources.cpu;
    if (resources.rss !== undefined) {
      data.resources.rss = Math.round(resources.rss / (1024 * 1024));
    }
    if (resources.fds !== undefined) data.resources.fds = resources.fds;
  }
  if (row.model) data.model = row.model;
  return data;
}

export const AgentMonitor: Component = () => {
  const params = useParams<{ id?: string }>();
  const sessionId = () => params.id ?? null;
  // F-153: honor `?instance=<id>` from the status-bar badge's nav so the
  // monitor pre-selects the clicked row instead of the default first row.
  // Param absence falls through to the auto-select-first effect below.
  const [searchParams] = useSearchParams<{ instance?: string }>();
  const navigate = useNavigate();

  const [filter, setFilter] = createSignal<AgentFilter>('all');
  const [selectedId, setSelectedId] = createSignal<string | null>(null);
  const [openStep, setOpenStep] = createSignal<AgentStep | null>(null);
  // F-449: live wall-clock for the trace-toolbar's elapsed cell. Ticks
  // every second; cleaned up on unmount so the route doesn't leak the
  // interval across navigations.
  const [now, setNow] = createSignal<number>(Date.now());
  const tick = setInterval(() => setNow(Date.now()), 1000);
  onCleanup(() => clearInterval(tick));

  // Background agents — refetched when the session id changes. F-401: no
  // `initialValue` here so the resource reports `loading` while the first
  // fetch is in flight; otherwise loading collapses into the ready-empty
  // branch and the four-state coverage regresses.
  const [bgAgents, { refetch }] = createResource(sessionId, fetchBgAgents);

  // Live rows observed via session:event — session-root and sub-agents. The
  // helper upserts a session row the first time `StepStarted.instance_id`
  // arrives, so the empty state transitions to a populated trace without a
  // hardcoded placeholder competing with the live id.
  const [subAgents, setSubAgents] = createSignal<AgentRow[]>([]);
  const [stepsByAgent, setStepsByAgent] = createSignal<Record<string, AgentStep[]>>({});
  // F-152: per-instance cpu / rss / fds folded from `resource_sample`
  // events. A completion event clears the entry so the pills reset.
  const [resourcesByAgent, setResourcesByAgent] = createSignal<
    Record<string, ResourceSnapshot>
  >({});
  // F-449: per-instance unique tool names folded from `tool_call_started`
  // events. Drives the trace-toolbar's tools-used summary.
  const [toolsByAgent, setToolsByAgent] = createSignal<Record<string, string[]>>({});

  const rows = createMemo<AgentRow[]>(() => {
    // F-401: reading `bgAgents()` while the resource is in its `errored`
    // state re-throws in the reactive scope. Gate on the resource's state
    // so a rejected fetch stays observable via `listStatus` without
    // crashing the route; sub-agents from session events still render.
    const fetched = bgAgents.state === 'ready' ? bgAgents() ?? [] : [];
    return [...fetched.map(toBgRow), ...subAgents()];
  });

  // F-401: discriminated async state for the list column. Loading only shows
  // until the resource has a first outcome; error wins over empty so a
  // rejected fetch never renders as "zero agents". Session-event-driven
  // sub-agents still render when they arrive in the error branch — those
  // rows live in `subAgents()` independent of the bg-agent fetch.
  const listStatus = createMemo<AgentListStatus>(() => {
    if (bgAgents.loading) return { kind: 'loading' };
    const err = bgAgents.error;
    if (err) {
      return {
        kind: 'error',
        detail: err instanceof Error ? `Error: ${err.message}` : String(err),
        onRetry: () => void refetch(),
      };
    }
    return { kind: 'ready' };
  });

  createMountedSubscription(() => onSessionEvent((payload) => applyEvent(payload)));

  function applyEvent(payload: SessionEventPayload) {
    const ev = payload.event as Record<string, unknown> | null;
    if (ev && typeof ev === 'object' && ev['type'] === 'background_agent_completed') {
      void refetch();
    }
    // All other variants fold through the pure helper — keeps the logic
    // testable without mounting the route.
    const snapshot: LiveAgentState = {
      subAgents: subAgents(),
      stepsByAgent: stepsByAgent(),
      resourcesByAgent: resourcesByAgent(),
      toolsByAgent: toolsByAgent(),
    };
    const next = applyEventToState(snapshot, payload);
    if (next === snapshot) return;
    if (next.subAgents !== snapshot.subAgents) setSubAgents(next.subAgents);
    if (next.stepsByAgent !== snapshot.stepsByAgent) setStepsByAgent(next.stepsByAgent);
    if (next.resourcesByAgent !== snapshot.resourcesByAgent) {
      setResourcesByAgent(next.resourcesByAgent);
    }
    if (next.toolsByAgent !== snapshot.toolsByAgent) {
      setToolsByAgent(next.toolsByAgent);
    }
  }

  // Auto-select the session root so the trace column isn't empty on mount.
  // F-153: when `?instance=<id>` is present, prefer selecting that row.
  // Falls back to the first row when the requested instance isn't in the
  // current list (stale id, navigation from a different session, etc.).
  createEffect(
    on(rows, (list) => {
      if (selectedId() || list.length === 0) return;
      const wanted = searchParams.instance;
      if (wanted) {
        const match = list.find((r) => r.id === wanted);
        if (match) {
          setSelectedId(match.id);
          return;
        }
      }
      const first = list[0];
      if (first) setSelectedId(first.id);
    }),
  );

  const selected = createMemo(() =>
    rows().find((r) => r.id === selectedId()) ?? null,
  );
  const selectedSteps = createMemo<AgentStep[]>(() => {
    const id = selectedId();
    if (!id) return [];
    return stepsByAgent()[id] ?? [];
  });
  // F-449: tools-used + spawned-by source for the trace toolbar.
  const selectedTools = createMemo<string[]>(() => {
    const id = selectedId();
    if (!id) return [];
    return toolsByAgent()[id] ?? [];
  });
  const selectedSpawnedBy = createMemo<string | undefined>(() => {
    const row = selected();
    if (!row || !row.parentId) return undefined;
    return spawnedByLabel(row.parentId, rows());
  });
  const inspector = createMemo<AgentInspectorData | null>(() => {
    const row = selected();
    if (!row) return null;
    const resources = resourcesByAgent()[row.id];
    return inspectorStub(row, resources);
  });

  const onStop = async (id: string) => {
    // Wire the inspector's Stop button to the `stop_background_agent` Tauri
    // command (F-140). F-138 already ships this command end-to-end — here
    // we route the Agent Monitor's Stop click through it so clicking the
    // button drives `forge_agents::Orchestrator::stop(id)`. The
    // orchestrator's resulting `AgentEvent::Completed` is forwarded by the
    // per-session background registry as `Event::BackgroundAgentCompleted`
    // on `session:event`; `applyEventToState` folds that into the row's
    // terminal variant so the UI doesn't need any extra reconciliation.
    //
    // Failures (stale id, network) are swallowed: clicking Stop on a row
    // that has already transitioned to terminal must not surface an error
    // — the orchestrator itself is idempotent on unknown ids, but a
    // concurrent terminal transition between render and click is the most
    // likely "failure" path and should be invisible to the user.
    await stopAgentInstance({ stopBackgroundAgent }, sessionId(), id);
  };

  // F-449: PROMOTE TO PANE wires the existing `promote_background_agent`
  // IPC + navigates back to the session window so the promoted agent's
  // transcript is in front of the user. The IPC is itself idempotent on
  // unknown ids (`forge-session::bg_agents::promote`) — clicking on an
  // already-promoted row resolves silently. After the call returns we
  // refetch the bg-agent list so the row drops out of the
  // `background`-filtered view; sub-agent rows live in `subAgents()` and
  // stay so their trace history remains inspectable.
  const onPromote = async (id: string) => {
    const sid = sessionId();
    const result = await promoteAgentInstance(
      { promoteBackgroundAgent },
      sid,
      id,
    );
    if (result === 'ok' || result === 'failed') {
      // Refetch even on failure: the most likely failure path is "already
      // promoted on the backend, race in flight" — the bg list is now the
      // source of truth, so re-syncing avoids a stale row staying visible.
      void refetch();
    }
    if (result === 'ok' && sid) {
      navigate(`/sessions/${sid}?promoted=${id}`);
    }
  };

  return (
    <main class="agent-monitor">
      <AgentList
        rows={rows()}
        filter={filter()}
        onFilter={setFilter}
        selectedId={selectedId()}
        onSelect={setSelectedId}
        status={listStatus()}
      />
      <AgentTrace
        agent={selected()}
        steps={selectedSteps()}
        onStepClick={setOpenStep}
        tools={selectedTools()}
        spawnedBy={selectedSpawnedBy()}
        now={now()}
      />
      <AgentInspector
        agent={selected()}
        data={inspector()}
        onStop={onStop}
        onPromote={onPromote}
      />
      <StepDrawer step={openStep()} onClose={() => setOpenStep(null)} />
    </main>
  );
};

function normaliseKind(raw: string): StepKind {
  const k = raw.toLowerCase();
  if (
    k === 'plan' ||
    k === 'tool' ||
    k === 'mcp' ||
    k === 'model' ||
    k === 'wait' ||
    k === 'spawn'
  ) {
    return k;
  }
  return 'model';
}

function outcomeOf(raw: unknown): 'ok' | 'error' {
  if (raw && typeof raw === 'object') {
    const status = (raw as { status?: unknown }).status;
    if (status === 'error') return 'error';
  }
  return 'ok';
}
