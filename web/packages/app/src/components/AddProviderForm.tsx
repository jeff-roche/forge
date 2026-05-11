// F-730: + Add provider modal.
// F-732: same component drives the Edit flow via `mode="edit"` — the kind
// selector locks to the existing kind, the `custom_openai` name field is
// read-only (the id is immutable), and submit calls `update_provider`
// instead of `add_provider`.
//
// Collects the kind selector and — for `custom_openai` — the endpoint /
// model / api_version fields, then dispatches the `add_provider` (or
// `update_provider`) IPC (providers-page.md §"Add provider" / §"Edit").
// On success the modal closes and the parent ProvidersPage refetches.
//
// State machine (per spec §Per-form):
//
//   idle          fields editable, submit gated on validation
//   validating    transient client-side check between submit and IPC
//   saving        IPC in flight; primary button `loading`; fields disabled
//   save-failed   verbatim daemon error rendered with role="alert"; form
//                 re-enabled, state preserved so the operator can correct
//
// Credential entry is intentionally out of scope here — the spec routes it
// through `login_provider` separately. F-731's update flow chains the two.

import {
  type Component,
  createSignal,
  createEffect,
  Match,
  onCleanup,
  onMount,
  Show,
  Switch,
} from 'solid-js';
import { Button } from '@forge/design';
import type {
  AddProviderInput,
  CustomOpenAiConfig,
  UpdateProviderInput,
} from '@forge/ipc';
import { invoke } from '../lib/tauri';
import { useFocusTrap } from '../lib/useFocusTrap';
import type { ProviderEntry } from '../ipc/dashboard';
import './AddProviderForm.css';

export type FormState = 'idle' | 'validating' | 'saving' | 'save-failed';
export type FormMode = 'add' | 'edit';

type BuiltinKind = 'anthropic' | 'openai' | 'ollama' | 'mistral';
type Kind = BuiltinKind | 'custom_openai';

const BUILTIN_KINDS: BuiltinKind[] = ['anthropic', 'openai', 'ollama', 'mistral'];
const KINDS: Kind[] = [...BUILTIN_KINDS, 'custom_openai'];
const CUSTOM_NAME_PATTERN = /^[A-Za-z0-9_-]+$/;

/**
 * Pre-fill payload for `mode = "edit"`. Only `custom_openai:<name>` entries
 * are editable today — the form rejects a built-in `id` upstream by hiding
 * the Edit button at the row level, so this shape models the custom-only
 * branch. `name` is derived from `id` (stripping the `custom_openai:` prefix)
 * and is read-only in the form.
 */
export interface EditInitialValues {
  id: string;
  endpoint: string;
  model: string;
  api_version?: string;
}

export interface AddProviderFormProps {
  open: boolean;
  onClose: () => void;
  /** Fires after `add_provider` / `update_provider` succeeds. */
  onAdded?: (entry: ProviderEntry) => void;
  /** Form mode. Defaults to `'add'`. */
  mode?: FormMode;
  /** Required when `mode === 'edit'`. Drives the initial field state. */
  initialValues?: EditInitialValues;
}

interface FieldErrors {
  name?: string;
  endpoint?: string;
  model?: string;
}

const CUSTOM_PREFIX = 'custom_openai:';

