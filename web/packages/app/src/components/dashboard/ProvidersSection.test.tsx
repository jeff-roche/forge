import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { cleanup, render, fireEvent } from '@solidjs/testing-library';
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
  sample({ id: 'ollama', display_name: 'Ollama' }),
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
    model_available: false,
  }),
  sample({
    id: 'custom_openai',
    display_name: 'Custom OpenAI-compat',
    credential_required: true,
    has_credential: false,
    model_available: false,
  }),
];

/**
 * Default invoke shim: routes by command name.
 * - `list_providers` → resolves with whatever was stubbed last
 * - `get_active_provider` → resolves with the active id stub
 * - `set_active_provider` → resolves undefined
 */
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

beforeEach(() => {
  invokeMock.mockReset();
  setInvokeForTesting(invokeMock as never);
});

afterEach(() => {
  setInvokeForTesting(null);
  cleanup();
});

describe('ProvidersSection (F-586)', () => {
  it('renders a skeleton loading state during the IPC fetch (F-684)', async () => {
    // Pending forever — the resource never settles, so the snapshot stays in
    // its `loading` branch.
    invokeMock.mockImplementation(() => new Promise(() => undefined));
    const { findByTestId, queryByText } = render(() => <ProvidersSection />);

    const skeleton = await findByTestId('providers-loading');
    expect(skeleton.getAttribute('role')).toBe('status');
    expect(skeleton.getAttribute('aria-busy')).toBe('true');
    // Plain-text "providers · probing" copy must be gone — replaced by
    // a card-shaped skeleton matching the live grid layout.
    expect(queryByText(/providers · probing/i)).toBeFalsy();
    // The skeleton renders one placeholder per expected card slot.
    expect(skeleton.querySelectorAll('.forge-skeleton--card').length).toBe(4);
  });

  it('renders a card per provider returned by list_providers', async () => {
    setupInvokeMock();
    const { findAllByRole } = render(() => <ProvidersSection />);
    await waitForFetch();

    const radios = await findAllByRole('radio');
    expect(radios.length).toBe(4);
    const labels = radios.map((r) => r.textContent ?? '');
    expect(labels.some((l) => l.includes('Ollama'))).toBe(true);
    expect(labels.some((l) => l.includes('Anthropic'))).toBe(true);
    expect(labels.some((l) => l.includes('OpenAI'))).toBe(true);
    expect(labels.some((l) => l.includes('Custom OpenAI-compat'))).toBe(true);
  });

  it('marks the active provider with aria-checked=true', async () => {
    setupInvokeMock({ active: 'anthropic' });
    const { findAllByRole } = render(() => <ProvidersSection />);
    await waitForFetch();

    const radios = await findAllByRole('radio');
    const anthropic = radios.find((r) => r.textContent?.includes('Anthropic'));
    const ollama = radios.find((r) => r.textContent?.includes('Ollama'));
    expect(anthropic?.getAttribute('aria-checked')).toBe('true');
    expect(ollama?.getAttribute('aria-checked')).toBe('false');
  });

  it('renders no aria-checked card when active is null', async () => {
    setupInvokeMock({ active: null });
    const { findAllByRole } = render(() => <ProvidersSection />);
    await waitForFetch();

    const radios = await findAllByRole('radio');
    for (const r of radios) {
      expect(r.getAttribute('aria-checked')).toBe('false');
    }
  });

  it('shows credential warning glyph only when required and missing', async () => {
    setupInvokeMock();
    const { findAllByRole } = render(() => <ProvidersSection />);
    await waitForFetch();

    const radios = await findAllByRole('radio');
    const anthropic = radios.find((r) => r.textContent?.includes('Anthropic'));
    const openai = radios.find((r) => r.textContent?.includes('OpenAI'));
    const ollama = radios.find((r) => r.textContent?.includes('Ollama'));

    // Anthropic: required + absent → warning glyph
    expect(anthropic?.querySelector('[aria-label="credential missing"]')).toBeTruthy();
    // OpenAI in the fixture: required + present → check glyph
    expect(openai?.querySelector('[aria-label="credential present"]')).toBeTruthy();
    // Ollama: not required → neither glyph
    expect(ollama?.querySelector('[aria-label="credential missing"]')).toBeFalsy();
    expect(ollama?.querySelector('[aria-label="credential present"]')).toBeFalsy();
  });

  it('clicking a card invokes set_active_provider with that id', async () => {
    setupInvokeMock();
    const { findAllByRole } = render(() => <ProvidersSection />);
    await waitForFetch();

    const radios = await findAllByRole('radio');
    const anthropic = radios.find((r) => r.textContent?.includes('Anthropic'));
    expect(anthropic).toBeTruthy();

    fireEvent.click(anthropic!);
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

    const { findAllByRole } = render(() => <ProvidersSection />);
    await waitForFetch();

    const radios = await findAllByRole('radio');
    const openai = radios.find((r) => r.textContent?.includes('OpenAI'))!;

    fireEvent.click(openai);
    await waitForFetch();
    await waitForFetch();

    const radiosAfter = await findAllByRole('radio');
    const openaiAfter = radiosAfter.find((r) => r.textContent?.includes('OpenAI'));
    expect(openaiAfter?.getAttribute('aria-checked')).toBe('true');
  });

  it('surfaces set_active_provider rejection inline', async () => {
    setupInvokeMock({ setActiveError: 'unknown provider: xyz' });
    const { findAllByRole, findByRole } = render(() => <ProvidersSection />);
    await waitForFetch();

    const radios = await findAllByRole('radio');
    expect(radios[0]).toBeTruthy();
    fireEvent.click(radios[0]!);
    await waitForFetch();
    await waitForFetch();

    const alert = await findByRole('alert');
    expect(alert.textContent).toMatch(/unknown provider/i);
  });

  it('surfaces list_providers rejection as a "providers unavailable" block', async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'dashboard_list_providers') return Promise.reject(new Error('keyring backend down'));
      if (cmd === 'get_active_provider') return Promise.resolve(null);
      return Promise.resolve(undefined);
    });

    const { findByRole } = render(() => <ProvidersSection />);
    await waitForFetch();
    await waitForFetch();

    const alert = await findByRole('alert');
    expect(alert.textContent).toMatch(/providers unavailable/i);
    expect(alert.textContent).toMatch(/keyring backend down/i);
  });

  it('empty-state uses canonical mono noun-phrase copy when no providers configured (F-691)', async () => {
    setupInvokeMock({ entries: [], active: null });
    const { findByTestId } = render(() => <ProvidersSection />);
    await waitForFetch();
    const empty = await findByTestId('providers-empty');
    expect(empty.textContent).toBe('// no providers configured');
  });

  it('drops a second click while the first switch is in flight', async () => {
    // F-586 review: double-tap guard. The first `setActiveProvider` IPC
    // is still pending (its promise hasn't resolved), so a second click
    // on a different card must NOT fire another invocation. Once the
    // first round-trip completes the UI is unlocked again.
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
          return firstSet; // never resolves until we say so
        default:
          return Promise.resolve(undefined);
      }
    });

    const { findAllByRole } = render(() => <ProvidersSection />);
    await waitForFetch();

    const radios = await findAllByRole('radio');
    const anthropic = radios.find((r) => r.textContent?.includes('Anthropic'))!;
    const openai = radios.find((r) => r.textContent?.includes('OpenAI'))!;

    // First click — fires the IPC. Pending state engages.
    fireEvent.click(anthropic);
    await waitForFetch();
    expect(setActiveCallCount).toBe(1);

    // Second click on a different card while the first is still in
    // flight — must be ignored.
    fireEvent.click(openai);
    await waitForFetch();
    expect(setActiveCallCount).toBe(1);

    // Resolve the first call; UI unlocks.
    resolverHolder.fn?.(undefined);
    await waitForFetch();
    await waitForFetch();

    // Subsequent click now fires.
    const radiosAfter = await findAllByRole('radio');
    const openaiAfter = radiosAfter.find((r) => r.textContent?.includes('OpenAI'))!;
    fireEvent.click(openaiAfter);
    await waitForFetch();
    expect(setActiveCallCount).toBe(2);
  });

  describe('voice & terminology (F-690)', () => {
    it('renders "unconfigured" not "no model" when a provider has no model_available', async () => {
      setupInvokeMock();
      const { findAllByRole } = render(() => <ProvidersSection />);
      await waitForFetch();

      const radios = await findAllByRole('radio');
      const anthropic = radios.find((r) => r.textContent?.includes('Anthropic'))!;
      expect(anthropic.textContent).toContain('unconfigured');
      expect(anthropic.textContent).not.toMatch(/no model/i);
    });

    it('renders "ready" not "model ready" when model_available is true with no model name', async () => {
      setupInvokeMock({
        entries: [sample({ id: 'ollama', display_name: 'Ollama', model_available: true })],
      });
      const { findAllByRole } = render(() => <ProvidersSection />);
      await waitForFetch();

      const radios = await findAllByRole('radio');
      expect(radios[0]?.textContent).toContain('ready');
      expect(radios[0]?.textContent).not.toMatch(/model ready/i);
    });

    it('aria-label uses provider terminology, never "model"', async () => {
      setupInvokeMock();
      const { findAllByRole } = render(() => <ProvidersSection />);
      await waitForFetch();

      const radios = await findAllByRole('radio');
      const anthropic = radios.find((r) => r.textContent?.includes('Anthropic'))!;
      const label = anthropic.getAttribute('aria-label') ?? '';
      expect(label).not.toMatch(/model/i);
      expect(label.toLowerCase()).toContain('unconfigured');
    });
  });

  it('marks the pending card with aria-busy while the IPC is in flight', async () => {
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

    const { findAllByRole } = render(() => <ProvidersSection />);
    await waitForFetch();

    const radios = await findAllByRole('radio');
    const anthropic = radios.find((r) => r.textContent?.includes('Anthropic'))!;

    fireEvent.click(anthropic);
    await waitForFetch();

    // The clicked card carries aria-busy=true; siblings stay
    // aria-busy=false but are disabled.
    const radiosNow = await findAllByRole('radio');
    const pending = radiosNow.find((r) => r.textContent?.includes('Anthropic'))!;
    expect(pending.getAttribute('aria-busy')).toBe('true');

    resolverHolder.fn?.(undefined);
    await waitForFetch();
    await waitForFetch();
  });
});
