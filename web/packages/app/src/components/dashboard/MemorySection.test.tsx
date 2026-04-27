import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';
import { setInvokeForTesting } from '../../lib/tauri';
import { resetSettingsStore } from '../../stores/settings';
import { effectiveEnabled, formatBytes, MemorySection } from './MemorySection';
import type { AgentMemoryEntry } from '../../ipc/memory';

type InvokeMock = ReturnType<typeof vi.fn>;

interface MemoryStub {
  entries: AgentMemoryEntry[];
  bodies: Record<string, string>;
}

function buildMemoryInvoke(seed: MemoryStub): { fn: InvokeMock; state: MemoryStub } {
  const state: MemoryStub = {
    entries: seed.entries.map((e) => ({ ...e })),
    bodies: { ...seed.bodies },
  };
  const fn = vi.fn(async (cmd: string, args?: Record<string, unknown>) => {
    if (cmd === 'list_agent_memory') return state.entries;
    if (cmd === 'read_agent_memory') {
      const id = args?.['agentId'] as string;
      return state.bodies[id] ?? '';
    }
    if (cmd === 'save_agent_memory') {
      const id = args?.['agentId'] as string;
      const body = args?.['body'] as string;
      state.bodies[id] = body;
      const entry = state.entries.find((e) => e.agent_id === id);
      if (entry) {
        entry.size_bytes = body.length;
        entry.version = (entry.version ?? 0) + 1;
        entry.updated_at = new Date().toISOString();
      }
      return { version: entry?.version ?? 1, updated_at: new Date().toISOString() };
    }
    if (cmd === 'clear_agent_memory') {
      const id = args?.['agentId'] as string;
      state.bodies[id] = '';
      const entry = state.entries.find((e) => e.agent_id === id);
      if (entry) {
        entry.size_bytes = 0;
        entry.version = (entry.version ?? 0) + 1;
      }
      return undefined;
    }
    if (cmd === 'set_setting') {
      const key = args?.['key'] as string;
      const value = args?.['value'] as boolean;
      const segments = key.split('.');
      // memory.enabled.<agent>
      if (segments[0] === 'memory' && segments[1] === 'enabled') {
        const agent = segments.slice(2).join('.');
        const entry = state.entries.find((e) => e.agent_id === agent);
        if (entry) entry.settings_override = value;
      }
      return undefined;
    }
    return undefined;
  });
  return { fn, state };
}

function entry(partial: Partial<AgentMemoryEntry> & { agent_id: string }): AgentMemoryEntry {
  return {
    agent_id: partial.agent_id,
    path: partial.path ?? `/cfg/forge/memory/${partial.agent_id}.md`,
    size_bytes: partial.size_bytes ?? null,
    updated_at: partial.updated_at ?? null,
    version: partial.version ?? null,
    def_enabled: partial.def_enabled ?? false,
    settings_override: partial.settings_override ?? null,
  };
}

