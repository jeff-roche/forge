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
  probeProviderConfig?: (input: unknown) => Promise<unknown>;
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
  const probeProviderConfig =
    opts.probeProviderConfig ?? (async () => ({ ok: true } as const));
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
        case 'probe_provider_config':
          return probeProviderConfig(args?.['input']);
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

/** Drive the custom `Dropdown` trigger so a test can pick an option the
 * same way the user would: click the trigger to open the listbox, then
 * click the option whose `data-option-value` matches. */
function pickDropdown(
  getByTestId: (id: string) => HTMLElement,
  triggerTestId: string,
  value: string,
): void {
  const trigger = getByTestId(triggerTestId);
  fireEvent.click(trigger);
  // Each Dropdown panel is portaled into the same document, so query
  // globally by the canonical `data-option-value` attribute scoped to the
  // listbox that the trigger controls.
  const listboxId = trigger.getAttribute('aria-controls');
  const panel = listboxId ? document.getElementById(listboxId) : null;
  const opt = panel?.querySelector<HTMLElement>(`[data-option-value="${value}"]`);
  if (!opt) {
    throw new Error(
      `Dropdown ${triggerTestId} has no option with value "${value}"`,
    );
  }
  fireEvent.click(opt);
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
  });

  it('renders all custom_openai fields when kind = custom_openai', async () => {
    const { getByTestId } = renderForm();
    pickDropdown(getByTestId, 'add-provider-kind', 'custom_openai');
    await waitFor(() => expect(getByTestId('add-provider-name')).toBeInTheDocument());
    expect(getByTestId('add-provider-endpoint')).toBeInTheDocument();
    expect(getByTestId('add-provider-model')).toBeInTheDocument();
  });
});

// ---------------------------------------------------------------------------
// Submit payload shape
// ---------------------------------------------------------------------------

