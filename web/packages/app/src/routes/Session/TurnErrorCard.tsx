// F-749: inline error card surfaced when a provider call fails mid-turn.
//
// Rendered by ChatPane at the transcript slot where the assistant response
// would have appeared. Visually distinct from assistant messages — no
// provider/model avatar, warning/danger surface tint, monospace error-type
// label — so it cannot be confused with model output.
//
// Action treatment is keyed off `error_kind`:
//
// | kind                 | action                                              |
// |----------------------|-----------------------------------------------------|
// | `auth`               | "Open provider settings" CTA (navigates to /providers) |
// | `rate_limit`         | Retry, disabled with countdown when retry_after_secs known |
// | `network` / `server` / `malformed_response` / `unknown` | Retry, available immediately |
//
// "Show details" disclosure reveals the verbatim `raw` provider error;
// collapsed by default. Raw is never shown in the main copy.

import {
  type Component,
  Show,
  createSignal,
  createMemo,
  onCleanup,
} from 'solid-js';
import { Button } from '@forge/design';
import type { ChatTurn } from '../../stores/messages';

type TurnErrorTurn = Extract<ChatTurn, { type: 'turn_error' }>;
type ErrorKind = TurnErrorTurn['error_kind'];

/**
 * Human-readable label rendered as the monospace error-type chip. Mirrors
 * the Rust `TurnErrorKind` wire tag so a triage chat against the daemon's
 * event log lines up 1:1 with what the user saw.
 */
function errorKindLabel(kind: ErrorKind): string {
  switch (kind) {
    case 'auth':
      return 'auth_error';
    case 'rate_limit':
      return 'rate_limit';
    case 'network':
      return 'network_error';
    case 'server':
      return 'server_error';
    case 'malformed_response':
      return 'malformed_response';
    case 'unknown':
      return 'unknown';
  }
}

export interface TurnErrorCardProps {
  turn: TurnErrorTurn;
  /**
   * Retry callback. Owner (`ChatPane`) re-sends the originating user
   * message text as a new turn. Hidden entirely for `auth` errors.
   */
  onRetry?: () => void;
  /**
   * Navigation callback for the auth CTA. Required for `auth` errors —
   * the parent (`ChatPane`) creates a `useNavigate('/providers')` closure
   * and passes it through. Decoupling the router dep from this component
   * keeps it pure presentation and lets tests mount it without booting
   * the router context.
   */
  onOpenProviderSettings?: () => void;
}

export const TurnErrorCard: Component<TurnErrorCardProps> = (props) => {
  const [detailsOpen, setDetailsOpen] = createSignal(false);

  // Countdown only matters when retry_after_secs is known. Re-derives once
  // per second; cleared on unmount via `onCleanup`. When the countdown hits
  // 0 the Retry button becomes active (parity with the network/server
  // branches).
  const [now, setNow] = createSignal(Date.now());
  const mountedAt = Date.now();
  const intervalId =
    props.turn.error_kind === 'rate_limit' &&
    props.turn.retry_after_secs !== undefined
      ? setInterval(() => setNow(Date.now()), 1000)
      : null;
  onCleanup(() => {
    if (intervalId !== null) clearInterval(intervalId);
  });

  const remainingSecs = createMemo<number | null>(() => {
    if (props.turn.error_kind !== 'rate_limit') return null;
    if (props.turn.retry_after_secs === undefined) return null;
    const elapsed = Math.floor((now() - mountedAt) / 1000);
    const remain = props.turn.retry_after_secs - elapsed;
    return remain > 0 ? remain : 0;
  });

  const isAuth = (): boolean => props.turn.error_kind === 'auth';
  const retryDisabled = createMemo<boolean>(() => {
    if (isAuth()) return true;
    if (!props.turn.retriable) return true;
    const r = remainingSecs();
    return r !== null && r > 0;
  });

  const handleRetry = (): void => {
    if (retryDisabled()) return;
    props.onRetry?.();
  };

  const handleOpenProviderSettings = (): void => {
    props.onOpenProviderSettings?.();
  };

  return (
    <div
      class="turn-error-card"
      data-testid={`turn-error-card-${props.turn.message_id}`}
      data-error-kind={props.turn.error_kind}
      role="alert"
    >
      <header class="turn-error-card__header">
        <span class="turn-error-card__icon" aria-hidden="true">
          !
        </span>
        <span
          class="turn-error-card__kind"
          data-testid={`turn-error-kind-${props.turn.message_id}`}
        >
          {errorKindLabel(props.turn.error_kind)}
        </span>
      </header>
      <p
        class="turn-error-card__message"
        data-testid={`turn-error-message-${props.turn.message_id}`}
      >
        {props.turn.message}
      </p>

      <div class="turn-error-card__actions">
        <Show when={isAuth()}>
          <Button
            variant="primary"
            class="turn-error-card__cta"
            data-testid={`turn-error-open-providers-${props.turn.message_id}`}
            onClick={handleOpenProviderSettings}
          >
            OPEN PROVIDER SETTINGS
          </Button>
        </Show>
        <Show when={!isAuth()}>
          <Button
            variant="primary"
            class="turn-error-card__retry"
            data-testid={`turn-error-retry-${props.turn.message_id}`}
            disabled={retryDisabled()}
            onClick={handleRetry}
          >
            <Show
              when={remainingSecs() !== null && remainingSecs()! > 0}
              fallback={<>RETRY</>}
            >
              RETRY IN {remainingSecs()}s
            </Show>
          </Button>
        </Show>
        <Button
          variant="ghost"
          size="sm"
          class="turn-error-card__details-toggle"
          data-testid={`turn-error-details-toggle-${props.turn.message_id}`}
          aria-expanded={detailsOpen()}
          onClick={() => setDetailsOpen((v) => !v)}
        >
          {detailsOpen() ? 'Hide details' : 'Show details'}
        </Button>
      </div>

      <Show when={detailsOpen() && props.turn.raw !== undefined}>
        <pre
          class="turn-error-card__raw"
          data-testid={`turn-error-raw-${props.turn.message_id}`}
        >
          {props.turn.raw}
        </pre>
      </Show>
      <Show when={detailsOpen() && props.turn.raw === undefined}>
        <p
          class="turn-error-card__raw-empty"
          data-testid={`turn-error-raw-empty-${props.turn.message_id}`}
        >
          No additional provider detail captured.
        </p>
      </Show>
    </div>
  );
};
