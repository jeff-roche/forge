// F-723: Dashboard surface for container lifecycle (v-dash relayout).
//
// Two pieces live in this file:
//
//   - `<ContainerRuntimeBanner>` — first-run banner shown when
//     `detect_container_runtime` reports the runtime is missing / broken /
//     rootless-unavailable. Dismissable via "Don't show again" which
//     persists `dashboard.container_banner_dismissed = true` in user-tier
//     settings (F-151) so the banner stays gone across launches.
//
//   - `<ContainersSection>` — the full-width row list of active Level-2
//     sandbox containers per `docs/ui-specs/containers-section.md`. Each
//     row carries a liveness dot, an identity stack, the image reference,
//     a `cpu · ram` readout, and a pair of icon buttons (`term`, `stop`).
//     The card header carries the aggregate `N running · X · podman` meta
//     plus `Prune` + `Logs` ghost actions; the Logs action toggles an
//     inline log strip beneath the row list.
//
// The list refreshes from the existing event channel: `stop_container`
// and `prune_containers` emit `containers:list_changed` app-wide and the
// section re-fetches on receipt.

import {
  type Component,
  createMemo,
  createResource,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
} from 'solid-js';
import { Button, IconButton, Skeleton } from '@forge/design';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import {
  CONTAINERS_CHANGED_EVENT,
  containerLogs,
  listActiveContainers,
  stopContainer,
  type ContainerInfo,
  type LogLine,
  type RuntimeStatus,
} from '../../ipc/containers';
import { pushToast } from '../toast';
import './ContainersSection.css';

/** Anchor for cross-section links. */
export const CONTAINERS_SECTION_ID = 'containers-section';

/** Runtime label rendered in the header meta. Hard-coded until F-???
 * (dynamic runtime probe surfacing) wires `detect_container_runtime`'s
 * `tool` field into the meta line. */
const RUNTIME_LABEL = 'podman';

// ---------------------------------------------------------------------------
// Containers section
// ---------------------------------------------------------------------------

export const ContainersSection: Component = () => {
  const [containers, { refetch }] = createResource<ContainerInfo[]>(async () => {
    try {
      return await listActiveContainers();
    } catch {
      return [];
    }
  });
  const [logsOpen, setLogsOpen] = createSignal(false);
  const [actionError, setActionError] = createSignal<string | null>(null);

  const runningCount = createMemo(
    () => (containers() ?? []).filter((c) => !c.stopped).length,
  );

  // Refresh on the existing event channel so the list stays in sync with
  // backend mutations (stop from another tab, session teardown).
  onMount(() => {
    let mounted = true;
    let unlisten: UnlistenFn | null = null;
    void (async () => {
      const fn = await listen<string>(CONTAINERS_CHANGED_EVENT, () => {
        void refetch();
      });
      if (mounted) unlisten = fn;
      else fn();
    })();
    onCleanup(() => {
      mounted = false;
      unlisten?.();
    });
  });

  const onStop = async (id: string) => {
    setActionError(null);
    try {
      await stopContainer(id);
      await refetch();
      // Per WCAG 3.3.4: stop is recoverable, so confirmation lives in a
      // post-action toast rather than a pre-action modal. Short-id keeps
      // the surface compact; the full id is still in the row's title.
      pushToast(
        'info',
        `Container ${id.slice(0, 12)} stopped — restart from session view`,
      );
    } catch (err: unknown) {
      setActionError(err instanceof Error ? err.message : String(err));
    }
  };

  // F-723: Prune is documented in the spec but no backend IPC ships in V1.
  // Surface the affordance disabled with an explicit tooltip until a future
  // task wires `prune_containers`.
  // TODO(F-???): wire prune_containers IPC and enable this button.
  const onPrune = () => {
    setActionError('prune_containers: not yet implemented');
  };

  // F-723: per-row terminal goes through the same session terminal pane the
  // spec describes. Until the dashboard→session bridge lands, the button
  // emits a custom DOM event so an outer host can attach a handler without
  // this component needing to know the bridge shape.
  // TODO(F-???): wire open_terminal_for_container IPC + session-pane bridge.
  const onTerminal = (id: string) => {
    setActionError(null);
    window.dispatchEvent(
      new CustomEvent('forge:container-open-terminal', { detail: { containerId: id } }),
    );
  };

  const isLoading = createMemo(() => containers.loading);
  const isEmpty = createMemo(() => !isLoading() && (containers() ?? []).length === 0);
  const isReady = createMemo(() => !isLoading() && (containers() ?? []).length > 0);
  const headDisabled = createMemo(() => !isReady());

  return (
    <section
      id={CONTAINERS_SECTION_ID}
      class="containers-section"
      aria-label="Active sandbox containers"
    >
      <header class="containers-section__header">
        <div class="containers-section__head-left">
          <span class="containers-section__label">CONTAINERS</span>
          <Show when={isReady() && runningCount() > 0}>
            <span class="containers-section__meta" data-testid="containers-meta">
              {runningCount()} running {'·'} unknown {'·'} {RUNTIME_LABEL}
            </span>
          </Show>
        </div>
        <div class="containers-section__head-right">
          <Button
            variant="ghost"
            size="sm"
            class="containers-section__head-btn"
            disabled={headDisabled()}
            data-testid="containers-prune-btn"
            onClick={onPrune}
          >
            Prune
          </Button>
          <Button
            variant="ghost"
            size="sm"
            class="containers-section__head-btn"
            disabled={headDisabled()}
            aria-pressed={logsOpen()}
            aria-controls="containers-section-logs"
            data-testid="containers-logs-btn"
            onClick={() => setLogsOpen((v) => !v)}
          >
            {logsOpen() ? 'Close logs' : 'Logs'}
          </Button>
        </div>
      </header>

      <Show when={isLoading()}>
        <Skeleton
          variant="block"
          count={3}
          label="Loading containers"
          class="containers-section__skeleton"
          data-testid="containers-loading"
        />
      </Show>

      <Show when={isEmpty()}>
        <p class="containers-section__empty" data-testid="containers-empty">
          No active sandbox containers. They appear here when a session uses Level-2 isolation.
        </p>
      </Show>

      <Show when={actionError()}>
        {(msg) => (
          <p
            class="containers-section__error"
            role="alert"
            data-testid="containers-error"
          >
            {msg()}
          </p>
        )}
      </Show>

      <Show when={isReady()}>
        <ul class="containers-section__list" role="list">
          <For each={containers() ?? []}>
            {(c) => (
              <ContainerRow
                container={c}
                onTerminal={() => onTerminal(c.container_id)}
                onStop={() => void onStop(c.container_id)}
              />
            )}
          </For>
        </ul>
      </Show>

      <Show when={logsOpen() && isReady()}>
        <InlineLogsPanel
          containers={containers() ?? []}
          onClose={() => setLogsOpen(false)}
        />
      </Show>
    </section>
  );
};

