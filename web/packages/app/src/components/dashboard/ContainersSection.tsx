// F-597: Dashboard surface for container lifecycle.
//
// Two pieces live in this file:
//
//   - `<ContainerRuntimeBanner>` — first-run banner shown when
//     `detect_container_runtime` reports the runtime is missing / broken /
//     rootless-unavailable. Dismissable via "Don't show again" which
//     persists `dashboard.container_banner_dismissed = true` in user-tier
//     settings (F-151) so the banner stays gone across launches.
//
//   - `<ContainersSection>` — the scrollable list of active Level-2
//     sandbox containers. Each row carries Stop / Remove buttons and a
//     "Logs" toggle that opens a flyout reading from
//     `forge-oci`'s `podman logs --timestamps` stream (polled every 2s,
//     bounded by MAX_LOG_TAIL on the backend so giant transcripts don't
//     freeze the UI).
//
// The list refreshes from the existing event channel: `stop_container`
// and `remove_container` emit `containers:list_changed` app-wide and the
// section re-fetches on receipt. Sessions also update the registry on
// Level-2 create / teardown — those edges are wired by forge-session
// when the session writes through `ContainerRegistryState::register`.

import {
  type Component,
  createResource,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
} from 'solid-js';
import { Button, Skeleton } from '@forge/design';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import {
  CONTAINERS_CHANGED_EVENT,
  containerLogs,
  detectContainerRuntime,
  listActiveContainers,
  removeContainer,
  stopContainer,
  type ContainerInfo,
  type LogLine,
  type RuntimeStatus,
} from '../../ipc/containers';
import { useFocusTrap } from '../../lib/useFocusTrap';
import { pushToast } from '../toast';
import './ContainersSection.css';

/** Anchor for cross-section links. */
export const CONTAINERS_SECTION_ID = 'containers-section';

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
  const [activeLogsId, setActiveLogsId] = createSignal<string | null>(null);
  const [actionError, setActionError] = createSignal<string | null>(null);
  // Pending REMOVE confirmation. `null` = no modal open. The id is the
  // full container_id of the row whose REMOVE button was clicked.
  const [pendingRemoveId, setPendingRemoveId] = createSignal<string | null>(null);
  const [removePending, setRemovePending] = createSignal(false);

  // Refresh on the existing event channel so the list stays in sync with
  // backend mutations (stop/remove from another tab, session teardown).
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

  // REMOVE is destructive (the container and any state inside it are gone)
  // so we gate it behind an explicit confirm step per WCAG 3.3.4. The
  // modal mirrors the credentials rotation confirm so the dashboard's
  // destructive surfaces share one visual contract.
  const requestRemove = (id: string) => {
    setActionError(null);
    setPendingRemoveId(id);
  };

  const cancelRemove = () => {
    if (removePending()) return;
    setPendingRemoveId(null);
  };

  const confirmRemove = async () => {
    const id = pendingRemoveId();
    if (id === null) return;
    setRemovePending(true);
    try {
      await removeContainer(id);
      // Close the logs flyout if we just removed the container it was
      // tracking — the container_id is gone and any logs poll would 404.
      if (activeLogsId() === id) setActiveLogsId(null);
      await refetch();
      setPendingRemoveId(null);
    } catch (err: unknown) {
      setActionError(err instanceof Error ? err.message : String(err));
      setPendingRemoveId(null);
    } finally {
      setRemovePending(false);
    }
  };

  return (
    <section
      id={CONTAINERS_SECTION_ID}
      class="containers-section"
      aria-label="Active sandbox containers"
    >
      <header class="containers-section__header">
        <span class="containers-section__label">CONTAINERS</span>
      </header>

      <Show when={containers.loading}>
        <Skeleton
          variant="block"
          count={3}
          label="Loading containers"
          class="containers-section__skeleton"
          data-testid="containers-loading"
        />
      </Show>

      <Show when={!containers.loading && (containers() ?? []).length === 0}>
        <p class="containers-section__empty" data-testid="containers-empty">
          // no active containers
        </p>
        <p
          class="containers-section__empty-hint"
          data-testid="containers-empty-hint"
        >
          Level-2 isolation populates this list.
        </p>
      </Show>

      <Show when={actionError()}>
        {(msg) => (
          <p class="containers-section__error" role="alert">
            {msg()}
          </p>
        )}
      </Show>

      <ul class="containers-section__list" role="list">
        <For each={containers() ?? []}>
          {(c) => (
            <ContainerRow
              container={c}
              logsOpen={activeLogsId() === c.container_id}
              onToggleLogs={() =>
                setActiveLogsId((cur) => (cur === c.container_id ? null : c.container_id))
              }
              onStop={() => void onStop(c.container_id)}
              onRemove={() => requestRemove(c.container_id)}
            />
          )}
        </For>
      </ul>

      <Show when={activeLogsId()}>
        {(id) => (
          <LogsFlyout
            containerId={id()}
            onClose={() => setActiveLogsId(null)}
          />
        )}
      </Show>

      <Show when={pendingRemoveId()}>
        {(id) => (
          <RemoveConfirm
            containerId={id()}
            pending={removePending()}
            onCancel={cancelRemove}
            onConfirm={() => void confirmRemove()}
          />
        )}
      </Show>
    </section>
  );
};

