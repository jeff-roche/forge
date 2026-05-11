// F-716: AppShell route-mount tests. AppShell wraps every top-level route
// and supplies the ActivityBar + StatusBar on all routes plus a
// session-scoped FilesSidebar slot. These tests pin the route-agnostic
// contract: the chrome is always present; FilesSidebar mounts only on the
// session route and only when its bridge (activeWorkspaceRoot + an
// activeOpenFile callback) is wired.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, waitFor } from '@solidjs/testing-library';
import { MemoryRouter, Route, createMemoryHistory } from '@solidjs/router';
import { AppShell } from './AppShell';
import {
  setActiveOpenFile,
  setActiveSessionId,
  setActiveWorkspaceRoot,
} from '../stores/session';
import { setInvokeForTesting } from '../lib/tauri';

// StatusBar mounts on every route; its bg-agents subscriber attaches a
// `session:event` listener via Tauri. Stub the event channel to a no-op
// unlisten so jsdom never reaches the missing Tauri bridge.
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async () => () => undefined),
}));

// CommandPalette mounts a global keydown listener; nothing else needed.

const DASHBOARD_TESTID = 'route-content-dashboard';
const SESSION_TESTID = 'route-content-session';
const USAGE_TESTID = 'route-content-usage';

function Stub(testid: string) {
  return () => <div data-testid={testid}>{testid}</div>;
}

function renderAt(path: string) {
  const history = createMemoryHistory();
  history.set({ value: path });
  return render(() => (
    <MemoryRouter history={history} root={AppShell}>
      <Route path="/" component={Stub(DASHBOARD_TESTID)} />
      <Route path="/session/:id" component={Stub(SESSION_TESTID)} />
      <Route path="/usage" component={Stub(USAGE_TESTID)} />
    </MemoryRouter>
  ));
}

describe('AppShell', () => {
  beforeEach(() => {
    // StatusBar's bg-agents snapshot call goes through `invoke`. Hermetic
    // stub keeps mount deterministic. `get_settings` covers the settings
    // store seeding triggered by surrounding initialization.
    setInvokeForTesting(
      (async () => undefined) as never,
    );
  });

  afterEach(() => {
    setInvokeForTesting(null);
    setActiveSessionId(null);
    setActiveWorkspaceRoot(null);
    setActiveOpenFile(null);
    cleanup();
  });

  it('renders the ActivityBar on the Dashboard route', async () => {
    const { findByTestId } = renderAt('/');
    expect(await findByTestId('activity-bar')).toBeInTheDocument();
    expect(await findByTestId(DASHBOARD_TESTID)).toBeInTheDocument();
  });

  it('renders the StatusBar on the Dashboard route', async () => {
    const { findByTestId } = renderAt('/');
    expect(await findByTestId('status-bar')).toBeInTheDocument();
  });

  it('renders the ActivityBar and StatusBar on the Session route', async () => {
    const { findByTestId } = renderAt('/session/abc123');
    expect(await findByTestId('activity-bar')).toBeInTheDocument();
    expect(await findByTestId('status-bar')).toBeInTheDocument();
    expect(await findByTestId(SESSION_TESTID)).toBeInTheDocument();
  });

  it('renders the ActivityBar and StatusBar on the Usage route', async () => {
    const { findByTestId } = renderAt('/usage');
    expect(await findByTestId('activity-bar')).toBeInTheDocument();
    expect(await findByTestId('status-bar')).toBeInTheDocument();
    expect(await findByTestId(USAGE_TESTID)).toBeInTheDocument();
  });

  it('renders the matched route content inside the shell', async () => {
    const { findByTestId } = renderAt('/usage');
    const shell = await findByTestId('app-shell');
    const content = await findByTestId(USAGE_TESTID);
    expect(shell.contains(content)).toBe(true);
  });

  it('does not mount FilesSidebar on non-session routes', async () => {
    setActiveWorkspaceRoot('/ws');
    const { findByTestId, queryByTestId } = renderAt('/');
    await findByTestId('activity-bar');
    // FilesSidebar is workspace chrome — it must not appear on Dashboard
    // even when the workspace bridge happens to be populated from a
    // previous session.
    expect(queryByTestId('files-sidebar')).toBeNull();
  });

  it('keeps FilesSidebar hidden on the session route until the Files activity is selected', async () => {
    setActiveWorkspaceRoot('/ws');
    const { findByTestId, queryByTestId } = renderAt('/session/abc123');
    await findByTestId('activity-bar');
    expect(queryByTestId('files-sidebar')).toBeNull();
  });

  it('mounts FilesSidebar on the session route when Files is selected and workspace is known', async () => {
    setActiveSessionId('abc123' as never);
    setActiveWorkspaceRoot('/ws');
    setActiveOpenFile(() => () => undefined);
    // FilesSidebar reads the tree via Tauri on mount; stub the response so
    // it renders cleanly.
    setInvokeForTesting(
      (async (cmd: string) => {
        if (cmd === 'tree') {
          return { name: 'ws', path: '/ws', kind: 'Dir', children: [] };
        }
        return undefined;
      }) as never,
    );

    const { findByTestId } = renderAt('/session/abc123');
    const filesBtn = await findByTestId('activity-bar-files');
    filesBtn.click();
    expect(await findByTestId('files-sidebar')).toBeInTheDocument();
  });

  it('toggles FilesSidebar via the Cmd/Ctrl+Shift+E shortcut on the session route', async () => {
    setActiveSessionId('abc123' as never);
    setActiveWorkspaceRoot('/ws');
    setActiveOpenFile(() => () => undefined);
    setInvokeForTesting(
      (async (cmd: string) => {
        if (cmd === 'tree') {
          return { name: 'ws', path: '/ws', kind: 'Dir', children: [] };
        }
        return undefined;
      }) as never,
    );

    const { findByTestId, queryByTestId } = renderAt('/session/abc123');
    await findByTestId('activity-bar');

    window.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'E', metaKey: true, shiftKey: true }),
    );
    await findByTestId('files-sidebar');

    window.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'E', metaKey: true, shiftKey: true }),
    );
    await waitFor(() => expect(queryByTestId('files-sidebar')).toBeNull());
  });

  it('does not surface FilesSidebar on the dashboard even with Files selected', async () => {
    setActiveWorkspaceRoot('/ws');
    const { findByTestId, queryByTestId } = renderAt('/');
    const filesBtn = await findByTestId('activity-bar-files');
    filesBtn.click();
    // FilesSidebar is gated behind `isSessionRoute()` — selecting the
    // activity on a non-session route must not mount the workspace tree.
    expect(queryByTestId('files-sidebar')).toBeNull();
  });
});
