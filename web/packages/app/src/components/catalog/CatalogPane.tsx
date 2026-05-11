// F-592: Catalog UI — three tabs (Skills / MCP / Agents) over F-591's
// `list_*` IPC commands. Search is shared across tabs; enable/disable is
// persisted via `set_setting` under the `catalog.enabled.<kind>.<id>`
// keyspace (default true). No new Tauri commands beyond F-591 + F-151.
//
// Per-tab "Providers" is intentionally not a top-level tab — provider
// discovery lives next door in the Dashboard's `<ProvidersSection>` (F-586).
// We still surface providers under the Skills/MCP/Agents grouping when the
// scope demands it, but the catalog's primary axis is the three asset kinds
// the user can toggle.

import {
  createMemo,
  createResource,
  createSignal,
  For,
  Show,
  type Component,
} from 'solid-js';
import { Button, Skeleton, Tab, Tabs } from '@forge/design';
import type { RosterEntry, ScopedRosterEntry } from '@forge/ipc';
import {
  listAgents,
  listMcpServers,
  listSkills,
  SESSION_WIDE_SCOPE,
} from '../../ipc/catalog';
import { settings } from '../../stores/settings';
import { AddMcpServerForm } from '../AddMcpServerForm';
import { CatalogEnabledToggle } from './CatalogEnabledToggle';
import './CatalogPane.css';

export type CatalogKind = 'skills' | 'mcp' | 'agents';

export interface CatalogPaneProps {
  workspaceRoot: string;
}

interface KindConfig {
  id: CatalogKind;
  label: string;
  fetch: (workspaceRoot: string) => Promise<ScopedRosterEntry[]>;
  /** Empty-state copy when zero entries are loaded across all scopes. */
  emptyTitle: string;
  emptyHint: string;
}

const KINDS: KindConfig[] = [
  {
    id: 'skills',
    label: 'Skills',
    fetch: (ws) => listSkills(ws, SESSION_WIDE_SCOPE),
    emptyTitle: 'No skills installed',
    emptyHint: 'Drop a SKILL.md under .skills/<name>/ in your workspace or ~/.skills/.',
  },
  {
    id: 'mcp',
    label: 'MCP',
    fetch: (ws) => listMcpServers(ws, SESSION_WIDE_SCOPE),
    emptyTitle: 'No MCP servers configured',
    emptyHint: 'Add a server entry to .mcp.json in your workspace or ~/.mcp.json.',
  },
  {
    id: 'agents',
    label: 'Agents',
    fetch: (ws) => listAgents(ws, SESSION_WIDE_SCOPE),
    emptyTitle: 'No agents defined',
    emptyHint: 'Add a definition under .agents/<name>.md in your workspace or ~/.agents/.',
  },
];

interface CatalogRow {
  kind: CatalogKind;
  /** Stable id used for the enable/disable settings key + DOM keying. */
  id: string;
  name: string;
  /** Free-form metadata line: provider model, agent background flag, etc. */
  meta: string;
  scope: ScopedRosterEntry['scope'];
  /** F-694: design-token color id when the row itself is a Provider entry. */
  providerColor: ProviderColorId | null;
  /**
   * F-736: file-tier the entry was loaded from (`workspace`/.<kind>` vs
   * `~/.<kind>`). The `Workspace` / `User` filter chips key off this.
   * Sourced from an optional `tier` field on the scope payload when the
   * backend supplies it; `undefined` otherwise.
   */
  tier: CatalogTier | undefined;
  /**
   * F-736: transport variant for MCP rows (`stdio` | `http`). Sourced from
   * an optional `transport` field on the `Mcp` roster entry when the backend
   * supplies it; `undefined` otherwise (including for non-MCP rows). The
   * MCP-only `stdio` / `http` filter chips key off this.
   */
  transport: McpTransport | undefined;
}

type CatalogTier = 'workspace' | 'user';
type McpTransport = 'stdio' | 'http';

/**
 * F-694: maps a runtime provider id onto one of the four `--color-provider-*`
 * design tokens. Runtime ids (`anthropic`, `openai`, `ollama`,
 * `custom_openai:<name>`) are richer than the four-color discipline; this
 * collapses them onto the canonical token names per `docs/design/ai-patterns.md`.
 */
type ProviderColorId = 'anthropic' | 'openai' | 'local' | 'custom';

