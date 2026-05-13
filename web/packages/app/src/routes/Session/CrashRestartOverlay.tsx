import type { Component } from 'solid-js';
import { Match, Show, Switch } from 'solid-js';
import { Button } from '@forge/design';
import './CrashRestartOverlay.css';

/**
 * F-748 crash-restart overlay states. The state machine is owned by
 * `SessionWindow`; this component is purely presentational.
 *
 * - `prompting` — daemon death detected, user is offered Restart / Close.
 * - `restarting` — `session_restart` IPC in flight; buttons replaced with
 *   spinner copy.
 * - `restart_failed` — the restart IPC rejected; surface the reason +
 *   retry/close affordance. The "restored" state is modeled as the
 *   overlay being unmounted entirely.
 */
export type CrashOverlayState = 'prompting' | 'restarting' | 'restart_failed';

export interface CrashRestartOverlayProps {
  state: CrashOverlayState;
  onRestart: () => void;
  onClose: () => void;
  /** Failure message to render in the `restart_failed` state. */
  errorMessage?: string | undefined;
}

/**
 * F-748: full-pane overlay rendered on top of `<ChatPane>` when the
 * `forged` daemon dies. Scoped to the chat pane — the transcript stays
 * visible (greyed) underneath so the user can re-read context while
 * deciding what to do. Per the design direction in issue #892:
 *
 * - Tone: technical but reassuring — state the fact, reassure that
 *   no work is lost.
 * - Dismissal: Restart or Close only. Escape does NOT dismiss because
 *   the session is unusable until one of the actions completes.
 * - No silent auto-restart; the user must opt in.
 */
export const CrashRestartOverlay: Component<CrashRestartOverlayProps> = (
  props,
) => {
  // F-748: keep keyboard activation inside the overlay. The chat
  // composer is unreachable (the overlay covers the pane), but a stray
  // Escape would otherwise bubble to the session cancel handler — and
  // we don't want a crashed session to surface as a "cancel" affordance.
  const swallowKeydown = (e: KeyboardEvent) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
    }
  };

  return (
    <div
      class="crash-restart-overlay"
      data-testid="crash-restart-overlay"
      data-state={props.state}
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="crash-restart-overlay-headline"
      aria-describedby="crash-restart-overlay-body"
      onKeyDown={swallowKeydown}
      tabIndex={-1}
    >
      <div class="crash-restart-overlay__card">
        <h2
          class="crash-restart-overlay__headline"
          id="crash-restart-overlay-headline"
          data-testid="crash-restart-overlay-headline"
        >
          Session crashed
        </h2>
        <p
          class="crash-restart-overlay__body"
          id="crash-restart-overlay-body"
          data-testid="crash-restart-overlay-body"
        >
          The session daemon stopped responding. Your messages are
          preserved — restart to resume from where you left off.
        </p>

        <Show when={props.state === 'restart_failed' && props.errorMessage}>
          <p
            class="crash-restart-overlay__error"
            data-testid="crash-restart-overlay-error"
            role="status"
          >
            {props.errorMessage}
          </p>
        </Show>

        <div
          class="crash-restart-overlay__actions"
          data-testid="crash-restart-overlay-actions"
        >
          <Switch>
            <Match when={props.state === 'restarting'}>
              <p
                class="crash-restart-overlay__progress"
                data-testid="crash-restart-overlay-progress"
                aria-live="polite"
              >
                <span
                  class="streaming-cursor"
                  aria-hidden="true"
                />
                Restarting…
              </p>
            </Match>
            <Match when={props.state === 'prompting'}>
              <Button
                variant="primary"
                data-testid="crash-restart-overlay-restart"
                onClick={() => props.onRestart()}
              >
                RESTART SESSION
              </Button>
              <Button
                variant="ghost"
                data-testid="crash-restart-overlay-close"
                onClick={() => props.onClose()}
              >
                CLOSE SESSION
              </Button>
            </Match>
            <Match when={props.state === 'restart_failed'}>
              <Button
                variant="primary"
                data-testid="crash-restart-overlay-retry"
                onClick={() => props.onRestart()}
              >
                RETRY
              </Button>
              <Button
                variant="ghost"
                data-testid="crash-restart-overlay-close"
                onClick={() => props.onClose()}
              >
                CLOSE SESSION
              </Button>
            </Match>
          </Switch>
        </div>
      </div>
    </div>
  );
};
