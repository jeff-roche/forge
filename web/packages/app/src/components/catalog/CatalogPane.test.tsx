import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';
import type { ScopedRosterEntry } from '@forge/ipc';
import { CatalogPane } from './CatalogPane';
import { setInvokeForTesting } from '../../lib/tauri';
import { applyLocalUpdate, resetSettingsStore } from '../../stores/settings';

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

// F-694: Provider color discipline — every catalog surface that names a
// provider must carry a `data-provider="<color-id>"` attribute that maps to a
// `--color-provider-*` token. Two surfaces qualify: (1) provider-scoped group
// headers (rows grouped under a `Provider · <id>` label), and (2) Provider-
// typed roster rows themselves. The mapping collapses runtime ids onto the
// four design tokens — `anthropic`, `openai`, `ollama`→`local`, anything else
// (including `custom_openai:*`) → `custom`.
describe('<CatalogPane> provider color discipline (F-694)', () => {
  const skillIn = (
    skillId: string,
    providerScopeId: string,
  ): ScopedRosterEntry => ({
    entry: { type: 'Skill', id: skillId },
    scope: { type: 'Provider', id: providerScopeId },
  });

  it('tags provider-scoped group headers with data-provider mapped to a color token id', async () => {
    invokeMock.mockImplementation((cmd: string) => {
      switch (cmd) {
        case 'list_skills':
          return Promise.resolve([
            skillIn('claude-skill', 'anthropic'),
            skillIn('gpt-skill', 'openai'),
            skillIn('llama-skill', 'ollama'),
            skillIn('byo-skill', 'custom_openai:acme'),
          ]);
        case 'list_mcp_servers':
        case 'list_agents':
          return Promise.resolve([]);
        default:
          return Promise.resolve(undefined);
      }
    });

    const { container } = render(() => <CatalogPane workspaceRoot="/ws" />);
    await flush();

    const groups = container.querySelectorAll('.catalog__group[data-provider]');
    const seen = new Set<string>();
    for (const g of Array.from(groups)) {
      seen.add(g.getAttribute('data-provider')!);
    }
    expect(seen.has('anthropic')).toBe(true);
    expect(seen.has('openai')).toBe(true);
    expect(seen.has('local')).toBe(true); // ollama → local
    expect(seen.has('custom')).toBe(true); // custom_openai:* → custom
  });

  it('tags Provider-typed rows with data-provider matching the entry id', async () => {
    invokeMock.mockImplementation((cmd: string) => {
      switch (cmd) {
        case 'list_skills':
          return Promise.resolve([
            {
              entry: { type: 'Provider', id: 'anthropic', model: 'claude-sonnet-4' },
              scope: { type: 'SessionWide' },
            } satisfies ScopedRosterEntry,
            {
              entry: { type: 'Provider', id: 'ollama', model: 'llama3.1' },
              scope: { type: 'SessionWide' },
            } satisfies ScopedRosterEntry,
          ]);
        case 'list_mcp_servers':
        case 'list_agents':
          return Promise.resolve([]);
        default:
          return Promise.resolve(undefined);
      }
    });

    const { container } = render(() => <CatalogPane workspaceRoot="/ws" />);
    await flush();

    const rows = container.querySelectorAll('.catalog-row[data-kind="skills"][data-provider]');
    const colorIds = Array.from(rows).map((r) => r.getAttribute('data-provider'));
    expect(colorIds).toContain('anthropic');
    expect(colorIds).toContain('local');
  });

  it('non-provider rows and session-wide groups do not carry a data-provider tag', async () => {
    setupInvoke({
      skills: [skill('typescript-review')],
    });
    const { container } = render(() => <CatalogPane workspaceRoot="/ws" />);
    await flush();

    const taggedGroups = container.querySelectorAll('.catalog__group[data-provider]');
    expect(taggedGroups.length).toBe(0);
    const taggedRows = container.querySelectorAll('.catalog-row[data-provider]');
    expect(taggedRows.length).toBe(0);
  });
});