// ---------------------------------------------------------------------------
// Single row
// ---------------------------------------------------------------------------

interface ContainerRowProps {
  container: ContainerInfo;
  logsOpen: boolean;
  onToggleLogs: () => void;
  onStop: () => void;
  onRemove: () => void;
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
      <div class="containers-section__row-head">
        <span class="containers-section__id" title={props.container.container_id}>
          {shortId()}
        </span>
        <span class="containers-section__image">{props.container.image}</span>
        <Show when={props.container.stopped}>
          <span class="containers-section__pip" aria-label="stopped">
            stopped
          </span>
        </Show>
      </div>
      <div class="containers-section__row-meta">
        <span class="containers-section__session">
          session: {props.container.session_id.slice(0, 8)}
        </span>
        <span class="containers-section__when">{startedAt()}</span>
      </div>
      <div class="containers-section__row-actions">
        <Button
          variant="ghost"
          size="sm"
          aria-pressed={props.logsOpen}
          aria-controls={`container-logs-${props.container.container_id}`}
          data-testid={`container-logs-btn-${props.container.container_id}`}
          onClick={props.onToggleLogs}
        >
          {props.logsOpen ? 'CLOSE LOGS' : 'LOGS'}
        </Button>
        <Button
          variant="ghost"
          size="sm"
          disabled={props.container.stopped}
          data-testid={`container-stop-${props.container.container_id}`}
          aria-label={`Stop container ${props.container.container_id}`}
          onClick={props.onStop}
        >
          STOP
        </Button>
        <Button
          variant="primary"
          size="sm"
          data-testid={`container-remove-${props.container.container_id}`}
          aria-label={`Remove container ${props.container.container_id}`}
          onClick={props.onRemove}
        >
          REMOVE
        </Button>
      </div>
    </li>
  );
};

// ---------------------------------------------------------------------------
// Remove confirmation modal
// ---------------------------------------------------------------------------

interface RemoveConfirmProps {
  containerId: string;
  pending: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}

/**
 * Destructive-action confirmation for REMOVE. Mirrors the credentials
 * rotation modal so the dashboard's destructive surfaces share one
 * visual contract: role="dialog", aria-modal, focus trap, Escape and
 * backdrop dismiss.
 */
const RemoveConfirm: Component<RemoveConfirmProps> = (props) => {
  let dialogRef: HTMLDivElement | undefined;
  useFocusTrap(() => dialogRef);

  // WAI-ARIA APG Dialog pattern: Escape must dismiss regardless of focus
  // location. A window-level listener supersedes any internal focus
  // traversal so screen-reader and shortcut-driven focus moves still
  // get caught.
  onMount(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        props.onCancel();
      }
    };
    window.addEventListener('keydown', handler);
    onCleanup(() => window.removeEventListener('keydown', handler));
  });

  const shortId = () => props.containerId.slice(0, 12);

  return (
    <div
      class="containers-section__modal-backdrop"
      onClick={(e) => {
        if (e.target === e.currentTarget) props.onCancel();
      }}
      data-testid="container-remove-modal-backdrop"
    >
      <div
        ref={dialogRef}
        class="containers-section__modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="container-remove-title"
        data-testid="container-remove-modal"
      >
        <header class="containers-section__modal-head">
          <h3
            id="container-remove-title"
            class="containers-section__modal-title"
          >
            REMOVE CONTAINER?
          </h3>
        </header>
        <p class="containers-section__modal-body">
          Remove container <strong>{shortId()}</strong>? This cannot be undone — any state inside
          the container will be lost.
        </p>
        <div class="containers-section__modal-actions">
          <Button
            variant="ghost"
            size="sm"
            data-testid="container-remove-cancel"
            disabled={props.pending}
            onClick={props.onCancel}
          >
            CANCEL
          </Button>
          <Button
            variant="primary"
            size="sm"
            data-testid="container-remove-confirm"
            aria-busy={props.pending}
            disabled={props.pending}
            onClick={props.onConfirm}
          >
            YES, REMOVE
          </Button>
        </div>
      </div>
    </div>
  );
};

