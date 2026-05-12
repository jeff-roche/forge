import type { SessionTelemetry } from '../../stores/sessionTelemetry';

/**
 * F-395 / `pane-header.md §PH.4`: format the chat PaneHeader's cost meter.
 *
 * When the session has not yet observed a `UsageTick` (all telemetry fields
 * null), returns the sanctioned em-dash placeholder — never a fabricated
 * `$0.00`. Per the spec the tokens render abbreviated above 1000 (`1.2k`,
 * `34k`); the dollar value uses two decimals.
 *
 * Kept as a pure helper so unit tests can pin the format without a full
 * `render()` roundtrip, and so the SessionWindow call-site is a one-liner.
 */
export function formatCostLabel(t: SessionTelemetry): string {
  if (t.tokensIn === null || t.tokensOut === null || t.costUsd === null) {
    return '—';
  }
  return `in ${formatTokens(t.tokensIn)} · out ${formatTokens(
    t.tokensOut,
  )} · ${formatUsd(t.costUsd)}`;
}

function formatTokens(n: number): string {
  if (n < 1000) return String(n);
  // Abbreviate: 1.2k up to 9.9k, then 10k, 100k, 1.2M, ...
  if (n < 10_000) return `${(n / 1000).toFixed(1)}k`;
  if (n < 1_000_000) return `${Math.round(n / 1000)}k`;
  if (n < 10_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  return `${Math.round(n / 1_000_000)}M`;
}

function formatUsd(n: number): string {
  // Spec §PH.4: "$0.04, not $.04" — two decimals, always leading zero.
  return `$${n.toFixed(2)}`;
}

/**
 * F-395: formats the PaneHeader subject for a chat pane. The spec prescribes
 * `<provider dot> <agent name> · <model>`; Phase-2 IPC doesn't yet carry an
 * agent name, so we fall back to a short session id until an extended
 * `HelloAck` ships (tracked separately). The bare legacy `Session <id>`
 * placeholder is removed outright — the issue explicitly calls it stale.
 */
export function formatChatSubject(
  sessionId: string,
  t: SessionTelemetry,
): string {
  const head = shortSessionId(sessionId);
  if (t.model !== null) {
    return `${head} · ${t.model}`;
  }
  return head;
}

function shortSessionId(id: string): string {
  if (id.length <= 8) return id;
  return id.slice(0, 8);
}

/**
 * F-395: formats the PaneHeader provider pill label. Before the first
 * `AssistantMessage` arrives, falls back to a neutral placeholder — never a
 * `· pending` state suffix, which is not in `voice-terminology.md §8`'s
 * state vocabulary. Once a provider/model pair is observed on the wire, the
 * label promotes to the live pair.
 */
export function formatProviderLabel(t: SessionTelemetry): string {
  if (t.provider === null) return '—';
  return t.provider;
}

/**
 * F-395: resolves the provider-id for accent-color lookup. Mirrors
 * `formatProviderLabel`'s fallback so the pill background and text stay in
 * lockstep before the first assistant turn lands.
 */
export function resolveProviderId(t: SessionTelemetry): string {
  return t.provider ?? 'custom';
}