// F-734: + Add MCP server button on the MCP tab. Click opens the modal;
// a successful add refetches the MCP resource.
describe('<CatalogPane> + Add MCP server (F-734)', () => {
  it('renders the button only on the MCP tab', async () => {
    setupInvoke({ skills: [], mcp: [], agents: [] });
    const { findAllByRole, queryByTestId, findByTestId } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    // Default tab is `skills` — button is hidden.
    expect(queryByTestId('catalog-add-mcp')).toBeNull();

    const tabs = await findAllByRole('tab');
    const mcpTab = tabs.find((t) => t.textContent?.includes('MCP'))!;
    fireEvent.click(mcpTab);
    await flush();

    expect(await findByTestId('catalog-add-mcp')).toBeInTheDocument();
  });

  it('clicking the button opens the AddMcpServerForm modal', async () => {
    setupInvoke({ skills: [], mcp: [], agents: [] });
    const { findAllByRole, findByTestId, queryByTestId } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    const tabs = await findAllByRole('tab');
    const mcpTab = tabs.find((t) => t.textContent?.includes('MCP'))!;
    fireEvent.click(mcpTab);
    await flush();

    expect(queryByTestId('add-mcp-form')).toBeNull();
    fireEvent.click(await findByTestId('catalog-add-mcp'));
    await flush();

    expect(await findByTestId('add-mcp-form')).toBeInTheDocument();
  });

  it('successful add refetches list_mcp_servers and closes the dialog', async () => {
    let mcpFetches = 0;
    invokeMock.mockImplementation((cmd: string) => {
      switch (cmd) {
        case 'list_skills':
          return Promise.resolve([]);
        case 'list_mcp_servers':
          mcpFetches += 1;
          return Promise.resolve([]);
        case 'list_agents':
          return Promise.resolve([]);
        case 'add_mcp_server':
          return Promise.resolve(undefined);
        default:
          return Promise.resolve(undefined);
      }
    });

    const { findAllByRole, findByTestId, queryByTestId } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    const initialFetches = mcpFetches;

    const tabs = await findAllByRole('tab');
    const mcpTab = tabs.find((t) => t.textContent?.includes('MCP'))!;
    fireEvent.click(mcpTab);
    await flush();

    fireEvent.click(await findByTestId('catalog-add-mcp'));
    await flush();

    fireEvent.input(await findByTestId('add-mcp-name'), {
      target: { value: 'github' },
    });
    fireEvent.input(await findByTestId('add-mcp-command'), {
      target: { value: 'npx' },
    });
    fireEvent.click(await findByTestId('add-mcp-submit'));
    await flush();
    await new Promise((r) => setTimeout(r, 0));
    await flush();

    expect(mcpFetches).toBeGreaterThan(initialFetches);
    expect(queryByTestId('add-mcp-form')).toBeNull();
  });
});

