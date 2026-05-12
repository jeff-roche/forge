import type { ProviderId } from '@forge/ipc';

/**
 * Per-provider pane accent (F-091, ai-patterns.md §7).
 *
 * Returns a `var(--color-provider-*)` reference suitable for assignment to a
 * CSS custom property in an inline `style` attribute. The four accent tokens
 * live in `web/packages/design/src/tokens.css` and are governed by
 * `scripts/check-tokens.mjs`.
 *
 * `ProviderId` is a string newtype on the IPC boundary, so unknown ids are
 * reachable at runtime — they fall back to the custom-endpoint accent.
 */
export function providerAccent(id: ProviderId): string {
  switch (id) {
    case 'anthropic':
      return 'var(--color-provider-anthropic)';
    case 'openai':
      return 'var(--color-provider-openai)';
    case 'lm-studio':
    case 'local':
      return 'var(--color-provider-local)';
    case 'custom':
      return 'var(--color-provider-custom)';
    default:
      return 'var(--color-provider-custom)';
  }
}
