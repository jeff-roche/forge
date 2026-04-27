// F-138: StatusBar + BgAgentsBadge component tests.
//
// The badge listens to the F-137 `BackgroundAgentStarted` /
// `BackgroundAgentCompleted` events (forwarded onto `session:event`) to flip
// its running-count and fire the notification configured in
// `settings.notifications.bg_agents`. The tests drive the component through
// injectable seams:
//
//  - `listBackgroundAgents` prop → initial snapshot, post-reconnect recovery.
//  - `promoteBackgroundAgent` / `stopBackgroundAgent` props → popover actions.
//  - `subscribe` prop → the IPC bus handler that fires `started` / `completed`.
//  - `notificationAdapter` prop → `{ toast, os }` doubles so we never touch
//    the real `@tauri-apps/plugin-notification` or the global `pushToast`
//    queue in jsdom.
//
// The DoD pins:
//   - badge renders "N bg" when count > 0
//   - click opens a popover listing running agents + Promote/Stop
//   - Promote/Stop call their respective IPCs with the right args
//   - completion event fires the configured notification
//     (toast | os | both | silent)

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';
import { MemoryRouter, Route, createMemoryHistory } from '@solidjs/router';
import type { BgAgentSummary, SessionId } from '@forge/ipc';
import { StatusBar } from './StatusBar';
import type {
  BgAgentsSubscribe,
  NotificationAdapter,
} from './StatusBar';
import type { SessionEventPayload } from '../ipc/session';
import { setActiveSessionId } from '../stores/session';
import { resetSettingsStore, seedSettings } from '../stores/settings';

const SID = 'test-session-1' as SessionId;

function running(id: string, name = 'writer'): BgAgentSummary {
  return { id, agent_name: name, state: 'Running' };
}

/**
 * Build a controllable fake subscribe surface so tests can drive the
 * StatusBar through `background_agent_*` events without a real Tauri bus.
 * The StatusBar calls `subscribe(handler)` once on mount; the returned
 * unlisten is invoked on cleanup.
 */
interface FakeBus {
  subscribe: BgAgentsSubscribe;
  emit: (payload: SessionEventPayload) => void;
  unlistenCalls: number;
}

function fakeBus(): FakeBus {
  let handler: ((payload: SessionEventPayload) => void) | null = null;
  let unlistenCalls = 0;
  const bus: FakeBus = {
    subscribe: async (h) => {
      handler = h;
      return () => {
        unlistenCalls += 1;
        handler = null;
      };
    },
    emit: (payload) => {
      if (handler) handler(payload);
    },
    unlistenCalls: 0,
  };
  // `unlistenCalls` reads through a getter so tests observe the live count.
  Object.defineProperty(bus, 'unlistenCalls', {
    get: () => unlistenCalls,
  });
  return bus;
}

function fakeNotifier(): NotificationAdapter & {
  toastCalls: string[];
  osCalls: string[];
  permission: 'granted' | 'denied' | 'prompt';
  requestCalls: number;
} {
  const toastCalls: string[] = [];
  const osCalls: string[] = [];
  let permission: 'granted' | 'denied' | 'prompt' = 'granted';
  let requestCalls = 0;
  const adapter = {
    toast: (msg: string) => {
      toastCalls.push(msg);
    },
    os: async (msg: string) => {
      osCalls.push(msg);
    },
    isPermissionGranted: async () => permission === 'granted',
    requestPermission: async () => {
      requestCalls += 1;
      if (permission === 'prompt') permission = 'granted';
      return permission === 'granted' ? 'granted' : 'denied';
    },
  } as NotificationAdapter;

  return new Proxy(adapter, {
    get(target, prop) {
      switch (prop) {
        case 'toastCalls':
          return toastCalls;
        case 'osCalls':
          return osCalls;
        case 'permission':
          return permission;
        case 'requestCalls':
          return requestCalls;
        default:
          return Reflect.get(target, prop);
      }
    },
    set(_target, prop, value) {
      if (prop === 'permission') permission = value;
      return true;
    },
  }) as NotificationAdapter & {
    toastCalls: string[];
    osCalls: string[];
    permission: 'granted' | 'denied' | 'prompt';
    requestCalls: number;
  };
}

beforeEach(() => {
  // F-385: StatusBar reads the session id from the global `activeSessionId`
  // signal. Tests mount outside of SessionWindow, so we seed the signal
  // directly before each mount and clear it in afterEach.
  setActiveSessionId(SID);
  resetSettingsStore();
});

afterEach(() => {
  setActiveSessionId(null);
  cleanup();
});

