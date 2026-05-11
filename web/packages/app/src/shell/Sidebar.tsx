// F-718: primary-nav sidebar.
//
// 240px fixed nav rail that mounts on every route, per
// `docs/ui-specs/app-shell.md` §Sidebar. Distinct from `FilesSidebar.tsx`
// (workspace tree, session-scoped). Renders the fixed ten-row nav with
// group labels, an active-row highlight, and per-row count badges that
// hydrate from existing IPC where available.
//
// Count-source convention. Each entry's `countSource` returns:
//   - a non-negative number → render the badge
//   - `0`                   → suppress the badge (spec: never render 0)
//   - `undefined`           → loading or no source known; suppress the badge
//
// Where a count source has no IPC backing yet (Git changed files, Recent
// fallback, Usage, Settings), `countSource` stays undefined so the row
// renders without a badge. Future tickets wire the missing sources.
//
// Active-row highlight uses solid-router's `useMatch` per entry. Group
// labels are static; row order is fixed by spec and may not be re-sorted
// by data.

import {
  type Component,
  type JSX,
  For,
  Show,
  createMemo,
  createResource,
  onCleanup,
  onMount,
} from 'solid-js';
import { useMatch } from '@solidjs/router';
import { listProviders, sessionList } from '../ipc/dashboard';
import { listAgents, listMcpServers, listSkills } from '../ipc/catalog';
import {
  CONTAINERS_CHANGED_EVENT,
  listActiveContainers,
} from '../ipc/containers';
import { activeWorkspaceRoot } from '../stores/session';
import { listen } from '@tauri-apps/api/event';
import './Sidebar.css';

// ---------------------------------------------------------------------------
// Nav contract
// ---------------------------------------------------------------------------

type GroupId = 'workspace' | 'ai' | 'system';

export interface NavEntry {
  id: string;
  group: GroupId;
  label: string;
  route: string;
  /** When set, the route is treated as a placeholder until a future ticket
   *  wires it. The row still renders + remains clickable, but tests may
   *  assert the `data-placeholder` attribute. */
  placeholderTicket?: string;
  icon: () => JSX.Element;
}

const GROUP_LABEL: Record<GroupId, string> = {
  workspace: 'WORKSPACE',
  ai: 'AI',
  system: 'SYSTEM',
};

const GROUP_ORDER: GroupId[] = ['workspace', 'ai', 'system'];

// Tiny inline SVGs at 16x16 viewBox 24, matching the 1.7px stroke convention
// used by ActivityBar. Reused so a future shared icon set can swap in
// without touching the nav.
const Icon = (path: string): (() => JSX.Element) => () => (
  <svg
    class="sidebar__icon"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="1.7"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
  >
    <path d={path} />
  </svg>
);

