// F-731: TestConnectionButton tests. The button no longer renders
// inline success / error pills — outcomes are pushed to the toast
// queue via `pushToast`. The inline `probing` pill is the only visible
// indicator during the IPC roundtrip.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';

import { TestConnectionButton } from './TestConnectionButton';
import { setInvokeForTesting } from '../lib/tauri';
import type { TestProviderConnectionOutput } from '@forge/ipc';

type ToastKind = 'info' | 'warning' | 'error';

interface StubOptions {
  test?: (input: unknown) => Promise<TestProviderConnectionOutput>;
}

function installInvokeStub(opts: StubOptions = {}): {
  calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }>;
} {
  const calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }> = [];
  const test =
    opts.test ??
    (async () =>
      ({
        ok: true,
        latency_ms: 42n as unknown as bigint,
        model_count: 3,
      }) satisfies TestProviderConnectionOutput);
  setInvokeForTesting(
    (async (cmd: string, args?: Record<string, unknown>) => {
      calls.push({ cmd, args });
      if (cmd === 'test_provider_connection') {
        return test(args?.['input']);
      }
      return undefined;
    }) as never,
  );
  return { calls };
}

interface RenderWithToast {
  toast: ReturnType<typeof vi.fn>;
}

function renderButton(
  providerId: string,
  extras: RenderWithToast = { toast: vi.fn() },
) {
  return {
    toast: extras.toast,
    ...render(() => (
      <TestConnectionButton providerId={providerId} pushToast={extras.toast} />
    )),
  };
}

afterEach(() => {
  setInvokeForTesting(null);
  cleanup();
});

// ---------------------------------------------------------------------------
// idle — initial paint
// ---------------------------------------------------------------------------

describe('TestConnectionButton idle state', () => {
  beforeEach(() => {
    installInvokeStub();
  });

  it('renders the Test button alone on first paint', () => {
    const { getByTestId, queryByTestId } = renderButton('anthropic');
    const btn = getByTestId('test-connection-button-anthropic');
    expect(btn).toBeInTheDocument();
    expect(btn).toHaveAttribute('aria-busy', 'false');
    expect(queryByTestId('test-connection-pill-anthropic')).toBeNull();
  });

  it('tags the wrapper with data-state="idle"', () => {
    const { container } = renderButton('anthropic');
    const root = container.querySelector('.test-connection');
    expect(root).toHaveAttribute('data-state', 'idle');
    expect(root).toHaveAttribute('data-provider-id', 'anthropic');
  });
});

// ---------------------------------------------------------------------------
// probing — IPC in flight
// ---------------------------------------------------------------------------

describe('TestConnectionButton probing state', () => {
  it('flips to probing while the IPC is in flight', async () => {
    let release!: (value: TestProviderConnectionOutput) => void;
    const pending = new Promise<TestProviderConnectionOutput>((res) => {
      release = res;
    });
    installInvokeStub({ test: () => pending });

    const { container, getByTestId } = renderButton('anthropic');
    fireEvent.click(getByTestId('test-connection-button-anthropic'));

    await waitFor(() => {
      const root = container.querySelector('.test-connection');
      expect(root).toHaveAttribute('data-state', 'probing');
    });
    expect(getByTestId('test-connection-button-anthropic')).toHaveAttribute(
      'aria-busy',
      'true',
    );
    // While probing, the Test label is replaced by a ring spinner.
    expect(getByTestId('test-connection-spinner-anthropic')).toBeInTheDocument();
    expect(
      getByTestId('test-connection-button-anthropic').textContent,
    ).not.toContain('Test');

    release({ ok: true, latency_ms: 12n as unknown as bigint } as TestProviderConnectionOutput);
  });

  it('ignores a click while a probe is already in flight', async () => {
    let release!: (value: TestProviderConnectionOutput) => void;
    const pending = new Promise<TestProviderConnectionOutput>((res) => {
      release = res;
    });
    const { calls } = installInvokeStub({ test: () => pending });

    const { getByTestId } = renderButton('anthropic');
    const btn = getByTestId('test-connection-button-anthropic');
    fireEvent.click(btn);
    await waitFor(() => expect(btn).toHaveAttribute('aria-busy', 'true'));
    fireEvent.click(btn);
    fireEvent.click(btn);
    expect(calls.filter((c) => c.cmd === 'test_provider_connection')).toHaveLength(1);

    release({ ok: true } as TestProviderConnectionOutput);
  });
});

// ---------------------------------------------------------------------------
// success — info toast with latency
// ---------------------------------------------------------------------------