describe('AddProviderForm submit', () => {
  it('invokes add_provider with `<kind>:default` for a built-in with no name', async () => {
    // Built-ins always carry a `<vendor>:<name>` id; blank name falls
    // back to `default` so the wire never sees a bare-vendor shape.
    const { calls } = installInvokeStub({
      addProvider: async () => ({ ...SAMPLE_ENTRY, id: 'anthropic:default' }),
    });
    const { getByTestId } = renderForm();
    fillApiKey(getByTestId);
    fireEvent.click(getByTestId('add-provider-submit'));
    await waitFor(() => {
      const add = calls.find((c) => c.cmd === 'add_provider');
      expect(add?.args).toEqual({ input: { id: 'anthropic:default' } });
    });
  });

  it('sends Vertex builtin config when Anthropic + Vertex auth is selected', async () => {
    // Phase B: Anthropic exposes an AUTH METHOD selector. Choosing
    // "Google Vertex AI" hides the API-key field (gcloud ADC supplies
    // the token at request time) and reveals GCP project + region.
    // The submit payload carries `builtin: { auth_kind: 'vertex', ... }`
    // and skips the chained `login_provider` call.
    const { calls } = installInvokeStub({
      addProvider: async () => ({ ...SAMPLE_ENTRY, id: 'anthropic:vertex-work' }),
    });
    const { getByTestId, queryByTestId } = renderForm();
    fireEvent.input(getByTestId('add-provider-builtin-name'), {
      target: { value: 'vertex-work' },
    });
    pickDropdown(getByTestId, 'add-provider-auth-kind', 'vertex');
    // API-key field is hidden in Vertex mode.
    expect(queryByTestId('add-provider-api-key')).toBeNull();
    fireEvent.input(getByTestId('add-provider-vertex-project'), {
      target: { value: 'my-gcp-project' },
    });
    fireEvent.input(getByTestId('add-provider-vertex-region'), {
      target: { value: 'us-central1' },
    });
    fireEvent.click(getByTestId('add-provider-submit'));
    await waitFor(() => {
      const add = calls.find((c) => c.cmd === 'add_provider');
      expect(add?.args).toEqual({
        input: {
          id: 'anthropic:vertex-work',
          builtin: {
            auth_kind: 'vertex',
            vertex_project: 'my-gcp-project',
            vertex_region: 'us-central1',
          },
        },
      });
    });
    // No `login_provider` call — Vertex skips the API-key chaining.
    expect(calls.some((c) => c.cmd === 'login_provider')).toBe(false);
  });

  it('rejects Vertex submission with empty project/region', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId, findByTestId } = renderForm();
    pickDropdown(getByTestId, 'add-provider-auth-kind', 'vertex');
    fireEvent.click(getByTestId('add-provider-submit'));
    await findByTestId('add-provider-vertex-project-error');
    await findByTestId('add-provider-vertex-region-error');
    expect(calls.some((c) => c.cmd === 'add_provider')).toBe(false);
  });

  it('invokes add_provider with a named built-in id when a name is supplied', async () => {
    // Phase A: optional NAME field for built-ins lets the user create
    // multiple `anthropic:<name>` configurations with independent
    // credentials. Blank name still produces the bare vendor id (see the
    // first test in this block).
    const { calls } = installInvokeStub({
      addProvider: async () => ({ ...SAMPLE_ENTRY, id: 'anthropic:work' }),
    });
    const { getByTestId } = renderForm();
    fireEvent.input(getByTestId('add-provider-builtin-name'), {
      target: { value: 'work' },
    });
    fillApiKey(getByTestId, 'sk-ant-work');
    fireEvent.click(getByTestId('add-provider-submit'));
    await waitFor(() => {
      const add = calls.find((c) => c.cmd === 'add_provider');
      expect(add?.args).toEqual({ input: { id: 'anthropic:work' } });
    });
    // Credential is keyed on the full id (anthropic:work), not the bare
    // vendor — that's the point of named instances. The login call is
    // chained async, so wait for it explicitly.
    await waitFor(() => {
      const login = calls.find((c) => c.cmd === 'login_provider');
      expect(login?.args).toEqual({ providerId: 'anthropic:work', key: 'sk-ant-work' });
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
    pickDropdown(getByTestId, 'add-provider-kind', 'custom_openai');
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

  it('removes ollama from the KIND dropdown (rolled into custom_openai preset)', () => {
    installInvokeStub();
    const { getByTestId } = renderForm();
    const trigger = getByTestId('add-provider-kind');
    fireEvent.click(trigger);
    const listboxId = trigger.getAttribute('aria-controls');
    const panel = listboxId ? document.getElementById(listboxId) : null;
    const values = Array.from(
      panel?.querySelectorAll<HTMLElement>('[data-option-value]') ?? [],
    ).map((o) => o.getAttribute('data-option-value'));
    expect(values).not.toContain('ollama');
    expect(values).toContain('custom_openai');
  });

  it('renders the PRESET selector only when kind = custom_openai', async () => {
    installInvokeStub();
    const { getByTestId, queryByTestId } = renderForm();
    // Built-in default kind: no preset selector.
    expect(queryByTestId('add-provider-preset')).toBeNull();
    pickDropdown(getByTestId, 'add-provider-kind', 'custom_openai');
    await waitFor(() =>
      expect(queryByTestId('add-provider-preset')).toBeInTheDocument(),
    );
  });

  it('Ollama preset auto-fills endpoint/model and seeds the API key with `ollama`', async () => {
    installInvokeStub();
    const { getByTestId, queryByTestId } = renderForm();
    pickDropdown(getByTestId, 'add-provider-kind', 'custom_openai');
    await waitFor(() => expect(getByTestId('add-provider-preset')).toBeInTheDocument());
    // Before switching the preset, API key is shown (default custom flow).
    expect(queryByTestId('add-provider-api-key')).toBeInTheDocument();

    pickDropdown(getByTestId, 'add-provider-preset', 'ollama');

    await waitFor(() => {
      expect((getByTestId('add-provider-endpoint') as HTMLInputElement).value).toBe(
        'http://127.0.0.1:11434',
      );
    });
    expect((getByTestId('add-provider-model') as HTMLInputElement).value).toBe(
      'llama3.2',
    );
    // API key field stays visible and is pre-seeded with the literal `ollama`.
    expect(queryByTestId('add-provider-api-key')).toBeInTheDocument();
    expect((getByTestId('add-provider-api-key') as HTMLInputElement).value).toBe(
      'ollama',
    );
  });

  it('Ollama preset submit chains login_provider with the literal `ollama` key', async () => {
    const { calls } = installInvokeStub({
      addProvider: async () => ({ ...SAMPLE_ENTRY, id: 'custom_openai:ollama' }),
    });
    const { getByTestId } = renderForm();
    pickDropdown(getByTestId, 'add-provider-kind', 'custom_openai');
    await waitFor(() => expect(getByTestId('add-provider-preset')).toBeInTheDocument());
    pickDropdown(getByTestId, 'add-provider-preset', 'ollama');
    await waitFor(() => expect(getByTestId('add-provider-name')).toBeInTheDocument());
    fireEvent.input(getByTestId('add-provider-name'), { target: { value: 'ollama' } });

    fireEvent.click(getByTestId('add-provider-submit'));

    await waitFor(() => {
      const add = calls.find((c) => c.cmd === 'add_provider');
      expect(add?.args).toEqual({
        input: {
          id: 'custom_openai:ollama',
          config: {
            endpoint: 'http://127.0.0.1:11434',
            model: 'llama3.2',
          },
        },
      });
    });
    // Credentialed preset → login_provider chained with `ollama` as the key.
    await waitFor(() => {
      const login = calls.find((c) => c.cmd === 'login_provider');
      expect(login?.args).toEqual({
        providerId: 'custom_openai:ollama',
        key: 'ollama',
      });
    });
  });

  it('switching the preset back to Custom clears the auto-filled fields', async () => {
    installInvokeStub();
    const { getByTestId } = renderForm();
    pickDropdown(getByTestId, 'add-provider-kind', 'custom_openai');
    await waitFor(() => expect(getByTestId('add-provider-preset')).toBeInTheDocument());
    pickDropdown(getByTestId, 'add-provider-preset', 'ollama');
    await waitFor(() =>
      expect((getByTestId('add-provider-endpoint') as HTMLInputElement).value).toBe(
        'http://127.0.0.1:11434',
      ),
    );
    pickDropdown(getByTestId, 'add-provider-preset', 'custom');
    await waitFor(() =>
      expect((getByTestId('add-provider-endpoint') as HTMLInputElement).value).toBe(''),
    );
    expect((getByTestId('add-provider-model') as HTMLInputElement).value).toBe('');
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
    pickDropdown(getByTestId, 'add-provider-kind', 'custom_openai');
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
    pickDropdown(getByTestId, 'add-provider-kind', 'custom_openai');
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
    pickDropdown(getByTestId, 'add-provider-kind', 'custom_openai');
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

    pickDropdown(getByTestId, 'add-provider-kind', 'custom_openai');
    await waitFor(() => expect(getByTestId('add-provider-name')).toBeInTheDocument());
    fireEvent.input(getByTestId('add-provider-name'), { target: { value: 'vllm' } });
    fireEvent.input(getByTestId('add-provider-endpoint'), {
      target: { value: 'https://api.example.com' },
    });
    fireEvent.input(getByTestId('add-provider-model'), { target: { value: 'qwen2' } });
    fillApiKey(getByTestId);

    fireEvent.click(getByTestId('add-provider-submit'));
    await waitFor(() => expect(getByTestId('add-provider-error')).toBeInTheDocument());

    expect(getByTestId('add-provider-kind').getAttribute('data-value')).toBe(
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
    expect(getByTestId('add-provider-kind').getAttribute('data-value')).toBe(
      'custom_openai',
    );
    expect((getByTestId('add-provider-name') as HTMLInputElement).value).toBe('vllm');
    expect((getByTestId('add-provider-endpoint') as HTMLInputElement).value).toBe(
      'https://api.example.com',
    );
    expect((getByTestId('add-provider-model') as HTMLInputElement).value).toBe('qwen2');
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
          },
        },
      });
    });
    // The add_provider command is never called in edit mode.
    expect(calls.some((c) => c.cmd === 'add_provider')).toBe(false);
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

// ---------------------------------------------------------------------------
// Test connection / model picker — custom_openai-only "Test connection"
// button drives an ad-hoc probe of the endpoint+key and, on success,
// turns the MODEL text input into a dropdown sourced from the response.
// ---------------------------------------------------------------------------

describe('AddProviderForm test connection', () => {
  it('hides the test-connection button for built-in kinds', () => {
    installInvokeStub();
    const { queryByTestId } = renderForm();
    // Default-mounted with kind=anthropic — the test button is custom_openai-only.
    expect(queryByTestId('add-provider-test-connection')).toBeNull();
  });

  it('shows the test-connection button for custom_openai', async () => {
    installInvokeStub();
    const { getByTestId } = renderForm();
    pickDropdown(getByTestId, 'add-provider-kind', 'custom_openai');
    await waitFor(() =>
      expect(getByTestId('add-provider-test-connection')).toBeInTheDocument(),
    );
  });

  it('blocks the probe when endpoint is blank and surfaces a field error', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId, queryByTestId } = renderForm();
    pickDropdown(getByTestId, 'add-provider-kind', 'custom_openai');
    await waitFor(() =>
      expect(getByTestId('add-provider-test-connection')).toBeInTheDocument(),
    );

    fireEvent.click(getByTestId('add-provider-test-connection'));

    await waitFor(() =>
      expect(queryByTestId('add-provider-endpoint-error')).toBeInTheDocument(),
    );
    expect(calls.some((c) => c.cmd === 'probe_provider_config')).toBe(false);
  });

  it('populates the MODEL dropdown and shows a success banner on a successful probe', async () => {
    const { calls } = installInvokeStub({
      probeProviderConfig: async () => ({
        ok: true,
        latency_ms: 42,
        model_count: 2,
        models: ['llama3.2', 'qwen2.5'],
      }),
    });
    const { getByTestId, queryByTestId } = renderForm();
    pickDropdown(getByTestId, 'add-provider-kind', 'custom_openai');
    await waitFor(() =>
      expect(getByTestId('add-provider-test-connection')).toBeInTheDocument(),
    );
    fireEvent.input(getByTestId('add-provider-endpoint'), {
      target: { value: 'http://127.0.0.1:11434/v1' },
    });
    fillApiKey(getByTestId, 'ollama');

    fireEvent.click(getByTestId('add-provider-test-connection'));

    await waitFor(() =>
      expect(queryByTestId('add-provider-test-success')).toBeInTheDocument(),
    );
    const probe = calls.find((c) => c.cmd === 'probe_provider_config');
    expect(probe?.args).toEqual({
      input: {
        endpoint: 'http://127.0.0.1:11434/v1',
        api_key: 'ollama',
      },
    });
    expect(getByTestId('add-provider-test-success').textContent).toContain(
      '42 ms',
    );
    expect(getByTestId('add-provider-test-success').textContent).toContain(
      '2 models',
    );
    // The MODEL field is now a Dropdown — pick the second advertised model.
    pickDropdown(getByTestId, 'add-provider-model', 'qwen2.5');
    // The "Type a custom model name" affordance reverts to free-text input.
    fireEvent.click(getByTestId('add-provider-model-clear'));
    await waitFor(() => {
      const input = getByTestId('add-provider-model') as HTMLInputElement;
      expect(input.value).toBe('qwen2.5');
      expect(input.tagName).toBe('INPUT');
    });
  });

  it('sends api_key=null when the key field is blank', async () => {
    const { calls } = installInvokeStub({
      probeProviderConfig: async () => ({ ok: true, models: [] }),
    });
    const { getByTestId } = renderForm();
    pickDropdown(getByTestId, 'add-provider-kind', 'custom_openai');
    await waitFor(() =>
      expect(getByTestId('add-provider-test-connection')).toBeInTheDocument(),
    );
    fireEvent.input(getByTestId('add-provider-endpoint'), {
      target: { value: 'http://127.0.0.1:11434/v1' },
    });

    fireEvent.click(getByTestId('add-provider-test-connection'));

    await waitFor(() => {
      const probe = calls.find((c) => c.cmd === 'probe_provider_config');
      expect(probe?.args).toEqual({
        input: {
          endpoint: 'http://127.0.0.1:11434/v1',
          api_key: null,
        },
      });
    });
  });

  it('renders the daemon error verbatim (with the test_provider_connection prefix stripped)', async () => {
    installInvokeStub({
      probeProviderConfig: async () => {
        throw new Error('test_provider_connection: auth HTTP 401');
      },
    });
    const { getByTestId, queryByTestId } = renderForm();
    pickDropdown(getByTestId, 'add-provider-kind', 'custom_openai');
    await waitFor(() =>
      expect(getByTestId('add-provider-test-connection')).toBeInTheDocument(),
    );
    fireEvent.input(getByTestId('add-provider-endpoint'), {
      target: { value: 'https://api.example.com' },
    });
    fillApiKey(getByTestId);

    fireEvent.click(getByTestId('add-provider-test-connection'));

    await waitFor(() =>
      expect(queryByTestId('add-provider-test-error')).toBeInTheDocument(),
    );
    const alert = getByTestId('add-provider-test-error');
    expect(alert).toHaveAttribute('role', 'alert');
    expect(alert.textContent).toBe('auth HTTP 401');
  });
});
