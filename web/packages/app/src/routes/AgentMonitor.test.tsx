import { describe, expect, it, vi } from 'vitest';
import { cleanup, render, fireEvent, waitFor } from '@solidjs/testing-library';
import { createSignal } from 'solid-js';

const { listenMock } = vi.hoisted(() => ({
  listenMock: vi.fn(),
}));

vi.mock('@tauri-apps/api/event', () => ({
  listen: listenMock,
}));

import { MemoryRouter, Route, createMemoryHistory } from '@solidjs/router';
import {
  AgentInspector,
  AgentList,
  AgentMonitor,
  AgentTrace,
  AgentTraceToolbar,
  applyEventToState,
  filterAgents,
  formatElapsed,
  inspectorStub,
  promoteAgentInstance,
  sortAgents,
  spawnedByLabel,
  StepDrawer,
  stopAgentInstance,
  type AgentInspectorData,
  type AgentRow,
  type AgentStep,
  type LiveAgentState,
} from './AgentMonitor';
import { setInvokeForTesting } from '../lib/tauri';
import { afterEach } from 'vitest';

afterEach(() => cleanup());

const row = (over: Partial<AgentRow> = {}): AgentRow => ({
  id: 'a1',
  name: 'coder',
  category: 'sub-agent',
  state: 'running',
  progress: 0.4,
  ...over,
});

const step = (over: Partial<AgentStep> = {}): AgentStep => ({
  id: 's1',
  kind: 'tool',
  title: 'fs.read readme.txt',
  status: 'running',
  startedAt: '2026-04-20T12:00:00Z',
  ...over,
});

const inspector = (over: Partial<AgentInspectorData> = {}): AgentInspectorData => ({
  allowedTools: [],
  allowedPaths: [],
  resources: {},
  ...over,
});

// ---------------------------------------------------------------------------
// Filter + sort helpers — exercised directly so the invariant is pinned
// independent of rendering.
// ---------------------------------------------------------------------------

describe('AgentMonitor: filter + sort helpers', () => {
  const rows: AgentRow[] = [
    row({ id: 'done', state: 'done', progress: 1, startedAt: '2026-04-20T10:00Z' }),
    row({ id: 'run', state: 'running', progress: 0.5, startedAt: '2026-04-20T11:00Z' }),
    row({ id: 'err', state: 'error', progress: 1, startedAt: '2026-04-20T10:30Z' }),
    row({
      id: 'bg',
      category: 'background',
      state: 'queued',
      progress: 0,
      startedAt: '2026-04-20T09:00Z',
    }),
  ];

  it('filters "running" to only running rows', () => {
    const out = filterAgents(rows, 'running');
    expect(out.map((r) => r.id)).toEqual(['run']);
  });

  it('filters "background" to only the background category', () => {
    const out = filterAgents(rows, 'background');
    expect(out.map((r) => r.id)).toEqual(['bg']);
  });

  it('filters "failed" to only errored rows', () => {
    const out = filterAgents(rows, 'failed');
    expect(out.map((r) => r.id)).toEqual(['err']);
  });

  it('sorts by state (running → queued → error → done), most recent first within group', () => {
    const sorted = sortAgents(rows);
    // running first, then queued (bg), then error, then done
    expect(sorted.map((r) => r.id)).toEqual(['run', 'bg', 'err', 'done']);
  });
});

// ---------------------------------------------------------------------------
// Live event folding — the path from `session:event` payload to rendered state
// ---------------------------------------------------------------------------

