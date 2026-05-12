// F-730: + Add provider modal.
// F-732: same component drives the Edit flow via `mode="edit"` — the kind
// selector locks to the existing kind, the `custom_openai` name field is
// read-only (the id is immutable), and submit calls `update_provider`
// instead of `add_provider`.
//
// Collects the kind selector and — for `custom_openai` — the endpoint /
// model fields, then dispatches the `add_provider` (or `update_provider`)
// IPC (providers-page.md §"Add provider" / §"Edit").
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
// Credentials: for credentialed kinds (anthropic / openai / mistral /
// custom_openai) the form also collects an API key and chains a
// `login_provider` call after `add_provider`/`update_provider` succeeds.
// The Ollama preset auto-fills the local endpoint/model and seeds the
// API key field with the literal `ollama` — Ollama's OpenAI-compat
// endpoint accepts any value, but a fronting proxy may require the
// `Authorization: Bearer ollama` header, so we default it explicitly
// and leave the field editable for non-default tokens. In edit mode the
// API key is optional — blank leaves the existing credential untouched.

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
  BuiltinAuthKind,
  BuiltinProviderConfig,
  CustomOpenAiConfig,
  ProbeProviderConfigInput,
  TestProviderConnectionOutput,
  UpdateProviderInput,
} from '@forge/ipc';
import { invoke } from '../lib/tauri';
import { loginProvider } from '../ipc/credentials';
import { useFocusTrap } from '../lib/useFocusTrap';
import type { ProviderEntry } from '../ipc/dashboard';
import { Dropdown } from './Dropdown';
import './AddProviderForm.css';

export type FormState = 'idle' | 'validating' | 'saving' | 'save-failed';
export type FormMode = 'add' | 'edit';

type BuiltinKind = 'anthropic' | 'openai' | 'mistral';
type Kind = BuiltinKind | 'custom_openai';

const BUILTIN_KINDS: BuiltinKind[] = ['anthropic', 'openai', 'mistral'];
const KINDS: Kind[] = [...BUILTIN_KINDS, 'custom_openai'];
const CUSTOM_NAME_PATTERN = /^[A-Za-z0-9_-]+$/;

/** Kinds for which the Vertex AI auth option is meaningful. Today only
 * Anthropic publishes Claude through Vertex; OpenAI has no equivalent so
 * the auth selector stays hidden for everything else. */
const VERTEX_CAPABLE_KINDS: ReadonlySet<Kind> = new Set<Kind>(['anthropic']);

/** Kinds that require an API key. */
const CREDENTIALED_KINDS: ReadonlySet<Kind> = new Set<Kind>([
  'anthropic',
  'openai',
  'mistral',
  'custom_openai',
]);

/** Preset selector for the `custom_openai` branch. `custom` lets the user
 * fill every field by hand; `ollama` is a one-click shortcut that pins
 * the local Ollama endpoint/model and seeds the API key with the
 * conventional literal `ollama`. */
type CustomPreset = 'custom' | 'ollama';

/** Local Ollama defaults used when the Ollama preset is selected. The
 * model is a sensible default — the user can edit it after selection.
 * The API key defaults to the literal `ollama`: the Ollama OpenAI-compat
 * endpoint accepts any value, but a fronting proxy may require an
 * `Authorization` header, so we always send `Bearer ollama` unless the
 * user overrides it. */