describe('MemorySection (F-602)', () => {
  beforeEach(() => {
    setInvokeForTesting(null);
    resetSettingsStore();
  });

  afterEach(() => {
    setInvokeForTesting(null);
    cleanup();
  });

  it('renders one row per agent returned by list_agent_memory', async () => {
    const { fn } = buildMemoryInvoke({
      entries: [entry({ agent_id: 'alpha', def_enabled: true }), entry({ agent_id: 'beta' })],
      bodies: {},
    });
    setInvokeForTesting(fn as never);

    const { findByTestId } = render(() => <MemorySection workspaceRoot="/work" />);
    expect(await findByTestId('memory-row-alpha')).toBeTruthy();
    expect(await findByTestId('memory-row-beta')).toBeTruthy();
  });

  it('shows the secrets warning verbatim', async () => {
    const { fn } = buildMemoryInvoke({
      entries: [entry({ agent_id: 'alpha' })],
      bodies: {},
    });
    setInvokeForTesting(fn as never);
    const { findByTestId } = render(() => <MemorySection workspaceRoot="/work" />);
    const warn = await findByTestId('memory-section-warning');
    expect(warn.textContent).toMatch(/DO NOT store secrets/i);
  });

  it('renders empty-state when no agents are loaded', async () => {
    const { fn } = buildMemoryInvoke({ entries: [], bodies: {} });
    setInvokeForTesting(fn as never);
    const { findByTestId } = render(() => <MemorySection workspaceRoot="/work" />);
    expect(await findByTestId('memory-section-empty')).toBeTruthy();
  });

  it('reflects the toggle state from settings_override', async () => {
    const { fn } = buildMemoryInvoke({
      entries: [
        entry({ agent_id: 'alpha', def_enabled: false, settings_override: true }),
      ],
      bodies: {},
    });
    setInvokeForTesting(fn as never);
    const { findByTestId } = render(() => <MemorySection workspaceRoot="/work" />);
    const toggle = (await findByTestId('memory-toggle-alpha')) as HTMLInputElement;
    expect(toggle.checked).toBe(true);
  });

  it('falls back to def_enabled when settings_override is null', async () => {
    const { fn } = buildMemoryInvoke({
      entries: [entry({ agent_id: 'alpha', def_enabled: true })],
      bodies: {},
    });
    setInvokeForTesting(fn as never);
    const { findByTestId } = render(() => <MemorySection workspaceRoot="/work" />);
    const toggle = (await findByTestId('memory-toggle-alpha')) as HTMLInputElement;
    expect(toggle.checked).toBe(true);
  });

  it('toggling writes a memory.enabled.<agent> setting and refetches', async () => {
    const { fn, state } = buildMemoryInvoke({
      entries: [entry({ agent_id: 'alpha', def_enabled: false })],
      bodies: {},
    });
    setInvokeForTesting(fn as never);
    const { findByTestId } = render(() => <MemorySection workspaceRoot="/work" />);
    const toggle = (await findByTestId('memory-toggle-alpha')) as HTMLInputElement;
    expect(toggle.checked).toBe(false);

    fireEvent.click(toggle);

    await waitFor(() => {
      const args = fn.mock.calls.find((c) => c[0] === 'set_setting')?.[1] as
        | Record<string, unknown>
        | undefined;
      expect(args?.['key']).toBe('memory.enabled.alpha');
      expect(args?.['value']).toBe(true);
      expect(args?.['level']).toBe('workspace');
    });
    expect(state.entries[0]?.settings_override).toBe(true);
  });

  it('opens the editor when Edit is clicked', async () => {
    const { fn } = buildMemoryInvoke({
      entries: [entry({ agent_id: 'alpha', def_enabled: true })],
      bodies: { alpha: 'hello' },
    });
    setInvokeForTesting(fn as never);
    const { findByTestId } = render(() => (
      <MemorySection workspaceRoot="/work" useTextareaForTest />
    ));
    fireEvent.click(await findByTestId('memory-edit-alpha'));
    expect(await findByTestId('memory-editor')).toBeTruthy();
  });

  it('editor surfaces the file path and the secrets warning', async () => {
    const { fn } = buildMemoryInvoke({
      entries: [
        entry({
          agent_id: 'alpha',
          def_enabled: true,
          path: '/cfg/forge/memory/alpha.md',
        }),
      ],
      bodies: { alpha: 'body' },
    });
    setInvokeForTesting(fn as never);
    const { findByTestId } = render(() => (
      <MemorySection workspaceRoot="/work" useTextareaForTest />
    ));
    fireEvent.click(await findByTestId('memory-edit-alpha'));
    const path = await findByTestId('memory-editor-path');
    expect(path.textContent).toContain('alpha.md');
    const warn = await findByTestId('memory-editor-warning');
    expect(warn.textContent).toMatch(/DO NOT store secrets/i);
  });

  it('editor opens read-only when memory is disabled', async () => {
    const { fn } = buildMemoryInvoke({
      entries: [
        entry({ agent_id: 'alpha', def_enabled: false, settings_override: false }),
      ],
      bodies: { alpha: 'cached body' },
    });
    setInvokeForTesting(fn as never);
    const { findByTestId, queryByTestId } = render(() => (
      <MemorySection workspaceRoot="/work" useTextareaForTest />
    ));
    fireEvent.click(await findByTestId('memory-edit-alpha'));
    expect(await findByTestId('memory-editor-readonly')).toBeTruthy();
    // Save button is hidden when read-only.
    expect(queryByTestId('memory-editor-save')).toBeNull();
  });

  it('saving writes the body via save_agent_memory', async () => {
    const { fn, state } = buildMemoryInvoke({
      entries: [entry({ agent_id: 'alpha', def_enabled: true })],
      bodies: { alpha: 'old' },
    });
    setInvokeForTesting(fn as never);
    const { findByTestId } = render(() => (
      <MemorySection workspaceRoot="/work" useTextareaForTest />
    ));
    fireEvent.click(await findByTestId('memory-edit-alpha'));
    const textarea = (await findByTestId('memory-editor-textarea')) as HTMLTextAreaElement;
    await waitFor(() => expect(textarea.value).toBe('old'));

    fireEvent.input(textarea, { target: { value: 'new body' } });
    fireEvent.click(await findByTestId('memory-editor-save'));

    await waitFor(() => {
      expect(state.bodies['alpha']).toBe('new body');
    });
  });

  it('Clear opens a confirm flyout and wipes when confirmed', async () => {
    const { fn, state } = buildMemoryInvoke({
      entries: [
        entry({ agent_id: 'alpha', def_enabled: true, size_bytes: 12 }),
      ],
      bodies: { alpha: 'something' },
    });
    setInvokeForTesting(fn as never);
    const { findByTestId } = render(() => (
      <MemorySection workspaceRoot="/work" useTextareaForTest />
    ));
    fireEvent.click(await findByTestId('memory-clear-alpha'));
    expect(await findByTestId('memory-clear-modal')).toBeTruthy();
    fireEvent.click(await findByTestId('memory-clear-confirm'));
    await waitFor(() => {
      expect(state.bodies['alpha']).toBe('');
    });
  });

  it('Clear cancel keeps the body intact', async () => {
    const { fn, state } = buildMemoryInvoke({
      entries: [
        entry({ agent_id: 'alpha', def_enabled: true, size_bytes: 4 }),
      ],
      bodies: { alpha: 'safe' },
    });
    setInvokeForTesting(fn as never);
    const { findByTestId, queryByTestId } = render(() => (
      <MemorySection workspaceRoot="/work" useTextareaForTest />
    ));
    fireEvent.click(await findByTestId('memory-clear-alpha'));
    fireEvent.click(await findByTestId('memory-clear-cancel'));
    await waitFor(() => expect(queryByTestId('memory-clear-modal')).toBeNull());
    expect(state.bodies['alpha']).toBe('safe');
  });
});

describe('MemorySection helpers', () => {
  it('effectiveEnabled prefers settings override over def', () => {
    expect(
      effectiveEnabled({
        agent_id: 'a',
        path: '',
        size_bytes: null,
        updated_at: null,
        version: null,
        def_enabled: false,
        settings_override: true,
      }),
    ).toBe(true);

    expect(
      effectiveEnabled({
        agent_id: 'a',
        path: '',
        size_bytes: null,
        updated_at: null,
        version: null,
        def_enabled: true,
        settings_override: false,
      }),
    ).toBe(false);

    expect(
      effectiveEnabled({
        agent_id: 'a',
        path: '',
        size_bytes: null,
        updated_at: null,
        version: null,
        def_enabled: true,
        settings_override: null,
      }),
    ).toBe(true);
  });

  it('formatBytes handles each magnitude', () => {
    expect(formatBytes(null)).toMatch(/—/);
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(512)).toBe('512 B');
    expect(formatBytes(2048)).toMatch(/KiB/);
    expect(formatBytes(2 * 1024 * 1024)).toMatch(/MiB/);
  });
});
