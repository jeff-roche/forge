import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { cleanup, render, fireEvent } from '@solidjs/testing-library';
import { MemoryRouter, Route } from '@solidjs/router';
import {
  SessionsPanel,
  pillForState,
  type SessionSummary,
} from './SessionsPanel';
import { StatusPill, type StatusPillVariant } from '@forge/design';
import { setInvokeForTesting } from '../../lib/tauri';

const invokeMock = vi.fn();

const sample = (over: Partial<SessionSummary> = {}): SessionSummary => ({
  id: 'abc1',
  subject: 'refactor payments',
  state: 'active',
  persistence: 'persist',
  createdAt: '2026-04-15T10:00:00Z',
  lastEventAt: '2026-04-15T11:00:00Z',
  provider: 'ollama',
  ...over,
});

async function waitForFetch() {
  // Drain microtasks so the resource reads the mocked invoke. The typed
  // sessionList() wrapper adds one extra async hop over a raw invoke().
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

function renderPanel() {
  // The View all action navigates via `useNavigate()`, which requires a
  // router context. MemoryRouter is the same harness `Dashboard.test.tsx`
  // and the other route tests use.
  return render(() => (
    <MemoryRouter>
      <Route path="/" component={SessionsPanel} />
    </MemoryRouter>
  ));
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
  setInvokeForTesting(invokeMock as never);
});

afterEach(() => {
  setInvokeForTesting(null);
  cleanup();
});