const NAV: NavEntry[] = [
  {
    id: 'sessions',
    group: 'workspace',
    label: 'Sessions',
    route: '/',
    icon: Icon('M4 5h16v4H4zM4 11h16v4H4zM4 17h16v2H4z'),
  },
  {
    id: 'recent',
    group: 'workspace',
    label: 'Recent',
    route: '/recent',
    placeholderTicket: 'F-XXX',
    icon: Icon('M12 8v5l3 2M21 12a9 9 0 1 1-9-9'),
  },
  {
    id: 'git',
    group: 'workspace',
    label: 'Git',
    route: '/git',
    placeholderTicket: 'F-XXX',
    icon: Icon('M5 3v10a4 4 0 0 0 4 4h6M5 7a2 2 0 1 0 0-4 2 2 0 0 0 0 4zm14 14a2 2 0 1 0 0-4 2 2 0 0 0 0 4z'),
  },
  {
    id: 'providers',
    group: 'ai',
    label: 'Providers',
    route: '/providers',
    placeholderTicket: 'F-729',
    icon: Icon('M12 2 4 6v6c0 5 3.5 9 8 10 4.5-1 8-5 8-10V6z'),
  },
  {
    id: 'skills',
    group: 'ai',
    label: 'Skills',
    route: '/catalog/skills',
    placeholderTicket: 'F-XXX',
    icon: Icon('m12 3 2.7 5.5 6.1.9-4.4 4.3 1 6.1L12 17l-5.4 2.8 1-6.1L3.2 9.4l6.1-.9z'),
  },
  {
    id: 'mcp',
    group: 'ai',
    label: 'MCP servers',
    route: '/catalog/mcp',
    placeholderTicket: 'F-XXX',
    icon: Icon('M4 7h16M4 12h16M4 17h10'),
  },
  {
    id: 'agents',
    group: 'ai',
    label: 'Agents',
    route: '/catalog/agents',
    placeholderTicket: 'F-XXX',
    icon: Icon('M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM4 21a8 8 0 0 1 16 0'),
  },
  {
    id: 'containers',
    group: 'system',
    label: 'Containers',
    route: '/containers',
    placeholderTicket: 'F-XXX',
    icon: Icon('M3 8h18v10H3zM3 8l3-4h12l3 4M8 12h2M14 12h2'),
  },
  {
    id: 'usage',
    group: 'system',
    label: 'Usage',
    route: '/usage',
    icon: Icon('M4 19V5M9 19V9M14 19v-6M19 19V3'),
  },
  {
    id: 'settings',
    group: 'system',
    label: 'Settings',
    route: '/settings',
    placeholderTicket: 'F-XXX',
    icon: Icon('M12 9a3 3 0 1 0 0 6 3 3 0 0 0 0-6zM19 12l2 1-1 3-2-.5-1.5 1.5.5 2-3 1-1-2h-2l-1 2-3-1 .5-2L6.5 16.5 4.5 17 3.5 14l2-1v-2l-2-1L4.5 7l2 .5 1.5-1.5-.5-2 3-1 1 2h2l1-2 3 1-.5 2L18 7.5l2-.5 1 3-2 1z'),
  },
];

// ---------------------------------------------------------------------------
// Count sources
// ---------------------------------------------------------------------------

type CountResult = number | undefined;
type CountSources = Record<string, () => CountResult>;

/** Public injection seam for tests. Production never threads a custom
 *  `sources` prop — the default sources hydrate from real IPC. */
export interface SidebarProps {
  /** Optional override for the per-row count map. Keys are NAV ids. */
  sources?: CountSources;
}

const RECENT_WINDOW_MS = 7 * 24 * 60 * 60 * 1000;

