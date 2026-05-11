import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, waitFor } from '@solidjs/testing-library';
import { MemoryRouter, Route } from '@solidjs/router';
import { Dashboard } from './Dashboard';
import { setInvokeForTesting } from '../lib/tauri';

// F-597: ContainersSection subscribes to a Tauri event via
// `@tauri-apps/api/event`. Stub it so the listen() call resolves to a
// no-op unlisten in jsdom (no Tauri runtime present).
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async () => () => undefined),
}));

describe('Dashboard', () => {
  beforeEach(() => {
    // F-719 Dashboard mounts SessionsPanel, ProvidersSection, and
    // ContainersSection, each of which invokes Tauri commands on mount.
    // Route every command to a hermetic stub so tests don't attempt a
    // real bridge call.
    setInvokeForTesting(
      (async (cmd: string) => {
        if (cmd === 'provider_status') {
          return {
            reachable: true,
            base_url: 'http://127.0.0.1:11434',
            models: [],
            last_checked: '2026-04-18T00:00:00Z',
          };
        }
        if (cmd === 'session_list') return [];
        if (cmd === 'dashboard_list_providers') return [];
        if (cmd === 'get_active_provider') return null;
        if (cmd === 'has_credential') return true;
        // F-722: UsageCard fetches the current + prior period summaries
        // on mount. Empty summary keeps the Dashboard suite hermetic.
        if (cmd === 'usage_summary') {
          return {
            range: { type: 'Last7' },
            group_by: 'Provider',
            total_tokens_in: 0,
            total_tokens_out: 0,
            total_cost: null,
            breakdown: [],
          };
        }
        // F-597: ContainersSection probes the runtime + lists containers
        // on mount. Hermetic responses keep tests deterministic and skip
        // the first-run banner (status: available).
        if (cmd === 'detect_container_runtime') return { kind: 'available' };
        if (cmd === 'list_active_containers') return [];
        // F-597: the dashboard reads persisted settings on mount to seed
        // the banner-dismissed signal. Default to "not dismissed" so the
        // banner-suppression behaviour is opt-in per test.
        if (cmd === 'get_settings') {
          return {
            notifications: { bg_agents: 'toast' },
            windows: { session_mode: 'single' },
            providers: { custom_openai: {} },
            dashboard: { container_banner_dismissed: false },
          };
        }
        return undefined;
      }) as never,
    );
  });

  afterEach(() => {
    setInvokeForTesting(null);
    cleanup();
  });

  // Dashboard is wrapped in a router-capable context so any descendant
  // router primitives resolve cleanly, matching the shell's runtime.
  function renderDashboard() {
    return render(() => (
      <MemoryRouter>
        <Route path="/" component={Dashboard} />
      </MemoryRouter>
    ));
  }

  // F-719: the dashboard hero owns the page heading. Verbatim per the
  // mock — `Welcome back. Forge something.` rendered across two lines.
  it('renders the F-719 hero headline', () => {
    const { getByRole } = renderDashboard();
    const heading = getByRole('heading', { level: 1 });
    expect(heading.textContent).toBe('Welcome back.Forge something.');
  });

  // F-409: spec dashboard.md §D.1 mandates a single flat surface — no tab
  // bar, no sidebar, no pane splits. A <nav> element violates the flatness
  // rule; AgentMonitor access is already provided by the StatusBar badge
  // and session-roster entry.
  it('renders no <nav> element (spec D.1 flat surface)', () => {
    const { container } = renderDashboard();
    expect(container.querySelector('nav')).toBeNull();
  });

  // F-719: the body is the 12-column grid with five slot wrappers
  // (Sessions col-8, Providers col-4, Usage col-6, Enabled col-6,
  // Containers col-12). F-722 / F-724 swap the Usage / Enabled
  // placeholders for their respective cards.
  it('renders the 12-col grid with all five slot wrappers', () => {
    const { container } = renderDashboard();
    const grid = container.querySelector('.dashboard__grid');
    expect(grid).not.toBeNull();
    expect(grid!.querySelectorAll('.dashboard__slot').length).toBe(5);
    expect(grid!.querySelector('.dashboard__slot--col-8')).not.toBeNull();
    expect(grid!.querySelector('.dashboard__slot--col-4')).not.toBeNull();
    expect(grid!.querySelectorAll('.dashboard__slot--col-6').length).toBe(2);
    expect(grid!.querySelector('.dashboard__slot--col-12')).not.toBeNull();
  });

  // F-719: the hero anchors above the grid.
  it('renders the hero above the grid', () => {
    const { container } = renderDashboard();
    const hero = container.querySelector('.dashboard-hero');
    const grid = container.querySelector('.dashboard__grid');
    expect(hero).not.toBeNull();
    expect(grid).not.toBeNull();
    expect(hero!.compareDocumentPosition(grid!) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  // F-722 / F-724: both placeholders have been swapped for their real
  // cards. The data-testids pin the grid slots.
  it('mounts the Usage and Enabled cards', () => {
    const { queryByTestId } = renderDashboard();
    expect(queryByTestId('usage-card')).not.toBeNull();
    expect(queryByTestId('enabled-assets-card')).not.toBeNull();
  });

  // F-588: when every credential-bearing provider has a key stored, the
  // first-run banner stays hidden.
  it('does not render the credential banner when all providers have keys', async () => {
    const { queryByTestId } = renderDashboard();
    // Resource resolves on next microtask; flush.
    await waitFor(() => {
      expect(queryByTestId('credential-banner')).toBeNull();
    });
  });

  // F-588: when at least one credential-bearing provider has no key, the
  // banner names the first such provider.
  it('renders the credential banner when a provider has no stored key', async () => {
    setInvokeForTesting(
      (async (cmd: string, args?: Record<string, unknown>) => {
        if (cmd === 'provider_status') {
          return {
            reachable: true,
            base_url: 'http://127.0.0.1:11434',
            models: [],
            last_checked: '2026-04-18T00:00:00Z',
          };
        }
        if (cmd === 'session_list') return [];
        if (cmd === 'dashboard_list_providers') return [];
        if (cmd === 'get_active_provider') return null;
        if (cmd === 'has_credential') {
          // Anthropic missing, OpenAI present — banner should name Anthropic.
          return args?.['providerId'] === 'openai';
        }
        return undefined;
      }) as never,
    );

    const { findByTestId } = renderDashboard();
    const banner = await findByTestId('credential-banner');
    expect(banner.textContent).toContain('Anthropic');
  });

  // F-597: persisted "Don't show again" preference survives a restart —
  // when `dashboard.container_banner_dismissed=true` is loaded from
  // settings on mount, the runtime banner must NOT render even if the
  // probe reports the runtime as missing.
  it('suppresses the runtime banner when dashboard.container_banner_dismissed=true', async () => {
    setInvokeForTesting(
      (async (cmd: string) => {
        if (cmd === 'provider_status') {
          return {
            reachable: true,
            base_url: 'http://127.0.0.1:11434',
            models: [],
            last_checked: '2026-04-18T00:00:00Z',
          };
        }
        if (cmd === 'session_list') return [];
        if (cmd === 'dashboard_list_providers') return [];
        if (cmd === 'get_active_provider') return null;
        if (cmd === 'has_credential') return true;
        // Probe reports the runtime is missing — without the persisted
        // dismissal, the banner WOULD render. The test asserts the
        // dismissal flag wins.
        if (cmd === 'detect_container_runtime') {
          return { kind: 'missing', tool: 'podman' };
        }
        if (cmd === 'list_active_containers') return [];
        if (cmd === 'get_settings') {
          return {
            notifications: { bg_agents: 'toast' },
            windows: { session_mode: 'single' },
            providers: { custom_openai: {} },
            dashboard: { container_banner_dismissed: true },
          };
        }
        return undefined;
      }) as never,
    );

    const { queryByTestId } = renderDashboard();
    // Give every mount-time async resolution a chance to land — the
    // probe, the settings read, and any reactive re-renders. The banner
    // must remain absent across the settle.
    await waitFor(() => {
      expect(queryByTestId('container-runtime-banner')).toBeNull();
    });
  });

  // F-597: regression — a previously dismissed banner must NOT flash
  // visible during the IPC round-trip that loads the persisted
  // dismissal flag. The dashboard signal is seeded as "unresolved" and
  // the render is gated on resolution, so even when the runtime probe
  // resolves first (reporting `missing`), the banner stays absent until
  // we know whether the user has dismissed it.
  it('does not flash banner before persisted dismissal resolves', async () => {
    let releaseSettings: (() => void) | null = null;
    const settingsPending = new Promise<void>((resolve) => {
      releaseSettings = resolve;
    });

    setInvokeForTesting(
      (async (cmd: string) => {
        if (cmd === 'provider_status') {
          return {
            reachable: true,
            base_url: 'http://127.0.0.1:11434',
            models: [],
            last_checked: '2026-04-18T00:00:00Z',
          };
        }
        if (cmd === 'session_list') return [];
        if (cmd === 'dashboard_list_providers') return [];
        if (cmd === 'get_active_provider') return null;
        if (cmd === 'has_credential') return true;
        // Probe resolves immediately with a missing runtime — the only
        // thing keeping the banner suppressed is the unresolved
        // dismissal signal.
        if (cmd === 'detect_container_runtime') {
          return { kind: 'missing', tool: 'podman' };
        }
        if (cmd === 'list_active_containers') return [];
        if (cmd === 'get_settings') {
          // Hold the settings response until we explicitly release it.
          await settingsPending;
          return {
            notifications: { bg_agents: 'toast' },
            windows: { session_mode: 'single' },
            providers: { custom_openai: {} },
            dashboard: { container_banner_dismissed: true },
          };
        }
        return undefined;
      }) as never,
    );

    const { queryByTestId } = renderDashboard();

    // Poll across many microtasks + macrotasks while the settings load
    // is in flight. A naive `false`-seeded signal would let the banner
    // render on any of these ticks; the tri-state guard must keep it
    // absent throughout the entire window.
    for (let i = 0; i < 25; i++) {
      expect(queryByTestId('container-runtime-banner')).toBeNull();
      await new Promise((r) => setTimeout(r, 2));
    }

    // Resolve the persisted-dismissal load and confirm the banner stays
    // suppressed (the loaded flag is `true`).
    releaseSettings!();
    await waitFor(() => {
      expect(queryByTestId('container-runtime-banner')).toBeNull();
    });
  });

  // F-588: a single broken probe must not silently suppress the banner
  // for the remaining providers. Anthropic throws, OpenAI is missing —
  // the banner must name OpenAI.
  it('falls back to the next provider when the first probe throws', async () => {
    setInvokeForTesting(
      (async (cmd: string, args?: Record<string, unknown>) => {
        if (cmd === 'provider_status') {
          return {
            reachable: true,
            base_url: 'http://127.0.0.1:11434',
            models: [],
            last_checked: '2026-04-18T00:00:00Z',
          };
        }
        if (cmd === 'session_list') return [];
        if (cmd === 'dashboard_list_providers') return [];
        if (cmd === 'get_active_provider') return null;
        if (cmd === 'has_credential') {
          if (args?.['providerId'] === 'anthropic') {
            throw new Error('keyring locked');
          }
          if (args?.['providerId'] === 'openai') return false;
          return false;
        }
        return undefined;
      }) as never,
    );

    const { findByTestId } = renderDashboard();
    const banner = await findByTestId('credential-banner');
    expect(banner.textContent).toContain('OpenAI');
    expect(banner.textContent).not.toContain('Anthropic has no stored credential');
  });
});
