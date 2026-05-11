// F-727: Attach to session picker.
//
// Modal triggered by the dashboard hero's `Attach to session` ghost CTA.
// Lists every "attachable" (detached) session — the wire-state `stopped`
// rows the daemon process no longer owns — and re-opens the chosen one's
// Session window via the existing `open_session` IPC.
//
// The Rust IPC (`list_sessions`) already filters server-side, so the V1
// picker has no state-chip filter UI; the spec's "state filter" mention
// is satisfied by the source filter.
//
// State machine mirrors the four-state coverage the dashboard uses
// elsewhere (loading / error / empty / ready) — comment-styled empty
// copy per the task spec.

import {
  type Component,
  createResource,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
} from 'solid-js';
import { Button, Skeleton, StatusPill } from '@forge/design';
import {
  listAttachableSessions,
  openSession,
  type SessionSummary,
} from '../ipc/dashboard';
import './AttachSessionPicker.css';

export interface AttachSessionPickerProps {
  open: boolean;
  onClose: () => void;
}

/** Empty-state copy — verbatim per the task spec's comment-styled message. */
export const EMPTY_COPY = '// no detached sessions';

function shortId(id: string): string {
  return id.slice(0, 4);
}

export const AttachSessionPicker: Component<AttachSessionPickerProps> = (
  props,
) => {
  // `open` drives a keyed resource so the IPC re-fires on every open and
  // a stale list never paints when the picker is dismissed mid-flight.
  const openFlag = (): boolean => props.open;
  const [sessions, { refetch }] = createResource(openFlag, async (isOpen) => {
    if (!isOpen) return [] as SessionSummary[];
    return listAttachableSessions();
  });

  const [openError, setOpenError] = createSignal<string | null>(null);

  const rows = (): SessionSummary[] =>
    sessions.state === 'ready' ? sessions() ?? [] : [];

  const loadErrorDetail = (): string | null => {
    const err = sessions.error;
    if (!err) return null;
    return err instanceof Error ? err.message : String(err);
  };

  const handleSelect = (id: string): void => {
    setOpenError(null);
    openSession(id)
      .then(() => props.onClose())
      .catch((err: unknown) => {
        const detail = err instanceof Error ? err.message : String(err);
        setOpenError(`open_session failed: ${detail}`);
      });
  };

  const onBackdropClick = (e: MouseEvent): void => {
    if (e.target !== e.currentTarget) return;
    props.onClose();
  };

  const onKeyDown = (e: KeyboardEvent): void => {
    if (e.key === 'Escape' && props.open) {
      e.preventDefault();
      props.onClose();
    }
  };

  onMount(() => {
    window.addEventListener('keydown', onKeyDown);
  });
  onCleanup(() => {
    window.removeEventListener('keydown', onKeyDown);
  });

  return (
    <Show when={props.open}>
      <div
        class="attach-session-picker__backdrop"
        data-testid="attach-picker-backdrop"
        onClick={onBackdropClick}
      >
        <div
          class="attach-session-picker"
          role="dialog"
          aria-modal="true"
          aria-labelledby="attach-session-title"
          data-testid="attach-session-picker"
        >
          <header class="attach-session-picker__head">
            <h2
              id="attach-session-title"
              class="attach-session-picker__title"
            >
              ATTACH TO SESSION
            </h2>
            <Button
              variant="ghost"
              size="sm"
              class="attach-session-picker__close"
              data-testid="attach-picker-close"
              aria-label="Close attach session picker"
              onClick={() => props.onClose()}
            >
              ×
            </Button>
          </header>

          <div class="attach-session-picker__body">
            <Show when={openError()}>
              {(msg) => (
                <p
                  class="attach-session-picker__error"
                  role="alert"
                  data-testid="attach-picker-open-error"
                >
                  {msg()}
                </p>
              )}
            </Show>

            <Show when={sessions.loading}>
              <Skeleton
                variant="card"
                count={3}
                label="Loading sessions"
                class="attach-session-picker__skeleton"
                data-testid="attach-picker-loading"
              />
            </Show>

            <Show when={loadErrorDetail()}>
              {(detail) => (
                <div
                  class="attach-session-picker__list-error"
                  role="alert"
                  data-testid="attach-picker-load-error"
                >
                  <p class="attach-session-picker__list-error-title">
                    Couldn't load sessions.
                  </p>
                  <p class="attach-session-picker__list-error-detail">
                    {detail()}
                  </p>
                  <Button
                    variant="ghost"
                    size="sm"
                    class="attach-session-picker__retry"
                    onClick={() => void refetch()}
                  >
                    RETRY
                  </Button>
                </div>
              )}
            </Show>

            <Show when={sessions.state === 'ready'}>
              <Show
                when={rows().length > 0}
                fallback={
                  <p
                    class="attach-session-picker__empty"
                    data-testid="attach-picker-empty"
                  >
                    {EMPTY_COPY}
                  </p>
                }
              >
                <ul
                  class="attach-session-picker__list"
                  data-testid="attach-picker-list"
                >
                  <For each={rows()}>
                    {(session) => (
                      <li class="attach-session-picker__row">
                        <Button
                          variant="ghost"
                          class="attach-session-picker__row-button"
                          data-testid={`attach-picker-row-${session.id}`}
                          aria-label={`Attach to ${session.subject}`}
                          onClick={() => handleSelect(session.id)}
                        >
                          <span class="attach-session-picker__row-identity">
                            <span class="attach-session-picker__row-name">
                              {session.subject}
                            </span>
                            <span class="attach-session-picker__row-id">
                              #{shortId(session.id)}
                            </span>
                          </span>
                          <span class="attach-session-picker__row-meta">
                            <Show when={session.provider}>
                              {(p) => (
                                <span class="attach-session-picker__row-provider">
                                  {p()}
                                </span>
                              )}
                            </Show>
                            <StatusPill
                              variant="idle"
                              class="attach-session-picker__row-pill"
                            >
                              detached
                            </StatusPill>
                          </span>
                        </Button>
                      </li>
                    )}
                  </For>
                </ul>
              </Show>
            </Show>
          </div>
        </div>
      </div>
    </Show>
  );
};
