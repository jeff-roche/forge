// F-731: per-row Test connection chip on the Providers page.
//
// State machine:
//   idle      no result rendered next to the button; click triggers probe.
//   probing   inline `probing` pill; click is a no-op.
//   resolved  result fires through `pushToast` (info on success, error on
//             failure) — see `ToastHost` for the auto-dismiss + manual
//             dismiss UX. The button returns to idle so the user can
//             retry without waiting for the toast to clear.
//
// Routing results to toasts (vs. inline) keeps the row visually quiet
// after the probe completes and lets the user copy verbatim error text
// from a persistent notification.

import {
  type Component,
  createSignal,
  Show,
} from 'solid-js';
import { Button } from '@forge/design';
import type {
  TestProviderConnectionInput,
  TestProviderConnectionOutput,
} from '@forge/ipc';
import { invoke } from '../lib/tauri';
import { pushToast } from './toast';
import './TestConnectionButton.css';

export type TestConnectionState = 'idle' | 'probing';

export interface TestConnectionButtonProps {
  providerId: string;
  /** Injection seam for tests. Defaults to the module-level
   * `pushToast`; tests pass a spy. */
  pushToast?: (kind: 'info' | 'warning' | 'error', message: string) => void;
}

const AUTH_PREFIX = 'test_provider_connection: auth ';

function isAuthFailure(message: string): boolean {
  return message.startsWith(AUTH_PREFIX);
}

function successMessage(providerId: string, out: TestProviderConnectionOutput): string {
  const latency = out.latency_ms != null ? ` (${String(out.latency_ms)}ms)` : '';
  return `${providerId}: connection succeeded${latency}`;
}

function errorMessage(providerId: string, raw: string): string {
  // Trim the `test_provider_connection: ` prefix once for display so the
  // toast reads cleanly. The full verbatim error stays visible through
  // the daemon's log target.
  const PREFIX = 'test_provider_connection: ';
  const trimmed = raw.startsWith(PREFIX) ? raw.slice(PREFIX.length) : raw;
  const label = isAuthFailure(raw) ? 'auth required' : 'unreachable';
  return `${providerId}: ${label} — ${trimmed}`;
}

export const TestConnectionButton: Component<TestConnectionButtonProps> = (
  props,
) => {
  const [state, setState] = createSignal<TestConnectionState>('idle');

  const probe = async (): Promise<void> => {
    if (state() === 'probing') return;
    setState('probing');
    const toast = props.pushToast ?? pushToast;
    try {
      const input: TestProviderConnectionInput = { provider_id: props.providerId };
      const out = await invoke<TestProviderConnectionOutput>(
        'test_provider_connection',
        { input },
      );
      toast('info', successMessage(props.providerId, out));
    } catch (err: unknown) {
      const msg = err instanceof Error ? err.message : String(err);
      toast('error', errorMessage(props.providerId, msg));
    } finally {
      setState('idle');
    }
  };

  const testIdSuffix = (): string => props.providerId;

  return (
    <div class="test-connection" data-state={state()} data-provider-id={props.providerId}>
      <Button
        variant="ghost"
        size="sm"
        type="button"
        data-testid={`test-connection-button-${testIdSuffix()}`}
        aria-busy={state() === 'probing' ? 'true' : 'false'}
        aria-label={state() === 'probing' ? 'Testing connection' : undefined}
        loading={state() === 'probing'}
        onClick={() => void probe()}
      >
        <Show when={state() === 'probing'} fallback={<>Test</>}>
          {/* SVG ring spinner — replaces the "Test" label while a probe
              is in flight. The stroke-dasharray + rotation animation is
              token-driven so reduced-motion users get a static ring. */}
          <span
            class="test-connection__spinner"
            data-testid={`test-connection-spinner-${testIdSuffix()}`}
            aria-hidden="true"
          />
        </Show>
      </Button>

      {/* Success / error outcomes are routed through `pushToast` —
          nothing inline beyond the spinner inside the Test button. */}
    </div>
  );
};
