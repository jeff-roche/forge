// F-721: ProvidersSection (v-dash Providers card) tests.
//
// Compact stacked-list layout. The list root is a `radiogroup`; each row is
// a `radio` carrying name, brand-color dot, model summary, and a trailing
// `ready` / `auth` status pill. Radio semantics (click → set_active_provider,
// aria-checked on the active row) are preserved from the F-586 grid layout.

import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { cleanup, render, fireEvent } from '@solidjs/testing-library';
import { MemoryRouter, Route } from '@solidjs/router';
import { ProvidersSection, type ProviderEntry } from './ProvidersSection';
import { setInvokeForTesting } from '../../lib/tauri';

const invokeMock = vi.fn();

const sample = (over: Partial<ProviderEntry> = {}): ProviderEntry => ({
  id: 'ollama',
  display_name: 'Ollama',
  credential_required: false,
  has_credential: false,
  model_available: true,
  ...over,
});

const FOUR_BUILTINS: ProviderEntry[] = [
  sample({ id: 'ollama', display_name: 'Ollama', model: 'llama-3.3' }),
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
    id: 'custom_openai',
    display_name: 'Custom OpenAI-compat',
    credential_required: true,
    has_credential: false,
    model_available: false,
  }),
];

function setupInvokeMock(opts: {
  entries?: ProviderEntry[];
  active?: string | null;
  setActiveError?: string;
} = {}) {
  invokeMock.mockImplementation((cmd: string) => {
    switch (cmd) {
      case 'dashboard_list_providers':
        return Promise.resolve(opts.entries ?? FOUR_BUILTINS);
      case 'get_active_provider':
        return Promise.resolve(opts.active ?? null);
      case 'set_active_provider':
        if (opts.setActiveError) return Promise.reject(new Error(opts.setActiveError));
        return Promise.resolve(undefined);
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

/**
 * Wraps the component in a MemoryRouter so the `Manage` link's `<A>`
 * resolves a context. The router never navigates in test — we only assert
 * link presence and `href`.
 */
function renderSection() {
  return render(() => (
    <MemoryRouter>
      <Route path="/" component={() => <ProvidersSection />} />
    </MemoryRouter>
  ));
}

beforeEach(() => {
  invokeMock.mockReset();
  setInvokeForTesting(invokeMock as never);
});

afterEach(() => {
  setInvokeForTesting(null);
  cleanup();
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

  it('ready-state renders a radiogroup with one row per provider', async () => {
    setupInvokeMock();
    const { findByRole, findAllByRole } = renderSection();
    await waitForFetch();

    const group = await findByRole('radiogroup');
    expect(group.getAttribute('aria-label')).toBe('Active provider');

    const rows = await findAllByRole('radio');
    expect(rows.length).toBe(4);
    const labels = rows.map((r) => r.textContent ?? '');
    expect(labels.some((l) => l.includes('Ollama'))).toBe(true);
    expect(labels.some((l) => l.includes('Anthropic'))).toBe(true);
    expect(labels.some((l) => l.includes('OpenAI'))).toBe(true);
    expect(labels.some((l) => l.includes('Custom OpenAI-compat'))).toBe(true);
  });

  // F-733: disabled providers (per the Providers page Enabled toggle) must
  // not appear in the active-provider selector. Disabled rows still appear
  // on the Providers page so the user can re-enable or remove them.
  it('skips providers with enabled=false from the radiogroup', async () => {
    setupInvokeMock({
      entries: [
        sample({ id: 'ollama', display_name: 'Ollama', enabled: true }),
        sample({ id: 'anthropic', display_name: 'Anthropic', enabled: false }),
        sample({ id: 'openai', display_name: 'OpenAI', enabled: true }),
      ],
    });
    const { findAllByRole } = renderSection();
    await waitForFetch();

    const rows = await findAllByRole('radio');
    const labels = rows.map((r) => r.textContent ?? '');
    expect(labels.some((l) => l.includes('Anthropic'))).toBe(false);
    expect(labels.some((l) => l.includes('Ollama'))).toBe(true);
    expect(labels.some((l) => l.includes('OpenAI'))).toBe(true);
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
    const { findAllByRole } = renderSection();
    await waitForFetch();

    const rows = await findAllByRole('radio');
    const anthropic = rows.find((r) => r.textContent?.includes('Anthropic'))!;
    const openai = rows.find((r) => r.textContent?.includes('OpenAI'))!;
    const ollama = rows.find((r) => r.textContent?.includes('Ollama'))!;
    const custom = rows.find((r) => r.textContent?.includes('Custom OpenAI-compat'))!;

    expect(anthropic.getAttribute('data-brand')).toBe('anthropic');
    expect(openai.getAttribute('data-brand')).toBe('openai');
    expect(ollama.getAttribute('data-brand')).toBe('local');
    expect(custom.getAttribute('data-brand')).toBe('custom');

    // Each row carries a single brand dot.
    for (const row of rows) {
      expect(row.querySelector('.providers__dot')).toBeTruthy();
    }
  });

  it('renders model subtext when a provider has a model configured', async () => {
    setupInvokeMock();
    const { findAllByRole } = renderSection();
    await waitForFetch();

    const rows = await findAllByRole('radio');
    const ollama = rows.find((r) => r.textContent?.includes('Ollama'))!;
    expect(ollama.querySelector('.providers__sub')?.textContent).toBe('llama-3.3');
  });

  // -------------------------------------------------------------------------
  // Status pill — variant rendering
  // -------------------------------------------------------------------------

  it('renders a `ready` pill when the provider has a model available', async () => {
    setupInvokeMock();
    const { findAllByRole } = renderSection();
    await waitForFetch();

    const rows = await findAllByRole('radio');
    const ollama = rows.find((r) => r.textContent?.includes('Ollama'))!;
    const pill = ollama.querySelector('.forge-status-pill');
    expect(pill).toBeTruthy();
    expect(pill?.classList.contains('forge-status-pill--ready')).toBe(true);
    expect(pill?.getAttribute('data-variant')).toBe('ready');
    expect(pill?.textContent).toContain('ready');
  });

  it('renders an `auth` pill when a required credential is missing', async () => {
    setupInvokeMock();
    const { findAllByRole } = renderSection();
    await waitForFetch();

    const rows = await findAllByRole('radio');
    const anthropic = rows.find((r) => r.textContent?.includes('Anthropic'))!;
    const pill = anthropic.querySelector('.forge-status-pill');
    expect(pill).toBeTruthy();
    expect(pill?.classList.contains('forge-status-pill--auth')).toBe(true);
    expect(pill?.getAttribute('data-variant')).toBe('auth');
    expect(pill?.textContent).toContain('auth');
    // Subtext reflects the underlying cause.
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
    const { findAllByRole } = renderSection();
    await waitForFetch();

    const rows = await findAllByRole('radio');
    const pill = rows[0]?.querySelector('.forge-status-pill');
    expect(pill?.classList.contains('forge-status-pill--auth')).toBe(true);
    expect(rows[0]?.querySelector('.providers__sub')?.textContent).toBe('unconfigured');
  });

  // -------------------------------------------------------------------------
  // Radio semantics — preserved from F-586
  // -------------------------------------------------------------------------

  it('marks the active provider with aria-checked=true', async () => {
    setupInvokeMock({ active: 'anthropic' });
    const { findAllByRole } = renderSection();
    await waitForFetch();

    const rows = await findAllByRole('radio');
    const anthropic = rows.find((r) => r.textContent?.includes('Anthropic'));
    const ollama = rows.find((r) => r.textContent?.includes('Ollama'));
    expect(anthropic?.getAttribute('aria-checked')).toBe('true');
    expect(ollama?.getAttribute('aria-checked')).toBe('false');
  });

  it('renders no aria-checked row when active is null', async () => {
    setupInvokeMock({ active: null });
    const { findAllByRole } = renderSection();
    await waitForFetch();

    const rows = await findAllByRole('radio');
    for (const r of rows) {
      expect(r.getAttribute('aria-checked')).toBe('false');
    }
  });

  it('clicking a row invokes set_active_provider with that id', async () => {
    setupInvokeMock();
    const { findAllByRole } = renderSection();
    await waitForFetch();

    const rows = await findAllByRole('radio');
    const anthropic = rows.find((r) => r.textContent?.includes('Anthropic'))!;

    fireEvent.click(anthropic);
    await waitForFetch();

    expect(invokeMock).toHaveBeenCalledWith('set_active_provider', { providerId: 'anthropic' });
  });

  it('after a click, refetches list to reflect the new active state', async () => {
    let activeAfterSet: string | null = null;
    invokeMock.mockImplementation((cmd: string, args?: Record<string, unknown>) => {
      switch (cmd) {
        case 'dashboard_list_providers':
          return Promise.resolve(FOUR_BUILTINS);
        case 'get_active_provider':
          return Promise.resolve(activeAfterSet);
        case 'set_active_provider':
          activeAfterSet = String(args?.providerId);
          return Promise.resolve(undefined);
        default:
          return Promise.resolve(undefined);
      }
    });

    const { findAllByRole } = renderSection();
    await waitForFetch();

    const rows = await findAllByRole('radio');
    const openai = rows.find((r) => r.textContent?.includes('OpenAI'))!;

    fireEvent.click(openai);
    await waitForFetch();
    await waitForFetch();

    const rowsAfter = await findAllByRole('radio');
    const openaiAfter = rowsAfter.find((r) => r.textContent?.includes('OpenAI'));
    expect(openaiAfter?.getAttribute('aria-checked')).toBe('true');
  });

  it('surfaces set_active_provider rejection inline', async () => {
    setupInvokeMock({ setActiveError: 'unknown provider: xyz' });
    const { findAllByRole, findAllByRole: findAllByRoleAgain } = renderSection();
    await waitForFetch();

    const rows = await findAllByRole('radio');
    expect(rows[0]).toBeTruthy();
    fireEvent.click(rows[0]!);
    await waitForFetch();
    await waitForFetch();

    const alerts = await findAllByRoleAgain('alert');
    const text = alerts.map((a) => a.textContent ?? '').join(' ');
    expect(text).toMatch(/unknown provider/i);
  });

  it('drops a second click while the first switch is in flight', async () => {
    type Resolver = (value: unknown) => void;
    const resolverHolder: { fn: Resolver | null } = { fn: null };
    const firstSet = new Promise<unknown>((res) => {
      resolverHolder.fn = res;
    });
    let setActiveCallCount = 0;
    invokeMock.mockImplementation((cmd: string) => {
      switch (cmd) {
        case 'dashboard_list_providers':
          return Promise.resolve(FOUR_BUILTINS);
        case 'get_active_provider':
          return Promise.resolve(null);
        case 'set_active_provider':
          setActiveCallCount += 1;
          return firstSet;
        default:
          return Promise.resolve(undefined);
      }
    });

    const { findAllByRole } = renderSection();
    await waitForFetch();

    const rows = await findAllByRole('radio');
    const anthropic = rows.find((r) => r.textContent?.includes('Anthropic'))!;
    const openai = rows.find((r) => r.textContent?.includes('OpenAI'))!;

    fireEvent.click(anthropic);
    await waitForFetch();
    expect(setActiveCallCount).toBe(1);

    fireEvent.click(openai);
    await waitForFetch();
    expect(setActiveCallCount).toBe(1);

    resolverHolder.fn?.(undefined);
    await waitForFetch();
    await waitForFetch();

    const rowsAfter = await findAllByRole('radio');
    const openaiAfter = rowsAfter.find((r) => r.textContent?.includes('OpenAI'))!;
    fireEvent.click(openaiAfter);
    await waitForFetch();
    expect(setActiveCallCount).toBe(2);
  });

  it('marks the pending row with aria-busy while the IPC is in flight', async () => {
    type Resolver = (value: unknown) => void;
    const resolverHolder: { fn: Resolver | null } = { fn: null };
    const firstSet = new Promise<unknown>((res) => {
      resolverHolder.fn = res;
    });
    invokeMock.mockImplementation((cmd: string) => {
      switch (cmd) {
        case 'dashboard_list_providers':
          return Promise.resolve(FOUR_BUILTINS);
        case 'get_active_provider':
          return Promise.resolve(null);
        case 'set_active_provider':
          return firstSet;
        default:
          return Promise.resolve(undefined);
      }
    });

    const { findAllByRole } = renderSection();
    await waitForFetch();

    const rows = await findAllByRole('radio');
    const anthropic = rows.find((r) => r.textContent?.includes('Anthropic'))!;

    fireEvent.click(anthropic);
    await waitForFetch();

    const rowsNow = await findAllByRole('radio');
    const pending = rowsNow.find((r) => r.textContent?.includes('Anthropic'))!;
    expect(pending.getAttribute('aria-busy')).toBe('true');

    resolverHolder.fn?.(undefined);
    await waitForFetch();
    await waitForFetch();
  });

  it('keyboard activation (Space / Enter) on a row fires set_active_provider', async () => {
    setupInvokeMock();
    const { findAllByRole } = renderSection();
    await waitForFetch();

    const rows = await findAllByRole('radio');
    const openai = rows.find((r) => r.textContent?.includes('OpenAI'))!;

    fireEvent.keyDown(openai, { key: 'Enter' });
    await waitForFetch();

    expect(invokeMock).toHaveBeenCalledWith('set_active_provider', { providerId: 'openai' });
  });
});
