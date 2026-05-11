// F-724: Dashboard "Enabled · workspace" card.
//
// Workspace-scoped slice of the catalog (skills / MCP servers / agents) with
// inline toggles. Reads live state from `list_skills` / `list_mcp_servers` /
// `list_agents` (F-591) and persists enable/disable through `set_setting`
// under `catalog.enabled.<kind>.<id>` (F-592 / F-735) — same write path the
// Catalog route uses. Absent settings default to enabled.

import {
  createMemo,
  createResource,
  createSignal,
  For,
  Show,
  type Component,
} from 'solid-js';
import { Button, Skeleton } from '@forge/design';
import type { ScopedRosterEntry } from '@forge/ipc';
import {
  listAgents,
  listMcpServers,
  listSkills,
  SESSION_WIDE_SCOPE,
} from '../../ipc/catalog';
import { activeWorkspaceRoot } from '../../stores/session';
import { settings, setSetting } from '../../stores/settings';
import './EnabledAssetsCard.css';

export type EnabledKind = 'skills' | 'mcp' | 'agents';

interface KindConfig {
  id: EnabledKind;
  title: string;
  fetch: (workspaceRoot: string) => Promise<ScopedRosterEntry[]>;
}

const KINDS: KindConfig[] = [
  { id: 'skills', title: 'Skills', fetch: (ws) => listSkills(ws, SESSION_WIDE_SCOPE) },
  { id: 'mcp', title: 'MCP servers', fetch: (ws) => listMcpServers(ws, SESSION_WIDE_SCOPE) },
  { id: 'agents', title: 'Agents', fetch: (ws) => listAgents(ws, SESSION_WIDE_SCOPE) },
];

interface EnabledRow {
  kind: EnabledKind;
  id: string;
  name: string;
}

function rowsFor(kind: EnabledKind, data: ScopedRosterEntry[] | undefined): EnabledRow[] {
  if (!data) return [];
  const rows: EnabledRow[] = [];
  for (const entry of data) {
    switch (entry.entry.type) {
      case 'Skill':
        if (kind === 'skills') rows.push({ kind, id: entry.entry.id, name: entry.entry.id });
        break;
      case 'Mcp':
        if (kind === 'mcp') rows.push({ kind, id: entry.entry.id, name: entry.entry.id });
        break;
      case 'Agent':
        if (kind === 'agents') rows.push({ kind, id: entry.entry.id, name: entry.entry.id });
        break;
      case 'Provider':
        break;
    }
  }
  return rows;
}

/**
 * Per-spec default: absent entries in `catalog.enabled.<kind>.<id>` read as
 * `true`. Solid's fine-grained store reactivity re-runs this accessor on
 * every store write, so toggle clicks paint immediately.
 */
function isEnabled(kind: EnabledKind, id: string): boolean {
  const kindMap = settings.catalog.enabled[kind];
  if (!kindMap) return true;
  const value = kindMap[id];
  return typeof value === 'boolean' ? value : true;
}

/** Last path segment of a workspace root (Unix or Windows separators). */
export function workspaceShortName(root: string | null): string {
  if (!root) return '';
  const trimmed = root.replace(/[/\\]+$/, '');
  const idx = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'));
  return idx === -1 ? trimmed : trimmed.slice(idx + 1);
}

