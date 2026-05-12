import { type Component, createResource } from 'solid-js';
import { Button } from '@forge/design';
import type { ProviderId } from '@forge/ipc';
import { listProviders, sessionList } from '../../ipc/dashboard';
import { hasCredential } from '../../ipc/credentials';
import { heroSentence, type HeroState } from './heroSentence';
import './DashboardHero.css';

/** Adjectives sampled into the hero headline — broad enough that repeat
 * dashboard visits within a session feel fresh rather than canned. */
export const FORGE_ADJECTIVES = [
  'awesome',
  'remarkable',
  'inspiring',
  'legendary',
  'unique',
  'creative',
  'bold',
  'brilliant',
  'ambitious',
  'original',
  'powerful',
  'elegant',
  'daring',
  'fearless',
  'visionary',
  'timeless',
  'relentless',
  'ingenious',
  'radical',
  'audacious',
  'masterful',
  'unstoppable',
  'pioneering',
  'fierce',
  'profound',
  'iconic',
  'spectacular',
  'extraordinary',
  'magnificent',
  'exceptional',
] as const;

export function pickForgeAdjective(): string {
  const idx = Math.floor(Math.random() * FORGE_ADJECTIVES.length);
  return FORGE_ADJECTIVES[idx];
}

export interface DashboardHeroProps {
  /** Click handler for the `Attach to session` ghost CTA. F-727 wires the picker. */
  onAttach?: () => void;
  /** Click handler for the `+ New session` primary CTA. F-726 wires the flow. */
  onNewSession?: () => void;
}

/**
 * F-728: synthesize the hero subtitle from live workspace state.
 *
 * `dashboard_list_providers` enumerates the providers; for the ones
 * that flag `credential_required` we probe `has_credential` to know
 * whether they're connected or awaiting a key. Probe failures are
 * collapsed to `false` so a broken keyring layer mirrors the missing-key
 * UX rather than silently showing the provider as connected.
 */
async function loadHeroState(): Promise<HeroState> {
  const [sessions, providers] = await Promise.all([
    sessionList().catch(() => []),
    listProviders().catch(() => []),
  ]);

  const credentialBearers = providers.filter((p) => p.credential_required);
  const probes = await Promise.all(
    credentialBearers.map(async (p) => {
      try {
        return [p.id, await hasCredential(p.id as ProviderId)] as const;
      } catch {
        return [p.id, false] as const;
      }
    }),
  );
  const credentialsPresent: Record<string, boolean> = {};
  for (const [id, present] of probes) credentialsPresent[id] = present;

  return { sessions, providers, credentialsPresent };
}

/**
 * F-719 dashboard hero. Two-column track per `DESIGN.md §Hero block`:
 * headline + status sentence on the left, dual CTA cluster on the right.
 * The brand word `Forge` paints in `var(--color-ember-400)` via inline
 * `<em>` (font-style normalised in CSS).
 *
 * F-728 replaces the placeholder status sentence with a live generator
 * sourced from `session_list`, `dashboard_list_providers`, and per-provider
 * `has_credential` probes. F-726 / F-727 will wire the CTAs to the
 * new-session and attach flows.
 */
export const DashboardHero: Component<DashboardHeroProps> = (props) => {
  const [heroState] = createResource(loadHeroState);
  // Picked once per mount so the headline stays stable while the user is
  // on the dashboard but rotates on next visit.
  const adjective = pickForgeAdjective();

  const onAttach = (): void => {
    // TODO(F-727): open the attach picker.
    props.onAttach?.();
  };

  const onNewSession = (): void => {
    // TODO(F-726): open the new-session flow.
    props.onNewSession?.();
  };

  const sentence = (): string => {
    const data = heroState();
    if (!data) return 'forge · …';
    return heroSentence(data);
  };

  return (
    <header class="dashboard-hero">
      <div class="dashboard-hero__lead">
        <h1 class="dashboard-hero__headline">
          Welcome back.
          <br />
          <em>Forge</em> something {adjective}.
        </h1>
        <p class="dashboard-hero__status">{sentence()}</p>
      </div>
      <div class="dashboard-hero__cta">
        <Button variant="ghost" onClick={onAttach}>
          Attach to session
        </Button>
        <Button variant="primary" onClick={onNewSession}>
          + New session
        </Button>
      </div>
    </header>
  );
};
