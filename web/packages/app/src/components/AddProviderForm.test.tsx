// F-730: AddProviderForm tests. Covers the four states from
// docs/ui-specs/providers-page.md §Per-form (idle / validating / saving /
// save-failed) plus field rendering per kind, invoke payload shape,
// success handoff, and verbatim error rendering.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';

import { AddProviderForm } from './AddProviderForm';
import { setInvokeForTesting } from '../lib/tauri';
import type { ProviderEntry } from '../ipc/dashboard';

interface StubOptions {
  addProvider?: (input: unknown) => Promise<unknown>;
  updateProvider?: (input: unknown) => Promise<unknown>;
  loginProvider?: (args: unknown) => Promise<unknown>;
}

const SAMPLE_ENTRY: ProviderEntry = {
  id: 'anthropic',
  display_name: 'Anthropic',
  credential_required: true,
  has_credential: false,
  model_available: true,
};

const TEST_API_KEY = 'sk-test-1234';

function installInvokeStub(opts: StubOptions = {}): {
  calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }>;
} {
  const calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }> = [];
  const addProvider =
    opts.addProvider ?? (async () => SAMPLE_ENTRY satisfies ProviderEntry);
  const updateProvider =
    opts.updateProvider ?? (async () => SAMPLE_ENTRY satisfies ProviderEntry);
  const loginProvider = opts.loginProvider ?? (async () => undefined);
  setInvokeForTesting(
    (async (cmd: string, args?: Record<string, unknown>) => {
      calls.push({ cmd, args });
      switch (cmd) {
        case 'add_provider':
          return addProvider(args?.['input']);
        case 'update_provider':
          return updateProvider(args?.['input']);
        case 'login_provider':
          return loginProvider(args);
        default:
          return undefined;
      }
    }) as never,
  );
  return { calls };
}

/** Fill in the API key field so credentialed-kind submission passes
 * validation. Use in every test that exercises the submit path against a
 * credentialed kind (anthropic / openai / mistral / custom_openai). */
function fillApiKey(getByTestId: (id: string) => HTMLElement, key = TEST_API_KEY): void {
  fireEvent.input(getByTestId('add-provider-api-key'), { target: { value: key } });
}

function renderForm(props: {
  open?: boolean;
  onClose?: () => void;
  onAdded?: (entry: ProviderEntry) => void;
} = {}) {
  const onClose = props.onClose ?? vi.fn();
  const onAdded = props.onAdded ?? vi.fn();
  const result = render(() => (
    <AddProviderForm
      open={props.open ?? true}
      onClose={onClose}
      onAdded={onAdded}
    />
  ));
  return { ...result, onClose, onAdded };
}

afterEach(() => {
  setInvokeForTesting(null);
  cleanup();
});

// ---------------------------------------------------------------------------
// Rendering — open/close, fields per kind, dialog chrome
// ---------------------------------------------------------------------------

describe('AddProviderForm rendering', () => {
  beforeEach(() => {
    installInvokeStub();
  });

  it('renders nothing when `open` is false', () => {
    const { queryByTestId } = renderForm({ open: false });
    expect(queryByTestId('add-provider-form')).toBeNull();
  });

  it('renders dialog chrome with role=dialog and aria-modal=true', () => {
    const { getByTestId } = renderForm();
    const root = getByTestId('add-provider-form');
    expect(root).toHaveAttribute('role', 'dialog');
    expect(root).toHaveAttribute('aria-modal', 'true');
    expect(root.getAttribute('aria-labelledby')).toBe('add-provider-title');
  });

  it('starts in the idle state', () => {
    const { getByTestId } = renderForm();
    expect(getByTestId('add-provider-form')).toHaveAttribute('data-state', 'idle');
  });

  it('renders only the kind selector for a built-in kind', () => {
    const { getByTestId, queryByTestId } = renderForm();
    expect(getByTestId('add-provider-kind')).toBeInTheDocument();
    expect(queryByTestId('add-provider-name')).toBeNull();
    expect(queryByTestId('add-provider-endpoint')).toBeNull();
    expect(queryByTestId('add-provider-model')).toBeNull();
    expect(queryByTestId('add-provider-api-version')).toBeNull();
  });

  it('renders all four custom_openai fields when kind = custom_openai', async () => {
    const { getByTestId } = renderForm();
    const select = getByTestId('add-provider-kind') as HTMLSelectElement;
    fireEvent.change(select, { target: { value: 'custom_openai' } });
    await waitFor(() => expect(getByTestId('add-provider-name')).toBeInTheDocument());
    expect(getByTestId('add-provider-endpoint')).toBeInTheDocument();
    expect(getByTestId('add-provider-model')).toBeInTheDocument();
    expect(getByTestId('add-provider-api-version')).toBeInTheDocument();
  });
});

