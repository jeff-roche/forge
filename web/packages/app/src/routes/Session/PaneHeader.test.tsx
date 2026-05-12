import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render } from '@solidjs/testing-library';
import type { ProviderId } from '@forge/ipc';
import { PaneHeader } from './PaneHeader';

// PaneHeader close button (F-084): per voice-terminology.md §8 "Button labels:
// verb + noun in display caps." The close button's visible text must read
// `CLOSE SESSION` literally — the aria-label remains a sentence for AT users.

describe('PaneHeader close button — voice & terminology (F-084)', () => {
  it('renders close button text as `CLOSE SESSION` (verb + noun, display caps)', () => {
    const { getByRole } = render(() => (
      <PaneHeader
        subject="example"
        providerId={'local' as ProviderId}
        providerLabel="local"
        costLabel="$0.00"
        onClose={vi.fn()}
      />
    ));
    const btn = getByRole('button', { name: 'Close session window' });
    // Literal match — case matters; this is the source-of-truth string,
    // not a CSS uppercase transform.
    expect(btn.textContent).toBe('CLOSE SESSION');
  });

  it('keeps the existing aria-label as a sentence for screen readers', () => {
    const { getByRole } = render(() => (
      <PaneHeader
        subject="example"
        providerId={'local' as ProviderId}
        providerLabel="local"
        costLabel="$0.00"
        onClose={vi.fn()}
      />
    ));
    const btn = getByRole('button', { name: 'Close session window' });
    expect(btn.getAttribute('aria-label')).toBe('Close session window');
  });
});

// PaneHeader provider pill (F-091): per ai-patterns.md §7, the provider pill
// color must follow the active provider — anthropic/ember, openai/amber,
// lm-studio/local/steel, otherwise iron-200. The component plumbs the
// active provider's accent into a CSS custom property on the provider pill so
// the existing class-driven CSS picks it up without per-provider rules.

describe('PaneHeader provider pill accent (F-091)', () => {
  it.each<[ProviderId, string]>([
    ['anthropic' as ProviderId, 'var(--color-provider-anthropic)'],
    ['openai' as ProviderId, 'var(--color-provider-openai)'],
    ['local' as ProviderId, 'var(--color-provider-local)'],
    ['lm-studio' as ProviderId, 'var(--color-provider-local)'],
    ['custom' as ProviderId, 'var(--color-provider-custom)'],
  ])('binds the pill accent to %s → %s', (providerId, expectedAccent) => {
    const { getByTestId } = render(() => (
      <PaneHeader
        subject="example"
        providerId={providerId}
        providerLabel={String(providerId)}
        costLabel="$0.00"
        onClose={vi.fn()}
      />
    ));
    const pill = getByTestId('pane-header-provider');
    // Inline style carries the CSS variable so the rule stays generic.
    // jsdom's getPropertyValue is reliable for inline custom properties.
    expect(pill.style.getPropertyValue('--pane-header-provider-accent')).toBe(
      expectedAccent,
    );
  });

  it('falls back to the custom accent for an unknown provider id', () => {
    const { getByTestId } = render(() => (
      <PaneHeader
        subject="example"
        providerId={'made-up-provider' as ProviderId}
        providerLabel="custom"
        costLabel="$0.00"
        onClose={vi.fn()}
      />
    ));
    const pill = getByTestId('pane-header-provider');
    expect(pill.style.getPropertyValue('--pane-header-provider-accent')).toBe(
      'var(--color-provider-custom)',
    );
  });
});

// PaneHeader compactness (F-119): per layout-panes.md §3.7 + pane-header.md
// §2.3, the header must collapse chrome as the pane narrows. The prop is
// optional — omitting it is `full`. At `compact` the type label collapses
// to an icon; at `icon-only` the provider pill + cost meter also hide.
// Callers derive the prop from `usePaneWidth` against the enclosing pane.