export const AddProviderForm: Component<AddProviderFormProps> = (props) => {
  const [state, setState] = createSignal<FormState>('idle');
  const [error, setError] = createSignal<string | null>(null);
  const [fieldErrors, setFieldErrors] = createSignal<FieldErrors>({});

  const [kind, setKind] = createSignal<Kind>('anthropic');
  const [name, setName] = createSignal('');
  const [endpoint, setEndpoint] = createSignal('');
  const [model, setModel] = createSignal('');
  const [apiVersion, setApiVersion] = createSignal('');

  const mode = (): FormMode => props.mode ?? 'add';
  const isEdit = (): boolean => mode() === 'edit';
  const isCustom = (): boolean => kind() === 'custom_openai';
  const isBusy = (): boolean => state() === 'saving';

  // Reset on reopen so a previous attempt's state doesn't bleed into the
  // next time the modal opens. In edit mode, seed every field from
  // `initialValues` instead of clearing them.
  createEffect(() => {
    if (!props.open) return;
    setState('idle');
    setError(null);
    setFieldErrors({});
    if (isEdit() && props.initialValues) {
      const iv = props.initialValues;
      // Edit-mode contract: only `custom_openai:<name>` entries are editable.
      setKind('custom_openai');
      setName(iv.id.startsWith(CUSTOM_PREFIX) ? iv.id.slice(CUSTOM_PREFIX.length) : '');
      setEndpoint(iv.endpoint);
      setModel(iv.model);
      setApiVersion(iv.api_version ?? '');
    } else {
      setKind('anthropic');
      setName('');
      setEndpoint('');
      setModel('');
      setApiVersion('');
    }
  });

  let dialogRef: HTMLDivElement | undefined;
  useFocusTrap(() => dialogRef, {
    initialFocus: () =>
      dialogRef?.querySelector<HTMLElement>('[data-testid="add-provider-kind"]') ??
      undefined,
  });

  const validate = (): FieldErrors => {
    const errs: FieldErrors = {};
    if (isCustom()) {
      const n = name().trim();
      if (n === '') {
        errs.name = 'Name is required';
      } else if (!CUSTOM_NAME_PATTERN.test(n)) {
        errs.name = 'Name must match [A-Za-z0-9_-]+';
      }
      const ep = endpoint().trim();
      if (ep === '') {
        errs.endpoint = 'Endpoint is required';
      } else {
        try {
          const parsed = new URL(ep);
          if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
            errs.endpoint = 'Endpoint must use http or https';
          }
        } catch {
          errs.endpoint = 'Endpoint is not a valid URL';
        }
      }
      if (model().trim() === '') {
        errs.model = 'Model is required';
      }
    }
    return errs;
  };

  const buildCustomConfig = (): CustomOpenAiConfig => {
    const config: CustomOpenAiConfig = {
      endpoint: endpoint().trim(),
      model: model().trim(),
    };
    const trimmed = apiVersion().trim();
    if (trimmed !== '') {
      config.api_version = trimmed;
    }
    return config;
  };

  const buildAddInput = (): AddProviderInput => {
    if (isCustom()) {
      return { id: `custom_openai:${name().trim()}`, config: buildCustomConfig() };
    }
    return { id: kind() };
  };

  const buildUpdateInput = (): UpdateProviderInput => ({
    id: `custom_openai:${name().trim()}`,
    config: buildCustomConfig(),
  });

  const onSubmit = async (e: Event): Promise<void> => {
    e.preventDefault();
    if (isBusy()) return;
    setError(null);
    setState('validating');
    const errs = validate();
    setFieldErrors(errs);
    if (Object.keys(errs).length > 0) {
      setState('idle');
      return;
    }
    setState('saving');
    try {
      const entry = isEdit()
        ? await invoke<ProviderEntry>('update_provider', { input: buildUpdateInput() })
        : await invoke<ProviderEntry>('add_provider', { input: buildAddInput() });
      props.onAdded?.(entry);
      props.onClose();
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
      setState('save-failed');
    }
  };

  const onBackdropClick = (e: MouseEvent): void => {
    if (e.target !== e.currentTarget) return;
    if (isBusy()) return;
    props.onClose();
  };

  const onKeyDown = (e: KeyboardEvent): void => {
    if (e.key === 'Escape' && !isBusy()) {
      e.preventDefault();
      props.onClose();
    }
  };

  onMount(() => {
    window.addEventListener('keydown', onKeyDown);
  });
  onCleanup(() => {
    window.removeEventListener('keydown', onKeyDown);
  });

  return (
    <Show when={props.open}>
      <div
        class="add-provider-form__backdrop"
        data-testid="add-provider-backdrop"
        onClick={onBackdropClick}
      >
        <div
          ref={dialogRef}
          class="add-provider-form"
          role="dialog"
          aria-modal="true"
          aria-labelledby="add-provider-title"
          aria-busy={isBusy() ? 'true' : 'false'}
          data-testid="add-provider-form"
          data-state={state()}
          id="add-provider-modal"
        >
          <header class="add-provider-form__head">
            <h2 id="add-provider-title" class="add-provider-form__title">
              {isEdit() ? 'EDIT PROVIDER' : 'ADD PROVIDER'}
            </h2>
            <Button
              variant="ghost"
              size="sm"
              type="button"
              class="add-provider-form__close"
              data-testid="add-provider-close"
              aria-label="Close add provider dialog"
              disabled={isBusy()}
              onClick={() => props.onClose()}
            >
              ×
            </Button>
          </header>

          <form class="add-provider-form__form" onSubmit={onSubmit}>
            <label class="add-provider-form__field">
              <span class="add-provider-form__label">KIND</span>
              <select
                class="add-provider-form__input"
                data-testid="add-provider-kind"
                value={kind()}
                disabled={isBusy() || isEdit()}
                aria-readonly={isEdit() ? 'true' : 'false'}
                onChange={(e) => setKind(e.currentTarget.value as Kind)}
              >
                {KINDS.map((k) => (
                  <option value={k}>{k}</option>
                ))}
              </select>
            </label>

            <Show when={isCustom()}>
              <label class="add-provider-form__field">
                <span class="add-provider-form__label">NAME</span>
                <input
                  type="text"
                  class="add-provider-form__input"
                  data-testid="add-provider-name"
                  value={name()}
                  disabled={isBusy() || isEdit()}
                  readonly={isEdit()}
                  aria-readonly={isEdit() ? 'true' : 'false'}
                  autocomplete="off"
                  spellcheck={false}
                  placeholder="vllm-local"
                  onInput={(e) => setName(e.currentTarget.value)}
                />
                <Show when={fieldErrors().name}>
                  {(msg) => (
                    <span class="add-provider-form__field-error" data-testid="add-provider-name-error">
                      {msg()}
                    </span>
                  )}
                </Show>
              </label>

              <label class="add-provider-form__field">
                <span class="add-provider-form__label">ENDPOINT</span>
                <input
                  type="url"
                  class="add-provider-form__input"
                  data-testid="add-provider-endpoint"
                  value={endpoint()}
                  disabled={isBusy()}
                  autocomplete="off"
                  spellcheck={false}
                  placeholder="https://api.example.com"
                  onInput={(e) => setEndpoint(e.currentTarget.value)}
                />
                <Show when={fieldErrors().endpoint}>
                  {(msg) => (
                    <span class="add-provider-form__field-error" data-testid="add-provider-endpoint-error">
                      {msg()}
                    </span>
                  )}
                </Show>
              </label>

              <label class="add-provider-form__field">
                <span class="add-provider-form__label">MODEL</span>
                <input
                  type="text"
                  class="add-provider-form__input"
                  data-testid="add-provider-model"
                  value={model()}
                  disabled={isBusy()}
                  autocomplete="off"
                  spellcheck={false}
                  placeholder="qwen2"
                  onInput={(e) => setModel(e.currentTarget.value)}
                />
                <Show when={fieldErrors().model}>
                  {(msg) => (
                    <span class="add-provider-form__field-error" data-testid="add-provider-model-error">
                      {msg()}
                    </span>
                  )}
                </Show>
              </label>

              <label class="add-provider-form__field">
                <span class="add-provider-form__label">API VERSION (optional)</span>
                <input
                  type="text"
                  class="add-provider-form__input"
                  data-testid="add-provider-api-version"
                  value={apiVersion()}
                  disabled={isBusy()}
                  autocomplete="off"
                  spellcheck={false}
                  onInput={(e) => setApiVersion(e.currentTarget.value)}
                />
              </label>
            </Show>

            <Show when={error() !== null}>
              <div
                class="add-provider-form__error"
                role="alert"
                data-testid="add-provider-error"
              >
                {error()}
              </div>
            </Show>

            <div class="add-provider-form__actions">
              <Button
                variant="ghost"
                type="button"
                data-testid="add-provider-cancel"
                disabled={isBusy()}
                onClick={() => props.onClose()}
              >
                Cancel
              </Button>
              <Button
                variant="primary"
                type="submit"
                data-testid="add-provider-submit"
                loading={isBusy()}
                disabled={isBusy()}
              >
                <Switch fallback={isEdit() ? <>Save</> : <>Add provider</>}>
                  <Match when={state() === 'saving'}>Saving…</Match>
                </Switch>
              </Button>
            </div>
          </form>
        </div>
      </div>
    </Show>
  );
};
