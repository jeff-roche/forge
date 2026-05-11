// F-722: Dashboard Usage card.
//
// Renders three KPI tiles (spend / tokens in / tokens out) plus a spark
// chart over the selected time range. Range toggle is local — defaults to
// `7D`, also supports `30D` and `MTD` (month-to-date). Each range change
// fans out two parallel `usage_summary` IPC calls: one for the current
// window, one for the prior-period window of equal length, so the Δ% line
// on each KPI can compare against a meaningful baseline.
//
// The backend's `usage_summary` does not (yet) expose a per-day series —
// `GroupBy` only buckets by Provider / Model / Scope. The spark chart is
// therefore rendered as a flat baseline derived from the period total; a
// TODO marks the per-day-series IPC follow-up. The empty-baseline visual
// holds for both `empty` and `ready` states until the per-day IPC lands.

import {
  createMemo,
  createResource,
  createSignal,
  For,
  Show,
  type Component,
  type JSX,
} from 'solid-js';
import { Button, Skeleton, Tab, Tabs } from '@forge/design';
import type { Money, UsageRange } from '@forge/ipc';
import {
  fetchUsageSummary,
  type UsageSummaryView,
} from '../../ipc/usage';
import { activeWorkspaceRoot } from '../../stores/session';
import { formatMoney } from '../usage/format';
import './UsageCard.css';

// ---------------------------------------------------------------------------
// Range toggle
// ---------------------------------------------------------------------------

export type RangeKey = '7D' | '30D' | 'MTD';

const RANGE_OPTIONS: ReadonlyArray<RangeKey> = ['7D', '30D', 'MTD'];

interface RangeWindow {
  current: UsageRange;
  prior: UsageRange;
  /** Whole-day count, used to size the spark chart x-axis and the prior
   *  window. `MTD` resolves at request time so the value reflects "today". */
  days: number;
  /** Header fragment — the spec demands `Usage · last <fragment>`. */
  fragment: string;
}

/** Snap `date` to the start of its UTC calendar day. */
function startOfUtcDay(date: Date): Date {
  return new Date(Date.UTC(date.getUTCFullYear(), date.getUTCMonth(), date.getUTCDate()));
}

/** Resolve the current + prior windows for `key`. `now` is injected for tests. */
export function resolveRangeWindow(key: RangeKey, now: Date = new Date()): RangeWindow {
  if (key === '7D') {
    return {
      current: { type: 'Last7' },
      prior: priorCustom(now, 7, 7),
      days: 7,
      fragment: '7 days',
    };
  }
  if (key === '30D') {
    return {
      current: { type: 'Last30' },
      prior: priorCustom(now, 30, 30),
      days: 30,
      fragment: '30 days',
    };
  }
  // MTD — first of the current UTC month through `now`. Prior window is the
  // same number of days, ending at the start of the current month.
  const today = startOfUtcDay(now);
  const monthStart = new Date(Date.UTC(today.getUTCFullYear(), today.getUTCMonth(), 1));
  const days = Math.max(1, Math.round((today.getTime() - monthStart.getTime()) / 86_400_000) + 1);
  return {
    current: {
      type: 'CustomRange',
      start: monthStart.toISOString(),
      end: new Date(today.getTime() + 86_400_000).toISOString(),
    },
    prior: {
      type: 'CustomRange',
      start: new Date(monthStart.getTime() - days * 86_400_000).toISOString(),
      end: monthStart.toISOString(),
    },
    days,
    fragment: 'month-to-date',
  };
}

function priorCustom(now: Date, currentDays: number, priorDays: number): UsageRange {
  const today = startOfUtcDay(now);
  const currentStart = new Date(today.getTime() - (currentDays - 1) * 86_400_000);
  const priorEnd = currentStart;
  const priorStart = new Date(priorEnd.getTime() - priorDays * 86_400_000);
  return {
    type: 'CustomRange',
    start: priorStart.toISOString(),
    end: priorEnd.toISOString(),
  };
}

// ---------------------------------------------------------------------------
// Data fetch — current + prior periods in parallel
// ---------------------------------------------------------------------------

interface FetchKey {
  rangeKey: RangeKey;
  workspaceRoot: string | null;
  // Tick to force a refetch from the Retry button.
  tick: number;
}

