import { type Component, createResource, createSignal, For, onMount, Show } from 'solid-js';
import { useSearchParams } from '@solidjs/router';
import { Button, IconButton, Skeleton, StatusPill } from '@forge/design';
import {
  getActiveProvider,
  listProviders,
  setActiveProvider,
  type ProviderEntry,
} from '../../ipc/dashboard';
import { AddProviderForm } from '../../components/AddProviderForm';
import { TestConnectionButton } from '../../components/TestConnectionButton';
import { RemoveProviderButton } from '../../components/RemoveProviderButton';
import { ProviderEnabledToggle } from '../../components/ProviderEnabledToggle';
import { pushToast } from '../../components/toast';
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
  const [activeId, { refetch: refetchActive }] = createResource<string | null>(
    () => getActiveProvider().then((v) => v ?? null),
  );
  // Pending id while the `set_active_provider` IPC is in flight so the
  // row can show a busy state and additional clicks are dropped on the
  // floor — mirrors the dashboard's ProvidersSection pattern.
  const [pendingActive, setPendingActive] = createSignal<string | null>(null);

  const refetchBoth = (): void => {
    void refetch();
    void refetchActive();
  };

  const handleSetActive = (id: string): void => {
    if (pendingActive() !== null) return;
    setPendingActive(id);
    setActiveProvider(id)
      .then(() => {
        refetchBoth();
        pushToast('info', `Active provider set to ${id}`);
      })
      .catch((err) => {
        const detail = err instanceof Error ? err.message : String(err);
        pushToast('error', `set_active_provider failed: ${detail}`);
      })
      .finally(() => setPendingActive(null));
  };

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
                  <ProviderRow
                    provider={provider}
                    active={activeId() === provider.id}
                    pending={pendingActive() === provider.id}
                    disabled={pendingActive() !== null && pendingActive() !== provider.id}
                    onSetActive={handleSetActive}
                    onMutated={refetchBoth}
                  />
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
  active: boolean;
  pending: boolean;
  /** Another row is currently being promoted to active. Disables the
   * Set-active button here so concurrent clicks can't race. */
  disabled: boolean;
  onSetActive: (id: string) => void;
  onMutated?: (() => void) | undefined;
}

type ProviderBrand = 'anthropic' | 'openai' | 'local' | 'custom' | 'unknown';