function buildDefaultSources(): {
  sources: CountSources;
  cleanup: () => void;
} {
  // Sessions list — drives both "Sessions" (active count) and "Recent"
  // (last-7-day count). Re-fetched once on mount; downstream tickets can
  // wire `session:list_changed` once that event exists.
  const [sessions] = createResource(async () => {
    try {
      return await sessionList();
    } catch {
      return undefined;
    }
  });

  const [providers] = createResource(async () => {
    try {
      return await listProviders();
    } catch {
      return undefined;
    }
  });

  // Catalog rows depend on a workspace root. When none is active the
  // resource resolves to `undefined` and the badge stays hidden.
  const catalogFetcher = <T,>(
    fn: (ws: string) => Promise<readonly T[]>,
  ) =>
    createResource(activeWorkspaceRoot, async (ws): Promise<CountResult> => {
      if (!ws) return undefined;
      try {
        const rows = await fn(ws);
        return rows.length;
      } catch {
        return undefined;
      }
    })[0];

  const skills = catalogFetcher(listSkills);
  const mcps = catalogFetcher(listMcpServers);
  const agents = catalogFetcher(listAgents);

  // Containers — list_active_containers is gated to the dashboard window
  // label. On non-dashboard windows the call rejects and the badge stays
  // hidden, which matches the spec's per-badge fail-silent rule.
  const [containers, { refetch: refetchContainers }] = createResource(
    async () => {
      try {
        const rows = await listActiveContainers();
        return rows.filter((r) => !r.stopped).length;
      } catch {
        return undefined;
      }
    },
  );

  let unlistenContainers: (() => void) | null = null;
  void (async () => {
    try {
      unlistenContainers = await listen(CONTAINERS_CHANGED_EVENT, () => {
        void refetchContainers();
      });
    } catch {
      unlistenContainers = null;
    }
  })();

  const recentCount = (): CountResult => {
    const rows = sessions();
    if (!rows) return undefined;
    const cutoff = Date.now() - RECENT_WINDOW_MS;
    return rows.filter((r) => {
      const t = Date.parse(r.lastEventAt);
      return Number.isFinite(t) && t >= cutoff;
    }).length;
  };

  const activeSessionCount = (): CountResult => {
    const rows = sessions();
    if (!rows) return undefined;
    return rows.filter((r) => r.state === 'active').length;
  };

  return {
    sources: {
      sessions: activeSessionCount,
      recent: recentCount,
      // Git: no IPC source exists for changed-file count today.
      providers: () => providers()?.length,
      skills: () => skills(),
      mcp: () => mcps(),
      agents: () => agents(),
      containers: () => containers(),
      // Usage / Settings intentionally omit a badge per spec.
    },
    cleanup: () => {
      if (unlistenContainers) unlistenContainers();
    },
  };
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export const Sidebar: Component<SidebarProps> = (props) => {
  const built = buildDefaultSources();
  onCleanup(built.cleanup);

  const sources = (): CountSources => props.sources ?? built.sources;

  // Group → ordered nav rows. The map preserves the source order of NAV so
  // the spec's row sequence is the rendered sequence.
  const grouped = createMemo<Record<GroupId, NavEntry[]>>(() => {
    const acc: Record<GroupId, NavEntry[]> = {
      workspace: [],
      ai: [],
      system: [],
    };
    for (const entry of NAV) acc[entry.group].push(entry);
    return acc;
  });

  return (
    <nav
      class="sidebar"
      aria-label="Primary navigation"
      data-testid="sidebar"
    >
      <header class="sidebar__brand" data-testid="sidebar-brand">
        <span class="sidebar__brand-mark" aria-hidden="true">{'▲'}</span>
        <span class="sidebar__brand-name">Forge</span>
        <span class="sidebar__brand-tag">any ai. one editor.</span>
      </header>
      <For each={GROUP_ORDER}>
        {(group) => (
          <section class="sidebar__group" data-testid={`sidebar-group-${group}`}>
            <h2 class="sidebar__group-label">{GROUP_LABEL[group]}</h2>
            <ul class="sidebar__list">
              <For each={grouped()[group]}>
                {(entry) => <SidebarRow entry={entry} count={sources()[entry.id]} />}
              </For>
            </ul>
          </section>
        )}
      </For>
    </nav>
  );
};

interface SidebarRowProps {
  entry: NavEntry;
  count: (() => CountResult) | undefined;
}

const SidebarRow: Component<SidebarRowProps> = (props) => {
  // Match the entry's route; child routes share the prefix so e.g. the
  // `/catalog/skills` row stays active on deeper nav. The bare `/` row is
  // intentionally exact-only — a prefix match would steal the highlight
  // from every other route.
  const exact = useMatch(() => props.entry.route);
  const prefix = useMatch(() =>
    props.entry.route === '/' ? props.entry.route : `${props.entry.route}/*`,
  );
  const isActive = (): boolean => exact() !== undefined || prefix() !== undefined;
  const count = (): CountResult => props.count?.();
  const showBadge = (): boolean => {
    const n = count();
    return typeof n === 'number' && n > 0;
  };

  return (
    <li>
      <a
        href={props.entry.route}
        class={`sidebar__entry${isActive() ? ' sidebar__entry--active' : ''}`}
        aria-current={isActive() ? 'page' : undefined}
        data-testid={`sidebar-entry-${props.entry.id}`}
        data-placeholder={props.entry.placeholderTicket ?? undefined}
      >
        {props.entry.icon()}
        <span class="sidebar__label">{props.entry.label}</span>
        <Show when={showBadge()}>
          <span
            class="sidebar__badge"
            data-testid={`sidebar-badge-${props.entry.id}`}
          >
            {count()}
          </span>
        </Show>
      </a>
    </li>
  );
};
