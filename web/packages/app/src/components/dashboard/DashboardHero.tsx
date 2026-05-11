import { type Component } from 'solid-js';
import { Button } from '@forge/design';
import './DashboardHero.css';

export interface DashboardHeroProps {
  /** Click handler for the `Attach to session` ghost CTA. F-727 wires the picker. */
  onAttach?: () => void;
  /** Click handler for the `+ New session` primary CTA. F-726 wires the flow. */
  onNewSession?: () => void;
}

/**
 * F-719 dashboard hero. Two-column track per `DESIGN.md §Hero block`:
 * headline + status sentence on the left, dual CTA cluster on the right.
 * The brand word `Forge` paints in `var(--color-ember-400)` via inline
 * `<em>` (font-style normalised in CSS).
 *
 * F-728 will replace the placeholder status sentence with a live
 * workspace-state generator; F-726 / F-727 will wire the CTAs to the
 * new-session and attach flows.
 */
export const DashboardHero: Component<DashboardHeroProps> = (props) => {
  const onAttach = (): void => {
    // TODO(F-727): open the attach picker.
    props.onAttach?.();
  };

  const onNewSession = (): void => {
    // TODO(F-726): open the new-session flow.
    props.onNewSession?.();
  };

  return (
    <header class="dashboard-hero">
      <div class="dashboard-hero__lead">
        <h1 class="dashboard-hero__headline">
          Welcome back.
          <br />
          <em>Forge</em> something.
        </h1>
        <p class="dashboard-hero__status">Status sentence wires in F-728.</p>
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
