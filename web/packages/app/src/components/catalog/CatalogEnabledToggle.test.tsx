// F-735: CatalogEnabledToggle unit tests.
//
// Coverage:
//   - renders role="switch" with aria-checked reflecting the enabled prop
//   - toggling on→off invokes set_setting with `catalog.enabled.<kind>.<id>`
//   - rejection reverts the visual and forwards the verbatim detail to onError
//   - aria-busy is set while the IPC is in flight
//   - the inverted aria-label describes the *next* action, not the current state

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';

import { CatalogEnabledToggle } from './CatalogEnabledToggle';
import { setInvokeForTesting } from '../../lib/tauri';
import { resetSettingsStore } from '../../stores/settings';

interface StubOptions {
  setSetting?: (args: Record<string, unknown> | undefined) => Promise<unknown>;
}

function installInvokeStub(opts: StubOptions = {}): {
  calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }>;
} {
  const calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }> = [];
  const setSettingImpl = opts.setSetting ?? (async () => undefined);
  setInvokeForTesting(
    (async (cmd: string, args?: Record<string, unknown>) => {
      calls.push({ cmd, args });
      switch (cmd) {
        case 'set_setting':
          return setSettingImpl(args);
        default:
          return undefined;
      }
    }) as never,
  );
  return { calls };
}

interface RenderOpts {
  kind?: 'skills' | 'mcp' | 'agents';
  id?: string;
  name?: string;
  enabled?: boolean;
  onToggled?: (next: boolean) => void;
  onError?: (detail: string) => void;
}

function renderToggle(opts: RenderOpts = {}) {
  const kind = opts.kind ?? 'skills';
  const id = opts.id ?? 'tdd';
  const name = opts.name ?? id;
  const enabled = opts.enabled ?? true;
  const onToggled = opts.onToggled ?? vi.fn();
  const onError = opts.onError ?? vi.fn();
  const result = render(() => (
    <CatalogEnabledToggle
      kind={kind}
      id={id}
      name={name}
      enabled={enabled}
      level="user"
      workspaceRoot="/ws"
      onToggled={onToggled}
      onError={onError}
    />
  ));
  return { ...result, kind, id, name, onToggled, onError };
}

beforeEach(() => {
  resetSettingsStore();
});

afterEach(() => {
  cleanup();
  setInvokeForTesting(null);
});

describe('<CatalogEnabledToggle> (F-735)', () => {
  it('renders role="switch" with aria-checked reflecting the enabled prop', () => {
    installInvokeStub();
    const { getByRole } = renderToggle({ enabled: true });
    const sw = getByRole('switch');
    expect(sw.getAttribute('aria-checked')).toBe('true');
  });

  it('renders aria-checked=false when starting disabled', () => {
    installInvokeStub();
    const { getByRole } = renderToggle({ enabled: false });
    const sw = getByRole('switch');
    expect(sw.getAttribute('aria-checked')).toBe('false');
  });

  it('the aria-label inverts the current state to describe the next action', () => {
    installInvokeStub();
    const { getByRole, unmount } = renderToggle({ enabled: true, name: 'tdd' });
    expect(getByRole('switch').getAttribute('aria-label')).toBe('Disable tdd');
    unmount();

    const { getByRole: getByRoleOff } = renderToggle({
      enabled: false,
      name: 'tdd',
    });
    expect(getByRoleOff('switch').getAttribute('aria-label')).toBe('Enable tdd');
  });

  it('toggling on→off fires set_setting with the catalog.enabled.<kind>.<id> key', async () => {
    const { calls } = installInvokeStub();
    const onToggled = vi.fn();
    const { getByRole } = renderToggle({
      kind: 'mcp',
      id: 'github',
      enabled: true,
      onToggled,
    });
    fireEvent.click(getByRole('switch'));

    await waitFor(() => {
      const hit = calls.find((c) => c.cmd === 'set_setting');
      expect(hit).toBeTruthy();
      expect(hit?.args).toEqual({
        key: 'catalog.enabled.mcp.github',
        value: false,
        level: 'user',
        workspaceRoot: '/ws',
      });
    });
    await waitFor(() => expect(onToggled).toHaveBeenCalledWith(false));
  });

  it('toggling off→on fires set_setting with value=true', async () => {
    const { calls } = installInvokeStub();
    const { getByRole } = renderToggle({
      kind: 'agents',
      id: 'planner',
      enabled: false,
    });
    fireEvent.click(getByRole('switch'));

    await waitFor(() => {
      const hit = calls.find((c) => c.cmd === 'set_setting');
      expect(hit?.args).toEqual({
        key: 'catalog.enabled.agents.planner',
        value: true,
        level: 'user',
        workspaceRoot: '/ws',
      });
    });
  });

  it('reverts the visual and forwards the verbatim detail to onError on rejection', async () => {
    installInvokeStub({
      setSetting: async () => {
        throw new Error('value type mismatch');
      },
    });
    const onError = vi.fn();
    const { getByRole } = renderToggle({
      kind: 'skills',
      id: 'tdd',
      enabled: true,
      onError,
    });
    const sw = getByRole('switch') as HTMLInputElement;
    fireEvent.click(sw);

    await waitFor(() => expect(onError).toHaveBeenCalledWith('value type mismatch'));
    // Visual reverted to the original enabled=true state.
    await waitFor(() => expect(sw.getAttribute('aria-checked')).toBe('true'));
  });

  it('marks the switch aria-busy while the IPC is in flight', async () => {
    let release!: () => void;
    const pending = new Promise<void>((res) => {
      release = res;
    });
    installInvokeStub({ setSetting: async () => await pending });

    const { getByRole } = renderToggle({ enabled: true });
    const sw = getByRole('switch') as HTMLInputElement;
    fireEvent.click(sw);

    await waitFor(() => expect(sw.getAttribute('aria-busy')).toBe('true'));
    release();
    await waitFor(() => expect(sw.getAttribute('aria-busy')).toBe('false'));
  });

  it('optimistically flips aria-checked before the IPC resolves', async () => {
    let release!: () => void;
    const pending = new Promise<void>((res) => {
      release = res;
    });
    installInvokeStub({ setSetting: async () => await pending });

    const { getByRole } = renderToggle({ enabled: true });
    const sw = getByRole('switch') as HTMLInputElement;
    fireEvent.click(sw);

    // Optimistic flip lands synchronously; aria-checked already reflects the
    // next state while the IPC is still pending.
    await waitFor(() => expect(sw.getAttribute('aria-checked')).toBe('false'));
    release();
  });
});