// ---------------------------------------------------------------------------
// Logs flyout
// ---------------------------------------------------------------------------

interface LogsFlyoutProps {
  containerId: string;
  onClose: () => void;
}

/** Maximum lines kept in the buffer regardless of how many polls happen. */
const LOGS_BUFFER_CAP = 1000;
/** Polling interval (ms). Matches the F-597 spec. */
const LOGS_POLL_MS = 2000;
/** Tail size on each poll — keeps the IPC payload bounded. */
const LOGS_TAIL = 200;

const LogsFlyout: Component<LogsFlyoutProps> = (props) => {
  const [lines, setLines] = createSignal<LogLine[]>([]);
  const [error, setError] = createSignal<string | null>(null);
  let dialogRef: HTMLDivElement | undefined;
  useFocusTrap(() => dialogRef);

  // The "since" cursor advances on every successful poll so the second
  // and subsequent polls only fetch new lines. The first poll uses
  // `tail` to seed the viewer with recent history.
  let sinceCursor: string | null = null;

  const fetchOnce = async (initial: boolean) => {
    try {
      const opts: { since?: string; tail?: number } = {};
      if (sinceCursor !== null) opts.since = sinceCursor;
      if (initial) opts.tail = LOGS_TAIL;
      const got = await containerLogs(props.containerId, opts);
      if (got.length === 0) return;
      // Advance the cursor past the newest line so the next poll only
      // returns deltas. The fallback is "now-ish" so a missing
      // timestamp on the last line doesn't peg us at the same spot.
      const newest = got[got.length - 1];
      if (newest?.timestamp) sinceCursor = newest.timestamp;
      setLines((cur) => {
        // `podman logs --since <ts>` is inclusive: if the boundary line
        // shares the cursor's timestamp, it'll come back on the next
        // poll. De-dup by `(timestamp, stream, line)` against the tail
        // of the buffer so identical adjacent entries don't double-print.
        // We only scan the recently-buffered tail (bounded by `got.length`)
        // to keep the merge O(n + m) rather than O(n*m).
        const seen = new Set<string>();
        const tailStart = Math.max(0, cur.length - got.length);
        for (let i = tailStart; i < cur.length; i++) {
          const l = cur[i]!;
          seen.add(`${l.timestamp ?? ''} ${l.stream} ${l.line}`);
        }
        const fresh = got.filter((l) => {
          const key = `${l.timestamp ?? ''} ${l.stream} ${l.line}`;
          if (seen.has(key)) return false;
          seen.add(key);
          return true;
        });
        const merged = cur.concat(fresh);
        return merged.length > LOGS_BUFFER_CAP
          ? merged.slice(merged.length - LOGS_BUFFER_CAP)
          : merged;
      });
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  // Initial load + interval polling.
  onMount(() => {
    void fetchOnce(true);
    const id = window.setInterval(() => {
      void fetchOnce(false);
    }, LOGS_POLL_MS);
    onCleanup(() => window.clearInterval(id));

    // ESC closes regardless of focus location.
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        props.onClose();
      }
    };
    window.addEventListener('keydown', onKey);
    onCleanup(() => window.removeEventListener('keydown', onKey));
  });

  return (
    <div
      class="containers-section__flyout-backdrop"
      onClick={(e) => {
        if (e.target === e.currentTarget) props.onClose();
      }}
      data-testid="container-logs-flyout"
    >
      <div
        ref={dialogRef}
        id={`container-logs-${props.containerId}`}
        class="containers-section__flyout"
        role="dialog"
        aria-modal="true"
        aria-labelledby="container-logs-title"
      >
        <header class="containers-section__flyout-head">
          <h3
            id="container-logs-title"
            class="containers-section__flyout-title"
          >
            LOGS — {props.containerId.slice(0, 12)}
          </h3>
          <Button
            variant="ghost"
            size="sm"
            onClick={props.onClose}
            data-testid="container-logs-close"
            aria-label="Close logs"
          >
            CLOSE
          </Button>
        </header>
        <Show when={error()}>
          {(msg) => (
            <p class="containers-section__error" role="alert">
              {msg()}
            </p>
          )}
        </Show>
        <pre
          class="containers-section__log-pane"
          role="log"
          aria-live="polite"
          aria-label={`Container ${props.containerId.slice(0, 12)} logs`}
        >
          <For each={lines()}>
            {(l) => (
              <div
                class="containers-section__log-line"
                classList={{
                  'containers-section__log-line--err': l.stream === 'stderr',
                }}
              >
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
        ⚠
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
  return s.length > max ? `${s.slice(0, max - 1)}…` : s;
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

