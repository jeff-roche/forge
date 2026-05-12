// F-721: ProvidersSection (v-dash Providers card) tests.
//
// Read-only summary: one row per user-configured provider showing the
// brand-color dot, name, model summary (or error subtext), and a trailing
// `ready` / `auth` status pill. The active provider is highlighted but
// not mutable from this card — setting active is done via `/providers`
// or `~/.config/forge/settings.toml`.

import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { cleanup, render } from '@solidjs/testing-library';
import { MemoryRouter, Route } from '@solidjs/router';
import { ProvidersSection, type ProviderEntry } from './ProvidersSection';
import { setInvokeForTesting } from '../../lib/tauri';
import { clearToastsForTesting } from '../toast';

const invokeMock = vi.fn();

const sample = (over: Partial<ProviderEntry> = {}): ProviderEntry => ({
  id: 'anthropic',
  display_name: 'Anthropic',
  credential_required: true,
  has_credential: true,
  model_available: true,
  ...over,
});

const BUILTINS: ProviderEntry[] = [
  sample({
    id: 'anthropic',
    display_name: 'Anthropic',
    credential_required: true,
    has_credential: false,
    model_available: false,
  }),
  sample({
    id: 'openai',
    display_name: 'OpenAI',
    credential_required: true,
    has_credential: true,
    model_available: true,
    model: 'gpt-4o',
  }),
  sample({
    id: 'custom_openai:ollama',
    display_name: 'custom_openai — ollama',
    credential_required: true,
    has_credential: true,
    model_available: true,
    model: 'llama3.2',
    endpoint: 'http://127.0.0.1:11434/v1',
  }),
];

function setupInvokeMock(opts: {
  entries?: ProviderEntry[];
  active?: string | null;
} = {}) {
  invokeMock.mockImplementation((cmd: string) => {
    switch (cmd) {
      case 'dashboard_list_providers':
        return Promise.resolve(opts.entries ?? BUILTINS);
      case 'get_active_provider':
        return Promise.resolve(opts.active ?? null);
      default:
        return Promise.resolve(undefined);
    }
  });
}

