// F-594: usage view — chart + limits + per-model breakdown.
//
// Owns the range selector + cross-workspace toggle and fans out two parallel
// `usage_summary` IPC calls per change: one grouped by provider (drives the
// chart + limits table), one grouped by model (drives the bottom table).
// Two calls is the simplest path that respects the IPC contract — the
// alternative (one call + client-side regrouping) would force the UI to
// reconstruct the by-model totals from raw monthly aggregates.

import {
  createMemo,
  createResource,
  createSignal,
  For,
  Show,
  type Component,
} from 'solid-js';
import { Button } from '@forge/design';
import type { GroupBy, UsageRange } from '@forge/ipc';
import { Skeleton } from '@forge/design';
import {
  fetchUsageSummary,
  type UsageBreakdownView,
  type UsageSummaryView,
} from '../../ipc/usage';
import { UsageChart, type UsageChartSegment } from './UsageChart';
import { UsageLimitsTable, type UsageLimit } from './UsageLimitsTable';
import { UsageModelTable } from './UsageModelTable';
import './UsagePane.css';

// ---------------------------------------------------------------------------
// Range selector
// ---------------------------------------------------------------------------

/** Tag the range options in declaration order so the toolbar renders the four
 *  values from #612 ("Today / Last 7 / Last 30 / All") without a hand-coded
 *  switch downstream. */
export type RangeKey = 'today' | 'last7' | 'last30' | 'all';

const RANGE_OPTIONS: ReadonlyArray<{ key: RangeKey; label: string; range: UsageRange }> = [
  { key: 'today', label: 'Today', range: { type: 'Today' } },
  { key: 'last7', label: 'Last 7', range: { type: 'Last7' } },
  { key: 'last30', label: 'Last 30', range: { type: 'Last30' } },
  { key: 'all', label: 'All', range: { type: 'All' } },
];

export function rangeFor(key: RangeKey): UsageRange {
  return RANGE_OPTIONS.find((o) => o.key === key)!.range;
}

// ---------------------------------------------------------------------------
// Data resource — two parallel `usage_summary` calls
// ---------------------------------------------------------------------------

interface FetchKey {
  range: UsageRange;
  crossWorkspace: boolean;
  workspaceRoot: string | null;
}

interface UsagePaneData {
  byProvider: UsageSummaryView;
  byModel: UsageSummaryView;
}

