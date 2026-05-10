import { createResource, createSignal, For, Show, type Component } from 'solid-js';
import { Skeleton, Tab, Tabs } from '@forge/design';
import {
  getActiveProvider,
  listProviders,
  setActiveProvider,
  type ProviderEntry,
} from '../../ipc/dashboard';
import { useRovingTabindex } from '../../lib/useRovingTabindex';
import './ProvidersSection.css';

export type { ProviderEntry };

interface Snapshot {
  entries: ProviderEntry[];
  active: string | null;
}

async function fetchSnapshot(): Promise<Snapshot> {
  const [entries, active] = await Promise.all([listProviders(), getActiveProvider()]);
  return { entries, active };
}

/**
 * F-586 Providers section for the Dashboard.
 *
 * Renders one card per built-in provider plus any user-configured
 * `[providers.custom_openai.<name>]` entry. The active provider is the
 * one whose id matches `[providers.active]`. Clicking a card invokes
 * `set_active_provider` and refetches; the IPC command emits a
 * `provider:changed` Tauri event app-wide so any open session window's
 * orchestrator picks up the change for the next turn.
 *
 * Uses the `@forge/design` Tab primitive in `radio` variant so the cards
 * are a single-select radiogroup with proper ARIA. Roving-tabindex via
 * the existing helper keeps the panel a single Tab stop with internal
 * arrow navigation.
 */
export const ProvidersSection: Component = () => {
  const [snapshot, { refetch }] = createResource(fetchSnapshot);
  const [actionError, setActionError] = createSignal<string | null>(null);
  // F-586 review: prevents the dashboard's double-tap race. Two quick
  // clicks would otherwise fire two `setActiveProvider` IPCs whose
  // refetch() resolutions could land out-of-order and leave the UI in a
  // stale intermediate state. The backend now serializes its
  // read-modify-write through `settings_write_guard`, but we still want
  // the UI to feel locked-in during the round-trip so the user sees a
  // single transition rather than a flicker.
  const [pendingId, setPendingId] = createSignal<string | null>(null);
  const isSubmitting = () => pendingId() !== null;

  const handleSelect = (id: string) => {
    if (isSubmitting()) return;
    setActionError(null);
    setPendingId(id);
    setActiveProvider(id)
      .then(() => refetch())
      .catch((err) => {
        const detail = err instanceof Error ? err.message : String(err);
        setActionError(`set_active_provider failed: ${detail}`);
      })
      .finally(() => setPendingId(null));
  };

  // F-699: the error-detail line MUST carry the verbatim IPC error message
  // so the user can match it against backend logs. Preserve `err.message`
  // unmodified; only the literal `Error: ` prefix is added as a label.
  const errorDetail = () => {
    const err = snapshot.error;
    if (!err) return null;
    return err instanceof Error ? `Error: ${err.message}` : String(err);
  };

  const [gridRef, setGridRef] = createSignal<HTMLDivElement | undefined>();
  // F-699 verification: the `@forge/design` Tab primitive forwards the
  // caller-supplied `class` onto the rendered <button>, so each card carries
  // `forge-tab forge-tab--radio … provider-card` and the `.provider-card`
  // selector below resolves cleanly. No wrapper class needed.
  useRovingTabindex(gridRef, '.provider-card');

  return (
    <section class="providers" aria-label="AI providers">
      <header class="providers__header">
        <span class="providers__label">PROVIDERS</span>
      </header>

      <Show when={snapshot.loading}>
        <Skeleton
          variant="card"
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

      <Show when={actionError()}>
        {(msg) => (
          <p class="providers__action-error" role="alert">
            {msg()}
          </p>
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
            <Tabs
              variant="radio"
              class="providers__grid"
              aria-label="Active provider"
              aria-busy={isSubmitting()}
              ref={setGridRef as never}
            >
              <For each={data().entries}>
                {(entry) => (
                  <ProviderCard
                    entry={entry}
                    active={data().active === entry.id}
                    pending={pendingId() === entry.id}
                    disabled={isSubmitting()}
                    onSelect={handleSelect}
                  />
                )}
              </For>
            </Tabs>
          </Show>
        )}
      </Show>
    </section>
  );
};

interface ProviderCardProps {
  entry: ProviderEntry;
  active: boolean;
  pending: boolean;
  disabled: boolean;
  onSelect: (id: string) => void;
}

const ProviderCard: Component<ProviderCardProps> = (props) => {
  const credentialNeeded = () => props.entry.credential_required && !props.entry.has_credential;
  const ariaLabel = () => {
    const parts = [`Select ${props.entry.display_name}`];
    if (credentialNeeded()) parts.push('CREDENTIAL MISSING');
    if (!props.entry.model_available) parts.push('UNCONFIGURED');
    if (props.pending) parts.push('SWITCHING');
    return parts.join(', ');
  };

  return (
    <Tab
      variant="radio"
      selected={props.active}
      class="provider-card"
      classList={{ 'provider-card--pending': props.pending }}
      aria-label={ariaLabel()}
      aria-busy={props.pending}
      disabled={props.disabled && !props.pending}
      onClick={() => props.onSelect(props.entry.id)}
    >
      <div class="provider-card__body">
        <div class="provider-card__row">
          <span class="provider-card__name">{props.entry.display_name}</span>
          <Show when={props.active}>
            <span class="provider-card__active-pip" aria-hidden="true" />
          </Show>
        </div>
        <div class="provider-card__row provider-card__row--meta">
          <ModelHint entry={props.entry} />
          <CredentialHint entry={props.entry} />
        </div>
      </div>
    </Tab>
  );
};

const ModelHint: Component<{ entry: ProviderEntry }> = (props) => (
  <Show
    when={props.entry.model_available}
    fallback={<span class="provider-card__hint provider-card__hint--missing">unconfigured</span>}
  >
    <span class="provider-card__hint">
      {props.entry.model ?? 'ready'}
    </span>
  </Show>
);

const CredentialHint: Component<{ entry: ProviderEntry }> = (props) => (
  <Show when={props.entry.credential_required}>
    <Show
      when={props.entry.has_credential}
      fallback={
        <span class="provider-card__cred provider-card__cred--missing" aria-label="credential missing">
          ⚠ key
        </span>
      }
    >
      <span class="provider-card__cred provider-card__cred--present" aria-label="credential present">
        ✓ key
      </span>
    </Show>
  </Show>
);
