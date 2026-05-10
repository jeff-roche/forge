import {
  type Component,
  createEffect,
  createResource,
  createSignal,
  For,
  Show,
} from 'solid-js';
import { Button } from '@forge/design';
import type { ProviderId } from '@forge/ipc';
import { providerStatus, type ProviderStatus } from '../../ipc/dashboard';
import { providerAccent } from '../Session/providerAccent';
import './ProviderPanel.css';

export type { ProviderStatus };

const fetchStatus = () => providerStatus();

// Phase-1 ships a single provider. Centralising the id here keeps the accent
// mapping, label, and future multi-provider swap in one spot.
const PHASE_1_PROVIDER: ProviderId = 'ollama' as ProviderId;

export const ProviderPanel: Component = () => {
  const [status, { refetch }] = createResource<ProviderStatus>(fetchStatus);
  const [expanded, setExpanded] = createSignal(false);

  return (
    <section class="provider-panel" aria-label="AI provider status">
      <header class="provider-panel__header">
        <span class="provider-panel__label">PROVIDER</span>
        <span class="provider-panel__name">ollama</span>
      </header>

      <Show
        when={!status.error}
        fallback={
          <div class="provider-panel__error" role="alert">
            <p class="provider-panel__error-title">PROVIDER UNAVAILABLE</p>
            <p class="provider-panel__error-detail">{String(status.error)}</p>
            <div class="provider-panel__actions">
              <Button variant="ghost" size="sm" class="provider-panel__btn" onClick={() => refetch()}>
                RETRY
              </Button>
            </div>
          </div>
        }
      >
        <Show when={status()} fallback={<p class="provider-panel__loading">ollama · probing</p>}>
          {(s) => (
            <>
              <div class="provider-panel__row">
                <HealthIndicator reachable={s().reachable} providerId={PHASE_1_PROVIDER} />
                <code class="provider-panel__url">{s().base_url}</code>
              </div>

              <Show
                when={s().reachable}
                fallback={<UnreachableHint baseUrl={s().base_url} errorKind={s().error_kind} />}
              >
                <ModelSection
                  models={s().models}
                  expanded={expanded()}
                  onToggle={() => setExpanded((v) => !v)}
                />
              </Show>

              <div class="provider-panel__actions">
                <Button variant="ghost" size="sm" class="provider-panel__btn" onClick={() => refetch()}>
                  REFRESH
                </Button>
              </div>
            </>
          )}
        </Show>
      </Show>
    </section>
  );
};

// F-413: when reachable, pass the provider accent via the `--provider-accent`
// custom property so the `.provider-panel__health--ok` rule can paint the dot
// and the §11.3 live-connected glow from one token. When unreachable, the
// custom property is removed so the error tint stays in charge.
//
// F-805: written via `setProperty` on a ref rather than a JSX `style={...}`
// binding so `style-src-attr` can drop `'unsafe-inline'`.
const HealthIndicator: Component<{ reachable: boolean; providerId: ProviderId }> = (props) => {
  let ref: HTMLSpanElement | undefined;
  createEffect(() => {
    if (!ref) return;
    if (props.reachable) {
      ref.style.setProperty('--provider-accent', providerAccent(props.providerId));
    } else {
      ref.style.removeProperty('--provider-accent');
    }
  });
  return (
    <span
      ref={ref}
      class="provider-panel__health"
      classList={{
        'provider-panel__health--ok': props.reachable,
        'provider-panel__health--down': !props.reachable,
      }}
      role="img"
      aria-label={props.reachable ? 'reachable' : 'unreachable'}
    />
  );
};

const ModelSection: Component<{
  models: string[];
  expanded: boolean;
  onToggle: () => void;
}> = (props) => (
  <div class="provider-panel__models">
    <Button
      variant="ghost"
      size="sm"
      class="provider-panel__btn provider-panel__btn--ghost"
      aria-expanded={props.expanded}
      onClick={props.onToggle}
    >
      {props.expanded ? 'HIDE MODELS' : 'SHOW MODELS'}
      <span class="provider-panel__count">
        {' '}
        — {props.models.length} {props.models.length === 1 ? 'model' : 'models'}
      </span>
    </Button>
    <Show when={props.expanded}>
      <ul class="provider-panel__model-list">
        <For each={props.models}>
          {(m) => <li class="provider-panel__model">{m}</li>}
        </For>
      </ul>
    </Show>
  </div>
);

const UnreachableHint: Component<{ baseUrl: string; errorKind?: string | undefined }> = (props) => {
  // Voice rule: include the exact technical identifier developers expect.
  const host = props.baseUrl.replace(/^https?:\/\//, '');
  return (
    <div class="provider-panel__unreachable">
      <p class="provider-panel__error-code">ECONNREFUSED {host}</p>
      <p class="provider-panel__error-detail">
        Start the Ollama daemon. The Dashboard reprobes on refresh.
      </p>
      <a
        href="https://ollama.com/download"
        target="_blank"
        rel="noreferrer noopener"
        role="button"
        class="provider-panel__btn provider-panel__btn--primary"
      >
        START OLLAMA
      </a>
    </div>
  );
};
