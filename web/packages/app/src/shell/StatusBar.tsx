// F-138: status-bar host that surfaces background-agent state for the active
// session. Follows `docs/ui-specs/shell.md` §2 for placement (bottom strip of
// the Session window) and `docs/product/ai-ux.md` §4.5 + §10.6 for the
// `notifications.bg_agents` semantics (`toast | os | both | silent`).
//
// Wiring:
//  - On mount: `listBackgroundAgents(sessionId)` seeds the running set.
//  - On mount: `subscribe(handler)` hooks into the same `session:event`
//    channel F-137 emits on. Started → insert; Completed → remove + fire the
//    configured notification.
//  - Badge hidden when count is zero. Click the badge → popover with one row
//    per running agent and Promote/Stop buttons.
//
// Testability seams. Every external dependency — the IPC list/promote/stop
// calls, the Tauri `session:event` listener, the OS-notification plugin, and
// the in-app toast queue — is injectable as a prop. This keeps the component
// testable in jsdom without pulling `@tauri-apps/plugin-notification` into
// the render. Production call sites use the defaults.

import {
  type Component,
  For,
  Show,
  createSignal,
  onCleanup,
  onMount,
} from 'solid-js';
import { useNavigate } from '@solidjs/router';
import { Button } from '@forge/design';
import type { BgAgentSummary } from '@forge/ipc';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import {
  isPermissionGranted as pluginIsPermissionGranted,
  requestPermission as pluginRequestPermission,
  sendNotification as pluginSendNotification,
} from '@tauri-apps/plugin-notification';
import {
  listBackgroundAgents as defaultListBackgroundAgents,
  onSessionEvent,
  promoteBackgroundAgent as defaultPromoteBackgroundAgent,
  stopBackgroundAgent as defaultStopBackgroundAgent,
  type SessionEventPayload,
} from '../ipc/session';
import {
  getActiveProvider as defaultGetActiveProvider,
  gitBranch as defaultGitBranch,
  sessionList as defaultSessionList,
} from '../ipc/dashboard';
import {
  detectContainerRuntime as defaultDetectContainerRuntime,
  type RuntimeStatus,
} from '../ipc/containers';
import { activeSessionId, activeWorkspaceRoot } from '../stores/session';
import { settings } from '../stores/settings';
import { pushToast } from '../components/toast';
import './StatusBar.css';

// ---------------------------------------------------------------------------
// Event bus + notification adapter surfaces (injectable).
// ---------------------------------------------------------------------------

/** Subscribe to `session:event` payloads. Returns an unlisten handle. */
export type BgAgentsSubscribe = (
  handler: (payload: SessionEventPayload) => void,
) => Promise<() => void>;

/**
 * Notification side-effect surface. `toast` pushes into the in-app queue;
 * `os` dispatches an OS-level notification; the permission helpers power the
 * lazy request path so we don't prompt at mount.
 */
export interface NotificationAdapter {
  toast: (message: string) => void;
  os: (message: string) => void | Promise<void>;
  isPermissionGranted: () => Promise<boolean>;
  requestPermission: () => Promise<'granted' | 'denied' | 'default'>;
}

export const defaultNotificationAdapter: NotificationAdapter = {
  toast: (message) => {
    pushToast('info', message);
  },
  os: (message) => {
    pluginSendNotification({ title: 'Forge', body: message });
  },
  isPermissionGranted: () => pluginIsPermissionGranted(),
  requestPermission: async () => {
    const res = await pluginRequestPermission();
    if (res === 'granted' || res === 'denied' || res === 'default') return res;
    return 'default';
  },
};

/** Production default: forward every `onSessionEvent` payload verbatim. */
const defaultSubscribe: BgAgentsSubscribe = (handler) =>
  onSessionEvent(handler);

// ---------------------------------------------------------------------------
// F-717: route-agnostic status feeds.
//
// Each segment of the left/right slots is sourced from a small injectable
// surface so jsdom tests can drive every (loading | unknown | ready)
// transition without a real Tauri bridge.
// ---------------------------------------------------------------------------

/** Connection-state pill variant per `app-shell.md §Status bar`. */
export type ConnectionState =
  | 'streaming'
  | 'awaiting approval'
  | 'done'
  | 'idle'
  | 'ready'
  | 'auth';

