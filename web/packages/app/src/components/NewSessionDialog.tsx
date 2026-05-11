// F-726: + New session modal.
//
// Collects { workspace_root, provider, agent } and dispatches the
// `session_start` IPC (F-725). On success the daemon allocates a session,
// the modal closes, and the existing `open_session` window-manager flow
// surfaces the freshly-spawned Session window at the v-fresh blank canvas.
//
// Spec: docs/ui-specs/new-session-flow.md.
//
// State machine (per spec §States):
//
//   idle          fields ready, submit gated on workspace_root non-empty
//   validating    transient client-side check between submit and IPC
//   spawning      IPC in flight; primary button `loading`; fields disabled
//   spawn-failed  verbatim daemon error rendered with role="alert"; form
//                 re-enabled, state preserved so the operator can amend
//
// Workspace picker: per spec §Empty-workspace branch the `Browse` action
// dispatches `tauri-plugin-dialog`'s directory picker. The typed field
// remains the source of truth so power-users can paste an absolute path;
// `Browse` overwrites the field on confirm and leaves it untouched on
// cancel (no toast, no error — see spec §Trigger).
//
// `activeWorkspaceRoot()` is a Dashboard-scoped signal that's typically
// null since the Dashboard window has no bound session — kept here so the
// component picks up a cached value if a Session window seeded it first.

import {
  type Component,
  createEffect,
  createMemo,
  createResource,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
} from 'solid-js';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { Button } from '@forge/design';
import type { SessionStartInput, SessionStartOutput } from '@forge/ipc';
import { invoke } from '../lib/tauri';
import {
  getActiveProvider,
  listProviders,
  openSession,
  type ProviderEntry,
} from '../ipc/dashboard';
import { activeWorkspaceRoot } from '../stores/session';
import { useFocusTrap } from '../lib/useFocusTrap';
import './NewSessionDialog.css';

export type DialogState = 'idle' | 'validating' | 'spawning' | 'spawn-failed';

export interface NewSessionDialogProps {
  open: boolean;
  onClose: () => void;
  /** Fires after `session_start` succeeds and `open_session` has been dispatched. */
  onSpawned?: (sessionId: string) => void;
}

const FALLBACK_AGENT = 'orchestrator';

interface ProviderOption {
  id: string;
  label: string;
  enabled: boolean;
}

/**
 * Derive the dropdown shape from the dashboard's provider list. Per spec
 * §Form, entries whose status is `auth` or `error` render disabled with a
 * verbatim status word — for V1 we collapse to a single `enabled` flag:
 * a credential-required provider that hasn't stored a key is `auth`.
 */
function toProviderOptions(entries: ProviderEntry[]): ProviderOption[] {
  // F-733: drop providers whose `enabled` flag is explicitly `false`. Absent
  // flags (legacy payloads) read as enabled; the Providers page is the only
  // writer of the kill switch.
  return entries
    .filter((p) => p.enabled !== false)
    .map((p) => {
      const blocked = p.credential_required && !p.has_credential;
      const suffix = blocked ? ' · auth' : !p.model_available ? ' · no model' : '';
      return {
        id: p.id,
        label: `${p.display_name}${suffix}`,
        enabled: !blocked && p.model_available,
      };
    });
}

/** Pick the default provider id per spec §Fields. */
function pickDefaultProvider(
  options: ProviderOption[],
  active: string | null,
): string | null {
  if (active && options.some((o) => o.id === active && o.enabled)) return active;
  const firstEnabled = options.find((o) => o.enabled);
  return firstEnabled?.id ?? null;
}

