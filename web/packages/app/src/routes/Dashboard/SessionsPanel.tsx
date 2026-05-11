import { createResource, createSignal, For, Show, type Component } from 'solid-js';
import { useNavigate } from '@solidjs/router';
import {
  sessionList,
  openSession as ipcOpenSession,
  type SessionWireState,
  type SessionSummary,
} from '../../ipc/dashboard';
import { Button, Skeleton, StatusPill, type StatusPillVariant } from '@forge/design';
import { useRovingTabindex } from '../../lib/useRovingTabindex';
import './SessionsPanel.css';

export type { SessionWireState, SessionSummary };

type Tab = 'active' | 'archived';

/**
 * F-720: dashboard Sessions card — verbatim copy + per-state pill colors
 * per `docs/ui-specs/dashboard.md §Sessions card`. Row layout, not a card
 * grid; chip filter (active / archived); ghost `View all` action.
 *
 * F-401 four-state coverage (loading / error / empty / ready) is preserved.
 */
async function fetchSessions(): Promise<SessionSummary[]> {
  const result = await sessionList();
  return Array.isArray(result) ? result : [];
}

function partition(sessions: SessionSummary[]): Record<Tab, SessionSummary[]> {
  return {
    active: sessions.filter((s) => s.state !== 'archived'),
    archived: sessions.filter((s) => s.state === 'archived'),
  };
}

/**
 * Map the IPC wire state to the F-720 four-variant pill model. `active`
 * sessions stream; `stopped` sessions sit idle (UDS not responding);
 * `archived` sessions are done. The `awaiting` variant is reserved for the
 * sub-agent approval pipeline (F-740); no wire state maps to it today.
 */
export function pillForState(state: SessionWireState): {
  variant: StatusPillVariant;
  label: string;
} {
  switch (state) {
    case 'active':
      return { variant: 'streaming', label: 'streaming' };
    case 'stopped':
      return { variant: 'idle', label: 'idle' };
    case 'archived':
      return { variant: 'done', label: 'done' };
  }
}

/**
 * Glyph keyed to the session lifecycle per spec §Row layout. Static
 * single-character marks — F-740 may swap to icon components, but the
 * verbatim mock uses these glyphs.
 */
function glyphForState(state: SessionWireState): string {
  switch (state) {
    case 'active':
      return '⚒';
    case 'stopped':
      return '▣';
    case 'archived':
      return '✓';
  }
}

function shortId(id: string): string {
  return id.slice(0, 4);
}

function count(n: number): string {
  return n.toString().padStart(2, '0');
}