// F-735: per-kind toggle coverage. Each asset kind (skills / mcp / agents)
// must wire the same `catalog.enabled.<kind>.<id>` write path, render an
// optimistic flip on click, and roll back on IPC rejection. Mounting must
// also pick up an existing persisted value from the settings store so a
// re-open of the pane preserves the user's choice.
describe('<CatalogPane> toggle per-kind coverage (F-735)', () => {
  const KIND_CASES: Array<{
    kind: 'skills' | 'mcp' | 'agents';
    label: string;
    tabLabel: string;
    fixture: (id: string) => ScopedRosterEntry;
  }> = [
    { kind: 'skills', label: 'Skills', tabLabel: 'Skills', fixture: skill },
    { kind: 'mcp', label: 'MCP', tabLabel: 'MCP', fixture: mcp },
    { kind: 'agents', label: 'Agents', tabLabel: 'Agents', fixture: (id) => agent(id) },
  ];

  for (const { kind, tabLabel, fixture } of KIND_CASES) {
    it(`${kind}: toggle on→off invokes set_setting with catalog.enabled.${kind}.<id>`, async () => {
      const setupOpts: SetupOpts = {};
      if (kind === 'skills') setupOpts.skills = [fixture('row-1')];
      if (kind === 'mcp') setupOpts.mcp = [fixture('row-1')];
      if (kind === 'agents') setupOpts.agents = [fixture('row-1')];
      setupInvoke(setupOpts);

      const { findAllByRole, findByRole } = render(() => (
        <CatalogPane workspaceRoot="/ws" />
      ));
      await flush();

      // Default tab is skills; switch tabs for mcp / agents so the row
      // panel paints and the switch lands in the tree.
      if (kind !== 'skills') {
        const tabs = await findAllByRole('tab');
        const target = tabs.find((t) => t.textContent?.includes(tabLabel))!;
        fireEvent.click(target);
        await flush();
      }

      const toggle = await findByRole('switch');
      fireEvent.click(toggle);
      await flush();

      expect(invokeMock).toHaveBeenCalledWith('set_setting', {
        key: `catalog.enabled.${kind}.row-1`,
        value: false,
        level: 'user',
        workspaceRoot: '/ws',
      });
    });

    it(`${kind}: click flips the visual optimistically before the IPC resolves`, async () => {
      // Hold the set_setting promise so the optimistic flip is observable
      // as a discrete pre-resolution state.
      let release!: () => void;
      const pending = new Promise<void>((res) => {
        release = res;
      });
      const setupOpts: SetupOpts = {};
      if (kind === 'skills') setupOpts.skills = [fixture('row-1')];
      if (kind === 'mcp') setupOpts.mcp = [fixture('row-1')];
      if (kind === 'agents') setupOpts.agents = [fixture('row-1')];

      invokeMock.mockImplementation((cmd: string) => {
        switch (cmd) {
          case 'list_skills':
            return Promise.resolve(setupOpts.skills ?? []);
          case 'list_mcp_servers':
            return Promise.resolve(setupOpts.mcp ?? []);
          case 'list_agents':
            return Promise.resolve(setupOpts.agents ?? []);
          case 'set_setting':
            return pending.then(() => undefined);
          default:
            return Promise.resolve(undefined);
        }
      });

      const { findAllByRole, findByRole } = render(() => (
        <CatalogPane workspaceRoot="/ws" />
      ));
      await flush();
      if (kind !== 'skills') {
        const tabs = await findAllByRole('tab');
        const target = tabs.find((t) => t.textContent?.includes(tabLabel))!;
        fireEvent.click(target);
        await flush();
      }

      const toggle = (await findByRole('switch')) as HTMLInputElement;
      expect(toggle.getAttribute('aria-checked')).toBe('true');
      fireEvent.click(toggle);
      await flush();

      // IPC is still pending — visual must already reflect the optimistic
      // off state and aria-busy must be set.
      await waitFor(() => {
        expect(toggle.getAttribute('aria-checked')).toBe('false');
        expect(toggle.getAttribute('aria-busy')).toBe('true');
      });

      release();
      await flush();
      await waitFor(() => expect(toggle.getAttribute('aria-busy')).toBe('false'));
    });

    it(`${kind}: rolls back the visual when set_setting rejects`, async () => {
      const setupOpts: SetupOpts = { setSettingError: 'invalid value' };
      if (kind === 'skills') setupOpts.skills = [fixture('row-1')];
      if (kind === 'mcp') setupOpts.mcp = [fixture('row-1')];
      if (kind === 'agents') setupOpts.agents = [fixture('row-1')];
      setupInvoke(setupOpts);

      const { findAllByRole, findByRole, findByText } = render(() => (
        <CatalogPane workspaceRoot="/ws" />
      ));
      await flush();
      if (kind !== 'skills') {
        const tabs = await findAllByRole('tab');
        const target = tabs.find((t) => t.textContent?.includes(tabLabel))!;
        fireEvent.click(target);
        await flush();
      }

      const toggle = (await findByRole('switch')) as HTMLInputElement;
      expect(toggle.getAttribute('aria-checked')).toBe('true');
      fireEvent.click(toggle);
      await flush();
      await new Promise((r) => setTimeout(r, 0));
      await flush();

      // Visual reverted; section-level error line surfaced with the verbatim
      // detail under the `set_setting failed:` prefix.
      await waitFor(() => expect(toggle.getAttribute('aria-checked')).toBe('true'));
      expect(await findByText(/set_setting failed: invalid value/)).toBeTruthy();
    });

    it(`${kind}: hydrates the initial visual state from the settings store on mount`, async () => {
      // Seed the store with a persisted disabled flag before mount so the
      // toggle renders the disabled visual immediately, matching what a
      // freshly-loaded settings tier would supply.
      applyLocalUpdate(`catalog.enabled.${kind}.row-1`, false);

      const setupOpts: SetupOpts = {};
      if (kind === 'skills') setupOpts.skills = [fixture('row-1')];
      if (kind === 'mcp') setupOpts.mcp = [fixture('row-1')];
      if (kind === 'agents') setupOpts.agents = [fixture('row-1')];
      setupInvoke(setupOpts);

      const { findAllByRole, findByRole } = render(() => (
        <CatalogPane workspaceRoot="/ws" />
      ));
      await flush();
      if (kind !== 'skills') {
        const tabs = await findAllByRole('tab');
        const target = tabs.find((t) => t.textContent?.includes(tabLabel))!;
        fireEvent.click(target);
        await flush();
      }

      const toggle = (await findByRole('switch')) as HTMLInputElement;
      expect(toggle.getAttribute('aria-checked')).toBe('false');
      expect(toggle.checked).toBe(false);
    });
  }
});

