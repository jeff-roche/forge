import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';
import { setInvokeForTesting } from '../../lib/tauri';
import {
  ContainerRuntimeBanner,
  ContainersSection,
  bannerDetail,
  bannerHeadline,
  installInstructionsUrl,
} from './ContainersSection';
import type { RuntimeStatus } from '../../ipc/containers';
import { clearToastsForTesting, toasts } from '../toast';

vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async () => () => undefined),
}));

type InvokeMock = ReturnType<typeof vi.fn>;

interface FakeBackend {
  containers: { container_id: string; session_id: string; image: string; started_at: string; stopped: boolean }[];
  logs: { stream: string; line: string; timestamp?: string | null }[];
  stopCalls: string[];
  removeCalls: string[];
  logsCalls: { containerId: string; since?: string | null; tail?: number | null }[];
}

function makeBackend(seed?: Partial<FakeBackend>): { fn: InvokeMock; state: FakeBackend } {
  const state: FakeBackend = {
    containers: seed?.containers ?? [],
    logs: seed?.logs ?? [],
    stopCalls: [],
    removeCalls: [],
    logsCalls: [],
  };
  const fn = vi.fn(async (cmd: string, args?: Record<string, unknown>) => {
    if (cmd === 'list_active_containers') return state.containers;
    if (cmd === 'detect_container_runtime') return { kind: 'available' };
    if (cmd === 'stop_container') {
      const id = args?.['containerId'] as string;
      state.stopCalls.push(id);
      const c = state.containers.find((x) => x.container_id === id);
      if (c) c.stopped = true;
      return undefined;
    }
    if (cmd === 'remove_container') {
      const id = args?.['containerId'] as string;
      state.removeCalls.push(id);
      state.containers = state.containers.filter((x) => x.container_id !== id);
      return undefined;
    }
    if (cmd === 'container_logs') {
      state.logsCalls.push({
        containerId: args?.['containerId'] as string,
        since: (args?.['since'] as string | null | undefined) ?? null,
        tail: (args?.['tail'] as number | null | undefined) ?? null,
      });
      return state.logs;
    }
    return undefined;
  });
  return { fn, state };
}

