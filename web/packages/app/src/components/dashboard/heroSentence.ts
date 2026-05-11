// F-728: synthesize the dashboard hero subtitle from live workspace state.
//
// Pure function — no IPC, no side effects. Inputs are the same data the
// Sessions and Providers cards already consume (`session_list`,
// `dashboard_list_providers`, `has_credential`); the DashboardHero owns
// the wiring and calls this generator on every reactive read.
//
// Sentence shape per `docs/ui-specs/dashboard.md §Hero block`:
//   `<N> sessions active. <M> agent[s] paused awaiting approval.
//    <Provider list> connected — <Provider list> awaiting credentials.`
// Empty case (no sessions AND no providers) collapses to the
// verbatim ticket-DoD copy:
//   `Ready to forge. Add a provider and start a session.`

import type { ProviderEntry, SessionSummary } from '../../ipc/dashboard';

export interface HeroState {
  sessions: SessionSummary[];
  providers: ProviderEntry[];
  /** `provider_id → has_credential`. Missing keys are treated as `false`
   *  (probe failed or not yet resolved). */
  credentialsPresent: Record<string, boolean>;
}

const NUMBER_WORDS: Record<number, string> = {
  1: 'One',
  2: 'Two',
  3: 'Three',
  4: 'Four',
  5: 'Five',
  6: 'Six',
  7: 'Seven',
  8: 'Eight',
  9: 'Nine',
};

/** 1–9 spell out (`One`–`Nine`), 10+ render as digits. Matches the verbatim
 *  spec example (`Two sessions active. One agent paused…`). */
function num(n: number): string {
  return NUMBER_WORDS[n] ?? n.toString();
}

/**
 * Provider names get a small style adjustment for the connected list —
 * the spec calls Ollama out as "local Ollama" so the brand-color story
 * (`var(--p-local)`) reads in copy too. Every other provider uses its
 * `display_name` verbatim.
 */
function connectedLabel(provider: ProviderEntry): string {
  if (provider.id === 'ollama') return 'local Ollama';
  return provider.display_name;
}

function awaitingLabel(provider: ProviderEntry): string {
  return provider.display_name;
}

/** "A", "A and B", "A, B and C", "A, B, C and D"… */
function joinList(items: string[]): string {
  if (items.length === 0) return '';
  if (items.length === 1) return items[0] ?? '';
  if (items.length === 2) return `${items[0]} and ${items[1]}`;
  const head = items.slice(0, -1).join(', ');
  const tail = items[items.length - 1] ?? '';
  return `${head} and ${tail}`;
}

function isActive(session: SessionSummary): boolean {
  return session.state === 'active';
}

/**
 * The wire-level `SessionState` is currently `'active' | 'archived' |
 * 'stopped'` — there's no canonical awaiting-approval value yet (F-740
 * will add it). We accept either the string `'awaiting approval'` or a
 * future enum value via a structural check so the generator is ready the
 * moment the IPC starts carrying it.
 */
function isAwaiting(session: SessionSummary): boolean {
  const s = session.state as unknown as string;
  return s === 'awaiting approval' || s === 'awaiting-approval';
}

function isConnected(provider: ProviderEntry, credentialsPresent: Record<string, boolean>): boolean {
  if (!provider.model_available) return false;
  if (provider.credential_required && !credentialsPresent[provider.id]) return false;
  return true;
}

function isMissingCredentials(
  provider: ProviderEntry,
  credentialsPresent: Record<string, boolean>,
): boolean {
  return provider.credential_required && !credentialsPresent[provider.id];
}

export function heroSentence(state: HeroState): string {
  const { sessions, providers, credentialsPresent } = state;

  const activeSessions = sessions.filter(isActive);
  const awaitingAgents = sessions.filter(isAwaiting);
  const connectedProviders = providers.filter((p) => isConnected(p, credentialsPresent));
  const missingProviders = providers.filter((p) => isMissingCredentials(p, credentialsPresent));

  if (activeSessions.length === 0 && providers.length === 0) {
    return 'Ready to forge. Add a provider and start a session.';
  }

  if (activeSessions.length === 0 && connectedProviders.length === 0) {
    return 'Ready when you are. Start a session above.';
  }

  const parts: string[] = [];

  if (activeSessions.length > 0) {
    const noun = activeSessions.length === 1 ? 'session' : 'sessions';
    parts.push(`${num(activeSessions.length)} ${noun} active.`);
  }

  if (awaitingAgents.length > 0) {
    const noun = awaitingAgents.length === 1 ? 'agent' : 'agents';
    parts.push(`${num(awaitingAgents.length)} ${noun} paused awaiting approval.`);
  }

  if (connectedProviders.length > 0 || missingProviders.length > 0) {
    const connected = joinList(connectedProviders.map(connectedLabel));
    const missing = joinList(missingProviders.map(awaitingLabel));

    if (connected && missing) {
      parts.push(`${connected} connected — ${missing} awaiting credentials.`);
    } else if (connected) {
      parts.push(`${connected} connected.`);
    } else if (missing) {
      parts.push(`${missing} awaiting credentials.`);
    }
  }

  if (parts.length === 0) {
    return 'Ready when you are. Start a session above.';
  }

  return parts.join(' ');
}
