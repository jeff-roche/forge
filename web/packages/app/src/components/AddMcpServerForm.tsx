// F-734: + Add MCP server modal.
//
// Catalog-MCP-tab affordance for declaring a new entry in `.mcp.json`.
// Stdio / http variants are discriminated by the kind radio; the form
// dispatches `add_mcp_server` (F-734) on submit and lets the parent
// catalog refetch on success.
//
// State machine (per docs/ui-specs/catalog.md §Add MCP server modal):
//
//   idle           fields editable, submit gated on validation
//   validating     transient client-side check between submit and IPC
//   saving         IPC in flight; primary button `loading`; fields disabled
//   save-failed    verbatim daemon error rendered with role="alert"; form
//                  re-enabled, state preserved so the operator can correct
//
// Credential entry is intentionally out of scope here — the spec routes
// HTTP auth through the headers list (an `Authorization` row), and there
// is no stdio credential surface today.

import {
  type Component,
  createEffect,
  createSignal,
  For,
  Match,
  onCleanup,
  onMount,
  Show,
  Switch,
} from 'solid-js';
import { Button } from '@forge/design';
import type { AddMcpServerInput, McpServerConfig } from '@forge/ipc';
import { invoke } from '../lib/tauri';
import { useFocusTrap } from '../lib/useFocusTrap';
import './AddMcpServerForm.css';

export type FormState = 'idle' | 'validating' | 'saving' | 'save-failed';
export type ServerKind = 'stdio' | 'http';
export type Scope = 'workspace' | 'user';

const NAME_PATTERN = /^[a-z0-9][a-z0-9_-]*$/;
const HEADER_NAME_PATTERN = /^[A-Za-z0-9_-]+$/;

export interface AddMcpServerFormProps {
  open: boolean;
  /** `workspace` writes `<workspace>/.mcp.json`; `user` writes `~/.mcp.json`. */
  scope: Scope;
  /** Workspace root, required when `scope === 'workspace'`. Ignored otherwise. */
  workspaceRoot?: string | null;
  onClose: () => void;
  /** Fires after `add_mcp_server` succeeds. The parent refetches the catalog. */
  onAdded?: () => void;
}

interface KeyValueRow {
  name: string;
  value: string;
}

interface FieldErrors {
  name?: string;
  command?: string;
  url?: string;
  headers?: string;
  env?: string;
}

