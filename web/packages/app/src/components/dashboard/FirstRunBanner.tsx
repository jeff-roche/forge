import { type Component, Show } from 'solid-js';
import { A } from '@solidjs/router';
import { Button } from '@forge/design';
import './FirstRunBanner.css';

export interface FirstRunBannerProps {
  /** Computed by the parent: `sessions.length === 0 && providers.length === 0`. */
  visible: boolean;
}

/**
 * F-737 first-run banner. Renders ONLY when the workspace has zero sessions
 * AND zero configured providers — a hard "nothing has happened yet" signal
 * the user needs an obvious next action for. The CTA routes to `/providers`
 * so the user can wire a credential and start a session.
 *
 * Once either a session exists or a provider is added, the parent flips
 * `visible` to `false` and the banner returns `null`, ceding the page to
 * the standard hero + card grid.
 */
export const FirstRunBanner: Component<FirstRunBannerProps> = (props) => {
  return (
    <Show when={props.visible}>
      <section
        class="first-run-banner"
        role="region"
        aria-label="Welcome"
        data-testid="first-run-banner"
      >
        <h2 class="first-run-banner__headline">Welcome to Forge.</h2>
        <p class="first-run-banner__subtitle">
          Add your first provider to start a session.
        </p>
        <A class="first-run-banner__cta" href="/providers">
          <Button variant="primary">+ Add provider</Button>
        </A>
      </section>
    </Show>
  );
};