describe('SessionsPanel — F-720', () => {
  it('renders the Sessions header label and View all action', async () => {
    invokeMock.mockResolvedValueOnce([]);
    const { findByText, findByRole } = renderPanel();
    await waitForFetch();

    expect(await findByText('Sessions')).toBeTruthy();
    const viewAll = await findByRole('button', { name: /view all/i });
    expect(viewAll).toBeTruthy();
  });

  it('renders Active and Archived chip tabs with zero-padded counts', async () => {
    invokeMock.mockResolvedValueOnce([
      sample({ id: '1aa1', state: 'active' }),
      sample({ id: '2bb2', state: 'stopped' }),
      sample({ id: '3cc3', state: 'archived' }),
    ]);
    const { findByRole } = renderPanel();
    await waitForFetch();

    const active = await findByRole('tab', { name: /active/i });
    const archived = await findByRole('tab', { name: /archived/i });
    expect(active.textContent).toMatch(/active/);
    expect(active.textContent).toMatch(/02/);
    expect(archived.textContent).toMatch(/archived/);
    expect(archived.textContent).toMatch(/01/);
  });

  it('renders the running · idle meta line when sessions exist', async () => {
    invokeMock.mockResolvedValueOnce([
      sample({ id: '1aa1', state: 'active' }),
      sample({ id: '2bb2', state: 'stopped' }),
      sample({ id: '3cc3', state: 'archived' }),
    ]);
    const { findByText } = renderPanel();
    await waitForFetch();

    const meta = await findByText(/running.*idle/i);
    expect(meta.textContent).toMatch(/1 running/);
    expect(meta.textContent).toMatch(/1 idle/);
  });

  it('shows only active/stopped rows on Active, archived on Archived', async () => {
    invokeMock.mockResolvedValueOnce([
      sample({ id: '1aa1', subject: 'alpha', state: 'active' }),
      sample({ id: '2bb2', subject: 'beta-stale', state: 'stopped' }),
      sample({ id: '3cc3', subject: 'gamma-old', state: 'archived' }),
    ]);
    const { findByRole, findByText, queryByText } = renderPanel();
    await waitForFetch();

    expect(await findByText('alpha')).toBeTruthy();
    expect(await findByText('beta-stale')).toBeTruthy();
    expect(queryByText('gamma-old')).toBeNull();

    fireEvent.click(await findByRole('tab', { name: /archived/i }));
    expect(await findByText('gamma-old')).toBeTruthy();
    expect(queryByText('alpha')).toBeNull();
  });

  it('clicking a row invokes open_session with the session id', async () => {
    invokeMock.mockResolvedValueOnce([
      sample({ id: 'xyz1', subject: 'click me', state: 'active' }),
    ]);
    const { findByLabelText } = renderPanel();
    await waitForFetch();

    const row = await findByLabelText(/open session click me/i);
    fireEvent.click(row);

    expect(invokeMock).toHaveBeenCalledWith('open_session', { id: 'xyz1' });
  });

  it('renders the empty-state copy per spec on each tab', async () => {
    invokeMock.mockResolvedValueOnce([]);
    const { findByText, findByRole } = renderPanel();
    await waitForFetch();

    expect(await findByText('No active sessions yet.')).toBeTruthy();
    fireEvent.click(await findByRole('tab', { name: /archived/i }));
    expect(await findByText('Archive is empty.')).toBeTruthy();
  });

  it("surfaces session_list failure with verbatim error detail", async () => {
    invokeMock.mockRejectedValueOnce(new Error('disk exploded'));
    const { findByText } = renderPanel();
    await waitForFetch();
    expect(await findByText("Couldn't load sessions.")).toBeTruthy();
    expect(await findByText(/disk exploded/)).toBeTruthy();
  });

  // F-079: open_session was previously a fire-and-forget `void invoke(...)`.
  // A rejection must surface user-visible feedback rather than silently
  // dropping the click.
  it('surfaces an inline error when open_session rejects', async () => {
    invokeMock
      .mockResolvedValueOnce([sample({ id: 'xyz1', subject: 'click me', state: 'active' })])
      .mockRejectedValueOnce(new Error('open denied'));

    const { findByLabelText, findByRole } = renderPanel();
    await waitForFetch();

    const row = await findByLabelText(/open session click me/i);
    fireEvent.click(row);

    await waitForFetch();
    const alert = await findByRole('alert');
    expect(alert.textContent ?? '').toMatch(/open denied/);
  });

  // F-720 DoD: pills render correct color + label per session state.
  describe('state pills — F-720 DoD', () => {
    it('maps active → streaming, stopped → idle, archived → done', () => {
      expect(pillForState('active')).toEqual({
        variant: 'streaming',
        label: 'streaming',
      });
      expect(pillForState('stopped')).toEqual({
        variant: 'idle',
        label: 'idle',
      });
      expect(pillForState('archived')).toEqual({
        variant: 'done',
        label: 'done',
      });
    });

    it('renders the streaming pill on active rows', async () => {
      invokeMock.mockResolvedValueOnce([
        sample({ id: 'a001', subject: 'live one', state: 'active' }),
      ]);
      const { findByLabelText } = renderPanel();
      await waitForFetch();
      const row = await findByLabelText(/open session live one/i);
      const pill = row.querySelector('.forge-status-pill');
      expect(pill).not.toBeNull();
      expect(pill!.classList.contains('forge-status-pill--streaming')).toBe(true);
      expect(pill!.textContent).toBe('streaming');
    });

    it('renders the idle pill on stopped rows', async () => {
      invokeMock.mockResolvedValueOnce([
        sample({ id: 's001', subject: 'stalled one', state: 'stopped' }),
      ]);
      const { findByLabelText } = renderPanel();
      await waitForFetch();
      const row = await findByLabelText(/open session stalled one/i);
      const pill = row.querySelector('.forge-status-pill');
      expect(pill).not.toBeNull();
      expect(pill!.classList.contains('forge-status-pill--idle')).toBe(true);
      expect(pill!.textContent).toBe('idle');
    });

    it('renders the done pill on archived rows', async () => {
      invokeMock.mockResolvedValueOnce([
        sample({ id: 'd001', subject: 'closed one', state: 'archived' }),
      ]);
      const { findByLabelText, findByRole } = renderPanel();
      await waitForFetch();
      fireEvent.click(await findByRole('tab', { name: /archived/i }));
      const row = await findByLabelText(/open session closed one/i);
      const pill = row.querySelector('.forge-status-pill');
      expect(pill).not.toBeNull();
      expect(pill!.classList.contains('forge-status-pill--done')).toBe(true);
      expect(pill!.textContent).toBe('done');
    });

    // The `awaiting` variant is reserved for the sub-agent approval
    // pipeline (F-740); no IPC state maps to it today, so the primitive
    // is exercised directly to satisfy the DoD's four-pill coverage.
    it.each<[StatusPillVariant, string]>([
      ['streaming', 'streaming'],
      ['awaiting', 'awaiting approval'],
      ['done', 'done'],
      ['idle', 'idle'],
    ])('StatusPill renders the %s variant verbatim', (variant, label) => {
      const { container } = render(() => (
        <StatusPill variant={variant}>{label}</StatusPill>
      ));
      const pill = container.querySelector(`.forge-status-pill--${variant}`);
      expect(pill).not.toBeNull();
      expect(pill!.textContent).toBe(label);
    });
  });

  // View all navigates to /sessions. The route isn't wired in `App.tsx`
  // yet (the full sessions list page lands later); the test mounts a
  // placeholder at `/sessions` so the navigation lands somewhere
  // observable.
  it('View all navigates to /sessions', async () => {
    invokeMock.mockResolvedValueOnce([]);
    const { findByRole, findByTestId } = render(() => (
      <MemoryRouter>
        <Route path="/" component={SessionsPanel} />
        <Route
          path="/sessions"
          component={() => <div data-testid="sessions-route">all sessions</div>}
        />
      </MemoryRouter>
    ));
    await waitForFetch();

    const viewAll = await findByRole('button', { name: /view all/i });
    fireEvent.click(viewAll);

    expect(await findByTestId('sessions-route')).toBeTruthy();
  });

  // F-416: tabs ↔ tabpanel association. Each role="tab" must reference
  // its panel via aria-controls; the matching role="tabpanel" must
  // reciprocate via aria-labelledby.
  describe('tabs ↔ tabpanel association', () => {
    it('each chip-tab carries aria-controls pointing at the active tabpanel', async () => {
      invokeMock.mockResolvedValueOnce([
        sample({ id: '1aa1', state: 'active' }),
        sample({ id: '2bb2', state: 'archived' }),
      ]);
      const { container, findByRole } = renderPanel();
      await waitForFetch();
      await findByRole('tab', { name: /active/i });

      const tabs = container.querySelectorAll<HTMLElement>('[role="tab"]');
      expect(tabs.length).toBeGreaterThanOrEqual(2);
      for (const tab of Array.from(tabs)) {
        const panelId = tab.getAttribute('aria-controls');
        expect(panelId, `tab "${tab.textContent}" missing aria-controls`).toBeTruthy();
        if (tab.getAttribute('aria-selected') === 'true') {
          const panel = document.getElementById(panelId!);
          expect(panel, `panel ${panelId} not found for selected tab`).not.toBeNull();
          expect(panel!.getAttribute('role')).toBe('tabpanel');
          expect(panel!.getAttribute('aria-labelledby')).toBe(tab.id);
        }
      }
    });
  });

  // F-416: roving tabindex on the row list. Tab enters the list at
  // whichever row is the current tab stop; arrows move focus within.
  describe('row list roving tabindex', () => {
    it('renders exactly one row with tabindex=0; the rest are tabindex=-1', async () => {
      invokeMock.mockResolvedValueOnce([
        sample({ id: '1aa1', subject: 'alpha', state: 'active' }),
        sample({ id: '2bb2', subject: 'beta', state: 'active' }),
        sample({ id: '3cc3', subject: 'gamma', state: 'active' }),
      ]);
      const { container } = renderPanel();
      await waitForFetch();

      const rows = container.querySelectorAll<HTMLElement>('.sessions__row');
      expect(rows.length).toBe(3);
      const tabStops = Array.from(rows).filter(
        (r) => r.getAttribute('tabindex') === '0',
      );
      expect(tabStops.length).toBe(1);
    });

    it('ArrowDown moves focus to the next row', async () => {
      invokeMock.mockResolvedValueOnce([
        sample({ id: '1aa1', subject: 'alpha', state: 'active' }),
        sample({ id: '2bb2', subject: 'beta', state: 'active' }),
      ]);
      const { container } = renderPanel();
      await waitForFetch();

      const rows = container.querySelectorAll<HTMLElement>('.sessions__row');
      const first = rows[0]!;
      const second = rows[1]!;
      first.focus();
      first.dispatchEvent(
        new KeyboardEvent('keydown', {
          key: 'ArrowDown',
          bubbles: true,
          cancelable: true,
        }),
      );
      expect(document.activeElement).toBe(second);
    });
  });
});