function providerColorId(id: string): ProviderColorId {
  if (id === 'anthropic') return 'anthropic';
  if (id === 'openai') return 'openai';
  if (id === 'ollama' || id === 'lm-studio' || id === 'local') return 'local';
  return 'custom';
}

function rosterId(entry: RosterEntry): string {
  switch (entry.type) {
    case 'Skill':
      return entry.id;
    case 'Mcp':
      return entry.id;
    case 'Agent':
      return entry.id;
    case 'Provider':
      return entry.id;
  }
}

function rosterMeta(entry: RosterEntry): string {
  switch (entry.type) {
    case 'Provider':
      return entry.model ?? '—';
    case 'Agent':
      return entry.background ? 'background' : 'foreground';
    case 'Skill':
    case 'Mcp':
      return '';
  }
}

function toRow(kind: CatalogKind, scoped: ScopedRosterEntry): CatalogRow {
  return {
    kind,
    id: rosterId(scoped.entry),
    name: rosterId(scoped.entry),
    meta: rosterMeta(scoped.entry),
    scope: scoped.scope,
    providerColor:
      scoped.entry.type === 'Provider' ? providerColorId(scoped.entry.id) : null,
    tier: readTier(scoped.scope),
    transport: readTransport(scoped.entry),
  };
}

/**
 * F-736: extract the file-tier from a roster scope. The generated `RosterScope`
 * union doesn't carry a `tier` field today; the catalog list_* commands will
 * grow one as the loader differentiates workspace vs user roots. Read it via
 * an unchecked property access so the filter predicates work the moment the
 * backend stamps it in, without round-tripping through the IPC type-gen.
 */
function readTier(scope: ScopedRosterEntry['scope']): CatalogTier | undefined {
  const value = (scope as { tier?: unknown }).tier;
  return value === 'workspace' || value === 'user' ? value : undefined;
}

/**
 * F-736: extract the transport (`stdio`/`http`) from an MCP roster entry.
 * The generated `RosterEntry::Mcp` variant carries only `{ type, id }`
 * today; the loader will surface the transport tag alongside it. Until then,
 * predicates that key off `transport` return false, which mirrors the spec
 * intent (the chip "filters to rows whose transport is X").
 */
function readTransport(entry: ScopedRosterEntry['entry']): McpTransport | undefined {
  if (entry.type !== 'Mcp') return undefined;
  const value = (entry as { transport?: unknown }).transport;
  return value === 'stdio' || value === 'http' ? value : undefined;
}

function scopeLabel(scope: ScopedRosterEntry['scope']): string {
  switch (scope.type) {
    case 'SessionWide':
      return 'Session-wide';
    case 'Agent':
      return `Agent · ${scope.id}`;
    case 'Provider':
      return `Provider · ${scope.id}`;
  }
}

function scopeKey(scope: ScopedRosterEntry['scope']): string {
  switch (scope.type) {
    case 'SessionWide':
      return 'session-wide';
    case 'Agent':
      return `agent:${scope.id}`;
    case 'Provider':
      return `provider:${scope.id}`;
  }
}

interface ScopeGroup {
  key: string;
  label: string;
  rows: CatalogRow[];
  /** F-694: design-token color id for provider-scoped groups; null otherwise. */
  providerColor: ProviderColorId | null;
}

function groupByScope(rows: CatalogRow[]): ScopeGroup[] {
  const groups = new Map<string, ScopeGroup>();
  for (const row of rows) {
    const key = scopeKey(row.scope);
    let group = groups.get(key);
    if (group === undefined) {
      group = {
        key,
        label: scopeLabel(row.scope),
        rows: [],
        providerColor:
          row.scope.type === 'Provider' ? providerColorId(row.scope.id) : null,
      };
      groups.set(key, group);
    }
    group.rows.push(row);
  }
  return Array.from(groups.values());
}

/**
 * Read the persisted enable flag for `(kind, id)`. The settings store carries
 * a typed `catalog.enabled` map (F-592 schema in `AppSettings`); absent entries
 * default to `true`, matching the spec's "default enabled" requirement. Solid's
 * fine-grained store reactivity re-runs the accessor on every store write, so
 * toggle clicks paint immediately.
 */
function isEnabled(kind: CatalogKind, id: string): boolean {
  const kindMap = settings.catalog.enabled[kind];
  if (!kindMap) return true;
  const value = kindMap[id];
  return typeof value === 'boolean' ? value : true;
}