async function fetchPaneData(key: FetchKey): Promise<UsagePaneData> {
  const args = {
    range: key.range,
    crossWorkspace: key.crossWorkspace,
    workspaceRoot: key.workspaceRoot,
  };
  const [byProvider, byModel] = await Promise.all([
    fetchUsageSummary({ ...args, groupBy: 'Provider' as GroupBy }),
    fetchUsageSummary({ ...args, groupBy: 'Model' as GroupBy }),
  ]);
  return { byProvider, byModel };
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Build the chart segment list from a provider-grouped breakdown. Sums
 *  in+out tokens per provider; rows with zero usage are dropped by the chart. */
export function toChartSegments(rows: UsageBreakdownView[]): UsageChartSegment[] {
  return rows.map((row) => ({
    provider: row.key,
    label: row.key,
    tokens: row.tokens_in + row.tokens_out,
  }));
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export interface UsagePaneProps {
  /** Active workspace root for the per-workspace filter. `null` is allowed —
   *  the shell logs a warning and applies no filter, matching the
   *  `cross_workspace=true` semantics from `usage_ipc.rs`. */
  workspaceRoot: string | null;
  /** Optional configured monthly caps. When omitted the limits table renders
   *  consumption with `// not configured` rows, which #612 explicitly allows
   *  ("configured monthly cap **if any**"). */
  limits?: UsageLimit[];
}

export const UsagePane: Component<UsagePaneProps> = (props) => {
  const [rangeKey, setRangeKey] = createSignal<RangeKey>('last30');
  const [crossWorkspace, setCrossWorkspace] = createSignal<boolean>(false);

  const fetchKey = createMemo<FetchKey>(() => ({
    range: rangeFor(rangeKey()),
    crossWorkspace: crossWorkspace(),
    workspaceRoot: props.workspaceRoot,
  }));

  const [data, { refetch }] = createResource(fetchKey, fetchPaneData);

  // Reading `data()` while the resource is in its `errored` state re-throws
  // in the reactive scope (the same gotcha AgentMonitor handles via
  // `bgAgents.state === 'ready'`). Gate every read on the resource state so
  // a rejected fetch flows cleanly into the alert branch instead of crashing
  // the route.
  const dataValue = (): UsagePaneData | undefined =>
    data.state === 'ready' ? data() : undefined;

  const chartSegments = createMemo<UsageChartSegment[]>(() => {
    const d = dataValue();
    if (!d) return [];
    return toChartSegments(d.byProvider.breakdown);
  });

  const errorDetail = (): string | null => {
    const err = data.error;
    if (!err) return null;
    return err instanceof Error ? `Error: ${err.message}` : String(err);
  };

  const isEmpty = createMemo<boolean>(() => {
    const d = dataValue();
    if (!d) return false;
    return d.byProvider.total_tokens_in + d.byProvider.total_tokens_out === 0;
  });

  return (
    <section class="usage-pane" aria-label="Usage view">
      <header class="usage-pane__toolbar">
        <div
          class="usage-pane__ranges"
          role="group"
          aria-label="Usage time range"
        >
          <For each={RANGE_OPTIONS}>
            {(opt) => (
              <Button
                variant="ghost"
                size="sm"
                aria-pressed={rangeKey() === opt.key}
                onClick={() => setRangeKey(opt.key)}
              >
                {opt.label}
              </Button>
            )}
          </For>
        </div>
        <label class="usage-pane__cross">
          <input
            type="checkbox"
            checked={crossWorkspace()}
            onChange={(e) => setCrossWorkspace(e.currentTarget.checked)}
          />
          Cross-workspace
        </label>
      </header>

      <Show when={data.loading}>
        <div class="usage-pane__skeletons" data-testid="usage-loading">
          <Skeleton
            variant="block"
            count={1}
            label="Loading usage chart"
            class="usage-pane__skeleton-chart"
          />
          <Skeleton
            variant="block"
            count={3}
            label="Loading usage tables"
            class="usage-pane__skeleton-rows"
          />
        </div>
      </Show>

      <Show when={errorDetail()}>
        {(detail) => (
          <div class="usage-pane__error" role="alert">
            <p class="usage-pane__error-title">USAGE UNAVAILABLE</p>
            <p class="usage-pane__error-detail">{detail()}</p>
            <Button
              variant="primary"
              size="sm"
              onClick={() => void refetch()}
            >
              Retry
            </Button>
          </div>
        )}
      </Show>

      <Show when={dataValue()}>
        {(d) => (
          <Show
            when={!isEmpty()}
            fallback={
              <section class="usage-pane__section" aria-label="Usage chart">
                <p class="usage-pane__section-label">USAGE</p>
                <p class="usage-pane__empty">// no usage recorded for this range</p>
              </section>
            }
          >
            <section class="usage-pane__section" aria-label="Usage chart">
              <p class="usage-pane__section-label">TOKENS BY PROVIDER</p>
              <UsageChart segments={chartSegments()} />
            </section>

            <section class="usage-pane__section" aria-label="Provider limits">
              <p class="usage-pane__section-label">PROVIDER LIMITS</p>
              <Show
                when={props.limits}
                fallback={
                  <UsageLimitsTable byProvider={d().byProvider.breakdown} />
                }
              >
                {(lims) => (
                  <UsageLimitsTable
                    byProvider={d().byProvider.breakdown}
                    limits={lims()}
                  />
                )}
              </Show>
            </section>

            <section class="usage-pane__section" aria-label="Per-model breakdown">
              <p class="usage-pane__section-label">PER MODEL</p>
              <UsageModelTable rows={d().byModel.breakdown} />
            </section>
          </Show>
        )}
      </Show>
    </section>
  );
};