// ---------------------------------------------------------------------------
// Single row
// ---------------------------------------------------------------------------

interface ContainerRowProps {
  container: ContainerInfo;
  onTerminal: () => void;
  onStop: () => void;
}

const ContainerRow: Component<ContainerRowProps> = (props) => {
  const shortId = () => props.container.container_id.slice(0, 12);
  const startedAt = () => formatRelative(props.container.started_at);
  return (
    <li
      class="containers-section__row"
      classList={{ 'containers-section__row--stopped': props.container.stopped }}
      data-testid={`container-row-${props.container.container_id}`}
    >
      <span
        class="containers-section__live-dot"
        classList={{ 'containers-section__live-dot--warn': props.container.stopped }}
        aria-label={props.container.stopped ? 'stopped' : 'running'}
      />
      <div class="containers-section__identity">
        <div class="containers-section__name">{props.container.container_id.length > 0 ? deriveName(props.container) : ''}</div>
        <div
          class="containers-section__sub"
          title={props.container.container_id}
        >
          <span class="containers-section__sha">sha256:{shortId()}</span>
          <span class="containers-section__dot" aria-hidden="true">{'·'}</span>
          <span class="containers-section__context">{startedAt()}</span>
        </div>
      </div>
      <div class="containers-section__image">{props.container.image}</div>
      <div class="containers-section__resources">cpu unknown {'·'} ram unknown</div>
      <div class="containers-section__row-actions">
        <IconButton
          class="containers-section__icon-btn"
          data-testid={`container-term-${props.container.container_id}`}
          label="Open terminal"
          icon={<TerminalGlyph />}
          onClick={props.onTerminal}
        />
        <IconButton
          class="containers-section__icon-btn"
          disabled={props.container.stopped}
          data-testid={`container-stop-${props.container.container_id}`}
          label="Stop container"
          icon={<StopGlyph />}
          onClick={props.onStop}
        />
      </div>
    </li>
  );
};