describe('applyEventToState', () => {
  const empty: LiveAgentState = {
    subAgents: [],
    stepsByAgent: {},
    resourcesByAgent: {},
    toolsByAgent: {},
  };

  it('upserts a session row the first time StepStarted arrives with an instance id', () => {
    const after = applyEventToState(empty, {
      event: {
        type: 'step_started',
        step_id: 'step-1',
        kind: 'model',
        instance_id: 'abc123def456',
        started_at: '2026-04-20T12:00:00Z',
      },
    });

    expect(after.subAgents).toHaveLength(1);
    expect(after.subAgents[0]?.id).toBe('abc123def456');
    expect(after.subAgents[0]?.category).toBe('session');
    expect(after.stepsByAgent['abc123def456']).toHaveLength(1);
    expect(after.stepsByAgent['abc123def456']?.[0]?.kind).toBe('model');
  });

  it('does not duplicate the row when further StepStarted events arrive for the same instance id', () => {
    const s1 = applyEventToState(empty, {
      event: {
        type: 'step_started',
        step_id: 'step-1',
        kind: 'model',
        instance_id: 'inst-1',
      },
    });
    const s2 = applyEventToState(s1, {
      event: {
        type: 'step_started',
        step_id: 'step-2',
        kind: 'tool',
        instance_id: 'inst-1',
      },
    });

    expect(s2.subAgents).toHaveLength(1);
    expect(s2.stepsByAgent['inst-1']).toHaveLength(2);
  });

  it('adds a sub-agent row on SubAgentSpawned', () => {
    const after = applyEventToState(empty, {
      event: { type: 'sub_agent_spawned', parent: 'p', child: 'c' },
    });

    expect(after.subAgents).toHaveLength(1);
    expect(after.subAgents[0]?.category).toBe('sub-agent');
    expect(after.subAgents[0]?.parentId).toBe('p');
  });

  it('closes a step to done on StepFinished with ok outcome', () => {
    const started = applyEventToState(empty, {
      event: {
        type: 'step_started',
        step_id: 's',
        kind: 'model',
        instance_id: 'inst',
      },
    });
    const finished = applyEventToState(started, {
      event: {
        type: 'step_finished',
        step_id: 's',
        outcome: { status: 'ok' },
      },
    });

    expect(finished.stepsByAgent['inst']?.[0]?.status).toBe('done');
  });

  it('closes a step to error on StepFinished with error outcome', () => {
    const started = applyEventToState(empty, {
      event: {
        type: 'step_started',
        step_id: 's',
        kind: 'tool',
        instance_id: 'inst',
      },
    });
    const finished = applyEventToState(started, {
      event: {
        type: 'step_finished',
        step_id: 's',
        outcome: { status: 'error', reason: 'nope' },
      },
    });

    expect(finished.stepsByAgent['inst']?.[0]?.status).toBe('error');
  });

  it('returns the same reference for an unrecognised event type', () => {
    const after = applyEventToState(empty, {
      event: { type: 'user_message', id: 'm', text: 'hi' },
    });
    expect(after).toBe(empty);
  });

  it('flips a running sub-agent row to done on background_agent_completed', () => {
    // F-140: Stop button → `stop_background_agent` → orchestrator.stop →
    // `BackgroundAgentCompleted` on `session:event`. The row must stay
    // visible but transition to a terminal variant so the user can still
    // inspect the trace.
    const spawned = applyEventToState(empty, {
      event: { type: 'sub_agent_spawned', parent: 'p', child: 'c' },
    });
    expect(spawned.subAgents[0]?.state).toBe('running');

    const completed = applyEventToState(spawned, {
      event: { type: 'background_agent_completed', id: 'c' },
    });
    const row = completed.subAgents.find((r) => r.id === 'c');
    expect(row?.state).toBe('done');
    expect(row?.progress).toBe(1);
  });

  it('is a no-op when the completed id has no tracked sub-agent row', () => {
    const after = applyEventToState(empty, {
      event: { type: 'background_agent_completed', id: 'unknown' },
    });
    expect(after).toBe(empty);
  });

  it('does not re-flip an already-terminal row', () => {
    const spawned = applyEventToState(empty, {
      event: { type: 'sub_agent_spawned', parent: 'p', child: 'c' },
    });
    const first = applyEventToState(spawned, {
      event: { type: 'background_agent_completed', id: 'c' },
    });
    const second = applyEventToState(first, {
      event: { type: 'background_agent_completed', id: 'c' },
    });
    // Stable reference proves we did not allocate a new state tree for a
    // redundant completion event.
    expect(second).toBe(first);
  });

  // F-152: resource_sample fold — populates the pills, clears on termination.

  it('upserts a resource snapshot on resource_sample', () => {
    const after = applyEventToState(empty, {
      event: {
        type: 'resource_sample',
        instance_id: 'inst-1',
        cpu_pct: 12.5,
        rss_bytes: 4 * 1024 * 1024,
        fd_count: 18,
        sampled_at: '2026-04-20T12:00:00Z',
      },
    });
    expect(after.resourcesByAgent['inst-1']).toEqual({
      cpu: 12.5,
      rss: 4 * 1024 * 1024,
      fds: 18,
    });
  });

  it('preserves missing fields as undefined on resource_sample', () => {
    // Partial platform probes send `null` for fields they could not read;
    // those survive as undefined in the snapshot so the Inspector renders
    // the `—` placeholder without falsely reading the pill value as zero.
    const after = applyEventToState(empty, {
      event: {
        type: 'resource_sample',
        instance_id: 'inst-2',
        cpu_pct: 5.0,
        rss_bytes: null,
        fd_count: null,
        sampled_at: '2026-04-20T12:00:00Z',
      },
    });
    const snapshot = after.resourcesByAgent['inst-2'];
    expect(snapshot).toBeDefined();
    expect(snapshot?.cpu).toBe(5.0);
    expect(snapshot?.rss).toBeUndefined();
    expect(snapshot?.fds).toBeUndefined();
  });

  it('overwrites an earlier resource snapshot for the same instance', () => {
    const first = applyEventToState(empty, {
      event: {
        type: 'resource_sample',
        instance_id: 'inst-3',
        cpu_pct: 1.0,
        rss_bytes: 1000,
        fd_count: 1,
        sampled_at: '2026-04-20T12:00:00Z',
      },
    });
    const second = applyEventToState(first, {
      event: {
        type: 'resource_sample',
        instance_id: 'inst-3',
        cpu_pct: 9.0,
        rss_bytes: 9000,
        fd_count: 9,
        sampled_at: '2026-04-20T12:00:01Z',
      },
    });
    expect(second.resourcesByAgent['inst-3']).toEqual({
      cpu: 9.0,
      rss: 9000,
      fds: 9,
    });
  });

  it('keeps snapshots for other instances when a new one arrives', () => {
    const a = applyEventToState(empty, {
      event: {
        type: 'resource_sample',
        instance_id: 'a',
        cpu_pct: 1,
        rss_bytes: 1,
        fd_count: 1,
        sampled_at: '2026-04-20T12:00:00Z',
      },
    });
    const b = applyEventToState(a, {
      event: {
        type: 'resource_sample',
        instance_id: 'b',
        cpu_pct: 2,
        rss_bytes: 2,
        fd_count: 2,
        sampled_at: '2026-04-20T12:00:01Z',
      },
    });
    expect(Object.keys(b.resourcesByAgent)).toEqual(['a', 'b']);
  });

  it('drops the resource snapshot on background_agent_completed', () => {
    // DoD: "pills clear back to '—' when the instance terminates". The
    // Inspector reads from `resourcesByAgent`, so clearing the entry is
    // the mechanism.
    const populated = applyEventToState(empty, {
      event: {
        type: 'resource_sample',
        instance_id: 'term',
        cpu_pct: 7,
        rss_bytes: 77,
        fd_count: 777,
        sampled_at: '2026-04-20T12:00:00Z',
      },
    });
    expect(populated.resourcesByAgent['term']).toBeDefined();
    const spawned = applyEventToState(populated, {
      event: { type: 'sub_agent_spawned', parent: 'p', child: 'term' },
    });
    const completed = applyEventToState(spawned, {
      event: { type: 'background_agent_completed', id: 'term' },
    });
    expect(completed.resourcesByAgent['term']).toBeUndefined();
  });

  it('is a no-op on malformed resource_sample (missing instance_id)', () => {
    const after = applyEventToState(empty, {
      event: {
        type: 'resource_sample',
        cpu_pct: 1,
        rss_bytes: 1,
        fd_count: 1,
        sampled_at: '2026-04-20T12:00:00Z',
      },
    });
    expect(after).toBe(empty);
  });
});