const OLLAMA_PRESET = {
  // Base URL must be the root — both the probe (`/v1/models`) and the
  // runtime (`/v1/chat/completions`) append their own versioned path.
  endpoint: 'http://127.0.0.1:11434',
  model: 'llama3.2',
  apiKey: 'ollama',
} as const;

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
  apiKey?: string;
  vertexProject?: string;
  vertexRegion?: string;
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
  const [apiKey, setApiKey] = createSignal('');
  const [authKind, setAuthKind] = createSignal<BuiltinAuthKind>('api_key');
  const [vertexProject, setVertexProject] = createSignal('');
  const [vertexRegion, setVertexRegion] = createSignal('');
  const [preset, setPreset] = createSignal<CustomPreset>('custom');

  /** "Test connection" probe state for the custom_openai branch. `models`
   * holds the id list returned by the most recent successful probe; when
   * non-empty the MODEL field renders as a dropdown sourced from it. */
  const [probeState, setProbeState] = createSignal<
    | { kind: 'idle' }
    | { kind: 'probing' }
    | { kind: 'success'; latencyMs?: number; count: number }
    | { kind: 'error'; message: string }
  >({ kind: 'idle' });
  const [models, setModels] = createSignal<string[]>([]);

  const mode = (): FormMode => props.mode ?? 'add';
  const isEdit = (): boolean => mode() === 'edit';
  const isCustom = (): boolean => kind() === 'custom_openai';
  const isBusy = (): boolean => state() === 'saving';
  const supportsVertex = (): boolean => !isEdit() && VERTEX_CAPABLE_KINDS.has(kind());
  const isVertex = (): boolean => supportsVertex() && authKind() === 'vertex';
  const needsCredential = (): boolean => {
    // Vertex auth pulls its token from gcloud / ADC at request time —
    // no API key field is shown for that path.
    if (isVertex()) return false;
    return CREDENTIALED_KINDS.has(kind());
  };

  // Reset on reopen so a previous attempt's state doesn't bleed into the
  // next time the modal opens. In edit mode, seed every field from
  // `initialValues` instead of clearing them.
  createEffect(() => {
    if (!props.open) return;
    setState('idle');
    setError(null);
    setFieldErrors({});
    setProbeState({ kind: 'idle' });
    setModels([]);
    if (isEdit() && props.initialValues) {
      const iv = props.initialValues;
      // Edit-mode contract: only `custom_openai:<name>` entries are editable.
      setKind('custom_openai');
      setName(iv.id.startsWith(CUSTOM_PREFIX) ? iv.id.slice(CUSTOM_PREFIX.length) : '');
      setEndpoint(iv.endpoint);
      setModel(iv.model);
      setApiKey('');
      setAuthKind('api_key');
      setVertexProject('');
      setVertexRegion('');
      setPreset('custom');
    } else {
      setKind('anthropic');
      setName('');
      setEndpoint('');
      setModel('');
      setApiKey('');
      setAuthKind('api_key');
      setVertexProject('');
      setVertexRegion('');
      setPreset('custom');
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
    } else {
      // Phase A: built-in kinds accept an optional instance name so the
      // user can hold multiple `<kind>:<name>` configurations
      // side-by-side (e.g. `anthropic:work` + `anthropic:personal`).
      // Blank name → bare vendor id, matching the legacy single-instance
      // shape.
      const n = name().trim();
      if (n !== '' && !CUSTOM_NAME_PATTERN.test(n)) {
        errs.name = 'Name must match [A-Za-z0-9_-]+';
      }
    }
    // API key is required when adding a credentialed provider. In edit
    // mode it's optional — blank means "leave the existing credential
    // untouched"; a value means "overwrite".
    if (!isEdit() && needsCredential() && apiKey().trim() === '') {
      errs.apiKey = 'API key is required';
    }
    if (isVertex()) {
      if (vertexProject().trim() === '') {
        errs.vertexProject = 'GCP project is required for Vertex AI';
      }
      if (vertexRegion().trim() === '') {
        errs.vertexRegion = 'Region is required for Vertex AI';
      }
    }
    return errs;
  };

  const buildBuiltinConfig = (): BuiltinProviderConfig | undefined => {
    if (!isVertex()) return undefined;
    return {
      auth_kind: 'vertex',
      vertex_project: vertexProject().trim(),
      vertex_region: vertexRegion().trim(),
    };
  };

  const buildCustomConfig = (): CustomOpenAiConfig => ({
    endpoint: endpoint().trim(),
    model: model().trim(),
  });

  /** Apply a preset's defaults to the endpoint / model / api-key fields.
   * Called from the PRESET selector's onChange — switching to `ollama`
   * pins the local endpoint/model and seeds the api-key with the literal
   * `ollama` so the user can submit in one click; switching back to
   * `custom` clears them so the form doesn't carry stale preset values
   * into a fresh entry. */
  const applyPreset = (next: CustomPreset): void => {
    setPreset(next);
    // Any stale probe result is invalidated by switching presets — the
    // new preset points at a different endpoint, so the model list and
    // status banner from the old one are no longer meaningful.
    setProbeState({ kind: 'idle' });
    setModels([]);
    if (next === 'ollama') {
      setEndpoint(OLLAMA_PRESET.endpoint);
      setModel(OLLAMA_PRESET.model);
      setApiKey(OLLAMA_PRESET.apiKey);
    } else {
      setEndpoint('');
      setModel('');
      setApiKey('');
    }
  };

  /** Run an ad-hoc probe against the current form values without saving.
   * Surfaces latency + model count inline on success and populates the
   * MODEL dropdown from the returned id list. Errors render in the same
   * inline banner so the user can correct endpoint / key and retry. */
  const onTestConnection = async (): Promise<void> => {
    if (probeState().kind === 'probing' || isBusy()) return;
    const ep = endpoint().trim();
    if (ep === '') {
      setFieldErrors({ ...fieldErrors(), endpoint: 'Endpoint is required' });
      return;
    }
    const key = apiKey().trim();
    // Mirror the Rust-side debug logs into the webview console so a
    // dev hitting `View → Toggle Developer Tools` can see exactly what
    // probe was issued. Never log the raw key — only whether one was
    // supplied — to match the backend redaction policy. Mirror the
    // backend's `/v1` strip so the logged URL matches what the daemon
    // actually requests when the user pasted a `…/v1` endpoint.
    const fullUrl = `${ep.replace(/\/+$/, '').replace(/\/v1$/, '')}/v1/models`;
    // eslint-disable-next-line no-console
    console.debug('[probe_provider_config] →', {
      endpoint: ep,
      url: fullUrl,
      hasKey: key !== '',
    });
    setProbeState({ kind: 'probing' });
    try {
      const input: ProbeProviderConfigInput = {
        endpoint: ep,
        api_key: key === '' ? null : key,
      };
      const out = await invoke<TestProviderConnectionOutput>('probe_provider_config', {
        input,
      });
      const list = out.models ?? [];
      // `latency_ms` is serialized as bigint by ts-rs when the Rust type
      // is u64 — coerce to number for display. Conditionally spread so
      // we don't violate `exactOptionalPropertyTypes` by assigning
      // `latencyMs: undefined` when the field is meant to be absent.
      const latency = out.latency_ms == null ? undefined : Number(out.latency_ms);
      // eslint-disable-next-line no-console
      console.debug('[probe_provider_config] ←', {
        url: fullUrl,
        ok: out.ok,
        latencyMs: latency,
        modelCount: list.length,
        modelsPreview: list.slice(0, 5),
      });
      setModels(list);
      // Pre-select the first returned model when the current value is
      // blank or no longer in the list; otherwise keep the user's pick.
      if (list.length > 0 && !list.includes(model())) {
        setModel(list[0] ?? '');
      }
      setProbeState({
        kind: 'success',
        count: list.length,
        ...(latency !== undefined ? { latencyMs: latency } : {}),
      });
    } catch (err: unknown) {
      const raw = err instanceof Error ? err.message : String(err);
      // eslint-disable-next-line no-console
      console.debug('[probe_provider_config] ✗', { url: fullUrl, error: raw });
      // Strip the canonical IPC prefix for display so the inline banner
      // reads like a normal error.
      const message = raw.replace(/^test_provider_connection:\s*/, '');
      setProbeState({ kind: 'error', message });
    }
  };

  /** Drop back to free-text MODEL input even after a successful probe —
   * lets the user enter a model the endpoint did not advertise. */
  const clearModelList = (): void => {
    setModels([]);
  };

  const buildAddInput = (): AddProviderInput => {
    if (isCustom()) {
      return { id: `custom_openai:${name().trim()}`, config: buildCustomConfig() };
    }
    // Built-in: always emit `<kind>:<name>` so every configuration has a
    // canonical instance identity. Blank name falls back to `default`,
    // matching the Phase-A invariant that built-ins always have a
    // vendor:name combo on the wire.
    const n = name().trim();
    const id = `${kind()}:${n === '' ? 'default' : n}`;
    const builtin = buildBuiltinConfig();
    if (builtin) {
      return { id, builtin };
    }
    return { id };
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
      // For credentialed kinds, persist the API key after the provider
      // is registered. Skip when blank in edit mode (= "leave existing
      // credential alone"). A failure here keeps the registered provider
      // — the user can retry credential entry from the Credentials
      // section without re-adding.
      const trimmedKey = apiKey().trim();
      if (needsCredential() && trimmedKey !== '') {
        await loginProvider(entry.id, trimmedKey);
        // Clear the key from local state immediately per the
        // credentials.ts "never reveal stored value" contract.
        setApiKey('');
      }
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
            <div class="add-provider-form__field">
              <span class="add-provider-form__label">KIND</span>
              <Dropdown
                testid="add-provider-kind"
                ariaLabel="Kind"
                disabled={isBusy() || isEdit()}
                readonly={isEdit()}
                value={kind()}
                onChange={(v) => setKind(v as Kind)}
                options={KINDS.map((k) => ({ value: k, label: k }))}
              />
            </div>

            <Show when={!isCustom() && !isEdit()}>
              {/* Phase A: optional instance name for built-in kinds. Blank
                  → bare vendor id (legacy single-instance). A value lets
                  the user keep multiple `anthropic:<name>` /
                  `openai:<name>` configs side-by-side with independent
                  credentials. */}
              <label class="add-provider-form__field">
                <span class="add-provider-form__label">NAME (optional)</span>
                <input
                  type="text"
                  class="add-provider-form__input"
                  data-testid="add-provider-builtin-name"
                  value={name()}
                  disabled={isBusy()}
                  autocomplete="off"
                  spellcheck={false}
                  placeholder="default"
                  onInput={(e) => setName(e.currentTarget.value)}
                />
                <Show when={fieldErrors().name}>
                  {(msg) => (
                    <span
                      class="add-provider-form__field-error"
                      data-testid="add-provider-builtin-name-error"
                    >
                      {msg()}
                    </span>
                  )}
                </Show>
                <span class="add-provider-form__hint">
                  Leave blank for the single default instance. Provide a name
                  to register multiple configurations of the same vendor.
                </span>
              </label>
            </Show>

            <Show when={supportsVertex()}>
              {/* Phase B: auth method selector. Vertex AI uses gcloud
                  application-default credentials at request time, so the
                  API-key field is replaced by GCP project + region
                  inputs. */}
              <div class="add-provider-form__field">
                <span class="add-provider-form__label">AUTH METHOD</span>
                <Dropdown
                  testid="add-provider-auth-kind"
                  ariaLabel="Auth method"
                  disabled={isBusy()}
                  value={authKind()}
                  onChange={(v) => setAuthKind(v as BuiltinAuthKind)}
                  options={[
                    { value: 'api_key', label: 'API key' },
                    { value: 'vertex', label: 'Google Vertex AI' },
                  ]}
                />
              </div>
            </Show>

            <Show when={isVertex()}>
              <label class="add-provider-form__field">
                <span class="add-provider-form__label">GCP PROJECT</span>
                <input
                  type="text"
                  class="add-provider-form__input"
                  data-testid="add-provider-vertex-project"
                  value={vertexProject()}
                  disabled={isBusy()}
                  autocomplete="off"
                  spellcheck={false}
                  placeholder="my-gcp-project"
                  onInput={(e) => setVertexProject(e.currentTarget.value)}
                />
                <Show when={fieldErrors().vertexProject}>
                  {(msg) => (
                    <span
                      class="add-provider-form__field-error"
                      data-testid="add-provider-vertex-project-error"
                    >
                      {msg()}
                    </span>
                  )}
                </Show>
              </label>

              <label class="add-provider-form__field">
                <span class="add-provider-form__label">REGION</span>
                <input
                  type="text"
                  class="add-provider-form__input"
                  data-testid="add-provider-vertex-region"
                  value={vertexRegion()}
                  disabled={isBusy()}
                  autocomplete="off"
                  spellcheck={false}
                  placeholder="us-central1"
                  onInput={(e) => setVertexRegion(e.currentTarget.value)}
                />
                <Show when={fieldErrors().vertexRegion}>
                  {(msg) => (
                    <span
                      class="add-provider-form__field-error"
                      data-testid="add-provider-vertex-region-error"
                    >
                      {msg()}
                    </span>
                  )}
                </Show>
                <span class="add-provider-form__hint">
                  Auth uses your gcloud Application Default Credentials. Run{' '}
                  <code>gcloud auth application-default login</code> first if
                  you haven't already.
                </span>
              </label>
            </Show>

            <Show when={isCustom() && !isEdit()}>
              {/* Preset selector — `Custom` is the open-ended OpenAI-
                  compatible flow (vLLM, internal mocks, anything that
                  speaks the OpenAI wire). `Ollama (local)` pins the
                  local Ollama endpoint/model and seeds the API key
                  field with the literal `ollama` so the user can
                  submit in one click. Hidden in edit mode since the
                  preset is fixed at add time. */}
              <div class="add-provider-form__field">
                <span class="add-provider-form__label">PRESET</span>
                <Dropdown
                  testid="add-provider-preset"
                  ariaLabel="Preset"
                  disabled={isBusy()}
                  value={preset()}
                  onChange={(v) => applyPreset(v as CustomPreset)}
                  options={[
                    { value: 'custom', label: 'Custom (OpenAI-compatible)' },
                    { value: 'ollama', label: 'Ollama (local)' },
                  ]}
                />
                <span class="add-provider-form__hint">
                  Ollama runs on http://127.0.0.1:11434 with API key
                  `ollama` — pick this preset for a one-click local
                  setup. Choose Custom for any other OpenAI-compatible
                  endpoint.
                </span>
              </div>
            </Show>

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

              <div class="add-provider-form__field">
                <span class="add-provider-form__label">MODEL</span>
                <Show
                  when={models().length > 0}
                  fallback={
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
                  }
                >
                  <Dropdown
                    testid="add-provider-model"
                    ariaLabel="Model"
                    disabled={isBusy()}
                    value={model()}
                    onChange={(v) => setModel(v)}
                    options={models().map((m) => ({ value: m, label: m }))}
                  />
                  <Button
                    variant="ghost"
                    type="button"
                    data-testid="add-provider-model-clear"
                    disabled={isBusy()}
                    onClick={clearModelList}
                  >
                    Type a custom model name
                  </Button>
                </Show>
                <Show when={fieldErrors().model}>
                  {(msg) => (
                    <span class="add-provider-form__field-error" data-testid="add-provider-model-error">
                      {msg()}
                    </span>
                  )}
                </Show>
              </div>

            </Show>

            <Show when={needsCredential()}>
              <label class="add-provider-form__field">
                <span class="add-provider-form__label">
                  API KEY{isEdit() ? ' (leave blank to keep existing)' : ''}
                </span>
                <input
                  type="password"
                  class="add-provider-form__input"
                  data-testid="add-provider-api-key"
                  value={apiKey()}
                  disabled={isBusy()}
                  autocomplete="off"
                  spellcheck={false}
                  placeholder="sk-..."
                  onInput={(e) => setApiKey(e.currentTarget.value)}
                />
                <Show when={fieldErrors().apiKey}>
                  {(msg) => (
                    <span
                      class="add-provider-form__field-error"
                      data-testid="add-provider-api-key-error"
                    >
                      {msg()}
                    </span>
                  )}
                </Show>
                <span class="add-provider-form__hint">
                  Stored in the OS keychain via login_provider; never written to disk.
                </span>
              </label>
            </Show>

            <Show when={isCustom()}>
              {/* Ad-hoc probe driven by the current form values. Hitting
                  `/v1/models` on the provider's base URL verifies
                  reachability + credential before save, and populates
                  the MODEL field above with whatever ids the endpoint
                  advertises. */}
              <div class="add-provider-form__field">
                <Button
                  variant="ghost"
                  type="button"
                  data-testid="add-provider-test-connection"
                  loading={probeState().kind === 'probing'}
                  disabled={probeState().kind === 'probing' || isBusy()}
                  onClick={() => {
                    void onTestConnection();
                  }}
                >
                  {probeState().kind === 'probing' ? 'Testing…' : 'Test connection'}
                </Button>
                <Show when={probeState().kind === 'success'}>
                  {(_) => {
                    const s = probeState();
                    if (s.kind !== 'success') return null;
                    const latencyTxt =
                      s.latencyMs !== undefined ? ` in ${s.latencyMs} ms` : '';
                    const modelsTxt =
                      s.count > 0
                        ? `, ${s.count} model${s.count === 1 ? '' : 's'} found`
                        : ', no models advertised';
                    return (
                      <span
                        class="add-provider-form__hint"
                        data-testid="add-provider-test-success"
                        role="status"
                      >
                        Connected{latencyTxt}
                        {modelsTxt}.
                      </span>
                    );
                  }}
                </Show>
                <Show when={probeState().kind === 'error'}>
                  {(_) => {
                    const s = probeState();
                    if (s.kind !== 'error') return null;
                    return (
                      <span
                        class="add-provider-form__field-error"
                        data-testid="add-provider-test-error"
                        role="alert"
                      >
                        {s.message}
                      </span>
                    );
                  }}
                </Show>
              </div>
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
