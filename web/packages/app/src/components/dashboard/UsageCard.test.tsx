// F-722: UsageCard component tests.
//
// Covers the four interaction states from `docs/ui-specs/dashboard.md
// §Usage card §States` — loading, empty, error, ready — plus the Δ%
// semantic (spend ↑ → warning, tokens-out ↑ → success), the 7D/30D/MTD
// range toggle, and the prior-period IPC fan-out.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';
import type { UsageRange, UsageSummary } from '@forge/ipc';
import { UsageCard, deltaFor, formatTokens, resolveRangeWindow } from './UsageCard';
import { setActiveWorkspaceRoot } from '../../stores/session';
import { setInvokeForTesting } from '../../lib/tauri';

interface Bucket {
  in: number;
  out: number;
  cost: number | null;
}

const ZERO: Bucket = { in: 0, out: 0, cost: null };

function summaryFor(bucket: Bucket, range: UsageRange = { type: 'Last7' }): UsageSummary {
  return {
    range,
    group_by: 'Provider',
    total_tokens_in: bucket.in as unknown as bigint,
    total_tokens_out: bucket.out as unknown as bigint,
    total_cost: bucket.cost === null ? null : { amount: bucket.cost, currency: 'USD' },
    breakdown: [],
  } as UsageSummary;
}

/** Stage two-call invoke. The first `usage_summary` call (current window)
 *  resolves with `current`; the second (prior window) resolves with `prior`.
 *  Helpers like `Promise.all` may resolve them out-of-order; the IPC `range`
 *  payload disambiguates so the right summary maps to the right window. */
function stageInvoke(
  curr: Bucket,
  prior: Bucket,
  opts: { error?: Error } = {},
): ReturnType<typeof vi.fn> {
  return vi.fn(async (cmd: string, args?: Record<string, unknown>) => {
    if (cmd !== 'usage_summary') return undefined;
    if (opts.error) throw opts.error;
    const range = (args?.['range'] as UsageRange) ?? { type: 'Last7' };
    // Current windows: `Last7` / `Last30` or the (later) custom range whose
    // `end` is "today + 1d"; prior windows are always `CustomRange` and
    // their `end` lands at the start of the current window.
    if (range.type === 'Last7' || range.type === 'Last30') {
      return summaryFor(curr, range);
    }
    if (range.type === 'CustomRange') {
      // Prior period in our scheme always starts strictly before the current
      // window — detect by comparing against `now`. MTD's current range also
      // uses CustomRange; differentiate by the `end` being "today + 1 day".
      const end = new Date(range.end);
      const now = Date.now();
      const isCurrent = end.getTime() > now;
      return summaryFor(isCurrent ? curr : prior, range);
    }
    return summaryFor(ZERO, range);
  });
}

beforeEach(() => {
  setActiveWorkspaceRoot('/ws/test');
});

afterEach(() => {
  setInvokeForTesting(null);
  cleanup();
});

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

describe('UsageCard helpers', () => {
  it('formatTokens: M / K / raw tiers', () => {
    expect(formatTokens(4_200_000)).toEqual({ num: '4.2', unit: 'M' });
    expect(formatTokens(1_100)).toEqual({ num: '1.1', unit: 'K' });
    expect(formatTokens(42)).toEqual({ num: '42', unit: null });
  });

  it('deltaFor: returns null when prior is zero (no denominator)', () => {
    expect(deltaFor(100, 0)).toBeNull();
  });

  it('deltaFor: classifies direction by sign', () => {
    expect(deltaFor(110, 100)?.direction).toBe('up');
    expect(deltaFor(90, 100)?.direction).toBe('dn');
    expect(deltaFor(100, 100)?.direction).toBe('flat');
  });

  it('resolveRangeWindow: 7D uses `Last7` for current and a 7-day prior window', () => {
    const w = resolveRangeWindow('7D', new Date('2026-05-10T12:00:00Z'));
    expect(w.current).toEqual({ type: 'Last7' });
    expect(w.days).toBe(7);
    expect(w.prior.type).toBe('CustomRange');
  });

  it('resolveRangeWindow: MTD covers month-start through today', () => {
    const w = resolveRangeWindow('MTD', new Date('2026-05-10T12:00:00Z'));
    // 1st through 10th inclusive == 10 days.
    expect(w.days).toBe(10);
    expect(w.current.type).toBe('CustomRange');
    expect(w.fragment).toBe('month-to-date');
  });
});

// ---------------------------------------------------------------------------
// State matrix — loading / empty / error / ready
// ---------------------------------------------------------------------------