describe('StatusBar — badge visibility + count', () => {
  it('hides the badge when there are zero running agents', async () => {
    const bus = fakeBus();
    const list = vi.fn().mockResolvedValue([]);
    const { queryByTestId } = render(() => (
      <StatusBar
        listBackgroundAgents={list}
        subscribe={bus.subscribe}
      />
    ));
    await waitFor(() => {
      expect(list).toHaveBeenCalledWith(SID);
    });
    expect(queryByTestId('bg-agents-badge')).toBeNull();
  });

  it('renders "N bg" when initial list returns running agents', async () => {
    const bus = fakeBus();
    const list = vi
      .fn()
      .mockResolvedValue([running('a1'), running('a2', 'reviewer')]);
    const { findByTestId } = render(() => (
      <StatusBar
        listBackgroundAgents={list}
        subscribe={bus.subscribe}
      />
    ));
    const badge = await findByTestId('bg-agents-badge');
    expect(badge).toHaveTextContent('2 bg');
  });

  it('increments on BackgroundAgentStarted and decrements on BackgroundAgentCompleted', async () => {
    const bus = fakeBus();
    const list = vi.fn().mockResolvedValue([running('a1')]);
    const { findByTestId, queryByTestId } = render(() => (
      <StatusBar
        listBackgroundAgents={list}
        subscribe={bus.subscribe}
      />
    ));
    const badge = await findByTestId('bg-agents-badge');
    expect(badge).toHaveTextContent('1 bg');

    // Another started event — badge flips to 2.
    bus.emit({
      session_id: SID,
      seq: 1,
      event: { type: 'background_agent_started', id: 'a2', agent: 'x', at: 'now' },
    });
    await waitFor(() => {
      expect(badge).toHaveTextContent('2 bg');
    });

    // Completion for a1 — badge back to 1.
    bus.emit({
      session_id: SID,
      seq: 2,
      event: { type: 'background_agent_completed', id: 'a1', at: 'now' },
    });
    await waitFor(() => {
      expect(badge).toHaveTextContent('1 bg');
    });

    // Final completion — badge hidden.
    bus.emit({
      session_id: SID,
      seq: 3,
      event: { type: 'background_agent_completed', id: 'a2', at: 'now' },
    });
    await waitFor(() => {
      expect(queryByTestId('bg-agents-badge')).toBeNull();
    });
  });

  it('ignores events for other sessions', async () => {
    const bus = fakeBus();
    const list = vi.fn().mockResolvedValue([running('a1')]);
    const { findByTestId } = render(() => (
      <StatusBar
        listBackgroundAgents={list}
        subscribe={bus.subscribe}
      />
    ));
    const badge = await findByTestId('bg-agents-badge');
    expect(badge).toHaveTextContent('1 bg');

    bus.emit({
      session_id: 'other-session',
      seq: 1,
      event: { type: 'background_agent_started', id: 'zz', agent: 'x', at: 'now' },
    });

    // Give the handler a turn to reject the event.
    await Promise.resolve();
    expect(badge).toHaveTextContent('1 bg');
  });
});

