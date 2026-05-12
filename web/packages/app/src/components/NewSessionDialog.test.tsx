// F-726: tests for the + New session dialog.
//
// Covers the four states from docs/ui-specs/new-session-flow.md §States —
// idle / validating / spawning / spawn-failed — plus the spec-level
// invariants: defaults, IPC payload shape, success handoff to
// `open_session`, verbatim daemon error rendering, form-state preservation
// across retries, and the empty-provider degraded mode.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';

// Mock the dialog plugin before importing the SUT — the real plugin throws
// outside a Tauri runtime. The mock is reset per-test via `mockOpenDialog`.
const mockOpenDialog = vi.fn();
vi.mock('@tauri-apps/plugin-dialog', () => ({
  open: (...args: unknown[]) => mockOpenDialog(...args),
}));

import { NewSessionDialog } from './NewSessionDialog';
import { setInvokeForTesting } from '../lib/tauri';
import { setActiveWorkspaceRoot } from '../stores/session';

interface ProviderRow {
  id: string;
  display_name: string;
  credential_required: boolean;
  has_credential: boolean;
  model_available: boolean;
  model?: string;
}

const PROVIDERS_DEFAULT: ProviderRow[] = [
  {
    id: 'anthropic',
    display_name: 'Anthropic',
    credential_required: true,
    has_credential: true,
    model_available: true,
  },
  {
    id: 'openai',
    display_name: 'OpenAI',
    credential_required: true,
    has_credential: false,
    model_available: true,
  },
  {
    id: 'custom_openai:ollama',
    display_name: 'custom_openai — ollama',
    credential_required: false,
    has_credential: false,
    model_available: true,
  },
];

interface StubOptions {
  providers?: ProviderRow[];
  activeProvider?: string | null;
  /** Override the `session_start` handler. Defaults to a success resolving
   * to a deterministic session id. */
  sessionStart?: (input: unknown) => Promise<unknown>;
  /** Override `open_session`. Defaults to a no-op resolved promise. */
  openSession?: (args: unknown) => Promise<unknown>;
}

function installInvokeStub(opts: StubOptions = {}): {
  calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }>;
} {
  const calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }> = [];
  const providers = opts.providers ?? PROVIDERS_DEFAULT;
  const activeProvider = opts.activeProvider ?? 'anthropic';
  const sessionStart = opts.sessionStart ?? (async () => ({ session_id: 'sess-001' }));
  const openSession = opts.openSession ?? (async () => undefined);

  setInvokeForTesting(
    (async (cmd: string, args?: Record<string, unknown>) => {
      calls.push({ cmd, args });
      switch (cmd) {
        case 'dashboard_list_providers':
          return providers;
        case 'get_active_provider':
          return activeProvider;
        case 'session_start':
          return sessionStart(args?.['input']);
        case 'open_session':
          return openSession(args);
        default:
          return undefined;
      }
    }) as never,
  );
  return { calls };
}

function renderDialog(props: {
  open?: boolean;
  onClose?: () => void;
  onSpawned?: (id: string) => void;
} = {}) {
  const onClose = props.onClose ?? vi.fn();
  const onSpawned = props.onSpawned ?? vi.fn();
  const result = render(() => (
    <NewSessionDialog
      open={props.open ?? true}
      onClose={onClose}
      onSpawned={onSpawned}
    />
  ));
  return { ...result, onClose, onSpawned };
}

beforeEach(() => {
  // Reset the workspace signal so each test starts from a known baseline.
  setActiveWorkspaceRoot(null);
  mockOpenDialog.mockReset();
});

afterEach(() => {
  setInvokeForTesting(null);
  cleanup();
});

// ---------------------------------------------------------------------------
// Rendering — fields, defaults, dialog chrome
// ---------------------------------------------------------------------------