export const SessionsPanel: Component = () => {
  const navigate = useNavigate();
  const [sessions, { refetch }] = createResource(fetchSessions);
  const [tab, setTab] = createSignal<Tab>('active');
  const [openError, setOpenError] = createSignal<string | null>(null);

  // Reading `sessions()` while the resource is in its `errored` state
  // re-throws inside the reactive scope — gate every read on `state`.
  const rows = () => (sessions.state === 'ready' ? sessions() ?? [] : []);
  const groups = () => partition(rows());
  const current = () => groups()[tab()];

  const runningCount = () => rows().filter((s) => s.state === 'active').length;
  const idleCount = () => rows().filter((s) => s.state === 'stopped').length;

  const listErrorDetail = () => {
    const err = sessions.error;
    if (!err) return null;
    return err instanceof Error ? `Error: ${err.message}` : String(err);
  };

  const handleOpen = (id: string) => {
    setOpenError(null);
    ipcOpenSession(id).catch((err) => {
      const detail = err instanceof Error ? err.message : String(err);
      setOpenError(`open_session failed: ${detail}`);
    });
  };

  // Roving tabindex on the row list (F-416 pattern). One row is the tab
  // stop; arrows / Home / End move focus within the list.
  const [listRef, setListRef] = createSignal<HTMLDivElement | undefined>();
  useRovingTabindex(listRef, '.sessions__row');

  const panelId = () => `sessions-panel-${tab()}`;
  const tabId = (t: Tab) => `sessions-tab-${t}`;

  return (
    <section class="sessions" aria-label="Sessions">
      <header class="sessions__header">
        <span class="sessions__label">Sessions</span>
        <div class="sessions__header-actions">
          <Button
            variant="ghost"
            size="sm"
            class="sessions__view-all"
            onClick={() => navigate('/sessions')}
          >
            View all
          </Button>
        </div>
      </header>

      <div class="sessions__sub-header">
        <div role="tablist" class="sessions__chips" aria-label="Session filter">
          <ChipTab
            tab="active"
            current={tab()}
            onSelect={setTab}
            count={groups().active.length}
          />
          <ChipTab
            tab="archived"
            current={tab()}
            onSelect={setTab}
            count={groups().archived.length}
          />
        </div>
        <Show when={runningCount() + idleCount() > 0}>
          <span class="sessions__meta">
            {runningCount()} running · {idleCount()} idle
          </span>
        </Show>
      </div>

      <Show when={openError()}>
        {(msg) => (
          <p class="sessions__error" role="alert">
            {msg()}
          </p>
        )}
      </Show>

      <Show when={sessions.loading}>
        <Skeleton
          variant="card"
          count={4}
          label="Loading sessions"
          class="sessions__skeleton"
          data-testid="sessions-loading"
        />
      </Show>

      <Show when={listErrorDetail()}>
        {(detail) => (
          <div
            class="sessions__list-error"
            id={panelId()}
            role="tabpanel"
            aria-labelledby={tabId(tab())}
          >
            <div class="sessions__list-error-body" role="alert">
              <p class="sessions__list-error-title">Couldn't load sessions.</p>
              <p class="sessions__list-error-detail">{detail()}</p>
              <Button
                variant="ghost"
                size="sm"
                class="sessions__retry"
                onClick={() => void refetch()}
              >
                RETRY
              </Button>
            </div>
          </div>
        )}
      </Show>

      <Show when={sessions.state === 'ready'}>
        <Show
          when={current().length > 0}
          fallback={
            <p
              class="sessions__empty"
              id={panelId()}
              role="tabpanel"
              aria-labelledby={tabId(tab())}
            >
              {tab() === 'active' ? 'No active sessions yet.' : 'Archive is empty.'}
            </p>
          }
        >
          <div
            ref={setListRef}
            class="sessions__list"
            id={panelId()}
            role="tabpanel"
            aria-labelledby={tabId(tab())}
          >
            <For each={current()}>
              {(session) => <SessionRow session={session} onOpen={handleOpen} />}
            </For>
          </div>
        </Show>
      </Show>
    </section>
  );
};

interface ChipTabProps {
  tab: Tab;
  current: Tab;
  count: number;
  onSelect: (t: Tab) => void;
}

const ChipTab: Component<ChipTabProps> = (props) => {
  const selected = () => props.tab === props.current;
  return (
    <button
      type="button"
      role="tab"
      id={`sessions-tab-${props.tab}`}
      aria-controls={`sessions-panel-${props.tab}`}
      aria-selected={selected()}
      class="sessions__chip"
      classList={{ 'sessions__chip--selected': selected() }}
      onClick={() => props.onSelect(props.tab)}
    >
      <span class="sessions__chip-label">{props.tab}</span>
      <span class="sessions__chip-sep">·</span>
      <span class="sessions__chip-count">{count(props.count)}</span>
    </button>
  );
};

interface SessionRowProps {
  session: SessionSummary;
  onOpen: (id: string) => void;
}

const SessionRow: Component<SessionRowProps> = (props) => {
  const pill = () => pillForState(props.session.state);
  const glyph = () => glyphForState(props.session.state);
  return (
    <button
      type="button"
      class="sessions__row"
      onClick={() => props.onOpen(props.session.id)}
      aria-label={`Open session ${props.session.subject}`}
    >
      <span class={`sessions__icon sessions__icon--${props.session.state}`} aria-hidden="true">
        {glyph()}
      </span>
      <div class="sessions__identity">
        <div class="sessions__identity-line">
          <span class="sessions__name">{props.session.subject}</span>
          <span class="sessions__id">#{shortId(props.session.id)}</span>
        </div>
        <div class="sessions__identity-meta">
          <Show when={props.session.provider}>
            {(p) => <span class="sessions__provider">{p()}</span>}
          </Show>
          <span class="sessions__last">{formatRelative(props.session.lastEventAt)}</span>
        </div>
      </div>
      <StatusPill variant={pill().variant} class="sessions__pill">
        {pill().label}
      </StatusPill>
    </button>
  );
};

/** Cheap relative-time formatter — no external date lib. */
export function formatRelative(iso: string, now: Date = new Date()): string {
  const then = new Date(iso);
  if (Number.isNaN(then.getTime())) return '—';
  const seconds = Math.floor((now.getTime() - then.getTime()) / 1000);
  if (seconds < 60) return 'just now';
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d ago`;
  return then.toISOString().slice(0, 10);
}