// ---------------------------------------------------------------------------
// Submit payload shape
// ---------------------------------------------------------------------------

describe('AddProviderForm submit', () => {
  it('invokes add_provider with the kind id for a built-in', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId } = renderForm();
    fillApiKey(getByTestId);
    fireEvent.click(getByTestId('add-provider-submit'));
    await waitFor(() => {
      const add = calls.find((c) => c.cmd === 'add_provider');
      expect(add?.args).toEqual({ input: { id: 'anthropic' } });
    });
  });

  it('chains login_provider with the api key after a successful add', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId } = renderForm();
    fillApiKey(getByTestId, 'sk-ant-xyz');
    fireEvent.click(getByTestId('add-provider-submit'));
    await waitFor(() => {
      const login = calls.find((c) => c.cmd === 'login_provider');
      expect(login?.args).toEqual({ providerId: 'anthropic', key: 'sk-ant-xyz' });
    });
    // Ordering: add_provider must precede login_provider.
    const addIdx = calls.findIndex((c) => c.cmd === 'add_provider');
    const loginIdx = calls.findIndex((c) => c.cmd === 'login_provider');
    expect(addIdx).toBeGreaterThanOrEqual(0);
    expect(loginIdx).toBeGreaterThan(addIdx);
  });

  it('invokes add_provider with the custom_openai payload', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId } = renderForm();
    fireEvent.change(getByTestId('add-provider-kind'), {
      target: { value: 'custom_openai' },
    });
    await waitFor(() => expect(getByTestId('add-provider-name')).toBeInTheDocument());

    fireEvent.input(getByTestId('add-provider-name'), { target: { value: 'vllm' } });
    fireEvent.input(getByTestId('add-provider-endpoint'), {
      target: { value: 'https://api.example.com' },
    });
    fireEvent.input(getByTestId('add-provider-model'), { target: { value: 'qwen2' } });
    fireEvent.input(getByTestId('add-provider-api-version'), {
      target: { value: '2025-01' },
    });
    fillApiKey(getByTestId);

    fireEvent.click(getByTestId('add-provider-submit'));

    await waitFor(() => {
      const add = calls.find((c) => c.cmd === 'add_provider');
      expect(add?.args).toEqual({
        input: {
          id: 'custom_openai:vllm',
          config: {
            endpoint: 'https://api.example.com',
            model: 'qwen2',
            api_version: '2025-01',
          },
        },
      });
    });
  });

  it('omits api_version from the payload when blank', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId } = renderForm();
    fireEvent.change(getByTestId('add-provider-kind'), {
      target: { value: 'custom_openai' },
    });
    await waitFor(() => expect(getByTestId('add-provider-name')).toBeInTheDocument());
    fireEvent.input(getByTestId('add-provider-name'), { target: { value: 'vllm' } });
    fireEvent.input(getByTestId('add-provider-endpoint'), {
      target: { value: 'https://api.example.com' },
    });
    fireEvent.input(getByTestId('add-provider-model'), { target: { value: 'qwen2' } });
    fillApiKey(getByTestId);

    fireEvent.click(getByTestId('add-provider-submit'));

    await waitFor(() => {
      const add = calls.find((c) => c.cmd === 'add_provider');
      expect(add?.args).toEqual({
        input: {
          id: 'custom_openai:vllm',
          config: {
            endpoint: 'https://api.example.com',
            model: 'qwen2',
          },
        },
      });
    });
  });
});