describe('ContainersSection (F-597)', () => {
  beforeEach(() => {
    const { fn } = makeBackend();
    setInvokeForTesting(fn as never);
    clearToastsForTesting();
  });

  afterEach(() => {
    setInvokeForTesting(null);
    cleanup();
    clearToastsForTesting();
  });

  it('renders the empty state when the registry is empty', async () => {
    const { findByTestId } = render(() => <ContainersSection />);
    expect(await findByTestId('containers-empty')).toBeTruthy();
  });

  it('renders a skeleton loading state during the IPC fetch (F-684)', async () => {
    const fn = vi.fn(async (cmd: string) => {
      if (cmd === 'list_active_containers') {
        return new Promise(() => undefined);
      }
      return undefined;
    });
    setInvokeForTesting(fn as never);
    const { findByTestId, queryByText } = render(() => <ContainersSection />);
    const skeleton = await findByTestId('containers-loading');
    expect(skeleton.getAttribute('role')).toBe('status');
    expect(skeleton.getAttribute('aria-busy')).toBe('true');
    // Plain-text "containers · loading" copy must be gone.
    expect(queryByText(/containers · loading/i)).toBeFalsy();
  });

  it('renders one row per registered container', async () => {
    const { fn } = makeBackend({
      containers: [
        {
          container_id: 'cid-aaaa',
          session_id: 'sess-1',
          image: 'alpine:3.19',
          started_at: new Date().toISOString(),
          stopped: false,
        },
        {
          container_id: 'cid-bbbb',
          session_id: 'sess-2',
          image: 'ubuntu:24.04',
          started_at: new Date().toISOString(),
          stopped: false,
        },
      ],
    });
    setInvokeForTesting(fn as never);

    const { findByTestId } = render(() => <ContainersSection />);
    expect(await findByTestId('container-row-cid-aaaa')).toBeTruthy();
    expect(await findByTestId('container-row-cid-bbbb')).toBeTruthy();
  });

  it('clicking STOP invokes stop_container with the row id', async () => {
    const { fn, state } = makeBackend({
      containers: [
        {
          container_id: 'cid-1',
          session_id: 'sess-1',
          image: 'alpine:3.19',
          started_at: new Date().toISOString(),
          stopped: false,
        },
      ],
    });
    setInvokeForTesting(fn as never);

    const { findByTestId } = render(() => <ContainersSection />);
    const stop = (await findByTestId('container-stop-cid-1')) as HTMLButtonElement;
    fireEvent.click(stop);
    await waitFor(() => {
      expect(state.stopCalls).toEqual(['cid-1']);
    });
  });

  it('clicking REMOVE opens a confirm modal and does NOT call remove_container until confirmed', async () => {
    const { fn, state } = makeBackend({
      containers: [
        {
          container_id: 'cid-1',
          session_id: 'sess-1',
          image: 'alpine:3.19',
          started_at: new Date().toISOString(),
          stopped: false,
        },
      ],
    });
    setInvokeForTesting(fn as never);

    const { findByTestId } = render(() => <ContainersSection />);
    const remove = (await findByTestId('container-remove-cid-1')) as HTMLButtonElement;
    fireEvent.click(remove);

    const modal = await findByTestId('container-remove-modal');
    expect(modal.getAttribute('role')).toBe('dialog');
    expect(modal.getAttribute('aria-modal')).toBe('true');
    expect(state.removeCalls).toEqual([]);
  });

  it('REMOVE confirm flow: cancelling closes the modal without invoking remove_container', async () => {
    const { fn, state } = makeBackend({
      containers: [
        {
          container_id: 'cid-1',
          session_id: 'sess-1',
          image: 'alpine:3.19',
          started_at: new Date().toISOString(),
          stopped: false,
        },
      ],
    });
    setInvokeForTesting(fn as never);

    const { findByTestId, queryByTestId } = render(() => <ContainersSection />);
    fireEvent.click((await findByTestId('container-remove-cid-1')) as HTMLButtonElement);
    await findByTestId('container-remove-modal');

    fireEvent.click((await findByTestId('container-remove-cancel')) as HTMLButtonElement);
    await waitFor(() => {
      expect(queryByTestId('container-remove-modal')).toBeNull();
    });
    expect(state.removeCalls).toEqual([]);
  });

  it('REMOVE confirm flow: confirming invokes remove_container and drops the row from the list', async () => {
    const { fn, state } = makeBackend({
      containers: [
        {
          container_id: 'cid-1',
          session_id: 'sess-1',
          image: 'alpine:3.19',
          started_at: new Date().toISOString(),
          stopped: false,
        },
      ],
    });
    setInvokeForTesting(fn as never);

    const { findByTestId, queryByTestId } = render(() => <ContainersSection />);
    fireEvent.click((await findByTestId('container-remove-cid-1')) as HTMLButtonElement);
    fireEvent.click((await findByTestId('container-remove-confirm')) as HTMLButtonElement);

    await waitFor(() => {
      expect(state.removeCalls).toEqual(['cid-1']);
      expect(queryByTestId('container-row-cid-1')).toBeNull();
      expect(queryByTestId('container-remove-modal')).toBeNull();
    });
  });

  it('clicking STOP pushes a confirmation toast after the IPC succeeds', async () => {
    const { fn, state } = makeBackend({
      containers: [
        {
          container_id: 'cid-1',
          session_id: 'sess-1',
          image: 'alpine:3.19',
          started_at: new Date().toISOString(),
          stopped: false,
        },
      ],
    });
    setInvokeForTesting(fn as never);

    const { findByTestId } = render(() => <ContainersSection />);
    fireEvent.click((await findByTestId('container-stop-cid-1')) as HTMLButtonElement);

    await waitFor(() => {
      expect(state.stopCalls).toEqual(['cid-1']);
      const queue = toasts();
      expect(queue.length).toBe(1);
      expect(queue[0]!.kind).toBe('info');
      expect(queue[0]!.message).toContain('stopped');
    });
  });

  it('clicking LOGS opens the flyout and fetches initial logs', async () => {
    const { fn, state } = makeBackend({
      containers: [
        {
          container_id: 'cid-1',
          session_id: 'sess-1',
          image: 'alpine:3.19',
          started_at: new Date().toISOString(),
          stopped: false,
        },
      ],
      logs: [{ stream: 'stdout', line: 'hello world', timestamp: '2025-04-26T10:00:00Z' }],
    });
    setInvokeForTesting(fn as never);

    const { findByTestId } = render(() => <ContainersSection />);
    const logsBtn = (await findByTestId('container-logs-btn-cid-1')) as HTMLButtonElement;
    fireEvent.click(logsBtn);
    await findByTestId('container-logs-flyout');
    await waitFor(() => {
      expect(state.logsCalls.length).toBeGreaterThanOrEqual(1);
      const first = state.logsCalls[0]!;
      expect(first.containerId).toBe('cid-1');
      // Initial poll requests `tail` so the viewer seeds with recent history.
      expect(first.tail).not.toBeNull();
    });
  });

  // F-696: the log pane sits inside a `useFocusTrap` modal flyout. A
  // `tabIndex={0}` on `<pre>` made it a Tab stop *inside* the trap, and
  // Tab from there exited the modal. The pane must instead expose itself
  // as `role="log"` (live region) without entering the focus order.
  it('log pane is a live region (role=log) and is not a Tab stop', async () => {
    const { fn } = makeBackend({
      containers: [
        {
          container_id: 'cid-1',
          session_id: 'sess-1',
          image: 'alpine:3.19',
          started_at: new Date().toISOString(),
          stopped: false,
        },
      ],
      logs: [{ stream: 'stdout', line: 'tick', timestamp: null }],
    });
    setInvokeForTesting(fn as never);

    const { findByTestId, container } = render(() => <ContainersSection />);
    const logsBtn = (await findByTestId('container-logs-btn-cid-1')) as HTMLButtonElement;
    fireEvent.click(logsBtn);
    await findByTestId('container-logs-flyout');
    const pane = container.querySelector('.containers-section__log-pane') as HTMLElement | null;
    expect(pane).not.toBeNull();
    expect(pane!.getAttribute('role')).toBe('log');
    expect(pane!.hasAttribute('tabindex')).toBe(false);
    expect(pane!.getAttribute('aria-live')).toBe('polite');
    expect(pane!.getAttribute('aria-label')).toMatch(/logs/i);
  });
});

