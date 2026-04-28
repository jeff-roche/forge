import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { cleanup, fireEvent, render } from '@solidjs/testing-library';
import type { ScopedRosterEntry } from '@forge/ipc';
import { CatalogPane } from './CatalogPane';
import { setInvokeForTesting } from '../../lib/tauri';
import { resetSettingsStore } from '../../stores/settings';

const invokeMock = vi.fn();

const skill = (id: string): ScopedRosterEntry => ({
  entry: { type: 'Skill', id },
  scope: { type: 'SessionWide' },
});

const mcp = (id: string): ScopedRosterEntry => ({
  entry: { type: 'Mcp', id },
  scope: { type: 'SessionWide' },
});

const agent = (id: string, background = false): ScopedRosterEntry => ({
  entry: { type: 'Agent', id, background },
  scope: { type: 'SessionWide' },
});

interface SetupOpts {
  skills?: ScopedRosterEntry[];
  mcp?: ScopedRosterEntry[];
  agents?: ScopedRosterEntry[];
  setSettingError?: string;
  listSkillsError?: string;
}

function setupInvoke(opts: SetupOpts = {}) {
  invokeMock.mockImplementation((cmd: string) => {
    switch (cmd) {
      case 'list_skills':
        if (opts.listSkillsError) return Promise.reject(new Error(opts.listSkillsError));
        return Promise.resolve(opts.skills ?? []);
      case 'list_mcp_servers':
        return Promise.resolve(opts.mcp ?? []);
      case 'list_agents':
        return Promise.resolve(opts.agents ?? []);
      case 'set_setting':
        if (opts.setSettingError) return Promise.reject(new Error(opts.setSettingError));
        return Promise.resolve(undefined);
      default:
        return Promise.resolve(undefined);
    }
  });
}

async function flush(): Promise<void> {
  for (let i = 0; i < 6; i += 1) {
    await Promise.resolve();
  }
}

beforeEach(() => {
  invokeMock.mockReset();
  setInvokeForTesting(invokeMock as never);
  resetSettingsStore();
});

afterEach(() => {
  setInvokeForTesting(null);
  cleanup();
});