// F-736: filter chip strip per `docs/ui-specs/catalog.md §Search and filters`.
// Single-select (`role="radiogroup"`); selecting one clears the others;
// `stdio`/`http` chips render only on the MCP tab; switching off MCP resets
// the selection to `'all'` if either was active.
type FilterChip = 'all' | 'enabled' | 'workspace' | 'user' | 'stdio' | 'http';

interface ChipDef {
  id: FilterChip;
  label: string;
}

const BASE_CHIPS: ChipDef[] = [
  { id: 'all', label: 'All' },
  { id: 'enabled', label: 'Enabled' },
  { id: 'workspace', label: 'Workspace' },
  { id: 'user', label: 'User' },
];

const MCP_TRANSPORT_CHIPS: ChipDef[] = [
  { id: 'stdio', label: 'stdio' },
  { id: 'http', label: 'http' },
];

function chipsForKind(kind: CatalogKind): ChipDef[] {
  return kind === 'mcp' ? [...BASE_CHIPS, ...MCP_TRANSPORT_CHIPS] : BASE_CHIPS;
}

function chipLabel(chip: FilterChip): string {
  const all = [...BASE_CHIPS, ...MCP_TRANSPORT_CHIPS];
  return all.find((c) => c.id === chip)?.label ?? chip;
}

function chipPredicate(chip: FilterChip): (row: CatalogRow) => boolean {
  switch (chip) {
    case 'all':
      return () => true;
    case 'enabled':
      return (row) => isEnabled(row.kind, row.id);
    case 'workspace':
      return (row) => row.tier === 'workspace';
    case 'user':
      return (row) => row.tier === 'user';
    case 'stdio':
      return (row) => row.transport === 'stdio';
    case 'http':
      return (row) => row.transport === 'http';
  }
}