describe('UsageCard states', () => {
  it('renders the loading skeletons until the IPC resolves', async () => {
    let release: (() => void) | null = null;
    const pending = new Promise<void>((r) => (release = r));
    const mock = vi.fn(async (cmd: string) => {
      if (cmd !== 'usage_summary') return undefined;
      await pending;
      return summaryFor(ZERO);
    });
    setInvokeForTesting(mock as never);

    const { queryByTestId } = render(() => <UsageCard />);
    // Skeleton scaffolding rendered immediately.
    expect(queryByTestId('usage-card-loading')).not.toBeNull();
    expect(queryByTestId('usage-card-empty')).toBeNull();
    expect(queryByTestId('usage-card-error')).toBeNull();
    release!();
    await waitFor(() => expect(queryByTestId('usage-card-loading')).toBeNull());
  });

  it('renders the verbatim empty copy when totals are zero', async () => {
    setInvokeForTesting(stageInvoke(ZERO, ZERO) as never);
    const { findByTestId } = render(() => <UsageCard />);
    const empty = await findByTestId('usage-card-empty');
    // Verbatim per spec §States.
    expect(empty.textContent).toBe('No usage recorded yet.');
  });

  it('renders the error block + verbatim title + Retry action when the IPC throws', async () => {
    const mock = stageInvoke(ZERO, ZERO, { error: new Error('boom') });
    setInvokeForTesting(mock as never);

    const { findByTestId, queryByTestId } = render(() => <UsageCard />);
    const error = await findByTestId('usage-card-error');
    // Verbatim title per spec §States.
    expect(error.textContent).toContain("Couldn't load usage.");
    // Verbatim error message survives — F-673 contract.
    expect(error.textContent).toContain('boom');
    // KPI grid is replaced; spark suppresses (per spec).
    expect(queryByTestId('usage-card-tile-spend')).toBeNull();
    expect(queryByTestId('usage-card-spark')).toBeNull();
    // Retry CTA is present and callable.
    const retry = await findByTestId('usage-card-retry');
    expect(retry).toBeTruthy();
  });

  it('renders all three KPI tiles + spark when usage data is non-zero', async () => {
    setInvokeForTesting(
      stageInvoke({ in: 4_200_000, out: 1_100_000, cost: 18.42 }, ZERO) as never,
    );
    const { findByTestId } = render(() => <UsageCard />);
    const spend = await findByTestId('usage-card-tile-spend');
    const tokensIn = await findByTestId('usage-card-tile-tokens-in');
    const tokensOut = await findByTestId('usage-card-tile-tokens-out');
    expect(spend.textContent).toContain('Spend');
    expect(spend.textContent).toContain('$18.42');
    expect(tokensIn.textContent).toContain('4.2');
    expect(tokensIn.textContent).toContain('M');
    expect(tokensOut.textContent).toContain('1.1');
    expect(await findByTestId('usage-card-spark')).toBeTruthy();
  });

  it('marks the latest spark point with the live-dot class so the pulse animates (F-740)', async () => {
    setInvokeForTesting(
      stageInvoke({ in: 1_000, out: 1_000, cost: 1 }, ZERO) as never,
    );
    const { findByTestId } = render(() => <UsageCard />);
    const spark = await findByTestId('usage-card-spark');
    const liveDots = spark.querySelectorAll('.usage-card__spark-live-dot');
    // Exactly one — the last point.
    expect(liveDots.length).toBe(1);
    // And it's the last <circle> in document order.
    const circles = spark.querySelectorAll('circle');
    const last = circles[circles.length - 1];
    expect(last).toBeDefined();
    expect(last!.classList.contains('usage-card__spark-live-dot')).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// Δ% semantic + range toggle
// ---------------------------------------------------------------------------

describe('UsageCard Δ% colour semantic', () => {
  it('Spend ↑ vs prior period paints warning; Tokens-out ↑ paints success', async () => {
    setInvokeForTesting(
      stageInvoke(
        { in: 200, out: 200, cost: 20 }, // current: 2x prior on every axis
        { in: 100, out: 100, cost: 10 },
      ) as never,
    );

    const { findByTestId } = render(() => <UsageCard />);

    const spendDelta = await findByTestId('usage-card-tile-spend-delta');
    expect(spendDelta.className).toContain('usage-card__delta--warn');

    const outDelta = await findByTestId('usage-card-tile-tokens-out-delta');
    expect(outDelta.className).toContain('usage-card__delta--ok');
  });
});

describe('UsageCard range toggle', () => {
  it('defaults to 7D and triggers a refetch when 30D is selected', async () => {
    const mock = stageInvoke(
      { in: 100, out: 100, cost: 1 },
      { in: 50, out: 50, cost: 0.5 },
    );
    setInvokeForTesting(mock as never);

    const { findByTestId } = render(() => <UsageCard />);
    await findByTestId('usage-card-tile-spend');

    const callsAfterMount = mock.mock.calls.length;
    expect(callsAfterMount).toBe(2); // current + prior

    const thirtyButton = await findByTestId('usage-card-range-30d');
    expect(thirtyButton.getAttribute('aria-checked')).toBe('false');
    const sevenButton = await findByTestId('usage-card-range-7d');
    expect(sevenButton.getAttribute('aria-checked')).toBe('true');

    fireEvent.click(thirtyButton);

    await waitFor(() => {
      expect(thirtyButton.getAttribute('aria-checked')).toBe('true');
      // Toggle fires another current+prior pair.
      expect(mock.mock.calls.length).toBeGreaterThanOrEqual(callsAfterMount + 2);
    });
  });
});
