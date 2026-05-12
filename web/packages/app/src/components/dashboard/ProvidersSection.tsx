import { createResource, For, Show, type Component } from 'solid-js';
import { A } from '@solidjs/router';
import { Skeleton, StatusPill, type StatusPillVariant } from '@forge/design';
import {
  getActiveProvider,
  isProviderEnabled,
  listProviders,
  type ProviderEntry,
} from '../../ipc/dashboard';
import './ProvidersSection.css';

export type { ProviderEntry };

interface Snapshot {
  entries: ProviderEntry[];
  active: string | null;
}

async function fetchSnapshot(): Promise<Snapshot> {
  const [entries, active] = await Promise.all([
    listProviders(),
    getActiveProvider(),
  ]);
  // F-733: the dashboard's active-provider selector only lists enabled rows
  // so the user cannot promote a disabled provider. Disabled rows still
  // appear on the Providers page so the user can re-enable or remove them.
  return { entries: entries.filter(isProviderEnabled), active };
}

/**
 * F-721 Providers card for the Dashboard's v-dash grid (col-4).
 *
 * Read-only summary of one row per user-configured provider (built-in
 * vendor + any `[providers.custom_openai.<name>]` entry). Each row
 * leads with a brand-color dot, carries the provider name + model
 * summary (or error subtext), and a trailing readiness pill — `ready`
 * (success) when the provider has a model available, `auth` (warning)
 * when a required credential is missing or the model is unconfigured.
 *
 * Setting the active provider is intentionally NOT available from the
 * Dashboard — users change it through the `/providers` page or by
 * editing `~/.config/forge/settings.toml` directly. The Dashboard card
 * still highlights which provider is currently active (ember pip /
 * border) so the user can see the state at a glance.
 *
 * The header carries a `Manage` link to `/providers` for everything
 * mutating: enable/disable, add/remove, set-active, edit credential.
 */
export const ProvidersSection: Component = () => {
  const [snapshot] = createResource(fetchSnapshot);

  // F-699: the error-detail line MUST carry the verbatim IPC error message
  // so the user can match it against backend logs. Preserve `err.message`
  // unmodified; only the literal `Error: ` prefix is added as a label.
  const errorDetail = () => {
    const err = snapshot.error;
    if (!err) return null;
    return err instanceof Error ? `Error: ${err.message}` : String(err);
  };

  return (
    <section class="providers" aria-label="AI providers">
      <header class="providers__header">
        <span class="providers__label">Providers</span>
        <A class="providers__manage" href="/providers">
          Manage
        </A>
      </header>

      <Show when={snapshot.loading}>
        <Skeleton
          variant="block"
          count={4}
          label="Loading providers"
          class="providers__skeleton"
          data-testid="providers-loading"
        />
      </Show>

      <Show when={errorDetail()}>
        {(detail) => (
          <div class="providers__error" role="alert">
            <p class="providers__error-title">PROVIDERS UNAVAILABLE</p>
            <p class="providers__error-detail">{detail()}</p>
          </div>
        )}
      </Show>

      <Show when={snapshot.state === 'ready' && snapshot()}>
        {(data) => (
          <Show
            when={(data().entries?.length ?? 0) > 0}
            fallback={
              <p class="providers__empty" data-testid="providers-empty">
                // no providers configured
              </p>
            }
          >
            <div class="providers__list" aria-label="Configured providers">
              <For each={data().entries}>
                {(entry) => (
                  <ProviderRow
                    entry={entry}
                    active={data().active === entry.id}
                  />
                )}
              </For>
            </div>
          </Show>
        )}
      </Show>
    </section>
  );
};

interface ProviderRowProps {
  entry: ProviderEntry;
  active: boolean;
}

/**
 * Stable mapping from runtime provider id onto one of the four
 * `--color-provider-*` design tokens. Matches the four-color discipline
 * enforced in `docs/design/ai-patterns.md` and reused by CatalogPane.
 */
type ProviderBrand = 'anthropic' | 'openai' | 'local' | 'custom' | 'unknown';

function providerBrand(id: string): ProviderBrand {
  // Phase A: split the vendor prefix off `<vendor>:<name>` so named
  // instances inherit their vendor's brand color.
  const vendor = id.includes(':') && !id.startsWith('custom_openai:')
    ? id.slice(0, id.indexOf(':'))
    : id;
  if (vendor === 'anthropic') return 'anthropic';
  if (vendor === 'openai') return 'openai';
  if (vendor === 'lm-studio' || vendor === 'local') return 'local';
  if (id === 'mistral' || vendor === 'mistral' || id === 'custom_openai' || id.startsWith('custom_openai:'))
    return 'custom';
  return 'unknown';
}