describe('PaneHeader compactness (F-119)', () => {
  it('defaults to `full` when no compactness prop is passed — all chrome visible', () => {
    const { getByTestId, queryByTestId } = render(() => (
      <PaneHeader
        subject="example"
        providerId={'local' as ProviderId}
        providerLabel="local"
        costLabel="$0.00"
        onClose={vi.fn()}
      />
    ));
    const root = getByTestId('pane-header-subject').parentElement;
    expect(root?.getAttribute('data-compactness')).toBe('full');
    expect(queryByTestId('pane-header-provider')).not.toBeNull();
    expect(queryByTestId('pane-header-cost')).not.toBeNull();
  });

  it('at `compact`: marks root with data-compactness="compact" and collapses the type label to an icon', () => {
    const { getByTestId, queryByTestId } = render(() => (
      <PaneHeader
        subject="example"
        providerId={'local' as ProviderId}
        providerLabel="local"
        costLabel="$0.00"
        compactness="compact"
        onClose={vi.fn()}
      />
    ));
    const root = getByTestId('pane-header-subject').parentElement;
    expect(root?.getAttribute('data-compactness')).toBe('compact');
    // Type label swapped to an icon glyph (aria-hidden; the a11y name is
    // carried by the pane-header-type-label element via aria-label).
    const typeLabel = getByTestId('pane-header-type-label');
    expect(typeLabel.getAttribute('data-icon-only')).toBe('true');
    // Badges (provider pill + cost meter) still render at `compact` — they
    // only disappear at `icon-only`. This matches DoD: labels collapse at
    // 320px, badges collapse at 240px.
    expect(queryByTestId('pane-header-provider')).not.toBeNull();
    expect(queryByTestId('pane-header-cost')).not.toBeNull();
  });

  it('at `icon-only`: hides both the provider pill and the cost meter badges', () => {
    const { getByTestId, queryByTestId } = render(() => (
      <PaneHeader
        subject="example"
        providerId={'local' as ProviderId}
        providerLabel="local"
        costLabel="$0.00"
        compactness="icon-only"
        onClose={vi.fn()}
      />
    ));
    const root = getByTestId('pane-header-subject').parentElement;
    expect(root?.getAttribute('data-compactness')).toBe('icon-only');
    // Label is still in icon mode at `icon-only` (`compact` or narrower).
    const typeLabel = getByTestId('pane-header-type-label');
    expect(typeLabel.getAttribute('data-icon-only')).toBe('true');
    // Badges removed from the tree entirely — the narrow-width spec treats
    // them as non-essential chrome. Keeping them with `display: none`
    // would leave them discoverable by screen readers.
    expect(queryByTestId('pane-header-provider')).toBeNull();
    expect(queryByTestId('pane-header-cost')).toBeNull();
  });

  it('keeps the close button visible at every compactness level', () => {
    for (const c of ['full', 'compact', 'icon-only'] as const) {
      const { getByRole, unmount } = render(() => (
        <PaneHeader
          subject="example"
          providerId={'local' as ProviderId}
          providerLabel="local"
          costLabel="$0.00"
          compactness={c}
          onClose={vi.fn()}
        />
      ));
      // Close is essential at every width — no pane is useful without a
      // way to dismiss it. (Per pane-header.md §PH.6 the close button stays.)
      expect(getByRole('button', { name: 'Close session window' })).not.toBeNull();
      unmount();
    }
  });
});

// F-394: the pane-header <header> is sub-structural (nested inside a pane
// <section>). ARIA treats a sub-structural <header> as a generic landmark
// only when it's a top-level descendant of <body>; inside a <section> it
// becomes a `banner` landmark only if explicitly tagged with role="banner".
// Previously both PaneHeader and EditorPane stamped role="banner" on their
// header, producing multiple banner landmarks per document. Drop it here
// so only the actual top-level banner (if any) counts.
describe('PaneHeader landmark (F-394)', () => {
  it('does not set role="banner" on the <header>', () => {
    const { getByTestId } = render(() => (
      <PaneHeader subject="example" onClose={vi.fn()} />
    ));
    const header = getByTestId('pane-header-subject').parentElement;
    expect(header?.tagName.toLowerCase()).toBe('header');
    expect(header?.getAttribute('role')).not.toBe('banner');
  });
});

// F-394: PaneHeader needs a slot for pane-specific badges (e.g. the editor's
// dirty-dot indicator) so consumers stop re-implementing the header markup.
// The slot sits between the subject and the cost meter, caller-owned JSX,
// removed from the tree at `icon-only` alongside the other badges.
describe('PaneHeader trailing slot (F-394)', () => {
  it('renders arbitrary trailing JSX after the subject', () => {
    const { getByTestId } = render(() => (
      <PaneHeader
        subject="file.ts"
        trailing={<span data-testid="ph-trailing-node">!</span>}
        onClose={vi.fn()}
      />
    ));
    const trailing = getByTestId('ph-trailing-node');
    expect(trailing).toBeInTheDocument();
    // Placement: trailing must sit after the subject in document order so
    // screen-reader traversal reads subject → badge.
    const subject = getByTestId('pane-header-subject');
    expect(subject.compareDocumentPosition(trailing) & Node.DOCUMENT_POSITION_FOLLOWING)
      .toBeTruthy();
  });

  it('omits the trailing slot from the tree at `icon-only`', () => {
    const { queryByTestId } = render(() => (
      <PaneHeader
        subject="file.ts"
        compactness="icon-only"
        trailing={<span data-testid="ph-trailing-node">!</span>}
        onClose={vi.fn()}
      />
    ));
    // At icon-only, provider + cost disappear; the trailing badge is
    // non-essential chrome with the same disposition.
    expect(queryByTestId('ph-trailing-node')).toBeNull();
  });

  it('renders no trailing wrapper when `trailing` is undefined', () => {
    // Default call shape must produce no extra DOM — consumers that don't
    // pass a trailing node see the same layout they had before.
    const { container } = render(() => (
      <PaneHeader subject="file.ts" onClose={vi.fn()} />
    ));
    expect(
      container.querySelector('[data-testid="pane-header-trailing"]'),
    ).toBeNull();
  });
});

// F-394: EditorPane needs to thread the F-150 drag-to-dock pointerdown into
// the header element. Accepting the handler on the primitive avoids wrapping
// the header in an extra <div> at every consumer site.
describe('PaneHeader onHeaderPointerDown (F-394)', () => {
  it('forwards pointerdown on the <header> to the supplied handler', () => {
    const onHeaderPointerDown = vi.fn();
    const { getByTestId } = render(() => (
      <PaneHeader
        subject="file.ts"
        onHeaderPointerDown={onHeaderPointerDown}
        onClose={vi.fn()}
      />
    ));
    const header = getByTestId('pane-header-subject').parentElement!;
    fireEvent.pointerDown(header);
    expect(onHeaderPointerDown).toHaveBeenCalledTimes(1);
  });
});
