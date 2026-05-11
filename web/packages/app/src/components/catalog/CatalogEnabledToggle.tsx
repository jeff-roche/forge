// F-735: per-row Enabled toggle on the Catalog pane.
//
// Optimistic flip + revert-on-error. Calls `set_setting` with key
// `catalog.enabled.<kind>.<id>` and the next boolean. On rejection the
// visual reverts and the verbatim daemon detail is forwarded to the parent
// via `onError` so the section-level `catalog__action-error` line can
// surface it (per `docs/ui-specs/catalog.md §Enablement toggles`).
//
// Toggle visual matches DESIGN.md §Toggle switch and mirrors the inline
// pattern from F-724's EnabledAssetsCard / F-733's ProviderEnabledToggle.

import { type Component, createSignal } from 'solid-js';
import { setSetting } from '../../stores/settings';
import './CatalogEnabledToggle.css';

export type CatalogToggleKind = 'skills' | 'mcp' | 'agents';

export interface CatalogEnabledToggleProps {
  kind: CatalogToggleKind;
  id: string;
  /** Human-readable row name; drives the inverted aria-label. */
  name: string;
  /** Persisted value sourced by the parent from the settings store. */
  enabled: boolean;
  /** Settings tier the write targets (`user` or `workspace`). */
  level: 'user' | 'workspace';
  /** Workspace root passed through to `set_setting`. */
  workspaceRoot: string;
  /** Fires after `set_setting` resolves successfully. */
  onToggled?: ((enabled: boolean) => void) | undefined;
  /** Fires with the verbatim daemon detail when `set_setting` rejects. */
  onError?: ((detail: string) => void) | undefined;
}

export const CatalogEnabledToggle: Component<CatalogEnabledToggleProps> = (
  props,
) => {
  // Optimistic value; reverts on rejection so the visual matches what's
  // actually persisted.
  const [optimistic, setOptimistic] = createSignal<boolean | null>(null);
  const [pending, setPending] = createSignal(false);

  const current = (): boolean => optimistic() ?? props.enabled;
  const inputId = () => `catalog-toggle-${props.kind}-${props.id}`;

  const handleChange = async (next: boolean): Promise<void> => {
    if (pending()) return;
    setOptimistic(next);
    setPending(true);
    try {
      const key = `catalog.enabled.${props.kind}.${props.id}`;
      await setSetting(key, next, props.level, props.workspaceRoot);
      // Settings store mirror now matches `next`; drop the optimistic
      // override so the parent's `enabled` prop drives the visual again.
      setOptimistic(null);
      props.onToggled?.(next);
    } catch (err: unknown) {
      setOptimistic(!next);
      const detail = err instanceof Error ? err.message : String(err);
      props.onError?.(detail);
    } finally {
      setPending(false);
    }
  };

  return (
    <span
      class="catalog-enabled-toggle"
      data-testid={`catalog-enabled-toggle-${props.kind}-${props.id}`}
    >
      <span
        class="catalog-enabled-toggle__state"
        classList={{
          'catalog-enabled-toggle__state--on': current(),
          'catalog-enabled-toggle__state--off': !current(),
        }}
        aria-hidden="true"
      >
        {current() ? 'enabled' : 'disabled'}
      </span>
      <label
        class="catalog-enabled-toggle__switch"
        for={inputId()}
        aria-disabled={pending() ? 'true' : 'false'}
      >
        <input
          id={inputId()}
          class="catalog-enabled-toggle__input"
          type="checkbox"
          role="switch"
          aria-checked={current()}
          aria-busy={pending() ? 'true' : 'false'}
          aria-label={`${current() ? 'Disable' : 'Enable'} ${props.name}`}
          disabled={pending()}
          checked={current()}
          onChange={(e) => void handleChange(e.currentTarget.checked)}
          data-testid={`catalog-enabled-toggle-input-${props.kind}-${props.id}`}
        />
        <span class="catalog-enabled-toggle__track" aria-hidden="true">
          <span class="catalog-enabled-toggle__thumb" />
        </span>
      </label>
    </span>
  );
};