export const EnabledAssetsCard: Component = () => {
  const workspaceRoot = (): string | null => activeWorkspaceRoot();
  const [toggleError, setToggleError] = createSignal<string | null>(null);

  // One resource per kind so a slow / failing loader does not block the others.
  const skillsRes = createResource(workspaceRoot, (ws) => KINDS[0]!.fetch(ws));
  const mcpRes = createResource(workspaceRoot, (ws) => KINDS[1]!.fetch(ws));
  const agentsRes = createResource(workspaceRoot, (ws) => KINDS[2]!.fetch(ws));

  const resources = [skillsRes, mcpRes, agentsRes] as const;

  const loading = createMemo(() =>
    resources.some(([r]) => r.loading),
  );

  const errorDetail = createMemo<string | null>(() => {
    for (const [r] of resources) {
      if (r.error) {
        return r.error instanceof Error ? r.error.message : String(r.error);
      }
    }
    return null;
  });

  const totalRows = createMemo(() => {
    let n = 0;
    for (let i = 0; i < KINDS.length; i += 1) {
      const [r] = resources[i]!;
      if (r.state === 'ready') n += rowsFor(KINDS[i]!.id, r()).length;
    }
    return n;
  });

  const refetchAll = () => {
    for (const [, { refetch }] of resources) void refetch();
  };

  const handleToggle = (kind: EnabledKind, id: string, next: boolean) => {
    setToggleError(null);
    const root = workspaceRoot();
    if (!root) {
      setToggleError('No workspace open. Open one to toggle enabled assets.');
      return;
    }
    const key = `catalog.enabled.${kind}.${id}`;
    setSetting(key, next, 'user', root).catch((err: unknown) => {
      const detail = err instanceof Error ? err.message : String(err);
      setToggleError(`set_setting failed: ${detail}`);
    });
  };

  return (
    <section
      class="enabled-assets"
      aria-label="Enabled workspace assets"
      data-testid="enabled-assets-card"
    >
      <header class="enabled-assets__header">
        <span class="enabled-assets__label">Enabled · workspace</span>
        <Show when={workspaceRoot()}>
          {(root) => (
            <span class="enabled-assets__workspace" data-testid="enabled-assets-workspace">
              {workspaceShortName(root())}
            </span>
          )}
        </Show>
      </header>

      <Show when={toggleError()}>
        {(msg) => (
          <p class="enabled-assets__action-error" role="alert">
            {msg()}
          </p>
        )}
      </Show>

      <Show
        when={workspaceRoot()}
        fallback={
          <p class="enabled-assets__empty" data-testid="enabled-assets-no-workspace">
            No workspace open. Open one to see enabled assets.
          </p>
        }
      >
        <Show when={loading()}>
          <Skeleton
            variant="block"
            count={5}
            label="Loading enabled assets"
            class="enabled-assets__skeleton"
            data-testid="enabled-assets-loading"
          />
        </Show>

        <Show when={!loading() && errorDetail()}>
          {(detail) => (
            <div class="enabled-assets__error" role="alert">
              <p class="enabled-assets__error-title">ENABLED EXTENSIONS UNAVAILABLE</p>
              <p class="enabled-assets__error-detail">{detail()}</p>
              <Button
                variant="ghost"
                size="sm"
                class="enabled-assets__retry"
                onClick={refetchAll}
              >
                RETRY
              </Button>
            </div>
          )}
        </Show>

        <Show when={!loading() && !errorDetail()}>
          <Show
            when={totalRows() > 0}
            fallback={
              <p class="enabled-assets__empty" data-testid="enabled-assets-empty">
                Nothing enabled in this workspace yet.
              </p>
            }
          >
            <div class="enabled-assets__groups">
              <For each={KINDS}>
                {(kind, i) => {
                  const [resource] = resources[i()]!;
                  const rows = () =>
                    resource.state === 'ready' ? rowsFor(kind.id, resource()) : [];
                  return (
                    <SectionGroup
                      kind={kind.id}
                      title={kind.title}
                      rows={rows()}
                      onToggle={handleToggle}
                    />
                  );
                }}
              </For>
            </div>
          </Show>
        </Show>
      </Show>
    </section>
  );
};

interface SectionGroupProps {
  kind: EnabledKind;
  title: string;
  rows: EnabledRow[];
  onToggle: (kind: EnabledKind, id: string, next: boolean) => void;
}

const SectionGroup: Component<SectionGroupProps> = (props) => (
  <Show when={props.rows.length > 0}>
    <section
      class="enabled-assets__group"
      data-kind={props.kind}
      data-testid={`enabled-assets-group-${props.kind}`}
    >
      <h3 class="enabled-assets__group-title">{props.title}</h3>
      <ul class="enabled-assets__rows">
        <For each={props.rows}>
          {(row) => (
            <ToggleRow
              row={row}
              enabled={isEnabled(row.kind, row.id)}
              onToggle={(next) => props.onToggle(row.kind, row.id, next)}
            />
          )}
        </For>
      </ul>
    </section>
  </Show>
);

interface ToggleRowProps {
  row: EnabledRow;
  enabled: boolean;
  onToggle: (next: boolean) => void;
}

const ToggleRow: Component<ToggleRowProps> = (props) => {
  const inputId = () => `enabled-toggle-${props.row.kind}-${props.row.id}`;
  return (
    <li
      class="enabled-row"
      data-kind={props.row.kind}
      data-id={props.row.id}
      data-testid={`enabled-row-${props.row.kind}-${props.row.id}`}
    >
      <span class="enabled-row__name">{props.row.name}</span>
      <span
        class="enabled-row__state"
        classList={{
          'enabled-row__state--on': props.enabled,
          'enabled-row__state--off': !props.enabled,
        }}
        aria-hidden="true"
      >
        {props.enabled ? 'on' : 'off'}
      </span>
      <label class="enabled-row__toggle" for={inputId()}>
        <input
          id={inputId()}
          class="enabled-row__input"
          type="checkbox"
          role="switch"
          aria-checked={props.enabled}
          aria-label={`${props.enabled ? 'Disable' : 'Enable'} ${props.row.name}`}
          checked={props.enabled}
          onChange={(e) => props.onToggle(e.currentTarget.checked)}
        />
        <span class="enabled-row__track" aria-hidden="true">
          <span class="enabled-row__thumb" />
        </span>
      </label>
    </li>
  );
};