/** Subscribe to `provider:changed` Tauri events. */
export type ProviderChangedSubscribe = (
  handler: (providerId: string) => void,
) => Promise<UnlistenFn>;

const defaultProviderChangedSubscribe: ProviderChangedSubscribe = async (
  handler,
) =>
  listen<{ type: 'provider_changed'; provider_id?: string }>(
    'provider:changed',
    (event) => {
      const providerId = event.payload?.provider_id;
      if (typeof providerId === 'string' && providerId.length > 0) {
        handler(providerId);
      }
    },
  );

/** Token rendered when a feed has not yielded data. */
export const UNKNOWN_TOKEN = 'unknown';

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

/**
 * Router navigate seam. Matches `@solidjs/router`'s `useNavigate()` return
 * shape closely enough for tests to pass a `vi.fn()` without importing the
 * router's `Navigator` type. F-153 uses this so double-click / right-click
 * handlers can navigate to `/agents/<sessionId>?instance=<id>` without the
 * test harness needing to mount a full `<MemoryRouter>`.
 */
export type StatusBarNavigate = (to: string) => void;

export interface StatusBarProps {
  /** Initial snapshot provider. Defaults to the real Tauri command. */
  listBackgroundAgents?: typeof defaultListBackgroundAgents;
  /** Promote IPC wrapper. */
  promoteBackgroundAgent?: typeof defaultPromoteBackgroundAgent;
  /** Stop IPC wrapper. */
  stopBackgroundAgent?: typeof defaultStopBackgroundAgent;
  /** Event-bus subscription. Test harnesses inject a controllable fake. */
  subscribe?: BgAgentsSubscribe;
  /** Notification delivery adapter. */
  notificationAdapter?: NotificationAdapter;
  /**
   * Router navigate function. Defaults to `useNavigate()`; tests inject a
   * spy so the badge / popover-row double-click + right-click nav can be
   * asserted without a router harness.
   */
  navigate?: StatusBarNavigate;
  /**
   * F-717: source of the `<N> sessions` segment in the left slot. Defaults
   * to the typed `session_list` IPC wrapper; tests inject a controllable
   * promise so the loading / unknown / ready transitions can be exercised
   * without a real Tauri bridge.
   */
  sessionList?: typeof defaultSessionList;
  /**
   * F-717: initial active-provider id read. Defaults to the
   * `get_active_provider` IPC wrapper. The `provider:changed` subscription
   * keeps the segment fresh after the first paint.
   */
  getActiveProvider?: typeof defaultGetActiveProvider;
  /**
   * F-717: subscribe to dashboard `provider:changed` events. Defaults to
   * the real Tauri `listen('provider:changed', ...)` channel.
   */
  subscribeProviderChanged?: ProviderChangedSubscribe;
  /**
   * F-717: connection state surfaced as the rightmost left-slot segment.
   * `null` renders the literal `unknown` token. Explicit values override
   * the F-741 derived streaming-state. Sourced upstream from the session
   * telemetry store; left as a prop so test harnesses can pin the segment
   * deterministically.
   */
  connectionState?: ConnectionState | null;
  /**
   * F-741: source of the `<branch>` segment in the left slot. Defaults to
   * the typed `git_branch` IPC wrapper. The component calls it with the
   * active workspace root; failures and detached HEAD fall back to the
   * literal `unknown` token per the status-bar contract.
   */
  gitBranch?: typeof defaultGitBranch;
  /**
   * F-741: source of the runtime segment in the right slot. Defaults to
   * the `detect_container_runtime` IPC wrapper. The probe returns a
   * structured `RuntimeStatus`; failures fall back to `unknown`.
   */
  detectContainerRuntime?: typeof defaultDetectContainerRuntime;
  /**
   * F-741: override the workspace root the branch feed queries. Defaults
   * to the global `activeWorkspaceRoot` signal. Tests inject a literal
   * string so they don't have to seed the store from outside the shell.
   */
  workspaceRoot?: string | null;
}

interface BgEventStarted {
  type: 'background_agent_started';
  id: string;
  agent?: string;
}

interface BgEventCompleted {
  type: 'background_agent_completed';
  id: string;
}