describe('ContainerRuntimeBanner (F-597)', () => {
  afterEach(() => cleanup());

  it('renders headline and detail for the missing variant', () => {
    const { getByTestId } = render(() => (
      <ContainerRuntimeBanner
        status={{ kind: 'missing', tool: 'podman' }}
        onDismiss={() => undefined}
      />
    ));
    const banner = getByTestId('container-runtime-banner');
    expect(banner.textContent).toContain('Container runtime not installed');
    expect(banner.textContent).toContain('podman');
  });

  it('renders detail for the rootless_unavailable variant', () => {
    const { getByTestId } = render(() => (
      <ContainerRuntimeBanner
        status={{
          kind: 'rootless_unavailable',
          tool: 'podman',
          reason: 'rootless=false',
        }}
        onDismiss={() => undefined}
      />
    ));
    const banner = getByTestId('container-runtime-banner');
    expect(banner.textContent).toContain('Rootless mode unavailable');
    expect(banner.textContent).toContain('rootless=false');
  });

  it('uses role="alert" so screen readers announce runtime failure assertively', () => {
    const { getByTestId } = render(() => (
      <ContainerRuntimeBanner
        status={{ kind: 'missing', tool: 'podman' }}
        onDismiss={() => undefined}
      />
    ));
    expect(getByTestId('container-runtime-banner').getAttribute('role')).toBe('alert');
  });

  it('clicking "DON\'T SHOW AGAIN" calls the onDismiss handler', () => {
    const onDismiss = vi.fn();
    const { getByTestId } = render(() => (
      <ContainerRuntimeBanner
        status={{ kind: 'missing', tool: 'podman' }}
        onDismiss={onDismiss}
      />
    ));
    fireEvent.click(getByTestId('container-runtime-banner-dismiss'));
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });

  it('exposes pure helper functions for headline / detail / install link', () => {
    const cases: RuntimeStatus[] = [
      { kind: 'available' },
      { kind: 'missing', tool: 'podman' },
      { kind: 'broken', tool: 'podman', reason: 'newuidmap missing' },
      { kind: 'rootless_unavailable', tool: 'podman', reason: 'rootless=false' },
      { kind: 'unknown', reason: 'boom' },
    ];
    for (const c of cases) {
      expect(bannerHeadline(c)).toBeTruthy();
      // Available has no detail; everything else does.
      if (c.kind === 'available') {
        expect(bannerDetail(c)).toBe('');
      } else {
        expect(bannerDetail(c).length).toBeGreaterThan(0);
      }
      expect(installInstructionsUrl(c)).toMatch(/^https?:\/\//);
    }
  });
});
