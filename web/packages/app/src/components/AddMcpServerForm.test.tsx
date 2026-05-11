// F-734: AddMcpServerForm tests. Covers the four states from
// docs/ui-specs/catalog.md §Add MCP server modal (idle / validating /
// saving / save-failed) plus stdio / http field rendering, invoke
// payload shape, success handoff, and verbatim error rendering.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';

import { AddMcpServerForm } from './AddMcpServerForm';
import { setInvokeForTesting } from '../lib/tauri';

interface StubOptions {
  addMcpServer?: (input: unknown) => Promise<unknown>;
}

function installInvokeStub(opts: StubOptions = {}): {
  calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }>;
} {
  const calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }> = [];
  const addMcpServer = opts.addMcpServer ?? (async () => undefined);
  setInvokeForTesting(
    (async (cmd: string, args?: Record<string, unknown>) => {
      calls.push({ cmd, args });
      switch (cmd) {
        case 'add_mcp_server':
          return addMcpServer(args?.['input']);
        default:
          return undefined;
      }
    }) as never,
  );
  return { calls };
}

function renderForm(props: {
  open?: boolean;
  scope?: 'workspace' | 'user';
  workspaceRoot?: string | null;
  onClose?: () => void;
  onAdded?: () => void;
} = {}) {
  const onClose = props.onClose ?? vi.fn();
  const onAdded = props.onAdded ?? vi.fn();
  const result = render(() => (
    <AddMcpServerForm
      open={props.open ?? true}
      scope={props.scope ?? 'workspace'}
      workspaceRoot={props.workspaceRoot ?? '/ws'}
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
// Rendering — chrome + per-kind fields
// ---------------------------------------------------------------------------

describe('AddMcpServerForm rendering', () => {
  beforeEach(() => {
    installInvokeStub();
  });

  it('renders nothing when `open` is false', () => {
    const { queryByTestId } = renderForm({ open: false });
    expect(queryByTestId('add-mcp-form')).toBeNull();
  });

  it('renders dialog chrome with role=dialog and aria-modal=true', () => {
    const { getByTestId } = renderForm();
    const root = getByTestId('add-mcp-form');
    expect(root).toHaveAttribute('role', 'dialog');
    expect(root).toHaveAttribute('aria-modal', 'true');
    expect(root.getAttribute('aria-labelledby')).toBe('add-mcp-title');
  });

  it('starts in the idle state', () => {
    const { getByTestId } = renderForm();
    expect(getByTestId('add-mcp-form')).toHaveAttribute('data-state', 'idle');
  });

  it('renders stdio fields by default', () => {
    const { getByTestId, queryByTestId } = renderForm();
    expect(getByTestId('add-mcp-kind-stdio')).toBeChecked();
    expect(getByTestId('add-mcp-command')).toBeInTheDocument();
    expect(queryByTestId('add-mcp-url')).toBeNull();
  });

  it('renders http fields when kind = http', async () => {
    const { getByTestId, queryByTestId } = renderForm();
    fireEvent.click(getByTestId('add-mcp-kind-http'));
    await waitFor(() => expect(getByTestId('add-mcp-url')).toBeInTheDocument());
    expect(queryByTestId('add-mcp-command')).toBeNull();
    expect(getByTestId('add-mcp-headers')).toBeInTheDocument();
  });
});

// ---------------------------------------------------------------------------
// Submit payload shape — stdio + http
// ---------------------------------------------------------------------------

describe('AddMcpServerForm submit payload', () => {
  it('invokes add_mcp_server with a stdio payload (workspace scope)', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId } = renderForm({ workspaceRoot: '/ws' });

    fireEvent.input(getByTestId('add-mcp-name'), { target: { value: 'github' } });
    fireEvent.input(getByTestId('add-mcp-command'), { target: { value: 'npx' } });
    fireEvent.input(getByTestId('add-mcp-arg-0'), { target: { value: '-y' } });
    fireEvent.click(getByTestId('add-mcp-arg-add'));
    await waitFor(() => expect(getByTestId('add-mcp-arg-1')).toBeInTheDocument());
    fireEvent.input(getByTestId('add-mcp-arg-1'), {
      target: { value: '@modelcontextprotocol/server-github' },
    });
    fireEvent.click(getByTestId('add-mcp-env-add'));
    await waitFor(() =>
      expect(getByTestId('add-mcp-env-name-0')).toBeInTheDocument(),
    );
    fireEvent.input(getByTestId('add-mcp-env-name-0'), {
      target: { value: 'GITHUB_TOKEN' },
    });
    fireEvent.input(getByTestId('add-mcp-env-value-0'), {
      target: { value: 'ghp_xxx' },
    });

    fireEvent.click(getByTestId('add-mcp-submit'));

    await waitFor(() => {
      const call = calls.find((c) => c.cmd === 'add_mcp_server');
      expect(call?.args).toEqual({
        input: {
          workspace_root: '/ws',
          name: 'github',
          config: {
            kind: 'stdio',
            command: 'npx',
            args: ['-y', '@modelcontextprotocol/server-github'],
            env: { GITHUB_TOKEN: 'ghp_xxx' },
          },
        },
      });
    });
  });

  it('omits workspace_root when scope = user', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId } = renderForm({ scope: 'user' });

    fireEvent.input(getByTestId('add-mcp-name'), { target: { value: 'local' } });
    fireEvent.input(getByTestId('add-mcp-command'), { target: { value: 'node' } });

    fireEvent.click(getByTestId('add-mcp-submit'));

    await waitFor(() => {
      const call = calls.find((c) => c.cmd === 'add_mcp_server');
      expect(call?.args).toEqual({
        input: {
          name: 'local',
          config: {
            kind: 'stdio',
            command: 'node',
            args: [],
            env: {},
          },
        },
      });
    });
  });

  it('invokes add_mcp_server with an http payload + auth header', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId } = renderForm({ workspaceRoot: '/ws' });

    fireEvent.input(getByTestId('add-mcp-name'), { target: { value: 'remote' } });
    fireEvent.click(getByTestId('add-mcp-kind-http'));
    await waitFor(() => expect(getByTestId('add-mcp-url')).toBeInTheDocument());

    fireEvent.input(getByTestId('add-mcp-url'), {
      target: { value: 'https://mcp.example.com/api' },
    });
    fireEvent.click(getByTestId('add-mcp-header-add'));
    await waitFor(() =>
      expect(getByTestId('add-mcp-header-name-0')).toBeInTheDocument(),
    );
    fireEvent.input(getByTestId('add-mcp-header-name-0'), {
      target: { value: 'Authorization' },
    });
    fireEvent.input(getByTestId('add-mcp-header-value-0'), {
      target: { value: 'Bearer xyz' },
    });

    fireEvent.click(getByTestId('add-mcp-submit'));

    await waitFor(() => {
      const call = calls.find((c) => c.cmd === 'add_mcp_server');
      expect(call?.args).toEqual({
        input: {
          workspace_root: '/ws',
          name: 'remote',
          config: {
            kind: 'http',
            url: 'https://mcp.example.com/api',
            headers: { Authorization: 'Bearer xyz' },
          },
        },
      });
    });
  });
});