describe('StatusBar — popover interaction', () => {
  it('opens a popover listing running agents when the badge is clicked', async () => {
    const bus = fakeBus();
    const list = vi
      .fn()
      .mockResolvedValue([running('a1', 'writer'), running('a2', 'reviewer')]);
    const { findByTestId, queryByTestId } = render(() => (
      <StatusBar
        listBackgroundAgents={list}
        subscribe={bus.subscribe}
      />
    ));
    const badge = await findByTestId('bg-agents-badge');
    expect(queryByTestId('bg-agents-popover')).toBeNull();

    fireEvent.click(badge);
    const popover = await findByTestId('bg-agents-popover');
    expect(popover).toHaveTextContent('writer');
    expect(popover).toHaveTextContent('reviewer');
  });

  it('Promote button calls promoteBackgroundAgent with the right id', async () => {
    const bus = fakeBus();
    const list = vi.fn().mockResolvedValue([running('abcd1234', 'writer')]);
    const promote = vi.fn().mockResolvedValue(undefined);
    const stop = vi.fn().mockResolvedValue(undefined);
    const { findByTestId } = render(() => (
      <StatusBar
        listBackgroundAgents={list}
        subscribe={bus.subscribe}
        promoteBackgroundAgent={promote}
        stopBackgroundAgent={stop}
      />
    ));
    fireEvent.click(await findByTestId('bg-agents-badge'));
    fireEvent.click(await findByTestId('bg-agents-promote-abcd1234'));
    await waitFor(() => {
      expect(promote).toHaveBeenCalledWith(SID, 'abcd1234');
    });
    expect(stop).not.toHaveBeenCalled();
  });

  it('Stop button calls stopBackgroundAgent with the right id', async () => {
    const bus = fakeBus();
    const list = vi.fn().mockResolvedValue([running('a1', 'writer')]);
    const promote = vi.fn().mockResolvedValue(undefined);
    const stop = vi.fn().mockResolvedValue(undefined);
    const { findByTestId } = render(() => (
      <StatusBar
        listBackgroundAgents={list}
        subscribe={bus.subscribe}
        promoteBackgroundAgent={promote}
        stopBackgroundAgent={stop}
      />
    ));
    fireEvent.click(await findByTestId('bg-agents-badge'));
    fireEvent.click(await findByTestId('bg-agents-stop-a1'));
    await waitFor(() => {
      expect(stop).toHaveBeenCalledWith(SID, 'a1');
    });
  });

  // F-411 (V5): verb+noun display caps per voice-terminology.md §8.
  // Popover actions must carry literal UPPERCASE source strings so screen
  // readers announce the branded phrasing; CSS text-transform alone does not
  // reach assistive tech.
  it('popover action buttons carry PROMOTE AGENT / STOP AGENT as literal text', async () => {
    const bus = fakeBus();
    const list = vi.fn().mockResolvedValue([running('a1', 'writer')]);
    const { findByTestId } = render(() => (
      <StatusBar
        listBackgroundAgents={list}
        subscribe={bus.subscribe}
      />
    ));
    fireEvent.click(await findByTestId('bg-agents-badge'));
    const promote = await findByTestId('bg-agents-promote-a1');
    const stop = await findByTestId('bg-agents-stop-a1');
    expect(promote.textContent).toContain('PROMOTE AGENT');
    expect(stop.textContent).toContain('STOP AGENT');
    expect(promote.textContent).not.toBe('Promote');
    expect(stop.textContent).not.toBe('Stop');
  });
});

describe('StatusBar — notification modes', () => {
  async function primeWithCompletion(
    mode: 'toast' | 'os' | 'both' | 'silent',
    opts: {
      notifier: NotificationAdapter;
    },
  ) {
    seedSettings({
      notifications: { bg_agents: mode },
      windows: { session_mode: 'single' },
      providers: { custom_openai: {} },
      catalog: { enabled: {} },
      dashboard: { container_banner_dismissed: false },
      memory: { enabled: {} },
    });
    const bus = fakeBus();
    const list = vi.fn().mockResolvedValue([running('a1', 'writer')]);
    const result = render(() => (
      <StatusBar
        listBackgroundAgents={list}
        subscribe={bus.subscribe}
        notificationAdapter={opts.notifier}
      />
    ));
    await waitFor(() => {
      expect(list).toHaveBeenCalled();
    });
    bus.emit({
      session_id: SID,
      seq: 1,
      event: { type: 'background_agent_completed', id: 'a1', at: 'now' },
    });
    return result;
  }

  it('mode=toast pushes an in-app toast only', async () => {
    const notifier = fakeNotifier();
    await primeWithCompletion('toast', { notifier });
    await waitFor(() => {
      expect(
        (notifier as unknown as { toastCalls: string[] }).toastCalls.length,
      ).toBe(1);
    });
    expect(
      (notifier as unknown as { osCalls: string[] }).osCalls,
    ).toEqual([]);
  });

  it('mode=os fires an OS notification only', async () => {
    const notifier = fakeNotifier();
    await primeWithCompletion('os', { notifier });
    await waitFor(() => {
      expect(
        (notifier as unknown as { osCalls: string[] }).osCalls.length,
      ).toBe(1);
    });
    expect(
      (notifier as unknown as { toastCalls: string[] }).toastCalls,
    ).toEqual([]);
  });

  it('mode=both fires both toast and OS notification', async () => {
    const notifier = fakeNotifier();
    await primeWithCompletion('both', { notifier });
    await waitFor(() => {
      expect(
        (notifier as unknown as { toastCalls: string[] }).toastCalls.length,
      ).toBe(1);
    });
    expect(
      (notifier as unknown as { osCalls: string[] }).osCalls.length,
    ).toBe(1);
  });

  it('mode=silent does nothing', async () => {
    const notifier = fakeNotifier();
    await primeWithCompletion('silent', { notifier });
    // Let the listener handler run.
    await Promise.resolve();
    await Promise.resolve();
    expect(
      (notifier as unknown as { toastCalls: string[] }).toastCalls,
    ).toEqual([]);
    expect(
      (notifier as unknown as { osCalls: string[] }).osCalls,
    ).toEqual([]);
  });

  it('requests OS permission before firing when not already granted', async () => {
    const notifier = fakeNotifier();
    (notifier as unknown as { permission: string }).permission = 'prompt';
    await primeWithCompletion('os', { notifier });
    await waitFor(() => {
      expect(
        (notifier as unknown as { requestCalls: number }).requestCalls,
      ).toBe(1);
    });
    await waitFor(() => {
      expect(
        (notifier as unknown as { osCalls: string[] }).osCalls.length,
      ).toBe(1);
    });
  });
});

