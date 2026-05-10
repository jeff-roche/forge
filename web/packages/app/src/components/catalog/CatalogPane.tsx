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
import { Skeleton, Tab, Tabs } from '@forge/design';
import type { RosterEntry, ScopedRosterEntry } from '@forge/ipc';
import {
  listAgents,
  listMcpServers,
  listSkills,
  SESSION_WIDE_SCOPE,
} from '../../ipc/catalog';
import { settings, setSetting } from '../../stores/settings';
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
}

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
  };
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

export const CatalogPane: Component<CatalogPaneProps> = (props) => {
  const [activeKind, setActiveKind] = createSignal<CatalogKind>('skills');
  const [search, setSearch] = createSignal('');
  const [toggleError, setToggleError] = createSignal<string | null>(null);

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
    if (q.length === 0) return rows;
    return rows.filter((r) => r.name.toLowerCase().includes(q));
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

  const handleToggle = (kind: CatalogKind, id: string, next: boolean) => {
    setToggleError(null);
    const key = `catalog.enabled.${kind}.${id}`;
    setSetting(key, next, 'user', props.workspaceRoot).catch((err: unknown) => {
      const detail = err instanceof Error ? err.message : String(err);
      setToggleError(`set_setting failed: ${detail}`);
    });
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
                          data-empty-reason="search"
                          role="status"
                          aria-live="polite"
                        >
                          <p class="catalog__empty-title">No matches</p>
                          <p class="catalog__empty-hint">
                            Nothing in {kind.label} matches “{search()}”.
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
                                      onToggle={(next) => handleToggle(row.kind, row.id, next)}
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
  onToggle: (next: boolean) => void;
}

const CatalogRowView: Component<CatalogRowViewProps> = (props) => {
  const id = `catalog-toggle-${props.row.kind}-${props.row.id}`;
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
      <label class="catalog-row__toggle" for={id}>
        <span class="catalog-row__toggle-label">
          {props.enabled ? 'enabled' : 'disabled'}
        </span>
        <input
          id={id}
          type="checkbox"
          role="switch"
          aria-label={`${props.enabled ? 'Disable' : 'Enable'} ${props.row.name}`}
          checked={props.enabled}
          onChange={(e) => props.onToggle(e.currentTarget.checked)}
        />
      </label>
    </li>
  );
};
