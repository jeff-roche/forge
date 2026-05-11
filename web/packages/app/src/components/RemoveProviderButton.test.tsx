// F-732: RemoveProviderButton tests.
//
// Coverage:
//   - confirm step renders on trigger click
//   - cancel closes the dialog without firing remove_provider
//   - accept fires remove_provider with the correct payload
//   - onRemoved fires after a successful remove
//   - verbatim daemon errors surface in role="alert" without re-throwing

import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';

import { RemoveProviderButton } from './RemoveProviderButton';
import { setInvokeForTesting } from '../lib/tauri';

interface StubOptions {
  removeProvider?: (input: unknown) => Promise<unknown>;
}

function installInvokeStub(opts: StubOptions = {}): {
  calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }>;
} {
  const calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }> = [];
  const removeProvider = opts.removeProvider ?? (async () => undefined);
  setInvokeForTesting(
    (async (cmd: string, args?: Record<string, unknown>) => {
      calls.push({ cmd, args });
      switch (cmd) {
        case 'remove_provider':
          return removeProvider(args?.['input']);
        default:
          return undefined;
      }
    }) as never,
  );
  return { calls };
}

function renderButton(
  props: { providerId?: string; onRemoved?: () => void } = {},
) {
  const providerId = props.providerId ?? 'custom_openai:vllm';
  const onRemoved = props.onRemoved ?? vi.fn();
  const result = render(() => (
    <RemoveProviderButton providerId={providerId} onRemoved={onRemoved} />
  ));
  return { ...result, providerId, onRemoved };
}

afterEach(() => {
  setInvokeForTesting(null);
  cleanup();
});

describe('RemoveProviderButton', () => {
  it('renders the trigger and does not show the dialog initially', () => {
    installInvokeStub();
    const { getByTestId, queryByTestId, providerId } = renderButton();
    expect(getByTestId(`remove-provider-trigger-${providerId}`)).toBeInTheDocument();
    expect(queryByTestId(`remove-provider-confirm-${providerId}`)).toBeNull();
  });

  it('opens the confirm dialog when the trigger is clicked', () => {
    installInvokeStub();
    const { getByTestId, providerId } = renderButton();
    fireEvent.click(getByTestId(`remove-provider-trigger-${providerId}`));
    const dialog = getByTestId(`remove-provider-confirm-${providerId}`);
    expect(dialog).toBeInTheDocument();
    // Dialog chrome contract.
    const inner = dialog.querySelector('[role="dialog"]');
    expect(inner).not.toBeNull();
    expect(inner?.getAttribute('aria-modal')).toBe('true');
  });

  it('cancel closes the dialog without firing remove_provider', () => {
    const { calls } = installInvokeStub();
    const { getByTestId, queryByTestId, providerId } = renderButton();
    fireEvent.click(getByTestId(`remove-provider-trigger-${providerId}`));
    fireEvent.click(getByTestId(`remove-provider-cancel-${providerId}`));
    expect(queryByTestId(`remove-provider-confirm-${providerId}`)).toBeNull();
    expect(calls.some((c) => c.cmd === 'remove_provider')).toBe(false);
  });

  it('confirm fires remove_provider with the wire payload', async () => {
    const { calls } = installInvokeStub();
    const onRemoved = vi.fn();
    const { getByTestId, providerId } = renderButton({ onRemoved });
    fireEvent.click(getByTestId(`remove-provider-trigger-${providerId}`));
    fireEvent.click(getByTestId(`remove-provider-confirm-btn-${providerId}`));
    await waitFor(() => {
      const remove = calls.find((c) => c.cmd === 'remove_provider');
      expect(remove?.args).toEqual({ input: { id: providerId } });
    });
    await waitFor(() => expect(onRemoved).toHaveBeenCalledTimes(1));
  });

  it('renders the verbatim daemon error with role="alert"', async () => {
    const msg = 'remove_provider: provider custom_openai:vllm not configured';
    installInvokeStub({
      removeProvider: async () => {
        throw new Error(msg);
      },
    });
    const onRemoved = vi.fn();
    const { getByTestId, providerId } = renderButton({ onRemoved });
    fireEvent.click(getByTestId(`remove-provider-trigger-${providerId}`));
    fireEvent.click(getByTestId(`remove-provider-confirm-btn-${providerId}`));
    await waitFor(() =>
      expect(getByTestId(`remove-provider-error-${providerId}`)).toBeInTheDocument(),
    );
    const alert = getByTestId(`remove-provider-error-${providerId}`);
    expect(alert).toHaveAttribute('role', 'alert');
    expect(alert).toHaveTextContent(msg);
    // The onRemoved callback does NOT fire on error.
    expect(onRemoved).not.toHaveBeenCalled();
  });
});
