// F-594: typed wrapper around the F-593 `usage_summary` Tauri command.
//
// The Rust side returns `u64` token counters that ts-rs renders as `bigint`
// in the generated TypeScript, but Tauri's JSON bridge ships them as plain
// `number` over the wire. We normalise to `number` at the boundary so the
// rest of the UI never reasons about a `bigint` whose value actually arrived
// as `number` — that mismatch is otherwise the kind of bug that blows up at
// runtime when a `+` operator silently coerces.

import { invoke } from '../lib/tauri';
import type {
  GroupBy,
  Money,
  UsageBreakdown,
  UsageRange,
  UsageSummary,
} from '@forge/ipc';

/**
 * Local UI shape — mirrors `UsageSummary` but with `number` token counters so
 * arithmetic + chart math is straightforward. `Money` round-trips unchanged
 * because its `amount` is already `f64`.
 */
export interface UsageSummaryView {
  range: UsageRange;
  group_by: GroupBy;
  total_tokens_in: number;
  total_tokens_out: number;
  total_cost: Money | null;
  breakdown: UsageBreakdownView[];
}

export interface UsageBreakdownView {
  key: string;
  tokens_in: number;
  tokens_out: number;
  cost: Money | null;
}

function toNumber(value: unknown): number {
  if (typeof value === 'number') return value;
  if (typeof value === 'bigint') return Number(value);
  if (typeof value === 'string') return Number(value);
  return 0;
}

function normaliseBreakdown(b: UsageBreakdown): UsageBreakdownView {
  return {
    key: b.key,
    tokens_in: toNumber(b.tokens_in),
    tokens_out: toNumber(b.tokens_out),
    cost: b.cost,
  };
}

export function normaliseUsageSummary(s: UsageSummary): UsageSummaryView {
  return {
    range: s.range,
    group_by: s.group_by,
    total_tokens_in: toNumber(s.total_tokens_in),
    total_tokens_out: toNumber(s.total_tokens_out),
    total_cost: s.total_cost,
    breakdown: s.breakdown.map(normaliseBreakdown),
  };
}

export interface UsageQueryArgs {
  range: UsageRange;
  groupBy: GroupBy;
  crossWorkspace: boolean;
  workspaceRoot: string | null;
}

/**
 * Fetch an aggregated usage summary from the shell. Returns the
 * `number`-normalised view shape (see `normaliseUsageSummary`).
 *
 * The backend's `cross_workspace` toggle drops the workspace filter when
 * `true`; when `false`, a `null` `workspace_root` falls back to "no
 * filter" too — the shell logs a warning but still responds. Callers that
 * want strict per-workspace results pass the active workspace's root.
 */
export async function fetchUsageSummary(
  args: UsageQueryArgs,
): Promise<UsageSummaryView> {
  const raw = await invoke<UsageSummary>('usage_summary', {
    range: args.range,
    groupBy: args.groupBy,
    crossWorkspace: args.crossWorkspace,
    workspaceRoot: args.workspaceRoot,
  });
  return normaliseUsageSummary(raw);
}
