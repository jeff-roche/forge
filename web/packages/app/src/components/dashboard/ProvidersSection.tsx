import { createResource, createSignal, For, Show, type Component } from 'solid-js';
import { A } from '@solidjs/router';
import { Button, Skeleton, StatusPill, type StatusPillVariant } from '@forge/design';
import {
  getActiveProvider,
  isProviderEnabled,
  listProviders,
  providerStatus,
  setActiveProvider,
  type ProviderEntry,
} from '../../ipc/dashboard';
import { useRovingTabindex } from '../../lib/useRovingTabindex';
import { pushToast } from '../toast';
import './ProvidersSection.css';

export type { ProviderEntry };

interface Snapshot {
  entries: ProviderEntry[];
  active: string | null;
  /**
   * F-738: Ollama reachability comes from the dedicated `provider_status` IPC
   * (the only one that probes the running daemon). `dashboard_list_providers`
   * carries no `reachable` field — TODO once a multi-provider reachability
   * IPC lands, key this off the per-row payload instead of a side-channel
   * probe. A probe failure leaves the flag undefined so the START OLLAMA CTA
   * stays hidden rather than firing on a transient IPC error.
   */
  ollamaReachable: boolean | undefined;
}

async function fetchSnapshot(): Promise<Snapshot> {
  const [entries, active, ollamaReachable] = await Promise.all([
    listProviders(),
    getActiveProvider(),
    providerStatus()
      .then((s) => s.reachable)
      .catch(() => undefined),
  ]);
  // F-733: the dashboard's active-provider selector only lists enabled rows
  // so the user cannot promote a disabled provider. Disabled rows still
  // appear on the Providers page so the user can re-enable or remove them.
  return { entries: entries.filter(isProviderEnabled), active, ollamaReachable };
}

/**
 * F-721 Providers card for the Dashboard's v-dash grid (col-4).
 *
 * Compact stacked-list of one row per built-in provider plus any
 * user-configured `[providers.custom_openai.<name>]` entry. Each row
 * leads with a brand-color dot, carries the provider name + model
 * summary (or error subtext), and a trailing readiness pill — `ready`
 * (success) when the provider has a model available, `auth` (warning)
 * when a required credential is missing or the model is unconfigured.
 *
 * Radio semantics are preserved: the wrapping list is a `radiogroup`
 * and each row carries `role="radio"` + `aria-checked`. Clicking a row
 * fires `set_active_provider`; the active row paints the ember pip /
 * border treatment. F-720's `set_active_provider` IPC continues to emit
 * `provider:changed` app-wide so any open session window picks up the
 * change on its next turn.
 *
 * The header carries a `Manage` link to `/providers`. F-729 wires that
 * route; until then the link dead-ends.
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

  const [listRef, setListRef] = createSignal<HTMLDivElement | undefined>();
  useRovingTabindex(listRef, '.providers__row');

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
            <div
              role="radiogroup"
              aria-label="Active provider"
              aria-busy={isSubmitting()}
              class="providers__list"
              ref={setListRef as never}
            >
              <For each={data().entries}>
                {(entry) => (
                  <ProviderRow
                    entry={entry}
                    active={data().active === entry.id}
                    pending={pendingId() === entry.id}
                    disabled={isSubmitting()}
                    ollamaReachable={data().ollamaReachable}
                    onSelect={handleSelect}
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
  pending: boolean;
  disabled: boolean;
  /**
   * F-738: reachability of the local Ollama daemon. Only meaningful for the
   * `ollama` row; other rows ignore it. `undefined` when the probe hasn't
   * resolved or rejected — in that case the START OLLAMA CTA stays hidden.
   */
  ollamaReachable: boolean | undefined;
  onSelect: (id: string) => void;
}

const OLLAMA_SERVE_COMMAND = 'ollama serve';

/**
 * F-738: Best-effort clipboard copy + user-visible toast. `navigator.clipboard`
 * is unavailable in non-secure contexts and some headless test envs; in those
 * cases the toast still carries the literal command so the user can copy it
 * by hand.
 */
async function copyOllamaServeToClipboard(): Promise<void> {
  try {
    await navigator.clipboard?.writeText(OLLAMA_SERVE_COMMAND);
    pushToast('info', `Copied \`${OLLAMA_SERVE_COMMAND}\` — run it in a terminal`);
  } catch {
    pushToast('warning', `Run \`${OLLAMA_SERVE_COMMAND}\` in a terminal`);
  }
}

/**
 * Stable mapping from runtime provider id onto one of the four
 * `--color-provider-*` design tokens. Matches the four-color discipline
 * enforced in `docs/design/ai-patterns.md` and reused by CatalogPane.
 */