describe('<CatalogPane> (F-592)', () => {
  it('renders a skeleton loading state per active tab during the fetch (F-684)', async () => {
    invokeMock.mockImplementation(() => new Promise(() => undefined));
    const { findByTestId, queryByText } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    // Default active tab is `skills`; only that tabpanel paints its skeleton
    // because each kind's resource is gated behind its own panel `<Show>`.
    const skeleton = await findByTestId('catalog-loading-skills');
    expect(skeleton.getAttribute('role')).toBe('status');
    expect(skeleton.getAttribute('aria-busy')).toBe('true');
    // Plain-text "Skills · loading" copy must be gone.
    expect(queryByText(/Skills · loading/i)).toBeFalsy();
  });

  it('renders three tabs (Skills / MCP / Agents)', async () => {
    setupInvoke();
    const { findAllByRole } = render(() => <CatalogPane workspaceRoot="/ws" />);
    await flush();

    const tabs = await findAllByRole('tab');
    expect(tabs).toHaveLength(3);
    const labels = tabs.map((t) => t.textContent ?? '');
    expect(labels.some((l) => l.includes('Skills'))).toBe(true);
    expect(labels.some((l) => l.includes('MCP'))).toBe(true);
    expect(labels.some((l) => l.includes('Agents'))).toBe(true);
  });

  it('fetches each list_* command with the workspaceRoot + SessionWide scope', async () => {
    setupInvoke();
    render(() => <CatalogPane workspaceRoot="/ws" />);
    await flush();

    expect(invokeMock).toHaveBeenCalledWith('list_skills', {
      workspaceRoot: '/ws',
      scope: { type: 'SessionWide' },
    });
    expect(invokeMock).toHaveBeenCalledWith('list_mcp_servers', {
      workspaceRoot: '/ws',
      scope: { type: 'SessionWide' },
    });
    expect(invokeMock).toHaveBeenCalledWith('list_agents', {
      workspaceRoot: '/ws',
      scope: { type: 'SessionWide' },
    });
  });

  it('renders rows on the active tab, grouped by scope', async () => {
    setupInvoke({
      skills: [
        skill('typescript-review'),
        skill('postgres-schemata'),
      ],
    });
    const { findByText } = render(() => <CatalogPane workspaceRoot="/ws" />);
    await flush();

    expect(await findByText('typescript-review')).toBeTruthy();
    expect(await findByText('postgres-schemata')).toBeTruthy();
    expect(await findByText('Session-wide')).toBeTruthy();
  });

  it('shows kind-specific empty copy when a tab returns zero entries', async () => {
    setupInvoke({ skills: [] });
    const { findByText } = render(() => <CatalogPane workspaceRoot="/ws" />);
    await flush();

    expect(await findByText('No skills installed')).toBeTruthy();
  });

  it('search filters the active tab', async () => {
    setupInvoke({
      skills: [skill('typescript-review'), skill('postgres-schemata')],
    });
    const { findByLabelText, queryByText, findByText } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    const search = await findByLabelText('Filter catalog entries');
    fireEvent.input(search, { target: { value: 'postgres' } });
    await flush();

    expect(queryByText('typescript-review')).toBeNull();
    expect(await findByText('postgres-schemata')).toBeTruthy();
  });

  it('search empties the row list with a "no matches" empty-state copy', async () => {
    setupInvoke({ skills: [skill('only')] });
    const { findByLabelText, findByText } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    const search = await findByLabelText('Filter catalog entries');
    fireEvent.input(search, { target: { value: 'nothingmatches' } });
    await flush();

    expect(await findByText('No matches')).toBeTruthy();
  });

  it('toggling a row persists `catalog.enabled.<kind>.<id>` via set_setting', async () => {
    setupInvoke({ skills: [skill('typescript-review')] });
    const { findByRole } = render(() => <CatalogPane workspaceRoot="/ws" />);
    await flush();

    const toggle = await findByRole('switch');
    fireEvent.click(toggle);
    await flush();

    expect(invokeMock).toHaveBeenCalledWith('set_setting', {
      key: 'catalog.enabled.skills.typescript-review',
      value: false,
      level: 'user',
      workspaceRoot: '/ws',
    });
  });

  it('toggle round-trip: store mirror reflects the new value on next render', async () => {
    // After a successful set_setting, the settings store mirror must update so
    // a subsequent read of `catalog.enabled.<kind>.<id>` returns the persisted
    // value. This is the round-trip the DoD requires: the toggle persists
    // *and* the in-memory state stays in sync, so a reload (or a re-mount)
    // preserves the user's choice rather than silently reverting it.
    setupInvoke({ skills: [skill('typescript-review')] });
    const { findByRole } = render(() => <CatalogPane workspaceRoot="/ws" />);
    await flush();

    const toggle = await findByRole('switch') as HTMLInputElement;
    expect(toggle.checked).toBe(true);

    fireEvent.click(toggle);
    await flush();

    // The set_setting IPC fired (resolved by the default mock), and the
    // store's `applyLocalUpdate` must have walked the dotted key into
    // `catalog.enabled.skills.typescript-review = false`. The Solid render
    // then re-reads `isEnabled` and reflects the new state.
    const refreshed = await findByRole('switch') as HTMLInputElement;
    expect(refreshed.checked).toBe(false);
  });

  it('badge count on the Skills tab matches the post-filter row count', async () => {
    setupInvoke({
      skills: [skill('alpha'), skill('beta'), skill('gamma')],
    });
    const { findAllByRole, findByLabelText } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    let tabs = await findAllByRole('tab');
    const skillsTab = tabs.find((t) => t.textContent?.includes('Skills'))!;
    expect(skillsTab.textContent).toMatch(/3/);

    const search = await findByLabelText('Filter catalog entries');
    fireEvent.input(search, { target: { value: 'alp' } });
    await flush();

    tabs = await findAllByRole('tab');
    const skillsTabAfter = tabs.find((t) => t.textContent?.includes('Skills'))!;
    expect(skillsTabAfter.textContent).toMatch(/1/);
  });

  it('clicking a tab switches the visible kind panel', async () => {
    setupInvoke({
      skills: [skill('s1')],
      mcp: [mcp('m1')],
      agents: [agent('a1')],
    });
    const { findAllByRole, findByText, queryByText } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    expect(await findByText('s1')).toBeTruthy();

    const tabs = await findAllByRole('tab');
    const mcpTab = tabs.find((t) => t.textContent?.includes('MCP'))!;
    fireEvent.click(mcpTab);
    await flush();

    expect(await findByText('m1')).toBeTruthy();
    expect(queryByText('s1')).toBeNull();
  });

  it('renders an error block when a list_* command rejects', async () => {
    setupInvoke({ listSkillsError: 'workspace_root not in registry: /ws' });
    const { findByText } = render(() => <CatalogPane workspaceRoot="/ws" />);
    // Resource rejection transitions through `loading=true` → `loading=false,
    // state='errored'`. The default 6-microtask flush above is enough for
    // happy-path resolutions but a rejection needs an extra macrotask to land,
    // hence the longer `findBy*` wait + explicit settle.
    await new Promise((r) => setTimeout(r, 0));
    await flush();

    expect(await findByText('SKILLS UNAVAILABLE')).toBeTruthy();
  });

  it('wires aria-controls/id and aria-labelledby/id between tabs and panels', async () => {
    // Each tab's aria-controls must point at the matching panel's id, and
    // each panel's aria-labelledby must point at the matching tab's id.
    // Without both, screen readers cannot follow the tab→panel relationship.
    setupInvoke({ skills: [skill('only')], mcp: [], agents: [] });
    const { findAllByRole, container } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    const tabs = await findAllByRole('tab');
    for (const tab of tabs) {
      const id = tab.getAttribute('id');
      const controls = tab.getAttribute('aria-controls');
      expect(id).toMatch(/^catalog-tab-(skills|mcp|agents)$/);
      expect(controls).toMatch(/^catalog-panel-(skills|mcp|agents)$/);
      // The kind suffix on tab id and panel reference must match.
      const tabKind = id!.replace('catalog-tab-', '');
      const panelKind = controls!.replace('catalog-panel-', '');
      expect(tabKind).toBe(panelKind);
    }

    // The visible panel must exist with the matching id and aria-labelledby.
    const panel = container.querySelector('[role="tabpanel"]') as HTMLElement | null;
    expect(panel).not.toBeNull();
    const panelId = panel!.getAttribute('id');
    const labelledBy = panel!.getAttribute('aria-labelledby');
    expect(panelId).toMatch(/^catalog-panel-(skills|mcp|agents)$/);
    expect(labelledBy).toMatch(/^catalog-tab-(skills|mcp|agents)$/);
    // Confirm the back-reference resolves to a real DOM node.
    expect(container.querySelector(`#${labelledBy}`)).not.toBeNull();
    // And the panel id is reachable from the active tab's aria-controls.
    const activeTab = tabs.find((t) => t.getAttribute('aria-selected') === 'true');
    expect(activeTab).toBeTruthy();
    expect(activeTab!.getAttribute('aria-controls')).toBe(panelId);
  });

  it('surfaces a set_setting rejection without disabling the toggle UI', async () => {
    setupInvoke({
      skills: [skill('typescript-review')],
      setSettingError: 'invalid value',
    });
    const { findByRole, findByText } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    const toggle = await findByRole('switch');
    fireEvent.click(toggle);
    await flush();

    expect(await findByText(/set_setting failed: invalid value/)).toBeTruthy();
  });
});