function providerBrand(id: string): ProviderBrand {
  // Phase A: peel `<vendor>:<name>` so named instances inherit their
  // vendor's brand. `custom_openai:*` is special-cased below.
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

type PillVariant = 'ready' | 'auth';

/**
 * F-755: a row is "not ready" whenever the credential probe didn't
 * return `'present'`. Both `'missing'` (no entry stored) and
 * `'backend_error'` (keyring locked, Secret Service unreachable) flip
 * the pill to `auth`; the distinction surfaces only in the tooltip /
 * remediation copy below.
 */
function credentialNotReady(entry: ProviderEntry): boolean {
  if (!entry.credential_required) return false;
  // Pre-F-755 fixtures may omit `credential_state`. Fall back to the
  // legacy `has_credential` bit so historical payloads keep working.
  const state = entry.credential_state ?? (entry.has_credential ? 'present' : 'missing');
  return state !== 'present';
}

function pillVariant(entry: ProviderEntry): PillVariant {
  // Phase B: Vertex-backed rows resolve auth via gcloud ADC at request
  // time and supply the model per request, so the credential /
  // model-availability heuristics do not apply.
  if (entry.auth_kind === 'vertex') return 'ready';
  if (credentialNotReady(entry)) return 'auth';
  if (!entry.model_available) return 'auth';
  return 'ready';
}

/**
 * Text rendered in the model column. Vertex rows display the auth source
 * (no model is pinned per-instance — it's supplied per request);
 * everything else shows the configured model or empty when no model is
 * pinned. The pill carries state, so we deliberately do NOT echo
 * "ready" / "auth" here.
 */
function modelColumnText(entry: ProviderEntry): string {
  if (entry.auth_kind === 'vertex') return 'gcloud ADC';
  return entry.model ?? '';
}

function rowTooltip(entry: ProviderEntry, variant: PillVariant): string {
  if (entry.auth_kind === 'vertex') {
    return 'Authenticates via gcloud Application Default Credentials. No API key needed — run `gcloud auth application-default login` if requests start failing.';
  }
  if (variant === 'auth') {
    if (entry.credential_required && entry.credential_state === 'backend_error') {
      // F-755: the credential probe errored — typically a locked keyring
      // or a Secret Service backend that isn't running. Pointing the
      // user at the Add-provider modal here is actively wrong (a key
      // may already exist), so the tooltip names the failure and lists
      // OS-specific remediation pointers instead.
      return `${entry.display_name} credential store is unreachable — unlock your OS keyring (Linux: start \`gnome-keyring-daemon\` or check Secret Service; macOS: unlock the login keychain; Windows: sign in to Credential Manager).`;
    }
    if (entry.credential_required && !entry.has_credential) {
      return `${entry.display_name} needs an API key — add one via the Add provider modal.`;
    }
    return `${entry.display_name} is unconfigured.`;
  }
  if (!entry.credential_required) {
    return `${entry.display_name} is keyless — no credential required.`;
  }
  return `${entry.display_name} is ready: credential stored and model available.`;
}

const ProviderRow: Component<ProviderRowProps> = (props) => {
  const brand = () => providerBrand(props.provider.id);
  const variant = () => pillVariant(props.provider);
  const title = () => rowTooltip(props.provider, variant());

  const isEnabled = (): boolean => props.provider.enabled !== false;

  return (
    <li
      // F-738: the dashboard's remediation CTAs route the user to
      // `/providers#<provider-id>`. The browser scrolls the row into view
      // when the fragment matches an element id, so each row carries one
      // keyed off the provider's stable slug.
      id={props.provider.id}
      class="providers-page__row"
      classList={{
        'providers-page__row--disabled': !isEnabled(),
        'providers-page__row--active': props.active,
      }}
      data-brand={brand()}
      data-enabled={isEnabled() ? 'true' : 'false'}
      data-active={props.active ? 'true' : 'false'}
      data-testid="provider-row"
      title={title()}
    >
      <ProviderBrandDot brand={brand()} />
      <ProviderIdentity provider={props.provider} />
      <ProviderModelSummary provider={props.provider} variant={variant()} title={title()} />
      <ActiveSlot
        providerId={props.provider.id}
        active={props.active}
        pending={props.pending}
        disabled={props.disabled}
        onSetActive={props.onSetActive}
      />
      {/* Order: Test → Enabled toggle → Edit (pencil) → Remove (trash).
          Test sits ahead of the toggle so the user can probe before
          flipping enablement, and the destructive Remove stays at the
          far right of the cluster. Edit sits next to Remove so all
          icon-style affordances cluster together. */}
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

interface ModelSummaryProps {
  provider: ProviderEntry;
  variant: PillVariant;
  title: string;
}

const ProviderModelSummary: Component<ModelSummaryProps> = (props) => {
  return (
    <div class="providers-page__summary" data-testid="provider-model-summary">
      <span class="providers-page__model">{modelColumnText(props.provider)}</span>
      <StatusPill
        variant={props.variant}
        class="providers-page__pill"
        title={props.title}
      >
        {props.variant}
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

/**
 * Active-provider selector. Active row renders a non-interactive
 * "ACTIVE" badge; non-active rows render a "SET ACTIVE" button that
 * fires `set_active_provider` via the parent's handler. While any swap
 * is in flight, this button is disabled across every row so concurrent
 * clicks can't race; the pending row's button shows a `loading` state.
 */
function ActiveSlot(props: {
  providerId: string;
  active: boolean;
  pending: boolean;
  disabled: boolean;
  onSetActive: (id: string) => void;
}) {
  return (
    <Show
      when={props.active}
      fallback={
        <Button
          variant="ghost"
          size="sm"
          type="button"
          data-testid={`set-active-${props.providerId}`}
          loading={props.pending}
          disabled={props.disabled || props.pending}
          onClick={() => props.onSetActive(props.providerId)}
        >
          Activate
        </Button>
      }
    >
      <StatusPill
        variant="ready"
        class="providers-page__active-badge"
        data-testid={`active-badge-${props.providerId}`}
        title="Active provider — used for new sessions"
      >
        active
      </StatusPill>
    </Show>
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

// F-732: Edit is meaningful only for `custom_openai:*` rows today —
// built-in providers carry no editable fields yet. The pencil renders
// on every row so the action cluster stays visually consistent; on
// non-editable rows it's `disabled` with a tooltip explaining why.
function EditProviderSlot(props: {
  provider: ProviderEntry;
  onUpdated?: (() => void) | undefined;
}) {
  const isCustom = (): boolean => props.provider.id.startsWith(CUSTOM_PROVIDER_PREFIX);
  const [open, setOpen] = createSignal(false);
  const initialValues = () => ({
    id: props.provider.id,
    endpoint: props.provider.endpoint ?? '',
    model: props.provider.model ?? '',
  });
  const tooltip = (): string =>
    isCustom()
      ? `Edit ${props.provider.id}`
      : `No editable fields for ${props.provider.id} yet`;
  return (
    <>
      <IconButton
        variant="ghost"
        size="sm"
        class="edit-provider__trigger"
        data-testid={`edit-provider-trigger-${props.provider.id}`}
        label={tooltip()}
        disabled={!isCustom()}
        onClick={() => {
          if (isCustom()) setOpen(true);
        }}
        icon={<PencilIcon />}
      />
      <Show when={isCustom()}>
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
    </>
  );
}

/** Inline pencil glyph that matches the trash icon's stroke convention.
 * Sized to the same 16×16 viewport so the row's terminal icon cluster
 * reads as a single visual family. */
const PencilIcon: Component = () => (
  <svg
    class="edit-provider__icon"
    viewBox="0 0 24 24"
    width="16"
    height="16"
    fill="none"
    stroke="currentColor"
    stroke-width="1.7"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
  >
    <path d="M12 20h9" />
    <path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4 12.5-12.5z" />
  </svg>
);

// F-732: destructive Remove with a confirm step per the providers-page
// spec's destructive-action contract.
function RemoveProviderSlot(props: {
  providerId: string;
  onRemoved?: (() => void) | undefined;
}) {
  return <RemoveProviderButton providerId={props.providerId} onRemoved={props.onRemoved} />;
}