/** Narrowing helper — guards against malformed payloads on the event bus. */
function classifyEvent(ev: unknown): BgEventStarted | BgEventCompleted | null {
  if (typeof ev !== 'object' || ev === null) return null;
  const obj = ev as Record<string, unknown>;
  const type = obj['type'];
  const id = obj['id'];
  if (typeof id !== 'string') return null;
  if (type === 'background_agent_started') {
    const agent =
      typeof obj['agent'] === 'string' ? (obj['agent'] as string) : undefined;
    return agent === undefined
      ? { type, id }
      : { type, id, agent };
  }
  if (type === 'background_agent_completed') {
    return { type, id };
  }
  return null;
}

/**
 * F-717: literal `·` between left/right slot segments. `aria-hidden` so
 * screen readers don't announce the dot as part of the slot copy — the
 * surrounding segments carry the meaningful labels.
 */
const Separator: Component = () => (
  <span class="status-bar__separator" aria-hidden="true">
    ·
  </span>
);

export const StatusBar: Component<StatusBarProps> = (props) => {
  const listBg = (): typeof defaultListBackgroundAgents =>
    props.listBackgroundAgents ?? defaultListBackgroundAgents;
  const promoteBg = (): typeof defaultPromoteBackgroundAgent =>
    props.promoteBackgroundAgent ?? defaultPromoteBackgroundAgent;
  const stopBg = (): typeof defaultStopBackgroundAgent =>
    props.stopBackgroundAgent ?? defaultStopBackgroundAgent;
  const subscribe = (): BgAgentsSubscribe =>
    props.subscribe ?? defaultSubscribe;
  const notifier = (): NotificationAdapter =>
    props.notificationAdapter ?? defaultNotificationAdapter;

  // F-153: default navigate comes from the router context when no test
  // spy is threaded through. Solid's `useNavigate()` is owner-scoped: it
  // MUST be resolved during component setup because event handlers fire
  // outside the component's owner and context lookups at that point
  // return undefined. Existing StatusBar tests mount without a
  // `<MemoryRouter>` and never exercise the nav path, so we tolerate the
  // missing router by swallowing the invariant during setup.
  let routerNavigate: ((to: string) => void) | null = null;
  try {
    routerNavigate = useNavigate();
  } catch {
    // No Router in the render tree (notification / unit tests outside the
    // app shell). The navigate fallback becomes a no-op; a production
    // mount always has the app-shell Router so this branch is unreachable
    // end-to-end.
    routerNavigate = null;
  }
  const navigate = (to: string): void => {
    if (props.navigate) {
      props.navigate(to);
      return;
    }
    if (routerNavigate) routerNavigate(to);
  };

  // Running set keyed by instance id. The same id from multiple sources (the
  // initial list + the subscribe stream) is idempotently deduped.
  const [running, setRunning] = createSignal<BgAgentSummary[]>([]);
  const [popoverOpen, setPopoverOpen] = createSignal(false);

  // F-717: left-slot feeds. `null` means the source has not yielded yet —
  // renders the literal `unknown` token per the app-shell spec.
  const [sessionCount, setSessionCount] = createSignal<number | null>(null);
  const [providerId, setProviderId] = createSignal<string | null>(null);

  // F-741: branch, streaming-state, and runtime feeds. Each defaults to
  // `null` (renders `unknown`) until its source yields. IPC failures are
  // logged + leave the signal at `null` so the chrome stays decoupled
  // from per-route error states.
  const [branch, setBranch] = createSignal<string | null>(null);
  const [streamingState, setStreamingState] =
    createSignal<ConnectionState | null>(null);
  const [runtimeStatus, setRuntimeStatus] = createSignal<RuntimeStatus | null>(
    null,
  );

  let unlisten: (() => void) | null = null;
  let unlistenProviderChanged: UnlistenFn | null = null;
  let mounted = true;

  const badgeCount = () =>
    running().filter((r) => r.state === 'Running').length;

  const notifyOnCompletion = async (instanceId: string): Promise<void> => {
    const mode = settings.notifications.bg_agents;
    const message = `Background agent ${instanceId.slice(0, 8)} completed`;
    if (mode === 'silent') return;
    const n = notifier();
    if (mode === 'toast' || mode === 'both') {
      try {
        n.toast(message);
      } catch (err) {
        console.error('bg_agents toast failed', err);
      }
    }
    if (mode === 'os' || mode === 'both') {
      try {
        let granted = await n.isPermissionGranted();
        if (!granted) {
          const res = await n.requestPermission();
          granted = res === 'granted';
        }
        if (granted) {
          await n.os(message);
        }
      } catch (err) {
        console.error('bg_agents OS notification failed', err);
      }
    }
  };

  const handlePayload = (payload: SessionEventPayload): void => {
    if (payload.session_id !== activeSessionId()) return;
    // F-741: derive streaming-state from the session event stream so the
    // left-slot pill reflects live turn activity. `StreamingStarted` /
    // `StreamingStopped` are the authoritative kinds emitted by the
    // orchestrator; `AssistantDelta` is a defensive fallback for the same
    // window so a missed start event still flips the pill on first delta.
    if (
      typeof payload.event === 'object' &&
      payload.event !== null
    ) {
      const kind = (payload.event as { kind?: unknown }).kind;
      if (kind === 'StreamingStarted' || kind === 'AssistantDelta') {
        setStreamingState('streaming');
      } else if (kind === 'StreamingStopped') {
        setStreamingState('idle');
      } else if (kind === 'AssistantMessage') {
        // The orchestrator emits an empty `AssistantMessage` at stream start
        // and a populated one at finalisation. Either flips us out of
        // `streaming` — the next `AssistantDelta` (or `StreamingStarted`)
        // brings it back if another turn begins.
        setStreamingState('idle');
      }
    }
    const ev = classifyEvent(payload.event);
    if (ev === null) return;
    if (ev.type === 'background_agent_started') {
      setRunning((prev) => {
        if (prev.some((r) => r.id === ev.id)) return prev;
        return [
          ...prev,
          {
            id: ev.id,
            agent_name: ev.agent ?? 'agent',
            state: 'Running',
          },
        ];
      });
      return;
    }
    // background_agent_completed
    setRunning((prev) => prev.filter((r) => r.id !== ev.id));
    void notifyOnCompletion(ev.id);
  };

  onMount(() => {
    // Install the subscription BEFORE the initial list so a fast completion
    // event between the two awaits isn't lost to a listener-registration race
    // — mirrors the same discipline `BackgroundAgentRegistry::start` uses for
    // its orchestrator-stream subscription.
    void (async () => {
      try {
        const off = await subscribe()(handlePayload);
        if (mounted) {
          unlisten = off;
        } else {
          off();
        }
      } catch (err) {
        console.error('bg-agents subscribe failed', err);
      }
    })();
    void (async () => {
      const id = activeSessionId();
      if (id === null) return;
      try {
        const snapshot = await listBg()(id);
        if (mounted && Array.isArray(snapshot)) {
          const runningRows = snapshot.filter((r) => r.state === 'Running');
          setRunning(runningRows);
        }
      } catch (err) {
        console.error('list_background_agents failed', err);
      }
    })();

    // F-717: seed the `<N> sessions` segment from `session_list`. A failed
    // fetch leaves the signal at `null`, which renders `unknown` — chrome
    // never paints `role="alert"` for status feeds.
    const listSessions = props.sessionList ?? defaultSessionList;
    void (async () => {
      try {
        const rows = await listSessions();
        if (mounted && Array.isArray(rows)) setSessionCount(rows.length);
      } catch (err) {
        console.error('session_list failed', err);
      }
    })();

    // F-717: seed the `<provider>` segment from `get_active_provider`, then
    // hook `provider:changed` so dashboard swaps update the bar live.
    const getProvider = props.getActiveProvider ?? defaultGetActiveProvider;
    void (async () => {
      try {
        const id = await getProvider();
        if (mounted && typeof id === 'string' && id.length > 0) {
          setProviderId(id);
        }
      } catch (err) {
        console.error('get_active_provider failed', err);
      }
    })();
    const subscribeProvider =
      props.subscribeProviderChanged ?? defaultProviderChangedSubscribe;
    void (async () => {
      try {
        const off = await subscribeProvider((id) => {
          if (mounted) setProviderId(id);
        });
        if (mounted) {
          unlistenProviderChanged = off;
        } else {
          off();
        }
      } catch (err) {
        console.error('provider:changed listen failed', err);
      }
    })();

    // F-741: branch feed. The active workspace root drives the query;
    // either prop override or the global signal. `null` workspace (no
    // session mounted yet) keeps the segment at `unknown` rather than
    // shelling out to git in `/` and surfacing a stray fatal.
    const getBranch = props.gitBranch ?? defaultGitBranch;
    const root = props.workspaceRoot ?? activeWorkspaceRoot();
    if (typeof root === 'string' && root.length > 0) {
      void (async () => {
        try {
          const name = await getBranch(root);
          if (mounted && typeof name === 'string' && name.length > 0) {
            setBranch(name);
          }
        } catch (err) {
          console.error('git_branch failed', err);
        }
      })();
    }

    // F-741: runtime probe. Single-shot at mount — the dashboard's
    // ContainersSection owns the long-lived banner; the status-bar
    // segment is a chrome-level hint, not the user's actionable banner.
    // Probe failures or unavailable runtimes both render `unknown`.
    const detectRuntime =
      props.detectContainerRuntime ?? defaultDetectContainerRuntime;
    void (async () => {
      try {
        const status = await detectRuntime();
        if (mounted && status && typeof status === 'object') {
          setRuntimeStatus(status);
        }
      } catch (err) {
        console.error('detect_container_runtime failed', err);
      }
    })();
  });

  onCleanup(() => {
    mounted = false;
    if (unlisten) {
      unlisten();
      unlisten = null;
    }
    if (unlistenProviderChanged) {
      unlistenProviderChanged();
      unlistenProviderChanged = null;
    }
  });

  const togglePopover = (): void => {
    setPopoverOpen((v) => !v);
  };

  // F-153: badge double-click opens the Agent Monitor for this session with
  // no `instance` param — the monitor auto-selects the first row. Useful as
  // a zero-context entry point when the user wants to see "whatever's
  // running" rather than inspect a specific agent.
  const onBadgeDoubleClick = (e: MouseEvent): void => {
    e.preventDefault();
    const id = activeSessionId();
    if (id === null) return;
    navigate(`/agents/${id}`);
  };

  // F-153: popover-row double-click / right-click is the primary "open this
  // exact instance" path. Both gestures push `?instance=<id>` so the monitor
  // pre-selects the row; right-click also suppresses the native context
  // menu so the Tauri webview doesn't surface its default menu over the
  // popover.
  const onRowDoubleClick = (instanceId: string) => (e: MouseEvent): void => {
    e.preventDefault();
    const sid = activeSessionId();
    if (sid === null) return;
    navigate(`/agents/${sid}?instance=${instanceId}`);
  };
  const onRowContextMenu = (instanceId: string) => (e: MouseEvent): void => {
    e.preventDefault();
    const sid = activeSessionId();
    if (sid === null) return;
    navigate(`/agents/${sid}?instance=${instanceId}`);
  };

  const onPromote = async (id: string): Promise<void> => {
    const sid = activeSessionId();
    if (sid === null) return;
    try {
      await promoteBg()(sid, id);
      setRunning((prev) => prev.filter((r) => r.id !== id));
    } catch (err) {
      console.error('promote_background_agent failed', err);
    }
  };

  const onStop = async (id: string): Promise<void> => {
    const sid = activeSessionId();
    if (sid === null) return;
    try {
      await stopBg()(sid, id);
      // Don't pre-remove: the completion event will clear the row and fire
      // the notification on the same path as any other terminal transition.
    } catch (err) {
      console.error('stop_background_agent failed', err);
    }
  };

  // F-717: every left/right slot field renders the literal `unknown` token
  // when its feed has not yielded. `app-shell.md §Status bar` is explicit:
  // unknown is NOT collapsed, NOT hidden, NOT an em-dash — the token reads
  // `unknown` so users can audit the missing-feed at a glance.
  const sessionCountLabel = (): string => {
    const n = sessionCount();
    return n === null ? UNKNOWN_TOKEN : `${n} sessions`;
  };
  const providerLabel = (): string => providerId() ?? UNKNOWN_TOKEN;
  // F-741: prop override wins so test harnesses can pin the segment; the
  // derived streaming-state otherwise drives the pill from `session:event`.
  const stateLabel = (): string =>
    props.connectionState ?? streamingState() ?? UNKNOWN_TOKEN;
  // F-741: branch comes from the workspace's git `rev-parse HEAD`. Detached
  // HEAD (`branch: null`) and IPC failures both render the literal token.
  const branchLabel = (): string => branch() ?? UNKNOWN_TOKEN;
  // TODO(F-741-followup): wire usage feed (spend + token totals from
  // forge-usage). Kept stubbed under the same F-741 umbrella — the usage
  // store is its own follow-up because the segment depends on a daemon
  // running, not just the dashboard probe.
  const usageLabel = (): string => UNKNOWN_TOKEN;
  // F-741: runtime status from the existing `detect_container_runtime`
  // probe. `available` renders the tool name + check; every other
  // variant carries a `tool` field (Missing / Broken / RootlessUnavailable)
  // and surfaces it with a `down` suffix; `Unknown` and pre-probe both
  // collapse to the literal token.
  const runtimeLabel = (): string => {
    const status = runtimeStatus();
    if (status === null) return UNKNOWN_TOKEN;
    switch (status.kind) {
      case 'available':
        // Phase 3 only ships the podman backend; the runtime probe does
        // not echo the tool name on `available`. Hard-coded for the same
        // reason `ContainersSection.tsx` does — F-595 fixed the backend.
        return 'podman up';
      case 'missing':
      case 'broken':
      case 'rootless_unavailable':
        return `${status.tool} down`;
      case 'unknown':
        return UNKNOWN_TOKEN;
    }
  };

  return (
    <footer class="status-bar" aria-label="Status bar" data-testid="status-bar">
      <div class="status-bar__left" data-testid="status-bar-left">
        <span class="status-bar__segment status-bar__brand">forge</span>
        <Separator />
        <span
          class="status-bar__segment"
          data-testid="status-bar-branch"
        >
          {branchLabel()}
        </span>
        <Separator />
        <span
          class="status-bar__segment"
          data-testid="status-bar-sessions"
        >
          {sessionCountLabel()}
        </span>
        <Separator />
        <span
          class="status-bar__segment"
          data-testid="status-bar-provider"
        >
          {providerLabel()}
        </span>
        <Separator />
        <span
          class="status-bar__segment status-bar__state"
          data-state={stateLabel()}
          data-testid="status-bar-state"
        >
          {stateLabel()}
        </span>
      </div>
      <div class="status-bar__right" data-testid="status-bar-right">
        <span
          class="status-bar__segment"
          data-testid="status-bar-usage"
        >
          {usageLabel()}
        </span>
        <Separator />
        <span
          class="status-bar__segment"
          data-testid="status-bar-runtime"
        >
          {runtimeLabel()}
        </span>
        <Show when={badgeCount() > 0}>
          <button
            type="button"
            class="status-bar__bg-badge"
            data-testid="bg-agents-badge"
            aria-label={`${badgeCount()} background agents`}
            aria-haspopup="menu"
            aria-expanded={popoverOpen()}
            onClick={togglePopover}
            onDblClick={onBadgeDoubleClick}
          >
            <span class="status-bar__bg-badge-count">{badgeCount()}</span>
            <span class="status-bar__bg-badge-label">{' bg'}</span>
          </button>
        </Show>
      </div>
      <Show when={popoverOpen() && badgeCount() > 0}>
        <div
          class="status-bar__bg-popover"
          data-testid="bg-agents-popover"
          role="menu"
        >
          <For each={running().filter((r) => r.state === 'Running')}>
            {(row) => (
              <div
                class="status-bar__bg-row"
                role="menuitem"
                data-testid={`bg-agents-row-${row.id}`}
                onDblClick={onRowDoubleClick(row.id)}
                onContextMenu={onRowContextMenu(row.id)}
              >
                <span
                  class="status-bar__bg-row-name"
                  data-testid={`bg-agents-name-${row.id}`}
                >
                  {row.agent_name}
                </span>
                <span class="status-bar__bg-row-id">{row.id.slice(0, 8)}</span>
                <Button
                  variant="ghost"
                  size="sm"
                  class="status-bar__bg-row-action"
                  data-testid={`bg-agents-promote-${row.id}`}
                  onClick={() => {
                    void onPromote(row.id);
                  }}
                >
                  PROMOTE AGENT
                </Button>
                <Button
                  variant="danger"
                  size="sm"
                  class="status-bar__bg-row-action status-bar__bg-row-action--stop"
                  data-testid={`bg-agents-stop-${row.id}`}
                  onClick={() => {
                    void onStop(row.id);
                  }}
                >
                  STOP AGENT
                </Button>
              </div>
            )}
          </For>
        </div>
      </Show>
    </footer>
  );
};