export const AddMcpServerForm: Component<AddMcpServerFormProps> = (props) => {
  const [state, setState] = createSignal<FormState>('idle');
  const [error, setError] = createSignal<string | null>(null);
  const [fieldErrors, setFieldErrors] = createSignal<FieldErrors>({});

  const [name, setName] = createSignal('');
  const [kind, setKind] = createSignal<ServerKind>('stdio');
  const [command, setCommand] = createSignal('');
  const [args, setArgs] = createSignal<string[]>(['']);
  const [envRows, setEnvRows] = createSignal<KeyValueRow[]>([]);
  const [url, setUrl] = createSignal('');
  const [headerRows, setHeaderRows] = createSignal<KeyValueRow[]>([]);

  const isBusy = (): boolean => state() === 'saving';
  const isStdio = (): boolean => kind() === 'stdio';

  // Reset on reopen so a previous attempt's state doesn't bleed into the
  // next time the modal opens.
  createEffect(() => {
    if (!props.open) return;
    setState('idle');
    setError(null);
    setFieldErrors({});
    setName('');
    setKind('stdio');
    setCommand('');
    setArgs(['']);
    setEnvRows([]);
    setUrl('');
    setHeaderRows([]);
  });

  let dialogRef: HTMLDivElement | undefined;
  useFocusTrap(() => dialogRef, {
    initialFocus: () =>
      dialogRef?.querySelector<HTMLElement>('[data-testid="add-mcp-name"]') ??
      undefined,
  });

  const validate = (): FieldErrors => {
    const errs: FieldErrors = {};
    const n = name().trim();
    if (n === '') {
      errs.name = 'Name is required';
    } else if (!NAME_PATTERN.test(n)) {
      errs.name = 'Name must match [a-z0-9][a-z0-9_-]*';
    }

    if (isStdio()) {
      if (command().trim() === '') {
        errs.command = 'Command is required';
      }
      const envSeen = new Set<string>();
      for (const row of envRows()) {
        if (row.name.trim() === '') continue;
        if (envSeen.has(row.name)) {
          errs.env = `Duplicate env var: ${row.name}`;
          break;
        }
        envSeen.add(row.name);
      }
    } else {
      const u = url().trim();
      if (u === '') {
        errs.url = 'URL is required';
      } else {
        try {
          const parsed = new URL(u);
          if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
            errs.url = 'URL must use http or https';
          }
        } catch {
          errs.url = 'URL is not valid';
        }
      }
      const headerSeen = new Set<string>();
      for (const row of headerRows()) {
        if (row.name.trim() === '') continue;
        if (!HEADER_NAME_PATTERN.test(row.name)) {
          errs.headers = `Header name must match [A-Za-z0-9_-]+: ${row.name}`;
          break;
        }
        if (headerSeen.has(row.name)) {
          errs.headers = `Duplicate header: ${row.name}`;
          break;
        }
        headerSeen.add(row.name);
      }
    }
    return errs;
  };

  const buildConfig = (): McpServerConfig => {
    if (isStdio()) {
      const env: Record<string, string> = {};
      for (const row of envRows()) {
        const key = row.name.trim();
        if (key === '') continue;
        env[key] = row.value;
      }
      return {
        kind: 'stdio',
        command: command().trim(),
        args: args()
          .map((a) => a.trim())
          .filter((a) => a !== ''),
        env,
      };
    }
    const headers: Record<string, string> = {};
    for (const row of headerRows()) {
      const key = row.name.trim();
      if (key === '') continue;
      headers[key] = row.value;
    }
    return { kind: 'http', url: url().trim(), headers };
  };

  const buildInput = (): AddMcpServerInput => {
    const workspaceRoot =
      props.scope === 'workspace' ? props.workspaceRoot ?? null : null;
    const input: AddMcpServerInput = {
      name: name().trim(),
      config: buildConfig(),
    };
    if (workspaceRoot !== null && workspaceRoot.trim() !== '') {
      input.workspace_root = workspaceRoot;
    }
    return input;
  };

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
      await invoke<void>('add_mcp_server', { input: buildInput() });
      props.onAdded?.();
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

  const addArg = () => setArgs((rows) => [...rows, '']);
  const removeArg = (idx: number) =>
    setArgs((rows) => rows.filter((_, i) => i !== idx));
  const updateArg = (idx: number, value: string) =>
    setArgs((rows) => rows.map((r, i) => (i === idx ? value : r)));

  const addEnv = () =>
    setEnvRows((rows) => [...rows, { name: '', value: '' }]);
  const removeEnv = (idx: number) =>
    setEnvRows((rows) => rows.filter((_, i) => i !== idx));
  const updateEnv = (idx: number, patch: Partial<KeyValueRow>) =>
    setEnvRows((rows) => rows.map((r, i) => (i === idx ? { ...r, ...patch } : r)));

  const addHeader = () =>
    setHeaderRows((rows) => [...rows, { name: '', value: '' }]);
  const removeHeader = (idx: number) =>
    setHeaderRows((rows) => rows.filter((_, i) => i !== idx));
  const updateHeader = (idx: number, patch: Partial<KeyValueRow>) =>
    setHeaderRows((rows) =>
      rows.map((r, i) => (i === idx ? { ...r, ...patch } : r)),
    );

  return (
    <Show when={props.open}>
      <div
        class="add-mcp-server-form__backdrop"
        data-testid="add-mcp-backdrop"
        onClick={onBackdropClick}
      >
        <div
          ref={dialogRef}
          class="add-mcp-server-form"
          role="dialog"
          aria-modal="true"
          aria-labelledby="add-mcp-title"
          aria-busy={isBusy() ? 'true' : 'false'}
          data-testid="add-mcp-form"
          data-state={state()}
        >
          <header class="add-mcp-server-form__head">
            <h2 id="add-mcp-title" class="add-mcp-server-form__title">
              ADD MCP SERVER
            </h2>
            <Button
              variant="ghost"
              size="sm"
              type="button"
              class="add-mcp-server-form__close"
              data-testid="add-mcp-close"
              aria-label="Close add MCP server dialog"
              disabled={isBusy()}
              onClick={() => props.onClose()}
            >
              ×
            </Button>
          </header>

          <form class="add-mcp-server-form__form" onSubmit={onSubmit}>
            <label class="add-mcp-server-form__field">
              <span class="add-mcp-server-form__label">NAME</span>
              <input
                type="text"
                class="add-mcp-server-form__input"
                data-testid="add-mcp-name"
                value={name()}
                disabled={isBusy()}
                autocomplete="off"
                spellcheck={false}
                placeholder="github"
                onInput={(e) => setName(e.currentTarget.value)}
              />
              <Show when={fieldErrors().name}>
                {(msg) => (
                  <span
                    class="add-mcp-server-form__field-error"
                    data-testid="add-mcp-name-error"
                  >
                    {msg()}
                  </span>
                )}
              </Show>
            </label>

            <fieldset class="add-mcp-server-form__field">
              <legend class="add-mcp-server-form__label">TRANSPORT</legend>
              <div
                class="add-mcp-server-form__radio-group"
                role="radiogroup"
                aria-label="Transport"
              >
                <label class="add-mcp-server-form__radio">
                  <input
                    type="radio"
                    name="add-mcp-kind"
                    value="stdio"
                    data-testid="add-mcp-kind-stdio"
                    checked={kind() === 'stdio'}
                    disabled={isBusy()}
                    onChange={() => setKind('stdio')}
                  />
                  <span>stdio</span>
                </label>
                <label class="add-mcp-server-form__radio">
                  <input
                    type="radio"
                    name="add-mcp-kind"
                    value="http"
                    data-testid="add-mcp-kind-http"
                    checked={kind() === 'http'}
                    disabled={isBusy()}
                    onChange={() => setKind('http')}
                  />
                  <span>http</span>
                </label>
              </div>
            </fieldset>

            <Show when={isStdio()}>
              <label class="add-mcp-server-form__field">
                <span class="add-mcp-server-form__label">COMMAND</span>
                <input
                  type="text"
                  class="add-mcp-server-form__input"
                  data-testid="add-mcp-command"
                  value={command()}
                  disabled={isBusy()}
                  autocomplete="off"
                  spellcheck={false}
                  placeholder="npx"
                  onInput={(e) => setCommand(e.currentTarget.value)}
                />
                <Show when={fieldErrors().command}>
                  {(msg) => (
                    <span
                      class="add-mcp-server-form__field-error"
                      data-testid="add-mcp-command-error"
                    >
                      {msg()}
                    </span>
                  )}
                </Show>
              </label>

              <div class="add-mcp-server-form__field">
                <span class="add-mcp-server-form__label">ARGS</span>
                <div class="add-mcp-server-form__list" data-testid="add-mcp-args">
                  <For each={args()}>
                    {(arg, idx) => (
                      <div class="add-mcp-server-form__arg-row">
                        <input
                          type="text"
                          class="add-mcp-server-form__input"
                          data-testid={`add-mcp-arg-${idx()}`}
                          value={arg}
                          disabled={isBusy()}
                          autocomplete="off"
                          spellcheck={false}
                          placeholder="-y"
                          onInput={(e) => updateArg(idx(), e.currentTarget.value)}
                        />
                        <Button
                          variant="ghost"
                          size="sm"
                          type="button"
                          data-testid={`add-mcp-arg-remove-${idx()}`}
                          aria-label={`Remove arg ${idx() + 1}`}
                          disabled={isBusy()}
                          onClick={() => removeArg(idx())}
                        >
                          ×
                        </Button>
                      </div>
                    )}
                  </For>
                  <Button
                    variant="ghost"
                    size="sm"
                    type="button"
                    class="add-mcp-server-form__add-row"
                    data-testid="add-mcp-arg-add"
                    disabled={isBusy()}
                    onClick={addArg}
                  >
                    + add arg
                  </Button>
                </div>
              </div>

              <div class="add-mcp-server-form__field">
                <span class="add-mcp-server-form__label">ENV</span>
                <div class="add-mcp-server-form__list" data-testid="add-mcp-env">
                  <For each={envRows()}>
                    {(row, idx) => (
                      <div class="add-mcp-server-form__kv-row">
                        <input
                          type="text"
                          class="add-mcp-server-form__input"
                          data-testid={`add-mcp-env-name-${idx()}`}
                          value={row.name}
                          disabled={isBusy()}
                          autocomplete="off"
                          spellcheck={false}
                          placeholder="GITHUB_TOKEN"
                          onInput={(e) =>
                            updateEnv(idx(), { name: e.currentTarget.value })
                          }
                        />
                        <input
                          type="text"
                          class="add-mcp-server-form__input"
                          data-testid={`add-mcp-env-value-${idx()}`}
                          value={row.value}
                          disabled={isBusy()}
                          autocomplete="off"
                          spellcheck={false}
                          placeholder="ghp_…"
                          onInput={(e) =>
                            updateEnv(idx(), { value: e.currentTarget.value })
                          }
                        />
                        <Button
                          variant="ghost"
                          size="sm"
                          type="button"
                          data-testid={`add-mcp-env-remove-${idx()}`}
                          aria-label={`Remove env ${idx() + 1}`}
                          disabled={isBusy()}
                          onClick={() => removeEnv(idx())}
                        >
                          ×
                        </Button>
                      </div>
                    )}
                  </For>
                  <Button
                    variant="ghost"
                    size="sm"
                    type="button"
                    class="add-mcp-server-form__add-row"
                    data-testid="add-mcp-env-add"
                    disabled={isBusy()}
                    onClick={addEnv}
                  >
                    + add env
                  </Button>
                </div>
                <Show when={fieldErrors().env}>
                  {(msg) => (
                    <span
                      class="add-mcp-server-form__field-error"
                      data-testid="add-mcp-env-error"
                    >
                      {msg()}
                    </span>
                  )}
                </Show>
              </div>
            </Show>

            <Show when={!isStdio()}>
              <label class="add-mcp-server-form__field">
                <span class="add-mcp-server-form__label">URL</span>
                <input
                  type="url"
                  class="add-mcp-server-form__input"
                  data-testid="add-mcp-url"
                  value={url()}
                  disabled={isBusy()}
                  autocomplete="off"
                  spellcheck={false}
                  placeholder="https://mcp.example.com/api"
                  onInput={(e) => setUrl(e.currentTarget.value)}
                />
                <Show when={fieldErrors().url}>
                  {(msg) => (
                    <span
                      class="add-mcp-server-form__field-error"
                      data-testid="add-mcp-url-error"
                    >
                      {msg()}
                    </span>
                  )}
                </Show>
              </label>

              <div class="add-mcp-server-form__field">
                <span class="add-mcp-server-form__label">HEADERS</span>
                <div
                  class="add-mcp-server-form__list"
                  data-testid="add-mcp-headers"
                >
                  <For each={headerRows()}>
                    {(row, idx) => (
                      <div class="add-mcp-server-form__kv-row">
                        <input
                          type="text"
                          class="add-mcp-server-form__input"
                          data-testid={`add-mcp-header-name-${idx()}`}
                          value={row.name}
                          disabled={isBusy()}
                          autocomplete="off"
                          spellcheck={false}
                          placeholder="Authorization"
                          onInput={(e) =>
                            updateHeader(idx(), { name: e.currentTarget.value })
                          }
                        />
                        <input
                          type="text"
                          class="add-mcp-server-form__input"
                          data-testid={`add-mcp-header-value-${idx()}`}
                          value={row.value}
                          disabled={isBusy()}
                          autocomplete="off"
                          spellcheck={false}
                          placeholder="Bearer …"
                          onInput={(e) =>
                            updateHeader(idx(), { value: e.currentTarget.value })
                          }
                        />
                        <Button
                          variant="ghost"
                          size="sm"
                          type="button"
                          data-testid={`add-mcp-header-remove-${idx()}`}
                          aria-label={`Remove header ${idx() + 1}`}
                          disabled={isBusy()}
                          onClick={() => removeHeader(idx())}
                        >
                          ×
                        </Button>
                      </div>
                    )}
                  </For>
                  <Button
                    variant="ghost"
                    size="sm"
                    type="button"
                    class="add-mcp-server-form__add-row"
                    data-testid="add-mcp-header-add"
                    disabled={isBusy()}
                    onClick={addHeader}
                  >
                    + add header
                  </Button>
                </div>
                <Show when={fieldErrors().headers}>
                  {(msg) => (
                    <span
                      class="add-mcp-server-form__field-error"
                      data-testid="add-mcp-headers-error"
                    >
                      {msg()}
                    </span>
                  )}
                </Show>
              </div>
            </Show>

            <Show when={error() !== null}>
              <div
                class="add-mcp-server-form__error"
                role="alert"
                data-testid="add-mcp-error"
              >
                {error()}
              </div>
            </Show>

            <div class="add-mcp-server-form__actions">
              <Button
                variant="ghost"
                type="button"
                data-testid="add-mcp-cancel"
                disabled={isBusy()}
                onClick={() => props.onClose()}
              >
                Cancel
              </Button>
              <Button
                variant="primary"
                type="submit"
                data-testid="add-mcp-submit"
                loading={isBusy()}
                disabled={isBusy()}
              >
                <Switch fallback={<>Add</>}>
                  <Match when={state() === 'saving'}>Adding…</Match>
                </Switch>
              </Button>
            </div>
          </form>
        </div>
      </div>
    </Show>
  );
};
