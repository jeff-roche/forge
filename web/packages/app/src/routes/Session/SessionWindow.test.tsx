import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';

const { invokeMock, listenMock, unlistenMock, closeMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  listenMock: vi.fn(),
  unlistenMock: vi.fn(),
  closeMock: vi.fn(),
}));

vi.mock('@tauri-apps/api/event', () => ({
  listen: listenMock,
}));

vi.mock('@tauri-apps/api/window', () => ({
  getCurrentWindow: () => ({ close: closeMock }),
}));

import { MemoryRouter, Route, createMemoryHistory } from '@solidjs/router';
import {
  SessionWindow,
  __setInjectedLayoutStoreForTesting,
} from './SessionWindow';
import { resetSessionEventStore } from '../../stores/session';
import { resetMessagesStore } from '../../stores/messages';
import {
  recordProviderModel,
  recordUsageTick,
  resetSessionTelemetryStore,
} from '../../stores/sessionTelemetry';
import type { ProviderId } from '@forge/ipc';
import { setInvokeForTesting } from '../../lib/tauri';
import type { LayoutTree, Layouts } from '@forge/ipc';
import {
  createLayoutStore,
  defaultLayouts,
  type LayoutStore,
} from '../../layout/layoutStore';

const helloAck = {
  session_id: 'abc123',
  workspace: '/ws',
  started_at: '2026-04-18T00:00:00Z',
  event_seq: 0,
  schema_version: 1,
};

function renderAt(path: string) {
  const history = createMemoryHistory();
  history.set({ value: path });
  return render(() => (
    <MemoryRouter history={history}>
      <Route path="/session/:id" component={SessionWindow} />
    </MemoryRouter>
  ));
}

// F-150: test-only wrapper around the real `createLayoutStore` that skips
// the `read_layouts` / `write_layouts` IPC roundtrip and exposes an
// `__openFileCalls` spy. Uses the production implementation so tree-based
// openFile / closeLeaf / setLayoutTree semantics are tested end-to-end —
// the previous fake reproduced only the singleton pane_state shape that
// F-150 removed.
function makeFakeLayoutStore(
  seed?: Layouts,
): LayoutStore & { __openFileCalls: string[] } {
  const calls: string[] = [];
  const initial = seed ?? defaultLayouts();
  // Synchronous stubs so `load()` completes in-line with `onMount`, keeping
  // `activeTree()` stable from the first paint. The scheduler is stubbed to
  // a no-op so no setTimeout handles leak between tests.
  const store = createLayoutStore('/ws', {
    read: async () => initial,
    write: async () => {},
    scheduler: {
      setTimeout: () => 0,
      clearTimeout: () => {},
    },
  });
  // Seed synchronously so test setup can call `store.openFile(...)` before
  // mount without awaiting `load()`.
  store.setLayouts(initial);
  const origOpen = store.openFile.bind(store);
  store.openFile = (path: string) => {
    calls.push(path);
    origOpen(path);
  };
  return Object.assign(store, { __openFileCalls: calls });
}

function renderWithStore(path: string, store: LayoutStore) {
  __setInjectedLayoutStoreForTesting(store);
  const history = createMemoryHistory();
  history.set({ value: path });
  return render(() => (
    <MemoryRouter history={history}>
      <Route path="/session/:id" component={SessionWindow} />
    </MemoryRouter>
  ));
}