// ---------------------------------------------------------------------------
// Validating state — field-level errors block submit
// ---------------------------------------------------------------------------

describe('AddProviderForm validating state', () => {
  beforeEach(() => {
    installInvokeStub();
  });

  it('rejects an empty custom_openai name with a field error', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId, queryByTestId } = renderForm();
    fireEvent.change(getByTestId('add-provider-kind'), {
      target: { value: 'custom_openai' },
    });
    await waitFor(() => expect(getByTestId('add-provider-name')).toBeInTheDocument());
    fireEvent.input(getByTestId('add-provider-endpoint'), {
      target: { value: 'https://api.example.com' },
    });
    fireEvent.input(getByTestId('add-provider-model'), { target: { value: 'qwen2' } });

    fireEvent.click(getByTestId('add-provider-submit'));

    await waitFor(() =>
      expect(queryByTestId('add-provider-name-error')).toBeInTheDocument(),
    );
    expect(calls.some((c) => c.cmd === 'add_provider')).toBe(false);
  });

  it('rejects a non-http endpoint with a field error', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId, queryByTestId } = renderForm();
    fireEvent.change(getByTestId('add-provider-kind'), {
      target: { value: 'custom_openai' },
    });
    await waitFor(() => expect(getByTestId('add-provider-name')).toBeInTheDocument());
    fireEvent.input(getByTestId('add-provider-name'), { target: { value: 'vllm' } });
    fireEvent.input(getByTestId('add-provider-endpoint'), {
      target: { value: 'ftp://api.example.com' },
    });
    fireEvent.input(getByTestId('add-provider-model'), { target: { value: 'qwen2' } });

    fireEvent.click(getByTestId('add-provider-submit'));

    await waitFor(() =>
      expect(queryByTestId('add-provider-endpoint-error')).toBeInTheDocument(),
    );
    expect(calls.some((c) => c.cmd === 'add_provider')).toBe(false);
  });

  it('rejects a name with disallowed characters', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId, queryByTestId } = renderForm();
    fireEvent.change(getByTestId('add-provider-kind'), {
      target: { value: 'custom_openai' },
    });
    await waitFor(() => expect(getByTestId('add-provider-name')).toBeInTheDocument());
    fireEvent.input(getByTestId('add-provider-name'), {
      target: { value: 'has spaces' },
    });
    fireEvent.input(getByTestId('add-provider-endpoint'), {
      target: { value: 'https://api.example.com' },
    });
    fireEvent.input(getByTestId('add-provider-model'), { target: { value: 'qwen2' } });

    fireEvent.click(getByTestId('add-provider-submit'));

    await waitFor(() =>
      expect(queryByTestId('add-provider-name-error')).toBeInTheDocument(),
    );
    expect(calls.some((c) => c.cmd === 'add_provider')).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// Saving state — IPC in flight, fields disabled
// ---------------------------------------------------------------------------

describe('AddProviderForm saving state', () => {
  it('disables fields and the primary button while saving', async () => {
    let release!: (value: ProviderEntry) => void;
    const pending = new Promise<ProviderEntry>((res) => {
      release = res;
    });
    installInvokeStub({ addProvider: () => pending });
    const { getByTestId } = renderForm();
    fillApiKey(getByTestId);
    fireEvent.click(getByTestId('add-provider-submit'));

    await waitFor(() => {
      expect(getByTestId('add-provider-form')).toHaveAttribute('data-state', 'saving');
    });
    expect(getByTestId('add-provider-kind')).toBeDisabled();
    expect(getByTestId('add-provider-submit')).toBeDisabled();
    expect(getByTestId('add-provider-submit')).toHaveAttribute('aria-busy', 'true');
    expect(getByTestId('add-provider-submit')).toHaveTextContent('Saving…');
    expect(getByTestId('add-provider-cancel')).toBeDisabled();

    release(SAMPLE_ENTRY);
  });
});