type PillVariant = Extract<StatusPillVariant, 'ready' | 'auth'>;

function pillVariant(entry: ProviderEntry): PillVariant {
  // Vertex-backed rows are self-contained: gcloud ADC handles auth and
  // the model is supplied per request. Neither the credential nor the
  // model-availability heuristic applies, so the pill is always `ready`
  // — the test-connection probe is the real validator for Vertex.
  if (entry.auth_kind === 'vertex') return 'ready';
  if (entry.credential_required && !entry.has_credential) return 'auth';
  if (!entry.model_available) return 'auth';
  return 'ready';
}

function subtext(entry: ProviderEntry, variant: PillVariant): string {
  if (entry.auth_kind === 'vertex') return 'gcloud ADC';
  if (variant === 'auth') {
    if (entry.credential_required && !entry.has_credential) return 'credentials missing';
    return 'unconfigured';
  }
  return entry.model ?? 'ready';
}

/**
 * Tooltip text for the pill + row. Tells the user *why* the row is in its
 * current state and what action (if any) is required. Surface as a
 * native `title` so the message appears on hover without claiming
 * additional dashboard real estate.
 *
 * `active` flips the message to mention "currently active" so the
 * ember/orange row border (also ember-themed) reads unambiguously as
 * "this is your active provider" rather than as an auth warning.
 */
function pillTooltip(entry: ProviderEntry, variant: PillVariant, active: boolean): string {
  const activePrefix = active ? 'Active provider. ' : '';
  if (entry.auth_kind === 'vertex') {
    return `${activePrefix}Authenticates via gcloud Application Default Credentials. No API key needed — run \`gcloud auth application-default login\` if requests start failing.`;
  }
  if (variant === 'auth') {
    if (entry.credential_required && !entry.has_credential) {
      return `${activePrefix}Click "Add credential" to store an API key for ${entry.display_name}.`;
    }
    return `${activePrefix}${entry.display_name} is unconfigured.`;
  }
  if (!entry.credential_required) {
    return `${activePrefix}${entry.display_name} is keyless — no credential required.`;
  }
  return `${activePrefix}${entry.display_name} is ready: credential stored and model available.`;
}

/**
 * F-738: remediation CTA shown beside the row pill when the row's state has
 * an actionable fix. `add-credential` routes the user to the Providers page
 * row for that id. The click bubbles out of the radio row's set-active
 * handler via `stopPropagation`. Vertex-authenticated rows skip the CTA —
 * gcloud ADC supplies their credential at request time.
 */
function needsCredential(entry: ProviderEntry): boolean {
  if (entry.auth_kind === 'vertex') return false;
  return entry.credential_required && !entry.has_credential;
}

const ProviderRow: Component<ProviderRowProps> = (props) => {
  const variant = () => pillVariant(props.entry);
  const brand = () => providerBrand(props.entry.id);

  return (
    <div
      class="providers__row"
      classList={{ 'providers__row--active': props.active }}
      data-brand={brand()}
      title={pillTooltip(props.entry, variant(), props.active)}
    >
      <span class="providers__dot" aria-hidden="true" />
      <div class="providers__identity">
        <span class="providers__name">{props.entry.display_name}</span>
        <span class="providers__sub">{subtext(props.entry, variant())}</span>
      </div>
      <StatusPill
        class="providers__pill"
        data-variant={variant()}
        variant={variant()}
        title={pillTooltip(props.entry, variant(), props.active)}
      >
        {variant()}
      </StatusPill>
      <Show when={needsCredential(props.entry)}>
        <A
          class="providers__cta"
          data-testid={`provider-cta-add-credential-${props.entry.id}`}
          href={`/providers#${props.entry.id}`}
        >
          Add credential
        </A>
      </Show>
      {/* Active-pip slot is rendered on every row to reserve space and
          prevent the pill/CTA from horizontally shifting when the
          active provider changes. Non-active rows render a muted bar
          (same dimensions); the active row paints it ember and adds the
          glow. */}
      <span
        class="providers__active-pip"
        classList={{ 'providers__active-pip--active': props.active }}
        aria-hidden="true"
        title={
          props.active
            ? 'Active provider — used by new sessions'
            : undefined
        }
      />
    </div>
  );
};
