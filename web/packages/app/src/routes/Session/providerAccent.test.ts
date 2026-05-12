import { describe, expect, it } from 'vitest';
import type { ProviderId } from '@forge/ipc';
import { providerAccent } from './providerAccent';

// F-091: per ai-patterns.md §7 each provider has a fixed accent token:
//   anthropic → ember-400, openai → amber, local/lm-studio → steel,
//   custom (and any unknown) → iron-200.
// The helper returns a `var(--color-provider-*)` reference so callers can
// drop it directly into an inline-style CSS custom property without touching
// CSS files when new provider ids land.

describe('providerAccent (F-091)', () => {
  it.each<[ProviderId, string]>([
    ['anthropic' as ProviderId, 'var(--color-provider-anthropic)'],
    ['openai' as ProviderId, 'var(--color-provider-openai)'],
    ['local' as ProviderId, 'var(--color-provider-local)'],
    ['lm-studio' as ProviderId, 'var(--color-provider-local)'],
    ['custom' as ProviderId, 'var(--color-provider-custom)'],
  ])('maps %s to %s', (id, expected) => {
    expect(providerAccent(id)).toBe(expected);
  });

  it('falls back to the custom accent for an unknown provider id', () => {
    // ProviderId is a string newtype, so unknown ids are reachable at runtime.
    expect(providerAccent('made-up-provider' as ProviderId)).toBe(
      'var(--color-provider-custom)',
    );
  });
});
