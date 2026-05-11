// F-718: Sidebar render + active-state + count-badge tests.
//
// Mounts the component inside a MemoryRouter so `useMatch` resolves. Count
// sources are injected via the `sources` prop to keep the suite hermetic —
// the production hydration path through real IPC is covered by integration
// tests at the AppShell level.

import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, waitFor } from '@solidjs/testing-library';
import { MemoryRouter, Route, createMemoryHistory } from '@solidjs/router';
import { createSignal } from 'solid-js';
import { Sidebar } from './Sidebar';
import { setInvokeForTesting } from '../lib/tauri';

vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async () => () => undefined),
}));

afterEach(() => {
  setInvokeForTesting(null);
  cleanup();
});

const ENTRY_ORDER = [
  'sessions',
  'recent',
  'git',
  'providers',
  'skills',
  'mcp',
  'agents',
  'containers',
  'usage',
  'settings',
];

function renderAt(path: string, props: Parameters<typeof Sidebar>[0] = {}) {
  const history = createMemoryHistory();
  history.set({ value: path });
  return render(() => (
    <MemoryRouter history={history}>
      <Route path="*" component={() => <Sidebar {...props} />} />
    </MemoryRouter>
  ));
}

describe('Sidebar', () => {
  it('renders the brand block with the spec wordmark + tagline', () => {
    setInvokeForTesting((async () => undefined) as never);
    const { getByTestId } = renderAt('/', { sources: {} });
    const brand = getByTestId('sidebar-brand');
    expect(brand.textContent).toContain('Forge');
    expect(brand.textContent).toContain('any ai. one editor.');
  });

  it('renders all ten nav entries in spec order', () => {
    setInvokeForTesting((async () => undefined) as never);
    const { getAllByTestId } = renderAt('/', { sources: {} });
    const entries = getAllByTestId(/^sidebar-entry-/);
    const ids = entries.map((el) =>
      (el.getAttribute('data-testid') ?? '').replace('sidebar-entry-', ''),
    );
    expect(ids).toEqual(ENTRY_ORDER);
  });

  it('renders each entry with an icon and a label', () => {
    setInvokeForTesting((async () => undefined) as never);
    const { getByTestId } = renderAt('/', { sources: {} });
    for (const id of ENTRY_ORDER) {
      const entry = getByTestId(`sidebar-entry-${id}`);
      expect(entry.querySelector('.sidebar__icon')).not.toBeNull();
      expect(entry.querySelector('.sidebar__label')?.textContent).toBeTruthy();
    }
  });

  it('renders the three group labels in spec order', () => {
    setInvokeForTesting((async () => undefined) as never);
    const { getByTestId } = renderAt('/', { sources: {} });
    expect(getByTestId('sidebar-group-workspace').textContent).toContain('WORKSPACE');
    expect(getByTestId('sidebar-group-ai').textContent).toContain('AI');
    expect(getByTestId('sidebar-group-system').textContent).toContain('SYSTEM');
  });

  it('highlights only the Sessions row on `/`', () => {
    setInvokeForTesting((async () => undefined) as never);
    const { getByTestId } = renderAt('/', { sources: {} });
    expect(getByTestId('sidebar-entry-sessions').classList.contains('sidebar__entry--active')).toBe(true);
    expect(getByTestId('sidebar-entry-sessions').getAttribute('aria-current')).toBe('page');
    // Other rows must not steal the highlight.
    for (const id of ENTRY_ORDER.filter((x) => x !== 'sessions')) {
      expect(getByTestId(`sidebar-entry-${id}`).classList.contains('sidebar__entry--active')).toBe(false);
    }
  });

  it('highlights Usage when the route is `/usage`', () => {
    setInvokeForTesting((async () => undefined) as never);
    const { getByTestId } = renderAt('/usage', { sources: {} });
    expect(getByTestId('sidebar-entry-usage').classList.contains('sidebar__entry--active')).toBe(true);
    expect(getByTestId('sidebar-entry-sessions').classList.contains('sidebar__entry--active')).toBe(false);
  });

  it('treats a child route as a match for the parent prefix (catalog/skills)', () => {
    setInvokeForTesting((async () => undefined) as never);
    const { getByTestId } = renderAt('/catalog/skills', { sources: {} });
    expect(getByTestId('sidebar-entry-skills').classList.contains('sidebar__entry--active')).toBe(true);
    // Sibling catalog rows must not also light up.
    expect(getByTestId('sidebar-entry-mcp').classList.contains('sidebar__entry--active')).toBe(false);
    expect(getByTestId('sidebar-entry-agents').classList.contains('sidebar__entry--active')).toBe(false);
  });

  it('renders a count badge when the count source returns a positive number', () => {
    setInvokeForTesting((async () => undefined) as never);
    const { getByTestId } = renderAt('/', {
      sources: {
        sessions: () => 3,
        providers: () => 4,
      },
    });
    expect(getByTestId('sidebar-badge-sessions').textContent).toBe('3');
    expect(getByTestId('sidebar-badge-providers').textContent).toBe('4');
  });

  it('suppresses the badge when the count source returns 0', () => {
    setInvokeForTesting((async () => undefined) as never);
    const { queryByTestId } = renderAt('/', {
      sources: { sessions: () => 0 },
    });
    expect(queryByTestId('sidebar-badge-sessions')).toBeNull();
  });

  it('suppresses the badge when the count source returns undefined', () => {
    setInvokeForTesting((async () => undefined) as never);
    const { queryByTestId } = renderAt('/', {
      sources: { sessions: () => undefined },
    });
    expect(queryByTestId('sidebar-badge-sessions')).toBeNull();
  });

  it('omits the badge entirely when no count source is registered for the row', () => {
    setInvokeForTesting((async () => undefined) as never);
    const { queryByTestId } = renderAt('/', { sources: {} });
    // Settings has no IPC source by design; Git has none today either.
    expect(queryByTestId('sidebar-badge-settings')).toBeNull();
    expect(queryByTestId('sidebar-badge-git')).toBeNull();
    expect(queryByTestId('sidebar-badge-usage')).toBeNull();
  });

  it('updates the badge reactively when the count source signal changes', async () => {
    setInvokeForTesting((async () => undefined) as never);
    const [count, setCount] = createSignal<number | undefined>(undefined);
    const { queryByTestId } = renderAt('/', {
      sources: { sessions: () => count() },
    });
    expect(queryByTestId('sidebar-badge-sessions')).toBeNull();
    setCount(5);
    await waitFor(() => {
      expect(queryByTestId('sidebar-badge-sessions')?.textContent).toBe('5');
    });
  });

  it('hydrates the Sessions badge from the session_list IPC', async () => {
    setInvokeForTesting((async (cmd: string) => {
      if (cmd === 'session_list') {
        return [
          {
            id: 's1',
            subject: 'one',
            state: 'active',
            persistence: 'persist',
            createdAt: '2025-01-01T00:00:00Z',
            lastEventAt: new Date().toISOString(),
          },
          {
            id: 's2',
            subject: 'two',
            state: 'active',
            persistence: 'persist',
            createdAt: '2025-01-01T00:00:00Z',
            lastEventAt: new Date().toISOString(),
          },
          {
            id: 's3',
            subject: 'three',
            state: 'archived',
            persistence: 'persist',
            createdAt: '2025-01-01T00:00:00Z',
            lastEventAt: new Date().toISOString(),
          },
        ];
      }
      return undefined;
    }) as never);

    const { findByTestId } = renderAt('/');
    const badge = await findByTestId('sidebar-badge-sessions');
    expect(badge.textContent).toBe('2');
  });
});
