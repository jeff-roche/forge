import { type Component, createResource, createSignal, For, onMount, Show } from 'solid-js';
import { useSearchParams } from '@solidjs/router';
import { Button, Skeleton, StatusPill } from '@forge/design';
import { listProviders, type ProviderEntry } from '../../ipc/dashboard';
import { AddProviderForm } from '../../components/AddProviderForm';
import { TestConnectionButton } from '../../components/TestConnectionButton';
import { RemoveProviderButton } from '../../components/RemoveProviderButton';
import { ProviderEnabledToggle } from '../../components/ProviderEnabledToggle';
import './ProvidersPage.css';

const CUSTOM_PROVIDER_PREFIX = 'custom_openai:';

/**
 * F-729: `/providers` page shell. Renders the list view spec'd in
 * `docs/ui-specs/providers-page.md` with placeholder slots for the
 * four follow-up tasks:
 *   - F-730 swaps `AddProviderButton` (and mounts `<AddProviderForm>`)
 *   - F-731 swaps `TestConnectionSlot`
 *   - F-732 swaps `EditProviderSlot` + `RemoveProviderSlot`
 *   - F-733 swaps `EnabledToggleSlot`
 *
 * The placeholders render visible inert content so each follow-up
 * agent can grep + Edit-swap their slot without conflicting with the
 * others.
 */
export const ProvidersPage: Component = () => {
  const [providers, { refetch }] = createResource<ProviderEntry[]>(listProviders);
  // F-737 follow-up: the Dashboard's "+ Add provider" CTA routes here with
  // `?add=1` so landing the user on this page also pops the modal. The
  // param is cleared on mount so a refresh doesn't reopen the dialog.
  const [searchParams, setSearchParams] = useSearchParams<{ add?: string }>();
  const [addOpen, setAddOpen] = createSignal(false);
  onMount(() => {
    if (searchParams.add === '1') {
      setAddOpen(true);
      setSearchParams({ add: undefined }, { replace: true });
    }
  });

  const errorDetail = (): string | null => {
    const err = providers.error;
    if (!err) return null;
    return err instanceof Error ? err.message : String(err);
  };

  return (
    <main class="providers-page">
      <header class="providers-page__head">
        <h1 class="providers-page__title">Providers</h1>
        <AddProviderButton
          open={addOpen()}
          onOpen={() => setAddOpen(true)}
          onClose={() => setAddOpen(false)}
          onAdded={() => {
            setAddOpen(false);
            void refetch();
          }}
        />
      </header>

      <Show when={providers.loading}>
        <Skeleton
          variant="block"
          count={3}
          label="Loading providers"
          class="providers-page__skeleton"
          data-testid="providers-page-loading"
        />
      </Show>

      <Show when={errorDetail()}>
        {(detail) => (
          <div class="providers-page__error" role="alert" data-testid="providers-page-error">
            <p class="providers-page__error-text">
              Couldn't load providers — {detail()}
            </p>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => void refetch()}
              data-testid="providers-page-retry"
            >
              RETRY
            </Button>
          </div>
        )}
      </Show>

      <Show when={providers.state === 'ready' && providers()}>
        {(rows) => (
          <Show
            when={(rows().length ?? 0) > 0}
            fallback={
              <p class="providers-page__empty" data-testid="providers-page-empty">
                No providers configured. Add one to get started.
              </p>
            }
          >
            <ul class="providers-page__list" data-testid="providers-page-list">
              <For each={rows()}>
                {(provider) => (
                  <ProviderRow provider={provider} onMutated={() => void refetch()} />
                )}
              </For>
            </ul>
          </Show>
        )}
      </Show>
    </main>
  );
};

interface ProviderRowProps {
  provider: ProviderEntry;
  onMutated?: (() => void) | undefined;
}

type ProviderBrand = 'anthropic' | 'openai' | 'local' | 'custom' | 'unknown';

function providerBrand(id: string): ProviderBrand {
  if (id === 'anthropic') return 'anthropic';
  if (id === 'openai') return 'openai';
  if (id === 'ollama' || id === 'lm-studio' || id === 'local') return 'local';
  if (id === 'mistral' || id === 'custom_openai' || id.startsWith('custom_openai:')) return 'custom';
  return 'unknown';
}