async function waitForFetch() {
  // Resource needs a few microtask flushes to settle (Promise.all + setStore).
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

function renderSection() {
  return render(() => (
    <MemoryRouter>
      <Route path="/" component={() => <ProvidersSection />} />
    </MemoryRouter>
  ));
}

function queryRows(container: HTMLElement): HTMLElement[] {
  return Array.from(container.querySelectorAll<HTMLElement>('.providers__row'));
}

beforeEach(() => {
  invokeMock.mockReset();
  setInvokeForTesting(invokeMock as never);
  clearToastsForTesting();
});

afterEach(() => {
  setInvokeForTesting(null);
  cleanup();
  clearToastsForTesting();
});

describe('ProvidersSection (F-721)', () => {
  // -------------------------------------------------------------------------
  // Four states: loading / empty / error / ready
  // -------------------------------------------------------------------------

  it('renders a skeleton loading state during the IPC fetch', async () => {
    invokeMock.mockImplementation(() => new Promise(() => undefined));
    const { findByTestId } = renderSection();

    const skeleton = await findByTestId('providers-loading');
    expect(skeleton.getAttribute('role')).toBe('status');
    expect(skeleton.getAttribute('aria-busy')).toBe('true');
    expect(skeleton.querySelectorAll('.forge-skeleton--block').length).toBe(4);
  });

  it('renders empty-state copy when no providers are configured', async () => {
    setupInvokeMock({ entries: [], active: null });
    const { findByTestId } = renderSection();
    await waitForFetch();
    const empty = await findByTestId('providers-empty');
    expect(empty.textContent).toBe('// no providers configured');
  });

  it('surfaces list_providers rejection as a "providers unavailable" block', async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'dashboard_list_providers') return Promise.reject(new Error('keyring backend down'));
      if (cmd === 'get_active_provider') return Promise.resolve(null);
      return Promise.resolve(undefined);
    });
    const { findByRole } = renderSection();
    await waitForFetch();
    await waitForFetch();

    const alert = await findByRole('alert');
    expect(alert.textContent).toMatch(/providers unavailable/i);
    expect(alert.textContent).toMatch(/keyring backend down/i);
  });

  it('ready-state renders one row per provider', async () => {
    setupInvokeMock();
    const { container } = renderSection();
    await waitForFetch();

    const rows = queryRows(container);
    expect(rows.length).toBe(3);
    const labels = rows.map((r) => r.textContent ?? '');
    expect(labels.some((l) => l.includes('Anthropic'))).toBe(true);
    expect(labels.some((l) => l.includes('OpenAI'))).toBe(true);
    expect(labels.some((l) => l.includes('custom_openai — ollama'))).toBe(true);
  });

  // F-733: disabled providers (per the Providers page Enabled toggle) must
  // not appear in this summary. Disabled rows still appear on the Providers
  // page so the user can re-enable or remove them.
  it('skips providers with enabled=false', async () => {
    setupInvokeMock({
      entries: [
        sample({ id: 'anthropic', display_name: 'Anthropic', enabled: false }),
        sample({ id: 'openai', display_name: 'OpenAI', enabled: true }),
        sample({
          id: 'custom_openai:ollama',
          display_name: 'custom_openai — ollama',
          enabled: true,
        }),
      ],
    });
    const { container } = renderSection();
    await waitForFetch();

    const rows = queryRows(container);
    const labels = rows.map((r) => r.textContent ?? '');
    expect(labels.some((l) => l.includes('Anthropic'))).toBe(false);
    expect(labels.some((l) => l.includes('OpenAI'))).toBe(true);
    expect(labels.some((l) => l.includes('custom_openai — ollama'))).toBe(true);
  });

  // -------------------------------------------------------------------------
  // Header — Manage link
  // -------------------------------------------------------------------------

  it('renders a Manage link to /providers in the header', async () => {
    setupInvokeMock();
    const { findByRole } = renderSection();
    await waitForFetch();

    const link = await findByRole('link', { name: /manage/i });
    expect(link.getAttribute('href')).toBe('/providers');
  });

  // -------------------------------------------------------------------------
  // Row anatomy — brand dot, identity stack, pill
  // -------------------------------------------------------------------------

  it('renders a brand-color dot keyed to the provider id', async () => {
    setupInvokeMock();
    const { container } = renderSection();
    await waitForFetch();

    const rows = queryRows(container);
    const anthropic = rows.find((r) => r.textContent?.includes('Anthropic'))!;
    const openai = rows.find((r) => r.textContent?.includes('OpenAI'))!;
    const custom = rows.find((r) => r.textContent?.includes('custom_openai — ollama'))!;

    expect(anthropic.getAttribute('data-brand')).toBe('anthropic');
    expect(openai.getAttribute('data-brand')).toBe('openai');
    expect(custom.getAttribute('data-brand')).toBe('custom');

    for (const row of rows) {
      expect(row.querySelector('.providers__dot')).toBeTruthy();
    }
  });

  it('renders model subtext when a provider has a model configured', async () => {
    setupInvokeMock();
    const { container } = renderSection();
    await waitForFetch();

    const rows = queryRows(container);
    const openai = rows.find((r) => r.textContent?.includes('OpenAI'))!;
    expect(openai.querySelector('.providers__sub')?.textContent).toBe('gpt-4o');
  });

  // -------------------------------------------------------------------------
  // Status pill — variant rendering
  // -------------------------------------------------------------------------

  it('renders a `ready` pill when the provider has a model available', async () => {
    setupInvokeMock();
    const { container } = renderSection();
    await waitForFetch();

    const rows = queryRows(container);
    const openai = rows.find((r) => r.textContent?.includes('OpenAI'))!;
    const pill = openai.querySelector('.forge-status-pill');
    expect(pill).toBeTruthy();
    expect(pill?.classList.contains('forge-status-pill--ready')).toBe(true);
    expect(pill?.getAttribute('data-variant')).toBe('ready');
    expect(pill?.textContent).toContain('ready');
  });

  it('renders an `auth` pill when a required credential is missing', async () => {
    setupInvokeMock();
    const { container } = renderSection();
    await waitForFetch();

    const rows = queryRows(container);
    const anthropic = rows.find((r) => r.textContent?.includes('Anthropic'))!;
    const pill = anthropic.querySelector('.forge-status-pill');
    expect(pill).toBeTruthy();
    expect(pill?.classList.contains('forge-status-pill--auth')).toBe(true);
    expect(pill?.getAttribute('data-variant')).toBe('auth');
    expect(pill?.textContent).toContain('auth');
    expect(anthropic.querySelector('.providers__sub')?.textContent).toBe('credentials missing');
  });

  it('renders an `auth` pill when the credential is present but no model is configured', async () => {
    setupInvokeMock({
      entries: [
        sample({
          id: 'anthropic',
          display_name: 'Anthropic',
          credential_required: true,
          has_credential: true,
          model_available: false,
        }),
      ],
    });
    const { container } = renderSection();
    await waitForFetch();

    const rows = queryRows(container);
    const pill = rows[0]?.querySelector('.forge-status-pill');
    expect(pill?.classList.contains('forge-status-pill--auth')).toBe(true);
    expect(rows[0]?.querySelector('.providers__sub')?.textContent).toBe('unconfigured');
  });

  // -------------------------------------------------------------------------
  // Active-provider highlight (display-only — no mutation from this card)
  // -------------------------------------------------------------------------

  it('marks the active provider row with the active class', async () => {
    setupInvokeMock({ active: 'anthropic' });
    const { container } = renderSection();
    await waitForFetch();

    const rows = queryRows(container);
    const anthropic = rows.find((r) => r.textContent?.includes('Anthropic'))!;
    const openai = rows.find((r) => r.textContent?.includes('OpenAI'))!;
    expect(anthropic.classList.contains('providers__row--active')).toBe(true);
    expect(openai.classList.contains('providers__row--active')).toBe(false);
  });

  it('renders no active row when active is null', async () => {
    setupInvokeMock({ active: null });
    const { container } = renderSection();
    await waitForFetch();

    const rows = queryRows(container);
    for (const r of rows) {
      expect(r.classList.contains('providers__row--active')).toBe(false);
    }
  });

  it('rows do NOT invoke set_active_provider when clicked', async () => {
    setupInvokeMock();
    const { container } = renderSection();
    await waitForFetch();

    const rows = queryRows(container);
    const anthropic = rows.find((r) => r.textContent?.includes('Anthropic'))!;
    anthropic.click();
    await waitForFetch();

    const setActiveCalls = invokeMock.mock.calls.filter(
      (c) => c[0] === 'set_active_provider',
    );
    expect(setActiveCalls.length).toBe(0);
  });

  // ---------------------------------------------------------------------------
  // F-738: provider remediation CTAs
  // ---------------------------------------------------------------------------

  describe('F-738 remediation CTAs', () => {
    it('renders `Add credential` for a provider that requires a credential but is missing one', async () => {
      setupInvokeMock();
      const { findByTestId } = renderSection();
      await waitForFetch();

      const cta = await findByTestId('provider-cta-add-credential-anthropic');
      expect(cta.textContent).toMatch(/add credential/i);
      expect(cta.getAttribute('href')).toBe('/providers#anthropic');
    });

    it('does NOT render `Add credential` when the provider has its credential', async () => {
      setupInvokeMock();
      const { queryByTestId } = renderSection();
      await waitForFetch();

      // OpenAI in the fixture has credential_required=true && has_credential=true.
      expect(queryByTestId('provider-cta-add-credential-openai')).toBeNull();
    });

    it('renders one `Add credential` CTA per credential-missing provider', async () => {
      setupInvokeMock();
      const { findByTestId } = renderSection();
      await waitForFetch();

      // Fixture has anthropic missing credentials.
      await findByTestId('provider-cta-add-credential-anthropic');
    });

    it('does NOT render `Add credential` for a Vertex-authenticated Anthropic instance', async () => {
      // Phase B: gcloud ADC supplies Vertex auth at request time, so the
      // row reports credential_required=false. The tooltip + sub-text
      // make the auth source explicit.
      const vertexEntry: ProviderEntry = {
        id: 'anthropic:vertex-work',
        display_name: 'Anthropic — vertex-work',
        credential_required: false,
        has_credential: false,
        model_available: true,
        enabled: true,
        auth_kind: 'vertex',
      };
      setupInvokeMock({ entries: [vertexEntry] });
      const { findByText, queryByTestId } = renderSection();
      await waitForFetch();

      const nameSpan = await findByText('Anthropic — vertex-work');
      const row = nameSpan.closest('.providers__row') as HTMLElement;
      expect(row).toBeTruthy();
      expect(queryByTestId('provider-cta-add-credential-anthropic:vertex-work')).toBeNull();
      const sub = row.querySelector('.providers__sub');
      expect(sub?.textContent).toBe('gcloud ADC');
      expect(row.getAttribute('title') ?? '').toContain('gcloud Application Default Credentials');
    });
  });
});
