import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render } from '@solidjs/testing-library';
import { MemoryRouter, Route, createMemoryHistory } from '@solidjs/router';
import { Usage } from './Usage';
import { setInvokeForTesting } from '../lib/tauri';

const invokeMock = vi.fn();

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue({
    range: { type: 'Last30' },
    group_by: 'Provider',
    total_tokens_in: 0,
    total_tokens_out: 0,
    total_cost: null,
    breakdown: [],
  });
  setInvokeForTesting(invokeMock as never);
});

afterEach(() => {
  setInvokeForTesting(null);
  cleanup();
});

function renderAt(path: string) {
  const history = createMemoryHistory();
  history.set({ value: path });
  return render(() => (
    <MemoryRouter history={history}>
      <Route path="/usage" component={Usage} />
    </MemoryRouter>
  ));
}

async function flush() {
  for (let i = 0; i < 6; i += 1) {
    await Promise.resolve();
  }
}

describe('<Usage> route (F-594)', () => {
  it('mounts <UsagePane> and triggers a usage_summary call', async () => {
    const { findByText } = renderAt('/usage');
    await flush();

    expect(await findByText('Usage')).toBeTruthy();
    const calls = invokeMock.mock.calls.filter((c) => c[0] === 'usage_summary');
    expect(calls.length).toBeGreaterThan(0);
  });

  it('forwards ?ws= to the IPC workspace_root argument', async () => {
    renderAt('/usage?ws=%2Fhome%2Fuser%2Fproj');
    await flush();

    const calls = invokeMock.mock.calls.filter((c) => c[0] === 'usage_summary');
    expect(calls.length).toBeGreaterThan(0);
    const args = calls[0]?.[1] as { workspaceRoot: string };
    expect(args.workspaceRoot).toBe('/home/user/proj');
  });
});
