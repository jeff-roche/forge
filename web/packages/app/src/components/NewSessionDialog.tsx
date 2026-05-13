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
  onCleanup,
  onMount,
  Show,
} from 'solid-js';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { A } from '@solidjs/router';
import { Button } from '@forge/design';
import type {
  RosterTier,
  SessionStartInput,
  SessionStartOutput,
} from '@forge/ipc';
import { invoke } from '../lib/tauri';
import {
  getActiveProvider,
  listProviders,
  openSession,
  type ProviderEntry,
} from '../ipc/dashboard';
import { listAgents } from '../ipc/catalog';
import { activeWorkspaceRoot } from '../stores/session';
import { useFocusTrap } from '../lib/useFocusTrap';
import { Dropdown, type DropdownOption } from './Dropdown';
import './NewSessionDialog.css';

export type DialogState = 'idle' | 'validating' | 'spawning' | 'spawn-failed';

export interface NewSessionDialogProps {
  open: boolean;
  onClose: () => void;
  /** Fires after `session_start` succeeds and `open_session` has been dispatched. */
  onSpawned?: (sessionId: string) => void;
}

/** Built-in default — mirrors `forge_agents::FORGE_DEFAULT_AGENT_NAME` on
 * the daemon side. The daemon's roster loader injects this name even when
 * the user has no `.agents/*.md` files, so the picker is never empty. */
const FALLBACK_AGENT = 'forge-default';

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

/**
 * F-746: stable wire-error suffix the daemon emits when
 * `session_start`'s pre-spawn credential check rejects. The full error
 * shape is
 * `session_start: credentials_missing for provider <provider_id>` —
 * extracting `<provider_id>` lets the dialog deep-link the recovery
 * CTA to `/providers#<id>`. Both the "no entry stored" and the
 * "keyring backend unreachable" branches collapse onto this single
 * reason on the daemon side; the Providers page surfaces the more
 * specific message after the user lands there.
 */
const CREDENTIALS_MISSING_REASON = 'credentials_missing for provider ';

/** Return the provider id when `msg` is a `credentials_missing` rejection. */
function extractMissingCredentialProviderId(msg: string | null): string | null {
  if (msg === null) return null;
  const idx = msg.indexOf(CREDENTIALS_MISSING_REASON);
  if (idx === -1) return null;
  const id = msg.slice(idx + CREDENTIALS_MISSING_REASON.length).trim();
  return id.length > 0 ? id : null;
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

  // Load agents whenever the workspace root changes — agents can be either
  // user-scoped (always present) or workspace-scoped (require a path). When
  // the field is empty we still surface user-scoped agents so the picker
  // is populated from the moment the dialog opens.
  interface AgentRow {
    id: string;
    tier: RosterTier | null;
  }
  const [agentsResource] = createResource<AgentRow[], string>(
    () => workspaceRoot().trim(),
    async (root) => {
      const entries = (await listAgents(root).catch(() => [])) ?? [];
      return entries
        .filter((e) => e.entry.type === 'Agent')
        .map<AgentRow>((e) => ({
          id: (e.entry as { id: string }).id,
          tier: e.tier ?? null,
        }));
    },
  );

  const agentOptions = createMemo<DropdownOption[]>(() => {
    const list = agentsResource() ?? [];
    // De-dupe by id (an agent could in theory exist at both tiers; workspace
    // wins because it's the more specific override).
    const byId = new Map<string, AgentRow>();
    for (const row of list) {
      const existing = byId.get(row.id);
      if (!existing || (existing.tier === 'user' && row.tier === 'workspace')) {
        byId.set(row.id, row);
      }
    }
    if (!byId.has(FALLBACK_AGENT)) {
      byId.set(FALLBACK_AGENT, { id: FALLBACK_AGENT, tier: null });
    }
    return [...byId.values()]
      .sort((a, b) => a.id.localeCompare(b.id))
      .map<DropdownOption>((row) => {
        const base: DropdownOption = { value: row.id, label: row.id };
        if (row.tier === 'workspace')
          return { ...base, chip: { text: 'workspace', tone: 'workspace' } };
        if (row.tier === 'user')
          return { ...base, chip: { text: 'user', tone: 'user' } };
        return base;
      });
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

  // Snap the agent selection to a valid option whenever the roster
  // resolves. The current selection wins if it is still present.
  createEffect(() => {
    const opts = agentOptions();
    const first = opts[0];
    if (first === undefined) return;
    if (!opts.some((o) => o.value === agentId())) {
      setAgentId(first.value);
    }
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

  // Memoize the credentials-missing parse so the `<Show>` guard and the
  // child `href` share a single evaluation per render. Solid evaluates
  // `when` and the child render expression independently, so calling
  // `extractMissingCredentialProviderId(error())` in both places would
  // re-parse the same string twice — a `createMemo` is the idiomatic
  // fix and also lets the child callback receive the non-null id
  // directly (no `?? ''` fallback).
  const missingCredentialProvider = createMemo(() =>
    extractMissingCredentialProviderId(error()),
  );

  // F-754: when the error path renders an actionable CTA, migrate focus
  // off the (now-disabled) submit button onto the CTA so screen-reader
  // users don't have to tab to it. Defer through `queueMicrotask` so the
  // `role="alert"` announcement fires first — moving focus synchronously
  // in the same tick would pre-empt assistive tech reading the alert.
  // Non-credential errors leave focus alone so retry stays one keypress
  // away on the submit button.
  createEffect(() => {
    if (missingCredentialProvider() === null) return;
    queueMicrotask(() => {
      const cta = dialogRef?.querySelector<HTMLElement>(
        '[data-testid="new-session-error-cta"]',
      );
      cta?.focus();
    });
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

            <div class="new-session-dialog__field">
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
                <Dropdown
                  testid="provider-select"
                  ariaLabel="Provider"
                  disabled={isBusy()}
                  value={providerId() ?? ''}
                  onChange={(v) => setProviderId(v || null)}
                  options={providerOptions().map((opt) => ({
                    value: opt.id,
                    label: opt.label,
                    disabled: !opt.enabled,
                  }))}
                />
              </Show>
            </div>

            <div class="new-session-dialog__field">
              <span class="new-session-dialog__label">AGENT</span>
              <Dropdown
                testid="agent-select"
                ariaLabel="Agent"
                disabled={isBusy()}
                value={agentId()}
                onChange={setAgentId}
                options={agentOptions()}
              />
            </div>

            <Show when={error() !== null}>
              <div
                class="new-session-dialog__error"
                role="alert"
                data-testid="new-session-error"
              >
                {error()}
                <Show when={missingCredentialProvider()}>
                  {(id) => (
                    <>
                      {' '}
                      <A
                        class="new-session-dialog__error-cta"
                        data-testid="new-session-error-cta"
                        href={`/providers#${id()}`}
                        onClick={() => props.onClose()}
                      >
                        Configure provider
                      </A>
                    </>
                  )}
                </Show>
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
