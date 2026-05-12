import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { cleanup, render } from '@solidjs/testing-library';
import { MemoryRouter, Route, createMemoryHistory } from '@solidjs/router';
import { Catalog } from './Catalog';
import { setInvokeForTesting } from '../lib/tauri';
import { setActiveWorkspaceRoot } from '../stores/session';

const invokeMock = vi.fn();

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue([]);
  setInvokeForTesting(invokeMock as never);
  setActiveWorkspaceRoot(null);
});

afterEach(() => {
  setInvokeForTesting(null);
  setActiveWorkspaceRoot(null);
  cleanup();
});

function renderAt(path: string) {
  const history = createMemoryHistory();
  history.set({ value: path });
  return render(() => (
    <MemoryRouter history={history}>
      <Route path="/catalog/:kind?" component={Catalog} />
    </MemoryRouter>
  ));
}

async function flush() {
  for (let i = 0; i < 6; i += 1) {
    await Promise.resolve();
  }
}

describe('<Catalog> route (F-592)', () => {
  it('renders the catalog with empty workspaceRoot when no workspace is set so user-tier entries load', async () => {
    // The backend treats `workspaceRoot=""` as "user-tier only" and
    // returns globally-installed skills / MCP / agents from `~/.agent-skills`,
    // `~/.mcp.json`, `~/.agents`. The catalog must always render so
    // those globals are reachable — no workspace-picker gate.
    const { findByLabelText } = renderAt('/catalog');
    await flush();

    expect(await findByLabelText('Filter catalog entries')).toBeTruthy();
    expect(invokeMock).toHaveBeenCalledWith('list_skills', {
      workspaceRoot: '',
      scope: { type: 'SessionWide' },
    });
    expect(invokeMock).toHaveBeenCalledWith('list_mcp_servers', {
      workspaceRoot: '',
      scope: { type: 'SessionWide' },
    });
    expect(invokeMock).toHaveBeenCalledWith('list_agents', {
      workspaceRoot: '',
      scope: { type: 'SessionWide' },
    });
  });

  it('falls back to activeWorkspaceRoot when ?ws= is absent so sidebar nav (no query) works', async () => {
    setActiveWorkspaceRoot('/home/user/proj');
    const { findByLabelText } = renderAt('/catalog/mcp');
    await flush();

    expect(await findByLabelText('Filter catalog entries')).toBeTruthy();
    expect(invokeMock).toHaveBeenCalledWith('list_mcp_servers', {
      workspaceRoot: '/home/user/proj',
      scope: { type: 'SessionWide' },
    });
  });

  it('mounts <CatalogPane> with the workspaceRoot from ?ws= and triggers list_* fetches', async () => {
    const { findByLabelText } = renderAt('/catalog?ws=%2Fhome%2Fuser%2Fproj');
    await flush();

    expect(await findByLabelText('Filter catalog entries')).toBeTruthy();
    expect(invokeMock).toHaveBeenCalledWith('list_skills', {
      workspaceRoot: '/home/user/proj',
      scope: { type: 'SessionWide' },
    });
    expect(invokeMock).toHaveBeenCalledWith('list_mcp_servers', {
      workspaceRoot: '/home/user/proj',
      scope: { type: 'SessionWide' },
    });
    expect(invokeMock).toHaveBeenCalledWith('list_agents', {
      workspaceRoot: '/home/user/proj',
      scope: { type: 'SessionWide' },
    });
  });
});
