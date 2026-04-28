// F-594: limits table — one row per provider with its monthly cap (when
// configured) and the user's current month-to-date consumption.
//
// The cap source is intentionally an injectable prop. F-593's IPC doesn't
// expose configurable limits yet, and #612 calls the cap "if any" — we
// surface the consumption regardless and render `// not configured` in the
// cap column when no entry exists. Once a future task wires limits into
// `AppSettings`, the route can pass them in without touching this file.

import { For, Show, type Component } from 'solid-js';
import type { Money } from '@forge/ipc';
import type { UsageBreakdownView } from '../../ipc/usage';
import { formatMoney } from './format';

/** A single configured cap. `amount` + `currency` mirror the IPC `Money` shape. */
export interface UsageLimit {
  /** Provider id this cap applies to (e.g. `"anthropic"`). */
  provider: string;
  /** Monthly spend cap. */
  cap: Money;
}

export interface UsageLimitsTableProps {
  /** Per-provider rows from a `usage_summary` call grouped by provider. */
  byProvider: UsageBreakdownView[];
  /** Optional configured caps. Empty / missing → row reads "not configured". */
  limits?: UsageLimit[];
}

interface Row {
  provider: string;
  consumed: Money | null;
  cap: Money | null;
  /** 0..1 when both `consumed` + `cap` exist and currencies match; otherwise null. */
  ratio: number | null;
}

function findCap(limits: UsageLimit[] | undefined, provider: string): Money | null {
  if (!limits) return null;
  const hit = limits.find((l) => l.provider === provider);
  return hit ? hit.cap : null;
}

function buildRows(
  byProvider: UsageBreakdownView[],
  limits: UsageLimit[] | undefined,
): Row[] {
  const seen = new Set<string>();
  const rows: Row[] = [];
  for (const entry of byProvider) {
    seen.add(entry.key);
    const cap = findCap(limits, entry.key);
    const ratio =
      cap && entry.cost && cap.currency === entry.cost.currency && cap.amount > 0
        ? entry.cost.amount / cap.amount
        : null;
    rows.push({
      provider: entry.key,
      consumed: entry.cost,
      cap,
      ratio,
    });
  }
  // Surface configured-cap rows even when the provider has zero usage, so
  // the user can see "you set a cap on Anthropic; you've spent $0 of it".
  if (limits) {
    for (const limit of limits) {
      if (seen.has(limit.provider)) continue;
      rows.push({
        provider: limit.provider,
        consumed: { amount: 0, currency: limit.cap.currency },
        cap: limit.cap,
        ratio: 0,
      });
    }
  }
  rows.sort((a, b) => a.provider.localeCompare(b.provider));
  return rows;
}

function progressState(ratio: number): 'ok' | 'warn' | 'exceeded' {
  if (ratio >= 1) return 'exceeded';
  if (ratio >= 0.8) return 'warn';
  return 'ok';
}

export const UsageLimitsTable: Component<UsageLimitsTableProps> = (props) => {
  const rows = (): Row[] => buildRows(props.byProvider, props.limits);

  return (
    <Show
      when={rows().length > 0}
      fallback={<p class="usage-pane__empty">// no providers in range</p>}
    >
      <table class="usage-pane__table" aria-label="Provider limits table">
        <thead>
          <tr>
            <th scope="col">Provider</th>
            <th scope="col">Consumed</th>
            <th scope="col">Cap</th>
            <th scope="col">Progress</th>
          </tr>
        </thead>
        <tbody>
          <For each={rows()}>
            {(r) => (
              <tr>
                <td>{r.provider}</td>
                <td class="usage-pane__table-num">{formatMoney(r.consumed)}</td>
                <td class="usage-pane__table-num">
                  <Show
                    when={r.cap}
                    fallback={
                      <span class="usage-pane__limit-meta">// not configured</span>
                    }
                  >
                    {(cap) => formatMoney(cap())}
                  </Show>
                </td>
                <td>
                  <Show
                    when={r.ratio !== null}
                    fallback={<span class="usage-pane__limit-meta">—</span>}
                  >
                    <ProgressBar ratio={r.ratio!} />
                  </Show>
                </td>
              </tr>
            )}
          </For>
        </tbody>
      </table>
    </Show>
  );
};

const ProgressBar: Component<{ ratio: number }> = (props) => {
  const clamped = (): number => Math.max(0, Math.min(1, props.ratio));
  const state = (): string => progressState(props.ratio);
  // Width travels through a CSS custom property so the component only writes
  // a percentage string into a JSX style — no raw px or hex, satisfying the
  // F-389 inline-style gate.
  const styleVar = (): { width: string } => ({ width: `${(clamped() * 100).toFixed(1)}%` });
  return (
    <span
      class="usage-pane__progress"
      role="progressbar"
      aria-valuenow={Math.round(clamped() * 100)}
      aria-valuemin={0}
      aria-valuemax={100}
    >
      <span
        class="usage-pane__progress-fill"
        data-state={state()}
        style={styleVar()}
      />
    </span>
  );
};
