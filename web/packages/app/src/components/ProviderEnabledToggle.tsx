// F-733: per-row Enabled toggle on the Providers page.
//
// Optimistic flip + revert-on-error. Calls `set_provider_enabled` with the
// opposite of the incoming `enabled` prop, surfaces the verbatim daemon
// error inline on failure, and notifies the parent so it can refetch.
//
// Toggle visual matches DESIGN.md §Toggle switch and mirrors the inline
// pattern from F-724's EnabledAssetsCard. No shared `<Toggle>` primitive
// exists in `@forge/design` today; extracting one is out of scope here.

import { type Component, createSignal, Show } from 'solid-js';
import type { SetProviderEnabledInput } from '@forge/ipc';
import { invoke } from '../lib/tauri';
import './ProviderEnabledToggle.css';

export interface ProviderEnabledToggleProps {
  providerId: string;
  enabled: boolean;
  /** Fires after `set_provider_enabled` resolves successfully. */
  onToggled?: ((enabled: boolean) => void) | undefined;
}

export const ProviderEnabledToggle: Component<ProviderEnabledToggleProps> = (
  props,
) => {
  // Optimistic value; reverts on rejection so the visual matches what's
  // actually persisted server-side.
  const [optimistic, setOptimistic] = createSignal<boolean | null>(null);
  const [pending, setPending] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const current = (): boolean => optimistic() ?? props.enabled;
  const inputId = () => `provider-enabled-toggle-${props.providerId}`;

  const handleChange = async (next: boolean): Promise<void> => {
    if (pending()) return;
    setError(null);
    setOptimistic(next);
    setPending(true);
    try {
      const input: SetProviderEnabledInput = { id: props.providerId, enabled: next };
      await invoke('set_provider_enabled', { input });
      setOptimistic(null);
      props.onToggled?.(next);
    } catch (err: unknown) {
      // Revert visual on failure; surface the verbatim error so the user
      // can match it against the daemon log.
      setOptimistic(!next);
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setPending(false);
    }
  };

  return (
    <span
      class="provider-enabled-toggle"
      data-testid={`provider-enabled-toggle-${props.providerId}`}
    >
      <span
        class="provider-enabled-toggle__state"
        classList={{
          'provider-enabled-toggle__state--on': current(),
          'provider-enabled-toggle__state--off': !current(),
        }}
        aria-hidden="true"
      >
        {current() ? 'on' : 'off'}
      </span>
      <label
        class="provider-enabled-toggle__switch"
        for={inputId()}
        aria-disabled={pending() ? 'true' : 'false'}
      >
        <input
          id={inputId()}
          class="provider-enabled-toggle__input"
          type="checkbox"
          role="switch"
          aria-checked={current()}
          aria-busy={pending() ? 'true' : 'false'}
          aria-label={`${current() ? 'Disable' : 'Enable'} ${props.providerId}`}
          disabled={pending()}
          checked={current()}
          onChange={(e) => void handleChange(e.currentTarget.checked)}
          data-testid={`provider-enabled-toggle-input-${props.providerId}`}
        />
        <span class="provider-enabled-toggle__track" aria-hidden="true">
          <span class="provider-enabled-toggle__thumb" />
        </span>
      </label>
      <Show when={error()}>
        {(msg) => (
          <span
            class="provider-enabled-toggle__error"
            role="alert"
            data-testid={`provider-enabled-toggle-error-${props.providerId}`}
            title={msg()}
          >
            {msg()}
          </span>
        )}
      </Show>
    </span>
  );
};