// ---------------------------------------------------------------------------
// Validating state — field-level errors block submit
// ---------------------------------------------------------------------------

describe('AddMcpServerForm validating state', () => {
  it('rejects an empty name with a field error', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId, queryByTestId } = renderForm();
    fireEvent.input(getByTestId('add-mcp-command'), { target: { value: 'npx' } });

    fireEvent.click(getByTestId('add-mcp-submit'));

    await waitFor(() =>
      expect(queryByTestId('add-mcp-name-error')).toBeInTheDocument(),
    );
    expect(calls.some((c) => c.cmd === 'add_mcp_server')).toBe(false);
  });

  it('rejects a non-slug name', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId, queryByTestId } = renderForm();
    fireEvent.input(getByTestId('add-mcp-name'), { target: { value: 'BAD NAME' } });
    fireEvent.input(getByTestId('add-mcp-command'), { target: { value: 'npx' } });

    fireEvent.click(getByTestId('add-mcp-submit'));

    await waitFor(() =>
      expect(queryByTestId('add-mcp-name-error')).toBeInTheDocument(),
    );
    expect(calls.some((c) => c.cmd === 'add_mcp_server')).toBe(false);
  });

  it('rejects a missing stdio command', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId, queryByTestId } = renderForm();
    fireEvent.input(getByTestId('add-mcp-name'), { target: { value: 'github' } });

    fireEvent.click(getByTestId('add-mcp-submit'));

    await waitFor(() =>
      expect(queryByTestId('add-mcp-command-error')).toBeInTheDocument(),
    );
    expect(calls.some((c) => c.cmd === 'add_mcp_server')).toBe(false);
  });

  it('rejects an invalid http URL', async () => {
    const { calls } = installInvokeStub();
    const { getByTestId, queryByTestId } = renderForm();
    fireEvent.input(getByTestId('add-mcp-name'), { target: { value: 'remote' } });
    fireEvent.click(getByTestId('add-mcp-kind-http'));
    await waitFor(() => expect(getByTestId('add-mcp-url')).toBeInTheDocument());
    fireEvent.input(getByTestId('add-mcp-url'), {
      target: { value: 'ftp://example.com' },
    });

    fireEvent.click(getByTestId('add-mcp-submit'));

    await waitFor(() =>
      expect(queryByTestId('add-mcp-url-error')).toBeInTheDocument(),
    );
    expect(calls.some((c) => c.cmd === 'add_mcp_server')).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// Saving state — IPC in flight, fields disabled, "Adding…" label
// ---------------------------------------------------------------------------

describe('AddMcpServerForm saving state', () => {
  it('disables fields and renders the Adding… label while saving', async () => {
    let release!: () => void;
    const pending = new Promise<void>((res) => {
      release = res;
    });
    installInvokeStub({ addMcpServer: () => pending });
    const { getByTestId } = renderForm();
    fireEvent.input(getByTestId('add-mcp-name'), { target: { value: 'github' } });
    fireEvent.input(getByTestId('add-mcp-command'), { target: { value: 'npx' } });

    fireEvent.click(getByTestId('add-mcp-submit'));

    await waitFor(() => {
      expect(getByTestId('add-mcp-form')).toHaveAttribute('data-state', 'saving');
    });
    expect(getByTestId('add-mcp-name')).toBeDisabled();
    expect(getByTestId('add-mcp-command')).toBeDisabled();
    expect(getByTestId('add-mcp-submit')).toBeDisabled();
    expect(getByTestId('add-mcp-submit')).toHaveAttribute('aria-busy', 'true');
    expect(getByTestId('add-mcp-submit')).toHaveTextContent('Adding…');
    expect(getByTestId('add-mcp-cancel')).toBeDisabled();

    release();
  });
});

// ---------------------------------------------------------------------------
// Success — onAdded + close
// ---------------------------------------------------------------------------

describe('AddMcpServerForm success path', () => {
  it('calls onAdded and closes the modal on success', async () => {
    installInvokeStub({ addMcpServer: async () => undefined });
    const onClose = vi.fn();
    const onAdded = vi.fn();
    const { getByTestId } = renderForm({ onClose, onAdded });

    fireEvent.input(getByTestId('add-mcp-name'), { target: { value: 'github' } });
    fireEvent.input(getByTestId('add-mcp-command'), { target: { value: 'npx' } });

    fireEvent.click(getByTestId('add-mcp-submit'));

    await waitFor(() => expect(onAdded).toHaveBeenCalledTimes(1));
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});

// ---------------------------------------------------------------------------
// Save-failed — verbatim error, form preserved
// ---------------------------------------------------------------------------

describe('AddMcpServerForm save-failed state', () => {
  it('renders the verbatim daemon error with role="alert"', async () => {
    const errMsg = 'add_mcp_server: server github already configured in workspace';
    installInvokeStub({
      addMcpServer: async () => {
        throw new Error(errMsg);
      },
    });
    const onClose = vi.fn();
    const { getByTestId } = renderForm({ onClose });

    fireEvent.input(getByTestId('add-mcp-name'), { target: { value: 'github' } });
    fireEvent.input(getByTestId('add-mcp-command'), { target: { value: 'npx' } });

    fireEvent.click(getByTestId('add-mcp-submit'));

    await waitFor(() => expect(getByTestId('add-mcp-error')).toBeInTheDocument());
    const alert = getByTestId('add-mcp-error');
    expect(alert).toHaveAttribute('role', 'alert');
    expect(alert).toHaveTextContent(errMsg);
    expect(getByTestId('add-mcp-form')).toHaveAttribute(
      'data-state',
      'save-failed',
    );
    // Form re-enabled for retry.
    expect(getByTestId('add-mcp-name')).not.toBeDisabled();
    expect(getByTestId('add-mcp-submit')).not.toBeDisabled();
    // Dialog stays open.
    expect(onClose).not.toHaveBeenCalled();
  });

  it('preserves field state across an error', async () => {
    let attempts = 0;
    installInvokeStub({
      addMcpServer: async () => {
        attempts += 1;
        if (attempts === 1) throw new Error('add_mcp_server: boom');
        return undefined;
      },
    });
    const { getByTestId } = renderForm();

    fireEvent.input(getByTestId('add-mcp-name'), { target: { value: 'github' } });
    fireEvent.input(getByTestId('add-mcp-command'), { target: { value: 'npx' } });
    fireEvent.input(getByTestId('add-mcp-arg-0'), { target: { value: '-y' } });

    fireEvent.click(getByTestId('add-mcp-submit'));
    await waitFor(() => expect(getByTestId('add-mcp-error')).toBeInTheDocument());

    expect((getByTestId('add-mcp-name') as HTMLInputElement).value).toBe('github');
    expect((getByTestId('add-mcp-command') as HTMLInputElement).value).toBe('npx');
    expect((getByTestId('add-mcp-arg-0') as HTMLInputElement).value).toBe('-y');
  });
});