// F-736: filter chips. Single-select radiogroup composing with the search
// input. Chip set is `[All, Enabled, Workspace, User]` on every tab, plus
// `[stdio, http]` on the MCP tab. Switching off MCP with `stdio`/`http`
// active resets the selection back to `All` per spec.
//
// The `tier` / `transport` predicates key off fields the catalog `list_*`
// IPC will grow as the loader differentiates workspace vs user roots and
// stamps transport tags onto `Mcp` roster entries; fixtures here pass those
// fields directly as documented by the F-714 spec.
describe('<CatalogPane> filter chips (F-736)', () => {
  const skillIn = (id: string, tier?: 'workspace' | 'user'): ScopedRosterEntry => ({
    entry: { type: 'Skill', id },
    scope: { type: 'SessionWide' },
    ...(tier ? { tier } : {}),
  });

  const mcpIn = (
    id: string,
    opts: { tier?: 'workspace' | 'user'; transport?: 'stdio' | 'http' } = {},
  ): ScopedRosterEntry => ({
    entry: {
      type: 'Mcp',
      id,
      ...(opts.transport ? { transport: opts.transport } : {}),
    } as never,
    scope: { type: 'SessionWide' },
    ...(opts.tier ? { tier: opts.tier } : {}),
  });

  const switchToTab = async (
    findAllByRole: (role: string) => Promise<HTMLElement[]>,
    tabLabel: 'Skills' | 'MCP' | 'Agents',
  ): Promise<void> => {
    const tabs = await findAllByRole('tab');
    const target = tabs.find((t) => t.textContent?.includes(tabLabel))!;
    fireEvent.click(target);
    await flush();
  };

  it('renders the base chip set on the Skills tab (no stdio/http)', async () => {
    setupInvoke({ skills: [skill('alpha')] });
    const { findByTestId, queryByTestId } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    expect(await findByTestId('catalog-chip-skills-all')).toBeInTheDocument();
    expect(await findByTestId('catalog-chip-skills-enabled')).toBeInTheDocument();
    expect(await findByTestId('catalog-chip-skills-workspace')).toBeInTheDocument();
    expect(await findByTestId('catalog-chip-skills-user')).toBeInTheDocument();
    expect(queryByTestId('catalog-chip-skills-stdio')).toBeNull();
    expect(queryByTestId('catalog-chip-skills-http')).toBeNull();
  });

  it('renders the base chip set on the Agents tab (no stdio/http)', async () => {
    setupInvoke({ agents: [agent('a1')] });
    const { findAllByRole, findByTestId, queryByTestId } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();
    await switchToTab(findAllByRole, 'Agents');

    expect(await findByTestId('catalog-chip-agents-all')).toBeInTheDocument();
    expect(await findByTestId('catalog-chip-agents-enabled')).toBeInTheDocument();
    expect(await findByTestId('catalog-chip-agents-workspace')).toBeInTheDocument();
    expect(await findByTestId('catalog-chip-agents-user')).toBeInTheDocument();
    expect(queryByTestId('catalog-chip-agents-stdio')).toBeNull();
    expect(queryByTestId('catalog-chip-agents-http')).toBeNull();
  });

  it('renders the extended chip set on the MCP tab (with stdio/http)', async () => {
    setupInvoke({ mcp: [mcp('m1')] });
    const { findAllByRole, findByTestId } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();
    await switchToTab(findAllByRole, 'MCP');

    expect(await findByTestId('catalog-chip-mcp-all')).toBeInTheDocument();
    expect(await findByTestId('catalog-chip-mcp-enabled')).toBeInTheDocument();
    expect(await findByTestId('catalog-chip-mcp-workspace')).toBeInTheDocument();
    expect(await findByTestId('catalog-chip-mcp-user')).toBeInTheDocument();
    expect(await findByTestId('catalog-chip-mcp-stdio')).toBeInTheDocument();
    expect(await findByTestId('catalog-chip-mcp-http')).toBeInTheDocument();
  });

  it('chip strip renders as a radiogroup with single-select aria-checked', async () => {
    setupInvoke({ skills: [skill('alpha')] });
    const { findByTestId, container } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    const group = await findByTestId('catalog-chips-skills');
    expect(group.getAttribute('role')).toBe('radiogroup');
    expect(group.getAttribute('aria-label')).toBe('Catalog filters');

    const radios = container.querySelectorAll(
      '[data-testid="catalog-chips-skills"] [role="radio"]',
    );
    expect(radios.length).toBe(4);
    // Default selection: only `All` is aria-checked=true; the others are false.
    const checkedCount = Array.from(radios).filter(
      (r) => r.getAttribute('aria-checked') === 'true',
    ).length;
    expect(checkedCount).toBe(1);
    expect(
      (await findByTestId('catalog-chip-skills-all')).getAttribute('aria-checked'),
    ).toBe('true');
  });

  it('clicking a chip moves the active selection (radio semantics)', async () => {
    setupInvoke({ skills: [skill('alpha')] });
    const { findByTestId } = render(() => <CatalogPane workspaceRoot="/ws" />);
    await flush();

    const allChip = await findByTestId('catalog-chip-skills-all');
    const enabledChip = await findByTestId('catalog-chip-skills-enabled');
    expect(allChip.getAttribute('aria-checked')).toBe('true');
    expect(enabledChip.getAttribute('aria-checked')).toBe('false');

    fireEvent.click(enabledChip);
    await flush();

    expect(allChip.getAttribute('aria-checked')).toBe('false');
    expect(enabledChip.getAttribute('aria-checked')).toBe('true');
  });

  it('Enabled chip filters out rows where catalog.enabled.<kind>.<id> === false', async () => {
    setupInvoke({ skills: [skill('alpha'), skill('beta')] });
    applyLocalUpdate('catalog.enabled.skills.beta', false);

    const { findByTestId, findByText, queryByText } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    // Default chip `All` shows both rows.
    expect(await findByText('alpha')).toBeTruthy();
    expect(await findByText('beta')).toBeTruthy();

    fireEvent.click(await findByTestId('catalog-chip-skills-enabled'));
    await flush();

    // `beta` is disabled in the store; only `alpha` survives the chip filter.
    expect(await findByText('alpha')).toBeTruthy();
    expect(queryByText('beta')).toBeNull();
  });

  it('Workspace chip filters to workspace-tier rows', async () => {
    setupInvoke({
      skills: [skillIn('ws-only', 'workspace'), skillIn('usr-only', 'user')],
    });
    const { findByTestId, findByText, queryByText } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    fireEvent.click(await findByTestId('catalog-chip-skills-workspace'));
    await flush();

    expect(await findByText('ws-only')).toBeTruthy();
    expect(queryByText('usr-only')).toBeNull();
  });

  it('User chip filters to user-tier rows', async () => {
    setupInvoke({
      skills: [skillIn('ws-only', 'workspace'), skillIn('usr-only', 'user')],
    });
    const { findByTestId, findByText, queryByText } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    fireEvent.click(await findByTestId('catalog-chip-skills-user'));
    await flush();

    expect(await findByText('usr-only')).toBeTruthy();
    expect(queryByText('ws-only')).toBeNull();
  });

  it('stdio chip filters MCP rows to transport=stdio', async () => {
    setupInvoke({
      mcp: [
        mcpIn('via-stdio', { transport: 'stdio' }),
        mcpIn('via-http', { transport: 'http' }),
      ],
    });
    const { findAllByRole, findByTestId, findByText, queryByText } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();
    await switchToTab(findAllByRole, 'MCP');

    fireEvent.click(await findByTestId('catalog-chip-mcp-stdio'));
    await flush();

    expect(await findByText('via-stdio')).toBeTruthy();
    expect(queryByText('via-http')).toBeNull();
  });

  it('http chip filters MCP rows to transport=http', async () => {
    setupInvoke({
      mcp: [
        mcpIn('via-stdio', { transport: 'stdio' }),
        mcpIn('via-http', { transport: 'http' }),
      ],
    });
    const { findAllByRole, findByTestId, findByText, queryByText } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();
    await switchToTab(findAllByRole, 'MCP');

    fireEvent.click(await findByTestId('catalog-chip-mcp-http'));
    await flush();

    expect(await findByText('via-http')).toBeTruthy();
    expect(queryByText('via-stdio')).toBeNull();
  });

  it('chip predicate composes with the search input', async () => {
    // alpha=workspace, alpha-2=workspace, beta=user. With Workspace chip
    // active, search='alpha' should leave both `alpha*` rows; the user row
    // is gated out by the chip and the workspace alpha-2 / alpha rows are
    // narrowed by the search to themselves; `beta` is excluded by both.
    setupInvoke({
      skills: [
        skillIn('alpha', 'workspace'),
        skillIn('alpha-2', 'workspace'),
        skillIn('beta', 'user'),
      ],
    });
    const { findByLabelText, findByTestId, findByText, queryByText } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    fireEvent.click(await findByTestId('catalog-chip-skills-workspace'));
    await flush();

    const search = await findByLabelText('Filter catalog entries');
    fireEvent.input(search, { target: { value: 'alpha' } });
    await flush();

    expect(await findByText('alpha')).toBeTruthy();
    expect(await findByText('alpha-2')).toBeTruthy();
    expect(queryByText('beta')).toBeNull();

    // Narrow further: typing the full id leaves only the exact match.
    fireEvent.input(search, { target: { value: 'alpha-2' } });
    await flush();
    expect(await findByText('alpha-2')).toBeTruthy();
    expect(queryByText('alpha')).toBeNull();
  });

  it('chip-only filter miss renders the chip-flavoured empty state', async () => {
    // Roster is non-empty but every row is disabled — selecting `Enabled`
    // empties the visible set without any search query. The empty hint
    // must mention the chip, not the (absent) search query.
    setupInvoke({ skills: [skill('only')] });
    applyLocalUpdate('catalog.enabled.skills.only', false);

    const { findByTestId, findByText } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    fireEvent.click(await findByTestId('catalog-chip-skills-enabled'));
    await flush();

    expect(await findByText('No matches')).toBeTruthy();
    expect(await findByText(/No Skills match the .Enabled. filter/)).toBeTruthy();
  });

  it('leaving the MCP tab while stdio/http is active resets the chip to All', async () => {
    setupInvoke({
      skills: [skill('s1')],
      mcp: [mcpIn('m1', { transport: 'stdio' })],
    });
    const { findAllByRole, findByTestId } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();
    await switchToTab(findAllByRole, 'MCP');

    fireEvent.click(await findByTestId('catalog-chip-mcp-stdio'));
    await flush();
    expect((await findByTestId('catalog-chip-mcp-stdio')).getAttribute('aria-checked')).toBe('true');

    await switchToTab(findAllByRole, 'Skills');

    // Selection collapsed back to `All` because `stdio` is no longer valid
    // for the Skills chip set.
    const allChip = await findByTestId('catalog-chip-skills-all');
    expect(allChip.getAttribute('aria-checked')).toBe('true');
  });

  it('non-transport chip selection persists when switching tabs', async () => {
    setupInvoke({ skills: [skill('s1')], mcp: [mcp('m1')], agents: [agent('a1')] });
    const { findAllByRole, findByTestId } = render(() => (
      <CatalogPane workspaceRoot="/ws" />
    ));
    await flush();

    fireEvent.click(await findByTestId('catalog-chip-skills-enabled'));
    await flush();

    await switchToTab(findAllByRole, 'MCP');
    expect(
      (await findByTestId('catalog-chip-mcp-enabled')).getAttribute('aria-checked'),
    ).toBe('true');

    await switchToTab(findAllByRole, 'Agents');
    expect(
      (await findByTestId('catalog-chip-agents-enabled')).getAttribute('aria-checked'),
    ).toBe('true');
  });
});