interface UsageCardData {
  current: UsageSummaryView;
  prior: UsageSummaryView;
  window: RangeWindow;
}

async function fetchUsage(key: FetchKey): Promise<UsageCardData> {
  const window = resolveRangeWindow(key.rangeKey);
  const args = {
    groupBy: 'Provider' as const,
    crossWorkspace: false,
    workspaceRoot: key.workspaceRoot,
  };
  const [current, prior] = await Promise.all([
    fetchUsageSummary({ ...args, range: window.current }),
    fetchUsageSummary({ ...args, range: window.prior }),
  ]);
  return { current, prior, window };
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

/** Compact integer with `K`/`M` suffix — matches the mock's `4.2M` voice. */
export function formatTokens(value: number): { num: string; unit: string | null } {
  if (value >= 1_000_000) return { num: (value / 1_000_000).toFixed(1), unit: 'M' };
  if (value >= 1_000) return { num: (value / 1_000).toFixed(1), unit: 'K' };
  return { num: String(value), unit: null };
}

export interface DeltaView {
  pct: number;
  direction: 'up' | 'dn' | 'flat';
}

/** Percentage change. `null` when the prior period had no data — Δ has no
 *  meaningful denominator and the UI hides the line rather than printing
 *  `+∞%`. A zero current against a non-zero prior is `-100%`. */
export function deltaFor(current: number, prior: number): DeltaView | null {
  if (prior === 0) return null;
  const pct = ((current - prior) / prior) * 100;
  if (Math.abs(pct) < 0.5) return { pct: 0, direction: 'flat' };
  return { pct, direction: pct > 0 ? 'up' : 'dn' };
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export const UsageCard: Component = () => {
  const [rangeKey, setRangeKey] = createSignal<RangeKey>('7D');
  const [retryTick, setRetryTick] = createSignal(0);

  const fetchKey = createMemo<FetchKey>(() => ({
    rangeKey: rangeKey(),
    workspaceRoot: activeWorkspaceRoot(),
    tick: retryTick(),
  }));

  const [data, { refetch }] = createResource(fetchKey, fetchUsage);

  const dataValue = (): UsageCardData | undefined =>
    data.state === 'ready' ? data() : undefined;

  const errorDetail = (): string | null => {
    const err = data.error;
    if (!err) return null;
    return err instanceof Error ? `Error: ${err.message}` : String(err);
  };

  const isEmpty = createMemo<boolean>(() => {
    const d = dataValue();
    if (!d) return false;
    const c = d.current;
    return c.total_tokens_in + c.total_tokens_out === 0 && !c.total_cost;
  });

  const retry = () => {
    setRetryTick((t) => t + 1);
    void refetch();
  };

  const headerFragment = (): string => dataValue()?.window.fragment ?? rangeFragmentFor(rangeKey());

  return (
    <section
      class="usage-card"
      aria-label="Usage summary"
      data-testid="usage-card"
    >
      <header class="usage-card__head">
        <span class="usage-card__label">
          Usage <span class="usage-card__label-sep">·</span>{' '}
          <span class="usage-card__label-range">last {headerFragment()}</span>
        </span>
        <Tabs
          variant="radio"
          class="usage-card__ranges"
          aria-label="Usage time range"
        >
          <For each={RANGE_OPTIONS}>
            {(opt) => (
              <Tab
                variant="radio"
                selected={rangeKey() === opt}
                class="usage-card__range"
                classList={{ 'usage-card__range--selected': rangeKey() === opt }}
                onClick={() => setRangeKey(opt)}
                data-testid={`usage-card-range-${opt.toLowerCase()}`}
              >
                {opt}
              </Tab>
            )}
          </For>
        </Tabs>
      </header>

      <Show when={data.loading}>
        <div class="usage-card__body" data-testid="usage-card-loading">
          <Skeleton
            variant="block"
            count={3}
            label="Loading usage tiles"
            class="usage-card__kpi-skeleton"
          />
          <Skeleton
            variant="block"
            count={1}
            label="Loading usage trend"
            class="usage-card__spark-skeleton"
          />
        </div>
      </Show>

      <Show when={errorDetail()}>
        {(detail) => (
          <div class="usage-card__body usage-card__body--error">
            <div
              class="usage-card__error"
              role="alert"
              data-testid="usage-card-error"
            >
              <p class="usage-card__error-title">Couldn't load usage.</p>
              <p class="usage-card__error-detail">{detail()}</p>
              <Button
                variant="primary"
                size="sm"
                onClick={retry}
                data-testid="usage-card-retry"
              >
                Retry
              </Button>
            </div>
          </div>
        )}
      </Show>

      <Show when={dataValue()}>
        {(d) => (
          <div class="usage-card__body">
            <div class="usage-card__kpis" role="list">
              <SpendTile
                current={d().current.total_cost}
                prior={d().prior.total_cost}
                empty={isEmpty()}
              />
              <TokenTile
                label="Tokens in"
                current={d().current.total_tokens_in}
                prior={d().prior.total_tokens_in}
                accent="info"
                adverseDirection="up"
                empty={isEmpty()}
                testid="usage-card-tile-tokens-in"
              />
              <TokenTile
                label="Tokens out"
                current={d().current.total_tokens_out}
                prior={d().prior.total_tokens_out}
                accent="ok"
                adverseDirection="dn"
                empty={isEmpty()}
                testid="usage-card-tile-tokens-out"
              />
            </div>
            <Show when={isEmpty()}>
              <p class="usage-card__empty" data-testid="usage-card-empty">
                No usage recorded yet.
              </p>
            </Show>
            <SparkChart days={d().window.days} empty={isEmpty()} />
          </div>
        )}
      </Show>
    </section>
  );
};

function rangeFragmentFor(key: RangeKey): string {
  if (key === '7D') return '7 days';
  if (key === '30D') return '30 days';
  return 'month-to-date';
}

// ---------------------------------------------------------------------------
// KPI tiles
// ---------------------------------------------------------------------------

type Accent = 'ember' | 'ok' | 'warn' | 'info';

interface SpendTileProps {
  current: Money | null;
  prior: Money | null;
  empty: boolean;
}

const SpendTile: Component<SpendTileProps> = (props) => {
  const delta = (): DeltaView | null => {
    if (props.empty) return null;
    return deltaFor(props.current?.amount ?? 0, props.prior?.amount ?? 0);
  };
  // Spend up == warning per spec D.5.1.
  return (
    <KpiTileShell
      label="Spend"
      accent="ember"
      delta={delta()}
      adverseDirection="up"
      testid="usage-card-tile-spend"
    >
      <span class="usage-card__value">
        {props.empty ? '$0.00' : formatMoney(props.current)}
      </span>
    </KpiTileShell>
  );
};

interface TokenTileProps {
  label: string;
  current: number;
  prior: number;
  accent: Accent;
  adverseDirection: 'up' | 'dn';
  empty: boolean;
  testid: string;
}

const TokenTile: Component<TokenTileProps> = (props) => {
  const formatted = (): { num: string; unit: string | null } =>
    props.empty ? { num: '0', unit: null } : formatTokens(props.current);
  const delta = (): DeltaView | null => {
    if (props.empty) return null;
    return deltaFor(props.current, props.prior);
  };
  return (
    <KpiTileShell
      label={props.label}
      accent={props.accent}
      delta={delta()}
      adverseDirection={props.adverseDirection}
      testid={props.testid}
    >
      <span class="usage-card__value">
        {formatted().num}
        <Show when={formatted().unit}>
          {(u) => <small class="usage-card__unit">{u()}</small>}
        </Show>
      </span>
    </KpiTileShell>
  );
};

interface KpiTileShellProps {
  label: string;
  accent: Accent;
  delta: DeltaView | null;
  adverseDirection: 'up' | 'dn';
  testid: string;
  children: JSX.Element;
}

const KpiTileShell: Component<KpiTileShellProps> = (props) => {
  const deltaClass = (): string => {
    const d = props.delta;
    if (!d || d.direction === 'flat') return 'usage-card__delta usage-card__delta--flat';
    const adverse = d.direction === props.adverseDirection;
    return `usage-card__delta usage-card__delta--${adverse ? 'warn' : 'ok'}`;
  };
  return (
    <article
      class={`usage-card__tile usage-card__tile--${props.accent}`}
      role="listitem"
      data-testid={props.testid}
    >
      <span class="usage-card__tile-label">{props.label}</span>
      {props.children}
      <Show when={props.delta} fallback={<span class="usage-card__delta usage-card__delta--hidden" aria-hidden="true" />}>
        {(d) => (
          <span class={deltaClass()} data-testid={`${props.testid}-delta`}>
            <Show when={d().direction !== 'flat'} fallback={'· 0%'}>
              {d().direction === 'up' ? '↑' : '↓'} {Math.abs(d().pct).toFixed(0)}%
            </Show>
          </span>
        )}
      </Show>
    </article>
  );
};

// ---------------------------------------------------------------------------
// Spark chart
// ---------------------------------------------------------------------------

interface SparkChartProps {
  days: number;
  empty: boolean;
}

/**
 * Inline SVG spark chart matching `DESIGN.md §Spark chart`. The viewBox is
 * fixed at `0 0 320 60`; `preserveAspectRatio="none"` stretches the path to
 * fill the card width. Day-dot markers are evenly spaced across `days`
 * data points; the latest point renders at `r="2.5"` with a 1px white
 * stroke per the spec.
 *
 * TODO(F-722 follow-up): `usage_summary` does not yet expose a per-day
 * series — `GroupBy` only buckets by Provider/Model/Scope. The chart
 * currently renders the empty-baseline path (the spec's `empty` visual)
 * regardless of `empty`; a follow-up IPC for daily buckets will replace
 * the constant baseline with the true trend.
 */
const SparkChart: Component<SparkChartProps> = (props) => {
  const points = createMemo<Array<{ x: number; y: number }>>(() => {
    const count = Math.max(2, props.days);
    const baseline = 45; // empty-baseline y per spec
    return Array.from({ length: count }, (_, i) => ({
      x: (i / (count - 1)) * 320,
      y: baseline,
    }));
  });

  const linePath = createMemo<string>(() => {
    const pts = points();
    return pts
      .map((p, i) => `${i === 0 ? 'M' : 'L'} ${p.x.toFixed(2)} ${p.y.toFixed(2)}`)
      .join(' ');
  });

  const areaPath = createMemo<string>(() => {
    const pts = points();
    const head = pts.map((p, i) => `${i === 0 ? 'M' : 'L'} ${p.x.toFixed(2)} ${p.y.toFixed(2)}`).join(' ');
    return `${head} L 320 60 L 0 60 Z`;
  });

  return (
    <div
      class="usage-card__spark"
      classList={{ 'usage-card__spark--empty': props.empty }}
      data-testid="usage-card-spark"
      aria-hidden="true"
    >
      <svg viewBox="0 0 320 60" preserveAspectRatio="none">
        <defs>
          <linearGradient id="usageCardSparkFill" x1="0" y1="0" x2="0" y2="1">
            {/* SVG `stop-color` does not accept CSS custom properties in V1;
                the hex matches `--color-ember-400` from tokens.css.
                TODO: --color-ember-400-rgb token so we can express this as
                `rgba(var(--color-ember-400-rgb), 0.35)`. */}
            <stop offset="0%" stop-color="#ff4a12" stop-opacity="0.35" />
            <stop offset="100%" stop-color="#ff4a12" stop-opacity="0" />
          </linearGradient>
        </defs>
        <path d={areaPath()} fill="url(#usageCardSparkFill)" />
        <path
          d={linePath()}
          fill="none"
          stroke="#ff4a12"
          stroke-width="1.5"
        />
        <g fill="#ff4a12">
          <For each={points()}>
            {(pt, i) => {
              const last = i() === points().length - 1;
              return (
                <circle
                  class={last ? 'usage-card__spark-live-dot' : undefined}
                  cx={pt.x.toFixed(2)}
                  cy={pt.y.toFixed(2)}
                  r={last ? 2.5 : 2}
                  stroke={last ? '#fff' : undefined}
                  stroke-width={last ? 1 : undefined}
                />
              );
            }}
          </For>
        </g>
      </svg>
    </div>
  );
};
