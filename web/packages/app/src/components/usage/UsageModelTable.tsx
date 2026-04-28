// F-594: per-model breakdown table — sorted by cost desc per #612.
//
// Pure presentation; the parent fetches a `group_by: 'Model'` summary and
// passes the rows in. Rows with `cost === null` (any contributing event
// lacked a price-table entry — see `UsageBreakdown`'s docstring) sort below
// rows with a known cost so the most-expensive items still surface first.

import { For, Show, type Component } from 'solid-js';
import type { UsageBreakdownView } from '../../ipc/usage';
import { formatMoney } from './format';

export interface UsageModelTableProps {
  rows: UsageBreakdownView[];
}

function sortByCostDesc(rows: UsageBreakdownView[]): UsageBreakdownView[] {
  const sorted = [...rows];
  sorted.sort((a, b) => {
    const aCost = a.cost ? a.cost.amount : -Infinity;
    const bCost = b.cost ? b.cost.amount : -Infinity;
    if (aCost !== bCost) return bCost - aCost;
    // Tiebreak by total tokens so two `null`-cost rows still order
    // deterministically (largest run first).
    const aTokens = a.tokens_in + a.tokens_out;
    const bTokens = b.tokens_in + b.tokens_out;
    return bTokens - aTokens;
  });
  return sorted;
}

export const UsageModelTable: Component<UsageModelTableProps> = (props) => {
  const rows = (): UsageBreakdownView[] => sortByCostDesc(props.rows);

  return (
    <Show
      when={rows().length > 0}
      fallback={<p class="usage-pane__empty">// no models in range</p>}
    >
      <table class="usage-pane__table" aria-label="Per-model breakdown table">
        <thead>
          <tr>
            <th scope="col">Model</th>
            <th scope="col">Tokens in</th>
            <th scope="col">Tokens out</th>
            <th scope="col">Cost</th>
          </tr>
        </thead>
        <tbody>
          <For each={rows()}>
            {(row) => (
              <tr>
                <td>{row.key}</td>
                <td class="usage-pane__table-num">
                  {row.tokens_in.toLocaleString()}
                </td>
                <td class="usage-pane__table-num">
                  {row.tokens_out.toLocaleString()}
                </td>
                <td class="usage-pane__table-num">{formatMoney(row.cost)}</td>
              </tr>
            )}
          </For>
        </tbody>
      </table>
    </Show>
  );
};

export { sortByCostDesc };