// F-385: single source of truth for sessionId. The StatusBar reads the id
// from the global `activeSessionId` signal; no `sessionId` prop is accepted.
// Proven by asserting (a) the initial `list` call carries the signal's
// value, and (b) an event payload whose `session_id` matches the signal is
// processed while a payload for a foreign id is ignored — the routing
// decision is signal-sourced.
describe('StatusBar — sessionId sourced from activeSessionId signal (F-385)', () => {
  it('initial list_background_agents call uses the signal value', async () => {
    const bus = fakeBus();
    const list = vi.fn().mockResolvedValue([]);
    render(() => (
      <StatusBar listBackgroundAgents={list} subscribe={bus.subscribe} />
    ));
    await waitFor(() => expect(list).toHaveBeenCalledWith(SID));
  });

  it('event routing keys off the signal, not a prop', async () => {
    const bus = fakeBus();
    const list = vi.fn().mockResolvedValue([]);
    const { findByTestId, queryByTestId } = render(() => (
      <StatusBar listBackgroundAgents={list} subscribe={bus.subscribe} />
    ));
    await waitFor(() => expect(list).toHaveBeenCalled());

    // A started event for a DIFFERENT session id must be ignored.
    bus.emit({
      session_id: 'other-session',
      seq: 1,
      event: { type: 'background_agent_started', id: 'x', agent: 'w', at: 'now' },
    });
    await Promise.resolve();
    expect(queryByTestId('bg-agents-badge')).toBeNull();

    // A started event for the signal's session id must land.
    bus.emit({
      session_id: SID,
      seq: 2,
      event: { type: 'background_agent_started', id: 'y', agent: 'w', at: 'now' },
    });
    const badge = await findByTestId('bg-agents-badge');
    expect(badge).toHaveTextContent('1 bg');
  });
});

describe('StatusBar — cleanup', () => {
  it('unsubscribes from the event bus on unmount', async () => {
    const bus = fakeBus();
    const list = vi.fn().mockResolvedValue([]);
    const { unmount } = render(() => (
      <StatusBar
        listBackgroundAgents={list}
        subscribe={bus.subscribe}
      />
    ));
    await waitFor(() => {
      expect(list).toHaveBeenCalled();
    });
    expect(bus.unlistenCalls).toBe(0);
    unmount();
    expect(bus.unlistenCalls).toBe(1);
  });
});

// ---------------------------------------------------------------------------
// F-153: AgentMonitor entry points from the status-bar badge.
//
// The DoD requires double-click OR right-click on the BgAgentsBadge to
// navigate to the agent-monitor route with the clicked instance pre-selected.
// "Clicked instance" only makes sense for a specific row, so the interpretation
// shipped here is:
//   - Double-click / right-click a POPOVER ROW → navigate with that row's id
//     as `?instance=<id>`. This is the primary "clicked instance" path.
//   - Double-click the BADGE → navigate without a pre-selection (the monitor
//     auto-selects the first row when no `instance` param is present). Gives
//     users a zero-click-open-to-last-running entry when they don't care which
//     instance they land on.
//
// Nav is exercised through an injected `navigate` prop so tests don't need to
// mount a full router harness. Matches the existing injectable-dep pattern in
// StatusBar.tsx (list/promote/stop/subscribe/notifier are all props).
// ---------------------------------------------------------------------------