const ProviderRow: Component<ProviderRowProps> = (props) => {
  const brand = () => providerBrand(props.provider.id);

  const isEnabled = (): boolean => props.provider.enabled !== false;

  return (
    <li
      // F-738: the dashboard's remediation CTAs route the user to
      // `/providers#<provider-id>`. The browser scrolls the row into view
      // when the fragment matches an element id, so each row carries one
      // keyed off the provider's stable slug.
      id={props.provider.id}
      class="providers-page__row"
      classList={{ 'providers-page__row--disabled': !isEnabled() }}
      data-brand={brand()}
      data-enabled={isEnabled() ? 'true' : 'false'}
      data-testid="provider-row"
    >
      <ProviderBrandDot brand={brand()} />
      <ProviderIdentity provider={props.provider} />
      <ProviderModelSummary provider={props.provider} />
      <TestConnectionSlot providerId={props.provider.id} />
      <EnabledToggleSlot provider={props.provider} onToggled={props.onMutated} />
      <EditProviderSlot provider={props.provider} onUpdated={props.onMutated} />
      <RemoveProviderSlot
        providerId={props.provider.id}
        onRemoved={props.onMutated}
      />
    </li>
  );
};

interface BrandDotProps {
  brand: ProviderBrand;
}

const ProviderBrandDot: Component<BrandDotProps> = (props) => (
  <span
    class="providers-page__dot"
    data-brand={props.brand}
    data-testid="provider-brand-dot"
    aria-hidden="true"
  />
);

interface IdentityProps {
  provider: ProviderEntry;
}

const ProviderIdentity: Component<IdentityProps> = (props) => (
  <div class="providers-page__identity" data-testid="provider-identity">
    <span class="providers-page__id">{props.provider.id}</span>
    <span class="providers-page__display">{props.provider.display_name}</span>
  </div>
);

const ProviderModelSummary: Component<IdentityProps> = (props) => {
  const variant = () => {
    if (props.provider.credential_required && !props.provider.has_credential) return 'auth';
    if (!props.provider.model_available) return 'auth';
    return 'ready';
  };
  return (
    <div class="providers-page__summary" data-testid="provider-model-summary">
      <span class="providers-page__model">{props.provider.model ?? '—'}</span>
      <StatusPill variant={variant()} class="providers-page__pill">
        {variant() === 'ready' ? 'ready' : 'auth'}
      </StatusPill>
    </div>
  );
};

interface AddProviderButtonProps {
  open: boolean;
  onOpen: () => void;
  onClose: () => void;
  onAdded?: () => void;
}

function AddProviderButton(props: AddProviderButtonProps) {
  return (
    <>
      <Button
        variant="primary"
        data-testid="add-provider-button"
        onClick={props.onOpen}
      >
        + Add provider
      </Button>
      <AddProviderForm
        open={props.open}
        onClose={props.onClose}
        onAdded={() => props.onAdded?.()}
      />
    </>
  );
}

function TestConnectionSlot(props: { providerId: string }) {
  return <TestConnectionButton providerId={props.providerId} />;
}

// F-733: per-row Enabled toggle. Disabled providers stay listed so the user
// can re-enable or remove them; the new-session picker filters them out.
function EnabledToggleSlot(props: {
  provider: ProviderEntry;
  onToggled?: (() => void) | undefined;
}) {
  return (
    <ProviderEnabledToggle
      providerId={props.provider.id}
      enabled={props.provider.enabled !== false}
      onToggled={() => props.onToggled?.()}
    />
  );
}

// F-732: Edit is meaningful only for `custom_openai:*` rows — built-in
// providers carry no editable fields today. Built-in rows omit the button
// entirely so the action cluster stays compact.
function EditProviderSlot(props: {
  provider: ProviderEntry;
  onUpdated?: (() => void) | undefined;
}) {
  const isCustom = (): boolean => props.provider.id.startsWith(CUSTOM_PROVIDER_PREFIX);
  const [open, setOpen] = createSignal(false);
  const initialValues = () => {
    const iv: {
      id: string;
      endpoint: string;
      model: string;
      api_version?: string;
    } = {
      id: props.provider.id,
      endpoint: props.provider.endpoint ?? '',
      model: props.provider.model ?? '',
    };
    if (props.provider.api_version !== undefined) {
      iv.api_version = props.provider.api_version;
    }
    return iv;
  };
  return (
    <Show
      when={isCustom()}
      fallback={
        <span
          class="providers-page__slot-placeholder"
          data-testid={`edit-not-applicable-${props.provider.id}`}
          aria-hidden="true"
        />
      }
    >
      <Button
        variant="ghost"
        size="sm"
        data-testid={`edit-provider-trigger-${props.provider.id}`}
        onClick={() => setOpen(true)}
      >
        Edit
      </Button>
      <AddProviderForm
        mode="edit"
        open={open()}
        initialValues={initialValues()}
        onClose={() => setOpen(false)}
        onAdded={() => {
          setOpen(false);
          props.onUpdated?.();
        }}
      />
    </Show>
  );
}

// F-732: destructive Remove with a confirm step per the providers-page
// spec's destructive-action contract.
function RemoveProviderSlot(props: {
  providerId: string;
  onRemoved?: (() => void) | undefined;
}) {
  return <RemoveProviderButton providerId={props.providerId} onRemoved={props.onRemoved} />;
}
