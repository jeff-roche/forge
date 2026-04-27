import { createStore, reconcile } from 'solid-js/store';
import type { ProviderId, SessionId } from '@forge/ipc';

/**
 * F-395: per-session provider/model + running usage telemetry.
 *
 * Fed by the IPC adapter as wire events arrive:
 * - `AssistantMessage` → provider + model (the wire event is where each turn's
 *   provider/model is authoritative; the session's "active" provider is the
 *   most recently observed one).
 * - `UsageTick` → tokens_in / tokens_out / cost_usd (running session totals).
 *
 * Components (`PaneHeader`) read the signals through `getSessionTelemetry`.
 * `null` fields mean "not observed yet" — callers render a sanctioned
 * placeholder (`—` for cost; provider id alone for the pill) rather than a
 * fabricated zero. This is the documented placeholder requirement in the
 * F-395 DoD — never render `$0.00` before a real tick has arrived.
 */
export interface SessionTelemetry {
  /** Latest observed provider id. `null` before any `AssistantMessage`. */
  provider: ProviderId | null;
  /** Latest observed model id. `null` before any `AssistantMessage`. */
  model: string | null;
  /** Running session-wide tokens_in. `null` before any `UsageTick`. */
  tokensIn: number | null;
  /** Running session-wide tokens_out. `null` before any `UsageTick`. */
  tokensOut: number | null;
  /** Running session-wide cost in USD. `null` before any `UsageTick`. */
  costUsd: number | null;
}

const EMPTY: SessionTelemetry = {
  provider: null,
  model: null,
  tokensIn: null,
  tokensOut: null,
  costUsd: null,
};

const [telemetryStore, setTelemetryStore] = createStore<
  Record<SessionId, SessionTelemetry>
>({});

function ensure(sessionId: SessionId): void {
  if (!telemetryStore[sessionId]) {
    setTelemetryStore(sessionId, { ...EMPTY });
  }
}

/**
 * Reactive accessor — Solid stores proxy through property access, so calling
 * this inside a `createMemo` / JSX expression tracks the per-field updates.
 * Returns the zero-state record for unknown sessions so callers never crash
 * on first paint before any event has landed.
 */
export function getSessionTelemetry(sessionId: SessionId): SessionTelemetry {
  ensure(sessionId);
  return telemetryStore[sessionId]!;
}

/**
 * Record the provider + model from an `AssistantMessage` event. Idempotent:
 * re-recording the same pair is a no-op on the signal graph so subscribers
 * don't re-render when the model hasn't changed. We deliberately **do not**
 * clear the pair on an unrelated `AssistantMessage` that omits provider/model
 * (older fixtures, pre-F-145 events) — once observed, the live pill should
 * keep showing the most recent identity rather than flicker back to `null`.
 */
export function recordProviderModel(
  sessionId: SessionId,
  provider: ProviderId,
  model: string,
): void {
  ensure(sessionId);
  const current = telemetryStore[sessionId]!;
  if (current.provider !== provider) {
    setTelemetryStore(sessionId, 'provider', provider);
  }
  if (current.model !== model) {
    setTelemetryStore(sessionId, 'model', model);
  }
}

/**
 * Record a `UsageTick` event. The Rust wire shape carries running totals
 * per-scope (`SessionWide`, `PerAgent`) — we track session-wide totals here.
 * Per-agent totals are out of scope for the chat PaneHeader.
 */
export function recordUsageTick(
  sessionId: SessionId,
  tokensIn: number,
  tokensOut: number,
  costUsd: number,
): void {
  ensure(sessionId);
  setTelemetryStore(sessionId, {
    tokensIn,
    tokensOut,
    costUsd,
  });
}

/**
 * Wire-event → telemetry-store router.
 *
 * SessionWindow's adapter listener calls this alongside `pushEvent` so both
 * the messages-store chat log and the PaneHeader's provider/cost pills see
 * every relevant wire event. Unknown / non-telemetry events are no-ops.
 *
 * Handled events:
 * - `assistant_message` — extracts `provider` + `model` (both required on the
 *   Rust wire shape since F-145; earlier replays may omit them, in which case
 *   we keep the last-observed pair rather than clear it).
 * - `usage_tick` — extracts `tokens_in`, `tokens_out`, `cost_usd`.
 *
 * Kept separate from `fromRustEvent` (which returns a `SessionEvent` for the
 * chat message store) so the telemetry path stays a pure side-channel — no
 * risk of a chat-store shape change rippling into the header, and vice versa.
 */
export function routeTelemetryEvent(
  sessionId: SessionId,
  rustEvent: unknown,
): void {
  if (typeof rustEvent !== 'object' || rustEvent === null) return;
  const ev = rustEvent as Record<string, unknown>;
  const type = ev['type'];

  if (type === 'assistant_message') {
    const provider = ev['provider'];
    const model = ev['model'];
    if (typeof provider === 'string' && typeof model === 'string') {
      recordProviderModel(sessionId, provider as ProviderId, model);
    }
    return;
  }

  if (type === 'usage_tick') {
    // F-605: ticks now carry the originating `session_id`. When present,
    // a tick whose id doesn't match this window's session is *another*
    // session bleeding through a shared event stream (a future
    // multiplexed log layout) and must not credit our totals. When
    // absent (legacy logs written before F-605), fall back to the prior
    // behaviour: trust the caller-supplied `sessionId` since the tick
    // was scoped to the per-session window the event was streamed to.
    const tickSessionId = ev['session_id'];
    if (typeof tickSessionId === 'string' && tickSessionId !== sessionId) {
      return;
    }
    const tokensIn = ev['tokens_in'];
    const tokensOut = ev['tokens_out'];
    const costUsd = ev['cost_usd'];
    if (
      typeof tokensIn === 'number' &&
      Number.isFinite(tokensIn) &&
      typeof tokensOut === 'number' &&
      Number.isFinite(tokensOut) &&
      typeof costUsd === 'number' &&
      Number.isFinite(costUsd)
    ) {
      recordUsageTick(sessionId, tokensIn, tokensOut, costUsd);
    }
    return;
  }
}

/** Test helper — clears all telemetry between tests. */
export function resetSessionTelemetryStore(): void {
  setTelemetryStore(reconcile({}));
}