// The registry exposes the container id but no separate display name.
// Reuse the id's short form for the row name and surface the full id in
// the second-row sha context — keeps every row uniquely identifiable
// without a registry schema bump.
function deriveName(c: ContainerInfo): string {
  return c.container_id.length > 12 ? c.container_id.slice(0, 12) : c.container_id;
}

const TerminalGlyph: Component = () => (
  <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
    <path
      d="M2 3l2 2-2 2M5 7h4"
      stroke="currentColor"
      stroke-width="1.2"
      stroke-linecap="round"
      stroke-linejoin="round"
      fill="none"
    />
  </svg>
);

const StopGlyph: Component = () => (
  <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
    <rect x="3" y="3" width="6" height="6" rx="1" fill="currentColor" />
  </svg>
);

// ---------------------------------------------------------------------------
// Inline logs panel
// ---------------------------------------------------------------------------

interface InlineLogsPanelProps {
  containers: ContainerInfo[];
  onClose: () => void;
}

/** Maximum lines kept in the buffer regardless of how many polls happen. */
const LOGS_BUFFER_CAP = 1000;
/** Polling interval (ms). Matches the F-597 spec. */
const LOGS_POLL_MS = 2000;
/** Tail size on each poll — keeps the IPC payload bounded. */
const LOGS_TAIL = 200;

interface DecoratedLine extends LogLine {
  containerId: string;
}

/**
 * F-723: inline log strip replacing the prior modal flyout. Renders below
 * the row list inside the card chrome and multiplexes log lines across
 * every running container, each line prefixed by the originating
 * container's 12-char id.
 */
const InlineLogsPanel: Component<InlineLogsPanelProps> = (props) => {
  const [lines, setLines] = createSignal<DecoratedLine[]>([]);
  const [error, setError] = createSignal<string | null>(null);

  // Per-container "since" cursors so each next poll only fetches new
  // lines per container. First poll uses `tail` to seed the viewer.
  const cursors: Map<string, string | null> = new Map();

  const targetIds = () =>
    props.containers.filter((c) => !c.stopped).map((c) => c.container_id);

  const fetchOnce = async (initial: boolean) => {
    try {
      for (const id of targetIds()) {
        const opts: { since?: string; tail?: number } = {};
        const cursor = cursors.get(id) ?? null;
        if (cursor !== null) opts.since = cursor;
        if (initial) opts.tail = LOGS_TAIL;
        const got = await containerLogs(id, opts);
        if (got.length === 0) continue;
        const newest = got[got.length - 1];
        if (newest?.timestamp) cursors.set(id, newest.timestamp);
        setLines((cur) => {
          const seen = new Set<string>();
          const tailStart = Math.max(0, cur.length - got.length);
          for (let i = tailStart; i < cur.length; i++) {
            const l = cur[i]!;
            if (l.containerId !== id) continue;
            seen.add(`${l.timestamp ?? ''} ${l.stream} ${l.line}`);
          }
          const fresh: DecoratedLine[] = got
            .filter((l) => {
              const key = `${l.timestamp ?? ''} ${l.stream} ${l.line}`;
              if (seen.has(key)) return false;
              seen.add(key);
              return true;
            })
            .map((l) => ({ ...l, containerId: id }));
          const merged = cur.concat(fresh);
          return merged.length > LOGS_BUFFER_CAP
            ? merged.slice(merged.length - LOGS_BUFFER_CAP)
            : merged;
        });
      }
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  onMount(() => {
    void fetchOnce(true);
    const id = window.setInterval(() => {
      void fetchOnce(false);
    }, LOGS_POLL_MS);
    onCleanup(() => window.clearInterval(id));

    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && document.activeElement?.closest('.containers-section__logs-panel')) {
        e.preventDefault();
        props.onClose();
      }
    };
    window.addEventListener('keydown', onKey);
    onCleanup(() => window.removeEventListener('keydown', onKey));
  });

  return (
    <div
      id="containers-section-logs"
      class="containers-section__logs-panel"
      data-testid="containers-logs-panel"
    >
      <header class="containers-section__logs-head">
        <span class="containers-section__logs-title">
          LOGS {'—'} {RUNTIME_LABEL}
        </span>
        <Button
          variant="ghost"
          size="sm"
          class="containers-section__head-btn"
          onClick={props.onClose}
          data-testid="containers-logs-close"
          aria-label="Close logs"
        >
          Close
        </Button>
      </header>
      <Show when={error()}>
        {(msg) => (
          <p
            class="containers-section__error"
            role="alert"
            data-testid="containers-logs-error"
          >
            {msg()}
          </p>
        )}
      </Show>
      <pre
        class="containers-section__log-pane"
        role="log"
        aria-live="polite"
        aria-label="Container logs"
      >
        <For each={lines()}>
          {(l) => (
            <div
              class="containers-section__log-line"
              classList={{
                'containers-section__log-line--err': l.stream === 'stderr',
              }}
            >
              <span class="containers-section__log-cid">{l.containerId.slice(0, 12)}</span>
              <Show when={l.timestamp}>
                {(ts) => (
                  <span class="containers-section__log-ts">{ts()}</span>
                )}
              </Show>
              <span class="containers-section__log-text">{l.line}</span>
            </div>
          )}
        </For>
      </pre>
    </div>
  );
};