// ---------------------------------------------------------------------------
// Success — onAdded + close
// ---------------------------------------------------------------------------

describe('AddProviderForm success path', () => {
  it('calls onAdded with the entry and closes the modal', async () => {
    const result: ProviderEntry = { ...SAMPLE_ENTRY, id: 'anthropic' };
    installInvokeStub({ addProvider: async () => result });
    const onClose = vi.fn();
    const onAdded = vi.fn();
    const { getByTestId } = renderForm({ onClose, onAdded });
    fillApiKey(getByTestId);

    fireEvent.click(getByTestId('add-provider-submit'));

    await waitFor(() => expect(onAdded).toHaveBeenCalledWith(result));
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});

// ---------------------------------------------------------------------------
// Save-failed — verbatim error, form preserved
// ---------------------------------------------------------------------------

describe('AddProviderForm save-failed state', () => {
  it('renders the verbatim daemon error with role="alert"', async () => {
    const errMsg = 'add_provider: provider anthropic already configured';
    installInvokeStub({
      addProvider: async () => {
        throw new Error(errMsg);
      },
    });
    const onClose = vi.fn();
    const { getByTestId } = renderForm({ onClose });
    fillApiKey(getByTestId);

    fireEvent.click(getByTestId('add-provider-submit'));

    await waitFor(() => expect(getByTestId('add-provider-error')).toBeInTheDocument());
    const alert = getByTestId('add-provider-error');
    expect(alert).toHaveAttribute('role', 'alert');
    expect(alert).toHaveTextContent(errMsg);
    expect(getByTestId('add-provider-form')).toHaveAttribute(
      'data-state',
      'save-failed',
    );
    // Form re-enabled for retry.
    expect(getByTestId('add-provider-kind')).not.toBeDisabled();
    expect(getByTestId('add-provider-submit')).not.toBeDisabled();
    // Dialog stays open.
    expect(onClose).not.toHaveBeenCalled();
  });

  it('preserves field state across an error', async () => {
    let attempts = 0;
    installInvokeStub({
      addProvider: async () => {
        attempts += 1;
        if (attempts === 1) throw new Error('add_provider: boom');
        return SAMPLE_ENTRY;
      },
    });
    const { getByTestId } = renderForm();

    fireEvent.change(getByTestId('add-provider-kind'), {
      target: { value: 'custom_openai' },
    });
    await waitFor(() => expect(getByTestId('add-provider-name')).toBeInTheDocument());
    fireEvent.input(getByTestId('add-provider-name'), { target: { value: 'vllm' } });
    fireEvent.input(getByTestId('add-provider-endpoint'), {
      target: { value: 'https://api.example.com' },
    });
    fireEvent.input(getByTestId('add-provider-model'), { target: { value: 'qwen2' } });
    fillApiKey(getByTestId);

    fireEvent.click(getByTestId('add-provider-submit'));
    await waitFor(() => expect(getByTestId('add-provider-error')).toBeInTheDocument());

    expect((getByTestId('add-provider-kind') as HTMLSelectElement).value).toBe(
      'custom_openai',
    );
    expect((getByTestId('add-provider-name') as HTMLInputElement).value).toBe('vllm');
    expect((getByTestId('add-provider-endpoint') as HTMLInputElement).value).toBe(
      'https://api.example.com',
    );
    expect((getByTestId('add-provider-model') as HTMLInputElement).value).toBe('qwen2');
  });
});

// ---------------------------------------------------------------------------
// F-732: edit mode — locked kind/name + dispatches update_provider
// ---------------------------------------------------------------------------

describe('AddProviderForm edit mode', () => {
  function renderEdit(
    props: { initialValues?: Parameters<typeof AddProviderForm>[0]['initialValues'] } = {},
  ) {
    const onClose = vi.fn();
    const onAdded = vi.fn();
    const initialValues = props.initialValues ?? {
      id: 'custom_openai:vllm',
      endpoint: 'https://api.example.com',
      model: 'qwen2',
      api_version: '2025-01',
    };
    const result = render(() => (
      <AddProviderForm
        mode="edit"
        open
        onClose={onClose}
        onAdded={onAdded}
        initialValues={initialValues}
      />
    ));
    return { ...result, onClose, onAdded };
  }

  it('pre-fills every field from initialValues', () => {
    installInvokeStub();
    const { getByTestId } = renderEdit();
    expect((getByTestId('add-provider-kind') as HTMLSelectElement).value).toBe(
      'custom_openai',
    );
    expect((getByTestId('add-provider-name') as HTMLInputElement).value).toBe('vllm');
    expect((getByTestId('add-provider-endpoint') as HTMLInputElement).value).toBe(
      'https://api.example.com',
    );
    expect((getByTestId('add-provider-model') as HTMLInputElement).value).toBe('qwen2');
    expect((getByTestId('add-provider-api-version') as HTMLInputElement).value).toBe(
      '2025-01',
    );
  });

  it('renders the EDIT PROVIDER title', () => {
    installInvokeStub();
    const { getByText } = renderEdit();
    expect(getByText('EDIT PROVIDER')).toBeInTheDocument();
  });

  it('locks the kind selector (disabled + aria-readonly)', () => {
    installInvokeStub();
    const { getByTestId } = renderEdit();
    const select = getByTestId('add-provider-kind');
    expect(select).toBeDisabled();
    expect(select).toHaveAttribute('aria-readonly', 'true');
  });

  it('locks the name field (read-only + aria-readonly)', () => {
    installInvokeStub();
    const { getByTestId } = renderEdit();
    const input = getByTestId('add-provider-name');
    expect(input).toHaveAttribute('readonly');
    expect(input).toHaveAttribute('aria-readonly', 'true');
  });

  it('dispatches update_provider (not add_provider) on submit', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId } = renderEdit();
    fireEvent.input(getByTestId('add-provider-model'), {
      target: { value: 'qwen2-72b' },
    });
    fireEvent.click(getByTestId('add-provider-submit'));
    await waitFor(() => {
      const update = calls.find((c) => c.cmd === 'update_provider');
      expect(update?.args).toEqual({
        input: {
          id: 'custom_openai:vllm',
          config: {
            endpoint: 'https://api.example.com',
            model: 'qwen2-72b',
            api_version: '2025-01',
          },
        },
      });
    });
    // The add_provider command is never called in edit mode.
    expect(calls.some((c) => c.cmd === 'add_provider')).toBe(false);
  });

  it('omits api_version from the payload when cleared', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId } = renderEdit({
      initialValues: {
        id: 'custom_openai:vllm',
        endpoint: 'https://api.example.com',
        model: 'qwen2',
        api_version: '2025-01',
      },
    });
    fireEvent.input(getByTestId('add-provider-api-version'), {
      target: { value: '' },
    });
    fireEvent.click(getByTestId('add-provider-submit'));
    await waitFor(() => {
      const update = calls.find((c) => c.cmd === 'update_provider');
      expect(update?.args).toEqual({
        input: {
          id: 'custom_openai:vllm',
          config: {
            endpoint: 'https://api.example.com',
            model: 'qwen2',
          },
        },
      });
    });
  });

  it('surfaces verbatim update_provider errors with role="alert"', async () => {
    const msg = 'update_provider: provider custom_openai:vllm not configured';
    installInvokeStub({
      updateProvider: async () => {
        throw new Error(msg);
      },
    });
    const { getByTestId } = renderEdit();
    fireEvent.click(getByTestId('add-provider-submit'));
    await waitFor(() => expect(getByTestId('add-provider-error')).toBeInTheDocument());
    const alert = getByTestId('add-provider-error');
    expect(alert).toHaveAttribute('role', 'alert');
    expect(alert).toHaveTextContent(msg);
  });
});