// ---------------------------------------------------------------------------
// inspectorStub resource mapping (F-152)
// ---------------------------------------------------------------------------

describe('inspectorStub', () => {
  it('returns em-dash placeholders when no resources are supplied', () => {
    const data = inspectorStub(row());
    expect(data.resources).toEqual({});
  });

  it('converts rss_bytes to MB for the pill label', () => {
    // The pill template renders `${rss}MB` — the backend emits bytes, so
    // the adapter must convert. 10 MiB should render as 10.
    const data = inspectorStub(row(), {
      cpu: 12.5,
      rss: 10 * 1024 * 1024,
      fds: 17,
    });
    expect(data.resources.cpu).toBe(12.5);
    expect(data.resources.rss).toBe(10);
    expect(data.resources.fds).toBe(17);
  });

  it('omits a field when the snapshot value is undefined', () => {
    // A partial platform probe yields e.g. `{cpu: 5}` with rss/fds absent.
    // The pill for the missing fields must render `—`, not `0MB` / `0`.
    const data = inspectorStub(row(), { cpu: 5 });
    expect(data.resources.cpu).toBe(5);
    expect(data.resources.rss).toBeUndefined();
    expect(data.resources.fds).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// Left column: list
// ---------------------------------------------------------------------------

describe('<AgentList>', () => {
  it('renders one tab per filter category (5 total)', () => {
    const { getAllByRole } = render(() => (
      <AgentList
        rows={[row()]}
        filter="all"
        onFilter={() => {}}
        selectedId={null}
        onSelect={() => {}}
      />
    ));
    const tabs = getAllByRole('tab');
    expect(tabs).toHaveLength(5);
    expect(tabs.map((t) => t.textContent?.trim())).toEqual([
      'all',
      'running',
      'background',
      'session',
      'failed',
    ]);
  });

  it('invokes onFilter when a filter tab is clicked', () => {
    const onFilter = vi.fn();
    const { getAllByRole } = render(() => (
      <AgentList
        rows={[]}
        filter="all"
        onFilter={onFilter}
        selectedId={null}
        onSelect={() => {}}
      />
    ));
    const tabs = getAllByRole('tab');
    const bgTab = tabs[2];
    if (!bgTab) throw new Error('expected 5 filter tabs, got fewer');
    fireEvent.click(bgTab); // background
    expect(onFilter).toHaveBeenCalledWith('background');
  });

  it('renders a per-row progress bar with width proportional to progress', () => {
    const { container } = render(() => (
      <AgentList
        rows={[row({ progress: 0.75 })]}
        filter="all"
        onFilter={() => {}}
        selectedId={null}
        onSelect={() => {}}
      />
    ));
    const fill = container.querySelector('.agent-monitor__progress-fill') as HTMLElement;
    expect(fill).toBeTruthy();
    expect(fill.style.width).toBe('75%');
  });

  it('applies the running modifier so the pulsing-ring style attaches', () => {
    const { container } = render(() => (
      <AgentList
        rows={[row({ state: 'running' })]}
        filter="all"
        onFilter={() => {}}
        selectedId={null}
        onSelect={() => {}}
      />
    ));
    const btn = container.querySelector('.agent-monitor__row--running');
    expect(btn).toBeTruthy();
    // Pulse marker is rendered inside the row name span.
    expect(container.querySelector('.agent-monitor__row-pulse')).toBeTruthy();
  });

  it('calls onSelect when a row is clicked', () => {
    const onSelect = vi.fn();
    const { getByLabelText } = render(() => (
      <AgentList
        rows={[row({ id: 'x', name: 'reviewer' })]}
        filter="all"
        onFilter={() => {}}
        selectedId={null}
        onSelect={onSelect}
      />
    ));
    fireEvent.click(getByLabelText(/Select agent reviewer/i));
    expect(onSelect).toHaveBeenCalledWith('x');
  });

  // F-416: filter tabs are the WAI-ARIA tabs pattern; each tab must carry
  // aria-controls pointing at the row-list tabpanel, and the tabpanel must
  // reciprocate via aria-labelledby on the currently-selected tab.
  describe('F-416 — filter tabs ↔ rows tabpanel association', () => {
    it('every filter tab has a non-empty aria-controls and id', () => {
      const { getAllByRole } = render(() => (
        <AgentList
          rows={[row()]}
          filter="all"
          onFilter={() => {}}
          selectedId={null}
          onSelect={() => {}}
        />
      ));
      const tabs = getAllByRole('tab');
      for (const tab of tabs) {
        expect(tab.id).toBeTruthy();
        expect(tab.getAttribute('aria-controls')).toBeTruthy();
      }
    });

    it('the rows tabpanel is labelled by the currently-selected filter tab', () => {
      const { getAllByRole, container } = render(() => (
        <AgentList
          rows={[row()]}
          filter="running"
          onFilter={() => {}}
          selectedId={null}
          onSelect={() => {}}
        />
      ));
      const tabs = getAllByRole('tab');
      const selected = tabs.find((t) => t.getAttribute('aria-selected') === 'true');
      expect(selected).toBeTruthy();
      const panel = container.querySelector('[role="tabpanel"]') as HTMLElement | null;
      expect(panel).not.toBeNull();
      expect(panel!.id).toBe(selected!.getAttribute('aria-controls'));
      expect(panel!.getAttribute('aria-labelledby')).toBe(selected!.id);
    });
  });
});

// ---------------------------------------------------------------------------
// Middle column: trace
// ---------------------------------------------------------------------------

describe('<AgentTrace>', () => {
  it('renders step chips tagged with their kind so styling can select on it', () => {
    const steps: AgentStep[] = [
      step({ id: 's1', kind: 'model', title: 'model pass' }),
      step({ id: 's2', kind: 'tool', title: 'tool call', status: 'done' }),
      step({ id: 's3', kind: 'spawn', title: 'spawn child', status: 'error' }),
    ];
    const { container } = render(() => (
      <AgentTrace agent={row()} steps={steps} onStepClick={() => {}} />
    ));
    const chips = container.querySelectorAll('.agent-monitor__step-chip');
    expect(chips).toHaveLength(3);
    const kinds = Array.from(chips).map((c) => c.getAttribute('data-kind'));
    expect(kinds).toEqual(['model', 'tool', 'spawn']);
  });

  it('renders a running-state dot so pulsing-ring CSS attaches', () => {
    const { container } = render(() => (
      <AgentTrace agent={row()} steps={[step({ status: 'running' })]} onStepClick={() => {}} />
    ));
    const dot = container.querySelector('.agent-monitor__step-dot--running');
    expect(dot).toBeTruthy();
  });

  it('invokes onStepClick with the clicked step', () => {
    const onStepClick = vi.fn();
    const s = step({ id: 'clickable', title: 'open me' });
    const { getByLabelText } = render(() => (
      <AgentTrace agent={row()} steps={[s]} onStepClick={onStepClick} />
    ));
    fireEvent.click(getByLabelText(/Open step open me/));
    expect(onStepClick).toHaveBeenCalledWith(s);
  });

  it('falls back to a placeholder when no agent is selected', () => {
    const { getByText } = render(() => (
      <AgentTrace agent={null} steps={[]} onStepClick={() => {}} />
    ));
    expect(getByText(/select an agent/i)).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// Right column: inspector
// ---------------------------------------------------------------------------

describe('<AgentInspector>', () => {
  it('renders definition/tools/paths sections and a Stop button', () => {
    const data = inspector({
      source: 'a.agents/coder.md:12',
      provider: 'anthropic',
      model: 'sonnet-4.5',
      allowedTools: ['fs.read', 'shell.exec'],
      allowedPaths: ['/workspace/**'],
      resources: { cpu: 12.5, rss: 128, fds: 4 },
    });
    const { getByText, getAllByText } = render(() => (
      <AgentInspector agent={row()} data={data} onStop={() => {}} />
    ));
    expect(getByText('Definition')).toBeTruthy();
    expect(getByText('Allowed tools')).toBeTruthy();
    expect(getByText('Allowed paths')).toBeTruthy();
    expect(getByText('Resource usage')).toBeTruthy();
    expect(getByText('STOP AGENT')).toBeTruthy();

    // Pills rendered literally.
    expect(getByText('fs.read')).toBeTruthy();
    expect(getByText('/workspace/**')).toBeTruthy();

    // Resource pills show formatted numbers.
    expect(getAllByText(/cpu\s+12.5%/i).length).toBeGreaterThan(0);
    expect(getAllByText(/rss\s+128MB/i).length).toBeGreaterThan(0);
    expect(getAllByText(/fds\s+4/i).length).toBeGreaterThan(0);
  });

  it('renders em-dashes when resource samples are unknown', () => {
    const { getByText } = render(() => (
      <AgentInspector agent={row()} data={inspector()} onStop={() => {}} />
    ));
    expect(getByText(/cpu\s+—/)).toBeTruthy();
    expect(getByText(/rss\s+—/)).toBeTruthy();
    expect(getByText(/fds\s+—/)).toBeTruthy();
  });

  it('invokes onStop with the agent id', () => {
    const onStop = vi.fn();
    const { getByText } = render(() => (
      <AgentInspector agent={row({ id: 'kill-me' })} data={inspector()} onStop={onStop} />
    ));
    fireEvent.click(getByText('STOP AGENT'));
    expect(onStop).toHaveBeenCalledWith('kill-me');
  });

  // F-449: PROMOTE TO PANE renders for promotable rows (background +
  // sub-agent), idempotent click semantics, and stays hidden for the
  // session-root row (promoting the live session pane has no meaning).
  describe('F-449 — PROMOTE TO PANE action', () => {
    it('renders the button for a sub-agent row when onPromote is supplied', () => {
      const { getByText } = render(() => (
        <AgentInspector
          agent={row({ id: 'sub-1', category: 'sub-agent' })}
          data={inspector()}
          onStop={() => {}}
          onPromote={() => {}}
        />
      ));
      expect(getByText('PROMOTE TO PANE')).toBeTruthy();
    });

    it('renders the button for a background row when onPromote is supplied', () => {
      const { getByText } = render(() => (
        <AgentInspector
          agent={row({ id: 'bg-1', category: 'background' })}
          data={inspector()}
          onStop={() => {}}
          onPromote={() => {}}
        />
      ));
      expect(getByText('PROMOTE TO PANE')).toBeTruthy();
    });

    it('hides the button for a session-root row (not promotable)', () => {
      const { queryByText } = render(() => (
        <AgentInspector
          agent={row({ id: 'session-root', category: 'session' })}
          data={inspector()}
          onStop={() => {}}
          onPromote={() => {}}
        />
      ));
      expect(queryByText('PROMOTE TO PANE')).toBeNull();
    });

    it('hides the button when onPromote is not supplied (Phase-2 callers)', () => {
      const { queryByText } = render(() => (
        <AgentInspector
          agent={row({ id: 'sub-1', category: 'sub-agent' })}
          data={inspector()}
          onStop={() => {}}
        />
      ));
      expect(queryByText('PROMOTE TO PANE')).toBeNull();
    });

    it('invokes onPromote with the agent id', () => {
      const onPromote = vi.fn();
      const { getByText } = render(() => (
        <AgentInspector
          agent={row({ id: 'promote-me', category: 'sub-agent' })}
          data={inspector()}
          onStop={() => {}}
          onPromote={onPromote}
        />
      ));
      fireEvent.click(getByText('PROMOTE TO PANE'));
      expect(onPromote).toHaveBeenCalledWith('promote-me');
    });
  });
});

// ---------------------------------------------------------------------------
// F-449 — Trace toolbar (§9.2)
//
// Ready-now subset: elapsed (live mm:ss), model label, tools-used summary,
// spawned-by relationship. Token in/out, cost, and the full step-of-M
// chip ride F-605 / F-606 prereqs and ship in a follow-up PR.
// ---------------------------------------------------------------------------

describe('formatElapsed', () => {
  it('renders mm:ss padded with leading zeros', () => {
    const start = '2026-04-20T12:00:00Z';
    const now = Date.parse(start) + 5 * 60 * 1000 + 7 * 1000; // 5m 7s
    expect(formatElapsed(start, now)).toBe('05:07');
  });

  it('renders 00:00 at the moment of start', () => {
    const start = '2026-04-20T12:00:00Z';
    expect(formatElapsed(start, Date.parse(start))).toBe('00:00');
  });

  it('clamps negative deltas to 00:00 (clock skew)', () => {
    const start = '2026-04-20T12:00:00Z';
    const now = Date.parse(start) - 1000;
    expect(formatElapsed(start, now)).toBe('00:00');
  });

  it('caps at 99:59+ for sessions over 100 minutes', () => {
    const start = '2026-04-20T12:00:00Z';
    const now = Date.parse(start) + 100 * 60 * 1000;
    expect(formatElapsed(start, now)).toBe('99:59+');
  });

  it('returns the em-dash placeholder when startedAt is missing', () => {
    expect(formatElapsed(undefined, Date.now())).toBe('—');
  });

  it('returns the em-dash placeholder when startedAt is unparseable', () => {
    expect(formatElapsed('not-a-date', Date.now())).toBe('—');
  });
});

describe('spawnedByLabel', () => {
  it('returns the parent row name when present in the rows list', () => {
    const parent = row({ id: 'p-1', name: 'orchestrator' });
    const child = row({ id: 'c-1', parentId: 'p-1' });
    expect(spawnedByLabel(child.parentId, [parent, child])).toBe('orchestrator');
  });

  it('falls back to the truncated id when the parent is not in the list', () => {
    const child = row({ id: 'c-1', parentId: 'parent12345678' });
    expect(spawnedByLabel(child.parentId, [child])).toBe('parent12');
  });

  it('returns undefined when parentId is undefined', () => {
    expect(spawnedByLabel(undefined, [])).toBeUndefined();
  });
});

describe('<AgentTraceToolbar>', () => {
  it('renders elapsed, model, and tool-count cells', () => {
    const start = '2026-04-20T12:00:00Z';
    const now = Date.parse(start) + 90 * 1000; // 1m 30s
    const { container, getByText } = render(() => (
      <AgentTraceToolbar
        agent={row({ startedAt: start, model: 'sonnet-4.5' })}
        tools={['fs.read', 'shell.exec']}
        now={now}
      />
    ));
    const cells = container.querySelectorAll(
      '.agent-monitor__trace-toolbar-cell',
    );
    expect(cells.length).toBeGreaterThanOrEqual(3);
    expect(getByText('01:30')).toBeTruthy();
    expect(getByText('sonnet-4.5')).toBeTruthy();
    expect(getByText('tools 2')).toBeTruthy();
  });

  it('renders em-dash placeholders when fields are missing', () => {
    // The base `row` helper does not set startedAt/model — pass it through
    // unchanged so `exactOptionalPropertyTypes` doesn't trip on `undefined`.
    const { container, getByText } = render(() => (
      <AgentTraceToolbar agent={row()} tools={[]} now={Date.now()} />
    ));
    const elapsed = container.querySelector(
      '.agent-monitor__trace-toolbar-cell[data-cell="elapsed"]',
    );
    expect(elapsed?.textContent).toBe('—');
    const model = container.querySelector(
      '.agent-monitor__trace-toolbar-cell[data-cell="model"]',
    );
    expect(model?.textContent).toBe('—');
    // tools 0 always renders to keep the toolbar's mono rhythm stable.
    expect(getByText('tools 0')).toBeTruthy();
  });

  it('renders the spawned-by cell only when supplied', () => {
    const { getByText, queryByText, unmount } = render(() => (
      <AgentTraceToolbar
        agent={row({ category: 'sub-agent' })}
        tools={[]}
        spawnedBy="orchestrator"
        now={Date.now()}
      />
    ));
    expect(getByText('↳ orchestrator')).toBeTruthy();
    unmount();

    const { queryByText: q2 } = render(() => (
      <AgentTraceToolbar agent={row()} tools={[]} now={Date.now()} />
    ));
    expect(q2('↳ orchestrator')).toBeNull();
    expect(queryByText('↳ orchestrator')).toBeNull();
  });

  it('is intentionally NOT a live region (elapsed cell ticks must not spam SR queue)', () => {
    const { container } = render(() => (
      <AgentTraceToolbar
        agent={row({ startedAt: '2026-04-20T12:00:00Z' })}
        tools={[]}
        now={Date.now()}
      />
    ));
    const toolbar = container.querySelector(
      '.agent-monitor__trace-toolbar',
    ) as HTMLElement | null;
    expect(toolbar).not.toBeNull();
    // No aria-live — a polite region wrapping a 1Hz-updating elapsed cell
    // would re-announce the entire toolbar every tick (WCAG anti-pattern).
    expect(toolbar!.getAttribute('aria-live')).toBeNull();
    // role="status" carries an implicit aria-live=polite, so it must be absent too.
    expect(toolbar!.getAttribute('role')).toBeNull();
  });

  it('elapsed cell text updates as `now` advances without a live region wrapping it', () => {
    const start = '2026-04-20T12:00:00Z';
    const t0 = Date.parse(start);
    const [now, setNow] = createSignal(t0 + 5_000);
    const { container } = render(() => (
      <AgentTraceToolbar
        agent={row({ startedAt: start })}
        tools={[]}
        now={now()}
      />
    ));
    const elapsedCell = container.querySelector(
      '[data-cell="elapsed"]',
    ) as HTMLElement | null;
    expect(elapsedCell).not.toBeNull();
    const before = elapsedCell!.textContent;
    setNow(t0 + 65_000);
    const after = elapsedCell!.textContent;
    expect(after).not.toBe(before);
    const toolbar = container.querySelector(
      '.agent-monitor__trace-toolbar',
    ) as HTMLElement | null;
    expect(toolbar!.getAttribute('aria-live')).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// F-449 — applyEventToState additions
//
// session_started → stamps startedAt on the session-root row.
// assistant_message → updates session-root model.
// tool_call_started → folds unique tool names into toolsByAgent['session-root'].
// sub_agent_spawned → preserves the optional `model` field on the new row.
// ---------------------------------------------------------------------------

describe('applyEventToState (F-449 chrome)', () => {
  const empty: LiveAgentState = {
    subAgents: [],
    stepsByAgent: {},
    resourcesByAgent: {},
    toolsByAgent: {},
  };

  it('upserts a session-root row from session_started.at', () => {
    const after = applyEventToState(empty, {
      event: {
        type: 'session_started',
        at: '2026-04-20T12:00:00Z',
        workspace: '/workspace',
        agent: null,
        persistence: 'on',
      },
    });
    expect(after.subAgents).toHaveLength(1);
    const root = after.subAgents[0]!;
    expect(root.id).toBe('session-root');
    expect(root.category).toBe('session');
    expect(root.startedAt).toBe('2026-04-20T12:00:00Z');
  });

  it('updates the existing session-root startedAt when session_started arrives later', () => {
    // step_started arrives first; it sets startedAt to "now". When the
    // SessionStarted event lands a moment later it must overwrite with the
    // wire-truth timestamp so the elapsed clock reads accurately.
    const stepFirst = applyEventToState(empty, {
      event: {
        type: 'step_started',
        step_id: 'step-1',
        kind: 'model',
        instance_id: 'session-root',
      },
    });
    expect(stepFirst.subAgents[0]?.startedAt).not.toBe('2026-04-20T12:00:00Z');
    const after = applyEventToState(stepFirst, {
      event: {
        type: 'session_started',
        at: '2026-04-20T12:00:00Z',
        workspace: '/workspace',
        agent: null,
        persistence: 'on',
      },
    });
    expect(after.subAgents[0]?.startedAt).toBe('2026-04-20T12:00:00Z');
  });

  it('updates the session-root model on assistant_message', () => {
    const seed = applyEventToState(empty, {
      event: {
        type: 'session_started',
        at: '2026-04-20T12:00:00Z',
        workspace: '/workspace',
        agent: null,
        persistence: 'on',
      },
    });
    const after = applyEventToState(seed, {
      event: {
        type: 'assistant_message',
        id: 'm-1',
        provider: 'anthropic',
        model: 'sonnet-4.5',
        at: '2026-04-20T12:00:01Z',
        stream_finalised: true,
        text: 'hello',
        branch_parent: null,
        branch_variant_index: 0,
      },
    });
    const root = after.subAgents.find((r) => r.id === 'session-root');
    expect(root?.model).toBe('sonnet-4.5');
  });

  it('is a no-op for assistant_message when there is no session-root row', () => {
    const after = applyEventToState(empty, {
      event: {
        type: 'assistant_message',
        id: 'm-1',
        provider: 'anthropic',
        model: 'sonnet-4.5',
        at: '2026-04-20T12:00:01Z',
        stream_finalised: true,
        text: 'hello',
        branch_parent: null,
        branch_variant_index: 0,
      },
    });
    expect(after).toBe(empty);
  });

  it('aggregates unique tool names from tool_call_started', () => {
    const a = applyEventToState(empty, {
      event: {
        type: 'tool_call_started',
        id: 'tc-1',
        msg: 'm-1',
        tool: 'fs.read',
        args: {},
        at: '2026-04-20T12:00:01Z',
        parallel_group: null,
      },
    });
    const b = applyEventToState(a, {
      event: {
        type: 'tool_call_started',
        id: 'tc-2',
        msg: 'm-1',
        tool: 'shell.exec',
        args: {},
        at: '2026-04-20T12:00:02Z',
        parallel_group: null,
      },
    });
    expect(b.toolsByAgent['session-root']).toEqual(['fs.read', 'shell.exec']);
  });

  it('does not duplicate a tool name already aggregated', () => {
    const a = applyEventToState(empty, {
      event: {
        type: 'tool_call_started',
        id: 'tc-1',
        msg: 'm-1',
        tool: 'fs.read',
        args: {},
        at: '2026-04-20T12:00:01Z',
        parallel_group: null,
      },
    });
    const b = applyEventToState(a, {
      event: {
        type: 'tool_call_started',
        id: 'tc-2',
        msg: 'm-1',
        tool: 'fs.read',
        args: {},
        at: '2026-04-20T12:00:02Z',
        parallel_group: null,
      },
    });
    expect(b).toBe(a);
  });

  it('preserves the optional model field from sub_agent_spawned on the new row', () => {
    const after = applyEventToState(empty, {
      event: {
        type: 'sub_agent_spawned',
        parent: 'p',
        child: 'c',
        from_msg: 'm',
        model: 'haiku-4.5',
      },
    });
    const sub = after.subAgents.find((r) => r.id === 'c');
    expect(sub?.model).toBe('haiku-4.5');
  });
});

// ---------------------------------------------------------------------------
// promoteAgentInstance wiring (F-449)
//
// Mirrors stopAgentInstance — exercises the promote_background_agent IPC
// boundary without mounting the route. Pins idempotency: rejection from the
// command (likely "already promoted, race") collapses to `failed` rather
// than throwing into the click handler.
// ---------------------------------------------------------------------------

describe('promoteAgentInstance wiring', () => {
  it('invokes promoteBackgroundAgent with the session + instance ids', async () => {
    const promoteBackgroundAgent = vi.fn().mockResolvedValue(undefined);
    const result = await promoteAgentInstance(
      { promoteBackgroundAgent },
      'sess-a',
      'promote-me',
    );
    expect(promoteBackgroundAgent).toHaveBeenCalledWith('sess-a', 'promote-me');
    expect(result).toBe('ok');
  });

  it('is a no-op when no session id is active', async () => {
    const promoteBackgroundAgent = vi.fn();
    const result = await promoteAgentInstance(
      { promoteBackgroundAgent },
      null,
      'whatever',
    );
    expect(promoteBackgroundAgent).not.toHaveBeenCalled();
    expect(result).toBe('skipped');
  });

  it('swallows promoteBackgroundAgent rejections so the click stays idempotent', async () => {
    const promoteBackgroundAgent = vi
      .fn()
      .mockRejectedValue(new Error('already promoted'));
    const result = await promoteAgentInstance(
      { promoteBackgroundAgent },
      'sess-a',
      'promote-me',
    );
    expect(promoteBackgroundAgent).toHaveBeenCalledTimes(1);
    expect(result).toBe('failed');
  });
});

// ---------------------------------------------------------------------------
// Step-detail drawer
// ---------------------------------------------------------------------------

describe('<StepDrawer>', () => {
  it('renders the step detail when a step is open', () => {
    const { getByText } = render(() => (
      <StepDrawer step={step({ title: 'fs.read readme.txt' })} onClose={() => {}} />
    ));
    expect(getByText('fs.read readme.txt')).toBeTruthy();
  });

  it('renders nothing when step is null', () => {
    const { queryByRole } = render(() => <StepDrawer step={null} onClose={() => {}} />);
    expect(queryByRole('dialog')).toBeNull();
  });

  it('closes on Escape keypress', () => {
    const onClose = vi.fn();
    render(() => <StepDrawer step={step()} onClose={onClose} />);
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(onClose).toHaveBeenCalled();
  });

  it('closes on the X button', () => {
    const onClose = vi.fn();
    const { getByLabelText } = render(() => (
      <StepDrawer step={step()} onClose={onClose} />
    ));
    fireEvent.click(getByLabelText(/Close step detail/i));
    expect(onClose).toHaveBeenCalled();
  });

  // F-402: dialog contract — focus lands in the drawer on open, Tab wraps
  // within, focus restores to the previously-focused element on close.
  it('moves focus into the drawer on open', async () => {
    const { findByLabelText } = render(() => (
      <StepDrawer step={step()} onClose={() => {}} />
    ));
    const closeBtn = await findByLabelText(/Close step detail/i);
    expect(document.activeElement).toBe(closeBtn);
  });

  it('traps Tab inside the drawer — Tab from last focusable cycles to first', async () => {
    const { findByRole, findByLabelText } = render(() => (
      <StepDrawer step={step()} onClose={() => {}} />
    ));
    const dialog = await findByRole('dialog');
    const closeBtn = await findByLabelText(/Close step detail/i);
    // Only the close button is focusable in the drawer, so Tab from it
    // should cycle back to itself. Assert focus is still inside the drawer.
    closeBtn.focus();
    const ev = new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true });
    closeBtn.dispatchEvent(ev);
    expect(dialog.contains(document.activeElement)).toBe(true);
  });

  it('restores focus to the prior active element when the drawer closes', async () => {
    const trigger = document.createElement('button');
    trigger.textContent = 'open step';
    document.body.appendChild(trigger);
    trigger.focus();
    expect(document.activeElement).toBe(trigger);

    const [currentStep, setCurrentStep] = createSignal<AgentStep | null>(step());
    const { queryByRole } = render(() => (
      <StepDrawer step={currentStep()} onClose={() => setCurrentStep(null)} />
    ));
    await waitFor(() => expect(queryByRole('dialog')).not.toBeNull());
    // Close by flipping the prop to null — simulates the real dismissal path.
    setCurrentStep(null);
    await waitFor(() => expect(queryByRole('dialog')).toBeNull());
    expect(document.activeElement).toBe(trigger);
    trigger.remove();
  });
});

// ---------------------------------------------------------------------------
// stopAgentInstance wiring
//
// Pins F-140's mandatory fix: the Inspector's Stop button must invoke the
// `stop_background_agent` Tauri command (F-138's authz-gated command)
// rather than no-op. The component test exercises the helper directly so
// the wiring is provable without mounting the full route shell (which needs
// a router + live session event bus to boot).
// ---------------------------------------------------------------------------
describe('stopAgentInstance wiring', () => {
  it('invokes stopBackgroundAgent with the session + instance ids', async () => {
    const stopBackgroundAgent = vi.fn().mockResolvedValue(undefined);
    const result = await stopAgentInstance({ stopBackgroundAgent }, 'sess-a', 'kill-me');
    expect(stopBackgroundAgent).toHaveBeenCalledWith('sess-a', 'kill-me');
    expect(result).toBe('ok');
  });

  it('is a no-op when no session id is active (session-root pre-subscribe)', async () => {
    const stopBackgroundAgent = vi.fn();
    const result = await stopAgentInstance({ stopBackgroundAgent }, null, 'whatever');
    expect(stopBackgroundAgent).not.toHaveBeenCalled();
    expect(result).toBe('skipped');
  });

  it('swallows stopBackgroundAgent rejections so the Stop click stays idempotent', async () => {
    const stopBackgroundAgent = vi.fn().mockRejectedValue(new Error('stale id'));
    const result = await stopAgentInstance({ stopBackgroundAgent }, 'sess-a', 'kill-me');
    expect(stopBackgroundAgent).toHaveBeenCalledTimes(1);
    expect(result).toBe('failed');
  });
});

// ---------------------------------------------------------------------------
// F-153: route-level pre-selection.
//
// The status-bar badge double-clicks / right-clicks navigate to
// `/agents/<sessionId>?instance=<id>`. The monitor must honor the `instance`
// query param as the initial `selectedId` so the trace + inspector columns
// surface the exact row the user clicked rather than the default first row.
// ---------------------------------------------------------------------------
describe('AgentMonitor — instance query-param pre-selection (F-153)', () => {
  function renderAt(path: string) {
    // `onSessionEvent` calls `listen('session:event', ...)`; under vitest the
    // mock just records the call and returns a no-op unlisten so the component
    // mounts cleanly without a Tauri runtime.
    listenMock.mockResolvedValue(() => {});
    const history = createMemoryHistory();
    history.set({ value: path });
    return render(() => (
      <MemoryRouter history={history}>
        <Route path="/agents/:id" component={AgentMonitor} />
      </MemoryRouter>
    ));
  }

  afterEach(() => {
    setInvokeForTesting(null);
    listenMock.mockReset();
  });

  it('seeds selectedId from ?instance=<id> when the row is present in the bg list', async () => {
    // `list_background_agents` returns two running rows; the instance param
    // singles out the second one. Without the pre-select, the first row
    // would auto-select on mount.
    setInvokeForTesting(
      (async (cmd: string) => {
        if (cmd === 'list_background_agents') {
          return [
            { id: 'aaaa1111', agent_name: 'writer', state: 'Running' },
            { id: 'bbbb2222', agent_name: 'reviewer', state: 'Running' },
          ];
        }
        return undefined;
      }) as never,
    );
    const { container } = renderAt('/agents/sess-a?instance=bbbb2222');

    // The trace column header renders the selected agent's name + id — both
    // halves must point at bbbb2222, not aaaa1111. Query by class rather
    // than by text because the row's name element in the list column also
    // renders "reviewer" and `findByText` would see both.
    await waitFor(() => {
      const idEl = container.querySelector(
        '.agent-monitor__trace-id',
      ) as HTMLElement | null;
      expect(idEl?.textContent).toBe('bbbb2222');
    });
    const traceHead = container.querySelector(
      '.agent-monitor__trace-name',
    ) as HTMLElement | null;
    expect(traceHead?.textContent).toBe('reviewer');
  });

  it('falls back to the first row when ?instance=<id> does not match any loaded row', async () => {
    setInvokeForTesting(
      (async (cmd: string) => {
        if (cmd === 'list_background_agents') {
          return [
            { id: 'aaaa1111', agent_name: 'writer', state: 'Running' },
          ];
        }
        return undefined;
      }) as never,
    );
    const { container } = renderAt('/agents/sess-a?instance=does-not-exist');
    await waitFor(() => {
      const traceHead = container.querySelector(
        '.agent-monitor__trace-name',
      ) as HTMLElement | null;
      expect(traceHead?.textContent).toBe('writer');
    });
  });
});
