// F-727: tests for the Attach to session picker.
//
// Covers the four states (loading / error / empty / ready) plus the
// load -> select -> open_session handoff. The IPC layer is stubbed via
// `setInvokeForTesting` so no Tauri runtime is required.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';
import { AttachSessionPicker, EMPTY_COPY } from './AttachSessionPicker';
import { setInvokeForTesting } from '../lib/tauri';

interface AttachableSummary {
  id: string;
  subject: string;
  state: 'active' | 'stopped' | 'archived';
  persistence: 'persist' | 'ephemeral';
  createdAt: string;
  lastEventAt: string;
  provider?: string;
}

const ROW_A: AttachableSummary = {
  id: 'deadbeefcafebabe',
  subject: 'refactor',
  state: 'stopped',
  persistence: 'persist',
  createdAt: '2026-05-09T10:00:00Z',
  lastEventAt: '2026-05-09T12:00:00Z',
  provider: 'anthropic',
};

const ROW_B: AttachableSummary = {
  id: '0123456789abcdef',
  subject: 'doc-site',
  state: 'stopped',
  persistence: 'persist',
  createdAt: '2026-05-09T09:00:00Z',
  lastEventAt: '2026-05-09T11:00:00Z',
};

interface StubOptions {
  listSessions?: () => Promise<AttachableSummary[]>;
  openSession?: (args: unknown) => Promise<unknown>;
}

function installInvokeStub(opts: StubOptions = {}): {
  calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }>;
} {
  const calls: Array<{ cmd: string; args: Record<string, unknown> | undefined }> = [];
  const listSessions = opts.listSessions ?? (async () => [ROW_A, ROW_B]);
  const openSession = opts.openSession ?? (async () => undefined);

  setInvokeForTesting(
    (async (cmd: string, args?: Record<string, unknown>) => {
      calls.push({ cmd, args });
      switch (cmd) {
        case 'list_sessions':
          return listSessions();
        case 'open_session':
          return openSession(args);
        default:
          return undefined;
      }
    }) as never,
  );
  return { calls };
}