describe('SessionWindow', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    listenMock.mockReset();
    unlistenMock.mockReset();
    closeMock.mockReset();
    resetSessionEventStore();
    resetMessagesStore();
    resetSessionTelemetryStore();
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === 'session_hello') return helloAck;
      // F-126: layoutStore.load() calls read_layouts on mount when no store
      // is injected. Return the default layouts so SessionWindow proceeds
      // cleanly; writes are no-ops for tests that don't assert on them.
      if (cmd === 'read_layouts') return defaultLayouts();
      if (cmd === 'write_layouts') return undefined;
      // F-126: the FilesSidebar calls `tree` when the sidebar opens; return
      // a minimal empty-root shape so the component mounts cleanly.
      if (cmd === 'tree') {
        return {
          name: 'ws',
          path: '/ws',
          kind: 'Dir',
          children: [],
        };
      }
      return undefined;
    });
    setInvokeForTesting(invokeMock as never);
    listenMock.mockResolvedValue(unlistenMock);
  });

  afterEach(() => {
    setInvokeForTesting(null);
    __setInjectedLayoutStoreForTesting(null);
    cleanup();
  });

  it('renders the session id from the route', async () => {
    const { findByTestId } = renderAt('/session/abc123');
    const subject = await findByTestId('pane-header-subject');
    expect(subject.textContent).toContain('abc123');
  });

  it('calls session_hello on mount with the route-param id', async () => {
    renderAt('/session/abc123');
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('session_hello', {
        sessionId: 'abc123',
      }),
    );
  });

  it('calls session_subscribe on mount after hello resolves', async () => {
    renderAt('/session/abc123');
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('session_subscribe', {
        sessionId: 'abc123',
        since: 0,
      }),
    );
    // hello must run before subscribe
    const helloIdx = invokeMock.mock.calls.findIndex((c) => c[0] === 'session_hello');
    const subIdx = invokeMock.mock.calls.findIndex((c) => c[0] === 'session_subscribe');
    expect(helloIdx).toBeGreaterThanOrEqual(0);
    expect(subIdx).toBeGreaterThan(helloIdx);
  });

  it('attaches a session:event listener on mount', async () => {
    renderAt('/session/abc123');
    await waitFor(() =>
      expect(listenMock).toHaveBeenCalledWith('session:event', expect.any(Function)),
    );
  });

  it('detaches the session:event listener on unmount', async () => {
    const { unmount } = renderAt('/session/abc123');
    // F-716: with the StatusBar lifted to AppShell, SessionWindow attaches
    // its own session:event adapter and the F-640 provider:changed
    // forwarder. F-748 adds a third for `session:crashed`. Wait for all
    // three before asserting unlisten counts so a race on whichever
    // resolves last doesn't under-count.
    await waitFor(() => expect(listenMock).toHaveBeenCalledTimes(3));
    unmount();
    await waitFor(() => expect(unlistenMock).toHaveBeenCalledTimes(3));
  });

  // F-640: dashboard `provider:changed` events must be forwarded to the
  // per-session daemon as `session_switch_provider` so the daemon arm in
  // `handle_connection` can call `SwappableProvider::swap` for the next
  // turn. The listener must filter out malformed payloads (no
  // provider_id) so a stale emit can't trigger a noisy backend call.
  it('forwards provider:changed events to session_switch_provider', async () => {
    let providerChangedHandler:
      | ((event: { payload: { type: string; provider_id?: string } }) => void)
      | null = null;
    listenMock.mockImplementation((channel: string, handler: never) => {
      if (channel === 'provider:changed') {
        providerChangedHandler = handler as never;
      }
      return Promise.resolve(unlistenMock);
    });
    renderAt('/session/abc123');
    await waitFor(() =>
      expect(listenMock).toHaveBeenCalledWith(
        'provider:changed',
        expect.any(Function),
      ),
    );
    expect(providerChangedHandler).not.toBeNull();
    providerChangedHandler!({
      payload: { type: 'provider_changed', provider_id: 'anthropic' },
    });
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('session_switch_provider', {
        sessionId: 'abc123',
        providerId: 'anthropic',
      }),
    );
  });

  it('ignores provider:changed events with empty provider_id', async () => {
    let providerChangedHandler:
      | ((event: { payload: { type: string; provider_id?: string } }) => void)
      | null = null;
    listenMock.mockImplementation((channel: string, handler: never) => {
      if (channel === 'provider:changed') {
        providerChangedHandler = handler as never;
      }
      return Promise.resolve(unlistenMock);
    });
    renderAt('/session/abc123');
    await waitFor(() =>
      expect(listenMock).toHaveBeenCalledWith(
        'provider:changed',
        expect.any(Function),
      ),
    );
    providerChangedHandler!({ payload: { type: 'provider_changed' } });
    providerChangedHandler!({
      payload: { type: 'provider_changed', provider_id: '' },
    });
    // Give the microtask queue a tick to drain — no invoke should fire.
    await new Promise((r) => setTimeout(r, 0));
    expect(invokeMock).not.toHaveBeenCalledWith(
      'session_switch_provider',
      expect.anything(),
    );
  });

  it('renders a single-leaf grid when no split is in the active layout (F-150)', async () => {
    const { container, findByTestId } = renderAt('/session/abc123');
    await findByTestId('pane-header-subject');
    // Default layout is a single chat leaf — GridContainer renders one
    // `.session-window__pane` and no SplitPane divider. After F-150 the
    // grid is always present, so assert on the leaf count rather than the
    // previous "exactly one pane slot" singleton shape.
    const panes = container.querySelectorAll('.session-window__pane');
    expect(panes.length).toBe(1);
    expect(container.querySelector('[data-testid="split-pane"]')).toBeNull();
  });

  // F-395: PaneHeader reflects live provider/model + session cost telemetry.
  // Before any AssistantMessage or UsageTick arrives, the pill falls back to
  // the sanctioned provider-id-only label (no "pending") and the cost meter
  // renders an em-dash placeholder (not $0.00). Once the telemetry store
  // records real values, both update reactively.

  it('pane header: before any telemetry, cost renders em-dash placeholder and pill has no "pending"', async () => {
    const { findByTestId, findByRole } = renderAt('/session/abc123');
    const subject = await findByTestId('pane-header-subject');
    // Subject no longer starts with the placeholder "Session " prefix —
    // F-395 removes the legacy `Session <id>` hardcoded label.
    expect(subject.textContent).not.toMatch(/^Session /);
    const provider = await findByTestId('pane-header-provider');
    // "pending" is not in the sanctioned state vocabulary — must not appear.
    expect(provider.textContent?.toLowerCase()).not.toContain('pending');
    const cost = await findByTestId('pane-header-cost');
    // Documented placeholder — literal em-dash, not the fabricated $0.00.
    expect(cost.textContent).toContain('—');
    expect(cost.textContent).not.toContain('$0.00');
    const close = await findByRole('button', { name: /close/i });
    expect(close).toBeInTheDocument();
  });

  it('pane header reflects the telemetry store provider/model after recordProviderModel', async () => {
    const { findByTestId } = renderAt('/session/abc123');
    await findByTestId('pane-header-subject');
    recordProviderModel(
      'abc123' as never,
      'anthropic' as ProviderId,
      'claude-opus-4-7',
    );
    const provider = await findByTestId('pane-header-provider');
    await waitFor(() =>
      expect(provider.textContent?.toLowerCase()).toContain('anthropic'),
    );
    const subject = await findByTestId('pane-header-subject');
    await waitFor(() =>
      expect(subject.textContent).toContain('claude-opus-4-7'),
    );
  });

  it('pane header reflects provider + cost driven end-to-end by mocked Rust-shaped IPC events (F-395 regression)', async () => {
    const handlers: Array<(ev: { payload: unknown }) => void> = [];
    listenMock.mockImplementation(
      async (_name: string, handler: (ev: { payload: unknown }) => void) => {
        handlers.push(handler);
        return unlistenMock;
      },
    );

    const { findByTestId } = renderAt('/session/abc123');
    await findByTestId('pane-header-subject');
    // Both adapter + bg-agents listeners must attach before we dispatch.
    await waitFor(() => expect(handlers.length).toBeGreaterThanOrEqual(2));

    // 1. assistant_message carries provider + model on the wire. Adapter
    //    routes it into the sessionTelemetry store, which the PaneHeader
    //    reads via getSessionTelemetry(sessionId).
    for (const h of handlers) {
      h({
        payload: {
          session_id: 'abc123',
          seq: 1,
          event: {
            type: 'assistant_message',
            id: 'a-1',
            at: '2026-04-21T10:00:00Z',
            provider: 'anthropic',
            model: 'claude-opus-4-7',
            text: 'hello',
            stream_finalised: true,
            branch_parent: null,
            branch_variant_index: 0,
          },
        },
      });
    }
    const provider = await findByTestId('pane-header-provider');
    await waitFor(() =>
      expect(provider.textContent?.toLowerCase()).toContain('anthropic'),
    );

    // 2. usage_tick on the wire must land in the cost meter. Until F-395 the
    //    adapter dropped it (returned null) — this is the regression.
    const cost = await findByTestId('pane-header-cost');
    expect(cost.textContent).toContain('—');
    for (const h of handlers) {
      h({
        payload: {
          session_id: 'abc123',
          seq: 2,
          event: {
            type: 'usage_tick',
            provider: 'anthropic',
            model: 'claude-opus-4-7',
            tokens_in: 500,
            tokens_out: 1500,
            cost_usd: 0.02,
            scope: 'SessionWide',
          },
        },
      });
    }
    await waitFor(() => {
      expect(cost.textContent).toMatch(/in\s+500/);
      // 1500 abbreviates to `1.5k` per spec §PH.4.
      expect(cost.textContent).toMatch(/out\s+1\.5k/);
      expect(cost.textContent).toContain('$0.02');
    });
  });

  it('pane header cost meter switches from placeholder to live values on UsageTick', async () => {
    const { findByTestId } = renderAt('/session/abc123');
    const cost = await findByTestId('pane-header-cost');
    expect(cost.textContent).toContain('—');
    recordUsageTick('abc123' as never, 1234, 5678, 0.042);
    await waitFor(() => {
      // Spec §PH.4: tokens abbreviated above 1000 — `1.2k`, `5.7k`.
      expect(cost.textContent).toMatch(/in\s+1\.2k/);
      expect(cost.textContent).toMatch(/out\s+5\.7k/);
      expect(cost.textContent).toContain('$0.04');
    });
  });

  it('close button invokes the current window close()', async () => {
    const { findByRole } = renderAt('/session/abc123');
    const close = await findByRole('button', { name: /close/i });
    close.click();
    expect(closeMock).toHaveBeenCalledTimes(1);
  });

  it('renders a ChatPane placeholder with the CHAT type label', async () => {
    const { findByTestId } = renderAt('/session/abc123');
    const chatPane = await findByTestId('chat-pane');
    expect(chatPane.textContent).toContain('CHAT');
  });

  it('calls get_persistent_approvals with the HelloAck workspace after hello resolves (F-036)', async () => {
    renderAt('/session/abc123');
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('get_persistent_approvals', {
        workspaceRoot: '/ws',
      }),
    );
    // Must fire after hello (we need its `workspace` field).
    const helloIdx = invokeMock.mock.calls.findIndex((c) => c[0] === 'session_hello');
    const getIdx = invokeMock.mock.calls.findIndex(
      (c) => c[0] === 'get_persistent_approvals',
    );
    expect(helloIdx).toBeGreaterThanOrEqual(0);
    expect(getIdx).toBeGreaterThan(helloIdx);
  });

  it('seeds the approvals store from get_persistent_approvals (F-036)', async () => {
    const seedMod = await import('../../stores/approvals');
    // Mock get_persistent_approvals to return two seed entries.
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === 'session_hello') return helloAck;
      if (cmd === 'get_persistent_approvals') {
        return [
          {
            scope_key: 'tool:fs.write',
            tool_name: 'fs.write',
            label: 'this tool',
            level: 'workspace',
          },
        ];
      }
      return undefined;
    });

    seedMod.resetApprovalsStore();
    renderAt('/session/abc123');
    await waitFor(() => {
      const wl = seedMod.getApprovalWhitelist('abc123' as never);
      expect('tool:fs.write' in wl.entries).toBe(true);
    });
    const wl = seedMod.getApprovalWhitelist('abc123' as never);
    expect(wl.entries['tool:fs.write']?.level).toBe('workspace');
  });

  it('routes Rust-shaped session:event payloads through the adapter into the chat pane', async () => {
    // F-716: SessionWindow attaches a single `session:event` adapter
    // listener (plus a `provider:changed` listener captured separately).
    // Capture every handler so the test can dispatch through the adapter
    // path regardless of attachment order.
    const handlers: Array<(ev: { payload: unknown }) => void> = [];
    listenMock.mockImplementation(async (_name: string, handler: (ev: { payload: unknown }) => void) => {
      handlers.push(handler);
      return unlistenMock;
    });

    const { findByTestId } = renderAt('/session/abc123');
    await findByTestId('chat-pane');
    await waitFor(() => expect(handlers.length).toBeGreaterThanOrEqual(1));

    // Fire a real Rust-shaped user_message event — the adapter must rename
    // id → message_id and discriminate on kind so the store renders it.
    // Fan out to every attached handler; the bg-agents subscriber
    // classifies this as a non-bg event and ignores it.
    for (const h of handlers) {
      h({
        payload: {
          session_id: 'abc123',
          seq: 1,
          event: {
            type: 'user_message',
            id: 'u-wire-1',
            at: '2026-04-18T10:00:00Z',
            text: 'hello from the wire',
            context: [],
            branch_parent: null,
          },
        },
      });
    }

    const list = await findByTestId('message-list');
    await waitFor(() => expect(list.textContent).toContain('hello from the wire'));
  });

  // -----------------------------------------------------------------------
  // F-150: Files-sidebar Open -> layoutStore -> GridContainer -> EditorPane.
  // Unlike F-126's singleton-slot flow, opening a file splits the grid so
  // the existing chat pane remains visible side-by-side with a new editor
  // leaf. Closing the editor leaf reclaims the grid space and leaves the
  // chat pane as the sole leaf.
  // -----------------------------------------------------------------------

  it('splits the grid and mounts an EditorPane when the FilesSidebar bridge fires openFile', async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === 'session_hello') return helloAck;
      if (cmd === 'read_layouts') return defaultLayouts();
      if (cmd === 'write_layouts') return undefined;
      if (cmd === 'read_file') {
        // EditorPane.sendOpen calls `read_file` when it mounts. Return a
        // stubbed content so the pane doesn't error out.
        return { content: '# hi', bytes: 4, sha256: 'abc' };
      }
      return undefined;
    });

    const store = makeFakeLayoutStore();
    const { findByTestId, queryByTestId } = renderWithStore(
      '/session/abc123',
      store,
    );

    // Initial state: ChatPane is mounted as the sole grid leaf; no editor.
    await findByTestId('chat-pane');
    expect(queryByTestId('editor-pane')).toBeNull();

    // F-716: SessionWindow publishes its openFile through the
    // `activeOpenFile` store while mounted; AppShell's FilesSidebar invokes
    // it. Drive that bridge directly here — the AppShell render harness is
    // covered separately in `AppShell.test.tsx`.
    const { activeOpenFile } = await import('../../stores/session');
    await waitFor(() => expect(activeOpenFile()).not.toBeNull());
    activeOpenFile()!('/ws/README.md');

    const editor = await findByTestId('editor-pane');
    expect(editor).toBeInTheDocument();
    expect(store.__openFileCalls).toContain('/ws/README.md');
    // F-150: chat pane stays — the split mounts editor beside it.
    expect(queryByTestId('chat-pane')).not.toBeNull();
    // F-394: breadcrumb leaf is the PaneHeader subject; prefix lands in
    // the detail/cost slot. Scope the lookup to the editor section —
    // multiple pane-headers render in parallel (chat + editor).
    const editorSection = editor;
    const subject = editorSection.querySelector(
      '[data-testid="pane-header-subject"]',
    );
    expect(subject?.textContent).toBe('README.md');
    // Tree is now a v-split with one editor leaf (F-150 DoD: split-when-none).
    const rootTree = store.layouts.named[store.layouts.active]?.tree;
    expect(rootTree?.kind).toBe('split');
  });

  it('reuses the existing editor leaf when opening a second file', async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === 'session_hello') return helloAck;
      if (cmd === 'read_layouts') return defaultLayouts();
      if (cmd === 'write_layouts') return undefined;
      if (cmd === 'tree') {
        return {
          name: 'ws',
          path: '/ws',
          kind: 'Dir',
          children: [
            { name: 'a.ts', path: '/ws/a.ts', kind: 'File', children: null },
            { name: 'b.ts', path: '/ws/b.ts', kind: 'File', children: null },
          ],
        };
      }
      if (cmd === 'read_file') {
        return { content: '', bytes: 0, sha256: '' };
      }
      return undefined;
    });

    const store = makeFakeLayoutStore();
    // First open creates the editor leaf. Count the editor leaves after
    // the second open to confirm only one exists.
    store.openFile('/ws/a.ts');
    const treeAfterFirst = store.layouts.named[store.layouts.active]?.tree;
    expect(treeAfterFirst?.kind).toBe('split');

    const { findByTestId } = renderWithStore('/session/abc123', store);
    await findByTestId('editor-pane');

    // Second open should reuse the same leaf, not add another split.
    store.openFile('/ws/b.ts');
    const treeAfterSecond = store.layouts.named[store.layouts.active]?.tree;
    expect(treeAfterSecond?.kind).toBe('split');
    if (treeAfterSecond?.kind === 'split') {
      // Still exactly one editor leaf under the root split.
      const ids = new Set<string>();
      const walk = (n: LayoutTree): void => {
        if (n.kind === 'leaf') ids.add(n.id);
        else {
          walk(n.a);
          walk(n.b);
        }
      };
      walk(treeAfterSecond);
      expect(ids.size).toBe(2); // chat + editor
    }
  });

  it('closing the EditorPane reclaims the grid space and leaves the chat pane', async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === 'session_hello') return helloAck;
      if (cmd === 'read_layouts') return defaultLayouts();
      if (cmd === 'write_layouts') return undefined;
      if (cmd === 'tree') {
        return { name: 'ws', path: '/ws', kind: 'Dir', children: [] };
      }
      if (cmd === 'read_file') {
        return { content: '', bytes: 0, sha256: '' };
      }
      return undefined;
    });

    const store = makeFakeLayoutStore();
    // Pre-seed an open file so the EditorPane mounts immediately.
    store.openFile('/ws/seed.ts');

    const { findByTestId, findByRole, queryByTestId } = renderWithStore(
      '/session/abc123',
      store,
    );

    await findByTestId('editor-pane');
    // Both panes render in parallel before the close.
    await findByTestId('chat-pane');

    // F-394: EditorPane uses the PaneHeader primitive; close aria-label is
    // "Close pane". The chat leaf's close button says "Close session window",
    // so the `/close pane/i` regex uniquely matches the editor leaf.
    const close = await findByRole('button', { name: /close pane/i });
    close.click();

    // Editor leaf removed; chat leaf promoted to the whole grid.
    await waitFor(() => expect(queryByTestId('editor-pane')).toBeNull());
    await findByTestId('chat-pane');
    const tree = store.layouts.named[store.layouts.active]?.tree;
    expect(tree?.kind).toBe('leaf');
  });

  // -----------------------------------------------------------------------
  // F-150: drag-to-dock regression — dragging an editor pane header must
  // reposition the leaf in the grid the same way it does for any other
  // pane type. We drive a real pointer sequence against the editor's
  // breadcrumb header and assert the tree mutates via layoutStore.
  // Geometry and hit-testing are stubbed the same way
  // `useDragToDock.test.ts` stubs them, so the whole pointerdown → pointer-
  // move → pointerup path participates.
  // -----------------------------------------------------------------------

  it('drag-to-dock moves an editor leaf like any other grid pane', async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === 'session_hello') return helloAck;
      if (cmd === 'read_layouts') return defaultLayouts();
      if (cmd === 'write_layouts') return undefined;
      if (cmd === 'tree') {
        return { name: 'ws', path: '/ws', kind: 'Dir', children: [] };
      }
      if (cmd === 'read_file') {
        return { content: '', bytes: 0, sha256: '' };
      }
      return undefined;
    });

    const store = makeFakeLayoutStore();
    // Seed the tree with chat + editor side-by-side.
    store.openFile('/ws/seed.ts');
    const beforeTree = store.layouts.named[store.layouts.active]?.tree;
    if (beforeTree?.kind !== 'split') throw new Error('expected split');
    const editorLeaf = beforeTree.b.kind === 'leaf' ? beforeTree.b : null;
    const chatLeaf = beforeTree.a.kind === 'leaf' ? beforeTree.a : null;
    if (editorLeaf === null || chatLeaf === null) {
      throw new Error('expected two sibling leaves');
    }
    const editorId = editorLeaf.id;
    const chatId = chatLeaf.id;

    const { findByTestId } = renderWithStore('/session/abc123', store);
    await findByTestId('editor-pane');

    // Editor leaf is exposed as a drop target (data-leaf-id marker).
    const editorMarker = document.querySelector(
      `[data-leaf-id="${editorId}"]`,
    ) as HTMLElement | null;
    expect(editorMarker).not.toBeNull();

    // Stub leaf geometry so `useDragToDock`'s `elementFromPoint` +
    // `getBoundingClientRect` resolve to the two leaves. Chat on the left
    // half, editor on the right half — matches the seeded v-split.
    const geometry: Record<
      string,
      { left: number; top: number; right: number; bottom: number; width: number; height: number }
    > = {
      [chatId]: { left: 0, top: 0, right: 500, bottom: 600, width: 500, height: 600 },
      [editorId]: {
        left: 500,
        top: 0,
        right: 1000,
        bottom: 600,
        width: 500,
        height: 600,
      },
    };
    const originalEfp = document.elementFromPoint;
    const originalRect = Element.prototype.getBoundingClientRect;
    Element.prototype.getBoundingClientRect = function stubbed(
      this: Element,
    ): DOMRect {
      if (this instanceof HTMLElement) {
        const id = this.getAttribute('data-leaf-id');
        if (id !== null && geometry[id] !== undefined) {
          const g = geometry[id];
          return {
            ...g,
            x: g.left,
            y: g.top,
            toJSON() {
              return g;
            },
          } as DOMRect;
        }
      }
      return originalRect.call(this);
    };
    document.elementFromPoint = function stubbed(
      x: number,
      y: number,
    ): Element | null {
      for (const [id, g] of Object.entries(geometry)) {
        if (x >= g.left && x <= g.right && y >= g.top && y <= g.bottom) {
          const el = document.querySelector(`[data-leaf-id="${id}"]`);
          if (el !== null) return el;
        }
      }
      return null;
    };

    try {
      // EditorPane's PaneHeader (F-394) is the drag source for the editor
      // leaf — it forwards `onHeaderPointerDown` to `onHeaderPointerDown`
      // on the primitive's <header>. Fire pointerdown on the PaneHeader
      // inside the editor section, then move + up over the chat leaf's
      // left edge to dock the editor on the far left.
      const editorSection = document.querySelector(
        '[data-testid="editor-pane"]',
      ) as HTMLElement | null;
      const editorHeader = editorSection?.querySelector(
        '.pane-header',
      ) as HTMLElement | null;
      expect(editorHeader).not.toBeNull();
      const pd = new MouseEvent('pointerdown', {
        bubbles: true,
        cancelable: true,
        clientX: 520,
        clientY: 10,
        button: 0,
      });
      Object.defineProperty(pd, 'pointerId', { value: 1 });
      editorHeader!.dispatchEvent(pd);

      const firePointer = (
        kind: 'pointermove' | 'pointerup',
        x: number,
        y: number,
      ) => {
        const ev = new MouseEvent(kind, {
          bubbles: true,
          cancelable: true,
          clientX: x,
          clientY: y,
        });
        Object.defineProperty(ev, 'pointerId', { value: 1 });
        window.dispatchEvent(ev);
      };
      // Dock onto the chat leaf's left edge.
      firePointer('pointermove', 10, 300);
      firePointer('pointerup', 10, 300);

      // Tree should have mutated — editor on the left now, chat on the
      // right — proving the editor leaf is a drag source the same as any
      // other pane. The exact shape matches `applyDockDrop` semantics.
      const after = store.layouts.named[store.layouts.active]?.tree;
      if (after?.kind !== 'split') throw new Error('expected split after drop');
      // Either (editor, chat) by id or an equivalent structural mutation.
      const ids: string[] = [];
      const walk = (n: LayoutTree) => {
        if (n.kind === 'leaf') ids.push(n.id);
        else {
          walk(n.a);
          walk(n.b);
        }
      };
      walk(after);
      expect(ids).toContain(editorId);
      expect(ids).toContain(chatId);
      // The editor leaf must now sit on the left half of the split.
      const leftmost = after.a.kind === 'leaf' ? after.a.id : null;
      expect(leftmost).toBe(editorId);
    } finally {
      Element.prototype.getBoundingClientRect = originalRect;
      document.elementFromPoint = originalEfp;
    }
  });
});

