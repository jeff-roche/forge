import {
  type Component,
  type JSX,
  Show,
  createSignal,
  onCleanup,
  onMount,
} from 'solid-js';
import { useParams } from '@solidjs/router';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { LayoutTree, ProviderId, SessionId } from '@forge/ipc';
import {
  getPersistentApprovals,
  getSettings,
  onSessionCrashed,
  onSessionEvent,
  sessionHello,
  sessionRestart,
  sessionSubscribe,
  sessionSwitchProvider,
} from '../../ipc/session';
import {
  activeWorkspaceRoot,
  setActiveOpenFile,
  setActiveSessionId,
  setActiveWorkspaceRoot,
  setSessionEvents,
  setSessions,
} from '../../stores/session';
import { pushEvent } from '../../stores/messages';
import {
  getSessionTelemetry,
  routeTelemetryEvent,
} from '../../stores/sessionTelemetry';
import { fromRustEvent } from '../../ipc/events';
import {
  formatChatSubject,
  formatCostLabel,
  formatProviderLabel,
  resolveProviderId,
} from './costLabel';
import { seedPersistentApprovals } from '../../stores/approvals';
import { seedSettings } from '../../stores/settings';
import { PaneHeader, type PaneHeaderStatus } from './PaneHeader';
import { ChatPane } from './ChatPane';
import {
  CrashRestartOverlay,
  type CrashOverlayState,
} from './CrashRestartOverlay';
import { EditorPane } from '../../panes/EditorPane';
import { TerminalPane } from '../../panes/TerminalPane';
import { usePaneWidth } from '../../layout/usePaneWidth';
import {
  createLayoutStore,
  type LayoutStore,
} from '../../layout/layoutStore';
import { GridContainer, type LayoutLeaf } from '../../layout/GridContainer';
import { useDragToDock } from '../../layout/useDragToDock';
import './SessionWindow.css';

/**
 * F-126 test seam. Setting this before mounting SessionWindow makes the
 * component use the provided store instead of constructing one via
 * `createLayoutStore(workspaceRoot)`. Production callers never touch this;
 * tests inject a fake store so they can drive the Open -> EditorPane flow
 * without stubbing `read_layouts`/`write_layouts`. Reset to `null` in
 * `afterEach` so cross-test leakage is impossible.
 *
 * Kept as a module-level slot rather than a prop so SessionWindow's public
 * signature remains `Component<RouteSectionProps>` — the `@solidjs/router`
 * `component=` prop can't accept a widened prop type.
 */
let injectedLayoutStore: LayoutStore | null = null;
export function __setInjectedLayoutStoreForTesting(
  store: LayoutStore | null,
): void {
  injectedLayoutStore = store;
}

/**
 * Session window shell — default single-pane layout from
 * docs/ui-specs/layout-panes.md §3.4. F-150 replaces the F-126 "singleton
 * editor slot" with a real GridContainer: the active layout's tree drives
 * rendering end-to-end, so multiple editors can coexist, drag-to-dock works
 * on editor leaves the same as any other pane, and F-119 compactness runs
 * per-leaf rather than on a single window-scope pane.
 */