describe('NewSessionDialog rendering', () => {
  it('renders nothing when `open` is false', () => {
    installInvokeStub();
    const { queryByTestId } = renderDialog({ open: false });
    expect(queryByTestId('new-session-dialog')).toBeNull();
  });

  it('renders dialog chrome with role=dialog and aria-modal=true', () => {
    installInvokeStub();
    const { getByTestId } = renderDialog();
    const root = getByTestId('new-session-dialog');
    expect(root).toHaveAttribute('role', 'dialog');
    expect(root).toHaveAttribute('aria-modal', 'true');
    expect(root.getAttribute('aria-labelledby')).toBe('new-session-title');
  });

  it('renders all three fields — workspace, provider, agent', async () => {
    installInvokeStub();
    const { getByTestId } = renderDialog();
    expect(getByTestId('workspace-input')).toBeInTheDocument();
    await waitFor(() => expect(getByTestId('provider-select')).toBeInTheDocument());
    expect(getByTestId('agent-select')).toBeInTheDocument();
    expect(getByTestId('agent-select').getAttribute('data-value')).toBe('forge-default');
  });

  it('defaults workspace to activeWorkspaceRoot when present', () => {
    installInvokeStub();
    setActiveWorkspaceRoot('/repo/forge');
    const { getByTestId } = renderDialog();
    expect((getByTestId('workspace-input') as HTMLInputElement).value).toBe('/repo/forge');
  });

  it('defaults provider to get_active_provider when enabled', async () => {
    installInvokeStub({ activeProvider: 'anthropic' });
    const { getByTestId } = renderDialog();
    await waitFor(() =>
      expect(getByTestId('provider-select').getAttribute('data-value')).toBe('anthropic'),
    );
  });

  it('falls back to the first enabled provider when active is unavailable', async () => {
    installInvokeStub({
      activeProvider: 'openai', // openai has credential_required && !has_credential → disabled
      providers: PROVIDERS_DEFAULT,
    });
    const { getByTestId } = renderDialog();
    await waitFor(() =>
      expect(getByTestId('provider-select').getAttribute('data-value')).toBe('anthropic'),
    );
  });
});

// ---------------------------------------------------------------------------
// Idle state — submit gating
// ---------------------------------------------------------------------------

describe('NewSessionDialog idle state', () => {
  it('disables submit when workspace is empty', async () => {
    installInvokeStub();
    const { getByTestId } = renderDialog();
    await waitFor(() => expect(getByTestId('provider-select')).toBeInTheDocument());
    expect(getByTestId('new-session-submit')).toBeDisabled();
  });

  it('enables submit when workspace is populated and a provider is enabled', async () => {
    installInvokeStub();
    setActiveWorkspaceRoot('/work/repo');
    const { getByTestId } = renderDialog();
    await waitFor(() => expect(getByTestId('provider-select')).toBeInTheDocument());
    expect(getByTestId('new-session-submit')).not.toBeDisabled();
  });

  it('renders idle state on the dialog root', () => {
    installInvokeStub();
    const { getByTestId } = renderDialog();
    expect(getByTestId('new-session-dialog')).toHaveAttribute('data-state', 'idle');
  });
});

// ---------------------------------------------------------------------------
// Spawning state — IPC payload + loading affordances
// ---------------------------------------------------------------------------

describe('NewSessionDialog spawning state', () => {
  it('invokes session_start with the correct payload', async () => {
    const { calls } = installInvokeStub();
    setActiveWorkspaceRoot('/work/repo');
    const { getByTestId } = renderDialog();
    await waitFor(() => expect(getByTestId('provider-select')).toBeInTheDocument());

    fireEvent.click(getByTestId('new-session-submit'));

    await waitFor(() => {
      const start = calls.find((c) => c.cmd === 'session_start');
      expect(start).toBeTruthy();
      expect(start?.args).toEqual({
        input: {
          workspace_root: '/work/repo',
          provider: 'anthropic',
          agent: 'forge-default',
        },
      });
    });
  });

  it('disables the primary button and sets aria-busy while spawning', async () => {
    let resolveStart!: (value: { session_id: string }) => void;
    const pending = new Promise<{ session_id: string }>((res) => {
      resolveStart = res;
    });
    installInvokeStub({ sessionStart: () => pending });
    setActiveWorkspaceRoot('/work/repo');

    const { getByTestId } = renderDialog();
    await waitFor(() => expect(getByTestId('provider-select')).toBeInTheDocument());

    fireEvent.click(getByTestId('new-session-submit'));

    await waitFor(() => {
      expect(getByTestId('new-session-dialog')).toHaveAttribute('data-state', 'spawning');
    });
    expect(getByTestId('new-session-submit')).toBeDisabled();
    expect(getByTestId('new-session-submit')).toHaveAttribute('aria-busy', 'true');
    expect(getByTestId('new-session-submit')).toHaveTextContent('Starting…');
    expect(getByTestId('workspace-input')).toBeDisabled();
    expect(getByTestId('provider-select')).toBeDisabled();
    expect(getByTestId('new-session-cancel')).toBeDisabled();

    resolveStart({ session_id: 'sess-xyz' });
  });
});

// ---------------------------------------------------------------------------
// Success path — open_session dispatch, dialog dismiss, onSpawned
// ---------------------------------------------------------------------------

