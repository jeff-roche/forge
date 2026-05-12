import { describe, expect, it } from 'vitest';
import type { SessionTelemetry } from '../../stores/sessionTelemetry';
import {
  formatChatSubject,
  formatCostLabel,
  formatProviderLabel,
  resolveProviderId,
} from './costLabel';

function telem(partial: Partial<SessionTelemetry> = {}): SessionTelemetry {
  return {
    provider: null,
    model: null,
    tokensIn: null,
    tokensOut: null,
    costUsd: null,
    ...partial,
  };
}

describe('formatCostLabel (F-395 / pane-header.md §PH.4)', () => {
  it('renders the em-dash placeholder when no UsageTick has landed', () => {
    expect(formatCostLabel(telem())).toBe('—');
  });

  it('renders in/out/cost once telemetry is populated', () => {
    const s = formatCostLabel(
      telem({ tokensIn: 500, tokensOut: 1500, costUsd: 0.04 }),
    );
    expect(s).toMatch(/in\s+500/);
    expect(s).toMatch(/out\s+1\.5k/);
    expect(s).toContain('$0.04');
  });

  it('abbreviates tokens above 1000', () => {
    expect(formatCostLabel(telem({ tokensIn: 999, tokensOut: 1, costUsd: 0 }))).toMatch(
      /in\s+999/,
    );
    expect(
      formatCostLabel(telem({ tokensIn: 1500, tokensOut: 1, costUsd: 0 })),
    ).toMatch(/in\s+1\.5k/);
    expect(
      formatCostLabel(telem({ tokensIn: 15_000, tokensOut: 1, costUsd: 0 })),
    ).toMatch(/in\s+15k/);
    expect(
      formatCostLabel(telem({ tokensIn: 1_200_000, tokensOut: 1, costUsd: 0 })),
    ).toMatch(/in\s+1\.2M/);
  });

  it('never strips the leading zero on dollars (spec §PH.4)', () => {
    expect(
      formatCostLabel(telem({ tokensIn: 1, tokensOut: 1, costUsd: 0.04 })),
    ).toContain('$0.04');
    // Never renders the offending "$.04" form.
    expect(
      formatCostLabel(telem({ tokensIn: 1, tokensOut: 1, costUsd: 0.04 })),
    ).not.toContain('$.04');
  });

  it('renders the live totals even when cost is exactly zero (local provider)', () => {
    const s = formatCostLabel(
      telem({ tokensIn: 100, tokensOut: 200, costUsd: 0 }),
    );
    // Zero cost is a legitimate local-provider state — the meter must still
    // render the real token counts, not the placeholder.
    expect(s).not.toBe('—');
    expect(s).toContain('$0.00');
  });
});

describe('formatProviderLabel (F-395)', () => {
  it('falls back to the em-dash placeholder before the first assistant turn', () => {
    expect(formatProviderLabel(telem())).toBe('—');
  });

  it('uses the observed provider id once recorded', () => {
    expect(formatProviderLabel(telem({ provider: 'anthropic' }))).toBe(
      'anthropic',
    );
  });

  it('never renders the unsanctioned `pending` state suffix', () => {
    expect(formatProviderLabel(telem())).not.toContain('pending');
    expect(formatProviderLabel(telem({ provider: 'anthropic' }))).not.toContain(
      'pending',
    );
  });
});

describe('formatChatSubject (F-395)', () => {
  it('drops the legacy `Session <id>` prefix entirely', () => {
    const s = formatChatSubject('abc123-uuid-very-long', telem());
    expect(s).not.toMatch(/^Session /);
  });

  it('uses a short session id as the fallback head', () => {
    expect(formatChatSubject('abcdefghijkl', telem())).toBe('abcdefgh');
  });

  it('appends the live model when known', () => {
    expect(
      formatChatSubject(
        'abcdefghijkl',
        telem({ provider: 'anthropic', model: 'claude-opus-4-7' }),
      ),
    ).toBe('abcdefgh · claude-opus-4-7');
  });
});

describe('resolveProviderId (F-395)', () => {
  it('falls back to `custom` when no provider observed', () => {
    expect(resolveProviderId(telem())).toBe('custom');
  });
  it('returns the observed provider id', () => {
    expect(resolveProviderId(telem({ provider: 'anthropic' }))).toBe(
      'anthropic',
    );
  });
});