// ---------------------------------------------------------------------------
// First-run runtime banner
// ---------------------------------------------------------------------------

interface ContainerRuntimeBannerProps {
  status: RuntimeStatus;
  onDismiss: () => void;
}

/**
 * Banner shown at the top of the Dashboard when
 * `detect_container_runtime` reports the runtime is unusable. The
 * dismissable "Don't show again" button persists
 * `dashboard.container_banner_dismissed = true` so the banner stays
 * gone across launches.
 *
 * `role="alert"` — the headline copy reports the runtime as
 * "broken / unavailable" which is semantically an assertive announcement
 * for screen-reader users, even though sessions still fall back to
 * Level-1 isolation per F-596.
 */
export const ContainerRuntimeBanner: Component<ContainerRuntimeBannerProps> = (props) => {
  const headline = () => bannerHeadline(props.status);
  const detail = () => bannerDetail(props.status);
  const installLink = () => installInstructionsUrl(props.status);

  return (
    <div
      class="containers-banner"
      role="alert"
      data-testid="container-runtime-banner"
      aria-label={headline()}
    >
      <span class="containers-banner__icon" aria-hidden="true">
        {'⚠'}
      </span>
      <div class="containers-banner__body">
        <p class="containers-banner__headline">{headline()}</p>
        <p class="containers-banner__detail">
          {detail()}{' '}
          <a
            class="containers-banner__link"
            href={installLink()}
            target="_blank"
            rel="noreferrer noopener"
          >
            See install instructions
          </a>
          .
        </p>
      </div>
      <Button
        variant="ghost"
        size="sm"
        class="containers-banner__dismiss"
        data-testid="container-runtime-banner-dismiss"
        aria-label="Don't show this banner again"
        onClick={props.onDismiss}
      >
        DON'T SHOW AGAIN
      </Button>
    </div>
  );
};

export function bannerHeadline(status: RuntimeStatus): string {
  switch (status.kind) {
    case 'available':
      return 'Container runtime ready';
    case 'missing':
      return `Container runtime not installed (${status.tool})`;
    case 'broken':
      return `Container runtime broken (${status.tool})`;
    case 'rootless_unavailable':
      return `Rootless mode unavailable (${status.tool})`;
    case 'unknown':
      return 'Container runtime probe failed';
  }
}

export function bannerDetail(status: RuntimeStatus): string {
  switch (status.kind) {
    case 'available':
      return '';
    case 'missing':
      return `Sessions fall back to Level-1 isolation (cgroup + seccomp).`;
    case 'broken':
      return `Sessions fall back to Level-1; underlying error: ${truncate(
        status.reason,
        160,
      )}.`;
    case 'rootless_unavailable':
      return `Probe reported rootless mode disabled: ${truncate(
        status.reason,
        160,
      )}.`;
    case 'unknown':
      return `Probe failed with an unexpected error: ${truncate(status.reason, 160)}.`;
  }
}

/**
 * Platform-specific install hint URL. We link out instead of embedding
 * platform docs so the banner stays small and the upstream pages stay
 * authoritative.
 */
export function installInstructionsUrl(_status: RuntimeStatus): string {
  return 'https://podman.io/docs/installation';
}

function truncate(s: string, max: number): string {
  return s.length > max ? `${s.slice(0, max - 1)}${'…'}` : s;
}

function formatRelative(rfc3339: string): string {
  const t = Date.parse(rfc3339);
  if (Number.isNaN(t)) return rfc3339;
  const now = Date.now();
  const ms = Math.max(0, now - t);
  if (ms < 60_000) return `${Math.floor(ms / 1000)}s ago`;
  if (ms < 3_600_000) return `${Math.floor(ms / 60_000)}m ago`;
  if (ms < 86_400_000) return `${Math.floor(ms / 3_600_000)}h ago`;
  return new Date(t).toISOString().slice(0, 10);
}