describe('NewSessionDialog success path', () => {
  it('dispatches open_session and closes the dialog on success', async () => {
    const { calls } = installInvokeStub({
      sessionStart: async () => ({ session_id: 'sess-good' }),
    });
    setActiveWorkspaceRoot('/work/repo');
    const onClose = vi.fn();
    const onSpawned = vi.fn();
    const { getByTestId } = renderDialog({ onClose, onSpawned });
    await waitFor(() => expect(getByTestId('provider-select')).toBeInTheDocument());

    fireEvent.click(getByTestId('new-session-submit'));

    await waitFor(() => {
      expect(calls.some((c) => c.cmd === 'open_session')).toBe(true);
    });
    const openCall = calls.find((c) => c.cmd === 'open_session');
    expect(openCall?.args).toEqual({ id: 'sess-good' });
    expect(onSpawned).toHaveBeenCalledWith('sess-good');
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});

// ---------------------------------------------------------------------------
// Spawn-failed state — verbatim error, form preserved, retry
// ---------------------------------------------------------------------------

describe('NewSessionDialog spawn-failed state', () => {
  it('renders the verbatim daemon error with role="alert"', async () => {
    const errMsg =
      'session_start: workspace not accessible: No such file or directory (os error 2)';
    installInvokeStub({
      sessionStart: async () => {
        throw new Error(errMsg);
      },
    });
    setActiveWorkspaceRoot('/work/repo');
    const onClose = vi.fn();
    const { getByTestId } = renderDialog({ onClose });
    await waitFor(() => expect(getByTestId('provider-select')).toBeInTheDocument());

    fireEvent.click(getByTestId('new-session-submit'));

    await waitFor(() => expect(getByTestId('new-session-error')).toBeInTheDocument());
    const alert = getByTestId('new-session-error');
    expect(alert).toHaveAttribute('role', 'alert');
    expect(alert).toHaveTextContent(errMsg);
    expect(getByTestId('new-session-dialog')).toHaveAttribute('data-state', 'spawn-failed');
    // Form re-enabled for retry.
    expect(getByTestId('new-session-submit')).not.toBeDisabled();
    expect(getByTestId('workspace-input')).not.toBeDisabled();
    // Dialog stays open.
    expect(onClose).not.toHaveBeenCalled();
  });

  it('preserves form state across an error', async () => {
    let attempts = 0;
    installInvokeStub({
      sessionStart: async () => {
        attempts += 1;
        if (attempts === 1) throw new Error('session_start: provider unavailable');
        return { session_id: 'sess-retry' };
      },
    });
    setActiveWorkspaceRoot('/work/repo');
    const { getByTestId } = renderDialog();
    await waitFor(() => expect(getByTestId('provider-select')).toBeInTheDocument());

    // Edit workspace to a custom path so we can assert preservation.
    const wsInput = getByTestId('workspace-input') as HTMLInputElement;
    fireEvent.input(wsInput, { target: { value: '/custom/path' } });

    fireEvent.click(getByTestId('new-session-submit'));
    await waitFor(() => expect(getByTestId('new-session-error')).toBeInTheDocument());

    // Form retained: workspace, provider, agent all still set.
    expect((getByTestId('workspace-input') as HTMLInputElement).value).toBe('/custom/path');
    expect(getByTestId('provider-select').getAttribute('data-value')).toBe('anthropic');
    expect(getByTestId('agent-select').getAttribute('data-value')).toBe('forge-default');

    // Retry — error clears once submit fires again.
    fireEvent.click(getByTestId('new-session-submit'));
    await waitFor(() => expect(attempts).toBe(2));
  });

  it('surfaces a client-side error when workspace is whitespace-only', async () => {
    installInvokeStub();
    const { getByTestId, queryByTestId } = renderDialog();
    await waitFor(() => expect(getByTestId('provider-select')).toBeInTheDocument());

    const wsInput = getByTestId('workspace-input') as HTMLInputElement;
    // Inject whitespace via DOM; submit by form submission so the disabled
    // attribute on the button doesn't block the test path.
    fireEvent.input(wsInput, { target: { value: '   ' } });
    // The submit button is gated on a non-empty trimmed workspace; force-fire
    // the form's submit event directly to exercise the validating branch.
    const form = wsInput.closest('form');
    expect(form).not.toBeNull();
    fireEvent.submit(form!);

    await waitFor(() => expect(queryByTestId('new-session-error')).toBeInTheDocument());
    expect(getByTestId('new-session-error')).toHaveTextContent(
      'session_start: workspace_root is empty',
    );
    expect(getByTestId('new-session-dialog')).toHaveAttribute('data-state', 'spawn-failed');
  });
});

// ---------------------------------------------------------------------------
// Degraded mode — no providers configured
// ---------------------------------------------------------------------------

describe('NewSessionDialog provider-empty state', () => {
  it('renders the empty-providers hint and disables submit when no provider is enabled', async () => {
    // OpenAI alone, missing credential → no enabled options.
    installInvokeStub({
      providers: [
        {
          id: 'openai',
          display_name: 'OpenAI',
          credential_required: true,
          has_credential: false,
          model_available: true,
        },
      ],
      activeProvider: null,
    });
    setActiveWorkspaceRoot('/work/repo');
    const { getByTestId, queryByTestId } = renderDialog();

    await waitFor(() => expect(getByTestId('provider-empty-hint')).toBeInTheDocument());
    expect(queryByTestId('provider-select')).toBeNull();
    expect(getByTestId('provider-empty-hint')).toHaveTextContent(
      'No providers configured. Add one on the Providers page.',
    );
    expect(getByTestId('new-session-submit')).toBeDisabled();
  });
});

// ---------------------------------------------------------------------------
// Cancel + dismiss paths
// ---------------------------------------------------------------------------

describe('NewSessionDialog dismiss paths', () => {
  it('calls onClose when Cancel is clicked', async () => {
    installInvokeStub();
    const onClose = vi.fn();
    const { getByTestId } = renderDialog({ onClose });
    await waitFor(() => expect(getByTestId('provider-select')).toBeInTheDocument());
    fireEvent.click(getByTestId('new-session-cancel'));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('calls onClose on Escape', async () => {
    installInvokeStub();
    const onClose = vi.fn();
    renderDialog({ onClose });
    fireEvent.keyDown(window, { key: 'Escape' });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('calls onClose on backdrop click', async () => {
    installInvokeStub();
    const onClose = vi.fn();
    const { getByTestId } = renderDialog({ onClose });
    fireEvent.click(getByTestId('new-session-backdrop'));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('ignores backdrop and Escape while spawning', async () => {
    let resolveStart!: (value: { session_id: string }) => void;
    const pending = new Promise<{ session_id: string }>((res) => {
      resolveStart = res;
    });
    installInvokeStub({ sessionStart: () => pending });
    setActiveWorkspaceRoot('/work/repo');

    const onClose = vi.fn();
    const { getByTestId } = renderDialog({ onClose });
    await waitFor(() => expect(getByTestId('provider-select')).toBeInTheDocument());

    fireEvent.click(getByTestId('new-session-submit'));
    await waitFor(() =>
      expect(getByTestId('new-session-dialog')).toHaveAttribute('data-state', 'spawning'),
    );

    fireEvent.keyDown(window, { key: 'Escape' });
    fireEvent.click(getByTestId('new-session-backdrop'));
    expect(onClose).not.toHaveBeenCalled();

    resolveStart({ session_id: 'done' });
  });
});

// ---------------------------------------------------------------------------
// Workspace picker — native directory dialog (F-726 follow-up)
// ---------------------------------------------------------------------------

describe('NewSessionDialog workspace picker', () => {
  it('renders a Browse button next to the workspace input', async () => {
    installInvokeStub();
    const { getByTestId } = renderDialog();
    await waitFor(() => expect(getByTestId('provider-select')).toBeInTheDocument());
    const browse = getByTestId('workspace-browse');
    expect(browse).toBeInTheDocument();
    expect(browse).toHaveTextContent('Browse');
  });

  it('updates the workspace input when the picker returns a path', async () => {
    installInvokeStub();
    mockOpenDialog.mockResolvedValueOnce('/picked/from/native');
    const { getByTestId } = renderDialog();
    await waitFor(() => expect(getByTestId('provider-select')).toBeInTheDocument());

    fireEvent.click(getByTestId('workspace-browse'));

    await waitFor(() =>
      expect((getByTestId('workspace-input') as HTMLInputElement).value).toBe(
        '/picked/from/native',
      ),
    );
    expect(mockOpenDialog).toHaveBeenCalledWith({
      directory: true,
      multiple: false,
      title: 'Pick a workspace for the new session',
    });
  });

  it('leaves the workspace input unchanged when the picker is cancelled', async () => {
    installInvokeStub();
    setActiveWorkspaceRoot('/seed/path');
    mockOpenDialog.mockResolvedValueOnce(null);
    const { getByTestId } = renderDialog();
    await waitFor(() => expect(getByTestId('provider-select')).toBeInTheDocument());

    fireEvent.click(getByTestId('workspace-browse'));

    // Give the picker promise a chance to resolve before asserting unchanged.
    await Promise.resolve();
    await Promise.resolve();

    expect((getByTestId('workspace-input') as HTMLInputElement).value).toBe('/seed/path');
    expect(mockOpenDialog).toHaveBeenCalledTimes(1);
  });

  it('disables the Browse button while spawning', async () => {
    let resolveStart!: (value: { session_id: string }) => void;
    const pending = new Promise<{ session_id: string }>((res) => {
      resolveStart = res;
    });
    installInvokeStub({ sessionStart: () => pending });
    setActiveWorkspaceRoot('/work/repo');
    const { getByTestId } = renderDialog();
    await waitFor(() => expect(getByTestId('provider-select')).toBeInTheDocument());

    fireEvent.click(getByTestId('new-session-submit'));
    await waitFor(() =>
      expect(getByTestId('new-session-dialog')).toHaveAttribute('data-state', 'spawning'),
    );

    expect(getByTestId('workspace-browse')).toBeDisabled();

    resolveStart({ session_id: 'done' });
  });
});
