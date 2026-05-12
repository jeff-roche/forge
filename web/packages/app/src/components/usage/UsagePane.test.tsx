import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';
import type { UsageSummary } from '@forge/ipc';
import { UsagePane } from './UsagePane';
import { chartProviderClass } from './UsageChart';
import { sortByCostDesc } from './UsageModelTable';
import { formatMoney } from './format';
import { setInvokeForTesting } from '../../lib/tauri';

const invokeMock = vi.fn();

interface UsageMockBuilder {
  byProvider: UsageSummary;
  byModel: UsageSummary;
}

const emptySummary = (): UsageSummary =>
  ({
    range: { type: 'Last30' },
    group_by: 'Provider',
    total_tokens_in: 0 as unknown as bigint,
    total_tokens_out: 0 as unknown as bigint,
    total_cost: null,
    breakdown: [],
  }) as UsageSummary;

const summary = (
  groupBy: 'Provider' | 'Model',
  rows: Array<{
    key: string;
    in: number;
    out: number;
    cost: number | null;
  }>,
): UsageSummary => {
  const breakdown = rows.map((r) => ({
    key: r.key,
    tokens_in: r.in as unknown as bigint,
    tokens_out: r.out as unknown as bigint,
    cost: r.cost === null ? null : { amount: r.cost, currency: 'USD' },
  }));
  return {
    range: { type: 'Last30' },
    group_by: groupBy,
    total_tokens_in: rows.reduce((a, r) => a + r.in, 0) as unknown as bigint,
    total_tokens_out: rows.reduce((a, r) => a + r.out, 0) as unknown as bigint,
    total_cost: null,
    breakdown,
  } as UsageSummary;
};

function setupInvoke(mock: UsageMockBuilder) {
  invokeMock.mockImplementation(
    async (cmd: string, args?: Record<string, unknown>) => {
      if (cmd !== 'usage_summary') return undefined;
      const groupBy = args?.['groupBy'];
      return groupBy === 'Provider' ? mock.byProvider : mock.byModel;
    },
  );
}

async function flush() {
  for (let i = 0; i < 8; i += 1) {
    await Promise.resolve();
  }
}

beforeEach(() => {
  invokeMock.mockReset();
  setInvokeForTesting(invokeMock as never);
});

afterEach(() => {
  setInvokeForTesting(null);
  cleanup();
});

// ---------------------------------------------------------------------------
// Pure helpers — exercise the invariants without mounting the route.
// ---------------------------------------------------------------------------

describe('UsagePane helpers', () => {
  it('chartProviderClass: maps known providers to their CSS data-attribute', () => {
    expect(chartProviderClass('anthropic')).toBe('anthropic');
    expect(chartProviderClass('openai')).toBe('openai');
    expect(chartProviderClass('local')).toBe('local');
    expect(chartProviderClass('Anthropic')).toBe('anthropic');
  });

  it('chartProviderClass: unknown provider id falls back to custom', () => {
    expect(chartProviderClass('made-up-provider')).toBe('custom');
  });

  it('sortByCostDesc: rows with null cost sink below known-cost rows', () => {
    const rows = [
      { key: 'a', tokens_in: 1, tokens_out: 1, cost: null },
      { key: 'b', tokens_in: 1, tokens_out: 1, cost: { amount: 5, currency: 'USD' } },
      { key: 'c', tokens_in: 1, tokens_out: 1, cost: { amount: 3, currency: 'USD' } },
    ];
    const sorted = sortByCostDesc(rows);
    expect(sorted.map((r) => r.key)).toEqual(['b', 'c', 'a']);
  });

  it('sortByCostDesc: equal cost falls back to total tokens (desc)', () => {
    const rows = [
      { key: 'a', tokens_in: 100, tokens_out: 100, cost: { amount: 1, currency: 'USD' } },
      { key: 'b', tokens_in: 50, tokens_out: 50, cost: { amount: 1, currency: 'USD' } },
    ];
    const sorted = sortByCostDesc(rows);
    expect(sorted.map((r) => r.key)).toEqual(['a', 'b']);
  });

  it('formatMoney: null cost renders as the dash placeholder', () => {
    expect(formatMoney(null)).toBe('—');
  });

  it('formatMoney: known currency uses Intl currency formatting', () => {
    expect(formatMoney({ amount: 1.23, currency: 'USD' })).toMatch(/1\.23/);
  });

  it('formatMoney: bogus currency falls back to amount + code', () => {
    expect(formatMoney({ amount: 1, currency: 'NOTACURRENCY' })).toContain(
      'NOTACURRENCY',
    );
  });
});

// ---------------------------------------------------------------------------
// Component — loading / empty / ready / error coverage per #612 DoD.
// ---------------------------------------------------------------------------

