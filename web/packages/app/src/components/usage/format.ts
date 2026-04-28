// F-594: small formatters shared by the usage tables.
//
// `Money.cost` is `null` whenever any contributing usage event lacked a
// price-table entry (see `UsageBreakdown` docstring). The UI surfaces that
// as the literal `—` glyph rather than a misleading `$0.00` — the missing
// price would otherwise silently zero out the column.

import type { Money } from '@forge/ipc';

export function formatMoney(money: Money | null): string {
  if (!money) return '—';
  // ISO-4217 codes are uppercase; defensively normalise so `Intl` doesn't
  // throw on a stray `"usd"` slipping through the wire.
  const currency = money.currency.toUpperCase();
  try {
    return new Intl.NumberFormat(undefined, {
      style: 'currency',
      currency,
      minimumFractionDigits: 2,
      maximumFractionDigits: 4,
    }).format(money.amount);
  } catch {
    // Unknown currency code — fall back to `<amount> <code>` so the row
    // still reads sensibly.
    return `${money.amount.toFixed(2)} ${currency}`;
  }
}