type ProviderBrand = 'anthropic' | 'openai' | 'local' | 'custom' | 'unknown';

function providerBrand(id: string): ProviderBrand {
  if (id === 'anthropic') return 'anthropic';
  if (id === 'openai') return 'openai';
  if (id === 'ollama' || id === 'lm-studio' || id === 'local') return 'local';
  if (id === 'mistral' || id === 'custom_openai' || id.startsWith('custom_openai:')) return 'custom';
  return 'unknown';
}

type PillVariant = Extract<StatusPillVariant, 'ready' | 'auth'>;

function pillVariant(entry: ProviderEntry): PillVariant {
  if (entry.credential_required && !entry.has_credential) return 'auth';
  if (!entry.model_available) return 'auth';
  return 'ready';
}

function subtext(entry: ProviderEntry, variant: PillVariant): string {
  if (variant === 'auth') {
    if (entry.credential_required && !entry.has_credential) return 'credentials missing';
    return 'unconfigured';
  }
  return entry.model ?? 'ready';
}

/**
 * F-738: remediation CTA shown beside the row pill when the row's state has
 * an actionable fix. `add-credential` routes the user to the Providers page
 * row for that id; `start-ollama` copies `ollama serve` to the clipboard and
 * dispatches a toast so the user can run it in their terminal. The click
 * bubbles out of the radio row's set-active handler via `stopPropagation`.
 */
function needsCredential(entry: ProviderEntry): boolean {
  return entry.credential_required && !entry.has_credential;
}

function isOllamaDown(entry: ProviderEntry, ollamaReachable: boolean | undefined): boolean {
  return entry.id === 'ollama' && ollamaReachable === false;
}

const ProviderRow: Component<ProviderRowProps> = (props) => {
  const variant = () => pillVariant(props.entry);
  const brand = () => providerBrand(props.entry.id);
  const handleClick = () => {
    if (props.disabled && !props.pending) return;
    props.onSelect(props.entry.id);
  };
  const handleKeyDown = (e: KeyboardEvent) => {
    if (e.key === ' ' || e.key === 'Enter') {
      e.preventDefault();
      handleClick();
    }
  };
  const ariaLabel = () => {
    const parts = [`Select ${props.entry.display_name}`];
    parts.push(variant() === 'ready' ? 'ready' : 'authentication required');
    if (props.pending) parts.push('SWITCHING');
    return parts.join(', ');
  };
  const stopRowSelect = (e: Event) => {
    // CTA buttons live inside the radio row so the spec's compact row layout
    // is preserved. Clicks must not also promote the row to active.
    e.stopPropagation();
  };

  return (
    <div
      role="radio"
      tabindex={-1}
      aria-checked={props.active}
      aria-busy={props.pending}
      aria-disabled={props.disabled && !props.pending}
      aria-label={ariaLabel()}
      class="providers__row"
      classList={{
        'providers__row--active': props.active,
        'providers__row--pending': props.pending,
      }}
      data-brand={brand()}
      onClick={handleClick}
      onKeyDown={handleKeyDown}
    >
      <span class="providers__dot" aria-hidden="true" />
      <div class="providers__identity">
        <span class="providers__name">{props.entry.display_name}</span>
        <span class="providers__sub">{subtext(props.entry, variant())}</span>
      </div>
      <StatusPill class="providers__pill" data-variant={variant()} variant={variant()}>
        {variant()}
      </StatusPill>
      <Show when={needsCredential(props.entry)}>
        <A
          class="providers__cta"
          data-testid={`provider-cta-add-credential-${props.entry.id}`}
          href={`/providers#${props.entry.id}`}
          onClick={stopRowSelect}
          onKeyDown={(e: KeyboardEvent) => {
            if (e.key === ' ' || e.key === 'Enter') e.stopPropagation();
          }}
        >
          Add credential
        </A>
      </Show>
      <Show when={isOllamaDown(props.entry, props.ollamaReachable)}>
        <Button
          variant="primary"
          size="sm"
          class="providers__cta providers__cta--primary"
          data-testid="provider-cta-start-ollama"
          onClick={(e) => {
            stopRowSelect(e);
            void copyOllamaServeToClipboard();
          }}
          onKeyDown={(e) => {
            if (e.key === ' ' || e.key === 'Enter') e.stopPropagation();
          }}
        >
          START OLLAMA
        </Button>
      </Show>
      <Show when={props.active}>
        <span class="providers__active-pip" aria-hidden="true" />
      </Show>
    </div>
  );
};
