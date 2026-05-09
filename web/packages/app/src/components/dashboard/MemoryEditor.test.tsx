// F-687: MemoryEditor must close on Escape even when focus is inside the
// Monaco iframe. The window-level `keydown` handler on the parent never
// sees keystrokes that fire inside the iframe's contentWindow, so the
// monaco-host iframe forwards Escape via postMessage and the parent
// handles it on the existing `message` channel.
//
// These tests render with `src="about:blank"` so Monaco never boots in
// jsdom (per monaco-host README). MessageEvents are dispatched manually
// with `source: iframe.contentWindow` to mirror what a real iframe post
// would carry.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, waitFor } from '@solidjs/testing-library';
import { MemoryEditor } from './MemoryEditor';
import { setInvokeForTesting } from '../../lib/tauri';

const AGENT = 'planner';
const PATH = '/home/u/.config/forge/memory/planner.md';

function fireFromIframe(iframe: HTMLIFrameElement, data: unknown): void {
  const event = new MessageEvent('message', {
    data,
    source: iframe.contentWindow as MessageEventSource,
  });
  window.dispatchEvent(event);
}

async function getMountedIframe(
  getByTestId: (id: string) => HTMLElement,
): Promise<HTMLIFrameElement> {
  return waitFor(() => getByTestId('memory-editor-iframe') as HTMLIFrameElement);
}

function buildInvoke() {
  return vi.fn(async (cmd: string) => {
    if (cmd === 'read_agent_memory') return '';
    if (cmd === 'save_agent_memory') {
      return { version: 1, updated_at: new Date().toISOString() };
    }
    return undefined;
  });
}

beforeEach(() => {
  setInvokeForTesting(buildInvoke() as never);
});

afterEach(() => {
  cleanup();
  setInvokeForTesting(null);
});

describe('MemoryEditor — iframe Escape passthrough (F-687)', () => {
  it('closes when the iframe posts a keydown:Escape message', async () => {
    const onClose = vi.fn();
    const { getByTestId } = render(() => (
      <MemoryEditor
        agentId={AGENT}
        path={PATH}
        readOnly={false}
        onClose={onClose}
        src="about:blank"
      />
    ));
    const iframe = await getMountedIframe(getByTestId);

    fireFromIframe(iframe, { kind: 'keydown', key: 'Escape' });

    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('ignores keydown messages from windows other than the hosted iframe', async () => {
    const onClose = vi.fn();
    const { getByTestId } = render(() => (
      <MemoryEditor
        agentId={AGENT}
        path={PATH}
        readOnly={false}
        onClose={onClose}
        src="about:blank"
      />
    ));
    // Wait for the iframe to mount before injecting the foreign-window event,
    // otherwise the message lands before the parent's listener is wired.
    await getMountedIframe(getByTestId);

    // No `source` — simulates a foreign window posting keydown:Escape.
    const event = new MessageEvent('message', {
      data: { kind: 'keydown', key: 'Escape' },
      source: null,
    });
    window.dispatchEvent(event);

    expect(onClose).not.toHaveBeenCalled();
  });

  it('ignores non-Escape keydown messages from the iframe', async () => {
    const onClose = vi.fn();
    const { getByTestId } = render(() => (
      <MemoryEditor
        agentId={AGENT}
        path={PATH}
        readOnly={false}
        onClose={onClose}
        src="about:blank"
      />
    ));
    const iframe = await getMountedIframe(getByTestId);

    fireFromIframe(iframe, { kind: 'keydown', key: 'Enter' });
    fireFromIframe(iframe, { kind: 'keydown', key: 'a' });

    expect(onClose).not.toHaveBeenCalled();
  });
});

describe('MemoryEditor — window-level Escape still works (regression)', () => {
  // The pre-F-687 path: window-level keydown handler on the parent. This
  // covers the case where focus is OUTSIDE the iframe (e.g. on the Save
  // button or the dialog body itself). Both paths must close the modal.
  it('closes when Escape fires on the window (focus outside the iframe)', async () => {
    const onClose = vi.fn();
    const { getByTestId } = render(() => (
      <MemoryEditor
        agentId={AGENT}
        path={PATH}
        readOnly={false}
        onClose={onClose}
        src="about:blank"
      />
    ));
    await getMountedIframe(getByTestId);

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));

    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