export const NewSessionDialog: Component<NewSessionDialogProps> = (props) => {
  const [state, setState] = createSignal<DialogState>('idle');
  const [error, setError] = createSignal<string | null>(null);

  // Form fields.
  const [workspaceRoot, setWorkspaceRoot] = createSignal<string>(
    activeWorkspaceRoot() ?? '',
  );
  const [providerId, setProviderId] = createSignal<string | null>(null);
  const [agentId, setAgentId] = createSignal<string>(FALLBACK_AGENT);

  // Load providers + active provider id once.
  const [providersResource] = createResource(async () => {
    const [list, active] = await Promise.all([
      listProviders().catch(() => [] as ProviderEntry[]),
      getActiveProvider().catch(() => null as string | null),
    ]);
    return { list, active };
  });

  const providerOptions = createMemo<ProviderOption[]>(() => {
    const data = providersResource();
    if (!data) return [];
    return toProviderOptions(data.list);
  });

  const hasEnabledProvider = createMemo(
    () => providerOptions().some((o) => o.enabled),
  );

  // Seed the provider selection once the resource resolves.
  createEffect(() => {
    const data = providersResource();
    if (!data) return;
    if (providerId() !== null) return;
    const def = pickDefaultProvider(toProviderOptions(data.list), data.active);
    setProviderId(def);
  });

  // Reset state when the dialog is reopened.
  createEffect(() => {
    if (props.open) {
      setState('idle');
      setError(null);
      // Pick up a freshly-cached workspace if the field was previously empty.
      if (workspaceRoot() === '') {
        const cached = activeWorkspaceRoot();
        if (cached !== null) setWorkspaceRoot(cached);
      }
    }
  });

  // Focus-trap. Spec §Trigger calls for the workspace field to receive
  // focus on open — the field is the first interactive element either way.
  let dialogRef: HTMLDivElement | undefined;
  useFocusTrap(() => dialogRef, {
    initialFocus: () =>
      dialogRef?.querySelector<HTMLElement>('[data-testid="workspace-input"]') ??
      undefined,
  });

  const onSubmit = async (e: Event): Promise<void> => {
    e.preventDefault();
    if (state() === 'spawning') return;

    setError(null);
    setState('validating');

    const trimmed = workspaceRoot().trim();
    if (trimmed === '') {
      setError('session_start: workspace_root is empty');
      setState('spawn-failed');
      return;
    }

    setState('spawning');

    const input: SessionStartInput = { workspace_root: trimmed };
    const provider = providerId();
    if (provider !== null) input.provider = provider;
    const agent = agentId();
    if (agent !== '') input.agent = agent;

    try {
      const out = await invoke<SessionStartOutput>('session_start', { input });
      try {
        await openSession(out.session_id);
      } catch (err: unknown) {
        // Spawn succeeded but the window-open dispatch failed — surface the
        // verbatim error so the operator sees something concrete; the
        // daemon-spawned session is still live and recoverable from the
        // Sessions panel.
        setError(err instanceof Error ? err.message : String(err));
        setState('spawn-failed');
        return;
      }
      props.onSpawned?.(out.session_id);
      props.onClose();
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
      setState('spawn-failed');
    }
  };

  // Spec §Empty-workspace branch + §Form: the `Browse` action launches
  // `tauri-plugin-dialog`'s directory picker. Cancel is a silent no-op — the
  // typed field is preserved exactly as it was.
  const onBrowse = async (): Promise<void> => {
    if (isBusy()) return;
    try {
      const picked = await openDialog({
        directory: true,
        multiple: false,
        title: 'Pick a workspace for the new session',
      });
      if (typeof picked === 'string' && picked.length > 0) {
        setWorkspaceRoot(picked);
      }
    } catch (err: unknown) {
      // Surface picker failures the same way as daemon spawn errors —
      // verbatim, with the `session_start:` prefix the spec mandates for
      // every error in this modal (spec §IPC contract).
      setError(
        `session_start: workspace picker failed: ${
          err instanceof Error ? err.message : String(err)
        }`,
      );
      setState('spawn-failed');
    }
  };

  const onBackdropClick = (e: MouseEvent): void => {
    if (e.target !== e.currentTarget) return;
    if (state() === 'spawning') return;
    props.onClose();
  };

  const onKeyDown = (e: KeyboardEvent): void => {
    if (e.key === 'Escape' && state() !== 'spawning') {
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

  const isBusy = (): boolean => state() === 'spawning';
  const submitDisabled = (): boolean =>
    isBusy() ||
    workspaceRoot().trim() === '' ||
    !hasEnabledProvider();

  return (
    <Show when={props.open}>
      <div
        class="new-session-dialog__backdrop"
        data-testid="new-session-backdrop"
        onClick={onBackdropClick}
      >
        <div
          ref={dialogRef}
          class="new-session-dialog"
          role="dialog"
          aria-modal="true"
          aria-labelledby="new-session-title"
          aria-busy={isBusy() ? 'true' : 'false'}
          data-testid="new-session-dialog"
          data-state={state()}
          id="new-session-modal"
        >
          <header class="new-session-dialog__head">
            <h2 id="new-session-title" class="new-session-dialog__title">
              NEW SESSION
            </h2>
            <Button
              variant="ghost"
              size="sm"
              class="new-session-dialog__close"
              data-testid="new-session-close"
              aria-label="Close new session dialog"
              disabled={isBusy()}
              onClick={() => props.onClose()}
            >
              ×
            </Button>
          </header>

          <form class="new-session-dialog__form" onSubmit={onSubmit}>
            <label class="new-session-dialog__field">
              <span class="new-session-dialog__label">WORKSPACE</span>
              <div class="new-session-dialog__workspace-row">
                <input
                  type="text"
                  class="new-session-dialog__input new-session-dialog__workspace-input"
                  data-testid="workspace-input"
                  value={workspaceRoot()}
                  disabled={isBusy()}
                  placeholder="/absolute/path/to/workspace"
                  onInput={(e) => setWorkspaceRoot(e.currentTarget.value)}
                />
                <Button
                  variant="ghost"
                  size="sm"
                  type="button"
                  data-testid="workspace-browse"
                  disabled={isBusy()}
                  onClick={onBrowse}
                >
                  Browse
                </Button>
              </div>
            </label>

            <label class="new-session-dialog__field">
              <span class="new-session-dialog__label">PROVIDER</span>
              <Show
                when={hasEnabledProvider()}
                fallback={
                  <div
                    class="new-session-dialog__hint"
                    data-testid="provider-empty-hint"
                    role="note"
                  >
                    No providers configured. Add one on the Providers page.
                  </div>
                }
              >
                <select
                  class="new-session-dialog__input"
                  data-testid="provider-select"
                  value={providerId() ?? ''}
                  disabled={isBusy()}
                  onChange={(e) => setProviderId(e.currentTarget.value || null)}
                >
                  <For each={providerOptions()}>
                    {(opt) => (
                      <option value={opt.id} disabled={!opt.enabled}>
                        {opt.label}
                      </option>
                    )}
                  </For>
                </select>
              </Show>
            </label>

            <label class="new-session-dialog__field">
              <span class="new-session-dialog__label">AGENT</span>
              <span
                class="new-session-dialog__static"
                data-testid="agent-static"
              >
                {agentId()}
              </span>
            </label>

            <Show when={error() !== null}>
              <div
                class="new-session-dialog__error"
                role="alert"
                data-testid="new-session-error"
              >
                {error()}
              </div>
            </Show>

            <div class="new-session-dialog__actions">
              <Button
                variant="ghost"
                type="button"
                data-testid="new-session-cancel"
                disabled={isBusy()}
                onClick={() => props.onClose()}
              >
                Cancel
              </Button>
              <Button
                variant="primary"
                type="submit"
                data-testid="new-session-submit"
                loading={isBusy()}
                disabled={submitDisabled()}
              >
                <Show when={isBusy()} fallback={<>Start session</>}>
                  Starting…
                </Show>
              </Button>
            </div>
          </form>
        </div>
      </div>
    </Show>
  );
};
