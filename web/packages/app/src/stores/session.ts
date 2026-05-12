import { createSignal } from 'solid-js';
import { createStore } from 'solid-js/store';
import type { SessionId, SessionState } from '@forge/ipc';
import { routeLogLineToConsole } from '../ipc/events';
import {
  onSessionEvent,
  type SessionEventPayload,
} from '../ipc/session';
import type { UnlistenFn } from '@tauri-apps/api/event';

export interface SessionSummary {
  id: SessionId;
  state: SessionState;
}

export const [activeSessionId, setActiveSessionId] = createSignal<SessionId | null>(null);

/**
 * F-036: workspace root for the active session, as returned by
 * `session_hello`'s `HelloAck.workspace`. Approval persistence commands
 * (`save_approval`, `remove_approval`, `get_persistent_approvals`) need this
 * to locate `<root>/.forge/approvals.toml`. `null` before hello resolves;
 * components that require it should guard accordingly.
 */
export const [activeWorkspaceRoot, setActiveWorkspaceRoot] =
  createSignal<string | null>(null);

/**
 * F-716: bridge from AppShell's FilesSidebar back to the active session's
 * layout store. SessionWindow publishes its `openFile` callback here while
 * mounted; AppShell reads it to wire the FilesSidebar's `onOpen`. Null when
 * no session is mounted, when called outside SessionWindow's lifecycle, or
 * before its layout store has loaded.
 */
export const [activeOpenFile, setActiveOpenFile] =
  createSignal<((path: string) => void) | null>(null);

export const [sessions, setSessions] = createStore<Record<SessionId, SessionSummary>>({});

/**
 * Per-session view of the most recent event received from the shell bridge.
 * Components that need fine-grained state (messages, tool calls, etc.) will
 * derive from this or dispatch to richer stores. For F-020 the pump's job is
 * simply to prove event delivery works end-to-end.
 */
export interface SessionEventSlot {
  lastSeq: number;
  lastEvent: unknown;
}

export const [sessionEvents, setSessionEvents] = createStore<
  Record<SessionId, SessionEventSlot>
>({});

/**
 * Subscribe the store to the `session:event` Tauri event. Returns the
 * unlisten handle so the caller (app bootstrap) can tear it down on unmount.
 */
export async function initSessionEventPump(): Promise<UnlistenFn> {
  return onSessionEvent((payload: SessionEventPayload) => {
    // Daemon `tracing::warn!`/`error!` records arrive here as
    // `Event::LogLine`. In dev, route them straight to `console.*` and
    // skip the store update — they aren't session UX state, just chrome
    // for the developer console.
    if (routeLogLineToConsole(payload.event)) return;
    setSessionEvents(payload.session_id, {
      lastSeq: payload.seq,
      lastEvent: payload.event,
    });
  });
}

/** Test helper — clears the event slots store between tests. */
export function resetSessionEventStore(): void {
  setSessionEvents({});
}