export const SessionWindow: Component = () => {
  const params = useParams<{ id: string }>();
  const sessionId = () => params.id as SessionId;

  let unlisten: (() => void) | null = null;
  // F-640: per-mount unlisten for the dashboard's `provider:changed`
  // Tauri event. Detached on cleanup so a remounted route never
  // double-subscribes.
  let unlistenProviderChanged: UnlistenFn | null = null;
  // F-748: per-mount unlisten for the bridge's `session:crashed`
  // notification.
  let unlistenSessionCrashed: UnlistenFn | null = null;
  let mounted = true;

  // F-748: crash-restart state machine.
  // - `crashState === null` is the steady-state (no overlay, idle pill).
  // - `prompting` → user has not chosen yet; overlay shows Restart/Close.
  // - `restarting` → `session_restart` IPC in flight.
  // - `restart_failed` → IPC rejected; overlay shows Retry/Close.
  // The "restored" branch is `crashState === null` again after a
  // successful restart + re-hello + re-subscribe.
  const [crashState, setCrashState] = createSignal<CrashOverlayState | null>(
    null,
  );
  // High-water mark of forwarded event seqs. Captured from the
  // `session:crashed` payload and re-supplied as `since` on the
  // post-restart `sessionSubscribe` so history replay starts after the
  // last frame the webview already rendered.
  const [lastSeq, setLastSeq] = createSignal<number>(0);
  const [restartError, setRestartError] = createSignal<string | null>(null);

  /**
   * F-748: per-pane health surface — `error` while the overlay is up,
   * `idle` once we've restored. The detecting state is reserved for a
   * future heartbeat / inactivity probe (the issue describes it as the
   * pre-crash silence window, but the bridge today only signals at the
   * confirmed-crash boundary, so we go straight to `error`).
   */
  const headerStatus = (): PaneHeaderStatus => {
    const cs = crashState();
    return cs === null ? 'idle' : 'error';
  };

  // F-126: layout store is lazily instantiated once the session's workspace
  // root is known (HelloAck). The FilesSidebar's onOpen handler and the
  // EditorPane slot both read/write through this store. Tests inject a
  // pre-built store via `__setInjectedLayoutStoreForTesting` to bypass
  // `read_layouts` and drive the Open -> EditorPane flow deterministically.
  const preInjected = injectedLayoutStore;
  const [store, setStore] = createSignal<LayoutStore | null>(preInjected);

  onMount(() => {
    const id = sessionId();
    setActiveSessionId(id);
    setSessions(id, { id, state: 'Active' });

    void (async () => {
      try {
        const ack = await sessionHello(id);
        // F-036: remember the workspace root from the HelloAck so the
        // ApprovalPrompt / WhitelistedPill code paths can pass it to the
        // persistent-approval commands without re-querying.
        setActiveWorkspaceRoot(ack.workspace);
        // F-126: spin up the layout store against the session's workspace
        // root (unless a test injected one). `store.load()` hydrates from
        // disk; a missing/corrupt layouts file silently defaults to the
        // single-pane layout so the UI doesn't stall on hello.
        if (preInjected === null) {
          const next = createLayoutStore(ack.workspace);
          try {
            await next.load();
          } catch (loadErr) {
            console.error('read_layouts failed; using default', loadErr);
          }
          if (mounted) setStore(next);
        }
        await sessionSubscribe(id);
        // F-036: seed the per-session whitelist with any persisted
        // workspace/user approvals for the active workspace. Failure here is
        // non-fatal — the session still runs, users just won't see auto-
        // approvals until they reload. Log and continue.
        try {
          const persisted = await getPersistentApprovals(ack.workspace);
          seedPersistentApprovals(id, persisted);
        } catch (seedErr) {
          console.error('get_persistent_approvals failed', seedErr);
        }
        // F-151: seed the persistent settings store. Same failure discipline
        // as approvals above — a failed load leaves the defaults in place
        // rather than blocking session startup.
        try {
          const persistedSettings = await getSettings(ack.workspace);
          seedSettings(persistedSettings);
        } catch (seedErr) {
          console.error('get_settings failed', seedErr);
        }
      } catch (err) {
        console.error('session_hello/subscribe failed', err);
      }
      // F-640: subscribe to the dashboard's `provider:changed` event
      // and forward each as `IpcMessage::SwitchProvider` over this
      // session's UDS. The daemon's arm in `handle_connection` resolves
      // `provider_id` and calls `SwappableProvider::swap` so the next
      // `run_turn` invocation dispatches to the new provider — the
      // in-flight turn (if any) finishes on the old one. Failure to
      // forward is logged and swallowed: a transient bridge error must
      // not crash the session window.
      try {
        unlistenProviderChanged = await listen<{
          type: 'provider_changed';
          provider_id: string;
        }>('provider:changed', (event) => {
          const providerId = event.payload?.provider_id;
          if (typeof providerId !== 'string' || providerId.length === 0) {
            return;
          }
          void sessionSwitchProvider(id, providerId).catch((err) => {
            console.error('session_switch_provider failed', err);
          });
        });
        if (!mounted && unlistenProviderChanged) {
          unlistenProviderChanged();
          unlistenProviderChanged = null;
        }
      } catch (err) {
        console.error('provider:changed listen failed', err);
      }

      const listener = await onSessionEvent((payload) => {
        if (payload.session_id !== id) return;
        setSessionEvents(payload.session_id, {
          lastSeq: payload.seq,
          lastEvent: payload.event,
        });
        // F-748: keep the local resume anchor in sync with what we've
        // rendered. If the daemon crashes next, this is the `since` we
        // hand back on re-subscribe so history replay produces no
        // duplicates.
        setLastSeq(payload.seq);
        // Route typed events to the messages store for ChatPane rendering.
        // `payload.event` is the Rust-serialized `forge_core::Event` shape
        // (`{"type":"user_message",...}`); fromRustEvent adapts it to the
        // store's discriminated union. Non-renderable variants return null.
        const storeEvent = fromRustEvent(payload.event);
        if (storeEvent) pushEvent(payload.session_id, storeEvent);
        // F-395: side-channel — the PaneHeader's provider pill + cost meter
        // read from the per-session telemetry store, which is fed by the
        // same wire events (assistant_message for provider/model, usage_tick
        // for tokens + cost). Kept off the messages-store path so a chat
        // log shape change can't ripple into the header.
        routeTelemetryEvent(payload.session_id, payload.event);
      });

      // F-748: subscribe to the bridge's crash signal. One emission per
      // dead daemon (the pump exits after firing it), so no debouncing.
      try {
        unlistenSessionCrashed = await onSessionCrashed((payload) => {
          if (payload.session_id !== id) return;
          setLastSeq(payload.last_seq);
          setRestartError(null);
          setCrashState('prompting');
        });
        if (!mounted && unlistenSessionCrashed) {
          unlistenSessionCrashed();
          unlistenSessionCrashed = null;
        }
      } catch (err) {
        console.error('session:crashed listen failed', err);
      }
      if (mounted) {
        unlisten = listener;
      } else {
        // Component unmounted before the async setup completed — detach immediately.
        listener();
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
    if (unlistenSessionCrashed) {
      unlistenSessionCrashed();
      unlistenSessionCrashed = null;
    }
    // Flush any pending layout-store write before we drop the reference so
    // a rapid "open file then navigate away" doesn't lose the active_file
    // update. `flush()` is a no-op when no debounce is pending.
    const s = store();
    if (s) void s.flush();
    setActiveSessionId(null);
    setActiveWorkspaceRoot(null);
    setActiveOpenFile(null);
  });

  const handleCloseWindow = () => {
    try {
      void getCurrentWindow().close();
    } catch (err) {
      console.error('window close failed', err);
    }
  };

  // F-748: drive the crash-restart IPC. Captures the workspace root from
  // the cached store (populated by the original HelloAck) and re-attaches
  // via `sessionHello` + `sessionSubscribe({ since: lastSeq })` once the
  // new daemon's UDS is up. Failure flips the overlay to `restart_failed`
  // with the error string so the user can retry or close.
  const handleRestart = async (): Promise<void> => {
    const id = sessionId();
    const workspace = activeWorkspaceRoot();
    if (!workspace) {
      setRestartError(
        'session_restart: no cached workspace root — close and re-open the session',
      );
      setCrashState('restart_failed');
      return;
    }
    setCrashState('restarting');
    setRestartError(null);
    try {
      await sessionRestart(id, workspace);
      // The new daemon is up; re-perform the handshake against the
      // canonical socket path. `sessionHello` is keyed on the same
      // session id so the shell's bridge registry (already drained by
      // `session_restart`'s `drop_connection`) re-installs cleanly.
      await sessionHello(id);
      // Resume from where we left off. `sessionSubscribe`'s `since`
      // anchor is what the no-duplicate-events invariant rides on.
      await sessionSubscribe(id, lastSeq());
      if (!mounted) return;
      setCrashState(null);
    } catch (err) {
      if (!mounted) return;
      const message = err instanceof Error ? err.message : String(err);
      setRestartError(message);
      setCrashState('restart_failed');
    }
  };

  // F-395: PaneHeader fields derive from the per-session telemetry store,
  // which is fed by wire events (`assistant_message` → provider/model,
  // `usage_tick` → tokens + cost). Reading through `getSessionTelemetry`
  // inside these accessor functions opts into Solid's fine-grained store
  // reactivity so the header updates the moment a tick lands. Before the
  // first assistant turn, the provider pill falls back to the active
  // provider id (or a neutral placeholder when none is set) per
  // `pane-header.md §PH.3` — never the unsanctioned `pending` state
  // suffix (`voice-terminology.md §8`). Before the first
  // usage_tick, the cost meter renders a documented em-dash placeholder
  // rather than the fabricated `$0.00` that triggered the F-395 report.
  const telemetry = () => getSessionTelemetry(sessionId());
  const subject = () => formatChatSubject(sessionId(), telemetry());
  const providerId = (): ProviderId =>
    resolveProviderId(telemetry()) as ProviderId;
  const providerLabel = () => formatProviderLabel(telemetry());
  const costLabel = () => formatCostLabel(telemetry());

  // F-716: AppShell owns the FilesSidebar mount and shortcut. SessionWindow
  // publishes its layout-store-backed openFile so the sidebar's Open routes
  // through `layoutStore.openFile(path)`, which either updates an existing
  // editor leaf's active_file or splits the tree to mount a fresh editor.
  // Solid's Setter treats a function value as an updater — wrap once so the
  // callable lands in the signal verbatim.
  onMount(() => {
    setActiveOpenFile(() => (path: string) => {
      const s = store();
      if (s === null) {
        console.warn('openFile dropped — layout store not ready', path);
        return;
      }
      s.openFile(path);
    });
  });

  // F-150: the active layout's tree is what GridContainer renders. When the
  // store hasn't loaded yet (no-workspace edge case on mount), we fall back
  // to a synthetic single-chat tree so the first paint isn't empty.
  const FALLBACK_TREE: LayoutTree = {
    kind: 'leaf',
    id: 'root',
    pane_type: 'chat',
  };
  const activeTree = (): LayoutTree => {
    const s = store();
    if (s === null) return FALLBACK_TREE;
    const name = s.layouts.active;
    return s.layouts.named[name]?.tree ?? FALLBACK_TREE;
  };
  const paneStateFor = (leafId: string) => {
    const s = store();
    if (s === null) return undefined;
    const name = s.layouts.active;
    return s.layouts.named[name]?.pane_state[leafId];
  };

  // Drag-to-dock hook. `getTree` reads the active tree through the store;
  // `onTreeChange` writes the new tree back via `setLayoutTree` so the
  // mutation is persisted alongside the next debounced write.
  const dockApi = useDragToDock({
    getTree: () => activeTree(),
    onTreeChange: (next) => {
      const s = store();
      if (s === null) return;
      s.setLayoutTree(s.layouts.active, next);
    },
  });

  const onRatioChange = (id: string, ratio: number): void => {
    const s = store();
    if (s === null) return;
    const current = s.layouts.named[s.layouts.active]?.tree;
    if (!current) return;
    const next = updateSplitRatio(current, id, ratio);
    if (next !== current) s.setLayoutTree(s.layouts.active, next);
  };

  const onCloseLeaf = (leafId: string): void => {
    // If the leaf being closed is the last chat leaf in the active tree,
    // treat CLOSE SESSION as a window-close (preserving the pre-F-150
    // behavior for the default single-pane session). In any other shape
    // we remove the leaf from the grid so the surviving panes reclaim
    // its space.
    const tree = activeTree();
    if (tree.kind === 'leaf' && tree.id === leafId && tree.pane_type === 'chat') {
      handleCloseWindow();
      return;
    }
    const s = store();
    if (s === null) return;
    s.closeLeaf(leafId);
  };

  const renderLeaf = (leaf: LayoutLeaf): JSX.Element => {
    // Each leaf owns its own compactness observation so a narrow split still
    // collapses chrome independently of the window width (§3.7).
    return (
      <LeafHost
        leaf={leaf}
        subject={subject()}
        providerId={providerId()}
        providerLabel={providerLabel()}
        costLabel={costLabel()}
        activeFile={paneStateFor(leaf.id)?.active_file ?? null}
        workspaceRoot={activeWorkspaceRoot()}
        onCloseLeaf={() => onCloseLeaf(leaf.id)}
        onPointerDownHeader={dockApi.startDrag(leaf.id)}
        headerStatus={headerStatus()}
        crashState={crashState()}
        restartError={restartError()}
        onRestart={() => void handleRestart()}
        onCloseSession={handleCloseWindow}
      />
    );
  };

  return (
    <section
      class="session-window"
      aria-label="Session pane grid"
    >
      <GridContainer
        tree={activeTree()}
        renderLeaf={renderLeaf}
        onRatioChange={onRatioChange}
        dragState={dockApi.drag()}
      />
    </section>
  );
};

/**
 * Per-leaf renderer. Owns the pane's own `usePaneWidth` observation so the
 * F-119 compactness thresholds are computed against the leaf's own extent,
 * not the enclosing window. Dispatches on `pane_type` — chat, editor, and
 * the remaining variants are stubbed out for now (F-125 / F-140 ship them).
 */
interface LeafHostProps {
  leaf: LayoutLeaf;
  subject: string;
  providerId: ProviderId;
  providerLabel: string;
  costLabel: string;
  activeFile: string | null;
  workspaceRoot: string | null;
  onCloseLeaf: () => void;
  onPointerDownHeader: (e: PointerEvent) => void;
  /** F-748: derived session-health status surfaced on the header. */
  headerStatus: PaneHeaderStatus;
  /** F-748: current crash-restart overlay state. `null` = no overlay. */
  crashState: CrashOverlayState | null;
  /** F-748: failure message displayed in the `restart_failed` state. */
  restartError: string | null;
  /** F-748: restart click + retry click both fire this. */
  onRestart: () => void;
  /** F-748: close-session click in the overlay. */
  onCloseSession: () => void;
}
const LeafHost: Component<LeafHostProps> = (props) => {
  const [leafEl, setLeafEl] = createSignal<HTMLElement | null>(null);
  const { compactness } = usePaneWidth(leafEl);

  // Close semantics: chat leaves inherit the prior CLOSE SESSION behavior
  // (tear down the window). Editor/terminal/etc. leaves call
  // `layoutStore.closeLeaf(id)` so the grid reclaims the freed space.
  const closeLabel = (): string =>
    props.leaf.pane_type === 'chat' ? 'CLOSE SESSION' : 'CLOSE PANE';
  const closeAriaLabel = (): string =>
    props.leaf.pane_type === 'chat' ? 'Close session window' : 'Close pane';

  // The editor and terminal panes own their own headers (EditorPane:
  // breadcrumb + CLOSE TAB; TerminalPane: shell subject + cwd + CLOSE PANE),
  // so we suppress the outer PaneHeader for those leaves to avoid a
  // double-header row. Everything else uses the standard PaneHeader.
  const showOuterHeader = (): boolean =>
    props.leaf.pane_type !== 'editor' && props.leaf.pane_type !== 'terminal';

  return (
    <div
      class="session-window__pane"
      ref={setLeafEl}
      data-pane-type={props.leaf.pane_type}
    >
      <Show when={showOuterHeader()}>
        <div
          class="session-window__pane-header"
          onPointerDown={props.onPointerDownHeader}
        >
          <Show
            when={props.leaf.pane_type === 'chat'}
            fallback={
              <PaneHeader
                subject={props.subject}
                typeLabel={paneTypeToHeaderLabel(props.leaf.pane_type)}
                compactness={compactness()}
                closeLabel={closeLabel()}
                closeAriaLabel={closeAriaLabel()}
                onClose={props.onCloseLeaf}
              />
            }
          >
            <PaneHeader
              subject={props.subject}
              providerId={props.providerId}
              providerLabel={props.providerLabel}
              costLabel={props.costLabel}
              typeLabel="CHAT"
              compactness={compactness()}
              closeLabel={closeLabel()}
              closeAriaLabel={closeAriaLabel()}
              onClose={props.onCloseLeaf}
              status={props.headerStatus}
            />
          </Show>
        </div>
      </Show>
      <div class="session-window__pane-body">
        <Show when={props.leaf.pane_type === 'chat'}>
          <div class="session-window__chat-host">
            <ChatPane />
            <Show when={props.crashState !== null}>
              <CrashRestartOverlay
                state={props.crashState as CrashOverlayState}
                onRestart={props.onRestart}
                onClose={props.onCloseSession}
                errorMessage={props.restartError ?? undefined}
              />
            </Show>
          </div>
        </Show>
        <Show when={props.leaf.pane_type === 'editor' && props.activeFile !== null}>
          <EditorPane
            path={props.activeFile as string}
            onClose={props.onCloseLeaf}
            onHeaderPointerDown={props.onPointerDownHeader}
          />
        </Show>
        <Show when={props.leaf.pane_type === 'editor' && props.activeFile === null}>
          <div class="session-window__pane-empty" data-testid="editor-pane-empty">
            No file open.
          </div>
        </Show>
        <Show when={props.leaf.pane_type === 'terminal' && props.workspaceRoot !== null}>
          <TerminalPane
            cwd={props.workspaceRoot as string}
            onClose={props.onCloseLeaf}
          />
        </Show>
        <Show when={props.leaf.pane_type === 'terminal' && props.workspaceRoot === null}>
          <div class="session-window__pane-empty" data-testid="terminal-pane-empty">
            Waiting for workspace…
          </div>
        </Show>
        <Show when={props.leaf.pane_type === 'files'}>
          <div class="session-window__pane-empty" data-testid="files-pane-stub">
            Files pane.
          </div>
        </Show>
        <Show when={props.leaf.pane_type === 'agentmonitor'}>
          <div class="session-window__pane-empty" data-testid="agent-monitor-stub">
            Agent monitor (F-140).
          </div>
        </Show>
      </div>
    </div>
  );
};

function paneTypeToHeaderLabel(
  paneType: LayoutLeaf['pane_type'],
): 'CHAT' | 'TERMINAL' | 'EDITOR' {
  if (paneType === 'terminal') return 'TERMINAL';
  if (paneType === 'editor') return 'EDITOR';
  return 'CHAT';
}

/**
 * Immutable ratio update: walk `tree`, return a new tree with the matching
 * split's `ratio` replaced. Returns the original reference if `id` isn't a
 * split node in the tree so an unknown id is a safe no-op.
 */
function updateSplitRatio(
  tree: LayoutTree,
  id: string,
  ratio: number,
): LayoutTree {
  if (tree.kind === 'leaf') return tree;
  if (tree.id === id) return { ...tree, ratio };
  const nextA = updateSplitRatio(tree.a, id, ratio);
  const nextB = updateSplitRatio(tree.b, id, ratio);
  if (nextA === tree.a && nextB === tree.b) return tree;
  return { ...tree, a: nextA, b: nextB };
}
