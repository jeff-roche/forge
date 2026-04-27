import { type Component, createResource, createSignal, onMount, Show } from 'solid-js';
import { ProviderPanel } from './Dashboard/ProviderPanel';
import { ProvidersSection } from '../components/dashboard/ProvidersSection';
import { SessionsPanel } from './Dashboard/SessionsPanel';
import {
  CREDENTIAL_PROVIDERS,
  CredentialBanner,
  CredentialsSection,
} from '../components/dashboard/CredentialsSection';
import {
  ContainerRuntimeBanner,
  ContainersSection,
} from '../components/dashboard/ContainersSection';
import { MemorySection } from '../components/dashboard/MemorySection';
import { hasCredential } from '../ipc/credentials';
import {
  CONTAINER_BANNER_DISMISSED_KEY,
  detectContainerRuntime,
  type RuntimeStatus,
} from '../ipc/containers';
import { getSettings, setSetting } from '../ipc/session';
import { activeWorkspaceRoot } from '../stores/session';
import './Dashboard.css';

/**
 * F-588: surface a first-run banner when the active provider has no stored
 * credential. Phase 3 ships two credential-bearing providers
 * (`anthropic`, `openai`); without an explicit "active provider" signal
 * yet, the banner picks the first such provider that lacks a credential
 * so the user has a concrete target to add.
 *
 * Probe failure on a single provider is treated as "stored for that
 * provider only" — the loop `continue`s so a broken probe on Anthropic
 * does not silently suppress the banner that would otherwise call out a
 * missing OpenAI key. The per-row indicator inside `<CredentialsSection>`
 * surfaces the underlying probe error to the user.
 */
async function firstMissingCredential(): Promise<{ id: string; label: string } | null> {
  for (const provider of CREDENTIAL_PROVIDERS) {
    try {
      const stored = await hasCredential(provider.id);
      if (!stored) return { id: provider.id as unknown as string, label: provider.label };
    } catch {
      continue;
    }
  }
  return null;
}

/**
 * F-597: probe the container runtime once on Dashboard mount. If the
 * probe reports "available" we never render the banner. Probe failures
 * (e.g. IPC throws because the command isn't wired) are treated as
 * "unknown" — the banner asks the user to investigate rather than
 * silently swallowing the failure.
 */
async function probeRuntime(): Promise<RuntimeStatus> {
  try {
    return await detectContainerRuntime();
  } catch (err: unknown) {
    return {
      kind: 'unknown',
      reason: err instanceof Error ? err.message : String(err),
    };
  }
}

/**
 * F-597: read the persisted "Don't show again" preference. The Dashboard
 * is workspace-agnostic and the dismissed flag lives in user-tier
 * settings, so we pass an empty `workspaceRoot` (the backend ignores it
 * for the user tier on read just as on write). Probe failures fall back
 * to "not dismissed" so a broken settings file doesn't permanently
 * suppress the banner.
 */
async function loadBannerDismissed(): Promise<boolean> {
  try {
    const s = await getSettings('');
    return s.dashboard?.container_banner_dismissed === true;
  } catch {
    return false;
  }
}

export const Dashboard: Component = () => {
  const [missing] = createResource(firstMissingCredential);
  const [runtimeStatus] = createResource(probeRuntime);
  // Tri-state seed for the "Don't show again" preference: `null` while
  // the persisted load is in flight, then `true`/`false` once resolved.
  // Rendering is gated on the resolved (non-`null`) state so a user who
  // previously dismissed the banner never sees a flash before the IPC
  // round-trip lands.
  const [bannerDismissed, setBannerDismissed] = createSignal<boolean | null>(null);
  onMount(() => {
    void (async () => {
      setBannerDismissed(await loadBannerDismissed());
    })();
  });

  // Pass an empty workspaceRoot — F-597 banner-dismissed is a user-tier
  // setting and the backend ignores `workspace_root` for `level: "user"`
  // beyond the size cap. The dashboard is workspace-agnostic in this
  // path, so there's no authoritative root to forward.
  const dismissBanner = async () => {
    setBannerDismissed(true);
    try {
      await setSetting(CONTAINER_BANNER_DISMISSED_KEY, true, 'user', '');
    } catch (err: unknown) {
      // Surface the failure on the next probe; the banner is suppressed
      // for this session regardless so the dismiss action feels reliable.
      // eslint-disable-next-line no-console
      console.warn('[Dashboard] failed to persist container banner dismissal', err);
    }
  };

  const showRuntimeBanner = () => {
    // Suppress until the persisted dismissal state has resolved — a
    // `null` value means the IPC round-trip is still in flight, and
    // rendering the banner during that window would flash for users
    // who previously dismissed it.
    const dismissed = bannerDismissed();
    if (dismissed === null || dismissed) return false;
    const s = runtimeStatus();
    return s !== undefined && s.kind !== 'available';
  };

  return (
    <main class="dashboard">
      <h1 class="dashboard__title">Forge — Dashboard</h1>
      <Show when={missing()}>
        {(m) => <CredentialBanner providerLabel={m().label} />}
      </Show>
      <Show when={showRuntimeBanner() && runtimeStatus()}>
        {(s) => (
          <ContainerRuntimeBanner
            status={s()}
            onDismiss={() => void dismissBanner()}
          />
        )}
      </Show>
      <ProviderPanel />
      <ProvidersSection />
      <CredentialsSection />
      <ContainersSection />
      <Show when={activeWorkspaceRoot()}>
        {(root) => <MemorySection workspaceRoot={root()} />}
      </Show>
      <SessionsPanel />
    </main>
  );
};