// F-748: crash-restart prompt integration. Validates the five states from
// the DoD end-to-end: detecting (header pill flips to error), prompting
// (overlay up with Restart/Close), restarting (spinner replaces buttons),
// restored (overlay tears down after success), restart-failed (overlay
// shows the error + retry/close). All states are driven through the
// `session:crashed` Tauri event + the `session_restart` IPC, mocked here.
describe('SessionWindow crash-restart prompt (F-748)', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    listenMock.mockReset();
    unlistenMock.mockReset();
    closeMock.mockReset();
    resetSessionEventStore();
    resetMessagesStore();
    resetSessionTelemetryStore();
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === 'session_hello') return helloAck;
      if (cmd === 'read_layouts') return defaultLayouts();
      if (cmd === 'write_layouts') return undefined;
      if (cmd === 'tree') {
        return {
          name: 'ws',
          path: '/ws',
          kind: 'Dir',
          children: [],
        };
      }
      return undefined;
    });
    setInvokeForTesting(invokeMock as never);
    listenMock.mockResolvedValue(unlistenMock);
  });
  afterEach(() => {
    setInvokeForTesting(null);
    __setInjectedLayoutStoreForTesting(null);
    cleanup();
  });

  /**
   * Capture the `session:crashed` Tauri handler that SessionWindow
   * registers on mount so individual tests can synthesise EOF without
   * spinning up a real bridge.
   */
  function listenWithCrashHandler() {
    let crashHandler:
      | ((event: { payload: { session_id: string; last_seq: number } }) => void)
      | null = null;
    listenMock.mockImplementation((channel: string, handler: never) => {
      if (channel === 'session:crashed') {
        crashHandler = handler as never;
      }
      return Promise.resolve(unlistenMock);
    });
    return () => crashHandler;
  }

  it('prompting: a `session:crashed` event flips the header pill to "error" and surfaces the overlay', async () => {
    const getCrashHandler = listenWithCrashHandler();
    const { findByTestId, queryByTestId } = renderAt('/session/abc123');
    await findByTestId('pane-header-subject');
    await waitFor(() => expect(getCrashHandler()).not.toBeNull());

    // Steady state: no overlay, no error pill.
    expect(queryByTestId('crash-restart-overlay')).toBeNull();
    expect(queryByTestId('pane-header-status')).toBeNull();

    // Synthesise a crash signal with a non-zero resume anchor.
    getCrashHandler()!({
      payload: { session_id: 'abc123', last_seq: 42 },
    });

    // Header pill flips to "error" (the DoD's "detecting/error" state).
    const pill = await findByTestId('pane-header-status');
    expect(pill.dataset.status).toBe('error');

    // The overlay's prompting variant is up.
    const overlay = await findByTestId('crash-restart-overlay');
    expect(overlay.dataset.state).toBe('prompting');
    await findByTestId('crash-restart-overlay-restart');
    await findByTestId('crash-restart-overlay-close');
  });

  it('restored: a successful session_restart tears down the overlay and clears the header pill', async () => {
    const getCrashHandler = listenWithCrashHandler();
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === 'session_hello') return helloAck;
      if (cmd === 'read_layouts') return defaultLayouts();
      if (cmd === 'write_layouts') return undefined;
      if (cmd === 'session_restart') return undefined;
      if (cmd === 'session_subscribe') return undefined;
      if (cmd === 'tree') {
        return { name: 'ws', path: '/ws', kind: 'Dir', children: [] };
      }
      return undefined;
    });

    const { findByTestId, queryByTestId } = renderAt('/session/abc123');
    await findByTestId('pane-header-subject');
    await waitFor(() => expect(getCrashHandler()).not.toBeNull());

    getCrashHandler()!({
      payload: { session_id: 'abc123', last_seq: 17 },
    });
    const overlay = await findByTestId('crash-restart-overlay');
    expect(overlay.dataset.state).toBe('prompting');

    // Click Restart — synthesise the user's affordance.
    fireEvent.click(await findByTestId('crash-restart-overlay-restart'));

    // The overlay tears down once `session_restart` + re-hello +
    // re-subscribe all resolve. The post-restart subscribe must ride on
    // the captured anchor (17) so the daemon's history replay produces
    // no duplicates — that's the DoD invariant.
    await waitFor(() => expect(queryByTestId('crash-restart-overlay')).toBeNull());
    expect(queryByTestId('pane-header-status')).toBeNull();
    expect(invokeMock).toHaveBeenCalledWith('session_restart', {
      input: {
        session_id: 'abc123',
        workspace_root: '/ws',
        agent: undefined,
        provider: undefined,
      },
    });
    expect(invokeMock).toHaveBeenCalledWith('session_subscribe', {
      sessionId: 'abc123',
      since: 17,
    });
  });

  it('restarting → restored: overlay shows the spinner while session_restart is in flight, then tears down on success', async () => {
    const getCrashHandler = listenWithCrashHandler();
    let resolveRestart: (() => void) | null = null;
    const restartPending = new Promise<void>((resolve) => {
      resolveRestart = resolve;
    });
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === 'session_hello') return helloAck;
      if (cmd === 'read_layouts') return defaultLayouts();
      if (cmd === 'write_layouts') return undefined;
      if (cmd === 'session_restart') {
        await restartPending;
        return undefined;
      }
      if (cmd === 'session_subscribe') return undefined;
      if (cmd === 'tree') {
        return { name: 'ws', path: '/ws', kind: 'Dir', children: [] };
      }
      return undefined;
    });

    const { findByTestId, queryByTestId } = renderAt('/session/abc123');
    await findByTestId('pane-header-subject');
    await waitFor(() => expect(getCrashHandler()).not.toBeNull());

    getCrashHandler()!({
      payload: { session_id: 'abc123', last_seq: 5 },
    });
    fireEvent.click(await findByTestId('crash-restart-overlay-restart'));

    // While `session_restart` is pending the overlay flips to
    // `restarting` and the affordance buttons are replaced.
    await waitFor(async () => {
      const overlay = await findByTestId('crash-restart-overlay');
      expect(overlay.dataset.state).toBe('restarting');
    });
    expect(queryByTestId('crash-restart-overlay-restart')).toBeNull();
    expect(queryByTestId('crash-restart-overlay-progress')).not.toBeNull();

    // Resolve the IPC — the overlay tears down on the next tick.
    resolveRestart!();
    await waitFor(() => expect(queryByTestId('crash-restart-overlay')).toBeNull());
  });

  it('restart-failed: a rejected session_restart surfaces the error and the retry+close affordances', async () => {
    const getCrashHandler = listenWithCrashHandler();
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === 'session_hello') return helloAck;
      if (cmd === 'read_layouts') return defaultLayouts();
      if (cmd === 'write_layouts') return undefined;
      if (cmd === 'session_restart') {
        throw new Error('session_restart: spawn forged: ENOENT');
      }
      if (cmd === 'tree') {
        return { name: 'ws', path: '/ws', kind: 'Dir', children: [] };
      }
      return undefined;
    });

    const { findByTestId } = renderAt('/session/abc123');
    await findByTestId('pane-header-subject');
    await waitFor(() => expect(getCrashHandler()).not.toBeNull());

    getCrashHandler()!({
      payload: { session_id: 'abc123', last_seq: 9 },
    });
    fireEvent.click(await findByTestId('crash-restart-overlay-restart'));

    const overlay = await findByTestId('crash-restart-overlay');
    await waitFor(() => expect(overlay.dataset.state).toBe('restart_failed'));
    const errorEl = await findByTestId('crash-restart-overlay-error');
    expect(errorEl.textContent).toContain('ENOENT');
    await findByTestId('crash-restart-overlay-retry');
    await findByTestId('crash-restart-overlay-close');
  });

  it('composer draft preservation: ChatPane stays mounted under the overlay so the local composer text survives a crash', async () => {
    const getCrashHandler = listenWithCrashHandler();
    const { findByTestId } = renderAt('/session/abc123');
    await findByTestId('pane-header-subject');
    await waitFor(() => expect(getCrashHandler()).not.toBeNull());

    // Pre-crash: user types into the composer.
    const composer = (await findByTestId(
      'composer-textarea',
    )) as HTMLTextAreaElement;
    fireEvent.input(composer, { target: { value: 'half-written prompt' } });
    expect(composer.value).toBe('half-written prompt');

    // Crash signal arrives — overlay appears on top, but ChatPane (and
    // the composer) stays mounted underneath.
    getCrashHandler()!({
      payload: { session_id: 'abc123', last_seq: 1 },
    });
    await findByTestId('crash-restart-overlay');

    // The composer's value is still the user's draft — preserved across
    // the crash because the component never unmounted.
    const stillThere = (await findByTestId(
      'composer-textarea',
    )) as HTMLTextAreaElement;
    expect(stillThere.value).toBe('half-written prompt');
  });
});