describe('TestConnectionButton success path', () => {
  it('pushes an info toast carrying the provider id and latency', async () => {
    installInvokeStub({
      test: async () =>
        ({
          ok: true,
          latency_ms: 142n as unknown as bigint,
          model_count: 7,
        }) satisfies TestProviderConnectionOutput,
    });
    const toast = vi.fn<(kind: ToastKind, msg: string) => void>();
    const { getByTestId } = renderButton('anthropic', { toast });

    fireEvent.click(getByTestId('test-connection-button-anthropic'));
    await waitFor(() => expect(toast).toHaveBeenCalled());
    expect(toast).toHaveBeenCalledWith(
      'info',
      'anthropic: connection succeeded (142ms)',
    );
    // Button returns to idle so the user can retry immediately.
    expect(getByTestId('test-connection-button-anthropic')).toHaveAttribute(
      'aria-busy',
      'false',
    );
  });

  it('omits the latency suffix when the daemon does not report one', async () => {
    installInvokeStub({
      test: async () => ({ ok: true }) satisfies TestProviderConnectionOutput,
    });
    const toast = vi.fn<(kind: ToastKind, msg: string) => void>();
    const { getByTestId } = renderButton('anthropic', { toast });

    fireEvent.click(getByTestId('test-connection-button-anthropic'));
    await waitFor(() => expect(toast).toHaveBeenCalled());
    expect(toast).toHaveBeenCalledWith('info', 'anthropic: connection succeeded');
  });
});

// ---------------------------------------------------------------------------
// error — auth-failure vs network-failure variants
// ---------------------------------------------------------------------------

describe('TestConnectionButton error path', () => {
  it('pushes an error toast tagged "auth required" when the error begins with `test_provider_connection: auth `', async () => {
    const verbatim = 'test_provider_connection: auth HTTP 401';
    installInvokeStub({
      test: async () => {
        throw new Error(verbatim);
      },
    });
    const toast = vi.fn<(kind: ToastKind, msg: string) => void>();
    const { getByTestId } = renderButton('anthropic', { toast });

    fireEvent.click(getByTestId('test-connection-button-anthropic'));
    await waitFor(() => expect(toast).toHaveBeenCalled());
    const [kind, message] = toast.mock.calls[0]!;
    expect(kind).toBe('error');
    expect(message).toBe('anthropic: auth required — auth HTTP 401');
  });

  it('pushes an error toast tagged "unreachable" for non-auth failures', async () => {
    const verbatim = 'test_provider_connection: network HTTP 503';
    installInvokeStub({
      test: async () => {
        throw new Error(verbatim);
      },
    });
    const toast = vi.fn<(kind: ToastKind, msg: string) => void>();
    const { getByTestId } = renderButton('anthropic', { toast });

    fireEvent.click(getByTestId('test-connection-button-anthropic'));
    await waitFor(() => expect(toast).toHaveBeenCalled());
    const [kind, message] = toast.mock.calls[0]!;
    expect(kind).toBe('error');
    expect(message).toBe('anthropic: unreachable — network HTTP 503');
  });
});

// ---------------------------------------------------------------------------
// invoke payload shape
// ---------------------------------------------------------------------------

describe('TestConnectionButton invoke payload', () => {
  it('invokes test_provider_connection with the wrapped {input} shape', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId } = renderButton('custom_openai:vllm');
    fireEvent.click(getByTestId('test-connection-button-custom_openai:vllm'));

    await waitFor(() => {
      const call = calls.find((c) => c.cmd === 'test_provider_connection');
      expect(call?.args).toEqual({
        input: { provider_id: 'custom_openai:vllm' },
      });
    });
  });

  it('lets the operator retry after a failure', async () => {
    let attempts = 0;
    installInvokeStub({
      test: async () => {
        attempts += 1;
        if (attempts === 1) throw new Error('test_provider_connection: network down');
        return { ok: true, latency_ms: 10n as unknown as bigint } as TestProviderConnectionOutput;
      },
    });
    const toast = vi.fn<(kind: ToastKind, msg: string) => void>();
    const { getByTestId } = renderButton('anthropic', { toast });

    fireEvent.click(getByTestId('test-connection-button-anthropic'));
    await waitFor(() => expect(toast).toHaveBeenCalledTimes(1));
    expect(toast.mock.calls[0]![0]).toBe('error');

    fireEvent.click(getByTestId('test-connection-button-anthropic'));
    await waitFor(() => expect(toast).toHaveBeenCalledTimes(2));
    const [secondKind, secondMessage] = toast.mock.calls[1]!;
    expect(secondKind).toBe('info');
    expect(secondMessage).toBe('anthropic: connection succeeded (10ms)');
  });
});