describe('<UsagePane>', () => {
  it('renders a skeleton loading state during the IPC fetch (F-684)', async () => {
    invokeMock.mockImplementation(() => new Promise(() => undefined));
    const { findByTestId, queryByText } = render(() => (
      <UsagePane workspaceRoot="/ws/a" />
    ));
    const skeletonHost = await findByTestId('usage-loading');
    // Skeleton primitive carries `role="status"` + `aria-busy="true"`; one
    // for the chart, one for the table rows.
    const statuses = skeletonHost.querySelectorAll('[role="status"]');
    expect(statuses.length).toBe(2);
    for (const s of Array.from(statuses)) {
      expect(s.getAttribute('aria-busy')).toBe('true');
    }
    // Plain-text loading copy must be gone — F-684 replaces it with skeletons.
    expect(queryByText(/usage · probing/i)).toBeFalsy();
  });

  it('renders the empty state when no usage exists in range', async () => {
    setupInvoke({ byProvider: emptySummary(), byModel: emptySummary() });
    const { findByText } = render(() => (
      <UsagePane workspaceRoot="/ws/a" />
    ));
    await flush();
    expect(
      await findByText('// no usage recorded for this range'),
    ).toBeTruthy();
  });

  it('hydrates chart, limits, and model tables from one IPC fetch (two parallel calls)', async () => {
    setupInvoke({
      byProvider: summary('Provider', [
        { key: 'anthropic', in: 100, out: 50, cost: 0.5 },
        { key: 'openai', in: 200, out: 100, cost: 1.0 },
      ]),
      byModel: summary('Model', [
        { key: 'claude-3-5-sonnet', in: 100, out: 50, cost: 0.5 },
        { key: 'gpt-4o', in: 200, out: 100, cost: 1.0 },
      ]),
    });

    const { findByLabelText, findByText } = render(() => (
      <UsagePane workspaceRoot="/ws/a" />
    ));
    await flush();

    // The chart's role="img" exists with the aria-label including totals.
    expect(await findByLabelText(/Token usage by provider/i)).toBeTruthy();
    // Limits table has both providers.
    expect(await findByLabelText('Provider limits table')).toBeTruthy();
    expect(await findByText('anthropic')).toBeTruthy();
    expect(await findByText('openai')).toBeTruthy();
    // Per-model table has both models, and gpt-4o sorts first (higher cost).
    const modelTable = await findByLabelText('Per-model breakdown table');
    const firstRow = modelTable.querySelector('tbody tr');
    expect(firstRow?.textContent).toContain('gpt-4o');

    // Two IPC calls per render (Provider + Model).
    const calls = invokeMock.mock.calls.filter((c) => c[0] === 'usage_summary');
    expect(calls.length).toBe(2);
  });

  it('range selector changes refetch the summary with the new range', async () => {
    setupInvoke({ byProvider: emptySummary(), byModel: emptySummary() });
    const { findByText } = render(() => (
      <UsagePane workspaceRoot="/ws/a" />
    ));
    await flush();
    invokeMock.mockClear();

    const today = await findByText('Today');
    fireEvent.click(today);
    await flush();

    const calls = invokeMock.mock.calls.filter((c) => c[0] === 'usage_summary');
    expect(calls.length).toBeGreaterThan(0);
    for (const [, args] of calls) {
      expect((args as { range: { type: string } }).range.type).toBe('Today');
    }
  });

  it('range selector buttons use the @forge/design Button primitive', async () => {
    setupInvoke({ byProvider: emptySummary(), byModel: emptySummary() });
    const { findByText } = render(() => (
      <UsagePane workspaceRoot="/ws/a" />
    ));
    await flush();

    const today = (await findByText('Today')) as HTMLElement;
    // The primitive baseline class must be on the rendered button — proves
    // the raw <button> was replaced with the design-system Button.
    expect(today.classList.contains('forge-button')).toBe(true);
    expect(today.classList.contains('forge-button--ghost')).toBe(true);
    // Active range is reflected via aria-pressed (default 'last30').
    const last30 = (await findByText('Last 30')) as HTMLElement;
    expect(last30.getAttribute('aria-pressed')).toBe('true');
    expect(today.getAttribute('aria-pressed')).toBe('false');
  });

  it('Retry button uses the @forge/design Button primitive', async () => {
    invokeMock.mockImplementation(() =>
      Promise.reject(new Error('shell offline')),
    );
    const { findByText } = render(() => (
      <UsagePane workspaceRoot="/ws/a" />
    ));
    await flush();
    await flush();

    const retry = (await findByText('Retry')) as HTMLElement;
    expect(retry.classList.contains('forge-button')).toBe(true);
    expect(retry.classList.contains('forge-button--primary')).toBe(true);
  });

  it('cross-workspace toggle flips the IPC argument', async () => {
    setupInvoke({ byProvider: emptySummary(), byModel: emptySummary() });
    const { findByLabelText } = render(() => (
      <UsagePane workspaceRoot="/ws/a" />
    ));
    await flush();
    invokeMock.mockClear();

    const checkbox = (await findByLabelText(/cross-workspace/i)) as HTMLInputElement;
    fireEvent.click(checkbox);
    await flush();

    const calls = invokeMock.mock.calls.filter((c) => c[0] === 'usage_summary');
    expect(calls.length).toBeGreaterThan(0);
    for (const [, args] of calls) {
      expect((args as { crossWorkspace: boolean }).crossWorkspace).toBe(true);
    }
  });

  it('renders the configured cap and progress bar when limits are passed', async () => {
    setupInvoke({
      byProvider: summary('Provider', [
        { key: 'anthropic', in: 1000, out: 1000, cost: 5 },
      ]),
      byModel: summary('Model', [
        { key: 'claude-3-5-sonnet', in: 1000, out: 1000, cost: 5 },
      ]),
    });

    const { findByRole } = render(() => (
      <UsagePane
        workspaceRoot="/ws/a"
        limits={[
          { provider: 'anthropic', cap: { amount: 10, currency: 'USD' } },
        ]}
      />
    ));
    await flush();

    const progress = await findByRole('progressbar');
    // 5 / 10 = 50%
    expect(progress.getAttribute('aria-valuenow')).toBe('50');
  });

  it('progress state escalates to "warn" then "exceeded" past the cap thresholds', async () => {
    setupInvoke({
      byProvider: summary('Provider', [
        { key: 'anthropic', in: 1000, out: 1000, cost: 9 },
        { key: 'openai', in: 1000, out: 1000, cost: 11 },
      ]),
      byModel: summary('Model', []),
    });

    const { findAllByRole } = render(() => (
      <UsagePane
        workspaceRoot="/ws/a"
        limits={[
          { provider: 'anthropic', cap: { amount: 10, currency: 'USD' } },
          { provider: 'openai', cap: { amount: 10, currency: 'USD' } },
        ]}
      />
    ));
    await flush();

    const bars = await findAllByRole('progressbar');
    const states = bars.map(
      (b) => b.querySelector('.usage-pane__progress-fill')?.getAttribute('data-state'),
    );
    // 9/10 = 90% → warn; 11/10 = 110% → exceeded (capped to 100% width).
    expect(states).toContain('warn');
    expect(states).toContain('exceeded');
  });

  it('renders an error block when the IPC rejects', async () => {
    invokeMock.mockImplementation(() =>
      Promise.reject(new Error('shell offline')),
    );
    const { findByRole } = render(() => (
      <UsagePane workspaceRoot="/ws/a" />
    ));
    await flush();
    await flush();

    const alert = await findByRole('alert');
    expect(alert.textContent).toMatch(/USAGE UNAVAILABLE/i);
    expect(alert.textContent).toMatch(/shell offline/i);
  });

  it('limits-only providers (zero usage but configured cap) still render a row', async () => {
    // The empty-state guard hides the limits table when total tokens are
    // zero, so we drive a tiny non-zero summary on a different provider
    // and verify the configured-cap row is added alongside it.
    setupInvoke({
      byProvider: summary('Provider', [
        { key: 'openai', in: 10, out: 10, cost: 0.1 },
      ]),
      byModel: summary('Model', []),
    });
    const { findByText } = render(() => (
      <UsagePane
        workspaceRoot="/ws/a"
        limits={[
          { provider: 'anthropic', cap: { amount: 50, currency: 'USD' } },
        ]}
      />
    ));
    await flush();

    // Both rows visible — anthropic (cap-only) and openai (usage-only).
    expect(await findByText('anthropic')).toBeTruthy();
    expect(await findByText('openai')).toBeTruthy();
  });

  it('passes workspaceRoot through to the IPC call when not cross-workspace', async () => {
    setupInvoke({ byProvider: emptySummary(), byModel: emptySummary() });
    render(() => <UsagePane workspaceRoot="/ws/specific" />);
    await flush();

    const calls = invokeMock.mock.calls.filter((c) => c[0] === 'usage_summary');
    expect(calls.length).toBeGreaterThan(0);
    for (const [, args] of calls) {
      expect((args as { workspaceRoot: string | null }).workspaceRoot).toBe(
        '/ws/specific',
      );
      expect((args as { crossWorkspace: boolean }).crossWorkspace).toBe(false);
    }
  });
});

// ---------------------------------------------------------------------------
// Limits table — pure rendering + aggregation correctness
// ---------------------------------------------------------------------------

import { UsageLimitsTable } from './UsageLimitsTable';

describe('<UsageLimitsTable>', () => {
  it('renders "// not configured" when no cap matches a provider row', async () => {
    const { findByText } = render(() => (
      <UsageLimitsTable
        byProvider={[
          {
            key: 'anthropic',
            tokens_in: 100,
            tokens_out: 50,
            cost: { amount: 0.5, currency: 'USD' },
          },
        ]}
      />
    ));
    expect(await findByText('// not configured')).toBeTruthy();
  });

  it('renders the empty state when no providers and no caps exist', async () => {
    const { findByText } = render(() => (
      <UsageLimitsTable byProvider={[]} />
    ));
    expect(await findByText('// no providers in range')).toBeTruthy();
  });
});
