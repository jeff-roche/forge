// F-732: per-row destructive Remove button on the Providers page.
//
// Two-step destructive flow per docs/ui-specs/providers-page.md §"Remove":
//
//   1. The operator clicks `Remove` on a row — opens a focus-trapped
//      confirm dialog asking for explicit acknowledgement.
//   2. The operator clicks `REMOVE` in the dialog — fires `remove_provider`
//      and surfaces the verbatim daemon error on failure.
//
// Cancel / Escape / backdrop-click all close the dialog without
// dispatching the IPC. The button itself stays mounted across the dialog
// lifecycle so the row never loses focus context.
//
// The active-provider safeguard is handled server-side in `remove_provider`
// — when the removed id matches `[providers.active]`, the daemon clears
// the active setting before returning. Callers only need to refetch.

import { type Component, createSignal, onCleanup, onMount, Show } from 'solid-js';
import { Button } from '@forge/design';
import type { RemoveProviderInput } from '@forge/ipc';
import { invoke } from '../lib/tauri';
import { useFocusTrap } from '../lib/useFocusTrap';
import './RemoveProviderButton.css';

export type RemoveButtonState = 'idle' | 'confirming' | 'removing' | 'failed';

export interface RemoveProviderButtonProps {
  providerId: string;
  /** Fires after `remove_provider` resolves successfully. */
  onRemoved?: (() => void) | undefined;
}

export const RemoveProviderButton: Component<RemoveProviderButtonProps> = (
  props,
) => {
  const [state, setState] = createSignal<RemoveButtonState>('idle');
  const [error, setError] = createSignal<string | null>(null);

  const open = (): void => {
    if (state() === 'removing') return;
    setError(null);
    setState('confirming');
  };

  const close = (): void => {
    if (state() === 'removing') return;
    setState('idle');
  };

  const confirm = async (): Promise<void> => {
    if (state() === 'removing') return;
    setState('removing');
    setError(null);
    try {
      const input: RemoveProviderInput = { id: props.providerId };
      await invoke('remove_provider', { input });
      setState('idle');
      props.onRemoved?.();
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
      setState('failed');
    }
  };

  return (
    <>
      <Button
        variant="ghost"
        size="sm"
        data-testid={`remove-provider-trigger-${props.providerId}`}
        onClick={open}
      >
        Remove
      </Button>
      <Show when={state() === 'confirming' || state() === 'removing' || state() === 'failed'}>
        <ConfirmDialog
          providerId={props.providerId}
          pending={state() === 'removing'}
          error={error()}
          onCancel={close}
          onConfirm={() => void confirm()}
        />
      </Show>
    </>
  );
};

interface ConfirmDialogProps {
  providerId: string;
  pending: boolean;
  error: string | null;
  onCancel: () => void;
  onConfirm: () => void;
}

const ConfirmDialog: Component<ConfirmDialogProps> = (props) => {
  let dialogRef: HTMLDivElement | undefined;
  useFocusTrap(() => dialogRef);

  // WAI-ARIA APG Dialog pattern: window-level Escape so focus-trap can't
  // swallow the keystroke when focus jumps via a screen-reader rotor.
  onMount(() => {
    const handler = (e: KeyboardEvent): void => {
      if (e.key === 'Escape' && !props.pending) {
        e.preventDefault();
        props.onCancel();
      }
    };
    window.addEventListener('keydown', handler);
    onCleanup(() => window.removeEventListener('keydown', handler));
  });

  return (
    <div
      class="remove-provider__backdrop"
      data-testid={`remove-provider-confirm-${props.providerId}`}
      onClick={(e) => {
        if (e.target === e.currentTarget && !props.pending) props.onCancel();
      }}
    >
      <div
        ref={dialogRef}
        class="remove-provider__dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="remove-provider-title"
        aria-busy={props.pending ? 'true' : 'false'}
      >
        <header class="remove-provider__head">
          <h3 id="remove-provider-title" class="remove-provider__title">
            REMOVE PROVIDER?
          </h3>
        </header>
        <p class="remove-provider__body">
          Removing <strong>{props.providerId}</strong> deletes its config from
          this workspace and clears its stored credential. Active sessions stay
          on the provider they started with.
        </p>
        <Show when={props.error}>
          {(msg) => (
            <p
              class="remove-provider__error"
              role="alert"
              data-testid={`remove-provider-error-${props.providerId}`}
            >
              {msg()}
            </p>
          )}
        </Show>
        <div class="remove-provider__actions">
          <Button
            variant="ghost"
            size="sm"
            data-testid={`remove-provider-cancel-${props.providerId}`}
            disabled={props.pending}
            onClick={props.onCancel}
          >
            CANCEL
          </Button>
          <Button
            variant="danger"
            size="sm"
            data-testid={`remove-provider-confirm-btn-${props.providerId}`}
            loading={props.pending}
            disabled={props.pending}
            onClick={props.onConfirm}
          >
            REMOVE
          </Button>
        </div>
      </div>
    </div>
  );
};
