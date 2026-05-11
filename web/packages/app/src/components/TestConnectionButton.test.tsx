// F-731: TestConnectionButton tests. Covers the four states from
// docs/ui-specs/providers-page.md §Per-test (idle / probing / success / error),
// the auth-failure variant detection, the verbatim error rendering, and the
// invoke payload shape.

import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';

import { TestConnectionButton } from './TestConnectionButton';
import { setInvokeForTesting } from '../lib/tauri';
import type { TestProviderConnectionOutput } from '@forge/ipc';

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

  it('renders the Test button and the `unknown` pill on first paint', () => {
    const { getByTestId } = render(() => (
      <TestConnectionButton providerId="anthropic" />
    ));
    const btn = getByTestId('test-connection-button-anthropic');
    expect(btn).toBeInTheDocument();
    expect(btn).toHaveAttribute('aria-busy', 'false');
    const pill = getByTestId('test-connection-pill-anthropic');
    expect(pill.textContent).toBe('unknown');
  });

  it('tags the wrapper with data-state="idle" so the dashboard list can target it', () => {
    const { container } = render(() => (
      <TestConnectionButton providerId="anthropic" />
    ));
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

    const { container, getByTestId } = render(() => (
      <TestConnectionButton providerId="anthropic" />
    ));

    fireEvent.click(getByTestId('test-connection-button-anthropic'));

    await waitFor(() => {
      const root = container.querySelector('.test-connection');
      expect(root).toHaveAttribute('data-state', 'probing');
    });
    expect(getByTestId('test-connection-button-anthropic')).toHaveAttribute(
      'aria-busy',
      'true',
    );
    expect(getByTestId('test-connection-pill-anthropic').textContent).toBe('probing');

    release({ ok: true, latency_ms: 12n as unknown as bigint } as TestProviderConnectionOutput);
  });

  it('ignores a click while a probe is already in flight', async () => {
    let release!: (value: TestProviderConnectionOutput) => void;
    const pending = new Promise<TestProviderConnectionOutput>((res) => {
      release = res;
    });
    const { calls } = installInvokeStub({ test: () => pending });

    const { getByTestId } = render(() => (
      <TestConnectionButton providerId="anthropic" />
    ));
    const btn = getByTestId('test-connection-button-anthropic');
    fireEvent.click(btn);
    await waitFor(() => expect(btn).toHaveAttribute('aria-busy', 'true'));
    fireEvent.click(btn);
    fireEvent.click(btn);
    // Only the first click reached invoke.
    expect(calls.filter((c) => c.cmd === 'test_provider_connection')).toHaveLength(1);

    release({ ok: true } as TestProviderConnectionOutput);
  });
});

// ---------------------------------------------------------------------------
// success — ready pill with latency
// ---------------------------------------------------------------------------

describe('TestConnectionButton success state', () => {
  it('renders the ready pill with latency on success', async () => {
    installInvokeStub({
      test: async () =>
        ({
          ok: true,
          latency_ms: 142n as unknown as bigint,
          model_count: 7,
        }) satisfies TestProviderConnectionOutput,
    });

    const { getByTestId } = render(() => (
      <TestConnectionButton providerId="anthropic" />
    ));
    fireEvent.click(getByTestId('test-connection-button-anthropic'));

    await waitFor(() => {
      expect(getByTestId('test-connection-pill-anthropic').textContent).toBe(
        'ready 142ms',
      );
    });
  });

  it('omits the latency suffix when the daemon does not report one', async () => {
    installInvokeStub({
      test: async () =>
        ({ ok: true }) satisfies TestProviderConnectionOutput,
    });

    const { getByTestId } = render(() => (
      <TestConnectionButton providerId="anthropic" />
    ));
    fireEvent.click(getByTestId('test-connection-button-anthropic'));

    await waitFor(() => {
      expect(getByTestId('test-connection-pill-anthropic').textContent).toBe('ready');
    });
  });
});

// ---------------------------------------------------------------------------
// error — auth-failure vs network-failure variants
// ---------------------------------------------------------------------------

describe('TestConnectionButton error state', () => {
  it('renders the auth-required pill when the error begins with `test_provider_connection: auth `', async () => {
    const verbatim = 'test_provider_connection: auth HTTP 401';
    installInvokeStub({
      test: async () => {
        throw new Error(verbatim);
      },
    });

    const { getByTestId } = render(() => (
      <TestConnectionButton providerId="anthropic" />
    ));
    fireEvent.click(getByTestId('test-connection-button-anthropic'));

    await waitFor(() => {
      expect(getByTestId('test-connection-pill-anthropic').textContent).toBe(
        'auth-required',
      );
    });
    const alert = getByTestId('test-connection-error-anthropic');
    expect(alert).toHaveAttribute('role', 'alert');
    expect(alert.textContent).toBe(verbatim);
  });

  it('renders the generic unreachable pill for non-auth failures', async () => {
    const verbatim = 'test_provider_connection: network HTTP 503';
    installInvokeStub({
      test: async () => {
        throw new Error(verbatim);
      },
    });

    const { getByTestId } = render(() => (
      <TestConnectionButton providerId="anthropic" />
    ));
    fireEvent.click(getByTestId('test-connection-button-anthropic'));

    await waitFor(() => {
      expect(getByTestId('test-connection-pill-anthropic').textContent).toBe(
        'unreachable',
      );
    });
    expect(getByTestId('test-connection-error-anthropic').textContent).toBe(verbatim);
  });
});

// ---------------------------------------------------------------------------
// invoke payload shape
// ---------------------------------------------------------------------------

describe('TestConnectionButton invoke payload', () => {
  it('invokes test_provider_connection with the wrapped {input} shape', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId } = render(() => (
      <TestConnectionButton providerId="custom_openai:vllm" />
    ));
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

    const { getByTestId } = render(() => (
      <TestConnectionButton providerId="anthropic" />
    ));
    fireEvent.click(getByTestId('test-connection-button-anthropic'));
    await waitFor(() => {
      expect(getByTestId('test-connection-pill-anthropic').textContent).toBe(
        'unreachable',
      );
    });
    // Retry — pill flips back to ready.
    fireEvent.click(getByTestId('test-connection-button-anthropic'));
    await waitFor(() => {
      expect(getByTestId('test-connection-pill-anthropic').textContent).toBe(
        'ready 10ms',
      );
    });
  });
});