export const CatalogPane: Component<CatalogPaneProps> = (props) => {
  const [activeKind, setActiveKindSignal] = createSignal<CatalogKind>('skills');
  const [search, setSearch] = createSignal('');
  const [toggleError, setToggleError] = createSignal<string | null>(null);
  // F-736: filter chip selection. Route-local; defaults to `'all'`. Switching
  // tabs persists the selection *unless* the previously-selected chip is no
  // longer valid for the new tab (i.e. `stdio`/`http` leaving the MCP tab),
  // in which case the spec calls for a reset back to `'all'`.
  const [activeChip, setActiveChip] = createSignal<FilterChip>('all');
  const setActiveKind = (kind: CatalogKind): void => {
    setActiveKindSignal(kind);
    if (kind !== 'mcp' && (activeChip() === 'stdio' || activeChip() === 'http')) {
      setActiveChip('all');
    }
  };
  // F-734: Catalog `+ Add MCP server` modal. Mounted at the pane level so
  // the dialog can refetch the MCP resource on success without re-rendering
  // the tab panel. The button itself lives in the MCP tab header block
  // (see the `mcp-tab-header` region below).
  const [addMcpOpen, setAddMcpOpen] = createSignal(false);

  // F-592: each kind owns its own resource so a slow / failing skill loader
  // does not block MCP + Agents tabs from rendering. The tab's loading,
  // empty, error, and ready states are surfaced independently.
  const skillsRes = createResource(() => props.workspaceRoot, KINDS[0]!.fetch);
  const mcpRes = createResource(() => props.workspaceRoot, KINDS[1]!.fetch);
  const agentsRes = createResource(() => props.workspaceRoot, KINDS[2]!.fetch);

  const resourceFor = (kind: CatalogKind) => {
    switch (kind) {
      case 'skills':
        return skillsRes;
      case 'mcp':
        return mcpRes;
      case 'agents':
        return agentsRes;
    }
  };

  const filterRows = (rows: CatalogRow[]): CatalogRow[] => {
    const q = search().trim().toLowerCase();
    const chip = chipPredicate(activeChip());
    const matchesSearch = (r: CatalogRow): boolean =>
      q.length === 0 || r.name.toLowerCase().includes(q);
    return rows.filter((r) => matchesSearch(r) && chip(r));
  };

  const rowsForKind = (kind: CatalogKind): CatalogRow[] => {
    const [resource] = resourceFor(kind);
    // F-401 pattern: reading `resource()` while the resource is in `'errored'`
    // state re-throws in the reactive scope. Gate on the state so the
    // rejection stays observable via `errorDetail()` without crashing the
    // panel.
    if (resource.state !== 'ready') return [];
    const data = resource();
    if (!data) return [];
    return data.map((scoped) => toRow(kind, scoped));
  };

  // F-592: per-tab badge counts reflect post-filter row counts so the search
  // box's effect on each tab is visible without flipping through them.
  const skillsCount = createMemo(() => filterRows(rowsForKind('skills')).length);
  const mcpCount = createMemo(() => filterRows(rowsForKind('mcp')).length);
  const agentsCount = createMemo(() => filterRows(rowsForKind('agents')).length);

  // F-735: toggle component owns the IPC roundtrip + optimistic rollback;
  // the pane only sinks the verbatim error into the section-level alert
  // line so the spec's "section-level error" surface stays the canonical
  // home for `set_setting` failures.
  const handleToggleError = (detail: string) => {
    setToggleError(`set_setting failed: ${detail}`);
  };
  const handleToggleSuccess = () => {
    setToggleError(null);
  };

  const handleSearchInput = (e: InputEvent) => {
    const target = e.currentTarget as HTMLInputElement;
    setSearch(target.value);
  };

  return (
    <section class="catalog" aria-label="Catalog">
      <header class="catalog__header">
        <h2 class="catalog__title">Catalog</h2>
        <input
          class="catalog__search"
          type="search"
          placeholder="Filter skills, MCP, agents…"
          aria-label="Filter catalog entries"
          value={search()}
          onInput={handleSearchInput}
        />
      </header>

      <Tabs class="catalog__tabs" aria-label="Catalog kind">
        <CatalogTab
          kind="skills"
          label="Skills"
          active={activeKind() === 'skills'}
          count={skillsCount()}
          onSelect={setActiveKind}
        />
        <CatalogTab
          kind="mcp"
          label="MCP"
          active={activeKind() === 'mcp'}
          count={mcpCount()}
          onSelect={setActiveKind}
        />
        <CatalogTab
          kind="agents"
          label="Agents"
          active={activeKind() === 'agents'}
          count={agentsCount()}
          onSelect={setActiveKind}
        />
      </Tabs>

      <Show when={toggleError()}>
        {(msg) => (
          <p class="catalog__action-error" role="alert">
            {msg()}
          </p>
        )}
      </Show>

      {/* F-734: + Add MCP server modal. Default scope = `workspace` since the
          catalog only mounts with a registered workspace root. */}
      <AddMcpServerForm
        open={addMcpOpen()}
        scope="workspace"
        workspaceRoot={props.workspaceRoot}
        onClose={() => setAddMcpOpen(false)}
        onAdded={() => {
          mcpRes[1].refetch();
        }}
      />

      <For each={KINDS}>
        {(kind) => {
          const [resource] = resourceFor(kind.id);
          const visible = () => activeKind() === kind.id;
          const filteredRows = () => filterRows(rowsForKind(kind.id));
          const totalRows = () => rowsForKind(kind.id).length;
          const groups = () => groupByScope(filteredRows());
          const errorDetail = () => {
            const err = resource.error;
            if (!err) return null;
            return err instanceof Error ? err.message : String(err);
          };

          return (
            <Show when={visible()}>
              <div
                class="catalog__panel"
                role="tabpanel"
                id={`catalog-panel-${kind.id}`}
                aria-labelledby={`catalog-tab-${kind.id}`}
              >
                {/*
                  F-734: MCP-tab-only header block. Hosts the `+ Add MCP
                  server` button. F-736 will append filter chips into the
                  same region; structure as a flex row so chips can sit
                  alongside the button without further surgery.
                */}
                <Show when={kind.id === 'mcp'}>
                  <div class="catalog__mcp-tab-header" data-testid="mcp-tab-header">
                    <Button
                      variant="primary"
                      size="sm"
                      type="button"
                      data-testid="catalog-add-mcp"
                      onClick={() => setAddMcpOpen(true)}
                    >
                      + Add MCP server
                    </Button>
                  </div>
                </Show>

                {/*
                  F-736: filter chips. Single-select; `stdio`/`http` only on
                  the MCP tab. Composes with the header search input through
                  `filterRows`.
                */}
                <Tabs
                  variant="radio"
                  class="catalog__chips"
                  aria-label="Catalog filters"
                  data-testid={`catalog-chips-${kind.id}`}
                >
                  <For each={chipsForKind(kind.id)}>
                    {(chip) => (
                      <Tab
                        variant="radio"
                        selected={activeChip() === chip.id}
                        data-testid={`catalog-chip-${kind.id}-${chip.id}`}
                        onClick={() => setActiveChip(chip.id)}
                      >
                        {chip.label}
                      </Tab>
                    )}
                  </For>
                </Tabs>

                <Show when={resource.loading}>
                  <Skeleton
                    variant="block"
                    count={4}
                    label={`Loading ${kind.label.toLowerCase()}`}
                    class="catalog__skeleton"
                    data-testid={`catalog-loading-${kind.id}`}
                  />
                </Show>

                <Show when={errorDetail()}>
                  {(detail) => (
                    <div class="catalog__error" role="alert">
                      <p class="catalog__error-title">
                        {kind.label.toUpperCase()} UNAVAILABLE
                      </p>
                      <p class="catalog__error-detail">{detail()}</p>
                    </div>
                  )}
                </Show>

                <Show
                  when={
                    resource.state === 'ready' &&
                    !resource.loading &&
                    !errorDetail()
                  }
                >
                  <Show
                    when={totalRows() > 0}
                    fallback={
                      <div class="catalog__empty" data-empty-kind={kind.id}>
                        <p class="catalog__empty-title">{kind.emptyTitle}</p>
                        <p class="catalog__empty-hint">{kind.emptyHint}</p>
                      </div>
                    }
                  >
                    <Show
                      when={filteredRows().length > 0}
                      fallback={
                        <div
                          class="catalog__empty"
                          data-empty-kind={kind.id}
                          data-empty-reason={search().trim() ? 'search' : 'chip'}
                          role="status"
                          aria-live="polite"
                        >
                          <p class="catalog__empty-title">No matches</p>
                          <p class="catalog__empty-hint">
                            {search().trim()
                              ? `Nothing in ${kind.label} matches “${search()}”.`
                              : `No ${kind.label} match the “${chipLabel(activeChip())}” filter.`}
                          </p>
                        </div>
                      }
                    >
                      <ul class="catalog__groups">
                        <For each={groups()}>
                          {(group) => (
                            <li
                              class="catalog__group"
                              data-provider={group.providerColor ?? undefined}
                            >
                              <h3 class="catalog__group-label">{group.label}</h3>
                              <ul class="catalog__rows">
                                <For each={group.rows}>
                                  {(row) => (
                                    <CatalogRowView
                                      row={row}
                                      enabled={isEnabled(row.kind, row.id)}
                                      workspaceRoot={props.workspaceRoot}
                                      onError={handleToggleError}
                                      onToggled={handleToggleSuccess}
                                    />
                                  )}
                                </For>
                              </ul>
                            </li>
                          )}
                        </For>
                      </ul>
                    </Show>
                  </Show>
                </Show>
              </div>
            </Show>
          );
        }}
      </For>
    </section>
  );
};