describe('StatusBar — AgentMonitor entry points (F-153)', () => {
  it('double-clicking the badge navigates to /agents/<sessionId> with no instance param', async () => {
    const bus = fakeBus();
    const list = vi.fn().mockResolvedValue([running('a1', 'writer')]);
    const navigate = vi.fn();
    const { findByTestId } = render(() => (
      <StatusBar
        listBackgroundAgents={list}
        subscribe={bus.subscribe}
        navigate={navigate}
      />
    ));
    const badge = await findByTestId('bg-agents-badge');
    fireEvent.dblClick(badge);
    await waitFor(() => {
      expect(navigate).toHaveBeenCalledWith(`/agents/${SID}`);
    });
  });

  it('double-clicking a popover row navigates with ?instance=<id>', async () => {
    const bus = fakeBus();
    const list = vi
      .fn()
      .mockResolvedValue([running('abcd1234', 'writer'), running('ef567890', 'reviewer')]);
    const navigate = vi.fn();
    const { findByTestId } = render(() => (
      <StatusBar
        listBackgroundAgents={list}
        subscribe={bus.subscribe}
        navigate={navigate}
      />
    ));
    fireEvent.click(await findByTestId('bg-agents-badge'));
    const row = await findByTestId('bg-agents-row-ef567890');
    fireEvent.dblClick(row);
    await waitFor(() => {
      expect(navigate).toHaveBeenCalledWith(
        `/agents/${SID}?instance=ef567890`,
      );
    });
  });

  it('right-clicking a popover row navigates with ?instance=<id> and suppresses the native menu', async () => {
    const bus = fakeBus();
    const list = vi.fn().mockResolvedValue([running('abcd1234', 'writer')]);
    const navigate = vi.fn();
    const { findByTestId } = render(() => (
      <StatusBar
        listBackgroundAgents={list}
        subscribe={bus.subscribe}
        navigate={navigate}
      />
    ));
    fireEvent.click(await findByTestId('bg-agents-badge'));
    const row = await findByTestId('bg-agents-row-abcd1234');
    const evt = new MouseEvent('contextmenu', { bubbles: true, cancelable: true });
    const prevented = !row.dispatchEvent(evt);
    expect(prevented).toBe(true);
    await waitFor(() => {
      expect(navigate).toHaveBeenCalledWith(
        `/agents/${SID}?instance=abcd1234`,
      );
    });
  });

  it('single-click on a popover row does NOT navigate (reserved for Promote/Stop focus)', async () => {
    const bus = fakeBus();
    const list = vi.fn().mockResolvedValue([running('abcd1234', 'writer')]);
    const navigate = vi.fn();
    const { findByTestId } = render(() => (
      <StatusBar
        listBackgroundAgents={list}
        subscribe={bus.subscribe}
        navigate={navigate}
      />
    ));
    fireEvent.click(await findByTestId('bg-agents-badge'));
    const row = await findByTestId('bg-agents-row-abcd1234');
    fireEvent.click(row);
    // Give any pending microtasks a chance to run.
    await Promise.resolve();
    await Promise.resolve();
    expect(navigate).not.toHaveBeenCalled();
  });

  // Production-path pin: when no `navigate` prop is threaded the component
  // falls back to `useNavigate()` from `@solidjs/router`. Mount under a real
  // `<MemoryRouter>` to prove the fallback actually dispatches a route change
  // (owner-scoping of the `useNavigate` call inside an event handler is not
  // otherwise covered by the injectable-seam tests above — they all pass a
  // spy prop and never exercise the router-context path).
  it('default (no injected navigate prop) dispatches through the router', async () => {
    const bus = fakeBus();
    const list = vi.fn().mockResolvedValue([running('prod-row', 'writer')]);
    const history = createMemoryHistory();
    history.set({ value: '/' });
    const { findByTestId } = render(() => (
      <MemoryRouter history={history}>
        <Route
          path="/"
          component={() => (
            <StatusBar
              listBackgroundAgents={list}
              subscribe={bus.subscribe}
            />
          )}
        />
      </MemoryRouter>
    ));
    const badge = await findByTestId('bg-agents-badge');
    fireEvent.dblClick(badge);
    await waitFor(() => {
      expect(history.get()).toBe(`/agents/${SID}`);
    });
  });

  it('default (no injected navigate prop) dispatches a row right-click with ?instance through the router', async () => {
    const bus = fakeBus();
    const list = vi.fn().mockResolvedValue([running('prod-row-2', 'reviewer')]);
    const history = createMemoryHistory();
    history.set({ value: '/' });
    const { findByTestId } = render(() => (
      <MemoryRouter history={history}>
        <Route
          path="/"
          component={() => (
            <StatusBar
              listBackgroundAgents={list}
              subscribe={bus.subscribe}
            />
          )}
        />
      </MemoryRouter>
    ));
    fireEvent.click(await findByTestId('bg-agents-badge'));
    const row = await findByTestId('bg-agents-row-prod-row-2');
    const evt = new MouseEvent('contextmenu', { bubbles: true, cancelable: true });
    row.dispatchEvent(evt);
    await waitFor(() => {
      expect(history.get()).toBe(`/agents/${SID}?instance=prod-row-2`);
    });
  });
});