describe('AttachSessionPicker (F-727)', () => {
  beforeEach(() => {
    installInvokeStub();
  });

  afterEach(() => {
    setInvokeForTesting(null);
    cleanup();
  });

  it('renders nothing when closed', () => {
    const { queryByTestId } = render(() => (
      <AttachSessionPicker open={false} onClose={vi.fn()} />
    ));
    expect(queryByTestId('attach-session-picker')).toBeNull();
  });

  it('renders dialog chrome with the verbatim title when open', async () => {
    const { findByTestId, getByText } = render(() => (
      <AttachSessionPicker open={true} onClose={vi.fn()} />
    ));
    const dialog = await findByTestId('attach-session-picker');
    expect(dialog.getAttribute('role')).toBe('dialog');
    expect(dialog.getAttribute('aria-modal')).toBe('true');
    expect(getByText('ATTACH TO SESSION')).toBeTruthy();
  });

  it('renders one row per attachable session returned by list_sessions', async () => {
    const { findByTestId, getByText } = render(() => (
      <AttachSessionPicker open={true} onClose={vi.fn()} />
    ));
    await findByTestId(`attach-picker-row-${ROW_A.id}`);
    await findByTestId(`attach-picker-row-${ROW_B.id}`);
    // Subject and short id paint per row.
    expect(getByText('refactor')).toBeTruthy();
    expect(getByText('doc-site')).toBeTruthy();
    expect(getByText('#dead')).toBeTruthy();
    expect(getByText('#0123')).toBeTruthy();
    // Provider chip painted when present.
    expect(getByText('anthropic')).toBeTruthy();
  });

  it('clicking a row invokes open_session with the matching session id and closes the picker', async () => {
    const onClose = vi.fn();
    const { calls } = installInvokeStub();
    const { findByTestId } = render(() => (
      <AttachSessionPicker open={true} onClose={onClose} />
    ));
    const row = await findByTestId(`attach-picker-row-${ROW_A.id}`);
    fireEvent.click(row);

    await waitFor(() => {
      const openCalls = calls.filter((c) => c.cmd === 'open_session');
      expect(openCalls.length).toBe(1);
      expect(openCalls[0]!.args).toEqual({ id: ROW_A.id });
    });
    await waitFor(() => {
      expect(onClose).toHaveBeenCalledTimes(1);
    });
  });

  it('renders the comment-styled empty state when the list is empty', async () => {
    installInvokeStub({ listSessions: async () => [] });
    const { findByTestId } = render(() => (
      <AttachSessionPicker open={true} onClose={vi.fn()} />
    ));
    const empty = await findByTestId('attach-picker-empty');
    expect(empty.textContent).toBe(EMPTY_COPY);
    expect(EMPTY_COPY).toBe('// no detached sessions');
  });

  it('renders the loading skeleton while list_sessions is in flight', async () => {
    let resolve: (rows: AttachableSummary[]) => void = () => {};
    const pending = new Promise<AttachableSummary[]>((r) => {
      resolve = r;
    });
    installInvokeStub({ listSessions: () => pending });
    const { findByTestId, queryByTestId } = render(() => (
      <AttachSessionPicker open={true} onClose={vi.fn()} />
    ));
    await findByTestId('attach-picker-loading');
    resolve([ROW_A]);
    await waitFor(() => {
      expect(queryByTestId('attach-picker-loading')).toBeNull();
    });
  });

  it('renders the verbatim error and RETRY button when list_sessions rejects', async () => {
    const verbatim = 'list_sessions: dashboard label required';
    installInvokeStub({
      listSessions: async () => {
        throw new Error(verbatim);
      },
    });
    const { findByTestId, getByRole, getByText } = render(() => (
      <AttachSessionPicker open={true} onClose={vi.fn()} />
    ));
    await findByTestId('attach-picker-load-error');
    expect(getByText(verbatim)).toBeTruthy();
    expect(getByRole('button', { name: 'RETRY' })).toBeTruthy();
  });

  it('RETRY re-issues list_sessions', async () => {
    let attempts = 0;
    installInvokeStub({
      listSessions: async () => {
        attempts += 1;
        if (attempts === 1) throw new Error('boom');
        return [ROW_A];
      },
    });
    const { findByRole, findByTestId } = render(() => (
      <AttachSessionPicker open={true} onClose={vi.fn()} />
    ));
    const retry = await findByRole('button', { name: 'RETRY' });
    fireEvent.click(retry);
    await findByTestId(`attach-picker-row-${ROW_A.id}`);
    expect(attempts).toBe(2);
  });

  it('surfaces an open_session failure without closing the picker', async () => {
    const onClose = vi.fn();
    installInvokeStub({
      openSession: async () => {
        throw new Error('open_session: window manager refused');
      },
    });
    const { findByTestId } = render(() => (
      <AttachSessionPicker open={true} onClose={onClose} />
    ));
    const row = await findByTestId(`attach-picker-row-${ROW_A.id}`);
    fireEvent.click(row);
    const errLine = await findByTestId('attach-picker-open-error');
    expect(errLine.textContent).toContain('open_session: window manager refused');
    expect(onClose).not.toHaveBeenCalled();
  });

  it('Escape key invokes onClose while open', () => {
    const onClose = vi.fn();
    render(() => <AttachSessionPicker open={true} onClose={onClose} />);
    fireEvent.keyDown(window, { key: 'Escape' });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('backdrop click invokes onClose; click on dialog body does not', async () => {
    const onClose = vi.fn();
    const { findByTestId } = render(() => (
      <AttachSessionPicker open={true} onClose={onClose} />
    ));
    const backdrop = await findByTestId('attach-picker-backdrop');
    const dialog = await findByTestId('attach-session-picker');

    fireEvent.click(dialog);
    expect(onClose).not.toHaveBeenCalled();

    fireEvent.click(backdrop);
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