interface CatalogTabProps {
  kind: CatalogKind;
  label: string;
  active: boolean;
  count: number;
  onSelect: (kind: CatalogKind) => void;
}

const CatalogTab: Component<CatalogTabProps> = (props) => (
  <Tab
    id={`catalog-tab-${props.kind}`}
    selected={props.active}
    badgeCount={props.count}
    aria-controls={`catalog-panel-${props.kind}`}
    onClick={() => props.onSelect(props.kind)}
  >
    {props.label}
  </Tab>
);

interface CatalogRowViewProps {
  row: CatalogRow;
  enabled: boolean;
  workspaceRoot: string;
  onError: (detail: string) => void;
  onToggled: (enabled: boolean) => void;
}

const CatalogRowView: Component<CatalogRowViewProps> = (props) => {
  return (
    <li
      class="catalog-row"
      data-kind={props.row.kind}
      data-id={props.row.id}
      data-provider={props.row.providerColor ?? undefined}
    >
      <div class="catalog-row__body">
        <span class="catalog-row__name">{props.row.name}</span>
        <Show when={props.row.meta}>
          <span class="catalog-row__meta">{props.row.meta}</span>
        </Show>
      </div>
      <CatalogEnabledToggle
        kind={props.row.kind}
        id={props.row.id}
        name={props.row.name}
        enabled={props.enabled}
        level="user"
        workspaceRoot={props.workspaceRoot}
        onError={props.onError}
        onToggled={props.onToggled}
      />
    </li>
  );
};
