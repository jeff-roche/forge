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

interface FakeContainer {
  container_id: string;
  session_id: string;
  image: string;
  started_at: string;
  stopped: boolean;
}

interface FakeBackend {
  containers: FakeContainer[];
  logs: { stream: string; line: string; timestamp?: string | null }[];
  stopCalls: string[];
  logsCalls: { containerId: string; since?: string | null; tail?: number | null }[];
}

function makeBackend(seed?: Partial<FakeBackend>): { fn: InvokeMock; state: FakeBackend } {
  const state: FakeBackend = {
    containers: seed?.containers ?? [],
    logs: seed?.logs ?? [],
    stopCalls: [],
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

function seedContainers(extra?: Partial<FakeContainer>[]): FakeContainer[] {
  const base: FakeContainer[] = [
    {
      container_id: 'cid-aaaa',
      session_id: 'sess-1',
      image: 'oci.io/forge/rust-tools:0.3.1',
      started_at: new Date(Date.now() - 5 * 60_000).toISOString(),
      stopped: false,
    },
  ];
  return extra?.length
    ? extra.map((e, i) => ({ ...(base[0] as FakeContainer), ...e, container_id: e?.container_id ?? `cid-${i}` }))
    : base;
}

describe('ContainersSection (F-723)', () => {
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

  // ---- State coverage ----------------------------------------------------

  it('renders a skeleton loading state during the IPC fetch (F-684)', async () => {
    const fn = vi.fn(async (cmd: string) => {
      if (cmd === 'list_active_containers') {
        return new Promise(() => undefined);
      }
      return undefined;
    });
    setInvokeForTesting(fn as never);
    const { findByTestId } = render(() => <ContainersSection />);
    const skeleton = await findByTestId('containers-loading');
    expect(skeleton.getAttribute('role')).toBe('status');
    expect(skeleton.getAttribute('aria-busy')).toBe('true');
  });

  it('renders the empty-state copy when the registry is empty', async () => {
    const { findByTestId } = render(() => <ContainersSection />);
    const empty = await findByTestId('containers-empty');
    expect(empty.textContent).toMatch(/No active sandbox containers/i);
    expect(empty.textContent).toMatch(/Level-2 isolation/);
  });

  it('renders an error line when the prune action rejects (placeholder IPC)', async () => {
    const { fn } = makeBackend({ containers: seedContainers() });
    setInvokeForTesting(fn as never);
    const { findByTestId } = render(() => <ContainersSection />);
    // Wait until the ready state paints so the header buttons are enabled.
    await findByTestId('containers-meta');
    fireEvent.click((await findByTestId('containers-prune-btn')) as HTMLButtonElement);
    const err = await findByTestId('containers-error');
    expect(err.getAttribute('role')).toBe('alert');
    expect(err.textContent).toMatch(/prune_containers/);
  });

  it('renders one row per registered container in the ready state', async () => {
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

  // ---- Header meta -------------------------------------------------------

  it('renders the header meta line as `N running · unknown · podman`', async () => {
    const { fn } = makeBackend({
      containers: [
        {
          container_id: 'cid-1',
          session_id: 'sess-1',
          image: 'alpine:3.19',
          started_at: new Date().toISOString(),
          stopped: false,
        },
        {
          container_id: 'cid-2',
          session_id: 'sess-2',
          image: 'ubuntu:24.04',
          started_at: new Date().toISOString(),
          stopped: false,
        },
        {
          container_id: 'cid-3',
          session_id: 'sess-3',
          image: 'busybox',
          started_at: new Date().toISOString(),
          stopped: true,
        },
      ],
    });
    setInvokeForTesting(fn as never);

    const { findByTestId } = render(() => <ContainersSection />);
    const meta = await findByTestId('containers-meta');
    // Memory is `unknown` because ContainerInfo does not carry per-container
    // memory; the spec accepts `unknown` over fabricated data.
    expect(meta.textContent).toMatch(/2 running/);
    expect(meta.textContent).toMatch(/unknown/);
    expect(meta.textContent).toMatch(/podman/);
  });

  it('header meta is suppressed in the empty state (label only)', async () => {
    const { findByTestId, queryByTestId } = render(() => <ContainersSection />);
    await findByTestId('containers-empty');
    expect(queryByTestId('containers-meta')).toBeNull();
  });

  // ---- Row layout --------------------------------------------------------

  it('row carries a 5-cell grid: live-dot, identity stack, image, resources, actions', async () => {
    const { fn } = makeBackend({ containers: seedContainers() });
    setInvokeForTesting(fn as never);

    const { findByTestId } = render(() => <ContainersSection />);
    const row = (await findByTestId('container-row-cid-aaaa')) as HTMLElement;
    // Liveness dot
    expect(row.querySelector('.containers-section__live-dot')).not.toBeNull();
    // Identity stack (name + sub line)
    expect(row.querySelector('.containers-section__identity')).not.toBeNull();
    // Image ref
    expect(row.querySelector('.containers-section__image')).not.toBeNull();
    // Resource readout
    const res = row.querySelector('.containers-section__resources') as HTMLElement | null;
    expect(res).not.toBeNull();
    expect(res!.textContent).toMatch(/cpu/);
    expect(res!.textContent).toMatch(/ram/);
    // Action cluster
    expect(row.querySelector('.containers-section__row-actions')).not.toBeNull();
  });

  // ---- Per-row actions ---------------------------------------------------

  it('clicking the stop icon invokes stop_container with the row id and pushes a toast', async () => {
    const { fn, state } = makeBackend({ containers: seedContainers() });
    setInvokeForTesting(fn as never);

    const { findByTestId } = render(() => <ContainersSection />);
    fireEvent.click((await findByTestId('container-stop-cid-aaaa')) as HTMLButtonElement);
    await waitFor(() => {
      expect(state.stopCalls).toEqual(['cid-aaaa']);
      expect(toasts().length).toBe(1);
      expect(toasts()[0]!.message).toMatch(/stopped/);
    });
  });

  it('stop icon is disabled when the row is already stopped', async () => {
    const { fn } = makeBackend({
      containers: [
        {
          container_id: 'cid-1',
          session_id: 'sess-1',
          image: 'alpine:3.19',
          started_at: new Date().toISOString(),
          stopped: true,
        },
      ],
    });
    setInvokeForTesting(fn as never);

    const { findByTestId } = render(() => <ContainersSection />);
    const stop = (await findByTestId('container-stop-cid-1')) as HTMLButtonElement;
    expect(stop.disabled).toBe(true);
  });

  it('clicking the terminal icon emits a `forge:container-open-terminal` window event with the row id', async () => {
    const { fn } = makeBackend({ containers: seedContainers() });
    setInvokeForTesting(fn as never);

    const events: string[] = [];
    const listener = (e: Event) => {
      events.push(((e as CustomEvent).detail as { containerId: string }).containerId);
    };
    window.addEventListener('forge:container-open-terminal', listener);

    const { findByTestId } = render(() => <ContainersSection />);
    fireEvent.click((await findByTestId('container-term-cid-aaaa')) as HTMLButtonElement);
    expect(events).toEqual(['cid-aaaa']);

    window.removeEventListener('forge:container-open-terminal', listener);
  });

  // ---- Header Logs toggles an inline panel ------------------------------

  it('clicking the Logs header button toggles an inline log panel beneath the row list', async () => {
    const { fn, state } = makeBackend({
      containers: seedContainers(),
      logs: [{ stream: 'stdout', line: 'hello world', timestamp: '2025-04-26T10:00:00Z' }],
    });
    setInvokeForTesting(fn as never);

    const { findByTestId, queryByTestId } = render(() => <ContainersSection />);
    await findByTestId('containers-meta');
    const logsBtn = (await findByTestId('containers-logs-btn')) as HTMLButtonElement;
    expect(queryByTestId('containers-logs-panel')).toBeNull();

    fireEvent.click(logsBtn);
    const panel = await findByTestId('containers-logs-panel');
    expect(panel).toBeTruthy();
    // No modal backdrop — the panel renders inline inside the card chrome.
    expect(panel.closest('.containers-section')).not.toBeNull();
    await waitFor(() => {
      expect(state.logsCalls.length).toBeGreaterThanOrEqual(1);
      expect(state.logsCalls[0]!.tail).not.toBeNull();
    });

    // Toggle off via the header button.
    fireEvent.click(logsBtn);
    await waitFor(() => {
      expect(queryByTestId('containers-logs-panel')).toBeNull();
    });
  });

  it('inline log panel exposes role="log" with aria-live for screen readers', async () => {
    const { fn } = makeBackend({
      containers: seedContainers(),
      logs: [{ stream: 'stdout', line: 'tick', timestamp: null }],
    });
    setInvokeForTesting(fn as never);

    const { findByTestId, container } = render(() => <ContainersSection />);
    await findByTestId('containers-meta');
    fireEvent.click((await findByTestId('containers-logs-btn')) as HTMLButtonElement);
    await findByTestId('containers-logs-panel');
    const pane = container.querySelector('.containers-section__log-pane') as HTMLElement | null;
    expect(pane).not.toBeNull();
    expect(pane!.getAttribute('role')).toBe('log');
    expect(pane!.getAttribute('aria-live')).toBe('polite');
  });

  it('header actions are disabled while loading and while empty', async () => {
    // Loading: never resolve the list IPC.
    const loadingFn = vi.fn(async (cmd: string) => {
      if (cmd === 'list_active_containers') return new Promise(() => undefined);
      return undefined;
    });
    setInvokeForTesting(loadingFn as never);
    const loading = render(() => <ContainersSection />);
    const prune = (await loading.findByTestId('containers-prune-btn')) as HTMLButtonElement;
    expect(prune.disabled).toBe(true);
    loading.unmount();

    // Empty: resolves immediately with [].
    const { fn } = makeBackend();
    setInvokeForTesting(fn as never);
    const empty = render(() => <ContainersSection />);
    await empty.findByTestId('containers-empty');
    const pruneEmpty = (await empty.findByTestId('containers-prune-btn')) as HTMLButtonElement;
    const logsEmpty = (await empty.findByTestId('containers-logs-btn')) as HTMLButtonElement;
    expect(pruneEmpty.disabled).toBe(true);
    expect(logsEmpty.disabled).toBe(true);
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
      if (c.kind === 'available') {
        expect(bannerDetail(c)).toBe('');
      } else {
        expect(bannerDetail(c).length).toBeGreaterThan(0);
      }
      expect(installInstructionsUrl(c)).toMatch(/^https?:\/\//);
    }
  });
});
