// F-731: per-row Test connection chip on the Providers page.
//
// State machine (per `docs/ui-specs/providers-page.md §Per-test`):
//
//   idle      pill reads `unknown`; chip enabled.
//   probing   pill reads `probing`; chip `aria-busy=true`; clicks are no-ops.
//   success   pill reads `ready <latency_ms>ms`; chip re-enabled.
//   error     pill reads `auth-required` (when the verbatim error starts
//             `test_provider_connection: auth `) or `unreachable`; the
//             verbatim error renders inline with `role="alert"`.
//
// The `auth ` infix is the load-bearing signal — the spec routes the pill
// to its `auth` variant without parsing the rest of the string.

import { type Component, createSignal, Show } from 'solid-js';
import { Button, StatusPill } from '@forge/design';
import type {
  TestProviderConnectionInput,
  TestProviderConnectionOutput,
} from '@forge/ipc';
import { invoke } from '../lib/tauri';
import './TestConnectionButton.css';

export type TestConnectionState = 'idle' | 'probing' | 'success' | 'error';

export interface TestConnectionButtonProps {
  providerId: string;
}

const AUTH_PREFIX = 'test_provider_connection: auth ';

export const TestConnectionButton: Component<TestConnectionButtonProps> = (
  props,
) => {
  const [state, setState] = createSignal<TestConnectionState>('idle');
  const [result, setResult] = createSignal<TestProviderConnectionOutput | null>(
    null,
  );
  const [error, setError] = createSignal<string | null>(null);

  const probe = async (): Promise<void> => {
    if (state() === 'probing') return;
    setState('probing');
    setError(null);
    setResult(null);
    try {
      const input: TestProviderConnectionInput = { provider_id: props.providerId };
      const out = await invoke<TestProviderConnectionOutput>(
        'test_provider_connection',
        { input },
      );
      setResult(out);
      setState('success');
    } catch (err: unknown) {
      const msg = err instanceof Error ? err.message : String(err);
      setError(msg);
      setState('error');
    }
  };

  const isAuthFailure = (): boolean => {
    const e = error();
    return e !== null && e.startsWith(AUTH_PREFIX);
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
        loading={state() === 'probing'}
        onClick={() => void probe()}
      >
        Test
      </Button>

      <Show when={state() === 'idle'}>
        <StatusPill
          variant="idle"
          data-testid={`test-connection-pill-${testIdSuffix()}`}
        >
          unknown
        </StatusPill>
      </Show>

      <Show when={state() === 'probing'}>
        <StatusPill
          variant="idle"
          data-testid={`test-connection-pill-${testIdSuffix()}`}
        >
          probing
        </StatusPill>
      </Show>

      <Show when={state() === 'success' && result()}>
        {(out) => (
          <StatusPill
            variant="ready"
            data-testid={`test-connection-pill-${testIdSuffix()}`}
          >
            ready{out().latency_ms != null ? ` ${String(out().latency_ms)}ms` : ''}
          </StatusPill>
        )}
      </Show>

      <Show when={state() === 'error'}>
        <StatusPill
          variant="auth"
          data-testid={`test-connection-pill-${testIdSuffix()}`}
        >
          {isAuthFailure() ? 'auth-required' : 'unreachable'}
        </StatusPill>
      </Show>

      <Show when={state() === 'error' && error()}>
        {(msg) => (
          <span
            class="test-connection__alert"
            role="alert"
            data-testid={`test-connection-error-${testIdSuffix()}`}
          >
            {msg()}
          </span>
        )}
      </Show>
    </div>
  );
};
