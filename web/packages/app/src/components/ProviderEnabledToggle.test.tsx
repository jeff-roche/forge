// F-733: ProviderEnabledToggle tests.
//
// Coverage:
//   - toggle on→off invokes set_provider_enabled with `enabled: false`
//   - toggle off→on invokes set_provider_enabled with `enabled: true`
//   - rejection reverts the visual state and surfaces the verbatim error
//   - aria-busy is set while the IPC is in flight
//   - role="switch" + aria-checked reflect the optimistic state

import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';

import { ProviderEnabledToggle } from './ProviderEnabledToggle';
import { setInvokeForTesting } from '../lib/tauri';

interface StubOptions {
  setProviderEnabled?: (input: unknown) => Promise<unknown>;
}

function installInvokeStub(opts: StubOptions = {}): {
  calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }>;
} {
  const calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }> = [];
  const setProviderEnabled = opts.setProviderEnabled ?? (async () => undefined);
  setInvokeForTesting(
    (async (cmd: string, args?: Record<string, unknown>) => {
      calls.push({ cmd, args });
      switch (cmd) {
        case 'set_provider_enabled':
          return setProviderEnabled(args?.['input']);
        default:
          return undefined;
      }
    }) as never,
  );
  return { calls };
}

function renderToggle(
  props: { providerId?: string; enabled?: boolean; onToggled?: (next: boolean) => void } = {},
) {
  const providerId = props.providerId ?? 'anthropic';
  const enabled = props.enabled ?? true;
  const onToggled = props.onToggled ?? vi.fn();
  const result = render(() => (
    <ProviderEnabledToggle
      providerId={providerId}
      enabled={enabled}
      onToggled={onToggled}
    />
  ));
  return { ...result, providerId, onToggled };
}

afterEach(() => {
  cleanup();
  setInvokeForTesting(null);
});

describe('<ProviderEnabledToggle> (F-733)', () => {
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

  it('toggling on→off fires set_provider_enabled with enabled=false', async () => {
    const { calls } = installInvokeStub();
    const onToggled = vi.fn();
    const { getByRole } = renderToggle({
      providerId: 'anthropic',
      enabled: true,
      onToggled,
    });
    const sw = getByRole('switch') as HTMLInputElement;

    fireEvent.click(sw);

    await waitFor(() => {
      const hit = calls.find((c) => c.cmd === 'set_provider_enabled');
      expect(hit).toBeTruthy();
      expect(hit?.args?.['input']).toEqual({ id: 'anthropic', enabled: false });
    });
    await waitFor(() => expect(onToggled).toHaveBeenCalledWith(false));
  });

  it('toggling off→on fires set_provider_enabled with enabled=true', async () => {
    const { calls } = installInvokeStub();
    const onToggled = vi.fn();
    const { getByRole } = renderToggle({
      providerId: 'openai',
      enabled: false,
      onToggled,
    });
    const sw = getByRole('switch') as HTMLInputElement;

    fireEvent.click(sw);

    await waitFor(() => {
      const hit = calls.find((c) => c.cmd === 'set_provider_enabled');
      expect(hit).toBeTruthy();
      expect(hit?.args?.['input']).toEqual({ id: 'openai', enabled: true });
    });
    await waitFor(() => expect(onToggled).toHaveBeenCalledWith(true));
  });

  it('reverts the visual state and surfaces the verbatim error on rejection', async () => {
    installInvokeStub({
      setProviderEnabled: async () => {
        throw new Error('set_provider_enabled: provider anthropic not configured');
      },
    });
    const { getByRole, findByTestId } = renderToggle({
      providerId: 'anthropic',
      enabled: true,
    });
    const sw = getByRole('switch') as HTMLInputElement;
    fireEvent.click(sw);

    const alert = await findByTestId('provider-enabled-toggle-error-anthropic');
    expect(alert.textContent).toContain(
      'set_provider_enabled: provider anthropic not configured',
    );

    // Visual reverted to the original enabled=true state.
    await waitFor(() => expect(sw.getAttribute('aria-checked')).toBe('true'));
  });

  it('marks the switch aria-busy while the IPC is in flight', async () => {
    let releaseSet!: () => void;
    const pending = new Promise<void>((res) => {
      releaseSet = res;
    });
    installInvokeStub({
      setProviderEnabled: async () => {
        await pending;
      },
    });

    const { getByRole } = renderToggle({ enabled: true });
    const sw = getByRole('switch') as HTMLInputElement;
    fireEvent.click(sw);

    await waitFor(() => expect(sw.getAttribute('aria-busy')).toBe('true'));

    releaseSet();
    await waitFor(() => expect(sw.getAttribute('aria-busy')).toBe('false'));
  });
});
